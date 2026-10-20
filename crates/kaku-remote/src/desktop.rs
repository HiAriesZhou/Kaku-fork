//! The web page's view of the Mac's own screen, from a separate helper,
//! `kaku-desktop`, which captures the display, encodes it and posts the
//! page's mouse and keyboard. Kaku only carries its messages: the page's
//! `desktop_*` messages go to the helper's stdin, and the `desktop_*` lines
//! it writes go back to the page, over the session's encrypted channel.
//!
//! Pictures go to a page that asks for it as binary messages: a zero byte,
//! the frame's JSON header length (u32, big endian), the header, then the
//! H.264 bytes. Sent as JSON, base64 made every picture a third larger.
//!
//! The helper runs as Kaku's child, so macOS counts its screen capture and
//! input against Kaku's own permissions. Built only with the `desktop`
//! feature, and offered to the page only when the helper is installed.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const PREFIX: &str = "desktop_";
/// A line from the helper larger than this is dropped: sealed, it would
/// pass the relay's 4 MB limit for one message.
const MAX_LINE: usize = 3 << 20;
/// How long a stopped helper gets to let go of held buttons and exit.
const STOP_GRACE: Duration = Duration::from_secs(1);
/// First byte of a binary message; a JSON one starts with `{`.
const BINARY_FRAME: u8 = 0;

pub(crate) fn helper_path() -> PathBuf {
    config::DATA_DIR.join("kaku-desktop")
}

/// Whether to offer the desktop view to the page.
pub(crate) fn available() -> bool {
    is_executable(&helper_path())
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The message's type when it is one of the desktop's.
fn desktop_type(text: &str) -> Option<String> {
    if !text.contains("\"desktop_") {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let kind = value.get("type")?.as_str()?;
    kind.starts_with(PREFIX).then(|| kind.to_string())
}

/// A `desktop_frame` line from the helper as one binary message, or None
/// when it is not one.
fn binary_frame(line: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    if !line.contains("\"desktop_frame\"") {
        return None;
    }
    let mut value: serde_json::Value = serde_json::from_str(line).ok()?;
    let header = value.as_object_mut()?;
    if header.get("type")?.as_str()? != "desktop_frame" {
        return None;
    }
    let data = header.remove("data")?;
    let data = base64::engine::general_purpose::STANDARD
        .decode(data.as_str()?)
        .ok()?;
    let header = serde_json::to_vec(&value).ok()?;
    let mut out = Vec::with_capacity(5 + header.len() + data.len());
    out.push(BINARY_FRAME);
    out.extend_from_slice(&u32::try_from(header.len()).ok()?.to_be_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(&data);
    Some(out)
}

/// Whether a `desktop_start` asks for pictures as binary messages.
fn wants_binary(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get("binary")?.as_bool())
        .unwrap_or(false)
}

/// Takes the page's desktop messages out of `inbox` and returns the inbox
/// the terminal session reads, which sees every other message unchanged.
/// Pictures go to `frames` when the page takes binary messages.
pub(crate) fn tap(
    inbox: mpsc::Receiver<String>,
    outbox: mpsc::Sender<String>,
    frames: mpsc::Sender<Vec<u8>>,
) -> mpsc::Receiver<String> {
    tap_with(inbox, outbox, frames, helper_path())
}

fn tap_with(
    mut inbox: mpsc::Receiver<String>,
    outbox: mpsc::Sender<String>,
    frames: mpsc::Sender<Vec<u8>>,
    helper: PathBuf,
) -> mpsc::Receiver<String> {
    let (session, session_inbox) = mpsc::channel::<String>(64);
    tokio::spawn(async move {
        let mut running: Option<Helper> = None;
        let binary = Arc::new(AtomicBool::new(false));
        while let Some(text) = inbox.recv().await {
            let Some(kind) = desktop_type(&text) else {
                if session.send(text).await.is_err() {
                    break;
                }
                continue;
            };
            if kind == "desktop_start" {
                binary.store(wants_binary(&text), Ordering::SeqCst);
            }
            if kind == "desktop_start" && running.as_mut().is_none_or(|h| h.exited()) {
                let out = Out {
                    text: outbox.clone(),
                    frames: frames.clone(),
                    binary: binary.clone(),
                };
                running = match Helper::spawn(&helper, out) {
                    Ok(h) => Some(h),
                    Err(e) => {
                        log::warn!(
                            "kaku-remote desktop: cannot start {}: {e:#}",
                            helper.display()
                        );
                        let detail = format!("Kaku Desktop did not start: {e}");
                        let status = serde_json::json!({ "type": "desktop_status", "state": "error", "detail": detail });
                        let _ = outbox.send(status.to_string()).await;
                        None
                    }
                };
            }
            if kind == "desktop_stop" {
                if let Some(h) = running.take() {
                    h.stop().await;
                }
                continue;
            }
            if let Some(h) = running.as_mut() {
                if h.write(&text).await.is_err() {
                    running = None;
                }
            }
        }
        if let Some(h) = running.take() {
            h.stop().await;
        }
    });
    session_inbox
}

/// Where the helper's lines go: pictures as binary messages when the page
/// asked for them, everything else as the JSON the helper wrote.
struct Out {
    text: mpsc::Sender<String>,
    frames: mpsc::Sender<Vec<u8>>,
    binary: Arc<AtomicBool>,
}

struct Helper {
    child: Child,
    stdin: Option<ChildStdin>,
    reader: JoinHandle<()>,
    /// Set before Kaku ends the helper, so only an exit of its own is
    /// reported to the page.
    stopping: Arc<AtomicBool>,
}

impl Helper {
    fn spawn(path: &Path, out: Out) -> anyhow::Result<Self> {
        let Out {
            text: outbox,
            frames,
            binary,
        } = out;
        let mut child = Command::new(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdout"))?;
        let stdin = child.stdin.take();
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    log::info!("{line}");
                }
            });
        }
        let stopping = Arc::new(AtomicBool::new(false));
        let stopped = stopping.clone();
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.len() > MAX_LINE {
                    log::warn!("kaku-remote desktop: dropped a {} byte message", line.len());
                    continue;
                }
                // The helper speaks only for the desktop view; anything
                // else it writes never reaches the page.
                if desktop_type(&line).is_none() {
                    continue;
                }
                if binary.load(Ordering::SeqCst) {
                    if let Some(frame) = binary_frame(&line) {
                        if frames.send(frame).await.is_err() {
                            return;
                        }
                        continue;
                    }
                }
                if outbox.send(line).await.is_err() {
                    return;
                }
            }
            // The helper ended on its own (a crash, a lost permission): the
            // page would otherwise keep showing its last picture.
            if !stopped.load(Ordering::SeqCst) {
                log::warn!("kaku-remote desktop: the helper exited");
                let status = serde_json::json!({ "type": "desktop_status", "state": "error", "detail": "Kaku Desktop stopped. Switch the view off and on to start it again." });
                let _ = outbox.send(status.to_string()).await;
            }
        });
        log::info!("kaku-remote desktop: started {}", path.display());
        Ok(Self {
            child,
            stdin,
            reader,
            stopping,
        })
    }

    fn exited(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }

    async fn write(&mut self, text: &str) -> std::io::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
        stdin.write_all(text.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await
    }

    /// Ask the helper to let go of the mouse and end, then make sure it did.
    async fn stop(mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        let _ = self.write("{\"type\":\"desktop_stop\"}").await;
        drop(self.stdin.take());
        if tokio::time::timeout(STOP_GRACE, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
        self.reader.abort();
        log::info!("kaku-remote desktop: stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_desktop_messages_are_the_helpers() {
        assert_eq!(
            desktop_type(r#"{"type":"desktop_start","fps":30}"#).as_deref(),
            Some("desktop_start")
        );
        assert_eq!(
            desktop_type(r#"{"type":"text","data":"\"desktop_start"}"#),
            None
        );
        assert_eq!(desktop_type(r#"{"type":"key","key":"a"}"#), None);
        assert_eq!(desktop_type("not json \"desktop_"), None);
    }

    /// A stand-in helper: answers each line with a desktop status naming it,
    /// and also writes a terminal frame, which must not reach the page.
    fn fake_helper(dir: &Path) -> PathBuf {
        let path = dir.join("kaku-desktop");
        std::fs::write(
            &path,
            "#!/bin/sh\nwhile read -r line; do\n  echo '{\"type\":\"frame\",\"data\":\"forged\"}'\n  printf '{\"type\":\"desktop_status\",\"state\":\"got\",\"line\":%s}\\n' \"$line\"\ndone\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn desktop_messages_reach_the_helper_and_the_rest_the_session() {
        let dir = std::env::temp_dir().join(format!("kaku-desktop-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let helper = fake_helper(&dir);
        let (page, inbox) = mpsc::channel(8);
        let (outbox, mut to_page) = mpsc::channel(8);
        let (frames, _) = mpsc::channel(8);
        let mut session = tap_with(inbox, outbox, frames, helper);

        page.send(r#"{"type":"key","key":"Enter"}"#.to_string())
            .await
            .unwrap();
        assert_eq!(
            session.recv().await.unwrap(),
            r#"{"type":"key","key":"Enter"}"#
        );

        page.send(r#"{"type":"desktop_start","fps":30}"#.to_string())
            .await
            .unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(5), to_page.recv())
            .await
            .unwrap()
            .unwrap();
        let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["type"], "desktop_status");
        assert_eq!(reply["line"]["type"], "desktop_start");

        // Stopping ends the helper; nothing it wrote besides desktop lines
        // got through, and the session keeps working.
        page.send(r#"{"type":"desktop_stop"}"#.to_string())
            .await
            .unwrap();
        page.send(r#"{"type":"text","data":"x"}"#.to_string())
            .await
            .unwrap();
        assert_eq!(
            session.recv().await.unwrap(),
            r#"{"type":"text","data":"x"}"#
        );
        while let Ok(Some(line)) =
            tokio::time::timeout(Duration::from_millis(300), to_page.recv()).await
        {
            assert!(line.contains("desktop_"), "{line}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_missing_helper_is_reported_to_the_page() {
        let (page, inbox) = mpsc::channel(8);
        let (outbox, mut to_page) = mpsc::channel(8);
        let (frames, _) = mpsc::channel(8);
        let _session = tap_with(
            inbox,
            outbox,
            frames,
            PathBuf::from("/nonexistent/kaku-desktop"),
        );
        page.send(r#"{"type":"desktop_start"}"#.to_string())
            .await
            .unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(5), to_page.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(reply.contains("\"state\":\"error\""), "{reply}");
    }

    #[tokio::test]
    async fn a_helper_that_ends_on_its_own_is_reported_but_a_stopped_one_is_not() {
        let dir = std::env::temp_dir().join(format!("kaku-desktop-exit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kaku-desktop");
        // Ends with an error when told to crash, otherwise when stdin closes.
        std::fs::write(
            &path,
            "#!/bin/sh\nwhile read -r line; do\n  case \"$line\" in *crash*) exit 3;; esac\ndone\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (page, inbox) = mpsc::channel(8);
        let (outbox, mut to_page) = mpsc::channel(8);
        let (frames, _) = mpsc::channel(8);
        let _session = tap_with(inbox, outbox, frames, path);
        page.send(r#"{"type":"desktop_start"}"#.to_string())
            .await
            .unwrap();
        page.send(r#"{"type":"desktop_crash"}"#.to_string())
            .await
            .unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(5), to_page.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(reply.contains("\"state\":\"error\""), "{reply}");
        assert!(reply.contains("stopped"), "{reply}");

        // Started again and stopped by the page: nothing to report.
        page.send(r#"{"type":"desktop_start"}"#.to_string())
            .await
            .unwrap();
        page.send(r#"{"type":"desktop_stop"}"#.to_string())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(1500), to_page.recv())
                .await
                .is_err(),
            "a stop the page asked for is not an error"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_picture_becomes_one_binary_message() {
        let line = r#"{"seq":7,"type":"desktop_frame","key":true,"data":"AAEC/w=="}"#;
        let frame = binary_frame(line).unwrap();
        assert_eq!(frame[0], BINARY_FRAME);
        let len = u32::from_be_bytes(frame[1..5].try_into().unwrap()) as usize;
        let header: serde_json::Value = serde_json::from_slice(&frame[5..5 + len]).unwrap();
        assert_eq!(header["seq"], 7);
        assert_eq!(header["type"], "desktop_frame");
        assert!(header.get("data").is_none());
        assert_eq!(&frame[5 + len..], &[0, 1, 2, 255]);

        assert!(binary_frame(r#"{"type":"desktop_status","state":"streaming"}"#).is_none());
        assert!(binary_frame(r#"{"type":"desktop_frame","data":"not base64!"}"#).is_none());
        assert!(binary_frame(r#"{"type":"text","x":"\"desktop_frame\""}"#).is_none());
    }

    async fn next(rx: &mut mpsc::Receiver<String>) -> String {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn pictures_go_binary_only_to_a_page_that_asks() {
        let dir = std::env::temp_dir().join(format!("kaku-desktop-bin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kaku-desktop");
        // Answers every start with a status and one picture.
        std::fs::write(
            &path,
            "#!/bin/sh\nwhile read -r line; do\n  case \"$line\" in *desktop_start*)\n    echo '{\"type\":\"desktop_status\",\"state\":\"streaming\"}'\n    echo '{\"type\":\"desktop_frame\",\"seq\":1,\"data\":\"AAEC\"}';;\n  esac\ndone\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (page, inbox) = mpsc::channel(8);
        let (outbox, mut to_page) = mpsc::channel(8);
        let (frames, mut frames_to_page) = mpsc::channel(8);
        let _session = tap_with(inbox, outbox, frames, path);

        page.send(r#"{"type":"desktop_start"}"#.to_string())
            .await
            .unwrap();
        assert!(next(&mut to_page).await.contains("streaming"));
        assert!(next(&mut to_page).await.contains("\"data\":\"AAEC\""));

        page.send(r#"{"type":"desktop_start","binary":true}"#.to_string())
            .await
            .unwrap();
        assert!(next(&mut to_page).await.contains("streaming"));
        let frame = tokio::time::timeout(Duration::from_secs(5), frames_to_page.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frame[0], BINARY_FRAME);
        assert!(frame.ends_with(&[0, 1, 2]));

        page.send(r#"{"type":"desktop_stop"}"#.to_string())
            .await
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
