//! Runs or resumes the full scan from the command line.
//!
//! Usage: captainsalvage scan <device-or-image> [--verify-only | --resolve-only | --call <tool> [json]]
//!
//! `--call` runs one engine tool as the person. `export_start` copies one file or folder, and
//! `disk_rebuild_copy_start` copies a Disk Rebuild; both wait for the copy to finish.

use anyhow::{bail, Context, Result};
use captainsalvage::engine::{Engine, AUTHOR_USER};
use captainsalvage::map::MapDb;
use captainsalvage::scan::{run_scan, ScanOptions};
use captainsalvage::source::{mount_line, os_denies_writes, FileSource};
use captainsalvage::volume::Volume;
use captainsalvage::workspace::Workspace;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

const REPORT_INTERVAL: Duration = Duration::from_secs(60);
/// The tools that start a copy, which this waits for before it prints the result.
const COPY_TOOLS: &[&str] = &["export_start", "disk_rebuild_copy_start"];

/// Runs one engine tool as the user, then waits for any copy it started.
fn run_tool(device: &str, arguments: &[String]) -> Result<()> {
    let tool = arguments.get(2).context("--call needs a tool name")?;
    let args: serde_json::Value =
        serde_json::from_str(arguments.get(3).map_or("{}", String::as_str))?;
    let engine = Engine::new();
    engine.open(device)?;
    let result = engine.call(tool, &args, AUTHOR_USER)?;
    if COPY_TOOLS.contains(&tool.as_str()) {
        let session = engine.session()?;
        while session.copy_status()?.state == "running" {
            std::thread::sleep(Duration::from_millis(250));
        }
        println!("{}", serde_json::to_string_pretty(&session.copy_status()?)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&result)?);
    }
    Ok(())
}

pub fn run(arguments: &[String]) -> Result<()> {
    let device = arguments
        .first()
        .context("usage: captainsalvage scan <device-or-image> [--verify-only | --resolve-only]")?;
    let mode = arguments.get(1).map(String::as_str);
    let path = Path::new(device);

    if let Some(line) = mount_line(path) {
        bail!("refusing to scan a mounted volume, because macOS may write to it: {line}");
    }

    let source = Arc::new(FileSource::open(path).with_context(|| format!("opening {device}"))?);
    let volume = Volume::open(source.clone())?;
    let workspace = Workspace::for_volume(&volume)?;
    let baseline = workspace.check_baseline(&volume.structures)?;

    println!("device: {device}");
    println!("os denies writes: {}", os_denies_writes(path));
    println!("workspace: {}", workspace.dir().display());
    println!("structure hash: {}", volume.structure_hash);
    println!(
        "baseline unchanged: {} (first recording: {})",
        baseline.unchanged, baseline.first_recording
    );
    println!("stats: {}", serde_json::to_string(&volume.stats)?);

    match mode {
        Some("--call") => return run_tool(device, arguments),
        Some("--verify-only") => return Ok(()),
        Some("--resolve-only") => {
            let mut database = MapDb::open(&workspace.database())?;
            let summary = database.resolve(&volume, &workspace)?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
            return Ok(());
        }
        Some(other) => bail!("unknown option {other}"),
        None => {}
    }

    let stop = AtomicBool::new(false);
    let mut last_report = Instant::now() - REPORT_INTERVAL;
    let final_progress = run_scan(
        &volume,
        &workspace,
        ScanOptions::for_volume(&volume),
        &stop,
        |progress| {
            if last_report.elapsed() >= REPORT_INTERVAL {
                last_report = Instant::now();
                println!(
                "{:>6.2}% cluster {} of {} | {:.0} MB/s | directory clusters {} | entry sets {} | read errors {}",
                f64::from(progress.clusters_scanned) * 100.0 / f64::from(progress.total_clusters),
                progress.clusters_scanned,
                progress.total_clusters,
                progress.bytes_per_second / 1e6,
                progress.directory_clusters,
                progress.sets_found,
                progress.read_errors,
            );
            }
        },
    )?;
    println!("finished: {}", serde_json::to_string(&final_progress)?);

    let database = MapDb::open(&workspace.database())?;
    if let Some(summary) = database.summary()? {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    }
    Ok(())
}
