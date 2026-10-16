//! This Kaku, reached through the relay by the browsers paired with it.
//!
//! Kaku keeps a share identity on disk: a secret that registers this Mac with
//! the relay and a key that signs each connection's exchange (see
//! `pairing`). The relay knows the Mac only by the SHA-256 of the secret, so
//! nobody else can register as this Mac, and it only ever forwards sealed
//! bytes. Resetting the identity forgets every paired browser: the running
//! Kaku notices the new file and re-registers under the new id.

use crate::pairing::{self, Exchange, MacKey, PageAuth, SessionKeys};
use crate::web::{self, WebDirty};
use anyhow::Context;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use ring::aead::{Aad, LessSafeKey, Nonce, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
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
    /// This Mac's ECDSA P-256 signing key, PKCS#8 base64url. Never leaves the
    /// Mac; pairing links carry its public half. Identities from before
    /// pairing have none (and a `key` field that is no longer read) and get
    /// one on load.
    #[serde(default)]
    mac_key: String,
}

impl ShareIdentity {
    fn generate() -> anyhow::Result<Self> {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).context("random secret")?;
        Ok(Self {
            secret: hex(&secret),
            mac_key: pairing::generate_mac_key()?,
        })
    }

    /// The relay's name for this Mac: the first 128 bits of SHA-256(secret).
    pub fn id(&self) -> String {
        let digest = ring::digest::digest(&ring::digest::SHA256, self.secret.as_bytes());
        hex(&digest.as_ref()[..16])
    }

    fn mac_key(&self) -> anyhow::Result<MacKey> {
        MacKey::from_text(&self.mac_key)
    }

    /// This Mac's page through the relay at `relay_url` (ws:// or wss://).
    /// Opening it pairs nothing; a paired browser bookmarks it.
    pub fn page_url(&self, relay_url: &str) -> String {
        let base = if relay_url.trim_end_matches('/') == crate::DEFAULT_RELAY {
            crate::DEFAULT_PAGE.to_string()
        } else {
            http_base(relay_url)
        };
        format!("{base}/{}/", self.id())
    }

    /// A link that pairs one more browser, once, within
    /// `pairing::OFFER_LIFETIME`; it replaces any earlier unused one.
    pub fn pairing_link(&self, relay_url: &str) -> anyhow::Result<String> {
        let secret = pairing::new_offer()?;
        let mac = self.mac_key()?;
        Ok(format!(
            "{}{}",
            self.page_url(relay_url),
            pairing::pairing_fragment(mac.public_key(), &secret)
        ))
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
    crate::write_private(&path, json.as_bytes()).context("save share identity")
}

fn access_path() -> PathBuf {
    config::DATA_DIR.join("remote-access")
}

/// The relay's access code, once `kaku remote` asked for it.
fn read_access_code() -> Option<String> {
    let code = std::fs::read_to_string(access_path()).ok()?;
    let code = code.trim();
    (!code.is_empty()).then(|| code.to_string())
}

/// Keep the relay's access code for the bridge to present. The running Kaku
/// notices the new file within a second and tries again.
pub fn save_access_code(code: &str) -> anyhow::Result<()> {
    let code = code.trim();
    anyhow::ensure!(!code.is_empty(), "the access code is empty");
    anyhow::ensure!(
        code.len() <= 256 && code.bytes().all(|b| (b' '..=b'~').contains(&b)),
        "an access code is letters, digits and punctuation"
    );
    let path = access_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("create data dir")?;
    }
    crate::write_private(&path, code.as_bytes()).context("save access code")
}

/// Marks a refusal over the access code in the relay status, which
/// `RelayStatus::needs_access_code` looks for.
pub(crate) const ACCESS_REFUSAL: &str = "access code";

fn token_path() -> PathBuf {
    config::DATA_DIR.join("remote-token")
}

/// The relay registration token, if one was set (`kaku remote --token`).
fn read_token() -> Option<String> {
    let token = std::fs::read_to_string(token_path()).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Keep a relay registration token for the bridge to present; the running
/// Kaku uses it from its next connection to the relay.
pub fn save_token(token: &str) -> anyhow::Result<()> {
    let token = token.trim();
    anyhow::ensure!(
        token.split('.').count() == 2 && token.bytes().all(|b| b.is_ascii_graphic()),
        "that does not look like a Kaku relay token"
    );
    let path = token_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).context("create data dir")?;
    }
    crate::write_private(&path, token.as_bytes()).context("save relay token")
}

/// This Kaku's share identity, created on first use.
pub fn load_or_create_identity() -> anyhow::Result<ShareIdentity> {
    if let Some(mut identity) = read_identity() {
        if identity.mac_key.is_empty() {
            identity.mac_key = pairing::generate_mac_key()?;
            write_identity(&identity)?;
        }
        return Ok(identity);
    }
    let identity = ShareIdentity::generate()?;
    write_identity(&identity)?;
    Ok(identity)
}

/// Replace the identity and forget every paired browser; their sessions end
/// once the running Kaku re-registers, within a few seconds.
pub fn reset_identity() -> anyhow::Result<ShareIdentity> {
    let identity = ShareIdentity::generate()?;
    write_identity(&identity)?;
    pairing::forget_all_devices()?;
    Ok(identity)
}

// ── Sealing ───────────────────────────────────────────────────────────────────

/// What a message is bound to besides its connection's key: its direction
/// and its position, so the relay cannot reflect, repeat or reorder messages.
/// Each connection has its own keys, so nothing moves between connections.
fn aad(label: &[u8], seq: u64) -> Vec<u8> {
    let mut aad = label.to_vec();
    aad.extend_from_slice(&seq.to_be_bytes());
    aad
}

fn seal(key: &LessSafeKey, rng: &SystemRandom, aad: &[u8], plain: &[u8]) -> Option<Vec<u8>> {
    let mut nonce = [0u8; NONCE_LEN];
    rng.fill(&mut nonce).ok()?;
    let mut buf = plain.to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(aad),
        &mut buf,
    )
    .ok()?;
    let mut out = Vec::with_capacity(NONCE_LEN + buf.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&buf);
    Some(out)
}

fn open(key: &LessSafeKey, aad: &[u8], sealed: &[u8]) -> Option<String> {
    if sealed.len() < NONCE_LEN {
        return None;
    }
    let (nonce, body) = sealed.split_at(NONCE_LEN);
    let nonce = Nonce::try_assume_unique_for_key(nonce).ok()?;
    let mut buf = body.to_vec();
    let plain = key.open_in_place(nonce, Aad::from(aad), &mut buf).ok()?;
    String::from_utf8(plain.to_vec()).ok()
}

// ── One browser's connection ──────────────────────────────────────────────────

/// Where a browser's connection is. Kaku sends its hello when the browser
/// connects; the page answers with its own, Kaku signs the exchange, the page
/// proves it is a paired device (or pairs with a link), and only then does a
/// session start. Nothing reaches the terminal before that.
enum Stage {
    Hello(Exchange),
    Proof {
        transcript: [u8; 32],
        keys: Arc<SessionKeys>,
    },
    Session {
        keys: Arc<SessionKeys>,
        device: String,
        inbox: Option<mpsc::Sender<String>>,
    },
    Gone,
}

struct Browser {
    stage: Stage,
    /// Position of the next message expected from this browser.
    next_in: u64,
    connected_at: tokio::time::Instant,
}

/// What to do with a browser after its latest message.
enum Step {
    Nothing,
    /// Send it these bytes.
    Send(Vec<u8>),
    /// Turn it away with this close code.
    Close(u16),
    /// It proved it is `device` (`new`: paired just now); start its session.
    Admitted {
        device: pairing::Device,
        new: bool,
    },
    /// A message for its session.
    Input(String),
}

/// The page's first message is its hello; one that is not (a `KCB2`
/// challenge, or a sealed message straight away) is a page from before
/// pairing, open across an update. It is told to reload, which shows the
/// new page and its pairing prompt.
const OLD_PAGE: u16 = 4426;

fn advance(
    browser: &mut Browser,
    data: &[u8],
    host_id: &str,
    mac_key: &MacKey,
    rng: &SystemRandom,
    store: &mut dyn pairing::DeviceStore,
) -> Step {
    match std::mem::replace(&mut browser.stage, Stage::Gone) {
        Stage::Hello(exchange) => {
            if !data.starts_with(pairing::PAGE_HELLO) {
                return Step::Close(OLD_PAGE);
            }
            let Some((transcript, keys)) = exchange.finish(host_id, data) else {
                return Step::Close(4403);
            };
            let Some(sig) = mac_key.sign(&transcript) else {
                return Step::Close(4403);
            };
            let auth = serde_json::json!({
                "type": "mac_auth",
                "sig": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig),
            })
            .to_string();
            let Some(sealed) = seal(&keys.from_host, rng, &aad(FROM_HOST, 0), auth.as_bytes())
            else {
                return Step::Close(4403);
            };
            browser.stage = Stage::Proof {
                transcript,
                keys: Arc::new(keys),
            };
            Step::Send(sealed)
        }
        Stage::Proof { transcript, keys } => {
            let Some(text) = open(&keys.to_host, &aad(TO_HOST, browser.next_in), data) else {
                return Step::Close(4403);
            };
            browser.next_in += 1;
            let Ok(auth) = serde_json::from_str::<PageAuth>(&text) else {
                return Step::Close(4403);
            };
            match pairing::admit(&auth, &transcript, store) {
                Ok((device, new)) => {
                    browser.stage = Stage::Session {
                        keys,
                        device: device.id.clone(),
                        inbox: None,
                    };
                    Step::Admitted { device, new }
                }
                Err(refusal) => Step::Close(refusal.close_code()),
            }
        }
        Stage::Session {
            keys,
            device,
            inbox,
        } => {
            let opened = open(&keys.to_host, &aad(TO_HOST, browser.next_in), data);
            browser.stage = Stage::Session {
                keys,
                device,
                inbox,
            };
            match opened {
                Some(text) => {
                    browser.next_in += 1;
                    Step::Input(text)
                }
                // Tampered, replayed or out of order.
                None => Step::Close(4403),
            }
        }
        Stage::Gone => Step::Nothing,
    }
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
            Ok(()) => {
                backoff = Duration::from_secs(1);
                crate::record_relay_status(&relay_url, false, None);
            }
            Err(e) => {
                log::info!("kaku-share: relay connection ended: {e:#}");
                crate::record_relay_status(&relay_url, false, Some(format!("{e:#}")));
            }
        }
        backoff = retry_delay(
            backoff,
            connected_at.map(|at: tokio::time::Instant| at.elapsed()),
        );
        if wait_unless_access_changes(backoff).await {
            backoff = Duration::from_secs(1);
        } else {
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }
}

/// Sleep `delay`, or less when `kaku remote` saves a new access code, so the
/// person typing it is not left waiting out a 30 s backoff. True when it did.
async fn wait_unless_access_changes(delay: Duration) -> bool {
    let stamp = || {
        std::fs::metadata(access_path())
            .and_then(|m| m.modified())
            .ok()
    };
    let before = stamp();
    let deadline = tokio::time::Instant::now() + delay;
    while tokio::time::Instant::now() < deadline {
        let step = (deadline - tokio::time::Instant::now()).min(Duration::from_secs(1));
        tokio::time::sleep(step).await;
        if stamp() != before {
            return true;
        }
    }
    false
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

type RelaySocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect_relay(
    request: tokio_tungstenite::tungstenite::handshake::client::Request,
    limit: Duration,
) -> anyhow::Result<RelaySocket> {
    // Keystrokes and echoes are tiny frames; Nagle would hold them back.
    let connected = tokio::time::timeout(
        limit,
        tokio_tungstenite::connect_async_with_config(request, None, true),
    )
    .await
    .map_err(|_| anyhow::anyhow!("relay handshake timed out after {limit:?}"))?;
    match connected {
        Ok((socket, _)) => Ok(socket),
        // The relay says why in plain text: an access code it needs or did
        // not accept, a full address, a busy relay.
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            let why = response
                .body()
                .as_deref()
                .map(String::from_utf8_lossy)
                .unwrap_or_default()
                .trim()
                .chars()
                .take(120)
                .collect::<String>();
            anyhow::bail!("relay refused this Mac ({}): {why}", response.status())
        }
        Err(e) => Err(anyhow::Error::new(e).context("connect to relay")),
    }
}

async fn serve(
    relay_url: &str,
    identity: &ShareIdentity,
    dirty: &WebDirty,
    connected_at: &mut Option<tokio::time::Instant>,
) -> anyhow::Result<()> {
    let mac_key = identity.mac_key()?;
    let id = identity.id();
    let mut request = format!("{}/host/{id}", relay_url.trim_end_matches('/'))
        .into_client_request()
        .context("relay url")?;
    request.headers_mut().insert(
        "x-kaku-secret",
        identity.secret.parse().context("secret header")?,
    );
    if let Some(token) = read_token() {
        if let Ok(value) = token.parse() {
            request.headers_mut().insert("x-kaku-token", value);
        }
    }
    if let Some(code) = read_access_code() {
        if let Ok(value) = code.parse() {
            request.headers_mut().insert("x-kaku-access", value);
        }
    }
    let socket = connect_relay(request, CONNECT_TIMEOUT).await?;
    *connected_at = Some(tokio::time::Instant::now());
    log::info!("kaku-share: registered with relay as {id}");
    crate::record_relay_status(relay_url, true, None);
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

    let rng = SystemRandom::new();
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
                            let Some(exchange) = Exchange::new(&rng) else { continue };
                            let hello = exchange.hello();
                            if to_relay.try_send(Message::Binary(frame(KIND_DATA, c, &hello).into())).is_err() {
                                break Err(anyhow::anyhow!("relay output queue is full"));
                            }
                            browsers.insert(c, Browser {
                                stage: Stage::Hello(exchange),
                                next_in: 0,
                                connected_at: tokio::time::Instant::now(),
                            });
                        }
                        Ok(RelayMsg::Close { c }) => {
                            browsers.remove(&c);
                        }
                        Ok(RelayMsg::Get { r, p }) => {
                            // Anyone who knows this Mac's id can ask for the
                            // page's files, without the key. Answer only while
                            // half the queue is free, so those requests can
                            // never crowd out a session's frames or drop the
                            // link; an unanswered one times out at the relay.
                            if to_relay.capacity() * 2 < to_relay.max_capacity() {
                                log::debug!("kaku-share: busy, not serving {p}");
                                continue;
                            }
                            let _ = to_relay.try_send(Message::Binary(asset_frame(r, &p).into()));
                        }
                        Err(e) => log::debug!("kaku-share: unknown relay message: {e}"),
                    },
                    Message::Binary(bytes) => {
                        if bytes.len() < 5 || bytes[0] != KIND_DATA {
                            continue;
                        }
                        let c = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
                        let Some(browser) = browsers.get_mut(&c) else { continue };
                        let step = advance(browser, &bytes[5..], &id, &mac_key, &rng, &mut pairing::DiskStore);
                        let text = match step {
                            Step::Nothing => continue,
                            Step::Send(bytes) => {
                                if to_relay.try_send(Message::Binary(frame(KIND_DATA, c, &bytes).into())).is_err() {
                                    break Err(anyhow::anyhow!("relay output queue is full"));
                                }
                                continue;
                            }
                            Step::Close(code) => {
                                browsers.remove(&c);
                                if to_relay.try_send(close_browser(c, code)).is_err() {
                                    break Err(anyhow::anyhow!("relay output queue is full"));
                                }
                                continue;
                            }
                            Step::Admitted { device, new } => {
                                if new {
                                    log::info!("kaku-share: paired {} ({})", device.name, device.id);
                                    pairing::announce(&device);
                                } else {
                                    pairing::announce_return(&device);
                                }
                                if let Stage::Session { keys, inbox, .. } = &mut browser.stage {
                                    // Kaku's own signature was message 0.
                                    *inbox = Some(start_session(c, keys.clone(), 1, dirty.clone(), to_relay.clone()));
                                }
                                continue;
                            }
                            Step::Input(text) => text,
                        };
                        let Stage::Session { inbox: Some(inbox), .. } = &browser.stage else { continue };
                        let inbox = inbox.clone();
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
                let paired = pairing::paired_ids();
                let ended: Vec<(u32, u16)> = browsers.iter().filter_map(|(&c, browser)| match &browser.stage {
                    // Removed with `kaku remote revoke`: end its session.
                    Stage::Session { device, .. } if !paired.contains(device) => Some((c, 4403)),
                    Stage::Session { .. } => None,
                    _ if browser.connected_at.elapsed() >= AUTH_TIMEOUT => Some((c, 4408)),
                    _ => None,
                }).collect();
                for (c, code) in ended {
                    browsers.remove(&c);
                    if to_relay.try_send(close_browser(c, code)).is_err() {
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

/// Start the session for browser `c`; returns where its messages go. Kaku's
/// messages to it are numbered from `first_seq`.
fn start_session(
    c: u32,
    keys: Arc<SessionKeys>,
    first_seq: u64,
    dirty: WebDirty,
    to_relay: mpsc::Sender<Message>,
) -> mpsc::Sender<String> {
    let (inbox, session_in) = mpsc::channel::<String>(64);
    let (session_out, mut replies) = mpsc::channel::<String>(64);
    tokio::spawn(web::run_session(session_in, session_out, dirty));
    tokio::spawn(async move {
        let rng = SystemRandom::new();
        let mut seq = first_seq;
        while let Some(text) = replies.recv().await {
            let Some(sealed) = seal(&keys.from_host, &rng, &aad(FROM_HOST, seq), text.as_bytes())
            else {
                break;
            };
            seq += 1;
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
    fn the_pairing_fragment_carries_the_mac_key_but_no_secret_of_the_mac() {
        let a = identity();
        let mac = a.mac_key().unwrap();
        assert_eq!(
            a.page_url("wss://relay.example.com/"),
            format!("https://relay.example.com/{}/", a.id())
        );
        // The default relay's page lives on its own origin.
        assert_eq!(
            a.page_url("wss://r.kaku.fun"),
            format!("https://t.kaku.fun/{}/", a.id())
        );
        let fragment = pairing::pairing_fragment(mac.public_key(), "c2VjcmV0");
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        assert_eq!(fragment, format!("#p=c2VjcmV0&m={}", b64(mac.public_key())));
        assert!(!fragment.contains(&a.secret) && !fragment.contains(&a.mac_key));
    }

    #[test]
    fn sealed_messages_open_only_in_their_direction_and_place() {
        let rng = SystemRandom::new();
        let keys = exchanged().1;
        let msg = seal(&keys.to_host, &rng, &aad(TO_HOST, 0), b"ls\r").unwrap();
        assert_eq!(
            open(&keys.to_host, &aad(TO_HOST, 0), &msg).as_deref(),
            Some("ls\r")
        );
        // Reflected back as if the Mac sent it, or replayed later.
        assert!(open(&keys.from_host, &aad(FROM_HOST, 0), &msg).is_none());
        assert!(open(&keys.to_host, &aad(TO_HOST, 1), &msg).is_none());
        // Another connection's keys open nothing.
        assert!(open(&exchanged().1.to_host, &aad(TO_HOST, 0), &msg).is_none());
    }

    use crate::pairing::tests::{MemoryStore, TestPage};
    use crate::pairing::DeviceStore;

    fn exchanged() -> ([u8; 32], SessionKeys) {
        let rng = SystemRandom::new();
        let mac = Exchange::new(&rng).unwrap();
        let page = TestPage::new();
        mac.finish("host", &page.hello()).unwrap()
    }

    struct Fixture {
        identity: ShareIdentity,
        mac_key: MacKey,
        store: MemoryStore,
        rng: SystemRandom,
    }

    /// A browser connected to Kaku, holding the keys its page derived.
    struct Connected {
        browser: Browser,
        keys: SessionKeys,
        transcript: [u8; 32],
        device: MacKey,
        sent: u64,
    }

    impl Fixture {
        fn new() -> Self {
            let identity = identity();
            let mac_key = identity.mac_key().unwrap();
            Self {
                identity,
                mac_key,
                store: MemoryStore::default(),
                rng: SystemRandom::new(),
            }
        }

        fn step(&mut self, browser: &mut Browser, data: &[u8]) -> Step {
            let id = self.identity.id();
            advance(
                browser,
                data,
                &id,
                &self.mac_key,
                &self.rng,
                &mut self.store,
            )
        }

        /// Connect a page and check Kaku's signature the way the page does.
        fn connect(&mut self) -> Connected {
            let exchange = Exchange::new(&self.rng).unwrap();
            let mac_hello = exchange.hello();
            let mut browser = Browser {
                stage: Stage::Hello(exchange),
                next_in: 0,
                connected_at: tokio::time::Instant::now(),
            };
            let page = TestPage::new();
            let Step::Send(signed) = self.step(&mut browser, &page.hello()) else {
                panic!("Kaku answers the page's hello with its signature");
            };
            let (transcript, keys, device) = page.finish(&self.identity.id(), &mac_hello);
            let auth = open(&keys.from_host, &aad(FROM_HOST, 0), &signed).unwrap();
            let auth: serde_json::Value = serde_json::from_str(&auth).unwrap();
            assert_eq!(auth["type"], "mac_auth");
            let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(auth["sig"].as_str().unwrap())
                .unwrap();
            assert!(pairing::verify(
                self.mac_key.public_key(),
                &transcript,
                &sig
            ));
            Connected {
                browser,
                keys,
                transcript,
                device,
                sent: 0,
            }
        }

        fn send(&mut self, conn: &mut Connected, text: &str) -> Step {
            let sealed = seal(
                &conn.keys.to_host,
                &self.rng,
                &aad(TO_HOST, conn.sent),
                text.as_bytes(),
            )
            .unwrap();
            conn.sent += 1;
            self.step(&mut conn.browser, &sealed)
        }

        fn pair(&mut self, conn: &Connected) -> String {
            let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
            let id = pairing::device_id(conn.device.public_key());
            self.store.add(pairing::Device {
                id: id.clone(),
                public_key: b64(conn.device.public_key()),
                name: String::new(),
                paired_at: 0,
                last_seen: 0,
            });
            id
        }
    }

    fn sign_in(conn: &Connected) -> String {
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        serde_json::json!({
            "type": "auth",
            "device": b64(conn.device.public_key()),
            "sig": b64(&conn.device.sign(&conn.transcript).unwrap()),
        })
        .to_string()
    }

    fn join(conn: &Connected, secret: &[u8]) -> String {
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        let proof = ring::hmac::sign(
            &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret),
            &conn.transcript,
        );
        serde_json::json!({
            "type": "pair",
            "device": b64(conn.device.public_key()),
            "sig": b64(&conn.device.sign(&conn.transcript).unwrap()),
            "proof": b64(proof.as_ref()),
            "name": "Safari on iPhone",
        })
        .to_string()
    }

    #[test]
    fn a_paired_browser_reaches_its_session_and_nothing_replays() {
        let mut fx = Fixture::new();
        let mut conn = fx.connect();
        let id = fx.pair(&conn);
        let Step::Admitted { device, new } = ({
            let m = sign_in(&conn);
            fx.send(&mut conn, &m)
        }) else {
            panic!("a paired device signs in");
        };
        assert_eq!((device.id, new), (id, false));
        // Its session takes input in order; a replayed message ends it.
        assert!(matches!(
            fx.send(&mut conn, r#"{"type":"ping"}"#),
            Step::Input(_)
        ));
        conn.sent -= 1;
        assert!(matches!(
            fx.send(&mut conn, r#"{"type":"ping"}"#),
            Step::Close(4403)
        ));
    }

    #[test]
    fn a_pairing_link_admits_a_new_browser_which_then_signs_in_without_it() {
        let mut fx = Fixture::new();
        fx.store.offer = Some((vec![1u8; 16], false));
        let mut conn = fx.connect();
        let Step::Admitted { device, new } = ({
            let m = join(&conn, &[1u8; 16]);
            fx.send(&mut conn, &m)
        }) else {
            panic!("a pairing link admits the browser");
        };
        assert!(new);
        assert_eq!(device.name, "Safari on iPhone");
        let mut again = fx.connect();
        again.device = conn.device;
        let Step::Admitted { device: same, new } = ({
            let m = sign_in(&again);
            fx.send(&mut again, &m)
        }) else {
            panic!("the paired browser signs in later");
        };
        assert_eq!((same.id, new), (device.id, false));
    }

    #[test]
    fn an_unpaired_browser_never_reaches_a_session() {
        let mut fx = Fixture::new();
        let mut conn = fx.connect();
        assert!(matches!(
            {
                let m = sign_in(&conn);
                fx.send(&mut conn, &m)
            },
            Step::Close(4403)
        ));
        // Skipping the proof and going straight to the terminal is refused too.
        let mut conn = fx.connect();
        assert!(matches!(
            fx.send(&mut conn, r#"{"type":"hello","frame_ack":true}"#),
            Step::Close(4403)
        ));
    }

    #[test]
    fn a_used_or_wrong_pairing_link_gets_its_own_close_code() {
        let mut fx = Fixture::new();
        let mut conn = fx.connect();
        assert!(matches!(
            {
                let m = join(&conn, &[1u8; 16]);
                fx.send(&mut conn, &m)
            },
            Step::Close(4401)
        ));
        fx.store.offer = Some((vec![1u8; 16], true));
        let mut conn = fx.connect();
        assert!(matches!(
            {
                let m = join(&conn, &[1u8; 16]);
                fx.send(&mut conn, &m)
            },
            Step::Close(4401)
        ));
    }

    #[test]
    fn a_page_from_before_pairing_is_told_to_reload() {
        let mut fx = Fixture::new();
        let exchange = Exchange::new(&fx.rng).unwrap();
        let mut browser = Browser {
            stage: Stage::Hello(exchange),
            next_in: 0,
            connected_at: tokio::time::Instant::now(),
        };
        let mut old = b"KCB2".to_vec();
        old.extend_from_slice(&[0u8; 16]);
        assert!(matches!(fx.step(&mut browser, &old), Step::Close(OLD_PAGE)));
    }
}
