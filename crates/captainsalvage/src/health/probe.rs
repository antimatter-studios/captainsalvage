//! The probe: finds the bad zones of a drive on purpose, gently, and resumably.
//!
//! **Phase 1, the grid.** One 4 KiB read at every `grid_bytes` (a GiB by default) and at the last
//! block, each timed. A read that fails, is slower than the threshold, or reset the device is bad.
//! The first bad read opens a **suspect band**, and the grid jumps forward `skip_bytes` (16 GiB by
//! default) before it reads again, because a damaged band is much wider than one block and each read
//! into it can cost a reset. A read after the jump that is bad, or that is good but far slower than
//! the running median (a rise of ten times, and over 100 ms), is part of the band too, and the grid
//! jumps again. The first clean read after the jump ends the band. A good but slow read just before
//! the first bad one is part of the band as well. The whole grid is read first, and the band is
//! never read into: everything from the last clean read before it to the first clean read after it is
//! marked `suspect`, which every reader treats as unreadable.
//!
//! **Phase 2, the edges, only when asked for.** The grid ends there. Mapping edges can cost a reset
//! for each band, so the person chooses when (`action: edges`). For each band, the probe walks from the
//! last clean read before it toward its first bad read, each step halfway to it, and from the first clean
//! read after it backwards toward its last bad read the same way. A side stops at the first read that
//! is slow or fails, or when its step is under `edge_bytes`. The band then shrinks to the stretch
//! between the last good read on each side, still suspect, and what was suspect outside it is free
//! again. Each band keeps count of the resets it has cost.
//!
//! **A failing drive.** Each time the device goes away the source's reset count goes up. The probe
//! pauses when `failing_after` resets (2 by default) have happened since it was last told to go on
//! (`proceed`), and says so. Once the source has reset that often, reads through the guard are careful:
//! each is journaled, none is longer than 64 KiB, and suspect bands are not read.
//!
//! Each call to [`step`] reads at most `max_reads` blocks and for at most `max_millis`, stores where
//! it is, and can be called again, also after the app was closed. A probe only reads.

use super::map::{HealthRange, RangeState};
use super::{Health, BLOCK, DEFAULT_FAILING_AFTER, DEFAULT_SLOW_MS};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

pub const DEFAULT_GRID_BYTES: u64 = 1024 * 1024 * 1024;
pub const MIN_GRID_BYTES: u64 = 1024 * 1024;
pub const DEFAULT_EDGE_BYTES: u64 = 1024 * 1024;
pub const DEFAULT_SKIP_BYTES: u64 = 16 * 1024 * 1024 * 1024;
pub const DEFAULT_MAX_READS: u64 = 64;
pub const DEFAULT_MAX_MILLIS: u64 = 20_000;
pub const MAX_MAX_READS: u64 = 2_000;
pub const MAX_MAX_MILLIS: u64 = 300_000;
/// The note of the zone a probe bounded, so a later probe does not bound it again.
const ZONE_NOTE: &str = "bounded zone";
const BAND_NOTE: &str = "suspect band: not read, its edges are not mapped";
/// A good read is elevated when it takes this many times the running median, and at least
/// [`RISE_FLOOR_MS`].
const RISE_FACTOR: u64 = 10;
const RISE_FLOOR_MS: u64 = 100;
/// How many clean latencies the median is taken over, and how many it needs.
const RECENT: usize = 32;
const RECENT_NEEDED: usize = 5;

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

/// A stretch of the source where reads failed, were slow or reset the device, and which the grid
/// jumped over instead of reading.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Band {
    /// The end of the last clean read before the band.
    pub start: u64,
    /// The first bad read, and the end of the last one.
    pub first_bad: u64,
    pub last_bad_end: u64,
    /// The start of the first clean read after the band.
    pub end: u64,
    pub state: RangeState,
    pub latency_ms: Option<u64>,
    /// How many times the device went away on reads of this band.
    pub resets: u64,
    pub mapped: bool,
    /// Where the band stands after its edges were mapped.
    pub zone: Option<(u64, u64)>,
}

/// The band whose edges are being mapped.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Zone {
    pub band: usize,
    /// The first bad read and the end of the last one.
    pub start: u64,
    pub end: u64,
    pub state: RangeState,
    pub latency_ms: Option<u64>,
    /// None when no good read lies on that side: the zone then reaches the end of the source.
    pub lower: Option<Approach>,
    pub upper: Option<Approach>,
    /// The reads that were good on the way.
    pub good_reads: Vec<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeState {
    pub grid_bytes: u64,
    pub edge_bytes: u64,
    pub slow_ms: u64,
    pub skip_bytes: u64,
    pub failing_after: u64,
    pub phase: Phase,
    /// The person said to map the edges.
    pub edges_enabled: bool,
    pub next_point: u64,
    pub points: u64,
    /// The band the grid is in, if the last read was bad.
    pub open: Option<Band>,
    pub bands: Vec<Band>,
    /// The end of the last clean read.
    pub clean_before: Option<u64>,
    /// The latencies of the last clean reads, for the median.
    pub recent_ms: Vec<u64>,
    pub zone: Option<Zone>,
    pub reads: u64,
    /// How many times the device went away during this probe, and how many of them the person has
    /// said to go on after.
    pub resets: u64,
    pub acknowledged_resets: u64,
    pub zones_found: u64,
    pub stopped_because: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub grid_bytes: u64,
    pub edge_bytes: u64,
    pub slow_ms: u64,
    pub skip_bytes: u64,
    pub failing_after: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            grid_bytes: DEFAULT_GRID_BYTES,
            edge_bytes: DEFAULT_EDGE_BYTES,
            slow_ms: DEFAULT_SLOW_MS,
            skip_bytes: DEFAULT_SKIP_BYTES,
            failing_after: DEFAULT_FAILING_AFTER,
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
        ensure!(
            options.skip_bytes >= BLOCK,
            "skipBytes must be at least 4096"
        );
        ensure!(
            options.failing_after >= 1,
            "failingAfterResets must be at least 1"
        );
        let last = last_block(length);
        Ok(Self {
            grid_bytes: options.grid_bytes,
            edge_bytes: options.edge_bytes,
            slow_ms: options.slow_ms.max(1),
            skip_bytes: options.skip_bytes,
            failing_after: options.failing_after,
            phase: Phase::Grid,
            edges_enabled: false,
            next_point: 0,
            points: last.div_ceil(options.grid_bytes) + 1,
            open: None,
            bands: Vec::new(),
            clean_before: None,
            recent_ms: Vec::new(),
            zone: None,
            reads: 0,
            resets: 0,
            acknowledged_resets: 0,
            zones_found: 0,
            stopped_because: None,
        })
    }

    pub fn finished(&self) -> bool {
        self.phase == Phase::Done
    }

    /// The median of the latencies of the last clean reads, once there are enough of them.
    fn median_ms(&self) -> Option<u64> {
        if self.recent_ms.len() < RECENT_NEEDED {
            return None;
        }
        let mut sorted = self.recent_ms.clone();
        sorted.sort_unstable();
        Some(sorted[sorted.len() / 2])
    }

    /// A good read that took far longer than the reads around it.
    fn elevated(&self, latency_ms: u64) -> bool {
        let floor = self.median_ms().map_or(RISE_FLOOR_MS, |median| {
            (median * RISE_FACTOR).max(RISE_FLOOR_MS)
        });
        latency_ms >= floor
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
    pub bands: usize,
    pub bands_mapped: usize,
    pub resets: u64,
    pub failing: bool,
    pub zones_found: u64,
    pub zone_in_progress: Option<(u64, u64)>,
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

/// Reads one block. A device that went away is counted and stops the step.
fn read_block(
    health: &Health,
    clock: &mut Clock,
    state: &mut ProbeState,
    offset: u64,
    through_suspect: bool,
) -> Option<(RangeState, u64, bool)> {
    let probed = health.probe_read(offset, BLOCK.min(health.length() - offset), through_suspect);
    if !probed.refused {
        clock.reads += 1;
        state.reads += 1;
    }
    if probed.gone && !probed.in_flight {
        // The device had gone before this read was issued: nothing is learned about the range, and it is
        // not a reset of this probe's. The read is made again when the drive is back.
        state.stopped_because = Some(
            "the device had already gone when the last read was issued, so nothing was learned about that place: reopen the drive, then continue the probe".into(),
        );
        return None;
    }
    if probed.gone {
        state.resets += 1;
        let failing = if health.failing() {
            format!(
                " The drive has now reset {} times and is called failing.",
                health.resets()
            )
        } else {
            String::new()
        };
        state.stopped_because = Some(format!(
            "the device went away during the last read, which is now marked reset-culprit: reopen the drive, then continue the probe, which will not read it again.{failing}"
        ));
    }
    Some((probed.state, probed.latency_ms, probed.gone))
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
    let Some((outcome, latency, gone)) = read_block(health, clock, state, next, true) else {
        return;
    };
    if gone {
        if let Some(band) = state.bands.get_mut(zone.band) {
            band.resets += 1;
        }
    }
    if outcome == RangeState::Good {
        let moved = match side {
            Side::Lower => zone.lower.as_mut(),
            Side::Upper => zone.upper.as_mut(),
        };
        if let Some(moved) = moved {
            moved.anchor = next;
        }
        zone.good_reads.push(next);
    } else {
        zone.state = zone.state.max(outcome);
        zone.latency_ms = zone.latency_ms.max(Some(latency));
        finished(zone);
    }
}

#[derive(Clone, Copy)]
enum Side {
    Lower,
    Upper,
}

fn begin_zone(health: &Health, index: usize, band: &Band) -> Zone {
    let map = health.snapshot();
    let lower = map.good_before(band.start).map(|good| Approach {
        anchor: good.end.saturating_sub(BLOCK).max(good.start),
        finished: false,
    });
    let upper = (band.end < health.length())
        .then(|| map.good_after(band.end))
        .flatten()
        .map(|good| Approach {
            anchor: good.start,
            finished: false,
        });
    Zone {
        band: index,
        start: band.first_bad,
        end: band.last_bad_end,
        state: band.state,
        latency_ms: band.latency_ms,
        lower,
        upper,
        good_reads: Vec::new(),
    }
}

fn close_zone(health: &Health, state: &mut ProbeState, zone: Zone) {
    let Some(band) = state.bands.get(zone.band).cloned() else {
        return;
    };
    let start = zone
        .lower
        .as_ref()
        .map_or(band.start, |side| side.anchor + BLOCK)
        .min(zone.start);
    let end = zone
        .upper
        .as_ref()
        .map_or(band.end, |side| side.anchor)
        .max(zone.end)
        .min(health.length());
    health.resolve_band(
        band.start,
        band.end,
        HealthRange::new(
            start,
            end,
            zone.state.max(RangeState::Suspect),
            zone.latency_ms,
            ZONE_NOTE,
        ),
        &zone.good_reads,
    );
    if let Some(band) = state.bands.get_mut(zone.band) {
        band.mapped = true;
        band.zone = Some((start, end));
    }
    state.zones_found += 1;
}

/// Ends the band the grid is in, at the start of the clean read after it.
fn close_band(health: &Health, state: &mut ProbeState, mut band: Band, end: u64) {
    band.end = end.max(band.last_bad_end);
    health.record_quiet(HealthRange::new(
        band.start,
        band.end,
        RangeState::Suspect,
        band.latency_ms,
        BAND_NOTE,
    ));
    state.bands.push(band);
}

/// One grid read, and what it does to the bands.
fn grid_point(health: &Health, clock: &mut Clock, state: &mut ProbeState, offset: u64) {
    let known = health.blocked(offset, offset + BLOCK);
    let (outcome, latency, gone) = match known {
        // Already known to be bad: not read, and it counts as bad.
        Some(range) => (range.state, 0, false),
        None => match read_block(health, clock, state, offset, false) {
            Some(read) => read,
            None => {
                // Read this point again after the drive is back.
                state.next_point = state.next_point.saturating_sub(1);
                return;
            }
        },
    };
    let bad = outcome != RangeState::Good;
    let elevated = !bad && state.elevated(latency);
    let jump_to = offset.saturating_add(state.skip_bytes);
    if !bad && !elevated {
        // Clean: it ends the band the grid was in.
        state.recent_ms.push(latency);
        if state.recent_ms.len() > RECENT {
            state.recent_ms.remove(0);
        }
        if let Some(band) = state.open.take() {
            close_band(health, state, band, offset);
        }
        state.clean_before = Some(offset + BLOCK);
        return;
    }
    if let Some(band) = state.open.as_mut() {
        // Bad, or good and far slower than the median, right after a jump: still the band.
        band.last_bad_end = offset + BLOCK;
        band.state = band.state.max(outcome.max(RangeState::Suspect));
        band.latency_ms = band.latency_ms.max(Some(latency));
        if gone {
            band.resets += 1;
        }
    } else if bad {
        state.open = Some(Band {
            start: state.clean_before.unwrap_or(0),
            first_bad: offset,
            last_bad_end: offset + BLOCK,
            end: health.length(),
            state: outcome.max(RangeState::Suspect),
            latency_ms: Some(latency),
            resets: u64::from(gone),
            mapped: false,
            zone: None,
        });
    } else {
        // Elevated, on its own: not a band yet. If the next read is bad it will be part of one,
        // because the last clean read is still the one before it.
        return;
    }
    // Never read inside the band: the grid goes on after the jump.
    let next = jump_to.div_ceil(state.grid_bytes);
    state.next_point = state.next_point.max(next);
}

const EDGES_NEEDED: &str = "the grid is complete and its suspect bands are not read; mapping their edges can cost a reset for each band, so it waits for the person: call probe_health with action edges";

/// Starts a probe, replacing one that was stored.
pub fn start(health: &Health, options: Options) -> Result<ProbeState> {
    let state = ProbeState::new(health.length(), options)?;
    health.set_slow_ms(state.slow_ms);
    health.set_failing_after(state.failing_after);
    health
        .store()
        .save_probe(health.own_key(), &serde_json::to_string(&state)?)?;
    Ok(state)
}

/// The probe stored for this source, if any.
pub fn load(health: &Health) -> Result<Option<ProbeState>> {
    Ok(health
        .store()
        .load_probe(health.own_key())?
        .and_then(|text| serde_json::from_str(&text).ok()))
}

/// Goes on with the stored probe for at most the budget, and stores where it is. It maps no edges.
pub fn step(health: &Health, budget: Budget) -> Result<(ProbeState, StepReport)> {
    step_with(health, budget, false, false)
}

/// Like [`step`], and the person says to map the edges of the suspect bands now.
pub fn edges(health: &Health, budget: Budget) -> Result<(ProbeState, StepReport)> {
    step_with(health, budget, true, false)
}

/// `proceed`: the person has seen that the drive is failing and says to go on.
pub fn step_with(
    health: &Health,
    budget: Budget,
    enable_edges: bool,
    proceed: bool,
) -> Result<(ProbeState, StepReport)> {
    let mut state = load(health)?
        .ok_or_else(|| anyhow::anyhow!("no probe was started for this source: start one first"))?;
    ensure!(
        !health.is_gone(),
        "the device has gone away: reopen it (the source is found again by its markers) and continue the probe"
    );
    health.set_slow_ms(state.slow_ms);
    state.stopped_because = None;
    if enable_edges {
        state.edges_enabled = true;
    }
    if proceed {
        state.acknowledged_resets = state.resets;
    }
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
        if state.resets >= state.acknowledged_resets + state.failing_after {
            state.stopped_because = Some(format!(
                "the drive reset {} times during this probe and is failing: each read into a bad area can cost another reset, so the probe pauses. Stop here, or call probe_health with proceed true to go on",
                state.resets - state.acknowledged_resets
            ));
            break;
        }
        match state.phase {
            Phase::Grid => {
                if state.next_point >= state.points {
                    if let Some(band) = state.open.take() {
                        close_band(health, &mut state, band, length);
                    }
                    state.phase = if state.bands.is_empty() {
                        Phase::Done
                    } else {
                        Phase::Edges
                    };
                    continue;
                }
                let offset = point_offset(&state, length, state.next_point);
                state.next_point += 1;
                grid_point(health, &mut clock, &mut state, offset);
            }
            Phase::Edges => {
                if !state.edges_enabled {
                    state.stopped_because = Some(EDGES_NEEDED.into());
                    break;
                }
                if state.zone.is_none() {
                    let Some(index) = state.bands.iter().position(|band| !band.mapped) else {
                        state.phase = Phase::Done;
                        break;
                    };
                    state.zone = Some(begin_zone(health, index, &state.bands[index]));
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
    } else if state.phase == Phase::Edges && !state.edges_enabled {
        state.stopped_because = Some(EDGES_NEEDED.into());
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
        .save_probe(health.own_key(), &serde_json::to_string(&state)?)?;
    let report = StepReport {
        reads: clock.reads,
        millis: clock.started.elapsed().as_millis() as u64,
        phase: state.phase,
        finished: state.finished(),
        stopped_because: state.stopped_because.clone(),
        grid_points_done: state.next_point.min(state.points),
        grid_points: state.points,
        bands: state.bands.len(),
        bands_mapped: state.bands.iter().filter(|band| band.mapped).count(),
        resets: state.resets,
        failing: health.failing(),
        zones_found: state.zones_found,
        zone_in_progress: state.zone.as_ref().map(|zone| (zone.start, zone.end)),
    };
    Ok((state, report))
}
