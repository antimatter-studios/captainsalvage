//! The desktop shell: exposes the engine to the window and starts the MCP server.

mod capture;
mod chat;
mod finder;
mod preview;

use chat::ChatState;
use salvage_core::engine::{Engine, AUTHOR_USER};
use salvage_core::mcp::{self, McpInfo};
use salvage_core::media::{MediaInfo, MediaServer};
use salvage_core::session::{Layer, Lens, Scope, MAX_READ_BYTES};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::ipc::Response;
use tauri::{Emitter, Manager, State};

/// Longest stream the window may load to preview a file.
const MAX_PREVIEW_BYTES: u64 = 256 * 1024 * 1024;
const LAST_DEVICE_FILE: &str = "last-device";
const THEME_FILE: &str = "theme";
const THEME_SYSTEM: &str = "system";
const LANGUAGE_FILE: &str = "language";
/// The window's languages. An unset or unknown choice follows macOS, decided by the window.
const LANGUAGES: [&str; 4] = ["en", "fr", "de", "es"];

/// Longest piece of a file the window reads at once as text.
const MAX_TEXT_READ_BYTES: u64 = 1024 * 1024;

pub(crate) struct AppState {
    pub(crate) engine: Arc<Engine>,
    thumbs: preview::ThumbQueue,
    pub(crate) media: Option<Arc<MediaServer>>,
    mcp: Option<McpInfo>,
    mcp_error: Option<String>,
}

fn config_dir() -> Option<PathBuf> {
    salvage_core::workspace::app_support().ok()
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
    let bytes = blocking(move || {
        let session = engine.session()?;
        session.note_viewing();
        session.read(Scope::Partition, offset, length)
    })
    .await?;
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

/// Whether ffmpeg is installed, and where the window's player can reach converted files.
#[tauri::command]
fn media_info(state: State<'_, AppState>) -> Option<MediaInfo> {
    state.media.as_ref().map(|media| media.info())
}

/// Connection details the window shows so other MCP clients can connect.
#[tauri::command]
fn mcp_info(state: State<'_, AppState>) -> Value {
    json!({"info": state.mcp, "error": state.mcp_error})
}

/// Shows the system's file chooser. Cancelling it gives nothing.
#[tauri::command(async)]
fn choose_disk_image() -> Option<String> {
    // macOS draws this dialog, so its prompt is chosen here, in the window's language.
    let prompt = match get_language().as_deref() {
        Some("fr") => "Choisissez une image brute d'une partition exFAT endommagée",
        Some("de") => "Wählen Sie ein Rohabbild einer beschädigten exFAT-Partition",
        Some("es") => "Elija una imagen sin procesar de una partición exFAT dañada",
        _ => "Choose a raw image of a damaged exFAT partition",
    };
    let script = format!("POSIX path of (choose file with prompt \"{prompt}\")");
    let output = std::process::Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !path.is_empty()).then_some(path)
}

#[tauri::command]
fn last_device() -> Option<String> {
    let text = std::fs::read_to_string(config_dir()?.join(LAST_DEVICE_FILE)).ok()?;
    Some(text.trim().to_string()).filter(|path| !path.is_empty())
}

/// `None` follows macOS. The window's appearance drives the sidebar's translucency, the
/// page's colours and native controls together, so they can never disagree.
fn window_theme(name: &str) -> Option<tauri::Theme> {
    match name {
        "light" => Some(tauri::Theme::Light),
        "dark" => Some(tauri::Theme::Dark),
        _ => None,
    }
}

#[tauri::command]
fn get_theme() -> String {
    config_dir()
        .and_then(|dir| std::fs::read_to_string(dir.join(THEME_FILE)).ok())
        .map(|text| text.trim().to_string())
        .filter(|name| window_theme(name).is_some())
        .unwrap_or_else(|| THEME_SYSTEM.to_string())
}

/// The language chosen in the window, or nothing when it should follow macOS.
#[tauri::command]
fn get_language() -> Option<String> {
    config_dir()
        .and_then(|dir| std::fs::read_to_string(dir.join(LANGUAGE_FILE)).ok())
        .map(|text| text.trim().to_string())
        .filter(|name| LANGUAGES.contains(&name.as_str()))
}

#[tauri::command]
fn set_language(language: String) -> Result<(), String> {
    if !LANGUAGES.contains(&language.as_str()) {
        return Err(format!("unknown language {language}"));
    }
    if let Some(dir) = config_dir() {
        std::fs::write(dir.join(LANGUAGE_FILE), &language).map_err(text_error)?;
    }
    Ok(())
}

#[tauri::command]
fn set_theme(app: tauri::AppHandle, theme: String) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("main") {
        window.set_theme(window_theme(&theme)).map_err(text_error)?;
    }
    if let Some(dir) = config_dir() {
        std::fs::write(dir.join(THEME_FILE), &theme).map_err(text_error)?;
    }
    Ok(())
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
            salvage_core::mount::keep_disks_awake();
            // From here until the app quits, macOS cannot mount the volumes the person blocked.
            if let Ok(folder) = salvage_core::workspace::app_support() {
                if let Err(error) = salvage_core::mountblock::start(&folder) {
                    eprintln!("mount blocking did not start: {error:#}");
                }
            }
            let engine = Engine::new();
            let handle = app.handle().clone();
            let allow_test_scripts = std::env::var_os("SALVAGE_TEST_UI").is_some();
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
                let _ = handle.emit(&format!("salvage-{event}"), payload);
            }));
            let (info, error) = match mcp::start(engine.clone()) {
                Ok(info) => (Some(info), None),
                Err(error) => (None, Some(format!("{error:#}"))),
            };
            // Lets a test instance run beside a window the user is working in.
            if std::env::var_os("SALVAGE_TEST_HIDDEN").is_some() {
                // Off screen, not hidden: a hidden web view cannot take its own picture.
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.set_position(tauri::LogicalPosition::new(-6000.0, 40.0));
                    if std::env::var_os("SALVAGE_TEST_DARK").is_some() {
                        let _ = window.set_theme(Some(tauri::Theme::Dark));
                    }
                }
            }
            if let Some(window) = app.get_webview_window("main") {
                let saved = window_theme(&get_theme());
                if saved.is_some() && std::env::var_os("SALVAGE_TEST_DARK").is_none() {
                    let _ = window.set_theme(saved);
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
            let media = MediaServer::start(engine.clone()).ok();
            if let Some(media) = &media {
                engine.set_media(media.clone());
            }
            app.manage(AppState {
                thumbs: preview::ThumbQueue::start(engine.clone(), media.clone()),
                media,
                engine,
                mcp: info,
                mcp_error: error,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            choose_disk_image,
            engine_call,
            map_layer,
            read_raw,
            read_entry_bytes,
            read_entry_range,
            media_info,
            mcp_info,
            last_device,
            get_theme,
            set_theme,
            get_language,
            set_language,
            chat_send,
            chat_cancel,
            chat_reset,
            finder::finder_status,
            finder::finder_show,
            finder::finder_hide,
            finder::finder_sync,
        ])
        .build(tauri::generate_context!())
        .expect("the app failed to start")
        .run(|_, event| {
            if let tauri::RunEvent::Exit = event {
                finder::go_offline();
            }
        });
}
