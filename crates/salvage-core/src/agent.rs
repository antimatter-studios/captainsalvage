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
const OPENCODE_TOOL_PREFIX: &str = "captain-salvage_";
/// Claude Code takes these names for its newest models, so the list never goes stale.
const CLAUDE_MODELS: &[(&str, &str)] = &[
    ("fable", "Fable"),
    ("opus", "Opus"),
    ("sonnet", "Sonnet"),
    ("haiku", "Haiku"),
];

/// The agents there are adapters for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentKind {
    Claude,
    Codex,
    Opencode,
}

pub const AGENTS: [AgentKind; 3] = [AgentKind::Claude, AgentKind::Codex, AgentKind::Opencode];

/// A model an agent offers. The empty id is the agent's own default.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Model {
    pub id: String,
    pub name: String,
}

/// One agent as the window lists it.
#[derive(Serialize)]
pub struct AgentInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub installed: bool,
    /// How to install it, when it is not.
    pub install: &'static str,
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
    /// An empty folder the agent runs in. Claude Code and opencode also find their MCP settings
    /// file here.
    pub work_dir: &'a Path,
    /// A model id from `models`, or nothing for the agent's default.
    pub model: Option<&'a str>,
}

impl AgentKind {
    pub fn id(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Opencode => "opencode",
        }
    }

    pub fn install(self) -> &'static str {
        match self {
            AgentKind::Claude => "npm install -g @anthropic-ai/claude-code",
            AgentKind::Codex => "npm install -g @openai/codex",
            AgentKind::Opencode => "npm install -g opencode-ai",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            AgentKind::Claude => "Claude Code",
            AgentKind::Codex => "Codex",
            AgentKind::Opencode => "opencode",
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
            AgentKind::Opencode => (
                &[".opencode/bin/opencode", ".local/bin/opencode"],
                &["/opt/homebrew/bin/opencode", "/usr/local/bin/opencode"],
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
                AgentKind::Opencode => {
                    "the opencode command was not found. Install opencode, then try again."
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
                if let Some(model) = ask.model {
                    command.arg("--model").arg(model);
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
                if let Some(model) = ask.model {
                    command.arg("--model").arg(model);
                }
                if let Some(session) = ask.session {
                    command.arg(session);
                }
                command.arg(ask.prompt).env(TOKEN_VARIABLE, &ask.mcp.token);
            }
            AgentKind::Opencode => {
                let config = write_opencode_config(ask.mcp, ask.work_dir, &instructions)?;
                command
                    .args(["run", "--format", "json"])
                    .env("OPENCODE_CONFIG", config);
                if let Some(model) = ask.model {
                    command.arg("--model").arg(model);
                }
                if let Some(session) = ask.session {
                    command.arg("--session").arg(session);
                }
                command.arg(ask.prompt);
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
            install: agent.install(),
        })
        .collect()
}

/// The models an agent offers, its default first. Claude Code's are its own names for its
/// newest models. Codex and opencode are asked for theirs.
pub fn models(agent: AgentKind) -> Result<Vec<Model>> {
    let mut found = vec![Model {
        id: String::new(),
        name: "Default".to_string(),
    }];
    let listed = |args: &[&str]| -> Result<String> {
        let output = Command::new(agent.find()?)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("asking {} for its models", agent.id()))?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    };
    match agent {
        AgentKind::Claude => found.extend(CLAUDE_MODELS.iter().map(|(id, name)| Model {
            id: id.to_string(),
            name: name.to_string(),
        })),
        AgentKind::Codex => found.extend(codex_models(&listed(&["debug", "models"])?)),
        AgentKind::Opencode => found.extend(opencode_models(&listed(&["models"])?)),
    }
    Ok(found)
}

/// Codex lists its models as JSON, some of them hidden from people.
fn codex_models(text: &str) -> Vec<Model> {
    let listed: Value = serde_json::from_str(text).unwrap_or_default();
    listed["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|model| model["visibility"] == "list")
        .filter_map(|model| {
            let id = model["slug"].as_str()?;
            Some(Model {
                id: id.to_string(),
                name: model["display_name"].as_str().unwrap_or(id).to_string(),
            })
        })
        .collect()
}

/// opencode lists one provider/model per line.
fn opencode_models(text: &str) -> Vec<Model> {
    text.lines()
        .map(str::trim)
        .filter(|line| line.contains('/') && !line.contains(' '))
        .map(|line| Model {
            id: line.to_string(),
            name: line.to_string(),
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

/// Writes a file only this user can read, from the moment it exists: these hold the token.
fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    const OWNER_ONLY: u32 = 0o600;
    let mut options = std::fs::OpenOptions::new();
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, OWNER_ONLY);
    let mut file = options.write(true).create(true).truncate(true).open(path)?;
    std::io::Write::write_all(&mut file, text.as_bytes())?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(OWNER_ONLY))?;
    Ok(())
}

/// opencode's settings for one question: only this server, and none of its own tools that
/// read or change files or run commands, so it works only through the engine.
fn write_opencode_config(mcp: &McpInfo, folder: &Path, instructions: &str) -> Result<PathBuf> {
    const OWN_TOOLS: &[&str] = &[
        "bash",
        "edit",
        "write",
        "patch",
        "read",
        "glob",
        "grep",
        "list",
        "webfetch",
        "task",
        "todowrite",
        "todoread",
    ];
    let rules = folder.join("instructions.md");
    std::fs::write(&rules, instructions)?;
    let tools: serde_json::Map<String, Value> = OWN_TOOLS
        .iter()
        .map(|tool| (tool.to_string(), Value::Bool(false)))
        .collect();
    let config = json!({
        "$schema": "https://opencode.ai/config.json",
        "instructions": [rules],
        "mcp": {
            SERVER_NAME: {
                "type": "remote",
                "url": mcp.url,
                "headers": {"Authorization": format!("Bearer {}", mcp.token)},
                "enabled": true,
            },
        },
        "tools": tools,
        "permission": {"edit": "deny", "bash": "deny", "webfetch": "deny"},
    });
    let path = folder.join("opencode.json");
    write_private(&path, &config.to_string())?;
    Ok(path)
}

fn write_claude_config(mcp: &McpInfo, folder: &Path) -> Result<PathBuf> {
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
    write_private(&path, &config.to_string())?;
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
            AgentKind::Opencode => opencode_events(&event),
        };
        let mut events = Vec::new();
        for event in found {
            let event = match event {
                // Codex and opencode give each message whole, so a later one starts a new
                // paragraph.
                AgentEvent::Text { text } if self.agent != AgentKind::Claude && self.said => {
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

fn opencode_events(event: &Value) -> Vec<AgentEvent> {
    let part = &event["part"];
    let mut found = Vec::new();
    match event["type"].as_str() {
        // Every event names the session; the first step is enough to learn it.
        Some("step_start") => {
            if let Some(id) = event["sessionID"].as_str() {
                found.push(AgentEvent::Session { id: id.to_string() });
            }
        }
        Some("tool_use") => found.push(AgentEvent::Tool {
            name: text(&part["tool"])
                .trim_start_matches(OPENCODE_TOOL_PREFIX)
                .to_string(),
            input: part["state"]["input"].to_string(),
        }),
        Some("text") => found.push(AgentEvent::Text {
            text: text(&part["text"]),
        }),
        Some("error") => found.push(AgentEvent::Error {
            text: event["error"]["data"]["message"]
                .as_str()
                .or_else(|| event["error"]["message"].as_str())
                .or_else(|| event["error"]["name"].as_str())
                .unwrap_or("The agent reported an error.")
                .to_string(),
        }),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn read(agent: AgentKind, lines: &[&str]) -> Vec<AgentEvent> {
        let mut parser = agent.parser();
        lines.iter().flat_map(|line| parser.line(line)).collect()
    }

    fn tool(name: &str, input: &str) -> AgentEvent {
        AgentEvent::Tool {
            name: name.into(),
            input: input.into(),
        }
    }

    fn said(text: &str) -> AgentEvent {
        AgentEvent::Text { text: text.into() }
    }

    fn session(id: &str) -> AgentEvent {
        AgentEvent::Session { id: id.into() }
    }

    #[test]
    fn claude_code_streams_its_answer_and_names_its_tools() {
        let events = read(
            AgentKind::Claude,
            &[
                r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"mcp__captain-salvage__volume_overview","input":{}}]}}"#,
                r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"20"}}}"#,
                r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"0"}}}"#,
                r#"{"type":"result","is_error":true,"result":"Overloaded"}"#,
            ],
        );
        assert_eq!(
            events,
            vec![
                session("s1"),
                tool("volume_overview", "{}"),
                said("20"),
                said("0"),
                AgentEvent::Error {
                    text: "Overloaded".into()
                },
            ]
        );
    }

    /// Lines as `codex exec --json` printed them against a test copy of the app.
    #[test]
    fn codex_gives_whole_messages_as_paragraphs() {
        let events = read(
            AgentKind::Codex,
            &[
                r#"{"type":"thread.started","thread_id":"t1"}"#,
                r#"{"type":"turn.started"}"#,
                r#"{"type":"item.started","item":{"id":"item_0","type":"mcp_tool_call","server":"captain-salvage","tool":"volume_overview","arguments":{},"status":"in_progress"}}"#,
                r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"Looking."}}"#,
                r#"{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"200"}}"#,
                r#"{"type":"turn.failed","error":{"message":"usage limit"}}"#,
            ],
        );
        assert_eq!(
            events,
            vec![
                session("t1"),
                tool("volume_overview", "{}"),
                said("Looking."),
                said("\n\n200"),
                AgentEvent::Error {
                    text: "usage limit".into()
                },
            ]
        );
    }

    /// Lines as `opencode run --format json` printed them against a test copy of the app.
    #[test]
    fn opencode_names_its_session_tools_and_text() {
        let events = read(
            AgentKind::Opencode,
            &[
                r#"{"type":"step_start","sessionID":"ses_1","part":{"type":"step-start"}}"#,
                r#"{"type":"tool_use","sessionID":"ses_1","part":{"type":"tool","tool":"captain-salvage_volume_overview","state":{"status":"completed","input":{},"output":"{}"}}}"#,
                r#"{"type":"step_finish","sessionID":"ses_1","part":{"reason":"tool-calls"}}"#,
                r#"{"type":"text","sessionID":"ses_1","part":{"type":"text","text":"200"}}"#,
                r#"{"type":"error","sessionID":"ses_1","error":{"name":"APIError","data":{"message":"rate limited"}}}"#,
            ],
        );
        assert_eq!(
            events,
            vec![
                session("ses_1"),
                tool("volume_overview", "{}"),
                said("200"),
                AgentEvent::Error {
                    text: "rate limited".into()
                },
            ]
        );
    }

    #[test]
    fn model_lists_keep_only_what_people_may_choose() {
        let codex = codex_models(
            r#"{"models":[{"slug":"a","display_name":"A","visibility":"list"},{"slug":"b","visibility":"hide"}]}"#,
        );
        assert_eq!(
            codex,
            vec![Model {
                id: "a".into(),
                name: "A".into()
            }]
        );
        let opencode = opencode_models("opencode/free-one\nnoise line here\nother/two\n");
        assert_eq!(
            opencode
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["opencode/free-one", "other/two"]
        );
    }

    #[test]
    fn the_token_is_never_an_argument() {
        let folder =
            std::env::temp_dir().join(format!("salvage-agent-test-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let mcp = McpInfo {
            url: "http://127.0.0.1:1/mcp".into(),
            token: "secret-token".into(),
            port: 1,
        };
        for agent in AGENTS {
            let ask = Ask {
                mcp: &mcp,
                prompt: "q",
                session: Some("s"),
                instructions: "",
                work_dir: &folder,
                model: Some("m"),
            };
            let command = agent.command(Path::new("/bin/echo"), &ask).unwrap();
            let arguments: Vec<String> = command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            assert!(
                arguments.iter().all(|arg| !arg.contains("secret-token")),
                "{}: {arguments:?}",
                agent.id()
            );
            assert!(
                arguments.iter().any(|arg| arg == "m"),
                "{} ignores the model",
                agent.id()
            );
        }
        let config = std::fs::read_to_string(folder.join("opencode.json")).unwrap();
        assert!(config.contains("\"bash\":false"));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(folder.join("opencode.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&folder);
    }
}
