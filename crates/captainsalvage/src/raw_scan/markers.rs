//! Source markers: how a drive is recognised again.
//!
//! A damaged drive degrades, and mounting it changes it, so one changed block must not make it a
//! different drive. A fingerprint is a set of markers (4 KiB blocks, each with its own SHA-256),
//! spread over the whole source. Opening a source compares each saved marker on its own and
//! reports how many match, how many changed and how many could not be read. The result is a
//! confidence, not a yes or no.
//!
//! Version 1 fingerprints (16 stored samples) still load and compare marker by marker over their
//! own count. Version 2 changes how many markers there are and how they are chosen.

use crate::disk::RawDisk;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

pub(super) const FINGERPRINT_VERSION: u32 = 2;
/// Every marker is one block this long (a shorter source gets one shorter marker).
pub const MARKER_BYTES: u64 = 4096;
/// How many markers a source is given when it can hold that many useful blocks. Twenty makes
/// each marker exactly 5 percent.
pub const MARKER_COUNT: u64 = 20;
/// A match needs at least this many markers to agree before it is reported at all (40 percent of
/// 20). Two drives made from the same template can share a few blocks, such as the boot area,
/// and a drive that lost most of its readable markers proves nothing. Sources with fewer markers
/// than this need all of them to agree.
pub const MIN_MATCHED_MARKERS: usize = 8;
/// From this share of markers matching (12 of 20) a match is `probable`. Below it, `weak`.
pub const PROBABLE_PERCENT: usize = 60;

/// A marker that cannot be read is replaced by a nearby block, at most this many times.
const SUBSTITUTE_TRIES: u32 = 12;
/// A block that repeats with a period up to this many bytes cannot tell one drive from another.
const MAX_PATTERN_BYTES: usize = 64;
/// Changed markers this close to the start are probably a mount rewriting metadata.
const START_REGION_BYTES: u64 = 1024 * 1024;
/// Marker reads stop starting new reads after this long, so a failing drive is not hammered.
const READ_BUDGET: Duration = Duration::from_secs(20);
/// Marker reads stop after this many failed reads in a row, for the same reason. Markers are
/// spread over the source, so a bad region fails one or two of them and a dead drive fails all.
const MAX_FAILED_READS_IN_A_ROW: u32 = 5;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    pub offset: u64,
    pub length: u64,
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fingerprint {
    pub version: u32,
    pub length: u64,
    /// The markers: a block's offset, length and SHA-256.
    pub samples: Vec<Sample>,
    /// One hash over all markers, kept so a fingerprint can be shown and compared as a whole.
    /// It no longer decides a match.
    pub sampled_sha256: Option<String>,
    /// How many markers were wanted. Fewer in `samples` means the source is small or mostly
    /// blank. Absent in version 1 fingerprints, which wanted what they stored.
    #[serde(default)]
    pub markers_wanted: u32,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Verification {
    /// Every marker matches.
    Exact,
    /// At least 60 percent of the markers match; the rest changed or could not be read.
    Probable,
    /// Enough markers match to be worth showing, but under 60 percent.
    Weak,
    /// No match, too few matching markers, or a different length.
    Different,
    /// The scanner or the fingerprint was made by a version this one cannot compare.
    Incompatible,
}

/// The result of comparing a saved fingerprint with the source that is open now.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Comparison {
    pub state: Verification,
    pub total: usize,
    pub matched: usize,
    pub changed: usize,
    /// Markers that could not be read now, or had no saved hash. They are unknown, not changed.
    pub unreadable: usize,
    /// Matched markers out of all markers, rounded down, so only an exact match reads 100.
    pub confidence_percent: u32,
    pub changed_offsets: Vec<u64>,
    pub unreadable_offsets: Vec<u64>,
    /// A sentence or two saying what changed and where. English, sentence by sentence, so the
    /// window can translate it (see src/engineText.ts).
    pub summary: String,
}

/// Reads markers from one source for one operation. Each block is read at most once, whatever
/// number of saved fingerprints ask for it, and reading stops when the drive has gone, a time
/// limit is passed or too many reads in a row have failed.
pub struct MarkerReader<'a> {
    disk: &'a RawDisk,
    cache: HashMap<(u64, u64), Option<String>>,
    started: Instant,
    failures: u32,
}

impl<'a> MarkerReader<'a> {
    pub fn new(disk: &'a RawDisk) -> Self {
        Self {
            disk,
            cache: HashMap::new(),
            started: Instant::now(),
            failures: 0,
        }
    }

    fn may_read(&self) -> bool {
        self.failures < MAX_FAILED_READS_IN_A_ROW
            && self.started.elapsed() < READ_BUDGET
            && !self.disk.source.is_gone()
    }

    fn block(&mut self, offset: u64, length: u64) -> Option<Vec<u8>> {
        if !self.may_read() {
            return None;
        }
        match self.disk.read(offset, length) {
            Ok(data) => {
                self.failures = 0;
                self.cache.insert((offset, length), Some(hash(&data)));
                Some(data)
            }
            Err(_) => {
                self.failures += 1;
                self.cache.insert((offset, length), None);
                None
            }
        }
    }

    fn hash(&mut self, offset: u64, length: u64) -> Option<String> {
        if let Some(known) = self.cache.get(&(offset, length)) {
            return known.clone();
        }
        self.block(offset, length).map(|data| hash(&data))
    }
}

impl Fingerprint {
    pub fn read(disk: &RawDisk) -> Self {
        Self::read_with(&mut MarkerReader::new(disk))
    }

    /// Chooses the markers for this source and reads them, each block once.
    ///
    /// The nominal offsets depend only on the source length: evenly spread, with the first and
    /// the last block among them. A nominal block that is blank, repeats a short pattern or
    /// cannot be read says nothing about which drive this is, so a nearby block takes its place:
    /// the next ones at growing distances, never touching another marker's block. The same
    /// content therefore gives the same offsets.
    pub fn read_with(reader: &mut MarkerReader) -> Self {
        let length = reader.disk.length;
        let size = length.min(MARKER_BYTES);
        let targets = nominal_offsets(length);
        let mut samples: Vec<Sample> = Vec::new();
        for index in 0..targets.len() {
            for offset in candidates(&targets, index, size) {
                if samples
                    .iter()
                    .any(|taken| taken.offset.abs_diff(offset) < size)
                {
                    continue;
                }
                if let Some(data) = reader.block(offset, size) {
                    if identifies(&data) {
                        samples.push(Sample {
                            offset,
                            length: size,
                            sha256: Some(hash(&data)),
                        });
                        break;
                    }
                }
                if !reader.may_read() {
                    break;
                }
            }
        }
        samples.sort_by_key(|sample| sample.offset);
        let sampled_sha256 = (!samples.is_empty()).then(|| {
            let mut digest = Sha256::new();
            digest.update(b"Captain Salvage sampled disk fingerprint\0");
            digest.update(FINGERPRINT_VERSION.to_le_bytes());
            digest.update(length.to_le_bytes());
            for sample in &samples {
                digest.update(sample.offset.to_le_bytes());
                digest.update(sample.length.to_le_bytes());
                digest.update(sample.sha256.as_deref().unwrap_or("").as_bytes());
            }
            hex(&digest.finalize())
        });
        Self {
            version: FINGERPRINT_VERSION,
            length,
            samples,
            sampled_sha256,
            markers_wanted: MARKER_COUNT as u32,
        }
    }

    /// Fewer markers than wanted: the source is small, or most of it is blank.
    pub fn is_short(&self) -> bool {
        self.version >= 2 && (self.samples.len() as u64) < u64::from(self.markers_wanted)
    }

    /// Compares each saved marker on its own with the same block of the source that is open now.
    pub fn compare(&self, reader: &mut MarkerReader) -> Comparison {
        let total = self.samples.len();
        let incomplete = |state, summary: &str| Comparison {
            state,
            total,
            matched: 0,
            changed: 0,
            unreadable: 0,
            confidence_percent: 0,
            changed_offsets: Vec::new(),
            unreadable_offsets: Vec::new(),
            summary: summary.into(),
        };
        if !(1..=FINGERPRINT_VERSION).contains(&self.version) {
            return incomplete(
                Verification::Incompatible,
                "The saved fingerprint was made by a newer version.",
            );
        }
        if self.length != reader.disk.length {
            return incomplete(Verification::Different, "The source has a different size.");
        }
        if total == 0 {
            return incomplete(
                Verification::Different,
                "The saved fingerprint has no markers.",
            );
        }
        let mut matched = 0;
        let mut changed_offsets = Vec::new();
        let mut unreadable_offsets = Vec::new();
        for sample in &self.samples {
            let now = sample
                .sha256
                .as_ref()
                .and_then(|_| reader.hash(sample.offset, sample.length));
            match (&sample.sha256, now) {
                (Some(saved), Some(now)) if *saved == now => matched += 1,
                (Some(_), Some(_)) => changed_offsets.push(sample.offset),
                _ => unreadable_offsets.push(sample.offset),
            }
        }
        let changed = changed_offsets.len();
        let unreadable = unreadable_offsets.len();
        let confidence_percent = (matched * 100 / total) as u32;
        let state = if matched == 0 || matched < MIN_MATCHED_MARKERS.min(total) {
            Verification::Different
        } else if matched == total {
            Verification::Exact
        } else if matched * 100 >= PROBABLE_PERCENT * total {
            Verification::Probable
        } else {
            Verification::Weak
        };
        let summary = summarise(self, state, total, matched, &changed_offsets, unreadable);
        Comparison {
            state,
            total,
            matched,
            changed,
            unreadable,
            confidence_percent,
            changed_offsets,
            unreadable_offsets,
            summary,
        }
    }
}

fn summarise(
    saved: &Fingerprint,
    state: Verification,
    total: usize,
    matched: usize,
    changed_offsets: &[u64],
    unreadable: usize,
) -> String {
    let changed = changed_offsets.len();
    let mut sentences: Vec<String> = Vec::new();
    if state == Verification::Exact {
        sentences.push(format!("All {total} markers match."));
    }
    if changed > 0 {
        sentences.push(format!("{changed} of {total} markers changed."));
        let near = changed_offsets
            .iter()
            .filter(|offset| **offset < START_REGION_BYTES)
            .count();
        sentences.push(if near == changed {
            "Every changed marker is within the first 1 MiB, so a mount probably wrote metadata."
                .into()
        } else if near > 0 {
            format!("Of the changed markers, {near} are within the first 1 MiB, so a mount probably wrote metadata, and the rest are further into the drive.")
        } else {
            "The changed markers are further into the drive, so its data may have changed.".into()
        });
    }
    if unreadable > 0 {
        sentences.push(format!(
            "Of the {total} markers, {unreadable} could not be read."
        ));
    }
    if state == Verification::Different {
        sentences.push(if matched == 0 {
            "No marker matches.".into()
        } else {
            format!("Only {matched} of {total} markers match, too few to identify this drive.")
        });
    }
    if saved.is_short() {
        let wanted = saved.markers_wanted;
        sentences.push(format!(
            "Markers used: {total} instead of {wanted}, because the source is small or mostly blank."
        ));
    }
    sentences.join(" ")
}

/// Evenly spread block offsets over the source, with the first and the last block among them.
fn nominal_offsets(length: u64) -> Vec<u64> {
    let size = length.min(MARKER_BYTES);
    let last = length - size;
    (0..MARKER_COUNT)
        .map(|index| {
            if index == MARKER_COUNT - 1 {
                last
            } else {
                (u128::from(last) * u128::from(index) / u128::from(MARKER_COUNT - 1)) as u64
                    / MARKER_BYTES
                    * MARKER_BYTES
            }
        })
        .collect::<BTreeSet<u64>>()
        .into_iter()
        .collect()
}

/// The blocks to try for one marker, in order: its nominal block, then blocks at growing
/// distances toward the middle of its stretch of the source (backward for the last marker), and
/// never into the next marker's nominal block.
fn candidates(targets: &[u64], index: usize, size: u64) -> Vec<u64> {
    let target = targets[index];
    let mut found = vec![target];
    let last = index + 1 == targets.len();
    for step in 0..SUBSTITUTE_TRIES {
        let distance = size << step;
        let offset = if last {
            let floor = index
                .checked_sub(1)
                .map_or(0, |previous| targets[previous] + size);
            target
                .checked_sub(distance)
                .filter(|offset| *offset >= floor)
        } else {
            let ceiling = targets[index + 1].saturating_sub(size);
            target
                .checked_add(distance)
                .filter(|offset| *offset <= ceiling)
        };
        match offset {
            Some(offset) => found.push(offset),
            None => break,
        }
    }
    found
}

/// Whether a block can tell one drive from another: not blank, and not a short repeating pattern.
pub(super) fn identifies(block: &[u8]) -> bool {
    block.len() >= 16
        && !(1..=MAX_PATTERN_BYTES.min(block.len() / 4))
            .any(|period| block[period..] == block[..block.len() - period])
}

pub(super) fn hash(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub(super) fn hex(data: &[u8]) -> String {
    data.iter().map(|byte| format!("{byte:02x}")).collect()
}
