//! The health map of one source: which ranges of it were read well, slowly, not at all, or were
//! being read when the device went away.
//!
//! The ranges never overlap. Where two observations overlap the worse one wins, so a range once
//! found bad is not made good again by a read next to it; only the person forgives a range.

use serde::{Deserialize, Serialize};

/// How a range reads, from best to worst. The order is the order of severity.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum RangeState {
    /// Read, and read quickly.
    Good,
    /// Read, but slower than the threshold: the drive retried inside.
    Slow,
    /// A read failed.
    Unreadable,
    /// The device went away during a read of this range, or a read of it started and never
    /// finished. It is the likely cause of a reset.
    ResetCulprit,
}

impl RangeState {
    /// A range in this state is not read again by anything that reads the source.
    pub fn blocks(self) -> bool {
        self >= RangeState::Unreadable
    }

    pub fn name(self) -> &'static str {
        match self {
            RangeState::Good => "good",
            RangeState::Slow => "slow",
            RangeState::Unreadable => "unreadable",
            RangeState::ResetCulprit => "reset-culprit",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "good" => RangeState::Good,
            "slow" => RangeState::Slow,
            "unreadable" => RangeState::Unreadable,
            "reset-culprit" => RangeState::ResetCulprit,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HealthRange {
    pub start: u64,
    /// One past the last byte.
    pub end: u64,
    pub state: RangeState,
    /// The slowest read that says so, in milliseconds.
    pub latency_ms: Option<u64>,
    /// Why, in English, for a person or an agent.
    pub note: String,
}

impl HealthRange {
    pub fn new(
        start: u64,
        end: u64,
        state: RangeState,
        latency_ms: Option<u64>,
        note: &str,
    ) -> Self {
        Self {
            start,
            end,
            state,
            latency_ms,
            note: note.into(),
        }
    }

    fn piece(&self, start: u64, end: u64) -> Self {
        Self {
            start,
            end,
            ..self.clone()
        }
    }
}

/// The ranges of one source, sorted and not overlapping.
#[derive(Clone, Debug, Default)]
pub struct HealthMap {
    ranges: Vec<HealthRange>,
}

fn worse_latency(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

impl HealthMap {
    pub fn from_ranges(ranges: Vec<HealthRange>) -> Self {
        let mut map = Self::default();
        for range in ranges {
            map.record(range);
        }
        map
    }

    pub fn ranges(&self) -> &[HealthRange] {
        &self.ranges
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Adds an observation. Where it overlaps what is known, the worse state wins and the
    /// slowest latency is kept.
    pub fn record(&mut self, new: HealthRange) {
        if new.end <= new.start {
            return;
        }
        let mut out: Vec<HealthRange> = Vec::with_capacity(self.ranges.len() + 2);
        let mut cursor = new.start;
        for old in std::mem::take(&mut self.ranges) {
            if old.end <= new.start || old.start >= new.end {
                out.push(old);
                continue;
            }
            if old.start < new.start {
                out.push(old.piece(old.start, new.start));
            }
            let from = old.start.max(new.start);
            let to = old.end.min(new.end);
            if cursor < from {
                out.push(new.piece(cursor, from));
            }
            let winner = if new.state > old.state { &new } else { &old };
            let mut middle = winner.piece(from, to);
            middle.latency_ms = worse_latency(old.latency_ms, new.latency_ms);
            out.push(middle);
            cursor = to;
            if old.end > new.end {
                out.push(old.piece(new.end, old.end));
            }
        }
        if cursor < new.end {
            out.push(new.piece(cursor, new.end));
        }
        out.sort_by_key(|range| range.start);
        self.ranges = coalesce(out);
    }

    /// Replaces everything in `start..end` with one range: a bounded bad zone found by probing
    /// absorbs the single reads inside it.
    pub fn replace(&mut self, new: HealthRange) {
        self.clear(new.start, new.end);
        self.record(new);
    }

    /// Forgets what is known about `start..end`.
    pub fn clear(&mut self, start: u64, end: u64) {
        let mut out = Vec::with_capacity(self.ranges.len() + 1);
        for old in std::mem::take(&mut self.ranges) {
            if old.end <= start || old.start >= end {
                out.push(old);
                continue;
            }
            if old.start < start {
                out.push(old.piece(old.start, start));
            }
            if old.end > end {
                out.push(old.piece(end, old.end));
            }
        }
        self.ranges = out;
    }

    /// The first range that blocks reading and overlaps `start..end`.
    pub fn blocked(&self, start: u64, end: u64) -> Option<&HealthRange> {
        let first = self.ranges.partition_point(|range| range.end <= start);
        self.ranges[first..]
            .iter()
            .take_while(|range| range.start < end)
            .find(|range| range.state.blocks())
    }

    /// The blocking ranges inside `start..end`, cut to it.
    pub fn blocked_within(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        let first = self.ranges.partition_point(|range| range.end <= start);
        self.ranges[first..]
            .iter()
            .take_while(|range| range.start < end)
            .filter(|range| range.state.blocks())
            .map(|range| (range.start.max(start), range.end.min(end)))
            .collect()
    }

    /// The last good range that ends at or before `offset`.
    pub fn good_before(&self, offset: u64) -> Option<&HealthRange> {
        self.ranges
            .iter()
            .rev()
            .find(|range| range.end <= offset && range.state == RangeState::Good)
    }

    /// The first good range that starts at or after `offset`.
    pub fn good_after(&self, offset: u64) -> Option<&HealthRange> {
        self.ranges
            .iter()
            .find(|range| range.start >= offset && range.state == RangeState::Good)
    }

    pub fn count(&self, state: RangeState) -> usize {
        self.ranges
            .iter()
            .filter(|range| range.state == state)
            .count()
    }

    pub fn bytes(&self, state: RangeState) -> u64 {
        self.ranges
            .iter()
            .filter(|range| range.state == state)
            .map(|range| range.end - range.start)
            .sum()
    }
}

/// Joins ranges that touch and say the same.
fn coalesce(ranges: Vec<HealthRange>) -> Vec<HealthRange> {
    let mut out: Vec<HealthRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match out.last_mut() {
            Some(last)
                if last.end == range.start
                    && last.state == range.state
                    && last.note == range.note =>
            {
                last.end = range.end;
                last.latency_ms = worse_latency(last.latency_ms, range.latency_ms);
            }
            _ => out.push(range),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: u64, end: u64, state: RangeState) -> HealthRange {
        HealthRange::new(start, end, state, None, "")
    }

    #[test]
    fn a_worse_state_wins_where_ranges_overlap_and_the_rest_stays() {
        let mut map = HealthMap::default();
        map.record(range(0, 100, RangeState::Good));
        map.record(range(40, 60, RangeState::Unreadable));
        let shown: Vec<_> = map
            .ranges()
            .iter()
            .map(|r| (r.start, r.end, r.state))
            .collect();
        assert_eq!(
            shown,
            vec![
                (0, 40, RangeState::Good),
                (40, 60, RangeState::Unreadable),
                (60, 100, RangeState::Good)
            ]
        );
        // A good read over a bad range does not make it good.
        map.record(range(0, 100, RangeState::Good));
        assert_eq!(map.count(RangeState::Unreadable), 1);
    }

    #[test]
    fn touching_ranges_that_say_the_same_join_and_keep_the_slowest_latency() {
        let mut map = HealthMap::default();
        map.record(HealthRange::new(0, 10, RangeState::Slow, Some(700), ""));
        map.record(HealthRange::new(10, 20, RangeState::Slow, Some(900), ""));
        assert_eq!(map.ranges().len(), 1);
        assert_eq!(
            (map.ranges()[0].end, map.ranges()[0].latency_ms),
            (20, Some(900))
        );
    }

    #[test]
    fn only_unreadable_and_culprit_ranges_block_a_read() {
        let mut map = HealthMap::default();
        map.record(range(0, 10, RangeState::Good));
        map.record(range(10, 20, RangeState::Slow));
        assert!(map.blocked(0, 100).is_none());
        map.record(range(30, 40, RangeState::ResetCulprit));
        assert_eq!(map.blocked(25, 31).map(|r| r.start), Some(30));
        assert!(map.blocked(40, 50).is_none());
        assert_eq!(map.blocked_within(0, 35), vec![(30, 35)]);
    }

    #[test]
    fn replace_absorbs_what_is_inside_and_clear_forgives() {
        let mut map = HealthMap::default();
        map.record(range(10, 14, RangeState::Unreadable));
        map.record(range(20, 24, RangeState::Slow));
        map.replace(range(5, 30, RangeState::Unreadable));
        assert_eq!(map.ranges().len(), 1);
        map.clear(10, 20);
        let shown: Vec<_> = map.ranges().iter().map(|r| (r.start, r.end)).collect();
        assert_eq!(shown, vec![(5, 10), (20, 30)]);
    }

    #[test]
    fn the_nearest_good_range_on_each_side() {
        let mut map = HealthMap::default();
        map.record(range(0, 4, RangeState::Good));
        map.record(range(50, 54, RangeState::Unreadable));
        map.record(range(100, 104, RangeState::Good));
        assert_eq!(map.good_before(50).map(|r| r.start), Some(0));
        assert_eq!(map.good_after(54).map(|r| r.start), Some(100));
        assert!(map.good_after(101).is_none());
    }
}
