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
            description: "Finds the bad zones of a failing drive gently, without reading into them, so the drive is not reset again and again. Phase 1 reads one 4 KiB block at every gridBytes (default 1 GiB) and at the last block, timing each. Phase 2 takes each bad or slow block and walks toward it from the nearest good read on each side, halfway each time, stopping at the first read that is slow (over slowMs, default 500) or fails, so a zone is bounded by good reads and the bytes between are never read. Every read is journaled before it starts: a read the device went away during is marked reset-culprit and never read again, here or by any other tool. Each call reads at most maxReads blocks (default 64, at most 2000) and for at most maxMillis (default 20000, at most 300000), keeps where it is, and can be continued, also after the app was closed. action: start (begin again with the options, and do the first step), step (continue), status (no reads), stop (forget the probe, keep the map), forgive (only the person: forget the health of start..end, after a repair). Only reads.",
            properties: json!({
                "diskId": {"type": "string", "description": "ID from open_raw_disk or open_source."},
                "action": {"type": "string", "description": "start, step, status, stop or forgive. Default step when a probe is stored, otherwise start."},
                "gridBytes": integer("Distance between the grid reads, at least 1 MiB. Default 1 GiB. Start only."),
                "edgeBytes": integer("A side stops walking toward a bad block when its next step is under this. Default 1 MiB. Start only."),
                "slowMs": integer("A read over this is slow. Default 500. Start only."),
                "maxReads": integer("Most blocks to read in this call. Default 64."),
                "maxMillis": integer("Most milliseconds for this call. Default 20000."),
                "start": integer("forgive: first byte."),
                "end": integer("forgive: one past the last byte.")
            }),
            required: &["diskId"],
        },
        Definition {
            name: "health_map",
            description: "The health map of the open source: ranges that read well (good), slowly (slow), failed (unreadable) or that a read of started and never finished, or that the device went away during (reset-culprit), with latencies, how many of each, what the last open found in the read journal, and the probe's progress. Nothing is read. Unreadable and reset-culprit ranges are never read again by any tool: scans, previews and copies report them as unreadable and write zeros there, and the fingerprint markers are taken from outside them.",
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
                    let (_, report) = probe::step(health, budget_from(args))?;
                    status(health, json!({"step": report}))
                }
                "step" => {
                    let (_, report) = probe::step(health, budget_from(args))?;
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
                other => bail!("unknown action {other}: use start, step, status, stop or forgive"),
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
