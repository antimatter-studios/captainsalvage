//! The health tools: `probe_health`, `health_map` and `rebind_source`.
//!
//! They show and build the health map of an open source (see [`crate::health`]). Everything here
//! only reads the source; the map and the read journal are kept in the catalog on this Mac.

use crate::disk::RawDisk;
use crate::health::probe::{self, Budget, Options};
use crate::health::{Health, HealthRange, RangeState};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::sync::Arc;

pub const TOOLS: &[&str] = &["probe_health", "health_map"];

pub struct Definition {
    pub name: &'static str,
    pub description: &'static str,
    pub properties: Value,
    pub required: &'static [&'static str],
}

fn integer(description: &str) -> Value {
    json!({"type": "integer", "description": description})
}

pub fn definitions() -> Vec<Definition> {
    vec![
        Definition {
            name: "probe_health",
            description: "Finds the bad zones of a failing drive gently, without reading into them, so the drive is not reset again and again. The grid first: one 4 KiB read at every gridBytes (default 1 GiB) and at the last block, each timed. The first read that fails, is slow (over slowMs, default 500) or resets the drive opens a suspect band, and the grid jumps forward skipBytes (default 16 GiB) before it reads again; a read after the jump that is bad, or far slower than the running median, is part of the band, and the jump repeats; the first clean read ends it. A good but slow read just before the first bad one is part of the band too. The grid is finished first, and a band is never read into: from the last clean read before it to the first clean read after it, the range is marked suspect, which every reader treats like unreadable (scans report it as read errors, previews mark it, copies write zeros and name it), until you forgive it. The edges come last and only when asked (action edges), because each band can cost a reset: from the nearest clean read on each side the probe steps toward the band, halfway each time, and stops at the first slow or failed read, so the band shrinks to the stretch between the last good reads and is never read into. Every read is journaled before it starts: a read the device went away during is marked reset-culprit and never read again, here or by any other tool. Each band reports the resets it has cost. When the drive has reset failingAfterResets times during a probe (default 2) the probe pauses and says so (call again with proceed true to go on), and once the source has reset that often reading becomes careful: every read journaled, none longer than 64 KiB, suspect bands not read. Each call reads at most maxReads blocks (default 64, at most 2000) and for at most maxMillis (default 20000, at most 300000), keeps where it is, and can be continued, also after the app was closed. action: start (begin again with the options, and read the first part of the grid), step (continue the grid; maps no edges), edges (map the edges of the suspect bands, and continue), status (no reads), stop (forget the probe, keep the map), forgive (only the person: forget the health of start..end, suspect bands included, after a repair), unflag (only the person: set the source's reset count back to zero). Only reads.",
            properties: json!({
                "diskId": {"type": "string", "description": "ID from open_raw_disk or open_source."},
                "action": {"type": "string", "description": "start, step, edges, status, stop, forgive or unflag. Default step when a probe is stored, otherwise start."},
                "gridBytes": integer("Distance between the grid reads, at least 1 MiB. Default 1 GiB. Start only."),
                "skipBytes": integer("How far the grid jumps after a bad read, so it does not read into the band. Default 16 GiB. Start only."),
                "edgeBytes": integer("A side stops walking toward a band when its next step is under this. Default 1 MiB. Start only."),
                "slowMs": integer("A read over this is slow. Default 500. Start only."),
                "failingAfterResets": integer("Resets after which the drive is called failing and the probe pauses. Default 2. Start only."),
                "proceed": {"type": "boolean", "description": "Go on after the probe paused for a failing drive."},
                "maxReads": integer("Most blocks to read in this call. Default 64."),
                "maxMillis": integer("Most milliseconds for this call. Default 20000."),
                "start": integer("forgive: first byte."),
                "end": integer("forgive: one past the last byte.")
            }),
            required: &["diskId"],
        },
        Definition {
            name: "health_map",
            description: "The health map of the open source: ranges that read well (good), slowly (slow), that a probe jumped over because a band around them is bad (suspect: never read, edges not mapped yet), failed (unreadable) or that a read of started and never finished, or that the device went away during (reset-culprit), with latencies, how many of each, how many times the device went away while the source was read and whether that makes it a failing drive, what the last open found in the read journal, and the probe's progress with the resets each band has cost. Nothing is read. Suspect, unreadable and reset-culprit ranges are never read again by any tool: scans, previews and copies report them as unreadable and write zeros there, and the fingerprint markers are taken from outside them.",
            properties: json!({
                "diskId": {"type": "string", "description": "ID from open_raw_disk or open_source."},
                "includeJournal": {"type": "boolean", "description": "Also the most recent reads of the journal."},
                "start": integer("Only ranges that end after this byte."),
                "end": integer("Only ranges that start before this byte.")
            }),
            required: &["diskId"],
        },
    ]
}

fn health_of(disk: &RawDisk) -> Result<&Arc<Health>> {
    Ok(&disk
        .health
        .as_ref()
        .context("this source was opened without a health guard, so it has no health map")?
        .health)
}

fn status(health: &Health, extra: Value) -> Result<Value> {
    let probe_state = probe::load(health)?;
    let mut report = health.report();
    report["probe"] = serde_json::to_value(&probe_state)?;
    if let (Some(object), Some(extra)) = (report.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            object.insert(key.clone(), value.clone());
        }
    }
    Ok(report)
}

fn options_from(args: &Value) -> Options {
    let defaults = Options::default();
    Options {
        grid_bytes: args
            .get("gridBytes")
            .and_then(Value::as_u64)
            .unwrap_or(defaults.grid_bytes),
        edge_bytes: args
            .get("edgeBytes")
            .and_then(Value::as_u64)
            .unwrap_or(defaults.edge_bytes),
        slow_ms: args
            .get("slowMs")
            .and_then(Value::as_u64)
            .unwrap_or(defaults.slow_ms),
        skip_bytes: args
            .get("skipBytes")
            .and_then(Value::as_u64)
            .unwrap_or(defaults.skip_bytes),
        failing_after: args
            .get("failingAfterResets")
            .and_then(Value::as_u64)
            .unwrap_or(defaults.failing_after),
    }
}

fn budget_from(args: &Value) -> Budget {
    let defaults = Budget::default();
    Budget {
        max_reads: args
            .get("maxReads")
            .and_then(Value::as_u64)
            .unwrap_or(defaults.max_reads),
        max_millis: args
            .get("maxMillis")
            .and_then(Value::as_u64)
            .unwrap_or(defaults.max_millis),
    }
}

pub fn call(tool: &str, disk: &RawDisk, args: &Value, is_person: bool) -> Result<Value> {
    let health = health_of(disk)?;
    match tool {
        "health_map" => {
            let mut report = status(health, json!({}))?;
            let from = args.get("start").and_then(Value::as_u64).unwrap_or(0);
            let to = args.get("end").and_then(Value::as_u64).unwrap_or(u64::MAX);
            let ranges: Vec<HealthRange> = health
                .snapshot()
                .ranges()
                .iter()
                .filter(|range| range.end > from && range.start < to)
                .cloned()
                .collect();
            report["ranges"] = serde_json::to_value(&ranges)?;
            if args
                .get("includeJournal")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                report["journal"] = json!(health.store().recent(health.key(), 50)?);
            }
            Ok(report)
        }
        "probe_health" => {
            let stored = probe::load(health)?;
            let action = args
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or(if stored.is_some() { "step" } else { "start" });
            match action {
                "start" => {
                    probe::start(health, options_from(args))?;
                    let (_, report) = probe::step_with(health, budget_from(args), false, false)?;
                    status(health, json!({"step": report}))
                }
                "step" | "edges" => {
                    let proceed = args.get("proceed").and_then(Value::as_bool).unwrap_or(false);
                    let (_, report) = probe::step_with(health, budget_from(args), action == "edges", proceed)?;
                    status(health, json!({"step": report}))
                }
                "status" => status(health, json!({})),
                "stop" => {
                    health.store().clear_probe(health.key())?;
                    status(health, json!({"stopped": true}))
                }
                "forgive" => {
                    ensure!(
                        is_person,
                        "Only the person forgives a range: it lets the app read it again, which can reset the drive. Suggest it to them."
                    );
                    let start = args
                        .get("start")
                        .and_then(Value::as_u64)
                        .context("forgive needs start")?;
                    let end = args
                        .get("end")
                        .and_then(Value::as_u64)
                        .context("forgive needs end")?;
                    ensure!(start < end, "start must be below end");
                    health.forgive(start, end);
                    status(health, json!({"forgiven": {"start": start, "end": end}}))
                }
                "unflag" => {
                    ensure!(
                        is_person,
                        "Only the person clears the reset count: it makes the app read the drive as it did before it failed. Suggest it to them."
                    );
                    health.clear_resets();
                    status(health, json!({"resetsCleared": true}))
                }
                other => bail!("unknown action {other}: use start, step, edges, status, stop, forgive or unflag"),
            }
        }
        _ => bail!("{tool} is not a health tool"),
    }
}

/// How a range reads, for the window's lane: the state and where.
pub fn lane(disk: &RawDisk) -> Value {
    match disk.health.as_ref() {
        Some(view) => json!(view
            .health
            .snapshot()
            .ranges()
            .iter()
            .filter(|range| range.state > RangeState::Good)
            .collect::<Vec<_>>()),
        None => json!([]),
    }
}

/// The ranges of the open volume's source that are bad, as the window draws them on the volume's
/// map: in the volume's own bytes and in its clusters (numbered as the map numbers them, from 2).
/// A range that lies before the cluster heap (the boot area and the allocation tables) has bytes and
/// no clusters.
pub fn bad_sectors(session: &crate::session::Session) -> Value {
    use fs_exfat::primitives::FIRST_CLUSTER;
    let volume = &session.volume;
    let heap = volume.geometry.heap_offset();
    let size = volume.geometry.cluster_size().max(1);
    let count = u64::from(volume.geometry.cluster_count);
    let ranges: Vec<Value> = volume
        .source()
        .bad_ranges()
        .iter()
        .map(|range| {
            let clusters = (range.end > heap)
                .then(|| {
                    let first = range.start.saturating_sub(heap) / size;
                    let last = (range.end - 1 - heap) / size;
                    (first < count).then(|| {
                        (
                            u64::from(FIRST_CLUSTER) + first,
                            u64::from(FIRST_CLUSTER) + last.min(count - 1),
                        )
                    })
                })
                .flatten();
            json!({
                "start": range.start,
                "end": range.end,
                "state": range.state,
                "latencyMs": range.latency_ms,
                "foundMillis": range.found_millis,
                "firstCluster": clusters.map(|(first, _)| first),
                "lastCluster": clusters.map(|(_, last)| last),
            })
        })
        .collect();
    json!({"clusterSize": size, "clusterCount": count, "firstCluster": FIRST_CLUSTER, "ranges": ranges})
}
