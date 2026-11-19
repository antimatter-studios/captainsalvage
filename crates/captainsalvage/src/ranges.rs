//! A typed description of where the bytes of one stream (a file's data) are, beside the cluster
//! lists the engine keeps.
//!
//! A stream is a list of ranges. Each range is a stretch of the stream by logical offset, of one
//! kind (readable bytes, a hole, bytes that are missing, compressed bytes, bytes another stream
//! also claims), with the place the bytes are on the source when there is one, or the record they
//! are inline in. The ranges of a stream are sorted, do not overlap, leave no gap and add up to the
//! stream's size, which `RangeList::new` checks.
//!
//! This is the first slice of issue #212. The model is filled in by the FAT, exFAT and NTFS
//! adapters for every entry they describe, from the same data the cluster lists come from, and
//! nothing reads it but tests: `map::EntrySource::Clusters` and NTFS's `StreamReference` stay what
//! the preview, the export, the map and the Disk Rebuilds read. It is never serialised and never
//! stored, so saved scans and the frozen parity references do not change.
//!
//! Reading a range, once something does: only `Sparse` reads as zeros. `Missing` is an error, a
//! `Compressed` range is decoded by the format's own reader (an LZNT1 stream is), and a `Shared` range
//! reads like `Readable` but says another stream claims it.

use crate::allocation::{self, valid_cluster};
use crate::filesystem::{
    NtfsEntry, VolumeGeometry, STORAGE_COMPRESSED, STORAGE_RESIDENT, STORAGE_RUNS,
};
use anyhow::{bail, Result};
use std::collections::HashSet;

/// What the bytes of a range are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeKind {
    /// Bytes that can be read, from the place in `provenance`.
    Readable,
    /// A hole, or bytes the file has not written (past its valid length): they read as zeros, and
    /// are the only ones that do.
    Sparse,
    /// Bytes the file has, that cannot be found or cannot be read: the chain ends early, a cluster
    /// is outside the volume, or the format keeps them where this app does not follow.
    Missing,
    /// Bytes the format stores compressed: a read decodes them, and a unit that does not decode is
    /// an error, never zeros.
    Compressed,
    /// Bytes that another stream claims too: readable, but not known to be this stream's.
    Shared,
}

/// Where the bytes of a range are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provenance {
    /// On the source, from the start of the volume: `length` bytes at `offset`.
    Source { offset: u64, length: u64 },
    /// Inside a record of the filesystem rather than in a cluster (an NTFS resident stream): the
    /// record's number, when its header says one.
    InlineInRecord { record: Option<u64> },
}

/// One stretch of a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    /// Bytes from the start of the stream.
    pub start: u64,
    pub length: u64,
    pub kind: RangeKind,
    pub provenance: Option<Provenance>,
}

impl Range {
    pub fn end(&self) -> u64 {
        self.start + self.length
    }
}

/// The ranges of one stream, checked: sorted, contiguous from zero, no overlap, no empty range,
/// and adding up to the stream's size.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RangeList {
    ranges: Vec<Range>,
}

impl RangeList {
    /// The ranges of a stream of `size` bytes, or why they are not one.
    pub fn new(ranges: Vec<Range>, size: u64) -> Result<Self> {
        let mut at = 0u64;
        for range in &ranges {
            if range.length == 0 {
                bail!("a range at byte {} has no length", range.start);
            }
            if range.start < at {
                bail!(
                    "the range at byte {} overlaps or comes before the one before it, which ends at {at}",
                    range.start
                );
            }
            if range.start > at {
                bail!("bytes {at} to {} are in no range", range.start);
            }
            at = range.start.checked_add(range.length).ok_or_else(|| {
                anyhow::anyhow!("a range at byte {} runs past the largest size", range.start)
            })?;
        }
        if at != size {
            bail!("the ranges add up to {at} bytes, not the stream's {size}");
        }
        Ok(Self { ranges })
    }

    pub fn ranges(&self) -> &[Range] {
        &self.ranges
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// The stream's size: where the last range ends.
    pub fn size(&self) -> u64 {
        self.ranges.last().map_or(0, Range::end)
    }

    /// The bytes of each kind, added up.
    pub fn bytes_of(&self, kind: RangeKind) -> u64 {
        self.ranges
            .iter()
            .filter(|range| range.kind == kind)
            .map(|range| range.length)
            .sum()
    }

    /// A stream that is one range of one kind.
    pub fn whole(size: u64, kind: RangeKind, provenance: Option<Provenance>) -> Self {
        Self {
            ranges: if size == 0 {
                Vec::new()
            } else {
                vec![Range {
                    start: 0,
                    length: size,
                    kind,
                    provenance,
                }]
            },
        }
    }

    /// Marks as `Shared` the readable bytes that sit on the source at an offset `is_shared` says
    /// another stream claims, splitting ranges where the answer changes. `step` is the size of the
    /// units claimed (a cluster), and `is_shared` is asked about the offset of each unit.
    pub fn mark_shared(&mut self, step: u64, is_shared: impl Fn(u64) -> bool) {
        let mut marked: Vec<Range> = Vec::new();
        for range in &self.ranges {
            let (RangeKind::Readable, Some(Provenance::Source { offset, .. })) =
                (range.kind, range.provenance)
            else {
                marked.push(*range);
                continue;
            };
            let mut taken = 0;
            while taken < range.length {
                let unit = step.min(range.length - taken);
                let kind = if is_shared(offset + taken) {
                    RangeKind::Shared
                } else {
                    RangeKind::Readable
                };
                push_merged(
                    &mut marked,
                    Range {
                        start: range.start + taken,
                        length: unit,
                        kind,
                        provenance: Some(Provenance::Source {
                            offset: offset + taken,
                            length: unit,
                        }),
                    },
                );
                taken += unit;
            }
        }
        self.ranges = marked;
    }

    /// The bytes from `valid` on are unwritten: they read as zeros whatever clusters hold them.
    fn unwritten_from(mut self, valid: u64) -> Self {
        let mut kept: Vec<Range> = Vec::new();
        for range in self.ranges.drain(..) {
            if range.end() <= valid {
                kept.push(range);
            } else if range.start >= valid {
                push_merged(
                    &mut kept,
                    Range {
                        start: range.start,
                        length: range.length,
                        kind: RangeKind::Sparse,
                        provenance: None,
                    },
                );
            } else {
                let head = valid - range.start;
                kept.push(Range {
                    length: head,
                    provenance: range.provenance.map(|place| shorten(place, head)),
                    ..range
                });
                push_merged(
                    &mut kept,
                    Range {
                        start: valid,
                        length: range.length - head,
                        kind: RangeKind::Sparse,
                        provenance: None,
                    },
                );
            }
        }
        Self { ranges: kept }
    }

    /// The ranges of a FAT or exFAT stream that starts at `first` and is `length` bytes, from the
    /// same rules `allocation::clusters_for` uses for its cluster list: a chain in the FAT, or a
    /// contiguous run when the entry says it has no chain. Bytes the chain does not reach, because
    /// it ends early or leaves the volume, are `Missing`, and bytes past `valid_length` are `Sparse`.
    pub fn for_cluster_stream(
        geometry: &VolumeGeometry,
        fat: &[u32],
        first: u32,
        length: u64,
        valid_length: u64,
        no_fat_chain: bool,
    ) -> Self {
        let size = geometry.cluster_size();
        let needed = length.div_ceil(size);
        let mut found: Vec<Range> = Vec::new();
        let mut claimed = 0u64;
        let add = |cluster: u32, found: &mut Vec<Range>, claimed: &mut u64| {
            let bytes = size.min(length - *claimed);
            push_merged(
                found,
                Range {
                    start: *claimed,
                    length: bytes,
                    kind: RangeKind::Readable,
                    provenance: Some(Provenance::Source {
                        offset: allocation::cluster_offset(geometry, cluster),
                        length: bytes,
                    }),
                },
            );
            *claimed += bytes;
        };
        if needed > 0
            && valid_cluster(geometry, first)
            && needed <= u64::from(geometry.cluster_count)
        {
            if no_fat_chain {
                let end =
                    (u64::from(first) + needed).min(u64::from(geometry.last_cluster_exclusive()));
                for cluster in first..end as u32 {
                    add(cluster, &mut found, &mut claimed);
                }
            } else {
                let mut seen = HashSet::new();
                let mut current = first;
                while valid_cluster(geometry, current) && claimed < length && seen.insert(current) {
                    add(current, &mut found, &mut claimed);
                    current = fat[current as usize];
                }
            }
        }
        if claimed < length {
            push_merged(
                &mut found,
                Range {
                    start: claimed,
                    length: length - claimed,
                    kind: RangeKind::Missing,
                    provenance: None,
                },
            );
        }
        Self { ranges: found }.unwritten_from(valid_length.min(length))
    }

    /// The ranges of an NTFS stream, from what the FILE record said about it: resident bytes are
    /// inline in the record; runs give readable ranges and holes (`Sparse`); a stream NTFS
    /// compresses is `Compressed`; one that is encrypted, continues in other records, is
    /// incomplete or has no data this app can follow is `Missing`; bytes past the valid length are
    /// `Sparse`; and what the runs do not cover is `Missing`.
    pub fn for_ntfs(
        entry: &NtfsEntry,
        geometry: &VolumeGeometry,
        length: u64,
        valid_length: u64,
    ) -> Self {
        if length == 0 {
            return Self::default();
        }
        match entry.storage.as_str() {
            STORAGE_RESIDENT => Self::whole(
                length,
                RangeKind::Readable,
                Some(Provenance::InlineInRecord {
                    record: entry.record_number,
                }),
            )
            .unwritten_from(valid_length.min(length)),
            STORAGE_COMPRESSED => Self::whole(length, RangeKind::Compressed, None),
            STORAGE_RUNS => {
                let size = geometry.cluster_size();
                let mut runs = entry.runs.clone();
                runs.sort_by_key(|run| run.vcn);
                let mut found: Vec<Range> = Vec::new();
                let mut at = 0u64;
                for run in &runs {
                    let start = run.vcn.saturating_mul(size);
                    if start >= length {
                        break;
                    }
                    if start > at {
                        push_merged(&mut found, missing(at, start - at));
                        at = start;
                    }
                    let end = run
                        .vcn
                        .saturating_add(run.count)
                        .saturating_mul(size)
                        .min(length);
                    if end <= at {
                        continue;
                    }
                    let bytes = end - at;
                    let range = match run.lcn {
                        Some(lcn) => Range {
                            start: at,
                            length: bytes,
                            kind: RangeKind::Readable,
                            provenance: Some(Provenance::Source {
                                offset: lcn.saturating_mul(size) + (at - start),
                                length: bytes,
                            }),
                        },
                        None => Range {
                            start: at,
                            length: bytes,
                            kind: RangeKind::Sparse,
                            provenance: None,
                        },
                    };
                    push_merged(&mut found, range);
                    at = end;
                }
                if at < length {
                    push_merged(&mut found, missing(at, length - at));
                }
                Self { ranges: found }.unwritten_from(valid_length.min(length))
            }
            // Encrypted, attribute-list, incomplete, none, damaged: the bytes are there in the
            // format's terms, and this app cannot follow them.
            _ => Self::whole(length, RangeKind::Missing, None),
        }
    }
}

fn missing(start: u64, length: u64) -> Range {
    Range {
        start,
        length,
        kind: RangeKind::Missing,
        provenance: None,
    }
}

fn shorten(place: Provenance, length: u64) -> Provenance {
    match place {
        Provenance::Source { offset, .. } => Provenance::Source { offset, length },
        inline => inline,
    }
}

/// Adds a range after the last, joining the two when they are one: the same kind, and for readable
/// bytes, side by side on the source.
fn push_merged(ranges: &mut Vec<Range>, next: Range) {
    if let Some(last) = ranges.last_mut() {
        let joins = last.end() == next.start
            && last.kind == next.kind
            && match (last.provenance, next.provenance) {
                (None, None) => true,
                (
                    Some(Provenance::Source { offset, length }),
                    Some(Provenance::Source {
                        offset: next_offset,
                        ..
                    }),
                ) => offset + length == next_offset,
                _ => false,
            };
        if joins {
            last.length += next.length;
            if let Some(Provenance::Source { length, .. }) = &mut last.provenance {
                *length += next.length;
            }
            return;
        }
    }
    ranges.push(next);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: u64, length: u64, kind: RangeKind, offset: Option<u64>) -> Range {
        Range {
            start,
            length,
            kind,
            provenance: offset.map(|offset| Provenance::Source { offset, length }),
        }
    }

    #[test]
    fn ranges_that_add_up_in_order_are_a_stream() {
        let list = RangeList::new(
            vec![
                range(0, 100, RangeKind::Readable, Some(4096)),
                range(100, 50, RangeKind::Sparse, None),
                range(150, 10, RangeKind::Missing, None),
            ],
            160,
        )
        .unwrap();
        assert_eq!(list.size(), 160);
        assert_eq!(list.bytes_of(RangeKind::Readable), 100);
        assert_eq!(list.bytes_of(RangeKind::Sparse), 50);
        assert!(RangeList::new(Vec::new(), 0).unwrap().is_empty());
    }

    #[test]
    fn an_overlap_a_gap_an_empty_range_and_a_wrong_total_are_refused() {
        let readable = |start, length| range(start, length, RangeKind::Readable, Some(0));
        let overlap = RangeList::new(vec![readable(0, 100), readable(50, 100)], 150).unwrap_err();
        assert!(overlap.to_string().contains("overlaps"), "{overlap}");
        let gap = RangeList::new(vec![readable(0, 100), readable(120, 30)], 150).unwrap_err();
        assert!(gap.to_string().contains("in no range"), "{gap}");
        let empty = RangeList::new(vec![readable(0, 0)], 0).unwrap_err();
        assert!(empty.to_string().contains("no length"), "{empty}");
        let total = RangeList::new(vec![readable(0, 100)], 101).unwrap_err();
        assert!(total.to_string().contains("add up to 100"), "{total}");
        let unsorted = RangeList::new(vec![readable(100, 50), readable(0, 100)], 150).unwrap_err();
        // Out of order shows as the gap before the first one.
        assert!(unsorted.to_string().contains("in no range"), "{unsorted}");
        let late = RangeList::new(vec![readable(10, 90)], 100).unwrap_err();
        assert!(late.to_string().contains("in no range"), "{late}");
    }

    #[test]
    fn bytes_past_the_valid_length_become_sparse_and_the_readable_head_keeps_its_place() {
        let list = RangeList::whole(
            100,
            RangeKind::Readable,
            Some(Provenance::Source {
                offset: 512,
                length: 100,
            }),
        )
        .unwritten_from(40);
        assert_eq!(
            list.ranges(),
            [
                range(0, 40, RangeKind::Readable, Some(512)),
                range(40, 60, RangeKind::Sparse, None)
            ]
        );
    }

    #[test]
    fn side_by_side_pieces_join_and_pieces_apart_do_not() {
        let mut found = Vec::new();
        push_merged(&mut found, range(0, 10, RangeKind::Readable, Some(100)));
        push_merged(&mut found, range(10, 10, RangeKind::Readable, Some(110)));
        push_merged(&mut found, range(20, 10, RangeKind::Readable, Some(500)));
        push_merged(&mut found, range(30, 5, RangeKind::Missing, None));
        push_merged(&mut found, range(35, 5, RangeKind::Missing, None));
        assert_eq!(
            found,
            [
                range(0, 20, RangeKind::Readable, Some(100)),
                range(20, 10, RangeKind::Readable, Some(500)),
                range(30, 10, RangeKind::Missing, None),
            ]
        );
    }

    #[test]
    fn what_another_stream_claims_is_marked_shared_and_the_rest_is_not() {
        let mut list =
            RangeList::new(vec![range(0, 40, RangeKind::Readable, Some(1000))], 40).unwrap();
        // Units of ten bytes; the unit at 1020 is claimed by another stream.
        list.mark_shared(10, |offset| offset == 1020);
        assert_eq!(
            list.ranges(),
            [
                range(0, 20, RangeKind::Readable, Some(1000)),
                range(20, 10, RangeKind::Shared, Some(1020)),
                range(30, 10, RangeKind::Readable, Some(1030)),
            ]
        );
        assert_eq!(list.size(), 40);
    }

    fn geometry(cluster_size: u64) -> VolumeGeometry {
        VolumeGeometry {
            volume_serial: 0,
            root_cluster: 0,
            partition_offset: 0,
            data_offset: 0,
            fat_offset: 0,
            fat_bytes: 0,
            bytes_per_sector: 512,
            cluster_size,
            cluster_count: 1000,
            volume_size: cluster_size * 1000,
            record_size: 1024,
        }
    }

    fn ntfs(storage: &str, runs: Vec<crate::filesystem::NtfsRun>) -> NtfsEntry {
        NtfsEntry {
            record_number: Some(42),
            sequence: 1,
            parent_record: None,
            parent_sequence: None,
            storage: storage.to_string(),
            runs,
        }
    }

    fn run(vcn: u64, lcn: Option<u64>, count: u64) -> crate::filesystem::NtfsRun {
        crate::filesystem::NtfsRun { vcn, lcn, count }
    }

    #[test]
    fn an_ntfs_stream_with_a_hole_a_compressed_unit_and_a_missing_run_is_described_as_such() {
        let geometry = geometry(4096);
        // Two clusters at 10, a two-cluster hole, one cluster at 50; then nothing for the last two
        // clusters of a stream of eight (a run list that stops early).
        let stream = ntfs(
            STORAGE_RUNS,
            vec![run(0, Some(10), 2), run(2, None, 2), run(4, Some(50), 1)],
        );
        let list = RangeList::for_ntfs(&stream, &geometry, 8 * 4096, 8 * 4096);
        assert_eq!(
            list.ranges(),
            [
                range(0, 8192, RangeKind::Readable, Some(10 * 4096)),
                range(8192, 8192, RangeKind::Sparse, None),
                range(16384, 4096, RangeKind::Readable, Some(50 * 4096)),
                range(20480, 12288, RangeKind::Missing, None),
            ]
        );
        assert_eq!(
            RangeList::new(list.ranges().to_vec(), 8 * 4096).unwrap(),
            list
        );
        // Bytes the file has not written read as zeros, whatever clusters hold them.
        let unwritten = RangeList::for_ntfs(&stream, &geometry, 8 * 4096, 3 * 4096);
        assert_eq!(unwritten.bytes_of(RangeKind::Readable), 8192);
        assert_eq!(unwritten.size(), 8 * 4096);
        assert_eq!(unwritten.ranges().last().unwrap().kind, RangeKind::Sparse);
        // A stream NTFS compresses, and the formats this app does not follow.
        let compressed =
            RangeList::for_ntfs(&ntfs(STORAGE_COMPRESSED, vec![]), &geometry, 9000, 9000);
        assert_eq!(
            compressed.ranges(),
            [range(0, 9000, RangeKind::Compressed, None)]
        );
        for storage in [
            "encrypted",
            "compression-unsupported",
            "attribute-list",
            "incomplete",
            "none",
            "damaged",
        ] {
            let missing = RangeList::for_ntfs(&ntfs(storage, vec![]), &geometry, 9000, 9000);
            assert_eq!(missing.bytes_of(RangeKind::Missing), 9000, "{storage}");
        }
        let resident = RangeList::for_ntfs(&ntfs(STORAGE_RESIDENT, vec![]), &geometry, 45, 45);
        assert_eq!(
            resident.ranges()[0].provenance,
            Some(Provenance::InlineInRecord { record: Some(42) })
        );
        assert!(RangeList::for_ntfs(&stream, &geometry, 0, 0).is_empty());
    }

    #[test]
    fn holes_and_missing_runs_are_not_readable() {
        let list = RangeList::new(
            vec![
                range(0, 10, RangeKind::Readable, Some(0)),
                range(10, 10, RangeKind::Sparse, None),
                range(20, 10, RangeKind::Missing, None),
                range(30, 10, RangeKind::Compressed, None),
            ],
            40,
        )
        .unwrap();
        assert_eq!(list.bytes_of(RangeKind::Readable), 10);
        assert_eq!(list.bytes_of(RangeKind::Sparse), 10);
        assert_eq!(list.bytes_of(RangeKind::Missing), 10);
        assert_eq!(list.bytes_of(RangeKind::Compressed), 10);
    }
}
