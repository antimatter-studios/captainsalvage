//! The desktop shell: exposes the engine to the window and starts the MCP server.

mod capture;
mod chat;
mod preview;

use chat::ChatState;
use lens_core::engine::{Engine, AUTHOR_USER};
use lens_core::mcp::{self, McpInfo};
use lens_core::session::{Layer, Lens, Scope, MAX_READ_BYTES};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::ipc::Response;
use tauri::{Emitter, Manager, State};

/// Longest stream the window may load to preview a file.
const MAX_PREVIEW_BYTES: u64 = 256 * 1024 * 1024;
const LAST_DEVICE_FILE: &str = "last-device";

/// Longest piece of a file the window reads at once as text.
const MAX_TEXT_READ_BYTES: u64 = 1024 * 1024;

struct AppState {
    engine: Arc<Engine>,
    thumbs: preview::ThumbQueue,
    mcp: Option<McpInfo>,
    mcp_error: Option<String>,
}

fn config_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support/exfat-lens"))
}

fn text_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(text_error)?
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
async fn engine_call(
    state: State<'_, AppState>,
    tool: String,
    args: Value,
) -> Result<Value, String> {
    let engine = state.engine.clone();
    blocking(move || {
        let result = engine.call(&tool, &args, AUTHOR_USER)?;
        if tool == "open_volume" {
            if let (Some(dir), Some(path)) =
                (config_dir(), args.get("path").and_then(Value::as_str))
            {
                let _ = std::fs::write(dir.join(LAST_DEVICE_FILE), path);
            }
        }
        Ok(result)
    })
    .await
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn map_layer(
    state: State<'_, AppState>,
    layer: Layer,
    start: u32,
    end: u32,
    buckets: usize,
    rows: usize,
    floor: f64,
    lens: Option<Lens>,
) -> Result<Response, String> {
    let engine = state.engine.clone();
    let bytes = blocking(move || {
        let session = engine.session()?;
        Ok(session.map_raster(layer, start, end, buckets, rows, floor, lens.as_ref()))
    })
    .await?;
    Ok(Response::new(bytes))
}

#[tauri::command]
async fn read_raw(
    state: State<'_, AppState>,
    offset: u64,
    length: u64,
) -> Result<Response, String> {
    if length > MAX_READ_BYTES {
        return Err(format!("read of {length} bytes is over the limit"));
    }
    let engine = state.engine.clone();
    let bytes = blocking(move || engine.session()?.read(Scope::Partition, offset, length)).await?;
    Ok(Response::new(bytes))
}

#[tauri::command]
async fn read_entry_bytes(state: State<'_, AppState>, id: i64) -> Result<Response, String> {
    let engine = state.engine.clone();
    let bytes =
        blocking(move || Ok(engine.session()?.read_entry(id, MAX_PREVIEW_BYTES)?.1)).await?;
    Ok(Response::new(bytes))
}

#[tauri::command]
async fn read_entry_range(
    state: State<'_, AppState>,
    id: i64,
    start: u64,
    length: u64,
) -> Result<Response, String> {
    let engine = state.engine.clone();
    let part = blocking(move || {
        Ok(engine
            .session()?
            .read_entry_range(id, start, length.min(MAX_TEXT_READ_BYTES))?
            .data)
    })
    .await?;
    Ok(Response::new(part))
}

/// Connection details the window shows so other MCP clients can connect.
#[tauri::command]
fn mcp_info(state: State<'_, AppState>) -> Value {
    json!({"info": state.mcp, "error": state.mcp_error})
}

#[tauri::command]
fn last_device() -> Option<String> {
    let text = std::fs::read_to_string(config_dir()?.join(LAST_DEVICE_FILE)).ok()?;
    Some(text.trim().to_string()).filter(|path| !path.is_empty())
}

#[tauri::command]
fn chat_send(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    chat: State<'_, ChatState>,
    message: String,
    selection: Value,
) -> Result<(), String> {
    let info = state
        .mcp
        .clone()
        .ok_or("the MCP server is not running, so Claude has no tools")?;
    chat.send(app, &info, &message, &selection)
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
fn chat_cancel(chat: State<'_, ChatState>) {
    chat.cancel();
}

#[tauri::command]
fn chat_reset(chat: State<'_, ChatState>) {
    chat.reset();
}

pub fn run() {
    tauri::Builder::default()
        .manage(ChatState::default())
        .register_asynchronous_uri_scheme_protocol("lens", |context, request, responder| {
            let app = context.app_handle();
            match app.try_state::<AppState>() {
                Some(state) => preview::handle(
                    state.engine.clone(),
                    state.thumbs.clone(),
                    request,
                    responder,
                ),
                None => responder.respond(
                    tauri::http::Response::builder()
                        .status(tauri::http::StatusCode::SERVICE_UNAVAILABLE)
                        .body(Vec::new())
                        .unwrap_or_default(),
                ),
            }
        })
        .setup(|app| {
            let engine = Engine::new();
            let handle = app.handle().clone();
            let allow_test_scripts = std::env::var_os("LENS_TEST_UI").is_some();
            engine.set_ui_sink(Arc::new(move |event, payload| {
                if event == "test-eval" {
                    // Inert unless this launch explicitly enabled it, for testing the window.
                    if let (true, Some(window), Some(code)) = (
                        allow_test_scripts,
                        handle.get_webview_window("main"),
                        payload.get("code").and_then(Value::as_str),
                    ) {
                        let _ = window.eval(code);
                    }
                    return;
                }
                let _ = handle.emit(&format!("lens-{event}"), payload);
            }));
            let (info, error) = match mcp::start(engine.clone()) {
                Ok(info) => (Some(info), None),
                Err(error) => (None, Some(format!("{error:#}"))),
            };
            // Lets a test instance run beside a window the user is working in.
            if std::env::var_os("LENS_TEST_HIDDEN").is_some() {
                // Off screen, not hidden: a hidden web view cannot take its own picture.
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.set_position(tauri::LogicalPosition::new(-6000.0, 40.0));
                    if std::env::var_os("LENS_TEST_DARK").is_some() {
                        let _ = window.set_theme(Some(tauri::Theme::Dark));
                    }
                }
            }
            let capture_handle = app.handle().clone();
            engine.set_screenshot_hook(Arc::new(move || capture::capture_window(&capture_handle)));

            // MCP clients must not depend on the window having loaded.
            let opener = engine.clone();
            std::thread::spawn(move || {
                if let Some(path) = last_device() {
                    let _ = opener.open(&path);
                }
            });
            app.manage(AppState {
                thumbs: preview::ThumbQueue::start(engine.clone()),
                engine,
                mcp: info,
                mcp_error: error,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            engine_call,
            map_layer,
            read_raw,
            read_entry_bytes,
            read_entry_range,
            mcp_info,
            last_device,
            chat_send,
            chat_cancel,
            chat_reset,
        ])
        .run(tauri::generate_context!())
        .expect("the app failed to start");
}
