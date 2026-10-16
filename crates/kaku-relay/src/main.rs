//! Relay between a Kaku on someone's Mac and the browsers opening its web link.
//!
//! The Mac connects out to `/host/{id}` and proves it owns `id`, which is the
//! first 128 bits of SHA-256 of a secret only the Mac holds. Browsers load
//! `/t/{id}/`, whose files the relay fetches from that Mac, and open
//! `/t/{id}/ws`. Messages between a browser and its Mac are sealed end to end
//! with a key carried in the link's fragment, so the relay only numbers the
//! browsers and forwards bytes it cannot read.

mod token;

use axum::extract::{ws, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tracing::info;

const MAX_HOSTS: usize = 10_000;
const MAX_BROWSERS_PER_HOST: usize = 8;
/// Browser sockets per client address for one host, so one address cannot
/// take every slot with sockets that never authenticate.
const MAX_BROWSERS_PER_ADDR: usize = 4;
/// Registrations kept per id: the serving one and one standby (a second
/// Kaku on the same Mac). A newer one pushes the oldest out.
const MAX_REGISTRATIONS_PER_ID: usize = 2;
/// Macs one address may keep registered at once; an office NAT can hold a
/// few dozen Kaku users.
const MAX_HOSTS_PER_ADDR: usize = 32;
/// New relay sockets (Macs and browsers) one address may open per minute.
const CONNECTS_PER_MINUTE: u32 = 120;
/// Wrong access codes one address may try before it waits out the window.
const ACCESS_FAILURES: u32 = 10;
const ACCESS_WINDOW: Duration = Duration::from_secs(10 * 60);
/// A Mac sends frames of a whole screen; a page sends keys, pastes capped at
/// 64 KB and 64 KB upload chunks.
const MAX_HOST_MESSAGE: usize = 4 << 20;
const MAX_BROWSER_MESSAGE: usize = 512 << 10;
/// Messages queued toward one socket before it counts as stuck.
const QUEUE: usize = 256;
/// Bytes queued toward one Mac. Counting messages alone let a host that
/// stopped reading hold hundreds of megabytes.
const HOST_QUEUE_BYTES: usize = 8 << 20;
/// Bytes queued toward one browser. Only a count bounded it before, and 256
/// Mac messages of up to 4 MB each let one stalled page hold a gigabyte.
const BROWSER_QUEUE_BYTES: usize = 8 << 20;
/// Bytes queued toward every socket together. Per-socket budgets alone add
/// up past the machine's 256 MB once enough sockets stall at once; past this
/// the socket that wants to queue more is let go.
const GLOBAL_QUEUE_BYTES: usize = 96 << 20;
/// The Mac pings every 20 s and a page every 20 s (a background tab about
/// once a minute); silence past these means a half-open socket.
const HOST_SILENCE: Duration = Duration::from_secs(75);
const BROWSER_SILENCE: Duration = Duration::from_secs(150);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const KIND_DATA: u8 = 0;

// ── State ─────────────────────────────────────────────────────────────────────

/// The web terminal's page and files, built into the relay from the same
/// sources Kaku serves on the LAN. Every Mac's page shares this origin, so
/// it must not be whatever each Mac sends: a Mac whose owner altered its
/// page could otherwise use the keys a browser holds for other Macs.
fn page_file(name: &str) -> Option<(&'static str, &'static [u8])> {
    Some(match name {
        "index.html" => (
            "text/html; charset=utf-8",
            include_bytes!("../../kaku-remote/assets/web/index.html"),
        ),
        "xterm.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("../../kaku-remote/assets/web/xterm.js"),
        ),
        "xterm-webgl.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("../../kaku-remote/assets/web/xterm-webgl.js"),
        ),
        "xterm.css" => (
            "text/css; charset=utf-8",
            include_bytes!("../../kaku-remote/assets/web/xterm.css"),
        ),
        "icon.png" => (
            "image/png",
            include_bytes!("../../kaku-remote/assets/web/icon.png"),
        ),
        "font-regular.ttf" => (
            "font/ttf",
            include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf"),
        ),
        "font-medium.ttf" => (
            "font/ttf",
            include_bytes!("../../../assets/fonts/JetBrainsMono-Medium.ttf"),
        ),
        _ => return None,
    })
}

/// Bytes waiting in one socket's outgoing queue, within its own budget and
/// the relay-wide one. Whatever is still queued when the last holder drops it
/// (the socket ended with messages unsent) goes back to the relay-wide count.
struct Queued {
    bytes: AtomicUsize,
    cap: usize,
    global: Arc<AtomicUsize>,
}

impl Queued {
    fn new(cap: usize, global: &Arc<AtomicUsize>) -> Self {
        Self {
            bytes: AtomicUsize::new(0),
            cap,
            global: global.clone(),
        }
    }

    fn reserve(&self, len: usize) -> bool {
        if self.bytes.fetch_add(len, Ordering::Relaxed) + len > self.cap {
            self.bytes.fetch_sub(len, Ordering::Relaxed);
            return false;
        }
        if self.global.fetch_add(len, Ordering::Relaxed) + len > GLOBAL_QUEUE_BYTES {
            self.global.fetch_sub(len, Ordering::Relaxed);
            self.bytes.fetch_sub(len, Ordering::Relaxed);
            return false;
        }
        true
    }

    fn release(&self, len: usize) {
        self.bytes.fetch_sub(len, Ordering::Relaxed);
        self.global.fetch_sub(len, Ordering::Relaxed);
    }

    #[cfg(test)]
    fn load(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }
}

impl Drop for Queued {
    fn drop(&mut self) {
        self.global
            .fetch_sub(*self.bytes.get_mut(), Ordering::Relaxed);
    }
}

struct BrowserLink {
    data: mpsc::Sender<ws::Message>,
    queued: Arc<Queued>,
    close: watch::Sender<Option<u64>>,
    addr: String,
}

struct Host {
    /// Distinguishes this registration from a later one under the same id.
    generation: u64,
    to_host: mpsc::Sender<ws::Message>,
    /// Bytes waiting in `to_host`.
    queued: Queued,
    /// Ends this registration when newer ones push it out.
    evicted: tokio::sync::Notify,
    browsers: Mutex<HashMap<u32, BrowserLink>>,
    next_id: AtomicU32,
    /// The client address it registered from (`MAX_HOSTS_PER_ADDR`).
    addr: String,
}

fn message_len(msg: &ws::Message) -> usize {
    match msg {
        ws::Message::Binary(b) => b.len(),
        ws::Message::Text(t) => t.len(),
        _ => 0,
    }
}

impl Host {
    /// Queue a message for the Mac within the byte budgets.
    fn send(&self, msg: ws::Message) -> bool {
        let len = message_len(&msg);
        if !self.queued.reserve(len) {
            return false;
        }
        if self.to_host.try_send(msg).is_err() {
            self.queued.release(len);
            return false;
        }
        true
    }
}

impl BrowserLink {
    /// Queue a message for the browser within the byte budgets.
    fn send(&self, msg: ws::Message) -> bool {
        let len = message_len(&msg);
        if !self.queued.reserve(len) {
            return false;
        }
        if self.data.try_send(msg).is_err() {
            self.queued.release(len);
            return false;
        }
        true
    }
}

#[derive(Clone, Default)]
struct AppState {
    /// Every connected registration per id; the last one serves browsers.
    /// Two Kakus can share an identity (a second instance on the same Mac):
    /// when the newer one leaves, the older one, still connected, takes over
    /// instead of the link reading offline while a Kaku is running.
    hosts: Arc<Mutex<HashMap<String, Vec<Arc<Host>>>>>,
    generation: Arc<AtomicU64>,
    /// Registration tokens (`token`): the public key to check them with,
    /// and whether a Mac without a valid one is turned away. Unset, every
    /// Mac registers as before.
    tokens: Option<Arc<TokenPolicy>>,
    /// The access code a Mac must present to register; unset, none is asked.
    access: Option<Arc<AccessCode>>,
    limits: Arc<Mutex<AddrLimits>>,
    /// Where the page lives when it is not served here
    /// (`KAKU_RELAY_PAGE_ORIGIN`, e.g. `https://t.kaku.fun`): page addresses
    /// redirect there and browsers are admitted only from it. Unset (local
    /// runs, tests), the relay serves its own copy to any origin.
    page_origin: Option<Arc<str>>,
    /// Bytes queued toward every socket (`GLOBAL_QUEUE_BYTES`).
    queued_bytes: Arc<AtomicUsize>,
}

impl AppState {
    fn host(&self, id: &str) -> Option<Arc<Host>> {
        self.hosts
            .lock()
            .get(id)
            .and_then(|all| all.last().cloned())
    }

    /// Make `host` the one serving `id`; returns the one it displaced.
    fn register(&self, id: &str, host: Arc<Host>) -> Option<Arc<Host>> {
        let mut hosts = self.hosts.lock();
        let all = hosts.entry(id.to_string()).or_default();
        let displaced = all.last().cloned();
        all.push(host);
        while all.len() > MAX_REGISTRATIONS_PER_ID {
            let oldest = all.remove(0);
            oldest.evicted.notify_one();
        }
        displaced
    }

    /// Drop this registration; returns how many remain for `id`.
    fn unregister(&self, id: &str, generation: u64) -> usize {
        let mut hosts = self.hosts.lock();
        let Some(all) = hosts.get_mut(id) else {
            return 0;
        };
        all.retain(|h| h.generation != generation);
        let left = all.len();
        if left == 0 {
            hosts.remove(id);
        }
        left
    }
}

fn short(id: &str) -> &str {
    &id[..8.min(id.len())]
}

fn owns(id: &str, secret: &str) -> bool {
    let digest = Sha256::digest(secret.as_bytes());
    let expected: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    expected == id
}

/// The fly edge that accepted the connection, so latency can be reasoned
/// about from logs: a Mac and its browsers entering far apart cost a long
/// hop per keystroke. Only the region code is recorded.
fn edge(headers: &HeaderMap) -> String {
    headers
        .get("fly-region")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string()
}

/// Browser and OS family from the User-Agent, enough to tell a laptop from
/// a phone in the logs without recording anything that identifies a person.
fn device(headers: &HeaderMap) -> &'static str {
    let ua = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let os = if ua.contains("iPhone") {
        "iPhone"
    } else if ua.contains("iPad") {
        "iPad"
    } else if ua.contains("Android") {
        "Android"
    } else if ua.contains("Windows") {
        "Windows"
    } else if ua.contains("Mac OS X") || ua.contains("Macintosh") {
        "macOS"
    } else if ua.contains("Linux") {
        "Linux"
    } else {
        "other"
    };
    let browser = if ua.contains("Edg/") {
        "Edge"
    } else if ua.contains("Chrome/") || ua.contains("CriOS/") {
        "Chrome"
    } else if ua.contains("Firefox/") || ua.contains("FxiOS/") {
        "Firefox"
    } else if ua.contains("Safari/") {
        "Safari"
    } else {
        "other"
    };
    match (browser, os) {
        ("Chrome", "Windows") => "Chrome Windows",
        ("Chrome", "macOS") => "Chrome macOS",
        ("Chrome", "Android") => "Chrome Android",
        ("Chrome", "iPhone") => "Chrome iPhone",
        ("Edge", "Windows") => "Edge Windows",
        ("Edge", "macOS") => "Edge macOS",
        ("Safari", "macOS") => "Safari macOS",
        ("Safari", "iPhone") => "Safari iPhone",
        ("Safari", "iPad") => "Safari iPad",
        ("Firefox", "Windows") => "Firefox Windows",
        ("Firefox", "macOS") => "Firefox macOS",
        (_, "iPhone") => "iPhone",
        (_, "iPad") => "iPad",
        (_, "Android") => "Android",
        (_, "Windows") => "Windows",
        (_, "macOS") => "macOS",
        _ => "other",
    }
}

fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

// ── Host side ─────────────────────────────────────────────────────────────────

struct TokenPolicy {
    public_key: Vec<u8>,
    required: bool,
}

impl TokenPolicy {
    fn from_env() -> Option<Self> {
        let key = std::env::var("KAKU_RELAY_TOKEN_KEY").ok()?;
        let Some(public_key) = token::decode_key(&key) else {
            tracing::warn!("KAKU_RELAY_TOKEN_KEY is not base64url; tokens are not checked");
            return None;
        };
        Some(Self {
            public_key,
            required: std::env::var("KAKU_RELAY_REQUIRE_TOKEN").as_deref() == Ok("1"),
        })
    }

    /// Whether a Mac presenting `presented` may register as `id` at `now`.
    fn admits(&self, id: &str, presented: Option<&str>, now: u64) -> bool {
        let checked = presented.map(|t| token::verify(t, &self.public_key, id, now));
        match checked {
            Some(Ok(_)) => true,
            Some(Err(why)) => {
                info!(host = short(id), ?why, "registration token refused");
                !self.required
            }
            None => !self.required,
        }
    }
}

/// The access code Macs present to register (`KAKU_RELAY_ACCESS_CODE`, a
/// fly secret), kept as its SHA-256 and compared in constant time.
struct AccessCode {
    digest: Vec<u8>,
}

impl AccessCode {
    fn new(code: &str) -> Self {
        Self {
            digest: ring::digest::digest(&ring::digest::SHA256, code.trim().as_bytes())
                .as_ref()
                .to_vec(),
        }
    }

    fn from_env() -> Option<Self> {
        let code = std::env::var("KAKU_RELAY_ACCESS_CODE").ok()?;
        (!code.trim().is_empty()).then(|| Self::new(&code))
    }

    fn admits(&self, presented: &str) -> bool {
        let digest = ring::digest::digest(&ring::digest::SHA256, presented.trim().as_bytes());
        digest
            .as_ref()
            .iter()
            .zip(&self.digest)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }
}

/// Per-address budgets: new sockets per minute, and wrong access codes per
/// `ACCESS_WINDOW`. Local runs have no client address (`-`) and are not
/// limited, so tests can open as many sockets as they need.
#[derive(Default)]
struct AddrLimits {
    connects: HashMap<String, (Instant, u32)>,
    failures: HashMap<String, (Instant, u32)>,
}

impl AddrLimits {
    fn count(
        map: &mut HashMap<String, (Instant, u32)>,
        addr: &str,
        window: Duration,
        now: Instant,
    ) -> u32 {
        if map.len() > 10_000 {
            map.retain(|_, (start, _)| now.duration_since(*start) < window);
        }
        let entry = map.entry(addr.to_string()).or_insert((now, 0));
        if now.duration_since(entry.0) >= window {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1
    }

    fn allow_connect(&mut self, addr: &str, now: Instant) -> bool {
        addr == "-"
            || Self::count(&mut self.connects, addr, Duration::from_secs(60), now)
                <= CONNECTS_PER_MINUTE
    }

    fn access_locked(&self, addr: &str, now: Instant) -> bool {
        addr != "-"
            && self.failures.get(addr).is_some_and(|(start, n)| {
                now.duration_since(*start) < ACCESS_WINDOW && *n >= ACCESS_FAILURES
            })
    }

    fn note_access_failure(&mut self, addr: &str, now: Instant) {
        if addr != "-" {
            Self::count(&mut self.failures, addr, ACCESS_WINDOW, now);
        }
    }
}

/// Fly puts the real client address in `fly-client-ip`; without it (local
/// runs) every client shares the `-` bucket.
fn client_addr(headers: &HeaderMap) -> String {
    headers
        .get("fly-client-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string()
}

/// A refusal the Mac shows its user: the status and why, as plain text.
fn refuse(status: StatusCode, why: &'static str) -> Response {
    (status, why).into_response()
}

async fn host_ws(
    Path(id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: ws::WebSocketUpgrade,
) -> Response {
    let secret = headers
        .get("x-kaku-secret")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !valid_id(&id) || !owns(&id, secret) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let addr = client_addr(&headers);
    let now = Instant::now();
    if !state.limits.lock().allow_connect(&addr, now) {
        return refuse(StatusCode::TOO_MANY_REQUESTS, "too many connections");
    }
    if let Some(access) = &state.access {
        if state.limits.lock().access_locked(&addr, now) {
            return refuse(StatusCode::TOO_MANY_REQUESTS, "too many wrong access codes");
        }
        match headers.get("x-kaku-access").and_then(|v| v.to_str().ok()) {
            None => return refuse(StatusCode::UNAUTHORIZED, "access code required"),
            Some(code) if !access.admits(code) => {
                state.limits.lock().note_access_failure(&addr, now);
                info!(host = short(&id), "wrong access code");
                return refuse(StatusCode::UNAUTHORIZED, "wrong access code");
            }
            Some(_) => {}
        }
    }
    if let Some(policy) = &state.tokens {
        let presented = headers.get("x-kaku-token").and_then(|v| v.to_str().ok());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if !policy.admits(&id, presented, now) {
            return refuse(StatusCode::UNAUTHORIZED, "registration token refused");
        }
    }
    {
        let hosts = state.hosts.lock();
        if hosts.len() >= MAX_HOSTS && !hosts.contains_key(&id) {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        let from_addr = hosts
            .iter()
            .filter(|(other, all)| *other != &id && all.iter().any(|h| h.addr == addr))
            .count();
        if addr != "-" && from_addr >= MAX_HOSTS_PER_ADDR {
            return refuse(
                StatusCode::TOO_MANY_REQUESTS,
                "too many Macs from this address",
            );
        }
    }
    upgrade
        .max_message_size(MAX_HOST_MESSAGE)
        .on_upgrade(move |socket| serve_host(socket, id, edge(&headers), addr, state))
}

async fn serve_host(
    socket: ws::WebSocket,
    id: String,
    edge: String,
    addr: String,
    state: AppState,
) {
    let (mut sink, mut stream) = socket.split();
    let (to_host, mut outgoing) = mpsc::channel::<ws::Message>(QUEUE);
    let host = Arc::new(Host {
        generation: state.generation.fetch_add(1, Ordering::Relaxed),
        to_host,
        queued: Queued::new(HOST_QUEUE_BYTES, &state.queued_bytes),
        evicted: tokio::sync::Notify::new(),
        browsers: Mutex::new(HashMap::new()),
        next_id: AtomicU32::new(1),
        addr,
    });
    // The newest registration serves; the previous one stays connected as a
    // standby, and its browsers reconnect to this one.
    if let Some(old) = state.register(&id, host.clone()) {
        old.browsers.lock().clear();
    }
    info!("host {} registered via edge {}", short(&id), edge);

    let writer_host = host.clone();
    let mut writer = tokio::spawn(async move {
        while let Some(msg) = outgoing.recv().await {
            writer_host.queued.release(message_len(&msg));
            if !matches!(
                tokio::time::timeout(WRITE_TIMEOUT, sink.send(msg)).await,
                Ok(Ok(()))
            ) {
                break;
            }
        }
    });

    let connected_at = tokio::time::Instant::now();
    let ended = loop {
        let msg = tokio::select! {
            _ = &mut writer => break "writer-stopped",
            _ = host.evicted.notified() => break "evicted",
            _ = tokio::time::sleep(HOST_SILENCE) => break "silent",
            next = stream.next() => match next {
                Some(Ok(msg)) => msg,
                Some(Err(error)) => {
                    info!(host = short(&id), error = %error, "host transport failed");
                    break "socket-error";
                }
                None => break "eof",
            }
        };
        match msg {
            // Older Kakus also answer page file requests (kind 1); the relay
            // serves its own copy now and asks for none.
            ws::Message::Binary(bytes) if bytes.len() >= 5 && bytes[0] == KIND_DATA => {
                let target = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
                let mut browsers = host.browsers.lock();
                if let Some(link) = browsers.get(&target) {
                    let payload = bytes.slice(5..);
                    if !link.send(ws::Message::Binary(payload)) {
                        // A browser that cannot keep up is let go.
                        browsers.remove(&target);
                    }
                }
            }
            ws::Message::Text(text) => {
                // The Mac rejected a browser (its key did not match).
                if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(text.as_str()) {
                    if map.get("t").and_then(|t| t.as_str()) == Some("close") {
                        if let Some(c) = map.get("c").and_then(|c| c.as_u64()) {
                            if let Some(tx) = host.browsers.lock().remove(&(c as u32)) {
                                let code = map.get("code").and_then(|v| v.as_u64()).unwrap_or(4403);
                                let _ = tx.close.send(Some(code));
                            }
                        }
                    }
                }
            }
            ws::Message::Close(_) => break "close-frame",
            _ => {}
        }
    };
    info!(
        host = short(&id),
        reason = ended,
        seconds = connected_at.elapsed().as_secs(),
        "host connection ended"
    );

    writer.abort();
    let left = state.unregister(&id, host.generation);
    host.browsers.lock().clear();
    if left > 0 {
        info!(
            "host {} gone, an earlier registration takes over",
            short(&id)
        );
    } else {
        info!("host {} gone", short(&id));
    }
}

/// Shown while the Mac's Kaku is not connected. It looks like the terminal
/// page's own notice and opens the terminal by itself once the Mac is back;
/// the link's key is still in the address bar, so the reload keeps it.
const OFFLINE_PAGE: &str = r#"<!doctype html><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<title>Kaku</title>
<style>
html,body{height:100%;margin:0;background:#15141a;color:#d4d4d8}
body{display:flex;flex-direction:column;align-items:center;justify-content:center;gap:6px;padding:24px;box-sizing:border-box;text-align:center;font:14px -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;-webkit-font-smoothing:antialiased}
.title{font-size:15px}.detail{font-size:13px;color:#8b8b93}
</style>
<div class="title">Kaku on the Mac is offline</div>
<div class="detail">This page opens the terminal by itself when it is back.</div>
<script>
setInterval(function () {
  fetch(location.pathname, { method: "HEAD", cache: "no-store" })
    .then(function (r) { if (r.ok) location.reload(); })
    .catch(function () {});
}, 3000);
</script>"#;

fn asset_response(name: &str) -> Response {
    let Some((content_type, body)) = page_file(name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [
            (header::CONTENT_TYPE, content_type),
            // The page comes fresh so an update reaches it at once; xterm.js
            // and the fonts are ~1 MB that rarely change.
            (
                header::CACHE_CONTROL,
                if name == "index.html" {
                    "no-cache"
                } else {
                    "max-age=3600"
                },
            ),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        body,
    )
        .into_response()
}

async fn page_redirect(Path(id): Path<String>) -> Response {
    // Keep the trailing slash so the page's relative paths resolve under it;
    // the browser carries the fragment (the key) over the redirect.
    Redirect::permanent(&format!("/t/{id}/")).into_response()
}

async fn short_page_redirect(Path(id): Path<String>) -> Response {
    Redirect::permanent(&format!("/{id}/")).into_response()
}

async fn page(Path(id): Path<String>, State(state): State<AppState>) -> Response {
    if !valid_id(&id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    // The page lives on its own origin: links from older Kakus land there,
    // and the browser carries the fragment (the pairing secret) over.
    if let Some(origin) = &state.page_origin {
        return Redirect::permanent(&format!("{origin}/{id}/")).into_response();
    }
    // The page answers 404 while the Mac is away: that is how an open page
    // tells an offline Mac from a network problem.
    if state.host(&id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            OFFLINE_PAGE,
        )
            .into_response();
    }
    asset_response("index.html")
}

async fn page_asset(
    Path((id, name)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Response {
    if !valid_id(&id) || name == "index.html" || state.page_origin.is_some() {
        return StatusCode::NOT_FOUND.into_response();
    }
    asset_response(&name)
}

/// Whether this Mac is connected: 200 or 404, readable by the page's origin,
/// so a page served elsewhere can tell an offline Mac from no network.
async fn host_status(Path(id): Path<String>, State(state): State<AppState>) -> Response {
    let status = if valid_id(&id) && state.host(&id).is_some() {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };
    let mut response = status.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    if let Some(origin) = state.page_origin.as_deref().and_then(|o| o.parse().ok()) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    }
    response
}

/// Whether a browser socket comes from the page: with a page origin set, its
/// `Origin` must be exactly that, so another site cannot drive a browser's
/// paired keys through the relay. Browsers always send `Origin` on a
/// WebSocket handshake.
fn origin_allowed(page_origin: Option<&str>, headers: &HeaderMap) -> bool {
    let Some(expected) = page_origin else {
        return true;
    };
    headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|origin| origin == expected)
}

async fn browser_ws(
    Path(id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    upgrade: ws::WebSocketUpgrade,
) -> Response {
    if !origin_allowed(state.page_origin.as_deref(), &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let addr = client_addr(&headers);
    if !state.limits.lock().allow_connect(&addr, Instant::now()) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let Some(host) = state.host(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    upgrade
        .max_message_size(MAX_BROWSER_MESSAGE)
        .on_upgrade(move |socket| async move {
            info!(
                "browser joined host {} via edge {} from {}",
                short(&id),
                edge(&headers),
                device(&headers)
            );
            serve_browser(socket, host, addr).await
        })
}

fn browser_close(code: u64) -> ws::Message {
    let (code, reason) = match code {
        4401 => (4401, "pairing link expired or used"),
        4403 => (4403, "not a paired browser"),
        4408 => (4408, "authentication timed out"),
        4426 => (4426, "page is out of date"),
        _ => (4413, "connection is busy"),
    };
    ws::Message::Close(Some(ws::CloseFrame {
        code,
        reason: reason.into(),
    }))
}

async fn serve_browser(socket: ws::WebSocket, host: Arc<Host>, addr: String) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut outgoing) = mpsc::channel::<ws::Message>(QUEUE);
    let queued = Arc::new(Queued::new(BROWSER_QUEUE_BYTES, &host.queued.global));
    let (close, mut closing) = watch::channel(None);
    let c = host.next_id.fetch_add(1, Ordering::Relaxed);
    // Reserve atomically, after the upgrade: simultaneous handshakes would
    // all have seen space in a check made before it.
    let accepted = {
        let mut browsers = host.browsers.lock();
        let from_addr = browsers.values().filter(|b| b.addr == addr).count();
        if browsers.len() >= MAX_BROWSERS_PER_HOST || from_addr >= MAX_BROWSERS_PER_ADDR {
            false
        } else {
            browsers.insert(
                c,
                BrowserLink {
                    data: tx,
                    queued: queued.clone(),
                    close,
                    addr,
                },
            );
            true
        }
    };
    if !accepted {
        let _ = tokio::time::timeout(WRITE_TIMEOUT, sink.send(browser_close(4413))).await;
        return;
    }
    let open = serde_json::json!({ "t": "open", "c": c }).to_string();
    if !host.send(ws::Message::Text(open.into())) {
        host.browsers.lock().remove(&c);
        return;
    }
    let mut writer = tokio::spawn(async move {
        // Leaving the host's list (rejected key, host replaced) ends the
        // socket with its reason, even in the middle of a stalled write; it
        // never waits for an old screen queue to drain.
        let close_with_reason = |closing: &watch::Receiver<Option<u64>>| {
            closing
                .borrow()
                .map(browser_close)
                .unwrap_or(ws::Message::Close(None))
        };
        loop {
            let msg = tokio::select! {
                biased;
                _ = closing.changed() => {
                    let _ = tokio::time::timeout(WRITE_TIMEOUT, sink.send(close_with_reason(&closing))).await;
                    return;
                }
                msg = outgoing.recv() => match msg { Some(msg) => msg, None => return },
            };
            queued.release(message_len(&msg));
            tokio::select! {
                biased;
                _ = closing.changed() => {
                    let _ = tokio::time::timeout(WRITE_TIMEOUT, sink.send(close_with_reason(&closing))).await;
                    return;
                }
                result = tokio::time::timeout(WRITE_TIMEOUT, sink.send(msg)) => {
                    if !matches!(result, Ok(Ok(()))) { return; }
                }
            }
        }
    });
    loop {
        tokio::select! {
            _ = &mut writer => break,
            _ = tokio::time::sleep(BROWSER_SILENCE) => break,
            msg = stream.next() => {
                let Some(Ok(msg)) = msg else { break };
                match msg {
                    ws::Message::Binary(payload) => {
                        // The writer owns the close reason; do not abort it for late input.
                        if !host.browsers.lock().contains_key(&c) { continue; }
                        let mut frame = Vec::with_capacity(5 + payload.len());
                        frame.push(KIND_DATA);
                        frame.extend_from_slice(&c.to_be_bytes());
                        frame.extend_from_slice(&payload);
                        // Past the Mac's byte budget this browser is the one
                        // flooding it; let it go rather than the Mac.
                        if !host.send(ws::Message::Binary(frame.into())) { break; }
                    }
                    ws::Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }
    host.browsers.lock().remove(&c);
    writer.abort();
    let close = serde_json::json!({ "t": "close", "c": c }).to_string();
    let _ = host.send(ws::Message::Text(close.into()));
}

async fn health() -> &'static str {
    "ok"
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn app(state: AppState) -> Router {
    Router::new()
        .route("/host/{id}", get(host_ws))
        .route("/t/{id}", get(page_redirect))
        .route("/t/{id}/", get(page))
        .route("/t/{id}/ws", get(browser_ws))
        .route("/t/{id}/assets/{name}", get(page_asset))
        .route("/health", get(health))
        .route("/{id}", get(short_page_redirect))
        .route("/{id}/", get(page))
        .route("/{id}/ws", get(browser_ws))
        .route("/{id}/status", get(host_status))
        .route("/{id}/assets/{name}", get(page_asset))
        .with_state(state)
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(result) = token::run_command(&args) {
        match result {
            Ok(out) => println!("{out}"),
            Err(e) => {
                eprintln!("kaku-relay: {e}");
                std::process::exit(2);
            }
        }
        return;
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kaku_relay=info".into()),
        )
        .init();

    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8080);
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    info!("kaku-relay listening on {}", addr);

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("kaku-relay: failed to bind {}: {}", addr, e);
            std::process::exit(1);
        }
    };
    // Keystrokes and echoes are tiny frames; Nagle would hold them back.
    let listener = axum::serve::ListenerExt::tap_io(listener, |tcp| {
        if let Err(e) = tcp.set_nodelay(true) {
            tracing::warn!("set_nodelay failed: {e}");
        }
    });
    let tokens = TokenPolicy::from_env().map(Arc::new);
    if let Some(policy) = &tokens {
        info!(
            required = policy.required,
            "registration tokens are checked"
        );
    }
    let access = AccessCode::from_env().map(Arc::new);
    if access.is_some() {
        info!("Macs need the access code to register");
    }
    let page_origin = std::env::var("KAKU_RELAY_PAGE_ORIGIN")
        .ok()
        .map(|o| o.trim().trim_end_matches('/').to_string())
        .filter(|o| !o.is_empty())
        .map(Arc::from);
    if let Some(origin) = &page_origin {
        info!("the page is served from {origin}");
    }
    let state = AppState {
        tokens,
        access,
        page_origin,
        ..AppState::default()
    };
    if let Err(e) = axum::serve(listener, app(state)).await {
        eprintln!("kaku-relay: server error: {}", e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejection_code_reaches_the_browser_without_arbitrary_reason_text() {
        let ws::Message::Close(Some(frame)) = browser_close(4403) else {
            panic!("close frame")
        };
        assert_eq!(frame.code, 4403);
        assert_eq!(frame.reason.as_str(), "not a paired browser");
        // The page tells an expired pairing link from a removed browser by
        // its code; a code the relay does not pass reads as busy and retries.
        for code in [4401u16, 4408, 4426] {
            let ws::Message::Close(Some(frame)) = browser_close(code.into()) else {
                panic!("close frame")
            };
            assert_eq!(frame.code, code);
        }
        let ws::Message::Close(Some(frame)) = browser_close(u64::MAX) else {
            panic!("close frame")
        };
        assert_eq!(frame.code, 4413);
    }

    #[test]
    fn the_page_is_the_relay_own_and_shows_only_for_a_connected_mac() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let state = AppState::default();
        let id = "0123456789abcdef0123456789abcdef".to_string();
        let page_for = |state: AppState| rt.block_on(page(Path(id.clone()), State(state)));
        // Away: the offline notice, as a 404 the open page can recognize.
        assert_eq!(page_for(state.clone()).status(), StatusCode::NOT_FOUND);
        state.register(&id, test_host(&state));
        let shown = page_for(state.clone());
        assert_eq!(shown.status(), StatusCode::OK);
        assert_eq!(shown.headers()[header::CACHE_CONTROL], "no-cache");
        // Files come from the relay's own copy, with or without the Mac.
        let asset = |name: &str| {
            rt.block_on(page_asset(
                Path((id.clone(), name.to_string())),
                State(state.clone()),
            ))
        };
        assert_eq!(asset("xterm.js").status(), StatusCode::OK);
        assert_eq!(
            asset("font-regular.ttf").headers()[header::CONTENT_TYPE],
            "font/ttf"
        );
        assert_eq!(asset("../Cargo.toml").status(), StatusCode::NOT_FOUND);
        assert_eq!(asset("index.html").status(), StatusCode::NOT_FOUND);
        assert!(page_file("index.html")
            .unwrap()
            .1
            .starts_with(b"<!doctype html>"));
    }

    #[test]
    fn with_a_page_origin_links_move_there_and_only_its_browsers_join() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let state = AppState {
            page_origin: Some(Arc::from("https://t.kaku.fun")),
            ..AppState::default()
        };
        let id = "0123456789abcdef0123456789abcdef".to_string();
        let shown = rt.block_on(page(Path(id.clone()), State(state.clone())));
        assert_eq!(shown.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            shown.headers()[header::LOCATION],
            "https://t.kaku.fun/0123456789abcdef0123456789abcdef/"
        );
        let asset = rt.block_on(page_asset(
            Path((id.clone(), "xterm.js".into())),
            State(state.clone()),
        ));
        assert_eq!(asset.status(), StatusCode::NOT_FOUND);

        let from = |origin: Option<&str>| {
            let mut headers = HeaderMap::new();
            if let Some(origin) = origin {
                headers.insert(header::ORIGIN, origin.parse().unwrap());
            }
            origin_allowed(state.page_origin.as_deref(), &headers)
        };
        assert!(from(Some("https://t.kaku.fun")));
        assert!(!from(Some("https://evil.example")));
        assert!(!from(Some("https://r.kaku.fun")));
        assert!(!from(None));
        // Without a page origin (local runs) any browser joins, as before.
        assert!(origin_allowed(None, &HeaderMap::new()));

        // The page asks whether the Mac is here, across origins.
        let status =
            |state: &AppState| rt.block_on(host_status(Path(id.clone()), State(state.clone())));
        let away = status(&state);
        assert_eq!(away.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            away.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "https://t.kaku.fun"
        );
        state.register(&id, test_host(&state));
        assert_eq!(status(&state).status(), StatusCode::OK);
    }

    fn test_host(state: &AppState) -> Arc<Host> {
        let (to_host, rx) = mpsc::channel(QUEUE);
        // Keep the receiver alive so sends are judged by the budget alone.
        std::mem::forget(rx);
        Arc::new(Host {
            generation: state.generation.fetch_add(1, Ordering::Relaxed),
            to_host,
            queued: Queued::new(HOST_QUEUE_BYTES, &state.queued_bytes),
            evicted: tokio::sync::Notify::new(),
            browsers: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            addr: "-".into(),
        })
    }

    #[test]
    fn a_third_registration_pushes_out_the_oldest() {
        let state = AppState::default();
        let hosts: Vec<_> = (0..3).map(|_| test_host(&state)).collect();
        for h in &hosts {
            state.register("id", h.clone());
        }
        let kept: Vec<u64> = state.hosts.lock()["id"]
            .iter()
            .map(|h| h.generation)
            .collect();
        assert_eq!(kept, vec![hosts[1].generation, hosts[2].generation]);
        // The pushed-out one was told to end.
        let notified = hosts[0].evicted.notified();
        tokio::pin!(notified);
        assert!(futures_util::FutureExt::now_or_never(notified.as_mut()).is_some());
    }

    #[test]
    fn a_host_queue_is_bounded_by_bytes() {
        let state = AppState::default();
        let host = test_host(&state);
        let big = || ws::Message::Binary(vec![0u8; 3 << 20].into());
        assert!(host.send(big()));
        assert!(host.send(big()));
        // A third 3 MB message would pass 8 MB queued.
        assert!(!host.send(big()));
        assert_eq!(host.queued.load(), 6 << 20);
        assert!(host.send(ws::Message::Text("small".into())));
    }

    fn test_browser(global: &Arc<AtomicUsize>) -> BrowserLink {
        let (data, rx) = mpsc::channel(QUEUE);
        std::mem::forget(rx);
        BrowserLink {
            data,
            queued: Arc::new(Queued::new(BROWSER_QUEUE_BYTES, global)),
            close: watch::channel(None).0,
            addr: String::new(),
        }
    }

    #[test]
    fn a_browser_queue_is_bounded_by_bytes() {
        let state = AppState::default();
        let browser = test_browser(&state.queued_bytes);
        // Screens from the Mac can be 4 MB each; a page that stopped reading
        // must not hold more than its budget however few messages that is.
        let screen = || ws::Message::Binary(vec![0u8; 3 << 20].into());
        assert!(browser.send(screen()));
        assert!(browser.send(screen()));
        assert!(!browser.send(screen()));
        assert_eq!(browser.queued.load(), 6 << 20);
        assert_eq!(state.queued_bytes.load(Ordering::Relaxed), 6 << 20);
    }

    #[test]
    fn the_relay_wide_budget_bounds_every_queue_together() {
        let state = AppState::default();
        let browsers: Vec<_> = (0..20).map(|_| test_browser(&state.queued_bytes)).collect();
        let chunk = || ws::Message::Binary(vec![0u8; 6 << 20].into());
        // Each stays within its own 8 MB, but together they stop at the
        // relay-wide budget.
        let accepted = browsers.iter().filter(|b| b.send(chunk())).count();
        assert_eq!(accepted, GLOBAL_QUEUE_BYTES / (6 << 20));
        assert!(state.queued_bytes.load(Ordering::Relaxed) <= GLOBAL_QUEUE_BYTES);
    }

    #[test]
    fn a_closed_socket_gives_its_unsent_bytes_back() {
        let state = AppState::default();
        let browser = test_browser(&state.queued_bytes);
        let host = test_host(&state);
        assert!(browser.send(ws::Message::Binary(vec![0u8; 1 << 20].into())));
        assert!(host.send(ws::Message::Binary(vec![0u8; 1 << 20].into())));
        assert_eq!(state.queued_bytes.load(Ordering::Relaxed), 2 << 20);
        // Sockets that end with messages still queued must not leak budget,
        // or the relay slowly stops accepting anything.
        drop(browser);
        drop(host);
        assert_eq!(state.queued_bytes.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn an_earlier_host_takes_over_when_the_newer_one_leaves() {
        let state = AppState::default();
        let first = test_host(&state);
        let second = test_host(&state);
        assert!(state.register("id", first.clone()).is_none());
        let displaced = state
            .register("id", second.clone())
            .expect("first displaced");
        assert_eq!(displaced.generation, first.generation);
        assert_eq!(state.host("id").unwrap().generation, second.generation);

        // The newer Kaku quits: the first, still connected, serves again.
        assert_eq!(state.unregister("id", second.generation), 1);
        assert_eq!(state.host("id").unwrap().generation, first.generation);

        assert_eq!(state.unregister("id", first.generation), 0);
        assert!(state.host("id").is_none());
    }

    #[test]
    fn device_names_browser_and_os_only() {
        let ua = |s: &str| {
            let mut h = HeaderMap::new();
            h.insert(axum::http::header::USER_AGENT, s.parse().unwrap());
            device(&h)
        };
        assert_eq!(ua("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36"), "Chrome macOS");
        assert_eq!(ua("Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1"), "Safari iPhone");
        assert_eq!(ua("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36 Edg/140.0.0.0"), "Edge Windows");
        assert_eq!(device(&HeaderMap::new()), "other");
    }

    #[test]
    fn only_the_secret_owner_matches_an_id() {
        let secret = "a".repeat(64);
        let digest = Sha256::digest(secret.as_bytes());
        let id: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
        assert!(valid_id(&id));
        assert!(owns(&id, &secret));
        assert!(!owns(&id, "b"));
        assert!(!owns(&id, ""));
    }

    #[test]
    fn tokens_are_checked_and_only_enforced_when_required() {
        let (private, public) = token::generate().unwrap();
        let private = token::decode_key(&private).unwrap();
        let valid = token::sign(
            &token::Claims {
                sub: "*".into(),
                kind: "invite".into(),
                exp: 100,
            },
            &private,
        )
        .unwrap();
        let mut policy = TokenPolicy {
            public_key: token::decode_key(&public).unwrap(),
            required: false,
        };
        // Until required, a Mac with no token (today's builds) or a bad one
        // still registers; only the log notes the bad one.
        assert!(policy.admits("id", None, 50));
        assert!(policy.admits("id", Some("bad"), 50));
        policy.required = true;
        assert!(policy.admits("id", Some(&valid), 50));
        assert!(!policy.admits("id", None, 50));
        assert!(!policy.admits("id", Some(&valid), 150));
        assert!(!policy.admits("id", Some("bad"), 50));
    }

    #[test]
    fn the_access_code_matches_whole_and_trimmed() {
        let code = AccessCode::new("open sesame");
        assert!(code.admits("open sesame"));
        assert!(code.admits(" open sesame\n"));
        assert!(!code.admits("open sesam"));
        assert!(!code.admits("open sesame2"));
        assert!(!code.admits(""));
    }

    #[test]
    fn an_address_is_held_to_its_budgets_and_freed_after_the_window() {
        let mut limits = AddrLimits::default();
        let start = Instant::now();
        for _ in 0..CONNECTS_PER_MINUTE {
            assert!(limits.allow_connect("1.2.3.4", start));
        }
        assert!(!limits.allow_connect("1.2.3.4", start));
        // Another address and local runs are not affected.
        assert!(limits.allow_connect("5.6.7.8", start));
        for _ in 0..1000 {
            assert!(limits.allow_connect("-", start));
        }
        assert!(limits.allow_connect("1.2.3.4", start + Duration::from_secs(61)));

        for _ in 0..ACCESS_FAILURES - 1 {
            limits.note_access_failure("1.2.3.4", start);
        }
        assert!(!limits.access_locked("1.2.3.4", start));
        limits.note_access_failure("1.2.3.4", start);
        assert!(limits.access_locked("1.2.3.4", start));
        assert!(!limits.access_locked("5.6.7.8", start));
        assert!(!limits.access_locked("1.2.3.4", start + ACCESS_WINDOW));
    }

    #[test]
    fn ids_must_be_128_bit_hex() {
        assert!(!valid_id("short"));
        assert!(!valid_id(&"g".repeat(32)));
        assert!(valid_id(&"0f".repeat(16)));
    }
}
