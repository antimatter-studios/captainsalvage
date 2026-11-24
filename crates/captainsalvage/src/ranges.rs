//! A typed description of where the bytes of one stream (a file's data) are, beside the cluster
//! lists the engine keeps.
//!
//! A stream is a list of ranges. Each range is a stretch of the stream by logical offset, of one
//! kind (readable bytes, a hole, bytes that are missing, compressed bytes, bytes another stream
//! also claims), with the place the bytes are on the source when there is one, or the record they
//! are inline in. The ranges of a stream are sorted, do not overlap, leave no gap and add up to the
//! stream's size, which `RangeList::new` checks.
//!
//! A driver's candidates have ranges too, from the walk the driver made (`RangeList::from_walk`).
//!
//! This is the first slice of issue #212. The model is filled in by the FAT, exFAT and NTFS
//! adapters for every entry they describe, from the same data the cluster lists come from. The second
//! slice moved the preview and the copy of a FAT or exFAT entry, and `Session::read_entry` (the whole file for any
//! entry), to read them (`RangeList::from_clusters` and `RangeList::read`); `map::EntrySource::Clusters` and NTFS's
//! `StreamReference` are still what the map and the Disk Rebuilds read, and a reassembly someone proposes (`read_clusters`)
//! is a list of clusters by definition. It is never serialised and never stored, so saved scans
//! and the frozen parity references do not change.
//!
//! Reading a range, once something does: only `Sparse` reads as zeros. `BadSector` is never read: it is
//! zeros that the caller is told are not the stream's bytes, and are named as bad sectors. `Missing` is an error, a
//! `Compressed` range is decoded by the format's own reader (an LZNT1 stream is), and a `Shared` range
//! reads like `Readable` but says another stream claims it.

use crate::allocation::{self, valid_cluster};
use crate::filesystem::{
    RecordEntry, VolumeGeometry, STORAGE_COMPRESSED, STORAGE_RESIDENT, STORAGE_RUNS,
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
    /// Bytes whose place on the source is in a range the health map says must not be read, because the
    /// drive reset or a read failed there, or a probe jumped over it (see [`crate::health`]). They are
    /// never read: they come back as zeros and are named as bad sectors, which is not what `Missing` is
    /// (bytes the format does not lead to). The place they have on the source is kept in `provenance`.
    BadSector,
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

    /// Marks as `BadSector` the readable and shared bytes whose place on the source falls inside one
    /// of `bad`, as `(start, end)` source ranges, splitting ranges where it changes. Nothing else is
    /// touched: a hole, a missing or a compressed range has no place on the source to be bad.
    pub fn mark_bad(&mut self, bad: &[(u64, u64)]) {
        if bad.is_empty() {
            return;
        }
        let mut marked: Vec<Range> = Vec::new();
        for range in &self.ranges {
            let (RangeKind::Readable | RangeKind::Shared, Some(Provenance::Source { offset, .. })) =
                (range.kind, range.provenance)
            else {
                marked.push(*range);
                continue;
            };
            let end = offset + range.length;
            let mut at = offset;
            // The bad ranges that touch this one, in order.
            let mut touching: Vec<(u64, u64)> = bad
                .iter()
                .copied()
                .filter(|(from, to)| *from < end && *to > offset)
                .map(|(from, to)| (from.max(offset), to.min(end)))
                .collect();
            touching.sort_unstable();
            let mut pieces: Vec<(u64, u64, bool)> = Vec::new();
            for (from, to) in touching {
                if to <= at {
                    continue;
                }
                let from = from.max(at);
                if from > at {
                    pieces.push((at, from, false));
                }
                pieces.push((from, to, true));
                at = to;
            }
            if at < end {
                pieces.push((at, end, false));
            }
            for (from, to, is_bad) in pieces {
                push_merged(
                    &mut marked,
                    Range {
                        start: range.start + (from - offset),
                        length: to - from,
                        kind: if is_bad {
                            RangeKind::BadSector
                        } else {
                            range.kind
                        },
                        provenance: Some(Provenance::Source {
                            offset: from,
                            length: to - from,
                        }),
                    },
                );
            }
        }
        self.ranges = marked;
    }

    /// The stretches of `start..end` of the stream that are bad sectors, as `(start, end)`.
    pub fn bad_sector_ranges(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        let mut found: Vec<(u64, u64)> = Vec::new();
        for range in self
            .ranges
            .iter()
            .filter(|range| range.kind == RangeKind::BadSector)
        {
            let (from, to) = (range.start.max(start), range.end().min(end));
            if from >= to {
                continue;
            }
            match found.last_mut() {
                Some(last) if last.1 == from => last.1 = to,
                _ => found.push((from, to)),
            }
        }
        found
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

    /// The ranges of a stream held in an explicit list of clusters (the list the map keeps for an entry):
    /// clusters that lie side by side are one readable range, a cluster outside the volume is
    /// `Missing`, and bytes past `valid_length` are `Sparse`. `length` is the stream's size, at most
    /// what the clusters hold; the list may be longer than the stream needs, and the extra is ignored.
    /// The ranges of a driver's candidate from the walk its driver made of the storage the candidate claims: the stored bytes
    /// of a run are `Readable`, a run the format stores encoded (compressed, or a window of a block that files share) is
    /// `Compressed` and is decoded by the driver, a run the format allocated but did not write and every hole are `Sparse`,
    /// and bytes the walk did not reach are `Missing` (zeros past a complete walk's end are `Sparse`). Nothing is
    /// assumed contiguous, and nothing is a cluster.
    pub fn from_walk(walk: &fs_core_layers::layers::ExtentWalk) -> Self {
        use fs_core_layers::layers::WalkEnd;
        let mut ranges: Vec<Range> = Vec::new();
        let mut at = 0u64;
        let mut push =
            |start: u64, length: u64, kind: RangeKind, provenance: Option<Provenance>| {
                if length > 0 {
                    ranges.push(Range {
                        start,
                        length,
                        kind,
                        provenance,
                    });
                }
            };
        for extent in &walk.extents {
            let length = extent.length();
            if length == 0 || extent.offset < at {
                // An empty run, or one that overlaps the one before: the walk is not trusted past it.
                continue;
            }
            push(at, extent.offset - at, RangeKind::Sparse, None);
            let place = Some(Provenance::Source {
                offset: extent.source.offset,
                length: extent.source.length,
            });
            if extent.uninitialized {
                push(extent.offset, length, RangeKind::Sparse, None);
            } else if extent.encoded.is_some() {
                push(extent.offset, length, RangeKind::Compressed, place);
            } else {
                push(extent.offset, length, RangeKind::Readable, place);
            }
            at = extent.offset + length;
        }
        let kind = if walk.end == WalkEnd::Complete {
            RangeKind::Sparse
        } else {
            RangeKind::Missing
        };
        push(at, walk.claimed.saturating_sub(at), kind, None);
        let size = ranges.last().map_or(0, Range::end);
        Self::new(ranges, size).unwrap_or_default()
    }

    pub fn from_clusters(
        geometry: &VolumeGeometry,
        clusters: &[u32],
        length: u64,
        valid_length: u64,
    ) -> Self {
        let size = geometry.cluster_size();
        let mut found: Vec<Range> = Vec::new();
        let mut claimed = 0u64;
        for &cluster in clusters {
            if claimed >= length {
                break;
            }
            let bytes = size.min(length - claimed);
            let range = if valid_cluster(geometry, cluster) {
                Range {
                    start: claimed,
                    length: bytes,
                    kind: RangeKind::Readable,
                    provenance: Some(Provenance::Source {
                        offset: allocation::cluster_offset(geometry, cluster),
                        length: bytes,
                    }),
                }
            } else {
                missing(claimed, bytes)
            };
            push_merged(&mut found, range);
            claimed += bytes;
        }
        if claimed < length {
            push_merged(&mut found, missing(claimed, length - claimed));
        }
        Self { ranges: found }.unwritten_from(valid_length.min(length))
    }

    /// Reads bytes `start` to `end` of the stream (`end` is not read, and is at most the stream's size).
    /// `read_source(offset, length)` reads bytes of the source, or none when it cannot.
    ///
    /// A `Readable` or `Shared` range is read from its place on the source in one piece; when that
    /// fails it is read again in whole units of `unit` bytes (a cluster, from the start of the range),
    /// so that one bad cluster costs only itself. A `Sparse` range is zeros and is not read. A range of
    /// any other kind, and a piece that cannot be read, is zeros too, and is returned among the
    /// stretches (as `(start, end)` in bytes from the start of the stream) that are not the stream's
    /// bytes: the caller shows those as missing, never as data.
    pub fn read(
        &self,
        start: u64,
        end: u64,
        unit: u64,
        read_source: &mut dyn FnMut(u64, u64) -> Option<Vec<u8>>,
    ) -> (Vec<u8>, Vec<(u64, u64)>) {
        let mut data = Vec::with_capacity(end.saturating_sub(start) as usize);
        let mut unreadable: Vec<(u64, u64)> = Vec::new();
        let mut note = |from: u64, to: u64| match unreadable.last_mut() {
            Some(last) if last.1 == from => last.1 = to,
            _ => unreadable.push((from, to)),
        };
        for range in &self.ranges {
            let from = range.start.max(start);
            let to = range.end().min(end);
            if from >= to {
                continue;
            }
            match (range.kind, range.provenance) {
                (RangeKind::Sparse, _) => data.resize(data.len() + (to - from) as usize, 0),
                (
                    RangeKind::Readable | RangeKind::Shared,
                    Some(Provenance::Source { offset, .. }),
                ) => {
                    let place = |at: u64| offset + (at - range.start);
                    match read_source(place(from), to - from) {
                        Some(bytes) if bytes.len() as u64 == to - from => {
                            data.extend_from_slice(&bytes)
                        }
                        _ => {
                            // Whole units, as a bad cluster fails as a whole, whatever part of it was asked for.
                            let mut at = from;
                            while at < to {
                                let first = range.start + (at - range.start) / unit * unit;
                                let next = (first + unit).min(to);
                                match read_source(place(first), unit) {
                                    Some(bytes) if bytes.len() as u64 == unit => {
                                        data.extend_from_slice(
                                            &bytes[(at - first) as usize..(next - first) as usize],
                                        );
                                    }
                                    _ => {
                                        data.resize(data.len() + (next - at) as usize, 0);
                                        note(at, next);
                                    }
                                }
                                at = next;
                            }
                        }
                    }
                }
                _ => {
                    data.resize(data.len() + (to - from) as usize, 0);
                    note(from, to);
                }
            }
        }
        (data, unreadable)
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
        entry: &RecordEntry,
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
    fn bytes_on_a_bad_sector_are_split_out_marked_and_never_read() {
        let mut list = RangeList::new(
            vec![
                range(0, 300, RangeKind::Readable, Some(1000)),
                range(300, 50, RangeKind::Sparse, None),
                range(350, 100, RangeKind::Shared, Some(5000)),
                range(450, 10, RangeKind::Missing, None),
            ],
            460,
        )
        .unwrap();
        // Source bytes 1100 to 1200 are bad, and 5050 to 5400 (which runs past the range that holds 5000 to 5100).
        list.mark_bad(&[(5050, 5400), (1100, 1200)]);
        assert_eq!(
            list.ranges(),
            [
                range(0, 100, RangeKind::Readable, Some(1000)),
                range(100, 100, RangeKind::BadSector, Some(1100)),
                range(200, 100, RangeKind::Readable, Some(1200)),
                range(300, 50, RangeKind::Sparse, None),
                range(350, 50, RangeKind::Shared, Some(5000)),
                range(400, 50, RangeKind::BadSector, Some(5050)),
                range(450, 10, RangeKind::Missing, None),
            ]
        );
        assert_eq!(list.size(), 460);
        assert_eq!(list.bytes_of(RangeKind::BadSector), 150);
        assert_eq!(list.bad_sector_ranges(0, 460), vec![(100, 200), (400, 450)]);
        assert_eq!(
            list.bad_sector_ranges(150, 410),
            vec![(150, 200), (400, 410)]
        );
        // Reading: the bad bytes are zeros and are reported as not the stream's, and the source is not asked for them.
        let asked = std::cell::RefCell::new(Vec::new());
        let (data, unreadable) = list.read(80, 220, 10, &mut |offset, length| {
            asked.borrow_mut().push((offset, length));
            Some(vec![7; length as usize])
        });
        assert_eq!(unreadable, vec![(100, 200)]);
        assert!(data[..20].iter().all(|byte| *byte == 7));
        assert!(data[20..120].iter().all(|byte| *byte == 0));
        assert!(data[120..].iter().all(|byte| *byte == 7));
        assert!(
            asked
                .borrow()
                .iter()
                .all(|(offset, length)| offset + length <= 1100 || *offset >= 1200),
            "{asked:?}"
        );
        // Nothing bad: nothing changes.
        let before = list.clone();
        list.mark_bad(&[]);
        assert_eq!(list, before);
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

    fn ntfs(storage: &str, runs: Vec<crate::filesystem::NtfsRun>) -> RecordEntry {
        RecordEntry {
            attribute_list: false,
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

    /// A source that is the offset itself, byte for byte, except where it is told it cannot read.
    fn source(bad: Option<std::ops::Range<u64>>) -> impl FnMut(u64, u64) -> Option<Vec<u8>> {
        move |offset, length| {
            if bad
                .iter()
                .any(|bad| offset < bad.end && offset + length > bad.start)
            {
                return None;
            }
            Some(
                (offset..offset + length)
                    .map(|at| (at % 251) as u8)
                    .collect(),
            )
        }
    }

    #[test]
    fn clusters_that_follow_each_other_are_one_range_and_one_outside_the_volume_is_missing() {
        let geometry = geometry(512);
        // Clusters 2, 3, then 9 (apart), then one outside the volume, for 4 x 512 bytes, 1600 valid.
        let list = RangeList::from_clusters(&geometry, &[2, 3, 9, 5000], 2048, 1600);
        let found: Vec<(u64, u64, RangeKind)> = list
            .ranges()
            .iter()
            .map(|range| (range.start, range.length, range.kind))
            .collect();
        assert_eq!(
            found,
            [
                (0, 1024, RangeKind::Readable),
                (1024, 512, RangeKind::Readable),
                (1536, 64, RangeKind::Missing),
                (1600, 448, RangeKind::Sparse),
            ]
        );
        assert_eq!(list.size(), 2048);
        // A list longer than the stream is cut at its size, and a shorter one leaves the rest missing.
        assert_eq!(
            RangeList::from_clusters(&geometry, &[2, 3, 4], 700, 700).size(),
            700
        );
        let short = RangeList::from_clusters(&geometry, &[2], 1000, 1000);
        assert_eq!(short.bytes_of(RangeKind::Missing), 488);
        assert!(RangeList::from_clusters(&geometry, &[], 0, 0).is_empty());
    }

    #[test]
    fn a_read_takes_readable_bytes_from_the_source_and_zeros_for_a_hole_and_names_the_rest() {
        let geometry = geometry(512);
        let list = RangeList::from_clusters(&geometry, &[2, 3, 9, 5000], 2048, 1600);
        let place = |cluster: u64| (cluster - 2) * 512;
        // Across the join of two ranges, into the missing stretch and the zeros past the valid length.
        let (data, unreadable) = list.read(1000, 1700, 512, &mut source(None));
        assert_eq!(data.len(), 700);
        assert_eq!(data[0], ((place(2) + 1000) % 251) as u8);
        assert_eq!(
            data[24],
            ((place(9)) % 251) as u8,
            "the second range starts at byte 1024"
        );
        assert_eq!(
            unreadable,
            [(1536, 1600)],
            "the cluster outside the volume is named"
        );
        assert!(
            data[600..].iter().all(|byte| *byte == 0),
            "bytes past the valid length are zeros"
        );
        // The whole of the first range cannot be read in one piece: only the bad cluster is lost.
        let (data, unreadable) = list.read(0, 1024, 512, &mut source(Some(0..512)));
        assert_eq!(unreadable, [(0, 512)]);
        assert!(data[..512].iter().all(|byte| *byte == 0));
        assert_eq!(
            data[512],
            ((place(2) + 512) % 251) as u8,
            "the cluster beside it is read"
        );
        // Nothing is read for a stretch that is only zeros.
        let (_, unreadable) = list.read(1700, 2000, 512, &mut |_, _| panic!("a hole was read"));
        assert!(unreadable.is_empty());
    }

    /// One run of a walk: file offset, source offset, length, whether it is unwritten, and (decoded size, run length) when encoded.
    type Run = (u64, u64, u64, bool, Option<(u64, u64)>);

    fn walk_of(
        extents: Vec<Run>,
        claimed: u64,
        end: fs_core_layers::layers::WalkEnd,
    ) -> fs_core_layers::layers::ExtentWalk {
        use fs_core_layers::layers::{Encoding, Extent, ExtentWalk, WalkExtent};
        ExtentWalk {
            extents: extents
                .into_iter()
                .map(
                    |(offset, source, length, uninitialized, encoded)| WalkExtent {
                        offset,
                        source: Extent::new(source, length),
                        uninitialized,
                        encoded: encoded.map(|(decoded, run)| Encoding {
                            codec: "gzip".into(),
                            decoded,
                            offset: 0,
                            length: run,
                        }),
                    },
                )
                .collect(),
            claimed,
            end,
        }
    }

    #[test]
    fn a_driver_walk_becomes_readable_compressed_and_sparse_ranges_with_holes_between() {
        use fs_core_layers::layers::WalkEnd;
        // 0..100 stored as it is, a hole to 300, 300..400 stored encoded (60 stored bytes decoding to the run), 400..450 not
        // written, and a claim of 600: the end of a complete walk is zeros.
        let walk = walk_of(
            vec![
                (0, 5000, 100, false, None),
                (300, 9000, 60, false, Some((100, 100))),
                (400, 0, 50, true, None),
            ],
            600,
            WalkEnd::Complete,
        );
        let list = RangeList::from_walk(&walk);
        let found: Vec<(u64, u64, RangeKind)> = list
            .ranges()
            .iter()
            .map(|range| (range.start, range.length, range.kind))
            .collect();
        assert_eq!(
            found,
            [
                (0, 100, RangeKind::Readable),
                (100, 200, RangeKind::Sparse),
                (300, 100, RangeKind::Compressed),
                (400, 50, RangeKind::Sparse),
                (450, 150, RangeKind::Sparse),
            ]
        );
        assert_eq!(list.size(), 600);
        assert_eq!(
            list.ranges()[0].provenance,
            Some(Provenance::Source {
                offset: 5000,
                length: 100
            })
        );
    }

    #[test]
    fn what_a_walk_did_not_reach_is_missing_and_a_walk_of_nothing_is_empty() {
        use fs_core_layers::layers::WalkEnd;
        let walk = walk_of(vec![(0, 0, 200, false, None)], 1000, WalkEnd::Broken);
        let list = RangeList::from_walk(&walk);
        assert_eq!(list.bytes_of(RangeKind::Readable), 200);
        assert_eq!(list.bytes_of(RangeKind::Missing), 800);
        assert!(RangeList::from_walk(&walk_of(Vec::new(), 0, WalkEnd::Complete)).is_empty());
        // A run that overlaps the one before is not trusted.
        let overlapping = walk_of(
            vec![(0, 0, 100, false, None), (50, 100, 100, false, None)],
            100,
            WalkEnd::Complete,
        );
        assert_eq!(RangeList::from_walk(&overlapping).size(), 100);
    }
}
