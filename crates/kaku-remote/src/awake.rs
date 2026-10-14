//! Keeps macOS from napping Kaku while a browser is connected.
//!
//! With the window in the background (the usual case when the Mac is used
//! remotely), App Nap throttles the main thread, where pty output becomes
//! terminal state and the pane change is announced. Each keystroke's echo then
//! waited 100-200 ms before the web session could render it. A latency-critical
//! activity lifts that throttling; idle system sleep stays allowed.

use parking_lot::Mutex;

/// Held by each web session; the activity lasts while any guard is alive.
pub(crate) struct Awake(());

static HOLDERS: Mutex<(usize, Option<platform::Activity>)> = Mutex::new((0, None));

impl Awake {
    pub(crate) fn hold() -> Self {
        let mut holders = HOLDERS.lock();
        holders.0 += 1;
        if holders.1.is_none() {
            holders.1 = platform::begin();
        }
        Awake(())
    }
}

impl Drop for Awake {
    fn drop(&mut self) {
        let mut holders = HOLDERS.lock();
        holders.0 -= 1;
        if holders.0 == 0 {
            if let Some(activity) = holders.1.take() {
                platform::end(activity);
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use objc2::rc::Retained;
    use objc2::runtime::{NSObjectProtocol, ProtocolObject};
    use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};

    pub(super) struct Activity(Retained<ProtocolObject<dyn NSObjectProtocol>>);

    // NSProcessInfo's activity calls are thread-safe and the token is only
    // handed back to endActivity, so it may move between session threads.
    unsafe impl Send for Activity {}

    pub(super) fn begin() -> Option<Activity> {
        let options = NSActivityOptions::UserInitiatedAllowingIdleSystemSleep
            | NSActivityOptions::LatencyCritical;
        let reason = NSString::from_str("Kaku web terminal session");
        let token = NSProcessInfo::processInfo().beginActivityWithOptions_reason(options, &reason);
        log::info!("kaku-remote: web session connected, App Nap held off");
        Some(Activity(token))
    }

    pub(super) fn end(activity: Activity) {
        // SAFETY: the token came from beginActivityWithOptions_reason.
        unsafe { NSProcessInfo::processInfo().endActivity(&activity.0) };
        log::info!("kaku-remote: last web session gone, App Nap allowed again");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_lasts_while_any_session_holds_it() {
        let a = Awake::hold();
        let b = Awake::hold();
        assert_eq!(HOLDERS.lock().0, 2);
        drop(a);
        assert_eq!(HOLDERS.lock().0, 1);
        assert_eq!(HOLDERS.lock().1.is_some(), cfg!(target_os = "macos"));
        drop(b);
        assert_eq!(HOLDERS.lock().0, 0);
        assert!(HOLDERS.lock().1.is_none());
    }
}
