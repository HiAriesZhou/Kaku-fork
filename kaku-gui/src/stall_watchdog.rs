//! Records GUI thread stalls so `kaku doctor` can bundle the evidence.
//!
//! A background thread posts a no-op onto the GUI thread every tick. When it
//! stays unanswered past `STALL_THRESHOLD` the GUI thread is blocked, so the
//! watchdog runs `/usr/bin/sample` on this process while the stall is still
//! happening and logs how long it lasted. Intermittent stalls (#559) are over
//! before a user can run a diagnostic by hand, which is why the capture has to
//! happen here, at the moment of the stall.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(250);
const STALL_THRESHOLD: Duration = Duration::from_secs(1);
/// Cold start legitimately keeps the GUI thread busy (GPU init, first paint).
const STARTUP_GRACE: Duration = Duration::from_secs(10);
/// A tick that wakes this much late means the whole process was throttled
/// (App Nap, system sleep), not that the GUI thread alone is stuck.
const THROTTLE_SLACK: Duration = Duration::from_millis(750);
const SAMPLE_SECONDS: &str = "2";
const MIN_CAPTURE_INTERVAL: Duration = Duration::from_secs(10 * 60);
const MAX_CAPTURES: usize = 3;
pub const CAPTURE_PREFIX: &str = "kaku-hang-";

pub fn start() {
    let spawned = std::thread::Builder::new()
        .name("stall-watchdog".into())
        .spawn(run);
    if let Err(err) = spawned {
        log::warn!("stall watchdog not started: {err:#}");
    }
}

fn run() {
    std::thread::sleep(STARTUP_GRACE);
    let mut last_capture: Option<Instant> = None;
    loop {
        // The GUI thread stamps when it ran the ping, so the logged stall
        // length stays exact even while this thread is busy running sample.
        let answered: Arc<OnceLock<Instant>> = Arc::new(OnceLock::new());
        let stamp = Arc::clone(&answered);
        let posted = Instant::now();
        promise::spawn::spawn_into_main_thread(async move {
            let _ = stamp.set(Instant::now());
        })
        .detach();

        let mut stall_start: Option<Instant> = None;
        // Time the wait from the last late tick, not from the post: a stall
        // that begins right after a system sleep or App Nap must still count.
        let mut since = posted;
        let mut rescued = false;
        while answered.get().is_none() {
            let before = Instant::now();
            std::thread::sleep(TICK);
            if before.elapsed() > TICK + THROTTLE_SLACK {
                since = Instant::now();
            }
            if stall_due(
                stall_start.is_some(),
                answered.get().is_some(),
                since.elapsed(),
            ) {
                stall_start = Some(since);
                log::warn!("GUI thread blocked for over {STALL_THRESHOLD:?}");
                if capture_due(last_capture, Instant::now()) {
                    last_capture = Some(Instant::now());
                    capture_sample();
                }
            }
            if stall_start.is_some() && !rescued && answered.get().is_none() {
                rescued = true;
                log::warn!(
                    "GUI thread still blocked: {}",
                    window::main_thread_queue_state()
                );
                window::rescue_main_thread_queue();
            }
        }
        if let (Some(start), Some(ran)) = (stall_start, answered.get()) {
            log::warn!("GUI thread unblocked after {:?}", ran.duration_since(start));
        }
        std::thread::sleep(TICK);
    }
}

/// Re-check `answered` after the sleep: a ping run just before the threshold
/// is not a stall, and counting it logged sub-second waits as over 1s.
fn stall_due(already_stalled: bool, answered: bool, waited: Duration) -> bool {
    !already_stalled && !answered && waited >= STALL_THRESHOLD
}

fn capture_due(last: Option<Instant>, now: Instant) -> bool {
    last.map_or(true, |at| now.duration_since(at) >= MIN_CAPTURE_INTERVAL)
}

fn capture_sample() {
    let dir = config::RUNTIME_DIR.clone();
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = dir.join(format!("{CAPTURE_PREFIX}{stamp}.txt"));
    let pid = std::process::id().to_string();
    let status = std::process::Command::new("/usr/bin/sample")
        .args([pid.as_str(), SAMPLE_SECONDS, "-mayDie", "-file"])
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => log::warn!("GUI thread stall sampled to {}", path.display()),
        Ok(s) => log::warn!("sample exited with {s} while capturing a GUI thread stall"),
        Err(err) => log::warn!("could not run sample for a GUI thread stall: {err:#}"),
    }
    prune_captures(&dir, MAX_CAPTURES);
}

/// Keep only the newest `keep` captures; the file name carries the timestamp.
fn prune_captures(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut captures: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(CAPTURE_PREFIX) && n.ends_with(".txt"))
        })
        .collect();
    captures.sort();
    let excess = captures.len().saturating_sub(keep);
    for old in captures.into_iter().take(excess) {
        let _ = std::fs::remove_file(old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ping_answered_by_the_threshold_is_not_a_stall() {
        let past = STALL_THRESHOLD + TICK;
        assert!(stall_due(false, false, past));
        assert!(!stall_due(false, true, past));
        assert!(!stall_due(true, false, past));
        assert!(!stall_due(false, false, STALL_THRESHOLD - TICK));
    }

    #[test]
    fn captures_are_rate_limited() {
        let now = Instant::now();
        assert!(capture_due(None, now));
        let recent = now.checked_sub(Duration::from_secs(60)).unwrap();
        assert!(!capture_due(Some(recent), now));
        let old = now.checked_sub(MIN_CAPTURE_INTERVAL).unwrap();
        assert!(capture_due(Some(old), now));
    }

    #[test]
    fn prune_keeps_the_newest_captures_only() {
        let dir = tempfile::tempdir().unwrap();
        for stamp in [
            "20260101-000001",
            "20260101-000003",
            "20260101-000002",
            "20260101-000004",
        ] {
            std::fs::write(dir.path().join(format!("{CAPTURE_PREFIX}{stamp}.txt")), "x").unwrap();
        }
        std::fs::write(dir.path().join("kaku-gui-log-1.txt"), "keep").unwrap();
        prune_captures(dir.path(), 3);
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                "kaku-gui-log-1.txt".to_string(),
                format!("{CAPTURE_PREFIX}20260101-000002.txt"),
                format!("{CAPTURE_PREFIX}20260101-000003.txt"),
                format!("{CAPTURE_PREFIX}20260101-000004.txt"),
            ]
        );
    }
}
