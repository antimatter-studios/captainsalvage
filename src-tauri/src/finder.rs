//! Shows canvases in Finder, through the File Provider extension inside the app.
//!
//! `file_provider` adds and removes the Finder locations, in the app itself. The extension
//! lists folders and reads files through the media port, so nothing here gives Finder a way to
//! write to the disk being recovered. A build without signing material has no extension, and
//! Show in Finder is not offered.

use crate::AppState;
use anyhow::{bail, Context, Result};
use salvage_core::media::serial_text;
use serde::Deserialize;
use serde_json::{json, Value};

use std::process::Command;
use tauri::State;

#[derive(Deserialize)]
pub struct CanvasRevision {
    id: i64,
    name: String,
    revision: String,
}

/// Whether this copy of the app can show canvases in Finder.
fn helper() -> Option<()> {
    crate::file_provider::available().then_some(())
}

fn run(command: &str, request: Value) -> Result<Value> {
    if !crate::file_provider::available() {
        bail!("This copy of Captain Salvage was built without its Finder extension.");
    }
    crate::file_provider::run(command, &request)
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
    status(&state).map_err(|error| format!("{error:#}"))
}

pub fn status(state: &AppState) -> Result<Value> {
    if helper().is_none() {
        return Ok(json!({"available": false, "shown": []}));
    }
    let serial = serial_text(&*state.engine.session()?);
    let listed = run("list", json!({}))?;
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
    show(&state, canvas, &name, &revision).map_err(|error| format!("{error:#}"))
}

pub fn show(state: &AppState, canvas: i64, name: &str, revision: &str) -> Result<Value> {
    let mut request = port(state)?;
    request["canvas"] = json!(canvas);
    request["name"] = json!(name);
    request["revision"] = json!(revision);
    let reply = run("show", request)?;
    // A hidden test copy opens no windows on the screen of the person using the Mac.
    let hidden = std::env::var_os("SALVAGE_TEST_HIDDEN").is_some();
    if let (Some(folder), false) = (reply["folder"].as_str(), hidden) {
        let _ = Command::new("/usr/bin/open").arg(folder).status();
    }
    Ok(reply)
}

#[tauri::command(async)]
pub fn finder_hide(state: State<'_, AppState>, canvas: i64) -> Result<Value, String> {
    hide(&state, canvas).map_err(|error| format!("{error:#}"))
}

pub fn hide(state: &AppState, canvas: i64) -> Result<Value> {
    let serial = serial_text(&*state.engine.session()?);
    run("hide", json!({"serial": serial, "canvas": canvas}))
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
