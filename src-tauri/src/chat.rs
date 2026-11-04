//! The chat sidebar: runs `claude -p` and streams its events to the window.
//!
//! Each question is one process, resumed by session id so the conversation
//! continues. Claude gets the app's MCP tools and no built-in tools, so it has
//! no shell or file access and works only through the engine, which cannot write
//! to the examined disk.

use anyhow::{bail, Context, Result};
use lens_core::mcp::{McpInfo, INSTRUCTIONS, SERVER_NAME};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use tauri::Emitter;

const EVENT: &str = "chat-event";
const OWNER_ONLY: u32 = 0o600;
const SEARCH_PATHS: &[&str] = &[".local/bin/claude", ".claude/local/claude"];
const SYSTEM_PATHS: &[&str] = &["/opt/homebrew/bin/claude", "/usr/local/bin/claude"];
/// Variables a parent Claude Code session sets, which would confuse a child run.
const INHERITED_PREFIXES: &[&str] = &["CLAUDE_CODE_", "CLAUDE_AGENT_"];
const INHERITED_NAMES: &[&str] = &["CLAUDECODE", "CLAUDE_PID", "CLAUDE_EFFORT"];

const CHAT_INSTRUCTIONS: &str = "\
You are the chat sidebar in exFAT Lens, talking to the owner of the disk. They can see the same map, tree and hex view \
you act on. Each message starts with what they have selected, so start there and use the tools to investigate rather \
than asking them for details you can read yourself. Keep answers short and concrete: cluster numbers, offsets, counts. \
Say plainly what you measured and what you are guessing. Use show_in_app to take them to what you found.";

#[derive(Default)]
pub struct ChatState {
    session_id: Arc<Mutex<Option<String>>>,
    child: Arc<Mutex<Option<Child>>>,
}

fn find_claude() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let candidates = SEARCH_PATHS
        .iter()
        .filter_map(|relative| home.as_ref().map(|home| home.join(relative)))
        .chain(SYSTEM_PATHS.iter().map(PathBuf::from))
        .chain(
            std::env::var_os("PATH")
                .into_iter()
                .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
                .map(|dir| dir.join("claude")),
        );
    for candidate in candidates {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("the claude command was not found. Install Claude Code, then try again.")
}

fn write_client_config(info: &McpInfo) -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let path = PathBuf::from(home).join("Library/Application Support/exfat-lens/mcp-client.json");
    let config = json!({
        "mcpServers": {
            SERVER_NAME: {
                "type": "http",
                "url": info.url,
                "headers": {"Authorization": format!("Bearer {}", info.token)},
            },
        },
    });
    std::fs::write(&path, config.to_string())?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(OWNER_ONLY))?;
    Ok(path)
}

impl ChatState {
    pub fn cancel(&self) {
        if let Ok(mut slot) = self.child.lock() {
            if let Some(child) = slot.as_mut() {
                let _ = child.kill();
            }
        }
    }

    pub fn reset(&self) {
        self.cancel();
        if let Ok(mut slot) = self.session_id.lock() {
            *slot = None;
        }
    }

    pub fn send(&self, app: tauri::AppHandle, info: &McpInfo, message: &str, selection: &Value) -> Result<()> {
        {
            let mut slot = self.child.lock().map_err(|_| anyhow::anyhow!("chat lock poisoned"))?;
            if let Some(child) = slot.as_mut() {
                if child.try_wait()?.is_none() {
                    bail!("Claude is still answering. Stop it first, or wait for it to finish.");
                }
            }
            *slot = None;
        }

        let prompt = format!(
            "Currently selected in the app:\n{}\n\nQuestion:\n{message}",
            serde_json::to_string_pretty(selection)?
        );
        let config = write_client_config(info)?;
        let working_dir = config.parent().context("config has no parent directory")?.to_path_buf();
        let mut command = Command::new(find_claude()?);
        command
            .arg("-p")
            .arg(&prompt)
            .args(["--output-format", "stream-json", "--verbose", "--include-partial-messages"])
            .arg("--strict-mcp-config")
            .arg("--mcp-config")
            .arg(&config)
            .args(["--tools", ""])
            .arg("--allowedTools")
            .arg(format!("mcp__{SERVER_NAME}"))
            .arg("--append-system-prompt")
            .arg(format!("{INSTRUCTIONS}\n\n{CHAT_INSTRUCTIONS}"))
            .current_dir(working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(id) = self.session_id.lock().map_err(|_| anyhow::anyhow!("chat lock poisoned"))?.as_ref() {
            command.arg("--resume").arg(id);
        }
        for (name, _) in std::env::vars_os() {
            let text = name.to_string_lossy();
            if INHERITED_NAMES.contains(&text.as_ref()) || INHERITED_PREFIXES.iter().any(|prefix| text.starts_with(prefix)) {
                command.env_remove(&name);
            }
        }

        let mut child = command.spawn().context("starting claude")?;
        let stdout = child.stdout.take().context("claude has no stdout")?;
        let stderr = child.stderr.take().context("claude has no stderr")?;
        *self.child.lock().map_err(|_| anyhow::anyhow!("chat lock poisoned"))? = Some(child);

        let session_id = self.session_id.clone();
        let child_slot = self.child.clone();
        std::thread::spawn(move || {
            let error_reader = std::thread::spawn(move || {
                let mut text = String::new();
                let _ = BufReader::new(stderr).read_to_string(&mut text);
                text
            });
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let Ok(event) = serde_json::from_str::<Value>(&line) else { continue };
                if let Some(id) = event.get("session_id").and_then(Value::as_str) {
                    if let Ok(mut slot) = session_id.lock() {
                        *slot = Some(id.to_string());
                    }
                }
                let _ = app.emit(EVENT, event);
            }
            let code = child_slot.lock().ok().and_then(|mut slot| {
                slot.as_mut().and_then(|child| child.wait().ok()).and_then(|status| status.code())
            });
            let error_text = error_reader.join().unwrap_or_default();
            let _ = app.emit(EVENT, json!({"type": "chat-end", "code": code, "stderr": error_text}));
        });
        Ok(())
    }
}
