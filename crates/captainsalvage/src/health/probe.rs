//! The probe: finds the bad zones of a drive on purpose, gently, and resumably.
//!
//! **Phase 1, the grid.** One 4 KiB read at every `grid_bytes` (a GiB by default) and at the last
//! block, each timed. A read that fails, or is slower than the threshold, makes its point bad.
//!
//! **Phase 2, the edges.** For each bad range, the nearest good read on each side is the start.
//! From the lower good read the probe steps toward the bad range, each step halfway to it, and
//! from the upper good read it steps backwards toward it the same way. A side stops at the first
//! read that is slow or fails, or when its step is under `edge_bytes`. The zone is then everything
//! between the last good read on the lower side and the last good read on the upper side: the edges
//! are found from the good side, and the bytes between are never read. A range already marked bad
//! is never read; a side that would step into one stops there.
//!
//! Each call to [`step`] reads at most `max_reads` blocks and for at most `max_millis`, stores where
//! it is, and can be called again, also after the app was closed. A probe only reads.

use super::map::{HealthRange, RangeState};
use super::{Health, BLOCK, DEFAULT_SLOW_MS};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

pub const DEFAULT_GRID_BYTES: u64 = 1024 * 1024 * 1024;
pub const MIN_GRID_BYTES: u64 = 1024 * 1024;
pub const DEFAULT_EDGE_BYTES: u64 = 1024 * 1024;
pub const DEFAULT_MAX_READS: u64 = 64;
pub const DEFAULT_MAX_MILLIS: u64 = 20_000;
pub const MAX_MAX_READS: u64 = 2_000;
pub const MAX_MAX_MILLIS: u64 = 300_000;
/// The note of a zone the probe bounded, so a later probe does not bound it again.
const ZONE_NOTE: &str = "bounded zone";

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Grid,
    Edges,
    Done,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Approach {
    /// Where the last good read of this side starts. Reading goes from there toward the target.
    pub anchor: u64,
    pub finished: bool,
}

/// The zone being bounded.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Zone {
    /// The bad range this started from.
    pub start: u64,
    pub end: u64,
    pub state: RangeState,
    pub latency_ms: Option<u64>,
    /// None when no good read lies on that side: the zone then reaches the end of the source.
    pub lower: Option<Approach>,
    pub upper: Option<Approach>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeState {
    pub grid_bytes: u64,
    pub edge_bytes: u64,
    pub slow_ms: u64,
    pub phase: Phase,
    pub next_point: u64,
    pub points: u64,
    /// The bad ranges phase 2 has to bound, from the start of the source up.
    pub queue: Vec<(u64, u64)>,
    pub zone: Option<Zone>,
    pub reads: u64,
    pub zones_found: u64,
    pub stopped_because: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub grid_bytes: u64,
    pub edge_bytes: u64,
    pub slow_ms: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            grid_bytes: DEFAULT_GRID_BYTES,
            edge_bytes: DEFAULT_EDGE_BYTES,
            slow_ms: DEFAULT_SLOW_MS,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Budget {
    pub max_reads: u64,
    pub max_millis: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_reads: DEFAULT_MAX_READS,
            max_millis: DEFAULT_MAX_MILLIS,
        }
    }
}

fn last_block(length: u64) -> u64 {
    length.saturating_sub(BLOCK) / BLOCK * BLOCK
}

fn point_offset(state: &ProbeState, length: u64, index: u64) -> u64 {
    (index.saturating_mul(state.grid_bytes) / BLOCK * BLOCK).min(last_block(length))
}

impl ProbeState {
    pub fn new(length: u64, options: Options) -> Result<Self> {
        ensure!(length >= BLOCK, "the source is shorter than one block");
        ensure!(
            options.grid_bytes >= MIN_GRID_BYTES,
            "gridBytes must be at least 1 MiB"
        );
        ensure!(
            options.edge_bytes >= BLOCK,
            "edgeBytes must be at least 4096"
        );
        let last = last_block(length);
        Ok(Self {
            grid_bytes: options.grid_bytes,
            edge_bytes: options.edge_bytes,
            slow_ms: options.slow_ms.max(1),
            phase: Phase::Grid,
            next_point: 0,
            points: last.div_ceil(options.grid_bytes) + 1,
            queue: Vec::new(),
            zone: None,
            reads: 0,
            zones_found: 0,
            stopped_because: None,
        })
    }

    pub fn finished(&self) -> bool {
        self.phase == Phase::Done
    }
}

/// How a step ended.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepReport {
    pub reads: u64,
    pub millis: u64,
    pub phase: Phase,
    pub finished: bool,
    pub stopped_because: Option<String>,
    pub grid_points_done: u64,
    pub grid_points: u64,
    pub zones_found: u64,
    pub zone_in_progress: Option<(u64, u64)>,
    pub queued: usize,
}

struct Clock {
    started: Instant,
    reads: u64,
    budget: Budget,
}

impl Clock {
    fn spent(&self) -> bool {
        self.reads >= self.budget.max_reads
            || self.started.elapsed() >= Duration::from_millis(self.budget.max_millis)
    }
}

/// Reads one block and says whether it was good. A bad block updates the zone's state.
fn read_block(
    health: &Health,
    clock: &mut Clock,
    state: &mut ProbeState,
    offset: u64,
) -> Option<(RangeState, u64)> {
    let probed = health.probe_read(offset, BLOCK.min(health.length() - offset));
    if !probed.refused {
        clock.reads += 1;
        state.reads += 1;
    }
    if probed.gone {
        state.stopped_because = Some(
            "the device went away during the last read, which is now marked reset-culprit: reopen the drive, then continue the probe, which will not read it again".into(),
        );
    }
    Some((probed.state, probed.latency_ms))
}

fn bad_ranges(health: &Health) -> Vec<(u64, u64, RangeState, Option<u64>)> {
    health
        .snapshot()
        .ranges()
        .iter()
        .filter(|range| range.state > RangeState::Good)
        .filter(|range| range.note != ZONE_NOTE)
        .map(|range| (range.start, range.end, range.state, range.latency_ms))
        .collect()
}

/// Steps a side toward the target once: halfway from its last good read, finishing the side when the
/// step is under `edge_bytes` or the read is not good.
fn approach(
    health: &Health,
    clock: &mut Clock,
    state: &mut ProbeState,
    side: Side,
    zone: &mut Zone,
) {
    let (anchor, target) = match side {
        Side::Lower => (zone.lower.as_ref().map(|a| a.anchor), zone.start),
        Side::Upper => (
            zone.upper.as_ref().map(|a| a.anchor),
            zone.end.saturating_sub(BLOCK),
        ),
    };
    let Some(anchor) = anchor else { return };
    let gap = anchor.abs_diff(target);
    let step = gap / 2 / BLOCK * BLOCK;
    let finished = |zone: &mut Zone| match side {
        Side::Lower => zone.lower.as_mut().map(|a| a.finished = true),
        Side::Upper => zone.upper.as_mut().map(|a| a.finished = true),
    };
    if step < state.edge_bytes.max(BLOCK) {
        finished(zone);
        return;
    }
    let next = match side {
        Side::Lower => anchor + step,
        Side::Upper => anchor - step,
    };
    match read_block(health, clock, state, next) {
        Some((RangeState::Good, _)) => {
            let moved = match side {
                Side::Lower => zone.lower.as_mut(),
                Side::Upper => zone.upper.as_mut(),
            };
            if let Some(moved) = moved {
                moved.anchor = next;
            }
        }
        Some((bad, latency)) => {
            zone.state = zone.state.max(bad);
            zone.latency_ms = zone.latency_ms.max(Some(latency));
            finished(zone);
        }
        None => {}
    }
}

#[derive(Clone, Copy)]
enum Side {
    Lower,
    Upper,
}

fn begin_zone(health: &Health, range: (u64, u64, RangeState, Option<u64>)) -> Zone {
    let map = health.snapshot();
    let lower = map.good_before(range.0).map(|good| Approach {
        anchor: good.end.saturating_sub(BLOCK).max(good.start),
        finished: false,
    });
    let upper = map.good_after(range.1).map(|good| Approach {
        anchor: good.start,
        finished: false,
    });
    Zone {
        start: range.0,
        end: range.1,
        state: range.2,
        latency_ms: range.3,
        lower,
        upper,
    }
}

fn close_zone(health: &Health, state: &mut ProbeState, zone: Zone) {
    let start = zone
        .lower
        .as_ref()
        .map_or(0, |side| side.anchor + BLOCK)
        .min(zone.start);
    let end = zone
        .upper
        .as_ref()
        .map_or(health.length(), |side| side.anchor)
        .max(zone.end);
    health.replace_zone(HealthRange::new(
        start,
        end.min(health.length()),
        zone.state,
        zone.latency_ms,
        ZONE_NOTE,
    ));
    state.zones_found += 1;
}

/// Starts a probe, replacing one that was stored.
pub fn start(health: &Health, options: Options) -> Result<ProbeState> {
    let state = ProbeState::new(health.length(), options)?;
    health.set_slow_ms(state.slow_ms);
    health
        .store()
        .save_probe(health.key(), &serde_json::to_string(&state)?)?;
    Ok(state)
}

/// The probe stored for this source, if any.
pub fn load(health: &Health) -> Result<Option<ProbeState>> {
    Ok(health
        .store()
        .load_probe(health.key())?
        .and_then(|text| serde_json::from_str(&text).ok()))
}

/// Goes on with the stored probe for at most the budget, and stores where it is.
pub fn step(health: &Health, budget: Budget) -> Result<(ProbeState, StepReport)> {
    let mut state = load(health)?
        .ok_or_else(|| anyhow::anyhow!("no probe was started for this source: start one first"))?;
    ensure!(
        !health.is_gone(),
        "the device has gone away: reopen it (the source is found again by its markers) and continue the probe"
    );
    health.set_slow_ms(state.slow_ms);
    state.stopped_because = None;
    let mut clock = Clock {
        started: Instant::now(),
        reads: 0,
        budget: Budget {
            max_reads: budget.max_reads.clamp(1, MAX_MAX_READS),
            max_millis: budget.max_millis.clamp(1, MAX_MAX_MILLIS),
        },
    };
    let _journal = health.journal_scope();
    let length = health.length();
    while state.phase != Phase::Done && !clock.spent() && state.stopped_because.is_none() {
        match state.phase {
            Phase::Grid => {
                if state.next_point >= state.points {
                    state.queue = bad_ranges(health)
                        .into_iter()
                        .map(|range| (range.0, range.1))
                        .collect();
                    state.phase = Phase::Edges;
                    continue;
                }
                let offset = point_offset(&state, length, state.next_point);
                state.next_point += 1;
                if health.blocked(offset, offset + BLOCK).is_some() {
                    continue;
                }
                read_block(health, &mut clock, &mut state, offset);
            }
            Phase::Edges => {
                if state.zone.is_none() {
                    let Some(next) = (!state.queue.is_empty()).then(|| state.queue.remove(0))
                    else {
                        state.phase = Phase::Done;
                        break;
                    };
                    let map = health.snapshot();
                    // A range an earlier zone already covers is bounded.
                    let range = map
                        .ranges()
                        .iter()
                        .find(|range| {
                            range.start <= next.0
                                && range.end >= next.1
                                && range.state > RangeState::Good
                        })
                        .cloned();
                    match range {
                        Some(range) if range.note != ZONE_NOTE => {
                            state.zone = Some(begin_zone(
                                health,
                                (range.start, range.end, range.state, range.latency_ms),
                            ));
                        }
                        _ => continue,
                    }
                }
                let mut zone = state.zone.take().expect("a zone is being bounded");
                if zone.lower.as_ref().is_some_and(|side| !side.finished) {
                    approach(health, &mut clock, &mut state, Side::Lower, &mut zone);
                } else if zone.upper.as_ref().is_some_and(|side| !side.finished) {
                    approach(health, &mut clock, &mut state, Side::Upper, &mut zone);
                }
                let done = zone.lower.as_ref().is_none_or(|side| side.finished)
                    && zone.upper.as_ref().is_none_or(|side| side.finished);
                if done {
                    close_zone(health, &mut state, zone);
                } else {
                    state.zone = Some(zone);
                }
            }
            Phase::Done => {}
        }
    }
    health.persist();
    if state.phase == Phase::Done {
        state.stopped_because = Some("finished".into());
    } else if state.stopped_because.is_none() {
        state.stopped_because = Some(
            if clock.reads >= clock.budget.max_reads {
                "the read budget of this call was used"
            } else {
                "the time budget of this call was used"
            }
            .into(),
        );
    }
    health
        .store()
        .save_probe(health.key(), &serde_json::to_string(&state)?)?;
    let report = StepReport {
        reads: clock.reads,
        millis: clock.started.elapsed().as_millis() as u64,
        phase: state.phase,
        finished: state.finished(),
        stopped_because: state.stopped_because.clone(),
        grid_points_done: state.next_point.min(state.points),
        grid_points: state.points,
        zones_found: state.zones_found,
        zone_in_progress: state.zone.as_ref().map(|zone| (zone.start, zone.end)),
        queued: state.queue.len(),
    };
    Ok((state, report))
}
