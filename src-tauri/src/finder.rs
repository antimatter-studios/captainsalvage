//! Shows canvases in Finder, through the Captain Salvage Finder helper app.
//!
//! macOS lets only the app that holds a File Provider extension add its Finder locations, and
//! the extension must be signed into an app group, so a small signed helper does that. This
//! runs it with a command and a JSON request, and reads back one line of JSON. The helper
//! never reads the disk: the extension lists folders and reads files through the media port,
//! so nothing here gives Finder a way to write to the disk being recovered.

use crate::AppState;
use anyhow::{bail, Context, Result};
use lens_core::media::serial_text;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use tauri::State;

const HELPER_APP: &str = "Captain Salvage Finder.app";
const HELPER_BINARY: &str = "Contents/MacOS/captain-salvage-finder";

#[derive(Deserialize)]
pub struct CanvasRevision {
    id: i64,
    name: String,
    revision: String,
}

/// The helper, if it is installed: named by LENS_FINDER_HELPER, or in an Applications folder.
fn helper() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("LENS_FINDER_HELPER") {
        return Some(PathBuf::from(path)).filter(|path| path.is_file());
    }
    let mut places = vec![PathBuf::from("/Applications")];
    if let Some(home) = std::env::var_os("HOME") {
        places.insert(0, PathBuf::from(home).join("Applications"));
    }
    places
        .into_iter()
        .map(|place| place.join(HELPER_APP).join(HELPER_BINARY))
        .find(|path| path.is_file())
}

fn run(command: &str, request: Value) -> Result<Value> {
    let path = helper().context("The Captain Salvage Finder helper is not installed.")?;
    let mut child = Command::new(path)
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("starting the Finder helper")?;
    // The port's token goes over a pipe, never on a command line other processes can see.
    child
        .stdin
        .take()
        .context("the Finder helper has no input")?
        .write_all(request.to_string().as_bytes())?;
    let output = child.wait_with_output()?;
    let reply: Value = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("the Finder helper gave no answer to {command}"))?;
    if let Some(error) = reply.get("error").and_then(Value::as_str) {
        bail!("{error}");
    }
    Ok(reply)
}

/// The port and the open disk, which every location of this disk is told about.
fn port(state: &AppState) -> Result<Value> {
    let media = state
        .media
        .as_ref()
        .context("The media port did not start, so Finder cannot read files.")?
        .info();
    let session = state.engine.session()?;
    Ok(json!({"base": media.base_url, "token": media.token, "serial": serial_text(&session)}))
}

/// Whether the helper is installed, and which of this disk's canvases are in Finder.
#[tauri::command(async)]
pub fn finder_status(state: State<'_, AppState>) -> Result<Value, String> {
    if helper().is_none() {
        return Ok(json!({"available": false, "shown": []}));
    }
    let serial = state
        .engine
        .session()
        .map(|session| serial_text(&session))
        .map_err(|error| format!("{error:#}"))?;
    let listed = run("list", json!({})).map_err(|error| format!("{error:#}"))?;
    let prefix = format!("{serial}-");
    let shown: Vec<i64> = listed["domains"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|domain| domain["id"].as_str()?.strip_prefix(&prefix)?.parse().ok())
        .collect();
    Ok(json!({"available": true, "shown": shown}))
}

/// Adds a canvas to Finder, or finds it there, and opens its folder in Finder.
#[tauri::command(async)]
pub fn finder_show(
    state: State<'_, AppState>,
    canvas: i64,
    name: String,
    revision: String,
) -> Result<Value, String> {
    let work = || -> Result<Value> {
        let mut request = port(&state)?;
        request["canvas"] = json!(canvas);
        request["name"] = json!(name);
        request["revision"] = json!(revision);
        let reply = run("show", request)?;
        // A hidden test copy opens no windows on the screen of the person using the Mac.
        let hidden = std::env::var_os("LENS_TEST_HIDDEN").is_some();
        if let (Some(folder), false) = (reply["folder"].as_str(), hidden) {
            let _ = Command::new("/usr/bin/open").arg(folder).status();
        }
        Ok(reply)
    };
    work().map_err(|error| format!("{error:#}"))
}

#[tauri::command(async)]
pub fn finder_hide(state: State<'_, AppState>, canvas: i64) -> Result<Value, String> {
    let work = || -> Result<Value> {
        let serial = serial_text(&*state.engine.session()?);
        run("hide", json!({"serial": serial, "canvas": canvas}))
    };
    work().map_err(|error| format!("{error:#}"))
}

/// Tells this disk's Finder locations about the canvases as they are now. A location whose
/// canvas was deleted goes, and other disks' locations show as unreachable.
#[tauri::command(async)]
pub fn finder_sync(
    state: State<'_, AppState>,
    canvases: Vec<CanvasRevision>,
) -> Result<Value, String> {
    if helper().is_none() {
        return Ok(json!({"shown": []}));
    }
    let work = || -> Result<Value> {
        let mut request = port(&state)?;
        request["canvases"] = canvases
            .iter()
            .map(
                |canvas| json!({"id": canvas.id, "name": canvas.name, "revision": canvas.revision}),
            )
            .collect();
        run("sync", request)
    };
    work().map_err(|error| format!("{error:#}"))
}

/// When the app quits, the port's token stops working, so Finder is told the files are away.
pub fn go_offline() {
    if helper().is_some() {
        let _ = run("offline", json!({}));
    }
}
