use anyhow::Context;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::serve::ListenerExt as _;
use axum::Router;
use config::{configuration, RgbaColor};
use mux::{Mux, MuxNotification};
use parking_lot::Mutex;
use qrcode::QrCode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

mod awake;
mod pairing;
mod share;
mod web;

pub use pairing::{list_devices, revoke_device, Device, OFFER_LIFETIME};
pub use share::{
    load_or_create_identity, reset_identity, save_access_code, save_token, ShareIdentity,
};
pub use web::{
    publish_tab_title, release_web_size, set_tab_action_handler, tab_is_web_sized,
    window_relayout_resizes_tab, TabAction, TabActionHandler,
};

// ── Token ─────────────────────────────────────────────────────────────────────

static TOKEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn get_or_init_token() -> &'static str {
    TOKEN.get_or_init(|| {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("getrandom failed");
        bytes.iter().map(|b| format!("{:02x}", b)).collect()
    })
}

// ── Page config ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct ConfigInfo {
    background: Option<String>,
    foreground: Option<String>,
    cursor_fg: Option<String>,
    /// Kaku's selection colors. Without them the page fell back to
    /// xterm.js's translucent white, under which white text vanished.
    selection_bg: Option<String>,
    selection_fg: Option<String>,
    cursor_bg: Option<String>,
    ansi: Vec<String>,
    brights: Vec<String>,
    font_family: String,
    font_size: f64,
    line_height: f64,
    copy_on_select: bool,
    /// Which Option key types composed characters (é, @ on some layouts)
    /// rather than acting as Alt, as Kaku does on the Mac.
    compose_left_option: bool,
    compose_right_option: bool,
    /// Where Kaku draws its tab bar, so the page puts its tabs there too.
    tab_bar_at_bottom: bool,
    /// Kaku's tab bar colors, so the page's tabs look like the window's.
    tab_bar: Option<TabBarInfo>,
}

#[derive(Debug, Clone, Serialize)]
struct TabBarInfo {
    background: Option<String>,
    active_bg: Option<String>,
    active_fg: Option<String>,
    inactive_fg: Option<String>,
    hover_bg: Option<String>,
    hover_fg: Option<String>,
}

#[derive(Deserialize)]
struct WsQuery {
    token: Option<String>,
}

fn rgba_to_hex(c: &RgbaColor) -> String {
    c.to_string()
}

fn opt_hex(c: Option<&RgbaColor>) -> Option<String> {
    c.map(rgba_to_hex)
}

fn config_info() -> ConfigInfo {
    let cfg = configuration();
    let palette = &cfg.resolved_palette;

    let ansi_colors = palette
        .ansi
        .map(|arr| arr.iter().map(|c| c.to_string()).collect::<Vec<_>>())
        .unwrap_or_default();

    let bright_colors = palette
        .brights
        .map(|arr| arr.iter().map(|c| c.to_string()).collect::<Vec<_>>())
        .unwrap_or_default();

    let font_family = cfg
        .font
        .font
        .first()
        .map(|f| f.family.clone())
        .unwrap_or_else(|| "JetBrains Mono".to_string());

    ConfigInfo {
        background: opt_hex(palette.background.as_ref()),
        foreground: opt_hex(palette.foreground.as_ref()),
        cursor_fg: opt_hex(palette.cursor_fg.as_ref()),
        selection_bg: opt_hex(palette.selection_bg.as_ref()),
        selection_fg: opt_hex(palette.selection_fg.as_ref()),
        cursor_bg: opt_hex(palette.cursor_bg.as_ref()),
        ansi: ansi_colors,
        brights: bright_colors,
        font_family,
        font_size: cfg.font_size,
        line_height: cfg.line_height,
        copy_on_select: cfg.copy_on_select,
        compose_left_option: cfg.send_composed_key_when_left_alt_is_pressed,
        compose_right_option: cfg.send_composed_key_when_right_alt_is_pressed,
        tab_bar_at_bottom: cfg.tab_bar_at_bottom,
        tab_bar: palette.tab_bar.as_ref().map(|bar| TabBarInfo {
            background: opt_hex(bar.background.as_ref()),
            active_bg: bar.active_tab.as_ref().map(|t| rgba_to_hex(&t.bg_color)),
            active_fg: bar.active_tab.as_ref().map(|t| rgba_to_hex(&t.fg_color)),
            inactive_fg: bar.inactive_tab.as_ref().map(|t| rgba_to_hex(&t.fg_color)),
            hover_bg: bar
                .inactive_tab_hover
                .as_ref()
                .map(|t| rgba_to_hex(&t.bg_color)),
            hover_fg: bar
                .inactive_tab_hover
                .as_ref()
                .map(|t| rgba_to_hex(&t.fg_color)),
        }),
    }
}

fn check_token(query: &WsQuery, headers: &axum::http::HeaderMap) -> bool {
    let expected = get_or_init_token();
    let header = headers.get("x-kaku-token").and_then(|v| v.to_str().ok());
    header.is_some_and(|t| same_secret(t, expected))
        || query
            .token
            .as_deref()
            .is_some_and(|t| same_secret(t, expected))
}

/// Compare a presented secret without leaking, through timing, how much of
/// it matched.
fn same_secret(presented: &str, expected: &str) -> bool {
    let (a, b) = (presented.as_bytes(), expected.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ── Host guard ────────────────────────────────────────────────────────────────

/// Whether `host` names this server by a loopback name. A web page on
/// another site can rebind its own name to 127.0.0.1 and then read
/// same-origin responses from this port; its requests still carry that name,
/// so they stay out. `port` is the listener's, which a config reload does not
/// move. The server listens on loopback only (2026-10-16: no LAN access).
fn host_allowed(host: &str, port: u16) -> bool {
    let Some((name, host_port)) = host.rsplit_once(':') else {
        return false;
    };
    host_port == port.to_string() && matches!(name, "127.0.0.1" | "localhost" | "[::1]")
}

async fn require_own_host(
    axum::extract::State(port): axum::extract::State<u16>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !host_allowed(host, port) {
        return (StatusCode::FORBIDDEN, "unexpected host").into_response();
    }
    next.run(req).await
}

/// Write a file only this user can read, in full or not at all: it is
/// created with mode 0600 (never briefly world-readable) under a temporary
/// name, then renamed over the target.
pub(crate) fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(data)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

// ── State file ────────────────────────────────────────────────────────────────

fn state_path() -> std::path::PathBuf {
    std::env::temp_dir().join("kaku-remote.json")
}

fn write_state(port: u16, token: &str, tunnel_relay: Option<&str>) {
    let mut val = serde_json::json!({
        "port": port,
        "token": token,
    });
    if let Some(relay) = tunnel_relay {
        val["tunnel_relay"] = serde_json::Value::String(relay.to_string());
    }
    if let Ok(json) = serde_json::to_string(&val) {
        if let Err(e) = write_private(&state_path(), json.as_bytes()) {
            log::warn!("kaku-remote: failed to write state file: {e}");
        }
    }
}

#[derive(serde::Deserialize)]
pub struct RemoteState {
    pub port: u16,
    pub token: String,
    #[serde(default)]
    pub tunnel_relay: Option<String>,
}

pub fn read_state() -> anyhow::Result<RemoteState> {
    let data = std::fs::read_to_string(state_path())
        .with_context(|| "Kaku remote bridge not running (state file not found)")?;
    serde_json::from_str(&data).context("invalid state file")
}

/// What the relay bridge last saw, for `kaku doctor`. Holds nothing secret:
/// no token, key or relay id.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
pub struct RelayStatus {
    pub relay: String,
    pub connected: bool,
    /// Unix seconds when `connected` last changed.
    pub since: u64,
    #[serde(default)]
    pub last_error: String,
    /// The Kaku that wrote it; the file outlives it when it quits.
    #[serde(default)]
    pub pid: u32,
    /// Unix seconds of the attempt this records, so `kaku remote` can tell
    /// that the bridge tried again after a new access code was saved.
    #[serde(default)]
    pub checked: u64,
}

impl RelayStatus {
    /// Whether the relay turned this Mac away for its access code.
    pub fn needs_access_code(&self) -> bool {
        !self.connected && self.last_error.contains(share::ACCESS_REFUSAL)
    }

    /// Whether the Kaku that wrote this is still running.
    pub fn is_current(&self) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        self.pid != 0 && unsafe { libc::kill(self.pid as libc::pid_t, 0) } == 0
    }
}

fn relay_status_path() -> std::path::PathBuf {
    std::env::temp_dir().join("kaku-remote-relay.json")
}

/// Record a change in the relay link; `since` moves only when `connected`
/// flips, so a link failing every 30 s still reads as down since it fell.
pub(crate) fn record_relay_status(relay: &str, connected: bool, error: Option<String>) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let previous = read_relay_status();
    let status = next_relay_status(
        previous.as_ref(),
        relay,
        connected,
        error,
        now,
        std::process::id(),
    );
    if previous.as_ref() == Some(&status) {
        return;
    }
    if let Ok(json) = serde_json::to_string(&status) {
        if let Err(e) = write_private(&relay_status_path(), json.as_bytes()) {
            log::debug!("kaku-remote: failed to write relay status: {e}");
        }
    }
}

fn next_relay_status(
    previous: Option<&RelayStatus>,
    relay: &str,
    connected: bool,
    error: Option<String>,
    now: u64,
    pid: u32,
) -> RelayStatus {
    let since = match previous {
        Some(p) if p.connected == connected && p.relay == relay && p.pid == pid => p.since,
        _ => now,
    };
    RelayStatus {
        relay: relay.to_string(),
        connected,
        since,
        last_error: error.unwrap_or_default(),
        pid,
        checked: now,
    }
}

pub fn read_relay_status() -> Option<RelayStatus> {
    let data = std::fs::read_to_string(relay_status_path()).ok()?;
    serde_json::from_str(&data).ok()
}

/// A QR code for `url`, drawn with block characters, followed by the url.
pub fn render_qr_terminal(url: &str) -> String {
    let qr = match QrCode::new(url.as_bytes()) {
        Ok(q) => q,
        Err(_) => return "Failed to generate QR code".to_string(),
    };
    let rendered = qr
        .render::<qrcode::render::unicode::Dense1x2>()
        .dark_color(qrcode::render::unicode::Dense1x2::Dark)
        .light_color(qrcode::render::unicode::Dense1x2::Light)
        .quiet_zone(false)
        .build();
    format!("{}\n{}", rendered.trim_end(), url)
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// The relay when `remote.tunnel_url` is empty. It lives here, not in the
/// config crate, so builds without the web terminal do not carry it.
const DEFAULT_RELAY: &str = "wss://r.kaku.fun";

pub fn start() {
    // install_default() returns Err if a provider is already installed; that is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cfg = configuration();
    if !cfg.remote.enabled {
        return;
    }

    let port = cfg.remote.port;
    let tunnel = cfg.remote.tunnel;
    let tunnel_url = match cfg.remote.tunnel_url.trim() {
        "" => DEFAULT_RELAY.to_string(),
        url => url.to_string(),
    };
    let token = get_or_init_token();

    log::info!("kaku-remote: starting on 127.0.0.1:{}", port);

    let web_dirty: web::WebDirty = Arc::new(Mutex::new(HashMap::new()));
    let web_dirty_for_sub = web_dirty.clone();

    if let Some(mux) = Mux::try_get() {
        mux.subscribe(move |notification| {
            match notification {
                MuxNotification::PaneOutput(pane_id) => {
                    web::notify_output(&web_dirty_for_sub, (*pane_id).into());
                }
                MuxNotification::AssignClipboard {
                    pane_id,
                    clipboard: Some(text),
                    ..
                } => {
                    web::publish_clipboard((*pane_id).into(), text.clone());
                }
                MuxNotification::Alert {
                    pane_id,
                    alert: wezterm_term::Alert::ToastNotification { title, body, .. },
                } => {
                    web::publish_notification((*pane_id).into(), title.as_deref(), body);
                }
                MuxNotification::PaneRemoved(pane_id) => {
                    web::pane_removed(&web_dirty_for_sub, (*pane_id).into());
                }
                _ => {}
            }
            true
        });
    }

    let tunnel_relay_opt: Option<String> = if tunnel { Some(tunnel_url) } else { None };
    std::thread::Builder::new()
        .name("kaku-remote".to_string())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("kaku-remote tokio runtime");

            rt.block_on(async move {
                if let Some(relay_url) = tunnel_relay_opt.clone() {
                    tokio::spawn(share::run(relay_url, web_dirty.clone()));
                }

                let app = Router::new()
                    .route("/", get(web::route_index))
                    .route("/assets/{name}", get(web::route_asset))
                    .route("/web/ws", get(web::route_ws))
                    .with_state(web_dirty)
                    .layer(axum::middleware::from_fn_with_state(port, require_own_host));

                // The relay link does not depend on the local listener, so a
                // busy port must not take it down with this task.
                let addr = SocketAddr::from(([127, 0, 0, 1], port));

                log::info!("kaku-remote: listening on http://{}", addr);
                write_state(port, token, tunnel_relay_opt.as_deref());

                // Keystrokes and frames are small writes; Nagle would hold one
                // back until the previous is acknowledged, as the relay
                // already avoids on its own sockets.
                let listener = match tokio::net::TcpListener::bind(addr).await {
                    Ok(l) => l.tap_io(|tcp| {
                        let _ = tcp.set_nodelay(true);
                    }),
                    Err(e) => {
                        log::error!("kaku-remote: failed to bind {addr}: {e}");
                        return std::future::pending().await;
                    }
                };
                if let Err(e) = axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                {
                    log::error!("kaku-remote: server error: {e}");
                }
            });
        })
        .expect("spawn kaku-remote thread");
}

#[cfg(test)]
mod relay_status_tests {
    use super::*;

    #[test]
    fn a_link_failing_over_and_over_reads_as_down_since_it_fell() {
        let up = next_relay_status(None, "wss://r", true, None, 100, 7);
        assert_eq!(up.since, 100);
        let down = next_relay_status(Some(&up), "wss://r", false, Some("reset".into()), 200, 7);
        assert_eq!((down.since, down.last_error.as_str()), (200, "reset"));
        let still_down = next_relay_status(
            Some(&down),
            "wss://r",
            false,
            Some("timed out".into()),
            230,
            7,
        );
        assert_eq!(
            (still_down.since, still_down.last_error.as_str()),
            (200, "timed out")
        );
        // A new Kaku, or another relay, starts its own clock.
        assert_eq!(
            next_relay_status(Some(&down), "wss://r", false, None, 300, 8).since,
            300
        );
        assert_eq!(
            next_relay_status(Some(&down), "wss://x", false, None, 300, 7).since,
            300
        );
    }

    #[test]
    fn an_access_code_refusal_is_told_apart() {
        let refused = |why: &str| {
            next_relay_status(
                None,
                "wss://r",
                false,
                Some(format!("relay refused this Mac (401 Unauthorized): {why}")),
                1,
                7,
            )
        };
        assert!(refused("access code required").needs_access_code());
        assert!(refused("wrong access code").needs_access_code());
        assert!(!refused("registration token refused").needs_access_code());
        assert!(!next_relay_status(None, "wss://r", true, None, 1, 7).needs_access_code());
        // Every attempt is stamped, even one that changes nothing else.
        let again = next_relay_status(
            Some(&refused("wrong access code")),
            "wss://r",
            false,
            Some("x".into()),
            9,
            7,
        );
        assert_eq!(again.checked, 9);
    }

    #[test]
    fn only_a_running_writer_makes_the_status_current() {
        let mine = next_relay_status(None, "wss://r", true, None, 0, std::process::id());
        assert!(mine.is_current());
        let gone = RelayStatus { pid: 0, ..mine };
        assert!(!gone.is_current());
    }
}

#[cfg(test)]
mod host_guard_tests {
    use super::*;

    #[test]
    fn only_loopback_names_are_accepted() {
        let ok = |host: &str| host_allowed(host, 9988);
        assert!(ok("127.0.0.1:9988"));
        assert!(ok("localhost:9988"));
        assert!(ok("[::1]:9988"));
        // A rebound name, another port, a LAN address, a missing header.
        assert!(!ok("evil.example:9988"));
        assert!(!ok("127.0.0.1:9989"));
        assert!(!ok("192.168.1.5:9988"));
        assert!(!ok(""));
    }

    #[test]
    fn secrets_compare_whole() {
        assert!(same_secret("abc123", "abc123"));
        assert!(!same_secret("abc124", "abc123"));
        assert!(!same_secret("abc12", "abc123"));
        assert!(!same_secret("", "abc123"));
    }
}
