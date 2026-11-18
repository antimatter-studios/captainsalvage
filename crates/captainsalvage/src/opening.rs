//! What opening a source finds out before it reads anything as a filesystem: whether the bytes
//! start with a volume (a partition) or not (a whole disk), what filesystem the boot evidence
//! says, and the interpretations a person can switch between.
//!
//! One door opens every device, partition and image (`open_source`, in the engine). It always
//! starts from the raw source, and applies an interpretation to it: the one stored for the last
//! scan of a matching source, otherwise the best detection. Nothing here writes to a source or
//! reads file contents; detection reads the first mebibyte, in bounded steps.

use crate::catalog::{InterpretationRecord, ScanRecord};
use crate::disk::RawDisk;
use crate::range_analysis::{self, Options, Status};
use anyhow::Result;
use serde::Serialize;

/// How much of the start of a source detection reads.
const HEAD_BYTES: u64 = 1024 * 1024;
/// Boot sectors sit on sector boundaries, so detection tries each 512 bytes.
const HEAD_STEP: u64 = 512;

/// The choices of the interpretation picker, in the order it lists them.
pub const FAMILIES: [&str; 4] = ["exFAT", "FAT", "NTFS", "none"];

/// The picker choice a format belongs to: FAT12, FAT16 and FAT32 are one choice, `FAT`.
pub fn family(format: &str) -> &'static str {
    let lower = format.to_ascii_lowercase();
    if lower == "exfat" {
        "exFAT"
    } else if lower.starts_with("fat") {
        "FAT"
    } else if lower == "ntfs" {
        "NTFS"
    } else {
        "none"
    }
}

/// A reading of an extent that could be opened: a format at an offset, with how sure we are.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub format: String,
    /// Relative to the extent being opened.
    pub start: u64,
    pub length: u64,
    pub boot_offset: Option<u64>,
    pub confidence: f64,
    /// `boot` when the start of the extent decodes as this format, `saved` when an earlier
    /// interpretation or a hypothesis says so.
    pub origin: String,
}

/// What the start of a source says.
#[derive(Clone, Debug, Default)]
pub struct Detection {
    /// Something that looks like a volume starts at byte zero, whether or not it can be opened.
    /// A source that does is one partition. One that does not is a whole disk.
    pub volume_at_start: bool,
    pub candidates: Vec<Candidate>,
}

/// Reads the start of `view` and says what volume, if any, begins there.
pub fn detect(view: &RawDisk) -> Result<Detection> {
    let window = view.length.min(HEAD_BYTES);
    let report = range_analysis::analyze(
        view,
        0,
        window,
        &Options {
            step: HEAD_STEP,
            per_format: 2,
            ..Options::default()
        },
        &mut |_, _| true,
    )?;
    let mut detection = Detection::default();
    for finding in &report.findings {
        if finding.volume_start != Some(0) {
            continue;
        }
        detection.volume_at_start = true;
        // Geometry worked out from other structures has no boot sector the volume can be read
        // through, so it is evidence and not something to open from here.
        if finding.status != Status::Volume || !finding.supported {
            continue;
        }
        detection.candidates.push(Candidate {
            format: finding.format.clone(),
            start: 0,
            length: view.length,
            boot_offset: finding.boot_offset.filter(|offset| *offset != 0),
            confidence: finding.confidence,
            origin: "boot".into(),
        });
    }
    detection.candidates.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(detection)
}

/// One entry of the picker.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Choice {
    /// `exFAT`, `FAT`, `NTFS` or `none`.
    pub id: String,
    /// What it opens as: FAT is the width the boot sector says.
    pub format: String,
    /// Something to open: boot evidence or a saved interpretation. `none` always is.
    pub available: bool,
    /// The best detection says this one.
    pub detected: bool,
    pub confidence: Option<f64>,
    /// A scan is stored for it, so choosing it reopens that scan without scanning.
    pub stored: bool,
    /// What the interpretation is applied to now.
    pub current: bool,
    pub origin: Option<String>,
    pub start: u64,
    pub length: u64,
    pub boot_offset: Option<u64>,
}

/// The candidate for one choice: the boot evidence if there is some, otherwise the most
/// recently saved interpretation of that format.
pub fn candidate_for(
    id: &str,
    detection: &Detection,
    saved: &[InterpretationRecord],
    inside: &[Candidate],
) -> Option<Candidate> {
    if let Some(found) = detection
        .candidates
        .iter()
        .find(|candidate| family(&candidate.format) == id)
    {
        return Some(found.clone());
    }
    inside
        .iter()
        .find(|candidate| family(&candidate.format) == id)
        .cloned()
        .or_else(|| {
            saved
                .iter()
                .filter(|record| family(&record.format) == id && record.verified)
                .max_by_key(|record| record.listed)
                .map(|record| Candidate {
                    format: record.format.clone(),
                    start: record.start,
                    length: record.length,
                    boot_offset: record.boot_offset,
                    confidence: record.confidence,
                    origin: "saved".into(),
                })
        })
}

/// The picker's entries for an extent: the three filesystems and raw bytes, each with whether it
/// can be opened, whether the detection says so, and whether a scan is already stored for it.
pub fn choices(
    detection: &Detection,
    saved: &[InterpretationRecord],
    inside: &[Candidate],
    scans: &[ScanRecord],
    current: Option<&Candidate>,
) -> Vec<Choice> {
    let best = detection.candidates.first();
    FAMILIES
        .iter()
        .map(|id| {
            if *id == "none" {
                return Choice {
                    id: (*id).into(),
                    format: "none".into(),
                    available: true,
                    detected: false,
                    confidence: None,
                    stored: scans.iter().any(|scan| scan.format == "none"),
                    current: current.is_none(),
                    origin: None,
                    start: 0,
                    length: 0,
                    boot_offset: None,
                };
            }
            let candidate = candidate_for(id, detection, saved, inside);
            let stored = candidate.as_ref().is_some_and(|candidate| {
                scans.iter().any(|scan| {
                    scan.format == candidate.format
                        && scan.start == candidate.start
                        && scan.length == candidate.length
                        && scan.boot_offset == candidate.boot_offset
                })
            });
            Choice {
                id: (*id).into(),
                format: candidate
                    .as_ref()
                    .map_or_else(|| (*id).to_string(), |candidate| candidate.format.clone()),
                available: candidate.is_some(),
                detected: best.is_some_and(|best| family(&best.format) == *id),
                confidence: candidate.as_ref().map(|candidate| candidate.confidence),
                stored,
                current: current.is_some_and(|now| family(&now.format) == *id),
                origin: candidate.as_ref().map(|candidate| candidate.origin.clone()),
                start: candidate.as_ref().map_or(0, |candidate| candidate.start),
                length: candidate.as_ref().map_or(0, |candidate| candidate.length),
                boot_offset: candidate.and_then(|candidate| candidate.boot_offset),
            }
        })
        .collect()
}
