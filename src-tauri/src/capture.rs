//! Captures the app's own window through WebKit, for the screenshot_app tool.
//!
//! Asking the web view for its contents needs no Screen Recording permission,
//! and shows only this app.

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

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);

pub fn capture_window(app: &tauri::AppHandle) -> Result<Vec<u8>> {
    let window = app
        .get_webview_window("main")
        .context("the app window is not open")?;
    let directory = std::env::temp_dir();
    let tiff = directory.join(format!("exfat-lens-capture-{}.tiff", std::process::id()));
    let png = directory.join(format!("exfat-lens-capture-{}.png", std::process::id()));
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

    if !receiver
        .recv_timeout(CAPTURE_TIMEOUT)
        .context("the window did not return a picture")?
    {
        bail!("the window returned no picture");
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
