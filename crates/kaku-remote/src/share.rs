//! One shareable web link per Kaku, reached through the relay.
//!
//! Kaku keeps a share identity on disk: a secret that registers this Mac with
//! the relay and a key that seals every message end to end. The relay knows
//! the Mac only by the SHA-256 of the secret, so holding the link (which
//! carries that id and the key) does not let anyone register as this Mac,
//! and the relay only ever forwards sealed bytes. Resetting the identity
//! retires the old link at once: the running Kaku notices the new file and
//! re-registers under the new id.

use crate::web::{self, WebDirty};
use anyhow::Context;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const TO_HOST: &[u8] = b"kaku-web c2h";
const FROM_HOST: &[u8] = b"kaku-web h2c";
const KIND_DATA: u8 = 0;
const KIND_ASSET: u8 = 1;
const IDENTITY_CHECK: Duration = Duration::from_secs(3);
/// Pings keep NATs and proxies from dropping an idle link, and their pongs
/// prove the relay is still there.
const KEEPALIVE: Duration = Duration::from_secs(20);
/// With pings every 20s, this long without hearing from the relay means the
/// link died without a close (sleep, network change); reconnect.
const SILENCE_LIMIT: Duration = Duration::from_secs(60);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const INBOX_TIMEOUT: Duration = Duration::from_secs(2);
const AUTH_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Longest a connect, TLS and WebSocket handshake may take. A proxy can
/// accept the TCP connection locally and then never answer once its own
/// upstream is gone; without a limit the bridge waited there for hours and
/// the link stayed offline until Kaku restarted.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

// ── Identity ──────────────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ShareIdentity {
    /// Registers this Mac with the relay. Never leaves the Mac.
    secret: String,
    /// AES-256-GCM key, base64url. Travels only in the link's fragment.
    key: String,
}

impl ShareIdentity {
    fn generate() -> anyhow::Result<Self> {
        let mut secret = [0u8; 32];
        let mut key = [0u8; 32];
        getrandom::fill(&mut secret).context("random secret")?;
        getrandom::fill(&mut key).context("random key")?;
        Ok(Self {
            secret: hex(&secret),
            key: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key),
        })
    }

    /// The relay's name for this Mac: the first 128 bits of SHA-256(secret).
    pub fn id(&self) -> String {
        let digest = ring::digest::digest(&ring::digest::SHA256, self.secret.as_bytes());
        hex(&digest.as_ref()[..16])
    }

    fn sealing_key(&self) -> anyhow::Result<LessSafeKey> {
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&self.key)
            .context("decode share key")?;
        let key = UnboundKey::new(&AES_256_GCM, &raw)
            .map_err(|_| anyhow::anyhow!("share key has the wrong length"))?;
        Ok(LessSafeKey::new(key))
    }

    /// The page link for a relay at `relay_url` (ws:// or wss://).
    pub fn link(&self, relay_url: &str) -> String {
        format!("{}/{}/#k={}", http_base(relay_url), self.id(), self.key)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn http_base(relay_url: &str) -> String {
    let url = relay_url.trim_end_matches('/');
    if let Some(rest) = url.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = url.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        url.to_string()
    }
}

fn identity_path() -> PathBuf {
    config::DATA_DIR.join("remote-share.json")
}

fn read_identity() -> Option<ShareIdentity> {
    let data = std::fs::read_to_string(identity_path()).ok()?;
    serde_json::from_str(&data).ok()
}

fn write_identity(identity: &ShareIdentity) -> anyhow::Result<()> {
    let path = identity_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("create data dir")?;
    }
    let json = serde_json::to_string(identity)?;
    // Write to a sibling and rename, so a reader never sees half a file.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).context("write share identity")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, &path).context("save share identity")?;
    Ok(())
}

/// This Kaku's share identity, created on first use.
pub fn load_or_create_identity() -> anyhow::Result<ShareIdentity> {
    if let Some(identity) = read_identity() {
        return Ok(identity);
    }
    let identity = ShareIdentity::generate()?;
    write_identity(&identity)?;
    Ok(identity)
}

/// Replace the identity; the old link stops working once the running Kaku
/// re-registers, within a few seconds.
pub fn reset_identity() -> anyhow::Result<ShareIdentity> {
    let identity = ShareIdentity::generate()?;
    write_identity(&identity)?;
    Ok(identity)
}

// ── Sealing ───────────────────────────────────────────────────────────────────

fn seal(key: &LessSafeKey, rng: &SystemRandom, plain: &[u8]) -> Option<Vec<u8>> {
    let mut nonce = [0u8; NONCE_LEN];
    rng.fill(&mut nonce).ok()?;
    let mut buf = plain.to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(FROM_HOST),
        &mut buf,
    )
    .ok()?;
    let mut out = Vec::with_capacity(NONCE_LEN + buf.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&buf);
    Some(out)
}

fn open(key: &LessSafeKey, sealed: &[u8]) -> Option<String> {
    if sealed.len() < NONCE_LEN {
        return None;
    }
    let (nonce, body) = sealed.split_at(NONCE_LEN);
    let nonce = Nonce::try_assume_unique_for_key(nonce).ok()?;
    let mut buf = body.to_vec();
    let plain = key
        .open_in_place(nonce, Aad::from(TO_HOST), &mut buf)
        .ok()?;
    String::from_utf8(plain.to_vec()).ok()
}

// ── Relay framing ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum RelayMsg {
    /// A browser opened the page's socket.
    Open { c: u32 },
    /// That browser went away.
    Close { c: u32 },
    /// The relay needs one of the page's files.
    Get { r: u32, p: String },
}

fn frame(kind: u8, id: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(kind);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn asset_frame(request: u32, path: &str) -> Vec<u8> {
    let (status, content_type, body): (u16, &str, &[u8]) = match web::asset(path) {
        Some((content_type, body)) => (200, content_type, body),
        None => (404, "", b""),
    };
    let mut payload = Vec::with_capacity(3 + content_type.len() + body.len());
    payload.extend_from_slice(&status.to_be_bytes());
    payload.push(content_type.len() as u8);
    payload.extend_from_slice(content_type.as_bytes());
    payload.extend_from_slice(body);
    frame(KIND_ASSET, request, &payload)
}

// ── Tunnel ────────────────────────────────────────────────────────────────────

/// Keep this Kaku registered with the relay for as long as the bridge runs.
pub(crate) async fn run(relay_url: String, dirty: WebDirty) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let identity = match load_or_create_identity() {
            Ok(identity) => identity,
            Err(e) => {
                log::warn!("kaku-share: no share identity: {e:#}");
                tokio::time::sleep(MAX_BACKOFF).await;
                continue;
            }
        };
        let mut connected_at = None;
        match serve(&relay_url, &identity, &dirty, &mut connected_at).await {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => log::info!("kaku-share: relay connection ended: {e:#}"),
        }
        backoff = retry_delay(
            backoff,
            connected_at.map(|at: tokio::time::Instant| at.elapsed()),
        );
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

fn retry_delay(previous: Duration, connected_for: Option<Duration>) -> Duration {
    if connected_for.is_some_and(|duration| duration >= KEEPALIVE) {
        Duration::from_secs(1)
    } else {
        previous
    }
}

fn close_browser(c: u32, code: u16) -> Message {
    Message::Text(
        serde_json::json!({ "t": "close", "c": c, "code": code })
            .to_string()
            .into(),
    )
}

async fn deliver_input(inbox: &mpsc::Sender<String>, text: String, limit: Duration) -> bool {
    matches!(
        tokio::time::timeout(limit, inbox.send(text)).await,
        Ok(Ok(()))
    )
}

enum Browser {
    /// Connected, but has not yet sent a message that opens with the key.
    Pending(tokio::time::Instant),
    Active(mpsc::Sender<String>),
}

type RelaySocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect_relay(
    request: tokio_tungstenite::tungstenite::handshake::client::Request,
    limit: Duration,
) -> anyhow::Result<RelaySocket> {
    // Keystrokes and echoes are tiny frames; Nagle would hold them back.
    let (socket, _) = tokio::time::timeout(
        limit,
        tokio_tungstenite::connect_async_with_config(request, None, true),
    )
    .await
    .map_err(|_| anyhow::anyhow!("relay handshake timed out after {limit:?}"))?
    .context("connect to relay")?;
    Ok(socket)
}

async fn serve(
    relay_url: &str,
    identity: &ShareIdentity,
    dirty: &WebDirty,
    connected_at: &mut Option<tokio::time::Instant>,
) -> anyhow::Result<()> {
    let key = identity.sealing_key()?;
    let id = identity.id();
    let mut request = format!("{}/host/{id}", relay_url.trim_end_matches('/'))
        .into_client_request()
        .context("relay url")?;
    request.headers_mut().insert(
        "x-kaku-secret",
        identity.secret.parse().context("secret header")?,
    );
    let socket = connect_relay(request, CONNECT_TIMEOUT).await?;
    *connected_at = Some(tokio::time::Instant::now());
    log::info!("kaku-share: registered with relay as {id}");
    let (mut relay_tx, mut relay_rx) = socket.split();

    // Everything bound for the relay goes through one writer.
    let (to_relay, mut outgoing) = mpsc::channel::<Message>(256);
    let mut writer = tokio::spawn(async move {
        while let Some(msg) = outgoing.recv().await {
            if !matches!(
                tokio::time::timeout(WRITE_TIMEOUT, relay_tx.send(msg)).await,
                Ok(Ok(()))
            ) {
                break;
            }
        }
    });

    let key = std::sync::Arc::new(key);
    let mut browsers: HashMap<u32, Browser> = HashMap::new();
    let mut identity_check = tokio::time::interval(IDENTITY_CHECK);
    identity_check.tick().await;
    let mut keepalive = tokio::time::interval(KEEPALIVE);
    keepalive.tick().await;
    let mut last_heard = tokio::time::Instant::now();

    let result = 'connection: loop {
        tokio::select! {
            _ = &mut writer => break Err(anyhow::anyhow!("relay writer stopped or timed out")),
            msg = relay_rx.next() => {
                let msg = match msg {
                    Some(Ok(msg)) => msg,
                    Some(Err(e)) => break Err(anyhow::Error::from(e)),
                    None => break Ok(()),
                };
                last_heard = tokio::time::Instant::now();
                match msg {
                    Message::Text(text) => match serde_json::from_str::<RelayMsg>(text.as_str()) {
                        Ok(RelayMsg::Open { c }) => {
                            browsers.insert(c, Browser::Pending(tokio::time::Instant::now()));
                        }
                        Ok(RelayMsg::Close { c }) => {
                            browsers.remove(&c);
                        }
                        Ok(RelayMsg::Get { r, p }) => {
                            if to_relay.try_send(Message::Binary(asset_frame(r, &p).into())).is_err() {
                                break Err(anyhow::anyhow!("relay output queue is full"));
                            }
                        }
                        Err(e) => log::debug!("kaku-share: unknown relay message: {e}"),
                    },
                    Message::Binary(bytes) => {
                        if bytes.len() < 5 || bytes[0] != KIND_DATA {
                            continue;
                        }
                        let c = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
                        let Some(text) = open(&key, &bytes[5..]) else {
                            // Wrong key or tampered: drop this browser.
                            browsers.remove(&c);
                            if to_relay.try_send(close_browser(c, 4403)).is_err() {
                                break Err(anyhow::anyhow!("relay output queue is full"));
                            }
                            continue;
                        };
                        let inbox = match browsers.get(&c) {
                            Some(Browser::Active(inbox)) => inbox.clone(),
                            Some(Browser::Pending(_)) => {
                                let inbox = start_session(c, dirty.clone(), key.clone(), to_relay.clone());
                                browsers.insert(c, Browser::Active(inbox.clone()));
                                inbox
                            }
                            None => continue,
                        };
                        // A burst of typing may briefly fill the mailbox while
                        // the session awaits the GUI. Preserve it, but never
                        // wait indefinitely and starve heartbeat/reset work.
                        if !deliver_input(&inbox, text, INBOX_TIMEOUT).await {
                            browsers.remove(&c);
                            if to_relay.try_send(close_browser(c, 4413)).is_err() {
                                break Err(anyhow::anyhow!("relay output queue is full"));
                            }
                        }
                    }
                    Message::Close(_) => break Ok(()),
                    _ => {}
                }
            }
            _ = keepalive.tick() => {
                if last_heard.elapsed() > SILENCE_LIMIT {
                    break Err(anyhow::anyhow!("relay went silent"));
                }
                if to_relay.try_send(Message::Ping(Vec::new().into())).is_err() {
                    break Ok(());
                }
            }
            _ = identity_check.tick() => {
                let expired: Vec<u32> = browsers.iter().filter_map(|(&id, browser)| {
                    matches!(browser, Browser::Pending(at) if at.elapsed() >= AUTH_TIMEOUT).then_some(id)
                }).collect();
                for c in expired {
                    browsers.remove(&c);
                    if to_relay.try_send(close_browser(c, 4408)).is_err() {
                        break 'connection Err(anyhow::anyhow!("relay output queue is full"));
                    }
                }
                // A reset identity means a new link; re-register under it so
                // the old one stops working.
                if read_identity().as_ref() != Some(identity) {
                    log::info!("kaku-share: share link changed, re-registering");
                    break Ok(());
                }
            }
        }
    };
    drop(browsers);
    writer.abort();
    result
}

/// Start the session for browser `c`; returns where its messages go.
fn start_session(
    c: u32,
    dirty: WebDirty,
    key: std::sync::Arc<LessSafeKey>,
    to_relay: mpsc::Sender<Message>,
) -> mpsc::Sender<String> {
    let (inbox, session_in) = mpsc::channel::<String>(64);
    let (session_out, mut replies) = mpsc::channel::<String>(64);
    tokio::spawn(web::run_session(session_in, session_out, dirty));
    tokio::spawn(async move {
        let rng = SystemRandom::new();
        while let Some(text) = replies.recv().await {
            let Some(sealed) = seal(&key, &rng, text.as_bytes()) else {
                break;
            };
            let msg = Message::Binary(frame(KIND_DATA, c, &sealed).into());
            if !matches!(
                tokio::time::timeout(WRITE_TIMEOUT, to_relay.send(msg)).await,
                Ok(Ok(()))
            ) {
                break;
            }
        }
    });
    inbox
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ShareIdentity {
        ShareIdentity::generate().unwrap()
    }

    #[test]
    fn a_healthy_connection_resets_error_backoff() {
        assert_eq!(
            retry_delay(MAX_BACKOFF, Some(KEEPALIVE)),
            Duration::from_secs(1)
        );
        assert_eq!(retry_delay(MAX_BACKOFF, None), MAX_BACKOFF);
        assert_eq!(
            retry_delay(MAX_BACKOFF, Some(Duration::from_millis(10))),
            MAX_BACKOFF
        );
    }

    #[test]
    fn rejection_carries_a_distinguishable_code() {
        let Message::Text(text) = close_browser(7, 4403) else {
            panic!("control frame")
        };
        let json: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
        assert_eq!(json["code"], 4403);
        assert_eq!(json["c"], 7);
    }

    #[tokio::test]
    async fn input_bursts_wait_for_a_busy_session_without_losing_keys() {
        let (tx, mut rx) = mpsc::channel(64);
        let reader = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut received = Vec::new();
            while let Some(text) = rx.recv().await {
                received.push(text);
            }
            received
        });
        for i in 0..200 {
            assert!(deliver_input(&tx, i.to_string(), INBOX_TIMEOUT).await);
        }
        drop(tx);
        assert_eq!(
            reader.await.unwrap(),
            (0..200).map(|n| n.to_string()).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn a_stalled_session_cannot_block_control_work_forever() {
        let (tx, _rx) = mpsc::channel(1);
        tx.send("first".into()).await.unwrap();
        assert!(!deliver_input(&tx, "second".into(), Duration::from_millis(20)).await);
    }

    #[tokio::test]
    async fn a_relay_that_never_answers_the_handshake_is_given_up_on() {
        // A listener that accepts and stays silent, like a local proxy whose
        // upstream died: the connect succeeds, the handshake never ends.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _held = tokio::spawn(async move {
            let mut kept = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                kept.push(socket);
            }
        });
        let request = format!("ws://{addr}/host/x").into_client_request().unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            connect_relay(request, Duration::from_millis(300)),
        )
        .await
        .expect("connect_relay must give up on its own");
        let Err(err) = result else {
            panic!("a silent relay must not connect");
        };
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
    }

    #[test]
    fn id_is_a_hash_of_the_secret() {
        let a = identity();
        assert_eq!(a.id().len(), 32);
        assert_eq!(a.id(), a.id());
        assert!(!a.id().contains(&a.secret[..16]));
        assert_ne!(a.id(), identity().id());
    }

    #[test]
    fn link_carries_id_and_key_in_fragment() {
        let a = identity();
        let link = a.link("wss://relay.example.com/");
        assert_eq!(
            link,
            format!("https://relay.example.com/{}/#k={}", a.id(), a.key)
        );
        assert!(!link.contains(&a.secret));
    }

    #[test]
    fn sealed_messages_open_only_in_the_other_direction_with_the_key() {
        let a = identity();
        let key = a.sealing_key().unwrap();
        let rng = SystemRandom::new();
        let from_host = seal(&key, &rng, b"hello").unwrap();
        // The page seals with the c2h label; a host-sealed frame must not
        // open as if it came from the page (no reflection).
        assert!(open(&key, &from_host).is_none());

        let mut nonce = [0u8; NONCE_LEN];
        rng.fill(&mut nonce).unwrap();
        let mut buf = b"from page".to_vec();
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(TO_HOST),
            &mut buf,
        )
        .unwrap();
        let mut from_page = nonce.to_vec();
        from_page.extend_from_slice(&buf);
        assert_eq!(open(&key, &from_page).as_deref(), Some("from page"));

        let other = identity().sealing_key().unwrap();
        assert!(open(&other, &from_page).is_none());
    }
}
