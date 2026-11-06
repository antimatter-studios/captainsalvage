//! Asks a coding agent's command-line tool a question, with the app's MCP tools, and reads its
//! answer back as a few plain events, whichever agent it is.
//!
//! Each agent is an adapter: how to find its command, how to start it, and how to read what it
//! prints. The window's Ask agent tab and the `salvage-agent` command both use these, so an
//! agent that works in one works in the other.
//!
//! Neither agent can write to the disk being examined. The engine behind the MCP tools has no
//! write path. Claude Code is given no built-in tools at all. Codex keeps its shell, but in its
//! read-only sandbox, and a raw disk can only be read by root anyway.

use crate::mcp::{McpInfo, INSTRUCTIONS, SERVER_NAME};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};

/// How the window wants the agent to behave, on top of the engine's own instructions.
pub const CHAT_INSTRUCTIONS: &str = "\
You are the chat sidebar in Captain Salvage, talking to the owner of the disk. They can see the same map, tree and hex view \
you act on. Each message starts with what they have selected, so start there and use the tools to investigate rather \
than asking them for details you can read yourself. Keep answers short and concrete: cluster numbers, offsets, counts. \
Say plainly what you measured and what you are guessing. Use show_in_app to take them to what you found.";

/// The variable Codex reads the MCP server's token from, so it is never on a command line.
const TOKEN_VARIABLE: &str = "SALVAGE_MCP_TOKEN";
const CLAUDE_TOOL_PREFIX: &str = "mcp__captain-salvage__";

/// The agents there are adapters for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentKind {
    Claude,
    Codex,
}

pub const AGENTS: [AgentKind; 2] = [AgentKind::Claude, AgentKind::Codex];

/// One agent as the window lists it.
#[derive(Serialize)]
pub struct AgentInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub installed: bool,
}

/// What an agent said or did, in the same shape whichever agent it was.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AgentEvent {
    /// The conversation's id, to continue it with the next question.
    Session { id: String },
    /// More of the answer, to add to what was already said.
    Text { text: String },
    /// The agent used a tool.
    Tool { name: String, input: String },
    /// The agent reported a failure.
    Error { text: String },
}

/// One question.
pub struct Ask<'a> {
    pub mcp: &'a McpInfo,
    pub prompt: &'a str,
    /// The conversation to continue, from an earlier Session event of the same agent.
    pub session: Option<&'a str>,
    /// Added to the engine's instructions.
    pub instructions: &'a str,
    /// An empty folder the agent runs in. Claude Code also finds its MCP settings file here.
    pub work_dir: &'a Path,
}

impl AgentKind {
    pub fn id(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            AgentKind::Claude => "Claude Code",
            AgentKind::Codex => "Codex",
        }
    }

    pub fn from_id(id: &str) -> Result<AgentKind> {
        AGENTS
            .into_iter()
            .find(|agent| agent.id() == id)
            .with_context(|| format!("there is no agent called {id}"))
    }

    /// Where the command may be, besides PATH. An app opened from Finder has a short PATH.
    fn places(self) -> (&'static [&'static str], &'static [&'static str]) {
        match self {
            AgentKind::Claude => (
                &[".local/bin/claude", ".claude/local/claude"],
                &["/opt/homebrew/bin/claude", "/usr/local/bin/claude"],
            ),
            AgentKind::Codex => (
                &[".local/bin/codex"],
                &[
                    "/opt/homebrew/bin/codex",
                    "/usr/local/bin/codex",
                    "/Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex",
                ],
            ),
        }
    }

    /// The agent's command. SALVAGE_AGENT_<ID> names another one, which tests use.
    pub fn find(self) -> Result<PathBuf> {
        let variable = format!("SALVAGE_AGENT_{}", self.id().to_uppercase());
        if let Some(path) = std::env::var_os(variable) {
            return Ok(PathBuf::from(path));
        }
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let (in_home, on_system) = self.places();
        let on_path = std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
            .unwrap_or_default();
        in_home
            .iter()
            .filter_map(|relative| home.as_ref().map(|home| home.join(relative)))
            .chain(on_system.iter().map(PathBuf::from))
            .chain(on_path.into_iter().map(|dir| dir.join(self.id())))
            .find(|candidate| candidate.is_file())
            .with_context(|| match self {
                AgentKind::Claude => {
                    "the claude command was not found. Install Claude Code, then try again."
                }
                AgentKind::Codex => {
                    "the codex command was not found. Install Codex, then try again."
                }
            })
    }

    /// The command for one question. The MCP token goes in a private file for Claude Code and
    /// in the environment for Codex, never in the arguments, which other processes can read.
    pub fn command(self, program: &Path, ask: &Ask) -> Result<Command> {
        let instructions = format!("{INSTRUCTIONS}\n\n{}", ask.instructions);
        let mut command = Command::new(program);
        command.current_dir(ask.work_dir);
        match self {
            AgentKind::Claude => {
                let config = write_claude_config(ask.mcp, ask.work_dir)?;
                command
                    .arg("-p")
                    .arg(ask.prompt)
                    .args(["--output-format", "stream-json", "--verbose"])
                    .arg("--include-partial-messages")
                    .arg("--strict-mcp-config")
                    .arg("--mcp-config")
                    .arg(config)
                    // No built-in tools: no shell and no files, only the engine's tools.
                    .args(["--tools", ""])
                    .arg("--allowedTools")
                    .arg(format!("mcp__{SERVER_NAME}"))
                    .arg("--append-system-prompt")
                    .arg(instructions);
                if let Some(session) = ask.session {
                    command.arg("--resume").arg(session);
                }
            }
            AgentKind::Codex => {
                let server = format!(
                    "mcp_servers={{\"{SERVER_NAME}\"={{url={},bearer_token_env_var=\"{TOKEN_VARIABLE}\",default_tools_approval_mode=\"approve\"}}}}",
                    toml_string(&ask.mcp.url)
                );
                command.arg("exec");
                if ask.session.is_some() {
                    command.arg("resume");
                }
                command
                    .args(["--json", "--skip-git-repo-check"])
                    // Only this server: the person's own Codex settings may name others.
                    .args(["--ignore-user-config", "--ignore-rules"])
                    .args(["-c", "sandbox_mode=\"read-only\""])
                    .args(["-c", "approval_policy=\"never\""])
                    .arg("-c")
                    .arg(server)
                    .arg("-c")
                    .arg(format!(
                        "developer_instructions={}",
                        toml_string(&instructions)
                    ));
                if let Some(session) = ask.session {
                    command.arg(session);
                }
                command.arg(ask.prompt).env(TOKEN_VARIABLE, &ask.mcp.token);
            }
        }
        // A parent agent session's variables would confuse the child.
        for (name, _) in std::env::vars_os() {
            if inherited(&name.to_string_lossy()) {
                command.env_remove(&name);
            }
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(command)
    }

    /// Starts the agent on one question.
    pub fn start(self, ask: &Ask) -> Result<(Child, ChildStdout, ChildStderr)> {
        std::fs::create_dir_all(ask.work_dir)?;
        let mut child = self
            .command(&self.find()?, ask)?
            .spawn()
            .with_context(|| format!("starting {}", self.id()))?;
        let stdout = child.stdout.take().context("the agent has no output")?;
        let stderr = child
            .stderr
            .take()
            .context("the agent has no error output")?;
        Ok((child, stdout, stderr))
    }

    pub fn parser(self) -> Parser {
        Parser {
            agent: self,
            said: false,
        }
    }
}

/// Every agent, and whether its command is on this Mac.
pub fn agents() -> Vec<AgentInfo> {
    AGENTS
        .into_iter()
        .map(|agent| AgentInfo {
            id: agent.id(),
            name: agent.name(),
            installed: agent.find().is_ok(),
        })
        .collect()
}

fn inherited(name: &str) -> bool {
    const NAMES: &[&str] = &["CLAUDECODE", "CLAUDE_PID", "CLAUDE_EFFORT"];
    const PREFIXES: &[&str] = &["CLAUDE_CODE_", "CLAUDE_AGENT_"];
    NAMES.contains(&name)
        || PREFIXES.iter().any(|prefix| name.starts_with(prefix))
        || (name.starts_with("CODEX_") && name != "CODEX_HOME")
}

/// A TOML basic string, which JSON's escaping is a valid form of.
fn toml_string(text: &str) -> String {
    Value::String(text.to_string()).to_string()
}

fn write_claude_config(mcp: &McpInfo, folder: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    const OWNER_ONLY: u32 = 0o600;
    let path = folder.join("mcp-client.json");
    let config = json!({
        "mcpServers": {
            SERVER_NAME: {
                "type": "http",
                "url": mcp.url,
                "headers": {"Authorization": format!("Bearer {}", mcp.token)},
            },
        },
    });
    // Created private, so the token is never readable by others, even for a moment.
    let mut options = std::fs::OpenOptions::new();
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, OWNER_ONLY);
    let mut file = options
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)?;
    std::io::Write::write_all(&mut file, config.to_string().as_bytes())?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(OWNER_ONLY))?;
    Ok(path)
}

/// Reads one agent's output, line by line, into events.
pub struct Parser {
    agent: AgentKind,
    /// Whether any of the answer has been given, so a later message starts a new paragraph.
    said: bool,
}

impl Parser {
    pub fn line(&mut self, line: &str) -> Vec<AgentEvent> {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        let found = match self.agent {
            AgentKind::Claude => claude_events(&event),
            AgentKind::Codex => codex_events(&event),
        };
        let mut events = Vec::new();
        for event in found {
            let event = match event {
                // Codex gives each message whole, so a later one starts a new paragraph.
                AgentEvent::Text { text } if self.agent == AgentKind::Codex && self.said => {
                    AgentEvent::Text {
                        text: format!("\n\n{text}"),
                    }
                }
                other => other,
            };
            self.said |= matches!(event, AgentEvent::Text { .. });
            events.push(event);
        }
        events
    }

    /// Reads the agent's output to its end, passing each event on.
    pub fn pump(mut self, output: ChildStdout, mut each: impl FnMut(AgentEvent)) {
        for line in BufReader::new(output).lines().map_while(Result::ok) {
            for event in self.line(&line) {
                each(event);
            }
        }
    }
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

fn claude_events(event: &Value) -> Vec<AgentEvent> {
    let mut found = Vec::new();
    if event["type"] == "system" {
        if let Some(id) = event["session_id"].as_str() {
            found.push(AgentEvent::Session { id: id.to_string() });
        }
    }
    match event["type"].as_str() {
        Some("stream_event") => {
            let inner = &event["event"];
            if inner["type"] == "content_block_delta" && inner["delta"]["type"] == "text_delta" {
                found.push(AgentEvent::Text {
                    text: text(&inner["delta"]["text"]),
                });
            }
        }
        Some("assistant") => {
            for block in event["message"]["content"].as_array().into_iter().flatten() {
                if block["type"] == "tool_use" {
                    let name = text(&block["name"]);
                    found.push(AgentEvent::Tool {
                        name: name.trim_start_matches(CLAUDE_TOOL_PREFIX).to_string(),
                        input: block["input"].to_string(),
                    });
                }
            }
        }
        Some("result") if event["is_error"] == true => {
            found.push(AgentEvent::Error {
                text: event["result"]
                    .as_str()
                    .unwrap_or("The agent reported an error.")
                    .to_string(),
            });
        }
        _ => {}
    }
    found
}

fn codex_events(event: &Value) -> Vec<AgentEvent> {
    let item = &event["item"];
    match (event["type"].as_str(), item["type"].as_str()) {
        (Some("thread.started"), _) => vec![AgentEvent::Session {
            id: text(&event["thread_id"]),
        }],
        (Some("item.started"), Some("mcp_tool_call")) => vec![AgentEvent::Tool {
            name: text(&item["tool"]),
            input: item["arguments"].to_string(),
        }],
        (Some("item.started"), Some("command_execution")) => vec![AgentEvent::Tool {
            name: "shell".to_string(),
            input: text(&item["command"]),
        }],
        (Some("item.completed"), Some("agent_message")) => vec![AgentEvent::Text {
            text: text(&item["text"]),
        }],
        (Some("item.completed"), Some("error")) => vec![AgentEvent::Error {
            text: text(&item["message"]),
        }],
        (Some("turn.failed"), _) => vec![AgentEvent::Error {
            text: event["error"]["message"]
                .as_str()
                .unwrap_or("The agent reported an error.")
                .to_string(),
        }],
        (Some("error"), _) => vec![AgentEvent::Error {
            text: text(&event["message"]),
        }],
        _ => Vec::new(),
    }
}

/// Reads what the agent prints as errors, alongside its output, so neither pipe fills and
/// stalls it. The text comes back when the agent has finished.
pub fn collect_errors(errors: ChildStderr) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut BufReader::new(errors), &mut text);
        text
    })
}

/// Fails unless a question can be asked at all, before any agent is started.
pub fn check_prompt(prompt: &str) -> Result<()> {
    if prompt.trim().is_empty() {
        bail!("There is no question to ask.");
    }
    Ok(())
}
