//! Stale regions: which parts of a scan's stored results may be out of date.
//!
//! A scan resumed on a source that was only a *probable* match has markers that changed: a mount
//! wrote Finder data, or a block went bad. A changed marker is evidence that **that** block
//! changed, and nothing more. What was stored for the area around it may no longer describe the
//! drive, so the area is marked stale until it has been read again.
//!
//! This module is the pure part: from the offsets of the changed markers it works out the
//! regions, and answers whether a byte range overlaps them. Storing them is
//! `catalog::stale`, and reading them again is `raw_scan::rescan` and `scan::rescan`.
//!
//! # The radius rule
//!
//! Each changed marker is one 4 KiB block. The stale region built from it is that block plus:
//!
//! 1. **A margin** of [`DEFAULT_RADIUS_BYTES`] (1 MiB) on each side. A mount, or a failing area,
//!    rarely changes exactly one block: Finder writes a few neighbouring clusters, and a bad
//!    stretch of a drive is wider than a block. One mebibyte is wide enough to take those, and
//!    narrow enough that a source of 590 GB with a few changed markers is rescanned in seconds
//!    and not hours. The marker blocks are spread over the whole source, so a margin never
//!    reaches the next marker.
//! 2. **The structure area**, when the changed block is inside the first
//!    [`DEFAULT_HEAD_BYTES`] (32 MiB) of the source: the whole of that first area, from byte 0.
//!    The boot sectors, the allocation tables (the FAT or the bitmap), the upcase table, the
//!    master file table and the root directory all sit there on exFAT, FAT and NTFS volumes, and
//!    they describe every file. A mount rewrites them, and one changed block in them can change
//!    what the rest of the drive means. For a 590 GB exFAT volume the FAT is about 19 MiB, so
//!    32 MiB covers the structures with room for the root directory.
//!
//! The rule is deliberately conservative: it may call more stale than changed, and never less.
//! At most 20 markers exist and a probable match has at most 8 changed, so the regions are
//! bounded: 8 margins of about 2 MiB, and the structure area. Both numbers are in
//! [`StaleOptions`], so a caller (`radiusBytes` and `headBytes` of the tools, `--radius-bytes`
//! and `--head-bytes` of the command line) can widen or narrow them.
//!
//! Markers that could not be read are **unknown, not changed**, so they make no region.

use crate::raw_scan::MARKER_BYTES;
use serde::{Deserialize, Serialize};

/// The margin on each side of a changed marker's block.
pub const DEFAULT_RADIUS_BYTES: u64 = 1024 * 1024;
/// A changed marker whose block starts before this makes the whole first area stale.
pub const DEFAULT_HEAD_BYTES: u64 = 32 * 1024 * 1024;
/// Regions start and end on this boundary, so each is whole blocks of the marker size and of any
/// sector size the drive reads in.
pub const ALIGN_BYTES: u64 = MARKER_BYTES;

/// How wide the stale region around a changed marker is. See the module documentation.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StaleOptions {
    /// The margin on each side of a changed marker's block.
    pub radius_bytes: u64,
    /// A changed marker before this offset makes everything before it stale. Zero turns it off.
    pub head_bytes: u64,
}

impl Default for StaleOptions {
    fn default() -> Self {
        Self {
            radius_bytes: DEFAULT_RADIUS_BYTES,
            head_bytes: DEFAULT_HEAD_BYTES,
        }
    }
}

impl StaleOptions {
    /// The options a tool's arguments name (`radiusBytes`, `headBytes`), the defaults for the rest.
    pub fn from_arguments(args: &serde_json::Value) -> Self {
        let defaults = Self::default();
        Self {
            radius_bytes: args
                .get("radiusBytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(defaults.radius_bytes),
            head_bytes: args
                .get("headBytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(defaults.head_bytes),
        }
    }
}

/// A half-open byte range `start..end` of a source.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub struct Span {
    pub start: u64,
    pub end: u64,
}

impl Span {
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    /// Whether the bytes `offset..offset + length` share a byte with this range. A range with no
    /// length is a point, and is inside when it lies in the range.
    pub fn overlaps(&self, offset: u64, length: u64) -> bool {
        let end = offset.saturating_add(length.max(1));
        offset < self.end && end > self.start
    }
}

fn align_down(value: u64) -> u64 {
    value / ALIGN_BYTES * ALIGN_BYTES
}

fn align_up(value: u64) -> u64 {
    value.div_ceil(ALIGN_BYTES) * ALIGN_BYTES
}

/// The stale regions for the markers that changed, in the source's own offsets, sorted, joined
/// where they touch, and inside `0..source_length`.
pub fn derive_regions(
    changed_offsets: &[u64],
    source_length: u64,
    options: &StaleOptions,
) -> Vec<Span> {
    let marker = MARKER_BYTES.min(source_length);
    let mut spans = Vec::new();
    for &offset in changed_offsets {
        if offset >= source_length {
            continue;
        }
        let block_end = offset.saturating_add(marker).min(source_length);
        spans.push(Span {
            start: align_down(offset.saturating_sub(options.radius_bytes)),
            end: align_up(block_end.saturating_add(options.radius_bytes)).min(source_length),
        });
        if offset < options.head_bytes {
            spans.push(Span {
                start: 0,
                end: options.head_bytes.min(source_length),
            });
        }
    }
    merge(spans)
}

/// Sorts the spans and joins those that overlap or touch. Empty ones are dropped.
pub fn merge(mut spans: Vec<Span>) -> Vec<Span> {
    spans.retain(|span| !span.is_empty());
    spans.sort();
    let mut merged: Vec<Span> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    merged
}

/// The parts of `spans` inside `start..end`.
pub fn clip(spans: &[Span], start: u64, end: u64) -> Vec<Span> {
    spans
        .iter()
        .map(|span| Span {
            start: span.start.max(start),
            end: span.end.min(end),
        })
        .filter(|span| !span.is_empty())
        .collect()
}

/// Whether the bytes `offset..offset + length` overlap any of `spans`.
pub fn overlaps_any(spans: &[Span], offset: u64, length: u64) -> bool {
    spans.iter().any(|span| span.overlaps(offset, length))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    #[test]
    fn a_marker_in_the_middle_makes_its_block_and_a_margin() {
        let regions = derive_regions(&[100 * GIB], 500 * GIB, &StaleOptions::default());
        assert_eq!(
            regions,
            vec![Span {
                start: 100 * GIB - MIB,
                end: 100 * GIB + MARKER_BYTES + MIB
            }]
        );
    }

    #[test]
    fn a_marker_in_the_first_area_makes_the_whole_structure_area() {
        let regions = derive_regions(&[0], 500 * GIB, &StaleOptions::default());
        assert_eq!(
            regions,
            vec![Span {
                start: 0,
                end: DEFAULT_HEAD_BYTES
            }]
        );
        let regions = derive_regions(&[20 * MIB], 500 * GIB, &StaleOptions::default());
        assert_eq!(regions[0].start, 0, "the structures start at byte 0");
        assert_eq!(regions[0].end, DEFAULT_HEAD_BYTES);
    }

    #[test]
    fn a_marker_just_past_the_first_area_makes_only_its_margin() {
        let regions = derive_regions(&[DEFAULT_HEAD_BYTES], 500 * GIB, &StaleOptions::default());
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].start, DEFAULT_HEAD_BYTES - MIB);
        assert!(regions[0].start > 0);
    }

    #[test]
    fn regions_that_touch_or_overlap_join_and_far_ones_stay_apart() {
        let options = StaleOptions::default();
        let regions = derive_regions(
            &[100 * GIB, 100 * GIB + MIB, 300 * GIB],
            500 * GIB,
            &options,
        );
        assert_eq!(regions.len(), 2);
        assert!(regions[0].end > 100 * GIB + MIB);
        assert_eq!(regions[1].start, 300 * GIB - MIB);
    }

    #[test]
    fn regions_stay_inside_the_source_and_on_block_boundaries() {
        let length = 10 * MIB + 123;
        let regions = derive_regions(
            &[length - 4096 - 123, 5000],
            length,
            &StaleOptions::default(),
        );
        for region in &regions {
            assert!(region.end <= length);
            assert_eq!(region.start % ALIGN_BYTES, 0);
            assert!(region.end % ALIGN_BYTES == 0 || region.end == length);
        }
        assert!(derive_regions(&[length + 1], length, &StaleOptions::default()).is_empty());
    }

    #[test]
    fn the_rule_is_configurable() {
        let narrow = StaleOptions {
            radius_bytes: 0,
            head_bytes: 0,
        };
        let regions = derive_regions(&[0, 8 * MIB], 100 * MIB, &narrow);
        assert_eq!(
            regions,
            vec![
                Span {
                    start: 0,
                    end: MARKER_BYTES
                },
                Span {
                    start: 8 * MIB,
                    end: 8 * MIB + MARKER_BYTES
                }
            ]
        );
        let options = StaleOptions::from_arguments(&serde_json::json!({"radiusBytes": 4096}));
        assert_eq!(options.radius_bytes, 4096);
        assert_eq!(options.head_bytes, DEFAULT_HEAD_BYTES);
    }

    #[test]
    fn overlap_and_clip() {
        let spans = vec![Span {
            start: 100,
            end: 200,
        }];
        assert!(overlaps_any(&spans, 199, 1));
        assert!(overlaps_any(&spans, 50, 51));
        assert!(
            !overlaps_any(&spans, 50, 50),
            "ending where the span starts is outside"
        );
        assert!(!overlaps_any(&spans, 200, 10));
        assert!(overlaps_any(&spans, 150, 0), "a point inside counts");
        assert_eq!(
            clip(&spans, 150, 400),
            vec![Span {
                start: 150,
                end: 200
            }]
        );
        assert!(clip(&spans, 200, 400).is_empty());
    }
}
