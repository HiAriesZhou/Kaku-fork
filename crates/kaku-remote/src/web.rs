//! Browser terminal served by the remote bridge.
//!
//! The page mirrors a pane by rendering Kaku's own terminal model to ANSI
//! frames, so the browser never parses the PTY stream itself: terminal queries
//! (DA, CPR, OSC color) keep being answered once, by Kaku. Keys go back through
//! `Pane::key_down` so cursor-key mode and keyboard protocols match what the
//! app asked Kaku for; text and pastes take the same paths Kaku's GUI uses.

use crate::{check_token, WsQuery};
use axum::extract::{ws, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use mux::pane::{CloseReason, Pane, PaneId};
use mux::Mux;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;
use termwiz::cell::{Blink, CellAttributes, Intensity, Underline};
use termwiz::color::ColorAttribute;
use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::{CursorShape, CursorVisibility, Line};
use tokio::sync::{broadcast, mpsc};
use wezterm_term::{MouseButton, MouseEvent, MouseEventKind, TerminalSize};

const INDEX_HTML: &str = include_str!("../assets/web/index.html");
const XTERM_JS: &str = include_str!("../assets/web/xterm.js");
const XTERM_CSS: &str = include_str!("../assets/web/xterm.css");
// Kaku's bundled terminal font: Regular for text, Medium for bold, matching
// the dark-theme weights in the bundled kaku.lua.
const FONT_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf");
const FONT_MEDIUM: &[u8] = include_bytes!("../../../assets/fonts/JetBrainsMono-Medium.ttf");
const ICON_PNG: &[u8] = include_bytes!("../assets/web/icon.png");

/// Frames are coalesced to this interval; the trailing edge always flushes,
/// so the last output of a burst is never dropped.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// Re-render this often even without output, to pick up cursor moves,
/// title changes and resizes that do not produce `PaneOutput`.
const IDLE_REFRESH: Duration = Duration::from_secs(1);
const MAX_INPUT_LEN: usize = 65_536;
/// A page pings every 20 s, and a background tab still gets a timer about
/// once a minute. This long without a word means the browser is gone (a
/// laptop asleep with the socket half open), and the session ends rather
/// than holding its subscriptions and App Nap off forever.
const SESSION_SILENCE: Duration = Duration::from_secs(180);
/// Cap on wheel notches forwarded to an app from one browser wheel event.
const MAX_WHEEL_STEPS: usize = 10;
const MIN_COLS: usize = 20;
const MAX_COLS: usize = 500;
const MIN_ROWS: usize = 5;
const MAX_ROWS: usize = 300;

/// Per-pane "something changed" signal fed by the mux subscriber.
pub(crate) type WebDirty = Arc<Mutex<HashMap<usize, broadcast::Sender<()>>>>;

/// Tab titles exactly as the GUI tab bar last drew them, keyed by tab id.
/// The tab bar runs the user's `format-tab-title`, which only the GUI can
/// evaluate, so the page reuses its output instead of re-deriving a title.
static TAB_TITLES: LazyLock<Mutex<HashMap<usize, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Tabs whose size a browser set. That size can be larger than the Mac
/// window, which then draws the rows that fit rather than over its tab bar.
static WEB_SIZED_TABS: LazyLock<Mutex<HashSet<usize>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Whether a browser set this tab's size, so it may not fit the Mac window.
pub fn tab_is_web_sized(tab_id: usize) -> bool {
    WEB_SIZED_TABS.lock().contains(&tab_id)
}

/// The Mac took the tab back to its window's size.
pub fn release_web_size(tab_id: usize) {
    WEB_SIZED_TABS.lock().remove(&tab_id);
}

/// Whether a relayout of the Mac window should resize this tab. A tab a
/// browser sized keeps that size through relayouts the Mac does on its own
/// (a display or scale change, focus, a theme switch, the tab bar showing):
/// shrinking it under the browser made Claude Code and Codex redraw for the
/// Mac's rows, and their whole screen jumped up in the page until the next
/// key. Someone dragging the window edge is using the Mac, which takes the
/// tab back, as its own typing and clicks do.
pub fn window_relayout_resizes_tab(tab_id: usize, live_resize: bool) -> bool {
    if live_resize {
        release_web_size(tab_id);
        return true;
    }
    !tab_is_web_sized(tab_id)
}

/// Tab changes the page asks for. The GUI performs them through the same
/// key assignments as its own Cmd+T and Close Tab, so cwd inheritance and
/// the closed-tab history behave as they do on the Mac.
#[derive(Debug, Clone)]
pub enum TabAction {
    /// Open a tab in the window holding this pane.
    New { pane_id: usize },
    /// Close the tab holding this pane. The page has already asked per
    /// tab_close_confirmation, so the Mac must not prompt again: its prompt
    /// would only be visible on the Mac.
    Close { pane_id: usize },
    /// The browser is looking at this pane: clear its unread bell, which the
    /// Mac only clears when its own window takes focus, and so never while
    /// the Mac sits unattended.
    Seen { pane_id: usize },
    /// Split the named pane through the native spawn path.
    Split {
        pane_id: usize,
        direction: config::keyassignment::PaneDirection,
    },
    /// Close the named pane after the browser completed confirmation.
    ClosePane { pane_id: usize },
    /// Make the tab holding this pane the active tab in its Mac window, and
    /// this pane the focused split in that tab.
    Activate { pane_id: usize },
    /// Rename the tab holding this pane; empty goes back to the computed title.
    Rename { pane_id: usize, title: String },
    /// Move the tab holding this pane to `index` in its window.
    Move { pane_id: usize, index: usize },
}

pub type TabActionHandler = Arc<dyn Fn(TabAction) + Send + Sync + 'static>;

static TAB_ACTION_HANDLER: OnceLock<TabActionHandler> = OnceLock::new();

/// Register the GUI's tab action handler. First registration wins.
pub fn set_tab_action_handler(handler: TabActionHandler) {
    let _ = TAB_ACTION_HANDLER.set(handler);
}

fn dispatch_tab_action(action: TabAction) {
    match TAB_ACTION_HANDLER.get() {
        Some(handler) => handler(action),
        None => log::debug!("kaku-remote web: no tab action handler for {action:?}"),
    }
}

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

/// Text a program put on the clipboard (OSC 52: Claude Code's copy, a vim
/// yank), by pane. The Mac's clipboard gets it as before; a browser viewing
/// that pane gets it too, since that is where the user is.
static CLIPBOARD: LazyLock<broadcast::Sender<(usize, Arc<str>)>> =
    LazyLock::new(|| broadcast::channel(8).0);

/// Larger copies stay on the Mac only.
const MAX_CLIPBOARD_BYTES: usize = 1 << 20;

pub(crate) fn publish_clipboard(pane_id: usize, text: Arc<str>) {
    if text.len() <= MAX_CLIPBOARD_BYTES {
        let _ = CLIPBOARD.send((pane_id, text));
    }
}

/// Notifications a program posted (OSC 9/777: Claude Code finishing, a long
/// build done), by pane. Kaku shows them on the Mac as before; every browser
/// gets them too, so a page in a background tab can raise one there.
static NOTIFICATIONS: LazyLock<broadcast::Sender<Notification>> =
    LazyLock::new(|| broadcast::channel(8).0);

#[derive(Clone)]
struct Notification {
    pane_id: usize,
    title: Arc<str>,
    body: Arc<str>,
}

/// Long enough for any real notice; a program cannot flood browsers with more.
fn clip(text: &str, max: usize) -> Arc<str> {
    match text.char_indices().nth(max) {
        Some((end, _)) => Arc::from(&text[..end]),
        None => Arc::from(text),
    }
}

pub(crate) fn publish_notification(pane_id: usize, title: Option<&str>, body: &str) {
    let _ = NOTIFICATIONS.send(Notification {
        pane_id,
        title: clip(title.unwrap_or(""), 120),
        body: clip(body, 500),
    });
}

// ── Static assets ─────────────────────────────────────────────────────────────

/// The page and its files, by path relative to the page. Served locally and,
/// through the relay, from this Kaku, so the page always matches the build.
pub(crate) fn asset(path: &str) -> Option<(&'static str, &'static [u8])> {
    Some(match path {
        "" | "index.html" => ("text/html; charset=utf-8", INDEX_HTML.as_bytes()),
        "assets/xterm.js" => ("text/javascript; charset=utf-8", XTERM_JS.as_bytes()),
        "assets/xterm.css" => ("text/css; charset=utf-8", XTERM_CSS.as_bytes()),
        "assets/font-regular.ttf" => ("font/ttf", FONT_REGULAR),
        "assets/font-medium.ttf" => ("font/ttf", FONT_MEDIUM),
        "assets/icon.png" => ("image/png", ICON_PNG),
        _ => return None,
    })
}

fn asset_response(path: &str) -> Response {
    match asset(path) {
        Some((content_type, body)) => {
            ([(header::CONTENT_TYPE, content_type)], body).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub(crate) async fn route_index() -> Response {
    asset_response("index.html")
}

pub(crate) async fn route_asset(Path(name): Path<String>) -> Response {
    asset_response(&format!("assets/{name}"))
}

#[derive(Serialize, PartialEq)]
struct TabInfo {
    tab_id: usize,
    /// Tabs reorder within their own Mac window.
    window_id: usize,
    pane_id: usize,
    title: String,
    active: bool,
    /// Whether closing should ask first, per tab_close_confirmation.
    confirm_close: bool,
    /// The splits of a tab with more than one pane, in cells, so the page
    /// can draw the layout and switch between them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    panes: Vec<PaneBox>,
}

#[derive(Serialize, PartialEq)]
struct PaneBox {
    pane_id: usize,
    left: usize,
    top: usize,
    width: usize,
    height: usize,
    confirm_close: bool,
}

/// Tabs in tab bar order across windows, each pointing at its active pane.
fn collect_tabs() -> Vec<TabInfo> {
    let Some(mux) = Mux::try_get() else {
        return Vec::new();
    };
    let titles = TAB_TITLES.lock().clone();
    let config = config::configuration();
    let policy = config.tab_close_confirmation;
    let pane_policy = config.pane_close_confirmation;
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
            let positioned = tab.iter_panes_ignoring_zoom();
            let panes = if positioned.len() > 1 {
                positioned
                    .iter()
                    .map(|p| PaneBox {
                        pane_id: p.pane.pane_id().into(),
                        left: p.left,
                        top: p.top,
                        width: p.width,
                        height: p.height,
                        confirm_close: pane_policy.should_prompt(true, || {
                            p.pane.can_close_without_prompting(CloseReason::Pane)
                        }),
                    })
                    .collect()
            } else {
                Vec::new()
            };
            tabs.push(TabInfo {
                tab_id,
                window_id,
                pane_id: pane.pane_id().into(),
                title,
                active: idx == active_idx,
                confirm_close: policy
                    .should_prompt(true, || tab.can_close_without_prompting(CloseReason::Tab)),
                panes,
            });
        }
    }
    // Forget tabs that closed, so these maps stay the size of what is open.
    let live: HashSet<usize> = tabs.iter().map(|t| t.tab_id).collect();
    TAB_TITLES.lock().retain(|id, _| live.contains(id));
    WEB_SIZED_TABS.lock().retain(|id| live.contains(id));
    tabs
}

// ── Wire protocol ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WebClientMsg {
    /// First message of a session. It says whether the page acknowledges
    /// frames (flow control); through the relay, whatever first opens with
    /// the key starts the session.
    Hello {
        #[serde(default)]
        frame_ack: bool,
    },
    /// Heartbeat; answered with `pong` so the page can tell a dead link
    /// from a quiet terminal, and so idle-timeout proxies see traffic.
    Ping,
    /// Browser has parsed every frame through this sequence.
    FrameAck {
        seq: u64,
    },
    /// Start mirroring a pane; the rest of the messages act on it.
    Attach {
        pane_id: usize,
        /// The user picked this tab in the page (a click, Cmd+digit): make it
        /// the Mac's active tab too. Reconnects and follows leave it alone.
        #[serde(default)]
        focus: bool,
    },
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
    /// A left click on a cell of the visible screen. Only a program that
    /// asked for the mouse (Claude Code's "Jump to bottom", Codex) gets it;
    /// otherwise a click is the browser's, for selecting text.
    Click {
        x: usize,
        y: usize,
    },
    /// The grid the browser can show at Kaku's font size. While the page is
    /// driving the pane, its tab takes this size; local input on the Mac
    /// takes it back.
    Resize {
        cols: usize,
        rows: usize,
    },
    /// Open a tab next to the one being viewed.
    NewTab,
    /// Find `query` in the pane's history, case-insensitively, going to
    /// older lines from the last match (or the bottom) when `older`, else to
    /// newer ones. The view moves to the match and a `Found` reply follows.
    Search {
        query: String,
        older: bool,
        /// A new query: start again from the bottom.
        #[serde(default)]
        restart: bool,
    },
    /// A file sent from the browser (dropped or pasted), in order: start,
    /// base64 chunks, end. It lands in ~/Downloads/Kaku and its path is
    /// pasted into the pane, as a file dropped on the Mac window would be.
    UploadStart {
        id: u32,
        name: String,
        size: u64,
        #[serde(default)]
        transfer: String,
        #[serde(default)]
        pane_id: Option<usize>,
    },
    UploadCancel {
        id: u32,
    },
    UploadChunk {
        id: u32,
        data: String,
    },
    UploadEnd {
        id: u32,
    },
    SplitPane {
        pane_id: usize,
        direction: config::keyassignment::PaneDirection,
    },
    /// Close a pane; the page has already confirmed with the user.
    ClosePane {
        pane_id: usize,
    },
    /// Close a tab; the page has already confirmed with the user.
    CloseTab {
        tab_id: usize,
    },
    /// Rename a tab, from the page's tab editor; empty clears the name.
    RenameTab {
        tab_id: usize,
        title: String,
    },
    /// Move a tab to `index` in its window, from a drag in the page.
    MoveTab {
        tab_id: usize,
        index: usize,
    },
    /// Send every row again when the browser needs a fresh screen.
    Redraw,
    /// Rows of the pane's history ending before stable row `end` (the
    /// bottom of the screen when absent), for the page to scroll locally.
    History {
        #[serde(default)]
        end: Option<isize>,
        /// Echoed in the reply, so the page can drop one that arrives after
        /// it moved on.
        #[serde(default)]
        request: u32,
    },
    /// The text of every row the page shows, once it has applied `frames`
    /// frames. Kaku compares it with what it believes the page shows; a
    /// mismatch is logged with enough context to find its cause, then healed
    /// with a full frame.
    Verify {
        frames: u64,
        rows: Vec<String>,
    },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WebServerMsg<'a> {
    Config(&'a crate::ConfigInfo),
    Error {
        detail: &'a str,
    },
    Tabs {
        tabs: &'a [TabInfo],
    },
    Frame {
        seq: u64,
        /// The pane this frame draws; the page drops frames still in flight
        /// from the tab it just left.
        pane_id: usize,
        cols: usize,
        rows: usize,
        title: &'a str,
        scrolled: bool,
        /// A plain shell: scrolling moves through Kaku's history, which the
        /// page scrolls locally. Otherwise the program gets the wheel.
        local_scroll: bool,
        data: &'a str,
    },
    /// The attached pane is gone; the page picks another tab. It names the
    /// pane because the page may already have moved on after a tabs update,
    /// and must not drop the tab it moved to.
    Closed {
        pane_id: usize,
    },
    Pong,
    /// Where a search landed, relative to the rows now shown, or no match.
    Found {
        found: bool,
        row: usize,
        col: usize,
        len: usize,
    },
    /// A chunk of a sent file reached the Mac: bytes written so far.
    UploadAck {
        id: u32,
        received: u64,
    },
    /// How a sent file ended: where it was saved, or why it was not.
    Uploaded {
        id: u32,
        ok: bool,
        detail: &'a str,
    },
    /// Text a program in the attached pane copied, for the browser clipboard.
    Clipboard {
        text: &'a str,
    },
    /// A notification a program in any pane posted.
    Notify {
        pane_id: usize,
        title: &'a str,
        body: &'a str,
    },
    /// History rows for the page's local scroll view: `rows` start at stable
    /// row `top`, and `first` is the oldest row the pane still holds. Each
    /// row is SGR runs with `CSI n G` pinning columns after wide glyphs.
    History {
        pane_id: usize,
        request: u32,
        first: isize,
        top: isize,
        rows: &'a [String],
    },
}

/// Rows sent per history request. The page asks for more as it nears the top.
const HISTORY_PAGE: isize = 400;

// ── Frame rendering ───────────────────────────────────────────────────────────

/// Where the browser is looking: following the live screen, or pinned to a
/// stable row so new output does not move the history being read.
#[derive(Clone, Copy, Default)]
struct View {
    pinned_top: Option<isize>,
    /// Grid size this browser asked for, re-applied on its next input in
    /// case the Mac took the tab back meanwhile.
    claimed: Option<(usize, usize)>,
    /// When the pane was last reported as seen, to report at most once a
    /// second while typing.
    seen_at: Option<std::time::Instant>,
    /// The last search match, as (stable row, column), so the next search
    /// continues from it.
    found_at: Option<(isize, usize)>,
}

/// Largest file a browser may send.
const MAX_UPLOAD_BYTES: u64 = 50 << 20;

/// A file name from the browser, made safe to create: its last component,
/// no separators or control characters, a sane length, never empty or dots.
fn safe_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && *c != ':')
        .take(120)
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_string();
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned
    }
}

/// `dir/name`, or `dir/stem (2).ext` and so on when that already exists.
fn unused_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

struct Upload {
    file: std::fs::File,
    path: std::path::PathBuf,
    size: u64,
    written: u64,
    pane_id: Option<usize>,
    transfer: String,
    name: String,
}

/// Completion receipts survive a transport reconnect, so losing the final
/// reply never repeats a file or pastes its path twice. Bound their lifetime
/// and number independently of the terminal's scrollback.
struct UploadReceipt {
    at: std::time::Instant,
    name: String,
    size: u64,
    pane_id: Option<usize>,
    path: std::path::PathBuf,
}
static UPLOAD_RECEIPTS: LazyLock<Mutex<HashMap<String, UploadReceipt>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const RECEIPT_TTL: Duration = Duration::from_secs(600);
const MAX_RECEIPTS: usize = 256;
fn prune_receipts(receipts: &mut HashMap<String, UploadReceipt>) {
    receipts.retain(|_, r| r.at.elapsed() < RECEIPT_TTL);
    if receipts.len() >= MAX_RECEIPTS {
        if let Some(oldest) = receipts
            .iter()
            .min_by_key(|(_, r)| r.at)
            .map(|(k, _)| k.clone())
        {
            receipts.remove(&oldest);
        }
    }
}

/// A session's uploads in progress. A session that ends mid-upload (the
/// link dropped, the tab closed) leaves no partial file behind; the page
/// sends the file again once it reconnects.
#[derive(Default)]
struct Uploads(HashMap<u32, Upload>);

impl Drop for Uploads {
    fn drop(&mut self) {
        for (_, upload) in self.0.drain() {
            drop(upload.file);
            let _ = std::fs::remove_file(&upload.path);
        }
    }
}

/// The outcome of an upload step that the page should hear about.
struct UploadReply {
    id: u32,
    ok: bool,
    detail: String,
    /// A chunk was written: bytes received so far. The page advances its
    /// bounded send window on this, so progress reflects bytes on the Mac.
    received: Option<u64>,
}

impl UploadReply {
    fn as_msg(&self) -> WebServerMsg<'_> {
        match self.received {
            Some(received) => WebServerMsg::UploadAck {
                id: self.id,
                received,
            },
            None => WebServerMsg::Uploaded {
                id: self.id,
                ok: self.ok,
                detail: &self.detail,
            },
        }
    }
}

/// Apply one upload message; a reply only when the upload finished or failed.
fn handle_upload(
    msg: &WebClientMsg,
    uploads: &mut HashMap<u32, Upload>,
    attached: Option<usize>,
) -> Option<UploadReply> {
    let fail = |id: u32, why: String| {
        Some(UploadReply {
            id,
            ok: false,
            detail: why,
            received: None,
        })
    };
    match msg {
        WebClientMsg::UploadCancel { id } => {
            if let Some(upload) = uploads.remove(id) {
                drop(upload.file);
                let _ = std::fs::remove_file(upload.path);
            }
            None
        }
        WebClientMsg::UploadStart {
            id,
            name,
            size,
            transfer,
            pane_id,
        } => {
            let target = (*pane_id).or(attached);
            if !transfer.is_empty()
                && (transfer.len() != 32 || !transfer.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                return fail(*id, "invalid transfer".into());
            }
            if !transfer.is_empty() {
                let mut receipts = UPLOAD_RECEIPTS.lock();
                receipts.retain(|_, r| r.at.elapsed() < RECEIPT_TTL);
                if let Some(done) = receipts.get(transfer) {
                    if done.name != *name || done.size != *size || done.pane_id != target {
                        return fail(*id, "transfer does not match the original file".into());
                    }
                    return Some(UploadReply {
                        id: *id,
                        ok: true,
                        detail: done.path.display().to_string(),
                        received: None,
                    });
                }
            }
            if uploads.len() >= 8 || uploads.contains_key(id) {
                return fail(*id, "too many files in progress".into());
            }
            if *size > MAX_UPLOAD_BYTES {
                return fail(*id, "larger than 50 MB".into());
            }
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
            let dir = home.join("Downloads").join("Kaku");
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return fail(*id, format!("cannot create {}: {e}", dir.display()));
            }
            let path = unused_path(&dir, &safe_file_name(name));
            // Never truncate an existing file if two sessions choose a name
            // together; the losing upload can retry with a fresh name.
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    uploads.insert(
                        *id,
                        Upload {
                            file,
                            path,
                            size: *size,
                            written: 0,
                            pane_id: target,
                            transfer: transfer.clone(),
                            name: name.clone(),
                        },
                    );
                    None
                }
                Err(e) => fail(*id, format!("cannot write {}: {e}", path.display())),
            }
        }
        WebClientMsg::UploadChunk { id, data } => {
            use base64::Engine as _;
            use std::io::Write as _;
            let upload = uploads.get_mut(id)?;
            let bytes = match base64::engine::general_purpose::STANDARD.decode(data) {
                Ok(bytes) => bytes,
                Err(_) => {
                    let gone = uploads.remove(id)?;
                    let _ = std::fs::remove_file(&gone.path);
                    return fail(*id, "bad data".into());
                }
            };
            upload.written += bytes.len() as u64;
            if upload.written > upload.size.max(1) || upload.written > MAX_UPLOAD_BYTES {
                let gone = uploads.remove(id)?;
                let _ = std::fs::remove_file(&gone.path);
                return fail(*id, "more data than announced".into());
            }
            if let Err(e) = upload.file.write_all(&bytes) {
                let gone = uploads.remove(id)?;
                let _ = std::fs::remove_file(&gone.path);
                return fail(*id, format!("write failed: {e}"));
            }
            Some(UploadReply {
                id: *id,
                ok: true,
                detail: String::new(),
                received: Some(upload.written),
            })
        }
        WebClientMsg::UploadEnd { id } => {
            let upload = uploads.remove(id)?;
            drop(upload.file);
            if upload.written != upload.size {
                let _ = std::fs::remove_file(&upload.path);
                return fail(*id, "file was not fully received".into());
            }
            let shown = upload.path.display().to_string();
            if !upload.transfer.is_empty() {
                let mut receipts = UPLOAD_RECEIPTS.lock();
                prune_receipts(&mut receipts);
                receipts.insert(
                    upload.transfer,
                    UploadReceipt {
                        at: std::time::Instant::now(),
                        name: upload.name,
                        size: upload.size,
                        pane_id: upload.pane_id,
                        path: upload.path,
                    },
                );
            }
            if let Some(pane) = upload.pane_id.and_then(lookup_pane) {
                let quoted = config::configuration().quote_dropped_files.escape(&shown);
                if let Err(e) = pane.send_paste(&format!("{quoted} ")) {
                    log::debug!("kaku-remote web: pasting an uploaded path failed: {e:#}");
                }
            }
            Some(UploadReply {
                id: *id,
                ok: true,
                detail: shown,
                received: None,
            })
        }
        _ => None,
    }
}

/// One row's text with the cell column of every character, so a match in
/// the text maps back to columns even with wide characters in the row.
struct RowText {
    row: isize,
    text: String,
    cols: Vec<usize>,
    /// The column just past each character, so a wide last character
    /// counts both of its cells.
    ends: Vec<usize>,
}

fn row_text_of(row: isize, line: &wezterm_term::Line) -> RowText {
    let mut text = String::new();
    let mut cols = Vec::new();
    let mut ends = Vec::new();
    for cell in line.visible_cells() {
        for ch in cell.str().chars() {
            for lower in ch.to_lowercase() {
                text.push(lower);
                cols.push(cell.cell_index());
                ends.push(cell.cell_index() + cell.width().max(1));
            }
        }
    }
    RowText {
        row,
        text,
        cols,
        ends,
    }
}

/// The match nearest to `from` in the given direction within `rows` (which
/// are in ascending row order): (row, column, width in cells).
fn find_in_rows(
    rows: &[RowText],
    needle: &str,
    from: Option<(isize, usize)>,
    older: bool,
) -> Option<(isize, usize, usize)> {
    let hit = |r: &RowText, at: usize| {
        let start = r.cols[at];
        let end = r.ends[at + needle.chars().count() - 1];
        (r.row, start, end - start)
    };
    let matches = |r: &RowText| -> Vec<usize> {
        let chars: Vec<char> = r.text.chars().collect();
        let n: Vec<char> = needle.chars().collect();
        (0..chars.len().saturating_sub(n.len().saturating_sub(1)))
            .filter(|&i| chars[i..].starts_with(&n))
            .collect()
    };
    if older {
        for r in rows.iter().rev() {
            if from.is_some_and(|(fr, _)| r.row > fr) {
                continue;
            }
            let found = matches(r);
            let pick = found.into_iter().rev().find(|&i| match from {
                Some((fr, fc)) if fr == r.row => r.cols[i] < fc,
                _ => true,
            });
            if let Some(i) = pick {
                return Some(hit(r, i));
            }
        }
    } else {
        for r in rows {
            if from.is_some_and(|(fr, _)| r.row < fr) {
                continue;
            }
            let found = matches(r);
            let pick = found.into_iter().find(|&i| match from {
                Some((fr, fc)) if fr == r.row => r.cols[i] > fc,
                _ => true,
            });
            if let Some(i) = pick {
                return Some(hit(r, i));
            }
        }
    }
    None
}

/// Search the pane's whole history in chunks from `from` toward `older` or
/// newer lines, stopping at the first match.
fn search_pane(
    pane: &Arc<dyn Pane>,
    query: &str,
    from: Option<(isize, usize)>,
    older: bool,
) -> Option<(isize, usize, usize)> {
    const CHUNK: isize = 500;
    let needle = query.to_lowercase();
    if needle.is_empty() {
        return None;
    }
    let dims = pane.get_dimensions();
    let first = dims.scrollback_top;
    let end = dims.physical_top + dims.viewport_rows as isize;
    let start = from
        .map(|(row, _)| row)
        .unwrap_or(if older { end - 1 } else { first });
    let mut cursor = start;
    loop {
        let range = if older {
            (cursor - CHUNK + 1).max(first)..(cursor + 1).min(end)
        } else {
            cursor.max(first)..(cursor + CHUNK).min(end)
        };
        if range.start >= range.end {
            return None;
        }
        let (top, lines) = pane.get_lines(range.clone());
        let rows: Vec<RowText> = lines
            .iter()
            .enumerate()
            .map(|(i, line)| row_text_of(top + i as isize, line))
            .collect();
        if let Some(hit) = find_in_rows(&rows, &needle, from, older) {
            return Some(hit);
        }
        if older {
            if range.start <= first {
                return None;
            }
            cursor = range.start - 1;
        } else {
            if range.end >= end {
                return None;
            }
            cursor = range.end;
        }
    }
}

fn mark_seen(pane_id: usize, view: &mut View) {
    let now = std::time::Instant::now();
    if view
        .seen_at
        .is_some_and(|at| now.duration_since(at) < Duration::from_secs(1))
    {
        return;
    }
    view.seen_at = Some(now);
    dispatch_tab_action(TabAction::Seen { pane_id });
}

/// Size the tab holding `pane_id` to the browser's grid. Runs on the GUI
/// thread, where the window's own resizes happen.
fn claim_tab_size(pane_id: usize, cols: usize, rows: usize) {
    promise::spawn::spawn_into_main_thread(async move {
        let Some(mux) = Mux::try_get() else {
            return;
        };
        let Some((_, _, tab_id)) = mux.resolve_pane_id(PaneId::from(pane_id)) else {
            return;
        };
        let Some(tab) = mux.get_tab(tab_id) else {
            return;
        };
        let current = tab.get_size();
        // In a split, the pane is a share of the tab: grow the tab so that
        // share comes out at the browser's grid. Splits keep their
        // proportions on resize, so scaling by the share lands within a cell.
        let (cols, rows) = match tab
            .iter_panes_ignoring_zoom()
            .into_iter()
            .find(|p| p.pane.pane_id() == PaneId::from(pane_id))
        {
            Some(p) if p.width < current.cols || p.height < current.rows => (
                split_share(cols, current.cols, p.width),
                split_share(rows, current.rows, p.height),
            ),
            _ => (cols, rows),
        };
        let (cols, rows) = (cols.min(MAX_COLS), rows.min(MAX_ROWS));
        WEB_SIZED_TABS.lock().insert(tab_id.into());
        if current.cols == cols && current.rows == rows {
            return;
        }
        let cell_width = current.pixel_width / current.cols.max(1);
        let cell_height = current.pixel_height / current.rows.max(1);
        tab.resize(TerminalSize {
            rows,
            cols,
            pixel_width: cell_width * cols,
            pixel_height: cell_height * rows,
            dpi: current.dpi,
        });
    })
    .detach();
}

/// The tab extent that gives a pane holding `part` of `whole` cells a
/// size of `want`.
fn split_share(want: usize, whole: usize, part: usize) -> usize {
    if part == 0 || part >= whole {
        return want;
    }
    (want * whole + part / 2) / part
}

struct Frame {
    cols: usize,
    rows: usize,
    title: String,
    scrolled: bool,
    local_scroll: bool,
    /// One self-contained string per row: it positions itself and starts
    /// from reset attributes, so any subset of rows can be sent alone.
    lines: Vec<String>,
    /// Cursor position, shape and visibility, applied after the rows.
    cursor: String,
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

    let mut rendered = Vec::with_capacity(rows);
    for row in 0..rows {
        let mut out = String::with_capacity(dims.cols + 16);
        let _ = write!(out, "\x1b[{};1H", row + 1);
        if let Some(line) = lines.get(row) {
            render_cells(line, dims.cols, &mut out);
        }
        // Clear the rest of the row with the default background.
        out.push_str("\x1b[0m\x1b[K");
        rendered.push(out);
    }

    let mut cursor_out = String::new();
    let cursor = pane.get_cursor_position();
    let cursor_row = cursor.y - top;
    if view.pinned_top.is_none() && cursor_row >= 0 && (cursor_row as usize) < rows {
        let _ = write!(
            cursor_out,
            "\x1b[{};{}H\x1b[{} q",
            cursor_row + 1,
            cursor.x + 1,
            cursor_shape_param(cursor.shape)
        );
        if cursor.visibility == CursorVisibility::Visible {
            cursor_out.push_str("\x1b[?25h");
        }
    }

    Frame {
        cols: dims.cols,
        rows,
        title: pane.get_title(),
        scrolled: view.pinned_top.is_some(),
        local_scroll: !pane.is_alt_screen_active() && !pane.is_mouse_grabbed(),
        lines: rendered,
        cursor: cursor_out,
    }
}

/// One row's cells as SGR runs, at most `cols` wide.
fn render_cells(line: &Line, cols: usize, out: &mut String) {
    let mut current: Option<Sgr> = None;
    let mut col = 0;
    for cell in line.visible_cells() {
        if col >= cols {
            break;
        }
        let sgr = Sgr::from_attrs(cell.attrs());
        if current.as_ref() != Some(&sgr) {
            sgr.write(out);
            current = Some(sgr);
        }
        let text = cell.str();
        let width = cell.width().max(1);
        if text.is_empty() {
            out.push(' ');
        } else {
            out.push_str(text);
        }
        col += width;
        // xterm.js has its own width table (Unicode 6 by default), so for
        // symbols and emoji it can disagree with Kaku about how many columns
        // a glyph takes and shift the rest of the row. Pin the next column
        // to Kaku's answer.
        if !text.is_ascii() && col < cols {
            let _ = write!(out, "\x1b[{}G", col + 1);
        }
    }
}

/// Up to `HISTORY_PAGE` rows of the pane ending before `end` (the bottom of
/// the screen when absent), with the first row's stable index and the
/// oldest row the pane holds.
fn history_rows(pane: &Arc<dyn Pane>, end: Option<isize>) -> (isize, isize, Vec<String>) {
    let dims = pane.get_dimensions();
    let bottom = dims.physical_top + dims.viewport_rows as isize;
    let end = end.unwrap_or(bottom).clamp(dims.scrollback_top, bottom);
    let top = (end - HISTORY_PAGE).max(dims.scrollback_top);
    let (_, lines) = pane.get_lines(top..end);
    let rows = lines
        .iter()
        .map(|line| {
            let mut out = String::new();
            render_cells(line, dims.cols, &mut out);
            out
        })
        .collect();
    (dims.scrollback_top, top, rows)
}

/// What the browser already shows, so the next frame only carries changes.
#[derive(Default)]
struct Shown {
    cols: usize,
    title: String,
    scrolled: bool,
    local_scroll: bool,
    lines: Vec<String>,
    cursor: String,
    /// For drift reports: the last row move sent, and frames since a full one.
    last_move: isize,
    since_full: usize,
}

impl Shown {
    /// The bytes that bring the browser from what it shows to `frame`, or
    /// None when nothing changed.
    fn update(&mut self, frame: Frame) -> Option<(Frame, String)> {
        let full = self.cols != frame.cols
            || self.lines.len() != frame.lines.len()
            || self.scrolled != frame.scrolled;
        let mut data = String::from("\x1b[?25l");
        let mut changed = false;
        // Scrolling, and output that pushes rows up, move most of the screen
        // without changing it. Let the browser move what it already has and
        // send only the rows that are new, instead of every row again.
        let moved = if full {
            0
        } else {
            row_shift(&self.lines, &frame.lines)
        };
        if full {
            self.since_full = 0;
        } else {
            self.since_full += 1;
        }
        if moved != 0 {
            self.last_move = moved;
            let _ = write!(
                data,
                "\x1b[0m\x1b[{}{}",
                moved.unsigned_abs(),
                if moved > 0 { 'S' } else { 'T' }
            );
            changed = true;
        }
        for (idx, line) in frame.lines.iter().enumerate() {
            let before = if full {
                None
            } else {
                idx.checked_add_signed(moved)
                    .and_then(|i| self.lines.get(i))
            };
            if before.map(|old| row_body(old)) != Some(row_body(line)) {
                data.push_str(line);
                changed = true;
            }
        }
        if !changed
            && self.cursor == frame.cursor
            && self.title == frame.title
            && self.local_scroll == frame.local_scroll
        {
            return None;
        }
        data.push_str(&frame.cursor);
        self.cols = frame.cols;
        self.title.clone_from(&frame.title);
        self.scrolled = frame.scrolled;
        self.local_scroll = frame.local_scroll;
        self.cursor.clone_from(&frame.cursor);
        self.lines.clone_from(&frame.lines);
        Some((frame, data))
    }
}

/// A row without its leading cursor position, so a row that moved compares
/// equal to where it was.
fn row_body(line: &str) -> &str {
    line.strip_prefix("\x1b[")
        .and_then(|rest| rest.find('H').map(|i| &rest[i + 1..]))
        .unwrap_or(line)
}

/// How many rows the screen content moved: positive when it moved up (new
/// output, or scrolling toward the bottom), negative when it moved down.
/// Zero unless the move reuses more rows than leaving everything in place.
fn row_shift(old: &[String], new: &[String]) -> isize {
    let rows = new.len();
    if old.len() != rows || rows < 4 {
        return 0;
    }
    let reused = |shift: isize| {
        (0..rows)
            .filter(|&i| {
                i.checked_add_signed(shift)
                    .and_then(|j| old.get(j))
                    .is_some_and(|o| row_body(o) == row_body(&new[i]))
            })
            .count()
    };
    let in_place = reused(0);
    let mut best = (0, in_place);
    for k in 1..=(rows / 2) as isize {
        for shift in [k, -k] {
            let n = reused(shift);
            if n > best.1 {
                best = (shift, n);
            }
        }
    }
    // A move costs one short sequence; worth it once it saves a few rows.
    if best.1 >= in_place + 2 {
        best.0
    } else {
        0
    }
}

/// The visible text of a row, escape sequences and whitespace removed, so
/// Kaku's row and xterm.js's copy compare equal however each spaced it.
fn row_text(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        if !c.is_whitespace() {
            out.push(c);
        }
    }
    out
}

/// Rows where the page's text differs from what Kaku believes it shows.
fn drifted_rows(shown: &[String], page: &[String]) -> Vec<usize> {
    let rows = shown.len().max(page.len());
    (0..rows)
        .filter(|&i| {
            let kaku = shown.get(i).map(|l| row_text(l)).unwrap_or_default();
            let seen: String = page
                .get(i)
                .map(|l| l.chars().filter(|c| !c.is_whitespace()).collect())
                .unwrap_or_default();
            kaku != seen
        })
        .collect()
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

/// A pane in tab `tab_id`, which is how tab actions name their tab.
fn tab_target(tab_id: usize) -> Option<usize> {
    Mux::try_get()
        .and_then(|mux| mux.get_tab(mux::tab::TabId::from(tab_id)))
        .and_then(|tab| tab.get_active_pane())
        .map(|pane| pane.pane_id().into())
}

fn handle_input(pane: &Arc<dyn Pane>, msg: WebClientMsg, view: &mut View) {
    let interactive = matches!(
        msg,
        WebClientMsg::Text { .. }
            | WebClientMsg::Key { .. }
            | WebClientMsg::Paste { .. }
            | WebClientMsg::Click { .. }
    );
    if interactive {
        if let Some((cols, rows)) = view.claimed {
            claim_tab_size(pane.pane_id().into(), cols, rows);
        }
        mark_seen(pane.pane_id().into(), view);
    }
    let result = match msg {
        WebClientMsg::Text { data } => {
            view.pinned_top = None;
            view.found_at = None;
            pane.writer()
                .write_all(data.as_bytes())
                .map_err(anyhow::Error::from)
        }
        WebClientMsg::Paste { data } => {
            view.pinned_top = None;
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
        // Same routing as Kaku's own wheel: an app that asked for the mouse
        // (Claude Code, vim with mouse on) gets wheel events and scrolls
        // itself; a full-screen app without the mouse gets arrow keys; a
        // plain shell scrolls Kaku's history.
        WebClientMsg::Scroll { lines } if pane.is_mouse_grabbed() => {
            let dims = pane.get_dimensions();
            let button = if lines > 0 {
                MouseButton::WheelUp(1)
            } else {
                MouseButton::WheelDown(1)
            };
            let mut result = Ok(());
            for _ in 0..lines.unsigned_abs().min(MAX_WHEEL_STEPS) {
                result = pane.mouse_event(MouseEvent {
                    kind: MouseEventKind::Press,
                    x: dims.cols / 2,
                    y: (dims.viewport_rows / 2) as i64,
                    x_pixel_offset: 0,
                    y_pixel_offset: 0,
                    button,
                    modifiers: Modifiers::NONE,
                });
                if result.is_err() {
                    break;
                }
            }
            result
        }
        WebClientMsg::Click { x, y } => {
            if !pane.is_mouse_grabbed() {
                return;
            }
            let dims = pane.get_dimensions();
            let mut result = Ok(());
            for kind in [MouseEventKind::Press, MouseEventKind::Release] {
                result = pane.mouse_event(MouseEvent {
                    kind,
                    x: x.min(dims.cols.saturating_sub(1)),
                    y: y.min(dims.viewport_rows.saturating_sub(1)) as i64,
                    x_pixel_offset: 0,
                    y_pixel_offset: 0,
                    button: MouseButton::Left,
                    modifiers: Modifiers::NONE,
                });
                if result.is_err() {
                    break;
                }
            }
            result
        }
        WebClientMsg::Scroll { lines } if pane.is_alt_screen_active() => {
            let key = if lines > 0 {
                KeyCode::UpArrow
            } else {
                KeyCode::DownArrow
            };
            let mut result = Ok(());
            for _ in 0..lines.unsigned_abs().min(MAX_WHEEL_STEPS) {
                result = pane.key_down(key, Modifiers::NONE);
                if result.is_err() {
                    break;
                }
            }
            result
        }
        WebClientMsg::Scroll { lines } => {
            let dims = pane.get_dimensions();
            let current = view.pinned_top.unwrap_or(dims.physical_top);
            let top = (current - lines).clamp(dims.scrollback_top, dims.physical_top);
            view.pinned_top = (top < dims.physical_top).then_some(top);
            Ok(())
        }
        WebClientMsg::NewTab => {
            dispatch_tab_action(TabAction::New {
                pane_id: pane.pane_id().into(),
            });
            Ok(())
        }
        WebClientMsg::SplitPane { pane_id, direction } => {
            use config::keyassignment::PaneDirection;
            if lookup_pane(pane_id).is_some()
                && matches!(
                    direction,
                    PaneDirection::Left
                        | PaneDirection::Right
                        | PaneDirection::Up
                        | PaneDirection::Down
                )
            {
                dispatch_tab_action(TabAction::Split { pane_id, direction });
            }
            Ok(())
        }
        WebClientMsg::ClosePane { pane_id } => {
            if lookup_pane(pane_id).is_some() {
                dispatch_tab_action(TabAction::ClosePane { pane_id });
            }
            Ok(())
        }
        WebClientMsg::CloseTab { tab_id } => {
            if let Some(target) = tab_target(tab_id) {
                dispatch_tab_action(TabAction::Close { pane_id: target });
            }
            Ok(())
        }
        WebClientMsg::RenameTab { tab_id, title } => {
            if let Some(target) = tab_target(tab_id) {
                let title: String = title.chars().take(200).collect();
                dispatch_tab_action(TabAction::Rename {
                    pane_id: target,
                    title,
                });
            }
            Ok(())
        }
        WebClientMsg::MoveTab { tab_id, index } => {
            if let Some(target) = tab_target(tab_id) {
                dispatch_tab_action(TabAction::Move {
                    pane_id: target,
                    index,
                });
            }
            Ok(())
        }
        WebClientMsg::Hello { .. }
        | WebClientMsg::Redraw
        | WebClientMsg::History { .. }
        | WebClientMsg::Ping
        | WebClientMsg::FrameAck { .. }
        | WebClientMsg::Attach { .. }
        | WebClientMsg::Verify { .. }
        | WebClientMsg::Search { .. }
        | WebClientMsg::UploadStart { .. }
        | WebClientMsg::UploadChunk { .. }
        | WebClientMsg::UploadCancel { .. }
        | WebClientMsg::UploadEnd { .. } => Ok(()),
        WebClientMsg::Resize { cols, rows } => {
            let cols = cols.clamp(MIN_COLS, MAX_COLS);
            let rows = rows.clamp(MIN_ROWS, MAX_ROWS);
            view.claimed = Some((cols, rows));
            claim_tab_size(pane.pane_id().into(), cols, rows);
            Ok(())
        }
    };
    if let Err(e) = result {
        log::debug!("kaku-remote web: input failed: {e:#}");
    }
}

// ── Session ───────────────────────────────────────────────────────────────────

fn lookup_pane(pane_id: usize) -> Option<Arc<dyn Pane>> {
    Mux::try_get()?.get_pane(PaneId::from(pane_id))
}

fn subscribe(dirty: &WebDirty, pane_id: usize) -> broadcast::Receiver<()> {
    dirty
        .lock()
        .entry(pane_id)
        .or_insert_with(|| broadcast::channel(16).0)
        .subscribe()
}

fn encode(msg: &WebServerMsg) -> Option<String> {
    serde_json::to_string(msg).ok()
}

/// One browser, whatever carries its messages: the local WebSocket or the
/// relay tunnel. `inbox` yields the page's JSON messages and `outbox` takes
/// ours; the session ends when either side goes away.
pub(crate) async fn run_session(
    mut inbox: mpsc::Receiver<String>,
    outbox: mpsc::Sender<String>,
    dirty: WebDirty,
) {
    let _awake = crate::awake::Awake::hold();
    if let Some(json) = encode(&WebServerMsg::Config(&crate::config_info())) {
        if outbox.send(json).await.is_err() {
            return;
        }
    }

    let mut attached: Option<usize> = None;
    let mut updates: Option<broadcast::Receiver<()>> = None;
    let mut view = View::default();
    let mut shown = Shown::default();
    let mut last_tabs: Option<Vec<TabInfo>> = None;
    let mut dirty_frame = false;
    // Frames sent this session; a page's verify only counts once it has
    // applied the same number, so frames in flight never read as drift.
    let mut frames_sent: u64 = 0;
    let mut frames_acked = 0u64;
    let mut flow_control = false;
    let mut last_frame_at = std::time::Instant::now();
    let mut clipboard = CLIPBOARD.subscribe();
    let mut notifications = NOTIFICATIONS.subscribe();
    let mut uploads = Uploads::default();
    let mut frame_tick = tokio::time::interval(FRAME_INTERVAL);
    frame_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut idle = tokio::time::interval(IDLE_REFRESH);
    let mut last_heard = std::time::Instant::now();

    loop {
        let pane_update = async {
            match updates.as_mut() {
                Some(rx) => rx.recv().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            msg = inbox.recv() => {
                let Some(text) = msg else { break };
                last_heard = std::time::Instant::now();
                let msg = match serde_json::from_str::<WebClientMsg>(&text) {
                    Ok(msg) => msg,
                    Err(e) => {
                        log::debug!("kaku-remote web: bad message: {e}");
                        continue;
                    }
                };
                if matches!(&msg, WebClientMsg::Text { data } | WebClientMsg::Paste { data } if data.len() > MAX_INPUT_LEN) {
                    if let Some(json) = encode(&WebServerMsg::Error { detail: "Text is larger than 64 KB. Send it as a file instead." }) {
                        if outbox.send(json).await.is_err() { break; }
                    }
                    continue;
                }
                if let WebClientMsg::Hello { frame_ack } = msg {
                    flow_control = frame_ack;
                    continue;
                }
                if let WebClientMsg::FrameAck { seq } = msg {
                    if seq <= frames_sent { frames_acked = frames_acked.max(seq); }
                    continue;
                }
                if matches!(msg, WebClientMsg::Ping) {
                    let Some(json) = encode(&WebServerMsg::Pong) else { continue };
                    if outbox.send(json).await.is_err() {
                        break;
                    }
                    continue;
                }
                if let Some(reply) = handle_upload(&msg, &mut uploads.0, attached) {
                    let Some(json) = encode(&reply.as_msg()) else { continue };
                    if outbox.send(json).await.is_err() {
                        break;
                    }
                    continue;
                }
                if matches!(
                    msg,
                    WebClientMsg::UploadStart { .. }
                        | WebClientMsg::UploadChunk { .. }
                        | WebClientMsg::UploadCancel { .. }
                        | WebClientMsg::UploadEnd { .. }
                ) {
                    continue;
                }
                if let WebClientMsg::Search { query, older, restart } = &msg {
                    let Some(pane) = attached.and_then(lookup_pane) else { continue };
                    if *restart {
                        view.found_at = None;
                    }
                    let hit = search_pane(&pane, query, view.found_at, *older);
                    let reply = match hit {
                        Some((row, col, len)) => {
                            view.found_at = Some((row, col));
                            let dims = pane.get_dimensions();
                            let rows = dims.viewport_rows as isize;
                            let top = (row - rows / 3).clamp(dims.scrollback_top, dims.physical_top);
                            view.pinned_top = (top < dims.physical_top).then_some(top);
                            dirty_frame = true;
                            let shown_top = view.pinned_top.unwrap_or(dims.physical_top);
                            WebServerMsg::Found { found: true, row: (row - shown_top) as usize, col, len }
                        }
                        None => WebServerMsg::Found { found: false, row: 0, col: 0, len: 0 },
                    };
                    let Some(json) = encode(&reply) else { continue };
                    if outbox.send(json).await.is_err() {
                        break;
                    }
                    continue;
                }
                if matches!(msg, WebClientMsg::Redraw) {
                    shown = Shown::default();
                    dirty_frame = attached.is_some();
                    continue;
                }
                if let WebClientMsg::History { end, request } = msg {
                    let Some(pane_id) = attached else { continue };
                    let Some(pane) = lookup_pane(pane_id) else { continue };
                    let (first, top, rows) = history_rows(&pane, end);
                    let reply = WebServerMsg::History { pane_id, request, first, top, rows: &rows };
                    let Some(json) = encode(&reply) else { continue };
                    if outbox.send(json).await.is_err() {
                        break;
                    }
                    continue;
                }
                if let WebClientMsg::Verify { frames, rows } = &msg {
                    if *frames == frames_sent && attached.is_some() {
                        let drift = drifted_rows(&shown.lines, rows);
                        if !drift.is_empty() {
                            let pane = attached.and_then(lookup_pane);
                            log::warn!(
                                "kaku-remote web: screen drift in {} of {} rows (last move {}, {} frames since full, scrolled {}, alt screen {}, mouse grabbed {}); redrawing",
                                drift.len(),
                                shown.lines.len(),
                                shown.last_move,
                                shown.since_full,
                                shown.scrolled,
                                pane.as_ref().is_some_and(|p| p.is_alt_screen_active()),
                                pane.as_ref().is_some_and(|p| p.is_mouse_grabbed()),
                            );
                            shown = Shown::default();
                            dirty_frame = true;
                        }
                    }
                    continue;
                }
                if let WebClientMsg::Attach { pane_id, focus } = msg {
                    // A pane that is already gone gets the same answer as one
                    // that closes while viewed, and no entry in `dirty`.
                    if lookup_pane(pane_id).is_none() {
                        let Some(json) = encode(&WebServerMsg::Closed { pane_id }) else { continue };
                        if outbox.send(json).await.is_err() {
                            break;
                        }
                        continue;
                    }
                    if focus {
                        dispatch_tab_action(TabAction::Activate { pane_id });
                    }
                    attached = Some(pane_id);
                    view.claimed = None;
                    updates = Some(subscribe(&dirty, pane_id));
                    view.pinned_top = None;
                    view.seen_at = None;
                    mark_seen(pane_id, &mut view);
                    shown = Shown::default();
                    dirty_frame = true;
                    continue;
                }
                let Some(pane) = attached.and_then(lookup_pane) else { continue };
                handle_input(&pane, msg, &mut view);
                dirty_frame = true;
            }
            update = pane_update => match update {
                Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => dirty_frame = true,
                Err(broadcast::error::RecvError::Closed) => updates = None,
            },
            posted = notifications.recv() => {
                if let Ok(n) = posted {
                    let msg = WebServerMsg::Notify { pane_id: n.pane_id, title: &n.title, body: &n.body };
                    let Some(json) = encode(&msg) else { continue };
                    if outbox.send(json).await.is_err() {
                        break;
                    }
                }
            }
            copied = clipboard.recv() => {
                if let Ok((pane_id, text)) = copied {
                    if attached == Some(pane_id) {
                        let Some(json) = encode(&WebServerMsg::Clipboard { text: &text }) else { continue };
                        if outbox.send(json).await.is_err() {
                            break;
                        }
                    }
                }
            }
            _ = idle.tick() => {
                if flow_control && frames_sent > frames_acked && last_frame_at.elapsed() > Duration::from_secs(90) { break; }
                if last_heard.elapsed() > SESSION_SILENCE {
                    log::info!("kaku-remote web: no word from the browser for {}s, ending its session", SESSION_SILENCE.as_secs());
                    break;
                }
                dirty_frame = attached.is_some();
                // Tab state lives on the GUI thread; the close-confirmation
                // check reads the Lua config, which only that thread may touch.
                let tabs = promise::spawn::spawn_into_main_thread(async { collect_tabs() }).await;
                if last_tabs.as_ref() != Some(&tabs) {
                    let Some(json) = encode(&WebServerMsg::Tabs { tabs: &tabs }) else { continue };
                    if outbox.send(json).await.is_err() {
                        break;
                    }
                    last_tabs = Some(tabs);
                }
            }
            _ = frame_tick.tick(), if dirty_frame && (!flow_control || frames_sent - frames_acked < 4) => {
                dirty_frame = false;
                let Some(pane_id) = attached else { continue };
                let Some(pane) = lookup_pane(pane_id) else {
                    attached = None;
                    updates = None;
                    let msg = WebServerMsg::Closed { pane_id };
                    let Some(json) = encode(&msg) else { continue };
                    if outbox.send(json).await.is_err() {
                        break;
                    }
                    continue;
                };
                // Only rows that changed since the last frame are sent; idle
                // refreshes usually change nothing and send nothing.
                let Some((frame, data)) = shown.update(render_frame(&pane, &mut view)) else {
                    continue;
                };
                let msg = WebServerMsg::Frame {
                    seq: frames_sent + 1,
                    pane_id,
                    cols: frame.cols,
                    rows: frame.rows,
                    title: &frame.title,
                    scrolled: frame.scrolled,
                    local_scroll: frame.local_scroll,
                    data: &data,
                };
                let Some(json) = encode(&msg) else { continue };
                if outbox.send(json).await.is_err() {
                    break;
                }
                frames_sent += 1;
                last_frame_at = std::time::Instant::now();
            }
        }
    }
}

// ── Local WebSocket ───────────────────────────────────────────────────────────

pub(crate) async fn route_ws(
    Query(query): Query<WsQuery>,
    State(dirty): State<WebDirty>,
    headers: axum::http::HeaderMap,
    upgrade: ws::WebSocketUpgrade,
) -> Response {
    if !check_token(&query, &headers) {
        return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
    }
    upgrade.on_upgrade(move |socket| handle_local_ws(socket, dirty))
}

async fn handle_local_ws(socket: ws::WebSocket, dirty: WebDirty) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (in_tx, in_rx) = mpsc::channel::<String>(64);
    let (out_tx, mut out_rx) = mpsc::channel::<String>(64);
    let session = tokio::spawn(run_session(in_rx, out_tx, dirty));
    let writer = tokio::spawn(async move {
        while let Some(text) = out_rx.recv().await {
            if ws_tx.send(ws::Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });
    forward_until_session_ends(&mut ws_rx, in_tx, session).await;
    writer.abort();
}

/// Passes the browser's text messages to its session until either side is
/// done. The session ends on its own after `SESSION_SILENCE`; a half-open
/// socket (a laptop that went to sleep) never delivers another message, so
/// waiting on the socket alone kept the task and its descriptor until TCP
/// gave up.
async fn forward_until_session_ends<S, E>(
    incoming: &mut S,
    in_tx: mpsc::Sender<String>,
    mut session: tokio::task::JoinHandle<()>,
) where
    S: futures_util::Stream<Item = Result<ws::Message, E>> + Unpin,
{
    loop {
        tokio::select! {
            _ = &mut session => return,
            msg = incoming.next() => match msg {
                Some(Ok(ws::Message::Text(text))) => {
                    if in_tx.send(text.to_string()).await.is_err() {
                        break;
                    }
                }
                Some(Ok(ws::Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    drop(in_tx);
    let _ = session.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use termwiz::color::SrgbaTuple;

    /// Notification text is capped by characters, never mid-character.
    #[test]
    fn notification_text_is_clipped_on_character_boundaries() {
        assert_eq!(&*clip("Done", 120), "Done");
        assert_eq!(&*clip("构建完成了", 2), "构建");
        assert_eq!(clip(&"a".repeat(600), 500).len(), 500);
    }

    /// A browser that went silent behind a half-open socket: the session gives
    /// up, and the socket must be let go with it instead of waiting forever.
    #[tokio::test]
    async fn a_finished_session_releases_a_silent_socket() {
        let mut silent = futures_util::stream::pending::<Result<ws::Message, ()>>();
        let (in_tx, _in_rx) = mpsc::channel::<String>(1);
        let session = tokio::spawn(async {});
        let done = tokio::time::timeout(
            Duration::from_secs(2),
            forward_until_session_ends(&mut silent, in_tx, session),
        )
        .await;
        assert!(done.is_ok());
    }

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
    fn web_sized_tabs_keep_their_size_until_the_mac_window_is_dragged() {
        let tab = usize::MAX - 7;
        assert!(window_relayout_resizes_tab(tab, false));
        WEB_SIZED_TABS.lock().insert(tab);
        assert!(!window_relayout_resizes_tab(tab, false));
        assert!(tab_is_web_sized(tab));
        assert!(window_relayout_resizes_tab(tab, true));
        assert!(!tab_is_web_sized(tab));
    }

    #[test]
    fn a_split_pane_gets_the_browser_grid() {
        // Half of a 161-column tab is 80: a 100-column browser grows the tab
        // so that half comes out at 100.
        assert_eq!(split_share(100, 161, 80), 201);
        // A lone pane is the whole tab.
        assert_eq!(split_share(100, 80, 80), 100);
        assert_eq!(split_share(100, 80, 0), 100);
    }

    #[test]
    fn closed_names_the_pane_it_reports() {
        // The page ignores a closed notice for a pane it already left; without
        // the id it dropped the tab it had just switched to.
        let json = encode(&WebServerMsg::Closed { pane_id: 7 }).unwrap();
        assert_eq!(json, r#"{"type":"closed","pane_id":7}"#);
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

    fn frame(cols: usize, lines: &[&str], cursor: &str) -> Frame {
        Frame {
            cols,
            rows: lines.len(),
            title: "t".into(),
            scrolled: false,
            local_scroll: true,
            lines: lines.iter().map(|l| l.to_string()).collect(),
            cursor: cursor.into(),
        }
    }

    #[test]
    fn frames_send_only_changed_rows() {
        let mut shown = Shown::default();
        let (_, first) = shown.update(frame(10, &["A", "B", "C"], "@1")).unwrap();
        assert_eq!(first, "\x1b[?25lABC@1", "first frame is complete");

        let (_, second) = shown.update(frame(10, &["A", "X", "C"], "@1")).unwrap();
        assert_eq!(second, "\x1b[?25lX@1", "only the changed row");

        assert!(
            shown.update(frame(10, &["A", "X", "C"], "@1")).is_none(),
            "nothing changed, nothing sent"
        );

        let (_, cursor_only) = shown.update(frame(10, &["A", "X", "C"], "@2")).unwrap();
        assert_eq!(cursor_only, "\x1b[?25l@2");

        let (_, resized) = shown.update(frame(12, &["A", "X", "C"], "@2")).unwrap();
        assert_eq!(resized, "\x1b[?25lAXC@2", "a resize redraws everything");
    }

    /// Rows as render_frame writes them: each carries its own position.
    fn rows(texts: &[&str]) -> Vec<String> {
        texts
            .iter()
            .enumerate()
            .map(|(i, t)| format!("\x1b[{};1H{t}", i + 1))
            .collect()
    }

    /// Applies what Shown sends to a model of the browser screen: row
    /// positions, scroll up (S) and scroll down (T) with blank rows coming in.
    fn apply(screen: &mut [String], data: &str) {
        let mut rest = data;
        while let Some(at) = rest.find("\x1b[") {
            rest = &rest[at + 2..];
            let end = rest.find(|c: char| c.is_ascii_alphabetic()).unwrap();
            let (params, cmd) = (&rest[..end], &rest[end..=end]);
            rest = &rest[end + 1..];
            let n: usize = params.split(';').next().unwrap().parse().unwrap_or(1);
            match cmd {
                "S" => {
                    screen.rotate_left(n);
                    let len = screen.len();
                    screen[len - n..].iter_mut().for_each(String::clear);
                }
                "T" => {
                    screen.rotate_right(n);
                    screen[..n].iter_mut().for_each(String::clear);
                }
                "H" if params.ends_with(";1") => {
                    let text = rest.split("\x1b[").next().unwrap();
                    screen[n - 1] = text.to_string();
                }
                _ => {}
            }
        }
    }

    #[test]
    fn moved_rows_are_scrolled_not_resent() {
        let texts = ["l1", "l2", "l3", "l4", "l5", "l6", "l7", "l8"];
        let mut shown = Shown::default();
        let mut screen = vec![String::new(); 8];
        let (_, first) = shown
            .update(Frame {
                lines: rows(&texts),
                ..frame(10, &[], "")
            })
            .unwrap();
        apply(&mut screen, &first);

        // Two new lines of output push everything up.
        let up = ["l3", "l4", "l5", "l6", "l7", "l8", "n1", "n2"];
        let (_, data) = shown
            .update(Frame {
                lines: rows(&up),
                ..frame(10, &[], "")
            })
            .unwrap();
        assert!(data.contains("\x1b[2S"), "scrolls up by two: {data:?}");
        assert_eq!(data.matches(";1H").count(), 2, "sends only the new rows");
        apply(&mut screen, &data);
        assert_eq!(screen, up);

        // Scrolling back into history moves it down again, with a row edited.
        let down = ["l1", "l2", "l3", "l4", "X5", "l6", "l7", "l8"];
        let (_, data) = shown
            .update(Frame {
                lines: rows(&down),
                ..frame(10, &[], "")
            })
            .unwrap();
        assert!(data.contains("\x1b[2T"), "scrolls down by two: {data:?}");
        assert_eq!(data.matches(";1H").count(), 3);
        apply(&mut screen, &data);
        assert_eq!(screen, down);
    }

    #[test]
    fn unrelated_screens_are_not_treated_as_moved() {
        let mut shown = Shown::default();
        shown.update(Frame {
            lines: rows(&["a", "b", "c", "d", "e", "f"]),
            ..frame(10, &[], "")
        });
        let (_, data) = shown
            .update(Frame {
                lines: rows(&["u", "v", "w", "x", "y", "z"]),
                ..frame(10, &[], "")
            })
            .unwrap();
        assert!(!data.contains('S') && !data.contains('T'), "{data:?}");
    }

    #[test]
    fn drift_compares_text_not_spacing() {
        let shown = rows(&["\x1b[0;31m中文 abc", "same", "kaku row"]);
        let page = vec![
            "中文  abc".to_string(),
            " same ".to_string(),
            "stale row".to_string(),
        ];
        assert_eq!(drifted_rows(&shown, &page), vec![2]);
        assert_eq!(
            row_text("\x1b[3;1H\x1b[0m⏺ hi\x1b[5G!\x1b[0m\x1b[K"),
            "⏺hi!"
        );
    }

    fn rt(row: isize, text: &str) -> RowText {
        let mut cols = Vec::new();
        let mut ends = Vec::new();
        let mut col = 0;
        for ch in text.chars() {
            cols.push(col);
            col += if ch.is_ascii() { 1 } else { 2 };
            ends.push(col);
        }
        RowText {
            row,
            text: text.to_lowercase(),
            cols,
            ends,
        }
    }

    #[test]
    fn search_walks_matches_in_both_directions() {
        let rows = vec![
            rt(10, "error one"),
            rt(11, "fine"),
            rt(12, "中文 error two error"),
        ];
        // "中文 " takes five cells, so the matches start at columns 5 and 15.
        // From the bottom, older first: the last match in the newest row.
        let older = |from| find_in_rows(&rows, "error", from, true);
        assert_eq!(older(None), Some((12, 15, 5)));
        assert_eq!(older(Some((12, 15))), Some((12, 5, 5)));
        assert_eq!(older(Some((12, 5))), Some((10, 0, 5)));
        assert_eq!(older(Some((10, 0))), None);
        // And newer from there.
        let newer = find_in_rows(&rows, "error", Some((10, 0)), false);
        assert_eq!(newer, Some((12, 5, 5)));
        // Wide characters take two cells.
        assert_eq!(find_in_rows(&rows, "中文", None, true), Some((12, 0, 4)));
    }

    #[test]
    fn chunks_are_acknowledged_and_unfinished_uploads_are_removed() {
        use base64::Engine as _;
        let path = std::env::temp_dir().join(format!("kaku-upload-test-{}", std::process::id()));
        let mut uploads = Uploads::default();
        uploads.0.insert(
            7,
            Upload {
                file: std::fs::File::create(&path).unwrap(),
                path: path.clone(),
                size: 6,
                written: 0,
                pane_id: None,
                transfer: String::new(),
                name: String::new(),
            },
        );
        let chunk = WebClientMsg::UploadChunk {
            id: 7,
            data: base64::engine::general_purpose::STANDARD.encode(b"abc"),
        };
        let reply = handle_upload(&chunk, &mut uploads.0, None).expect("a chunk is acknowledged");
        assert_eq!(reply.received, Some(3));
        assert!(matches!(
            reply.as_msg(),
            WebServerMsg::UploadAck { id: 7, received: 3 }
        ));
        // The session ends before upload_end: the partial file goes.
        assert!(path.exists());
        drop(uploads);
        assert!(!path.exists());
    }

    #[test]
    fn an_incomplete_upload_cannot_be_completed() {
        let path = std::env::temp_dir().join(format!("kaku-incomplete-{}", std::process::id()));
        let mut uploads = Uploads::default();
        uploads.0.insert(
            99,
            Upload {
                file: std::fs::File::create(&path).unwrap(),
                path: path.clone(),
                size: 6,
                written: 0,
                pane_id: None,
                transfer: String::new(),
                name: String::new(),
            },
        );
        let result =
            handle_upload(&WebClientMsg::UploadEnd { id: 99 }, &mut uploads.0, None).unwrap();
        // Remove our fixture even on the old implementation, which keeps it.
        let remains = path.exists();
        let _ = std::fs::remove_file(path);
        assert!(!result.ok, "no data reached the Mac");
        assert!(!remains, "unfinished file is discarded");
    }

    #[test]
    fn completed_upload_receipt_survives_reconnect_without_pasting_again() {
        let path = std::env::temp_dir().join(format!("kaku-receipt-{}", std::process::id()));
        let transfer = "ab".repeat(16);
        let mut uploads = HashMap::new();
        uploads.insert(
            7,
            Upload {
                file: std::fs::File::create(&path).unwrap(),
                path: path.clone(),
                size: 0,
                written: 0,
                pane_id: Some(42),
                transfer: transfer.clone(),
                name: "empty.txt".into(),
            },
        );
        // Focus has moved to 99, but completion belongs to 42.
        let first =
            handle_upload(&WebClientMsg::UploadEnd { id: 7 }, &mut uploads, Some(99)).unwrap();
        assert!(first.ok);
        assert_eq!(
            UPLOAD_RECEIPTS.lock().get(&transfer).unwrap().pane_id,
            Some(42)
        );
        // A fresh session repeats the start after losing the final reply.
        let mut reconnected = HashMap::new();
        let reply = handle_upload(
            &WebClientMsg::UploadStart {
                id: 8,
                name: "empty.txt".into(),
                size: 0,
                transfer: transfer.clone(),
                pane_id: Some(42),
            },
            &mut reconnected,
            Some(99),
        )
        .unwrap();
        assert!(reply.ok);
        assert_eq!(reply.detail, first.detail);
        assert!(
            reconnected.is_empty(),
            "a completed retry never opens a second file"
        );
        let mismatch = handle_upload(
            &WebClientMsg::UploadStart {
                id: 9,
                name: "different.txt".into(),
                size: 0,
                transfer: transfer.clone(),
                pane_id: Some(42),
            },
            &mut reconnected,
            Some(99),
        )
        .unwrap();
        assert!(!mismatch.ok);
        UPLOAD_RECEIPTS.lock().remove(&transfer);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cancelled_upload_discards_only_its_partial_file() {
        let path = std::env::temp_dir().join(format!("kaku-cancel-{}", std::process::id()));
        let mut uploads = HashMap::new();
        uploads.insert(
            7,
            Upload {
                file: std::fs::File::create(&path).unwrap(),
                path: path.clone(),
                size: 5,
                written: 0,
                pane_id: Some(42),
                transfer: String::new(),
                name: "partial".into(),
            },
        );
        assert!(handle_upload(&WebClientMsg::UploadCancel { id: 7 }, &mut uploads, None).is_none());
        assert!(!path.exists());
        assert!(uploads.is_empty());
    }

    #[test]
    fn matching_dimensions_still_record_browser_ownership() {
        let source = include_str!("web.rs");
        let claim = source
            .split("fn claim_tab_size(")
            .nth(1)
            .unwrap()
            .split("fn split_share(")
            .next()
            .unwrap();
        assert!(
            claim
                .find("WEB_SIZED_TABS.lock().insert(tab_id.into());")
                .unwrap()
                < claim
                    .find("if current.cols == cols && current.rows == rows {")
                    .unwrap()
        );
    }

    #[test]
    fn uploaded_names_cannot_leave_the_folder() {
        assert_eq!(safe_file_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_file_name("C:\\Users\\a\\shot.png"), "shot.png");
        assert_eq!(safe_file_name(".bashrc"), "bashrc");
        assert_eq!(safe_file_name(""), "file");
        assert_eq!(safe_file_name("..."), "file");
        assert_eq!(safe_file_name("a\u{7}b.md"), "ab.md");
        let dir = tempfile_dir();
        std::fs::write(dir.join("note.md"), "x").unwrap();
        assert_eq!(unused_path(&dir, "note.md"), dir.join("note (2).md"));
        assert_eq!(unused_path(&dir, "new.md"), dir.join("new.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kaku-web-upload-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
