//! The Ask agent tab: runs the chosen agent's command-line tool on each question and streams
//! what it says to the window, through the adapters in `salvage_core::agent`.
//!
//! Each question is one process, resumed by session id so the conversation continues. The
//! agent works only through the app's MCP tools, and the engine behind them cannot write to the
//! examined disk.

use anyhow::{bail, Result};
use salvage_core::agent::{self, AgentInfo, AgentKind, Ask, CHAT_INSTRUCTIONS};
use salvage_core::mcp::McpInfo;
use serde_json::{json, Value};
use std::process::Child;
use std::sync::{Arc, Mutex};
use tauri::Emitter;

const EVENT: &str = "chat-event";

#[derive(Default)]
pub struct ChatState {
    /// The conversation so far, and which agent it is with: a session belongs to one agent.
    session: Arc<Mutex<Option<(AgentKind, String)>>>,
    child: Arc<Mutex<Option<Child>>>,
}

fn poisoned() -> anyhow::Error {
    anyhow::anyhow!("chat lock poisoned")
}

impl ChatState {
    pub fn agents() -> Vec<AgentInfo> {
        agent::agents()
    }

    pub fn cancel(&self) {
        if let Ok(mut slot) = self.child.lock() {
            if let Some(child) = slot.as_mut() {
                let _ = child.kill();
            }
        }
    }

    pub fn reset(&self) {
        self.cancel();
        if let Ok(mut slot) = self.session.lock() {
            *slot = None;
        }
    }

    pub fn send(
        &self,
        app: tauri::AppHandle,
        info: &McpInfo,
        agent_id: &str,
        message: &str,
        selection: &Value,
    ) -> Result<()> {
        let kind = AgentKind::from_id(agent_id)?;
        {
            let mut slot = self.child.lock().map_err(|_| poisoned())?;
            if let Some(child) = slot.as_mut() {
                if child.try_wait()?.is_none() {
                    bail!("The agent is still answering. Stop it first, or wait for it to finish.");
                }
            }
            *slot = None;
        }

        let prompt = format!(
            "Currently selected in the app:\n{}\n\nQuestion:\n{message}",
            serde_json::to_string_pretty(selection)?
        );
        // A conversation with another agent cannot be continued by this one.
        let session = self
            .session
            .lock()
            .map_err(|_| poisoned())?
            .clone()
            .filter(|(with, _)| *with == kind)
            .map(|(_, id)| id);
        let work_dir = salvage_core::workspace::app_support()?
            .join("agent")
            .join(kind.id());
        let (child, output, errors) = kind.start(&Ask {
            mcp: info,
            prompt: &prompt,
            session: session.as_deref(),
            instructions: CHAT_INSTRUCTIONS,
            work_dir: &work_dir,
        })?;
        *self.child.lock().map_err(|_| poisoned())? = Some(child);

        let session_slot = self.session.clone();
        let child_slot = self.child.clone();
        std::thread::spawn(move || {
            let errors = agent::collect_errors(errors);
            kind.parser().pump(output, |event| {
                if let agent::AgentEvent::Session { id } = &event {
                    if let Ok(mut slot) = session_slot.lock() {
                        *slot = Some((kind, id.clone()));
                    }
                }
                let _ = app.emit(EVENT, &event);
            });
            let code = child_slot.lock().ok().and_then(|mut slot| {
                slot.as_mut()
                    .and_then(|child| child.wait().ok())
                    .and_then(|status| status.code())
            });
            let error_text = errors.join().unwrap_or_default();
            let _ = app.emit(
                EVENT,
                json!({"type": "end", "code": code, "stderr": error_text}),
            );
        });
        Ok(())
    }
}
