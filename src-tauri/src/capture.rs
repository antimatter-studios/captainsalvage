//! Captures the app's own window through WebKit, for the screenshot_app tool.
//!
//! Asking the web view for its contents needs no Screen Recording permission,
//! and shows only this app.
//!
//! WebKit stops drawing a window that has been idle for a while and is not in front, and a
//! snapshot then waits for a drawing that never comes. Seen on a window covered by others,
//! and on an off-screen test window idle for 30 seconds, while page script still ran. So each
//! attempt first nudges the page into drawing again, and an attempt that gets no answer is
//! tried again after another nudge.

use anyhow::{anyhow, bail, Context, Result};
use block2::RcBlock;
use objc2::msg_send;
use objc2::runtime::{AnyObject, Bool};
use objc2_foundation::NSString;
use std::process::Command;
use std::ptr::null_mut;
use std::sync::mpsc;
use std::time::Duration;
use tauri::Manager;

/// How long one attempt waits for WebKit's picture, and how many attempts are made.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);
const ATTEMPTS: usize = 3;
/// Changes something on the page and asks for the next frame, so WebKit draws again.
const NUDGE_SCRIPT: &str = "document.documentElement.dataset.captureNudge = String(Date.now()); requestAnimationFrame(() => { delete document.documentElement.dataset.captureNudge; });";
/// Time for the nudged frame to be drawn before the snapshot is asked for.
const NUDGE_SETTLE: Duration = Duration::from_millis(150);

pub fn capture_window(app: &tauri::AppHandle) -> Result<Vec<u8>> {
    let window = app
        .get_webview_window("main")
        .context("the app window is not open")?;
    let directory = std::env::temp_dir();
    let tiff = directory.join(format!(
        "captain-salvage-capture-{}.tiff",
        std::process::id()
    ));
    let png = directory.join(format!(
        "captain-salvage-capture-{}.png",
        std::process::id()
    ));
    let mut taken = false;
    for _ in 0..ATTEMPTS {
        let _ = window.eval(NUDGE_SCRIPT);
        std::thread::sleep(NUDGE_SETTLE);
        match snapshot(&window, &tiff)? {
            Some(true) => {
                taken = true;
                break;
            }
            Some(false) => bail!("the window returned no picture"),
            None => continue,
        }
    }
    if !taken {
        bail!(
            "the window did not draw a picture in {} tries of {} seconds. WebKit may have stopped drawing it: is it minimised, or on a screen that is asleep?",
            ATTEMPTS,
            ATTEMPT_TIMEOUT.as_secs()
        );
    }
    let converted = Command::new("/usr/bin/sips")
        .args(["-s", "format", "png"])
        .arg(&tiff)
        .arg("--out")
        .arg(&png)
        .output()
        .context("running sips")?;
    if !converted.status.success() {
        bail!(
            "sips failed: {}",
            String::from_utf8_lossy(&converted.stderr)
        );
    }
    let bytes = std::fs::read(&png)?;
    let _ = std::fs::remove_file(&tiff);
    let _ = std::fs::remove_file(&png);
    Ok(bytes)
}

/// Asks WebKit for one picture of the window, saved as TIFF. None when it did not answer.
fn snapshot(window: &tauri::WebviewWindow, tiff: &std::path::Path) -> Result<Option<bool>> {
    let (sender, receiver) = mpsc::channel::<bool>();
    let tiff_text = tiff.display().to_string();
    window
        .with_webview(move |webview| {
            let view = webview.inner() as *mut AnyObject;
            let handler = RcBlock::new(move |image: *mut AnyObject, _error: *mut AnyObject| {
                if image.is_null() {
                    let _ = sender.send(false);
                    return;
                }
                // SAFETY: WebKit passes a valid NSImage. TIFFRepresentation returns
                // NSData, and both messages are part of their public interfaces.
                let saved = unsafe {
                    let data: *mut AnyObject = msg_send![image, TIFFRepresentation];
                    let target = NSString::from_str(&tiff_text);
                    let written: Bool = msg_send![data, writeToFile: &*target, atomically: true];
                    written.as_bool()
                };
                let _ = sender.send(saved);
            });
            let configuration: *mut AnyObject = null_mut();
            // SAFETY: on macOS this handle is the window's WKWebView, this closure
            // runs on the main thread as WebKit requires, and WebKit copies the block.
            unsafe {
                let _: () = msg_send![view, takeSnapshotWithConfiguration: configuration, completionHandler: &*handler];
            }
        })
        .map_err(|error| anyhow!("cannot reach the web view: {error}"))?;
    Ok(receiver.recv_timeout(ATTEMPT_TIMEOUT).ok())
}
