//! Analyzing a selected byte range: what could it be, and how much of it decodes?
//!
//! The range is a window onto the source. Every filesystem driver the app knows is asked, at
//! every start offset the slide tries, how much of the window decodes from there. The report
//! gives, per driver, the offsets where the most decodes, and a list of findings with the
//! format, where it starts, how much decoded and how sure the app is.
//!
//! Signature probes (boot sectors, the `FILE` magic) are the cheap early rejection that keeps
//! the slide fast, and they raise confidence. They are not required: with them off, the
//! structural decoders still find directory entries and MFT records by their shape. Data that is
//! identified but cannot be decoded into a volume stays an identified island, with an offer to
//! search for the metadata that is missing.
//!
//! Nothing here writes. The source is only ever read, through the same read-only handle every
//! other raw operation uses.

pub mod geometry;
pub mod probes;

use crate::disk::{DiskRegion, RawDisk, ReadFailure};
use crate::filesystem::driver::Driver;
use crate::filesystem::ntfs_salvage::SourceDevice;
use anyhow::{ensure, Result};
use probes::{Decoded, FormatProbe, Kind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

/// The most one analysis reads. A larger range is analysed in pieces.
pub const MAX_RANGE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Ranges up to this size slide one byte at a time; larger ones slide by `LARGE_RANGE_STEP`.
const FINE_RANGE_BYTES: u64 = 128 * 1024 * 1024;
const LARGE_RANGE_STEP: u64 = 32;
const CHUNK_BYTES: u64 = 4 * 1024 * 1024;
/// How far past a chunk a run of structures is followed while sliding.
const OVERLAP_BYTES: u64 = 1024 * 1024;
const READ_PIECE_BYTES: u64 = 64 * 1024;
/// How far a found run is measured again, once the slide is over.
const REMEASURE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_STEP: u64 = 1024 * 1024;
const MAX_CANDIDATES: usize = 100_000;
/// How far either side of an island an offered search looks for the metadata it lacks.
const SEARCH_MARGIN_BYTES: u64 = 256 * 1024 * 1024;
const BOOT_BYTES: u64 = 512;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Options {
    /// How far the start offset moves each time, in bytes, counted from byte zero of the source
    /// so structures aligned in the source are met wherever the range starts. Zero chooses 1 for
    /// ranges up to 128 MiB and 32 beyond.
    pub step: u64,
    /// Use the signature probes. False makes the slide rely on structure alone.
    pub signatures: bool,
    /// How many findings to keep for each driver and kind.
    pub per_format: usize,
    /// Only these drivers (exFAT, FAT, NTFS, BitLocker). Empty means all of them.
    pub formats: Vec<String>,
    /// Try to work the volume's geometry out from the structures found.
    pub geometry: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            step: 0,
            signatures: true,
            per_format: 5,
            formats: Vec::new(),
            geometry: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// A boot sector decoded and the volume opens and lists its root.
    Volume,
    /// The geometry was worked out from structures, and the volume opens with a boot sector
    /// rebuilt in memory.
    Inferred,
    /// Metadata that belongs to a volume found elsewhere in the report.
    Structures,
    /// Identified, but nothing places it in a volume: no boot sector, no geometry, or damaged.
    Island,
}

/// What to search for when an island lacks its metadata.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataOffer {
    pub formats: Vec<String>,
    pub looks_for: Vec<String>,
    pub search_start: u64,
    pub search_length: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub format: String,
    pub structure: String,
    pub status: Status,
    /// Where the decoded bytes start in the source.
    pub offset: u64,
    /// How far the decoded run reaches.
    pub length: u64,
    /// Bytes of structures that decoded. For a volume, the boot sector and the root listing.
    pub decoded_bytes: u64,
    pub units: u64,
    pub confidence: f64,
    pub level: &'static str,
    /// A signature identified it. False when it decoded by structure alone.
    pub signature: bool,
    /// Where the volume starts, when the finding says: a boot sector, or inferred geometry.
    pub volume_start: Option<u64>,
    pub volume_length: Option<u64>,
    pub boot_offset: Option<u64>,
    /// The app can open this format.
    pub supported: bool,
    pub detail: String,
    /// A few names the structures hold. Hidden from agents that are kept from names.
    pub names: Vec<String>,
    /// What an island lacks.
    pub missing: Vec<String>,
    pub offer: Option<MetadataOffer>,
    pub geometry: Option<geometry::Geometry>,
    /// The start of the volume found in this report that these structures lie inside.
    pub inside_volume: Option<u64>,
}

impl Finding {
    /// The extent worth saving as a hypothesis: the volume when the finding places one, and
    /// otherwise the decoded run.
    pub fn extent(&self) -> (u64, u64) {
        match (self.volume_start, self.volume_length) {
            (Some(start), Some(length)) => (start, length),
            _ => (self.offset, self.length),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DriverSummary {
    pub driver: String,
    pub offsets_tried: u64,
    /// Offsets where anything decoded at all, before the best were chosen.
    pub decoded_at: u64,
    pub best_offset: Option<u64>,
    pub best_decoded_bytes: u64,
    pub best_confidence: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub start: u64,
    pub length: u64,
    pub step: u64,
    pub signatures: bool,
    pub offsets_tried: u64,
    pub drivers: Vec<DriverSummary>,
    pub findings: Vec<Finding>,
    pub unreadable: Vec<ReadFailure>,
    pub notes: Vec<String>,
}

struct Candidate {
    family: usize,
    offset: u64,
    decoded: Decoded,
}

fn level(confidence: f64) -> &'static str {
    if confidence >= 0.8 {
        "high"
    } else if confidence >= 0.5 {
        "medium"
    } else {
        "low"
    }
}

fn wanted(probe: &dyn FormatProbe, formats: &[String]) -> bool {
    formats.is_empty()
        || formats.iter().any(|name| {
            let name = name.to_ascii_lowercase();
            let name = if name.starts_with("fat") {
                "fat".to_string()
            } else {
                name
            };
            name == probe.family().to_ascii_lowercase()
        })
}

/// Reads `[start, end)` in pieces, zero-filling any piece that cannot be read and recording it.
fn read_range(
    disk: &RawDisk,
    start: u64,
    end: u64,
    limit: u64,
    failures: &mut BTreeMap<u64, ReadFailure>,
) -> Vec<u8> {
    let mut buffer = vec![0; (end - start) as usize];
    let mut position = start;
    while position < end {
        // Pieces end on 64 KiB boundaries, so a failure is the same piece however the chunks fall.
        let next = ((position / READ_PIECE_BYTES + 1) * READ_PIECE_BYTES).min(end);
        let target = &mut buffer[(position - start) as usize..(next - start) as usize];
        if let Err(error) = disk.source.read_exact_at(position, target) {
            target.fill(0);
            if position < limit {
                failures.entry(position).or_insert_with(|| ReadFailure {
                    offset: position,
                    length: next - position,
                    error: format!("{error}"),
                });
            }
        }
        position = next;
    }
    buffer
}

/// Slides over `[start, start + length)` and reports what decodes. `progress` is told how many
/// bytes have been examined and the total, and returns false to stop early.
pub fn analyze(
    disk: &RawDisk,
    start: u64,
    length: u64,
    options: &Options,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Report> {
    ensure!(length > 0, "the range is empty");
    let end = start
        .checked_add(length)
        .filter(|end| *end <= disk.length)
        .ok_or_else(|| anyhow::anyhow!("the range is outside the source"))?;
    ensure!(
        length <= MAX_RANGE_BYTES,
        "a range of {length} bytes is over the {MAX_RANGE_BYTES} byte limit; analyse it in pieces"
    );
    ensure!(
        options.step <= MAX_STEP,
        "the step is over {MAX_STEP} bytes"
    );
    let step = match options.step {
        0 if length <= FINE_RANGE_BYTES => 1,
        0 => LARGE_RANGE_STEP,
        step => step,
    };
    let all = probes::probes();
    let probes: Vec<&dyn FormatProbe> = all
        .iter()
        .map(|probe| probe.as_ref())
        .filter(|probe| wanted(*probe, &options.formats))
        .collect();
    ensure!(
        !probes.is_empty(),
        "no driver matches the formats asked for"
    );

    let mut failures = BTreeMap::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut skip = vec![0u64; probes.len()];
    let mut tried = vec![0u64; probes.len()];
    let mut decoded_at = vec![0u64; probes.len()];
    let mut best: Vec<Option<(u64, u64, f64)>> = vec![None; probes.len()];
    let mut offsets_tried = 0u64;
    let mut chunk_start = start;
    while chunk_start < end {
        ensure!(
            !disk.source.is_gone(),
            "The disk has been disconnected; analysis stopped."
        );
        let chunk_end = (chunk_start + CHUNK_BYTES).min(end);
        let read_end = (chunk_end + OVERLAP_BYTES).min(end);
        let buffer = read_range(disk, chunk_start, read_end, end, &mut failures);
        let mut offset = chunk_start.div_ceil(step) * step;
        while offset < chunk_end {
            let window = &buffer[(offset - chunk_start) as usize..];
            offsets_tried += 1;
            for (index, probe) in probes.iter().enumerate() {
                if offset < skip[index] {
                    continue;
                }
                tried[index] += 1;
                if let Some(decoded) = probe.decode_at(window, options.signatures) {
                    skip[index] = offset + decoded.bytes.max(1);
                    decoded_at[index] += 1;
                    let better = best[index].is_none_or(|(_, bytes, confidence)| {
                        (decoded.bytes, decoded.confidence) > (bytes, confidence)
                    });
                    if better {
                        best[index] = Some((offset, decoded.bytes, decoded.confidence));
                    }
                    candidates.push(Candidate {
                        family: index,
                        offset,
                        decoded,
                    });
                }
            }
            offset += step;
        }
        if candidates.len() > MAX_CANDIDATES * 2 {
            prune(&mut candidates, MAX_CANDIDATES);
        }
        chunk_start = chunk_end;
        if !progress(chunk_end - start, length) {
            break;
        }
    }
    let complete = chunk_start >= end;

    // Remeasure the best candidates over a longer window, then keep the best of each kind.
    prune(&mut candidates, MAX_CANDIDATES);
    candidates.sort_by(|a, b| {
        rank(b)
            .partial_cmp(&rank(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let per_format = options.per_format.max(1);
    let mut kept: Vec<Candidate> = Vec::new();
    for candidate in candidates {
        let kind = candidate.decoded.kind;
        let same = kept
            .iter()
            .filter(|other| other.family == candidate.family && other.decoded.kind == kind)
            .count();
        let overlaps = kept.iter().any(|other| {
            other.family == candidate.family
                && other.offset < candidate.offset + candidate.decoded.bytes
                && candidate.offset < other.offset + other.decoded.bytes
        });
        if same < per_format * 2 && !overlaps {
            kept.push(candidate);
        }
    }
    for candidate in kept
        .iter_mut()
        .filter(|candidate| candidate.decoded.reached_end)
    {
        let until = (candidate.offset + REMEASURE_BYTES).min(end);
        let buffer = read_range(disk, candidate.offset, until, end, &mut failures);
        if let Some(decoded) = probes[candidate.family].decode_at(&buffer, options.signatures) {
            if decoded.bytes >= candidate.decoded.bytes {
                candidate.decoded = decoded;
            }
        }
    }
    kept.sort_by(|a, b| {
        rank(b)
            .partial_cmp(&rank(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut counts: BTreeMap<(usize, u8), usize> = BTreeMap::new();
    kept.retain(|candidate| {
        let count = counts
            .entry((candidate.family, candidate.decoded.kind as u8))
            .or_default();
        *count += 1;
        *count <= per_format
    });

    let mut findings: Vec<Finding> = kept
        .iter()
        .map(|candidate| finding(disk, candidate, probes[candidate.family], options))
        .collect();
    findings.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.decoded_bytes.cmp(&a.decoded_bytes))
            .then(a.offset.cmp(&b.offset))
    });
    link_findings(&mut findings);
    for finding in findings.iter_mut() {
        offer_search(disk, finding);
    }

    let drivers = probes
        .iter()
        .enumerate()
        .map(|(index, probe)| DriverSummary {
            driver: probe.family().into(),
            offsets_tried: tried[index],
            decoded_at: decoded_at[index],
            best_offset: best[index].map(|(offset, _, _)| offset),
            best_decoded_bytes: best[index].map_or(0, |(_, bytes, _)| bytes),
            best_confidence: best[index].map_or(0.0, |(_, _, confidence)| confidence),
        })
        .collect();
    let mut notes = vec![
        "A finding is evidence, not proof. Nothing was written to the source.".to_string(),
        format!("The start offset slid by {step} byte(s), counted from byte zero of the source."),
    ];
    if step > 1 {
        notes.push(
            "Structures that do not start on a multiple of the step were not tried; choose a step of 1 to try every offset."
                .into(),
        );
    }
    if !options.signatures {
        notes.push("Signature probes were off: only structure was used.".into());
    }
    if !complete {
        notes.push("The analysis was stopped before it reached the end of the range.".into());
    }
    if !failures.is_empty() {
        notes.push("Unreadable parts of the range were not examined, and are listed apart.".into());
    }
    Ok(Report {
        start,
        length,
        step,
        signatures: options.signatures,
        offsets_tried,
        drivers,
        findings,
        unreadable: failures.into_values().collect(),
        notes,
    })
}

fn rank(candidate: &Candidate) -> (f64, u64) {
    (candidate.decoded.confidence, candidate.decoded.bytes)
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        rank(self).partial_cmp(&rank(other))
    }
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        rank(self) == rank(other)
    }
}

fn prune(candidates: &mut Vec<Candidate>, keep: usize) {
    if candidates.len() > keep {
        candidates.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        candidates.truncate(keep);
    }
}

fn finding(
    disk: &RawDisk,
    candidate: &Candidate,
    probe: &dyn FormatProbe,
    options: &Options,
) -> Finding {
    let decoded = &candidate.decoded;
    let mut found = Finding {
        format: decoded.format.into(),
        structure: decoded.structure.into(),
        status: Status::Island,
        offset: candidate.offset,
        length: decoded.bytes,
        decoded_bytes: decoded.bytes,
        units: decoded.units,
        confidence: decoded.confidence,
        level: level(decoded.confidence),
        signature: decoded.signature,
        volume_start: None,
        volume_length: None,
        boot_offset: None,
        supported: decoded.kind != Kind::Encrypted,
        detail: decoded.detail.clone(),
        names: decoded.names.clone(),
        missing: Vec::new(),
        offer: None,
        geometry: None,
        inside_volume: None,
    };
    match decoded.kind {
        Kind::Boot => verify_boot(disk, &mut found, decoded),
        Kind::Encrypted => {
            found.missing =
                vec!["the volume's key: it must be unlocked and decrypted by Windows first".into()];
        }
        Kind::Structures => {
            found.missing = missing_for_structures(probe.family());
            if options.geometry && probe.family() == "NTFS" {
                geometry::infer(disk, &mut found);
            }
        }
    }
    found.level = level(found.confidence);
    found
}

fn missing_for_structures(family: &str) -> Vec<String> {
    match family {
        "FAT" => vec![
            "the boot sector, which gives the cluster size and where the data area starts".into(),
            "the FAT, which links each file's clusters".into(),
        ],
        "exFAT" => vec![
            "the boot sector, which gives the cluster size and the heap's offset".into(),
            "the FAT or allocation bitmap, which place the clusters".into(),
        ],
        "NTFS" => {
            vec!["the boot sector, which gives the cluster size and where the MFT starts".into()]
        }
        _ => Vec::new(),
    }
}

/// What opening a volume came to.
enum Opened {
    /// The driver opened it and its root directory lists this many entries.
    Listed(usize),
    /// The app's tolerant opener accepts it, though the strict driver does not: boot region
    /// copies that disagree, for instance. The root was not listed.
    Tolerated,
}

/// Opens a volume as the filesystem it claims to be, through a bounded read-only view.
fn open_volume(disk: &RawDisk, region: &DiskRegion) -> Result<Opened> {
    let source = disk.interpretation(region)?;
    let strict = Driver::open(SourceDevice::new(Arc::clone(&source), region.length))
        .and_then(|driver| driver.root_directory());
    match strict {
        Ok(entries) => Ok(Opened::Listed(entries.len())),
        Err(error) => match crate::volume::Volume::open(source) {
            Ok(_) => Ok(Opened::Tolerated),
            Err(_) => Err(error),
        },
    }
}

fn verify_boot(disk: &RawDisk, found: &mut Finding, decoded: &Decoded) {
    let Some(declared) = decoded.volume_length else {
        return;
    };
    let offset = found.offset;
    let format = decoded.format;
    let region = |start: u64, boot: u64, length: u64| DiskRegion {
        start,
        length: length.min(disk.length.saturating_sub(start)),
        format: format.into(),
        label: "range analysis".into(),
        boot_offset: boot,
    };
    let mut attempt = open_volume(disk, &region(offset, offset, declared));
    let mut start = offset;
    let mut backup = false;
    if attempt.is_err() {
        // A boot sector may be the backup copy: NTFS keeps one just past the end of its
        // volume, exFAT twelve sectors after the main one.
        let candidate = match format {
            "NTFS" => offset
                .checked_sub(declared)
                .map(|start| (start, declared + BOOT_BYTES)),
            "exFAT" => {
                let sector = declared_sector(disk, offset);
                offset
                    .checked_sub(12 * sector)
                    .map(|start| (start, declared.max(13 * sector)))
            }
            _ => None,
        };
        if let Some((candidate_start, length)) = candidate {
            let second = open_volume(disk, &region(candidate_start, offset, length));
            if second.is_ok() {
                attempt = second;
                start = candidate_start;
                backup = true;
            }
        }
    }
    found.volume_start = Some(start);
    found.volume_length = Some(declared.min(disk.length.saturating_sub(start)));
    found.boot_offset = Some(offset);
    match attempt {
        Ok(opened) => {
            let (entries, listing) = match opened {
                Opened::Listed(entries) => {
                    (entries as u64, format!("its root lists {entries} entries"))
                }
                Opened::Tolerated => (
                    0,
                    "it opens, though the strict driver refuses it, so its root was not listed"
                        .to_string(),
                ),
            };
            found.status = Status::Volume;
            found.decoded_bytes = BOOT_BYTES + entries * 32;
            found.units = entries;
            found.confidence =
                found
                    .confidence
                    .max(if format.starts_with("FAT") { 0.9 } else { 0.95 });
            found.detail = format!(
                "{}; the volume opens and {listing}{}",
                found.detail,
                if backup {
                    "; this is a backup copy, the main boot sector is not usable"
                } else {
                    ""
                }
            );
            if backup {
                found.structure = "backup boot sector".into();
            }
        }
        Err(error) => {
            found.status = Status::Island;
            found.detail = format!("{}; the volume does not open: {error:#}", found.detail);
            found.missing = match format {
                "NTFS" => vec!["a readable MFT: the boot sector decodes but the records it points at do not".into()],
                _ => vec!["the volume's own metadata: the boot sector decodes but the volume does not open".into()],
            };
        }
    }
}

fn declared_sector(disk: &RawDisk, offset: u64) -> u64 {
    disk.read(offset, 512)
        .ok()
        .and_then(|bytes| fs_exfat::boot::BootSector::parse(&bytes).ok())
        .map_or(512, |boot| boot.bytes_per_sector())
}

/// Structures that lie inside a volume the report found are that volume's own metadata, not
/// islands.
fn link_findings(findings: &mut [Finding]) {
    let volumes: Vec<(u64, u64)> = findings
        .iter()
        .filter(|finding| matches!(finding.status, Status::Volume | Status::Inferred))
        .filter_map(|finding| Some((finding.volume_start?, finding.volume_length?)))
        .collect();
    for finding in findings
        .iter_mut()
        .filter(|finding| finding.status == Status::Island)
    {
        if finding.volume_start.is_some() && finding.boot_offset.is_some() {
            continue;
        }
        let inside = volumes
            .iter()
            .find(|(start, length)| finding.offset >= *start && finding.offset < start + length);
        if let Some((start, _)) = inside {
            finding.status = Status::Structures;
            finding.inside_volume = Some(*start);
            finding.missing.clear();
        }
    }
}

fn offer_search(disk: &RawDisk, finding: &mut Finding) {
    if finding.status != Status::Island || finding.missing.is_empty() || !finding.supported {
        return;
    }
    let family = match finding.format.as_str() {
        "FAT12" | "FAT16" | "FAT32" => "FAT",
        other => other,
    };
    let start = finding.offset.saturating_sub(SEARCH_MARGIN_BYTES);
    let end = (finding.offset + finding.length + SEARCH_MARGIN_BYTES).min(disk.length);
    finding.offer = Some(MetadataOffer {
        formats: vec![family.into()],
        looks_for: finding.missing.clone(),
        search_start: start,
        search_length: end - start,
    });
}
