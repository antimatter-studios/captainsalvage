//! Asks an agent about the disk open in Captain Salvage, from the command line, with the same
//! adapters as the window's Ask agent tab. The app must be open: the agent works through its
//! MCP tools.
//!
//! Usage:
//!   salvage-agent list
//!   salvage-agent ask [--agent claude|codex] [--session ID] [--json] QUESTION...
//!
//! The answer is printed as it comes, and each tool the agent uses is shown on standard error.
//! The last line on standard error is the session, to continue with --session. With --json,
//! every event is one line of JSON on standard output instead.

use anyhow::{bail, Context, Result};
use salvage_core::agent::{self, AgentEvent, AgentKind, Ask, CHAT_INSTRUCTIONS};
use salvage_core::mcp::published_info;
use salvage_core::workspace::app_support;
use std::io::Write;

const USAGE: &str = "usage: salvage-agent list\n       salvage-agent ask [--agent claude|codex] [--session ID] [--json] QUESTION...";

fn list() -> Result<()> {
    for found in agent::agents() {
        let state = if found.installed {
            "installed"
        } else {
            "not installed"
        };
        println!("{:<8} {:<12} {state}", found.id, found.name);
    }
    Ok(())
}

fn ask(arguments: &[String]) -> Result<()> {
    let mut kind = AgentKind::Claude;
    let mut session = None;
    let mut json = false;
    let mut words = Vec::new();
    let mut rest = arguments.iter();
    while let Some(argument) = rest.next() {
        match argument.as_str() {
            "--agent" => kind = AgentKind::from_id(rest.next().context("--agent needs a name")?)?,
            "--session" => session = Some(rest.next().context("--session needs an id")?.clone()),
            "--json" => json = true,
            other if other.starts_with("--") => bail!("unknown option {other}\n{USAGE}"),
            word => words.push(word.to_string()),
        }
    }
    let question = words.join(" ");
    agent::check_prompt(&question)?;
    let mcp = published_info()?;
    let work_dir = app_support()?.join("agent").join(kind.id());
    let (mut child, output, errors) = kind.start(&Ask {
        mcp: &mcp,
        prompt: &question,
        session: session.as_deref(),
        instructions: CHAT_INSTRUCTIONS,
        work_dir: &work_dir,
    })?;
    let errors = agent::collect_errors(errors);
    let mut found_session = None;
    let mut failed = false;
    let stdout = std::io::stdout();
    kind.parser().pump(output, |event| {
        let mut out = stdout.lock();
        if json {
            let _ = writeln!(out, "{}", serde_json::to_string(&event).unwrap_or_default());
        }
        match event {
            AgentEvent::Session { id } => found_session = Some(id),
            AgentEvent::Text { text } if !json => {
                let _ = write!(out, "{text}");
            }
            AgentEvent::Tool { name, .. } if !json => eprintln!("[{name}]"),
            AgentEvent::Error { text } => {
                failed = true;
                if !json {
                    eprintln!("error: {text}");
                }
            }
            _ => {}
        }
        let _ = out.flush();
    });
    let status = child.wait()?;
    let error_text = errors.join().unwrap_or_default();
    if !json {
        println!();
    }
    if let Some(id) = found_session {
        eprintln!("session: {id}");
    }
    if !status.success() {
        bail!("{} stopped with {status}: {}", kind.id(), error_text.trim());
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.first().map(String::as_str) {
        Some("list") => list(),
        Some("ask") => ask(&arguments[1..]),
        _ => bail!("{USAGE}"),
    }
}
