//! Keeps App Nap from putting the app to sleep while it is open.
//!
//! macOS naps an app whose window is out of sight. A napped Captain Salvage stopped answering:
//! on an idle test copy with App Nap on, a quit request timed out after 121 seconds where it took
//! 2 seconds with App Nap off, and a tool call from an agent never reached the app's port in 60
//! seconds. Agents use the app while the person looks elsewhere, and a scan or a copy must not
//! slow down, so the app holds one activity for as long as it runs. The Mac can still sleep.

use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
use std::sync::OnceLock;

/// Holding the token is what keeps the activity going.
struct Activity {
    _token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

// SAFETY: the activity token is an opaque object that NSProcessInfo accepts from any thread,
// and it is only kept here, never used, until the process ends.
unsafe impl Send for Activity {}
unsafe impl Sync for Activity {}

static ACTIVITY: OnceLock<Activity> = OnceLock::new();

/// Starts the activity once. It ends when the process does.
pub fn keep_awake_while_open() {
    ACTIVITY.get_or_init(|| {
        let reason = NSString::from_str("Reading a damaged disk and answering agents");
        Activity {
            _token: NSProcessInfo::processInfo().beginActivityWithOptions_reason(
                NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
                &reason,
            ),
        }
    });
}
