//! The window's own tools, for MCP clients: Finder, language and appearance. The engine hands
//! these to the app, and each one tells the window, so it shows the change too.

use crate::{
    finder, get_language, get_theme, set_language, set_theme, window_theme, AppState, THEME_SYSTEM,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tauri::{Emitter, Manager};

fn text(args: &Value, name: &str) -> Result<String> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("{name} is needed"))
}

pub fn app_tool(app: &tauri::AppHandle, tool: &str, args: &Value) -> Result<Value> {
    match tool {
        "app_settings" => Ok(json!({"language": get_language(), "theme": get_theme()})),
        "set_language" => {
            let language = text(args, "language")?;
            set_language(language.clone()).map_err(anyhow::Error::msg)?;
            let _ = app.emit("salvage-language", json!({"language": language}));
            Ok(json!({"language": language}))
        }
        "set_theme" => {
            let theme = text(args, "theme")?;
            if window_theme(&theme).is_none() && theme != THEME_SYSTEM {
                bail!("theme must be system, light or dark");
            }
            set_theme(app.clone(), theme.clone()).map_err(anyhow::Error::msg)?;
            let _ = app.emit("salvage-theme", json!({"theme": theme}));
            Ok(json!({"theme": theme}))
        }
        "finder_status" | "finder_show" | "finder_hide" => {
            let state = app.state::<AppState>();
            let reply = match tool {
                "finder_status" => finder::status(&state)?,
                "finder_hide" => finder::hide(&state, canvas(args)?)?,
                _ => {
                    let id = canvas(args)?;
                    let listed = state.engine.call(
                        "canvas_list",
                        &json!({}),
                        salvage_core::engine::AUTHOR_USER,
                    )?;
                    let found = listed
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|item| item["id"].as_i64() == Some(id))
                        .with_context(|| format!("there is no canvas {id}"))?;
                    let name = found["name"].as_str().unwrap_or_default();
                    finder::show(&state, id, name, &found["updatedAtMillis"].to_string())?
                }
            };
            let _ = app.emit("salvage-finder-changed", json!({}));
            Ok(reply)
        }
        other => bail!("unknown app tool {other}"),
    }
}

fn canvas(args: &Value) -> Result<i64> {
    args.get("canvas")
        .and_then(Value::as_i64)
        .context("canvas is needed")
}
