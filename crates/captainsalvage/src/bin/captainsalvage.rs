//! Captain Salvage command-line entry point.

use anyhow::{bail, Result};

#[path = "salvage-agent.rs"]
mod agent;
#[path = "salvage-scan.rs"]
mod scan;

#[path = "disk.rs"]
mod disk;
#[path = "filesystem.rs"]
mod filesystem;
#[path = "range.rs"]
mod range;

const USAGE: &str = "Captain Salvage command line\n\
\n\
Usage: captainsalvage <command> [arguments]\n\
\n\
Commands:\n\
  filesystem <list|cat|capabilities|ntfs-record|ntfs-stream> <device-or-image> [options]\n\
      Read FAT/exFAT/NTFS files, see what is supported for a filesystem, or inspect and recover\n\
      surviving NTFS records.\n\
  disk <device-or-image> [--scan | --list-scans [--show-all] | --resume ID --accept-sampled-match [--accept-changed-markers]]\n\
      Open raw bytes and save or resume evidence discovery without mounting.\n\
      New scans accept --offset BYTES and --length BYTES. Resume uses its saved range.\n\
  range analyze <device-or-image> [--offset BYTES] [--length BYTES] [--step BYTES] [--no-signatures]\n\
      Slide every filesystem driver over a byte range and report what decodes, and how sure it is.\n\
  scan <device-or-image> [--verify-only | --resolve-only]\n\
      Scan a volume or disk image.\n\
  agent <list|models|ask> ...\n\
      Ask an installed AI agent about the open volume.\n\
\n\
Run 'captainsalvage agent' for agent command details.";

fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.first().map(String::as_str) {
        Some("-h" | "--help" | "help") | None => {
            println!("{USAGE}");
            Ok(())
        }
        Some("--version" | "-V") => {
            println!("captainsalvage {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("scan") => scan::run(&arguments[1..]),
        Some("disk") => disk::run(&arguments[1..]),
        Some("filesystem") => filesystem::run(&arguments[1..]),
        Some("range") => range::run(&arguments[1..]),
        Some("agent") => agent::run(&arguments[1..]),
        Some(command) => bail!("unknown command '{command}'\n\n{USAGE}"),
    }
}
