//! Browser terminal served by the remote bridge.
//!
//! The page mirrors a pane by rendering Kaku's own terminal model to ANSI
//! frames, so the browser never parses the PTY stream itself: terminal queries
//! (DA, CPR, OSC color) keep being answered once, by Kaku. Keys go back through
//! `Pane::key_down` so cursor-key mode and keyboard protocols match what the
//! app asked Kaku for; text and pastes take the same paths Kaku's GUI uses.

use crate::{check_token, WsQuery};
use axum::extract::{ws, ConnectInfo, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use mux::pane::{Pane, PaneId};
use mux::Mux;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use termwiz::cell::{Blink, CellAttributes, Intensity, Underline};
use termwiz::color::ColorAttribute;
use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::{CursorShape, CursorVisibility};
use tokio::sync::broadcast;

const INDEX_HTML: &str = include_str!("../assets/web/index.html");
const XTERM_JS: &str = include_str!("../assets/web/xterm.js");
const XTERM_CSS: &str = include_str!("../assets/web/xterm.css");
// Kaku's bundled terminal font: Regular for text, Medium for bold, matching
// the dark-theme weights in the bundled kaku.lua.
const FONT_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf");
const FONT_MEDIUM: &[u8] = include_bytes!("../../../assets/fonts/JetBrainsMono-Medium.ttf");

/// Frames are coalesced to this interval; the trailing edge always flushes,
/// so the last output of a burst is never dropped.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// Re-render this often even without output, to pick up cursor moves,
/// title changes and resizes that do not produce `PaneOutput`.
const IDLE_REFRESH: Duration = Duration::from_secs(1);
const MAX_INPUT_LEN: usize = 65_536;

/// Per-pane "something changed" signal fed by the mux subscriber.
pub(crate) type WebDirty = Arc<Mutex<HashMap<usize, broadcast::Sender<()>>>>;

/// Tab titles exactly as the GUI tab bar last drew them, keyed by tab id.
/// The tab bar runs the user's `format-tab-title`, which only the GUI can
/// evaluate, so the page reuses its output instead of re-deriving a title.
static TAB_TITLES: LazyLock<Mutex<HashMap<usize, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Called by the GUI whenever it lays out the tab bar.
pub fn publish_tab_title(tab_id: usize, title: &str) {
    let title = title.trim();
    let mut titles = TAB_TITLES.lock();
    if titles.get(&tab_id).map(String::as_str) != Some(title) {
        titles.insert(tab_id, title.to_string());
    }
}

pub(crate) fn notify_output(dirty: &WebDirty, pane_id: usize) {
    if let Some(tx) = dirty.lock().get(&pane_id) {
        let _ = tx.send(());
    }
}

pub(crate) fn pane_removed(dirty: &WebDirty, pane_id: usize) {
    dirty.lock().remove(&pane_id);
}

// ── Static routes ─────────────────────────────────────────────────────────────

pub(crate) async fn route_index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

pub(crate) async fn route_xterm_js() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        XTERM_JS,
    )
        .into_response()
}

pub(crate) async fn route_xterm_css() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        XTERM_CSS,
    )
        .into_response()
}

fn font_response(bytes: &'static [u8]) -> Response {
    (
        [
            (header::CONTENT_TYPE, "font/ttf"),
            (header::CACHE_CONTROL, "max-age=86400"),
        ],
        bytes,
    )
        .into_response()
}

pub(crate) async fn route_font_regular() -> Response {
    font_response(FONT_REGULAR)
}

pub(crate) async fn route_font_medium() -> Response {
    font_response(FONT_MEDIUM)
}

/// Loopback-only page that hands the browser on this Mac a link carrying the
/// token in the URL fragment, which browsers never send to the server.
pub(crate) async fn route_link(ConnectInfo(addr): ConnectInfo<SocketAddr>) -> Response {
    if !addr.ip().is_loopback() {
        return (StatusCode::FORBIDDEN, "only accessible from this Mac").into_response();
    }
    let token = crate::get_or_init_token();
    let cfg = config::configuration();
    let port = cfg.remote.port;
    let lan = if cfg.remote.bind == "127.0.0.1" || cfg.remote.bind == "localhost" {
        None
    } else {
        crate::lan_ip()
    };
    let local = format!("http://127.0.0.1:{port}/#t={token}");
    let mut body = format!(
        r#"<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Kaku Web</title>
<style>body{{font:15px -apple-system,sans-serif;background:#16161a;color:#ddd;margin:40px auto;max-width:640px;padding:0 16px}}
a{{color:#9ecbff;word-break:break-all}}p{{color:#999}}</style>
<h2>Kaku Web</h2>
<p>This Mac</p><a href="{local}">{local}</a>"#
    );
    if let Some(ip) = lan {
        let url = format!("http://{ip}:{port}/#t={token}");
        let _ = write!(
            body,
            r#"<p>Other computers on this network</p><a href="{url}">{url}</a>"#
        );
    }
    body.push_str("<p>The link stops working when Kaku restarts.</p>");
    Html(body).into_response()
}

#[derive(Serialize)]
struct TabInfo {
    tab_id: usize,
    pane_id: usize,
    title: String,
    active: bool,
}

/// Tabs in tab bar order across windows, each pointing at its active pane.
pub(crate) async fn route_tabs(
    Query(query): Query<WsQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    if !check_token(&query, &headers) {
        return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
    }
    let Some(mux) = Mux::try_get() else {
        return axum::Json(Vec::<TabInfo>::new()).into_response();
    };
    let titles = TAB_TITLES.lock().clone();
    let mut tabs = Vec::new();
    for window_id in mux.iter_windows() {
        let Some(window) = mux.get_window(window_id) else {
            continue;
        };
        let active_idx = window.get_active_idx();
        for (idx, tab) in window.iter().enumerate() {
            let Some(pane) = tab.get_active_pane() else {
                continue;
            };
            let tab_id: usize = tab.tab_id().into();
            let title = titles
                .get(&tab_id)
                .filter(|t| !t.is_empty())
                .cloned()
                .unwrap_or_else(|| pane.get_title());
            tabs.push(TabInfo {
                tab_id,
                pane_id: pane.pane_id().into(),
                title,
                active: idx == active_idx,
            });
        }
    }
    axum::Json(tabs).into_response()
}

// ── Wire protocol ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WebClientMsg {
    /// Committed text: typed characters, IME output.
    Text {
        data: String,
    },
    /// A non-text key, encoded by Kaku for the pane's current modes.
    Key {
        key: String,
        #[serde(default)]
        ctrl: bool,
        #[serde(default)]
        alt: bool,
        #[serde(default)]
        shift: bool,
    },
    Paste {
        data: String,
    },
    /// Lines to move the view; positive scrolls back into history.
    Scroll {
        lines: isize,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WebServerMsg<'a> {
    Frame {
        cols: usize,
        rows: usize,
        title: &'a str,
        scrolled: bool,
        data: &'a str,
    },
    Closed,
}

// ── Frame rendering ───────────────────────────────────────────────────────────

/// Where the browser is looking: following the live screen, or pinned to a
/// stable row so new output does not move the history being read.
#[derive(Clone, Copy, Default)]
struct View {
    pinned_top: Option<isize>,
}

struct Frame {
    cols: usize,
    rows: usize,
    title: String,
    scrolled: bool,
    data: String,
}

fn render_frame(pane: &Arc<dyn Pane>, view: &mut View) -> Frame {
    let dims = pane.get_dimensions();
    let rows = dims.viewport_rows;
    let live_top = dims.physical_top;

    let top = match view.pinned_top {
        Some(top) if top < live_top => top.max(dims.scrollback_top),
        _ => {
            view.pinned_top = None;
            live_top
        }
    };

    let (_, lines) = pane.get_lines(top..top + rows as isize);

    let mut out = String::with_capacity(rows * (dims.cols + 16));
    out.push_str("\x1b[?25l");
    let mut current: Option<Sgr> = None;
    for row in 0..rows {
        let _ = write!(out, "\x1b[{};1H", row + 1);
        if let Some(line) = lines.get(row) {
            let mut col = 0;
            for cell in line.visible_cells() {
                if col >= dims.cols {
                    break;
                }
                let sgr = Sgr::from_attrs(cell.attrs());
                if current.as_ref() != Some(&sgr) {
                    sgr.write(&mut out);
                    current = Some(sgr);
                }
                let text = cell.str();
                if text.is_empty() {
                    out.push(' ');
                } else {
                    out.push_str(text);
                }
                col += cell.width().max(1);
            }
        }
        // Clear the rest of the row with the default background.
        out.push_str("\x1b[0m\x1b[K");
        current = None;
    }

    let cursor = pane.get_cursor_position();
    let cursor_row = cursor.y - top;
    if view.pinned_top.is_none() && cursor_row >= 0 && (cursor_row as usize) < rows {
        let _ = write!(
            out,
            "\x1b[{};{}H\x1b[{} q",
            cursor_row + 1,
            cursor.x + 1,
            cursor_shape_param(cursor.shape)
        );
        if cursor.visibility == CursorVisibility::Visible {
            out.push_str("\x1b[?25h");
        }
    }

    Frame {
        cols: dims.cols,
        rows,
        title: pane.get_title(),
        scrolled: view.pinned_top.is_some(),
        data: out,
    }
}

fn cursor_shape_param(shape: CursorShape) -> u8 {
    match shape {
        // The app did not pick a shape, so Kaku draws its configured default.
        CursorShape::Default => match config::configuration().default_cursor_style {
            config::DefaultCursorStyle::BlinkingBlock => 1,
            config::DefaultCursorStyle::SteadyBlock => 2,
            config::DefaultCursorStyle::BlinkingUnderline => 3,
            config::DefaultCursorStyle::SteadyUnderline => 4,
            config::DefaultCursorStyle::BlinkingBar => 5,
            config::DefaultCursorStyle::SteadyBar => 6,
        },
        CursorShape::BlinkingBlock => 1,
        CursorShape::SteadyBlock => 2,
        CursorShape::BlinkingUnderline => 3,
        CursorShape::SteadyUnderline => 4,
        CursorShape::BlinkingBar => 5,
        CursorShape::SteadyBar => 6,
    }
}

/// The subset of cell attributes that xterm.js can draw.
#[derive(PartialEq)]
struct Sgr {
    fg: ColorAttribute,
    bg: ColorAttribute,
    intensity: Intensity,
    underline: Underline,
    blink: Blink,
    italic: bool,
    reverse: bool,
    strikethrough: bool,
    invisible: bool,
}

impl Sgr {
    fn from_attrs(attrs: &CellAttributes) -> Self {
        Self {
            fg: attrs.foreground(),
            bg: attrs.background(),
            intensity: attrs.intensity(),
            underline: attrs.underline(),
            blink: attrs.blink(),
            italic: attrs.italic(),
            reverse: attrs.reverse(),
            strikethrough: attrs.strikethrough(),
            invisible: attrs.invisible(),
        }
    }

    fn write(&self, out: &mut String) {
        out.push_str("\x1b[0");
        match self.intensity {
            Intensity::Normal => {}
            Intensity::Bold => out.push_str(";1"),
            Intensity::Half => out.push_str(";2"),
        }
        if self.italic {
            out.push_str(";3");
        }
        match self.underline {
            Underline::None => {}
            Underline::Double => out.push_str(";21"),
            _ => out.push_str(";4"),
        }
        if self.blink != Blink::None {
            out.push_str(";5");
        }
        if self.reverse {
            out.push_str(";7");
        }
        if self.invisible {
            out.push_str(";8");
        }
        if self.strikethrough {
            out.push_str(";9");
        }
        write_color(out, self.fg, 38);
        write_color(out, self.bg, 48);
        out.push('m');
    }
}

fn write_color(out: &mut String, color: ColorAttribute, base: u8) {
    match color {
        ColorAttribute::Default => {}
        ColorAttribute::PaletteIndex(idx) => {
            let _ = write!(out, ";{base};5;{idx}");
        }
        ColorAttribute::TrueColorWithPaletteFallback(c, _)
        | ColorAttribute::TrueColorWithDefaultFallback(c) => {
            let (r, g, b, _) = c.to_srgb_u8();
            let _ = write!(out, ";{base};2;{r};{g};{b}");
        }
    }
}

// ── Input ─────────────────────────────────────────────────────────────────────

fn key_code(name: &str) -> Option<KeyCode> {
    Some(match name {
        "Enter" => KeyCode::Enter,
        "Tab" => KeyCode::Tab,
        "Escape" => KeyCode::Escape,
        "Backspace" => KeyCode::Backspace,
        "Delete" => KeyCode::Delete,
        "Insert" => KeyCode::Insert,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        "ArrowLeft" => KeyCode::LeftArrow,
        "ArrowRight" => KeyCode::RightArrow,
        "ArrowUp" => KeyCode::UpArrow,
        "ArrowDown" => KeyCode::DownArrow,
        _ => {
            if let Some(n) = name.strip_prefix('F').and_then(|n| n.parse::<u8>().ok()) {
                if (1..=24).contains(&n) {
                    return Some(KeyCode::Function(n));
                }
                return None;
            }
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => KeyCode::Char(c),
                _ => return None,
            }
        }
    })
}

fn handle_input(pane: &Arc<dyn Pane>, msg: WebClientMsg, view: &mut View) {
    let result = match msg {
        WebClientMsg::Text { data } => {
            view.pinned_top = None;
            if data.len() > MAX_INPUT_LEN {
                return;
            }
            pane.writer()
                .write_all(data.as_bytes())
                .map_err(anyhow::Error::from)
        }
        WebClientMsg::Paste { data } => {
            view.pinned_top = None;
            if data.len() > MAX_INPUT_LEN {
                return;
            }
            pane.send_paste(&data)
        }
        WebClientMsg::Key {
            key,
            ctrl,
            alt,
            shift,
        } => {
            view.pinned_top = None;
            let Some(code) = key_code(&key) else {
                return;
            };
            let mut mods = Modifiers::NONE;
            if ctrl {
                mods |= Modifiers::CTRL;
            }
            if alt {
                mods |= Modifiers::ALT;
            }
            if shift {
                mods |= Modifiers::SHIFT;
            }
            pane.key_down(code, mods)
        }
        WebClientMsg::Scroll { lines } => {
            let dims = pane.get_dimensions();
            let current = view.pinned_top.unwrap_or(dims.physical_top);
            let top = (current - lines).clamp(dims.scrollback_top, dims.physical_top);
            view.pinned_top = (top < dims.physical_top).then_some(top);
            Ok(())
        }
    };
    if let Err(e) = result {
        log::debug!("kaku-remote web: input failed: {e:#}");
    }
}

// ── WebSocket ─────────────────────────────────────────────────────────────────

pub(crate) async fn route_ws(
    Path(pane_id): Path<usize>,
    Query(query): Query<WsQuery>,
    State(dirty): State<WebDirty>,
    headers: axum::http::HeaderMap,
    upgrade: ws::WebSocketUpgrade,
) -> Response {
    if !check_token(&query, &headers) {
        return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
    }
    let rx = dirty
        .lock()
        .entry(pane_id)
        .or_insert_with(|| broadcast::channel(16).0)
        .subscribe();
    upgrade.on_upgrade(move |socket| handle_ws(socket, pane_id, rx))
}

fn lookup_pane(pane_id: usize) -> Option<Arc<dyn Pane>> {
    Mux::try_get()?.get_pane(PaneId::from(pane_id))
}

async fn handle_ws(socket: ws::WebSocket, pane_id: usize, mut rx: broadcast::Receiver<()>) {
    let (mut tx, mut ws_rx) = socket.split();
    let mut view = View::default();
    let mut last_sent = String::new();
    let mut dirty = true;
    let mut frame_tick = tokio::time::interval(FRAME_INTERVAL);
    frame_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut idle = tokio::time::interval(IDLE_REFRESH);

    loop {
        tokio::select! {
            msg = ws_rx.next() => {
                let Some(Ok(msg)) = msg else { break };
                let text = match msg {
                    ws::Message::Text(t) => t,
                    ws::Message::Close(_) => break,
                    _ => continue,
                };
                let Some(pane) = lookup_pane(pane_id) else { break };
                match serde_json::from_str::<WebClientMsg>(text.as_str()) {
                    Ok(msg) => {
                        handle_input(&pane, msg, &mut view);
                        dirty = true;
                    }
                    Err(e) => log::debug!("kaku-remote web: bad message: {e}"),
                }
            }
            recv = rx.recv() => match recv {
                Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => dirty = true,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = idle.tick() => dirty = true,
            _ = frame_tick.tick(), if dirty => {
                dirty = false;
                let Some(pane) = lookup_pane(pane_id) else { break };
                let frame = render_frame(&pane, &mut view);
                // Skip identical idle refreshes; the title and size live in
                // the message, so include them in the comparison.
                let key = format!("{}x{}|{}|{}|{}", frame.cols, frame.rows, frame.scrolled, frame.title, frame.data);
                if key == last_sent {
                    continue;
                }
                let msg = WebServerMsg::Frame {
                    cols: frame.cols,
                    rows: frame.rows,
                    title: &frame.title,
                    scrolled: frame.scrolled,
                    data: &frame.data,
                };
                let Ok(json) = serde_json::to_string(&msg) else { continue };
                if tx.send(ws::Message::Text(json.into())).await.is_err() {
                    return;
                }
                last_sent = key;
            }
        }
    }

    if let Ok(json) = serde_json::to_string(&WebServerMsg::Closed) {
        let _ = tx.send(ws::Message::Text(json.into())).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termwiz::color::SrgbaTuple;

    #[test]
    fn sgr_encodes_palette_truecolor_and_flags() {
        let mut attrs = CellAttributes::default();
        attrs.set_foreground(ColorAttribute::PaletteIndex(2));
        attrs.set_background(ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(
            1.0, 0.0, 0.5, 1.0,
        )));
        attrs.set_intensity(Intensity::Bold);
        attrs.set_reverse(true);
        let mut out = String::new();
        Sgr::from_attrs(&attrs).write(&mut out);
        assert_eq!(out, "\x1b[0;1;7;38;5;2;48;2;255;0;127m");
    }

    #[test]
    fn default_attrs_reset_only() {
        let mut out = String::new();
        Sgr::from_attrs(&CellAttributes::default()).write(&mut out);
        assert_eq!(out, "\x1b[0m");
    }

    #[test]
    fn published_tab_titles_are_trimmed() {
        publish_tab_title(usize::MAX, "  Users/tw93  ");
        assert_eq!(
            TAB_TITLES.lock().get(&usize::MAX).map(String::as_str),
            Some("Users/tw93")
        );
    }

    #[test]
    fn key_names_map_to_kaku_key_codes() {
        assert_eq!(key_code("Enter"), Some(KeyCode::Enter));
        assert_eq!(key_code("ArrowUp"), Some(KeyCode::UpArrow));
        assert_eq!(key_code("F5"), Some(KeyCode::Function(5)));
        assert_eq!(key_code("c"), Some(KeyCode::Char('c')));
        assert_eq!(key_code("F99"), None);
        assert_eq!(key_code("Unidentified"), None);
    }
}
