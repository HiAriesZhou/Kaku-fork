use anyhow::Context;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use config::{configuration, RgbaColor};
use mux::{Mux, MuxNotification};
use parking_lot::Mutex;
use qrcode::QrCode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

mod awake;
mod share;
mod web;

pub use share::{load_or_create_identity, reset_identity, ShareIdentity};
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
        || query.token.as_deref().is_some_and(|t| same_secret(t, expected))
}

/// Compare a presented secret without leaking, through timing, how much of
/// it matched.
fn same_secret(presented: &str, expected: &str) -> bool {
    let (a, b) = (presented.as_bytes(), expected.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ── Host guard ────────────────────────────────────────────────────────────────

/// Whether `host` names this server: a loopback name, or, when it listens
/// beyond loopback, one of this Mac's own addresses written as an IP. A web
/// page on another site can rebind its own name to 127.0.0.1 and then read
/// same-origin responses from this port; its requests still carry that name,
/// never an IP literal, so they stay out. `port` is the listener's, which a
/// config reload does not move.
fn host_allowed(host: &str, port: u16, lan: bool, is_own_ip: impl Fn(IpAddr) -> bool) -> bool {
    let Some((name, host_port)) = host.rsplit_once(':') else {
        return false;
    };
    if host_port != port.to_string() {
        return false;
    }
    if matches!(name, "127.0.0.1" | "localhost" | "[::1]") {
        return true;
    }
    let literal = name.trim_start_matches('[').trim_end_matches(']');
    lan && literal.parse::<IpAddr>().is_ok_and(is_own_ip)
}

/// Binding succeeds only for an address on one of this Mac's interfaces right
/// now, so a Wi-Fi change is followed without caching anything.
fn is_own_ip(ip: IpAddr) -> bool {
    !ip.is_unspecified() && std::net::UdpSocket::bind((ip, 0)).is_ok()
}

async fn require_own_host(
    axum::extract::State((port, lan)): axum::extract::State<(u16, bool)>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !host_allowed(host, port, lan, is_own_ip) {
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
    let bind = cfg.remote.bind.clone();
    let tunnel = cfg.remote.tunnel;
    let tunnel_url = match cfg.remote.tunnel_url.trim() {
        "" => DEFAULT_RELAY.to_string(),
        url => url.to_string(),
    };
    let token = get_or_init_token();

    log::info!("kaku-remote: starting on {}:{}", bind, port);

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
                    .layer(axum::middleware::from_fn_with_state(
                        (port, bind != "127.0.0.1" && bind != "localhost"),
                        require_own_host,
                    ));

                // The relay link does not depend on the local listener, so a
                // busy port must not take it down with this task.
                let addr: SocketAddr = match format!("{}:{}", bind, port).parse() {
                    Ok(a) => a,
                    Err(e) => {
                        log::error!("kaku-remote: invalid bind address: {e}");
                        return std::future::pending().await;
                    }
                };

                log::info!("kaku-remote: listening on http://{}", addr);
                write_state(port, token, tunnel_relay_opt.as_deref());

                let listener = match tokio::net::TcpListener::bind(addr).await {
                    Ok(l) => l,
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
mod host_guard_tests {
    use super::*;

    #[test]
    fn only_this_servers_own_names_are_accepted() {
        let own = |ip: IpAddr| ip.to_string() == "192.168.1.5" || ip.to_string() == "fe80::1";
        let ok = |host: &str, lan: bool| host_allowed(host, 9988, lan, own);
        assert!(ok("127.0.0.1:9988", false));
        assert!(ok("localhost:9988", false));
        assert!(ok("[::1]:9988", false));
        assert!(ok("192.168.1.5:9988", true));
        assert!(ok("[fe80::1]:9988", true));
        // A rebound name, another port, the LAN address while bound to
        // loopback only, an address that is not this Mac's, a missing header.
        assert!(!ok("evil.example:9988", true));
        assert!(!ok("127.0.0.1:9989", false));
        assert!(!ok("192.168.1.5:9988", false));
        assert!(!ok("192.168.1.9:9988", true));
        assert!(!ok("", true));
    }

    /// After a Wi-Fi change the new address must be accepted without a
    /// restart: ownership is checked per request, against the live
    /// interfaces, and loopback always binds.
    #[test]
    fn own_addresses_are_checked_live() {
        assert!(is_own_ip("127.0.0.1".parse().unwrap()));
        assert!(!is_own_ip("0.0.0.0".parse().unwrap()));
        assert!(!is_own_ip("203.0.113.7".parse().unwrap()));
    }

    #[test]
    fn secrets_compare_whole() {
        assert!(same_secret("abc123", "abc123"));
        assert!(!same_secret("abc124", "abc123"));
        assert!(!same_secret("abc12", "abc123"));
        assert!(!same_secret("", "abc123"));
    }
}
