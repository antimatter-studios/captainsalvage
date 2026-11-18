//! Recovery hypotheses: an extent of the source and the format it may hold, saved with how sure
//! the app is.
//!
//! A hypothesis is a claim, not a fact, and it costs nothing. Several can cover the same bytes
//! and compete: each is a layer that can be switched on or off, and where enabled layers
//! overlap, the upper one is the one that is read. Start, end, format and confidence can be
//! changed at any time.
//!
//! A hypothesis is also one segment of an overlay, like the segments of a Disk Rebuild: a source
//! byte range, and where it sits in a virtual device. [`Layout`] layers the enabled segments, and
//! [`LayoutSource`] reads the result. A virtual partition table is such a layout whose segments
//! are partitions. Nothing here writes: segments only name ranges of a read-only source, and the
//! records are kept on this Mac, in the same database as saved raw scans, never on the examined
//! disk.

use crate::disk::RawDisk;
use crate::raw_scan::Fingerprint;
use crate::source::{BlockSource, ReadStats};
use crate::workspace::{now_millis, Workspace};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// The formats a hypothesis can name: what the app can open.
pub const FORMATS: &[&str] = &["exFAT", "FAT12", "FAT16", "FAT32", "NTFS"];
const MAX_LABEL_BYTES: usize = 128;
const BOOT_BYTES: u64 = 512;

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Hypothesis {
    pub id: String,
    pub label: String,
    pub format: String,
    /// Where the extent starts in the source, and how long it is.
    pub start: u64,
    pub length: u64,
    /// Where boot evidence is, when it is not at the start of the extent.
    pub boot_offset: Option<u64>,
    /// Where the segment's first byte sits in the virtual device of a layout.
    pub virtual_start: u64,
    pub confidence: f64,
    /// `analysis` when a finding became it, `manual` when the person made it.
    pub origin: String,
    pub enabled: bool,
    /// Higher layers are on top: where enabled extents overlap, the highest is read.
    pub layer: i64,
    pub created_at_millis: u64,
    pub updated_at_millis: u64,
}

impl Hypothesis {
    pub fn end(&self) -> u64 {
        self.start + self.length
    }

    pub fn virtual_end(&self) -> u64 {
        self.virtual_start + self.length
    }
}

/// What a new hypothesis is made of. Missing fields are filled in: the virtual start is the
/// source start (the segment stays where it is), the confidence is zero, the layer is the top.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NewHypothesis {
    pub label: Option<String>,
    pub format: String,
    pub start: u64,
    pub length: u64,
    pub boot_offset: Option<u64>,
    pub virtual_start: Option<u64>,
    pub confidence: Option<f64>,
    pub origin: Option<String>,
    pub enabled: Option<bool>,
}

/// Changes to a hypothesis. A field left out is left as it is.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Patch {
    pub label: Option<String>,
    pub format: Option<String>,
    pub start: Option<u64>,
    pub length: Option<u64>,
    /// `Some(None)` clears the boot offset; it is only reachable from JSON as `null`.
    #[serde(deserialize_with = "double_option")]
    pub boot_offset: Option<Option<u64>>,
    pub virtual_start: Option<u64>,
    pub confidence: Option<f64>,
    pub enabled: Option<bool>,
    pub layer: Option<i64>,
}

fn double_option<'de, D, T>(deserializer: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::deserialize(deserializer)?))
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn identifier() -> Result<String> {
    let mut bytes = [0u8; 8];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(hex(&bytes))
}

/// How a source is recognised again by this store: by the sampled fingerprint scans use, never by
/// a device number or a path alone. The catalog (`catalog.rs`) is what recognises a source by
/// its markers, so a drive a mount touched is still the same drive; this key is what hypotheses
/// saved before the catalog carry.
pub(crate) fn source_key(disk: &RawDisk) -> String {
    Fingerprint::read(disk)
        .sampled_sha256
        .unwrap_or_else(|| format!("path:{}", disk.path.to_string_lossy()))
}

/// The table of saved hypotheses. Created here, and by the catalog, which refers to it.
pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS hypotheses (
    id TEXT PRIMARY KEY, source_key TEXT NOT NULL, source_length INTEGER NOT NULL,
    source_path TEXT NOT NULL, label TEXT NOT NULL, format TEXT NOT NULL,
    src_start INTEGER NOT NULL, src_length INTEGER NOT NULL, boot_offset INTEGER,
    virtual_start INTEGER NOT NULL, confidence REAL NOT NULL, origin TEXT NOT NULL,
    enabled INTEGER NOT NULL, layer INTEGER NOT NULL,
    created_at_millis INTEGER NOT NULL, updated_at_millis INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS hypotheses_source ON hypotheses(source_key, source_length);";

pub struct HypothesisStore {
    connection: Connection,
}

const COLUMNS: &str = "id, label, format, src_start, src_length, boot_offset, virtual_start, \
    confidence, origin, enabled, layer, created_at_millis, updated_at_millis";

fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Hypothesis> {
    Ok(Hypothesis {
        id: row.get(0)?,
        label: row.get(1)?,
        format: row.get(2)?,
        start: row.get::<_, i64>(3)? as u64,
        length: row.get::<_, i64>(4)? as u64,
        boot_offset: row.get::<_, Option<i64>>(5)?.map(|value| value as u64),
        virtual_start: row.get::<_, i64>(6)? as u64,
        confidence: row.get(7)?,
        origin: row.get(8)?,
        enabled: row.get::<_, i64>(9)? != 0,
        layer: row.get(10)?,
        created_at_millis: row.get::<_, i64>(11)? as u64,
        updated_at_millis: row.get::<_, i64>(12)? as u64,
    })
}

impl HypothesisStore {
    pub fn for_user() -> Result<Self> {
        Self::open(&Workspace::root()?.join("raw-scans.sqlite"))
    }

    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;")?;
        connection.execute_batch(SCHEMA)?;
        connection.execute_batch(crate::catalog::SCHEMA)?;
        Ok(Self { connection })
    }

    /// Every hypothesis saved for this source, lowest layer first.
    pub fn list(&self, disk: &RawDisk) -> Result<Vec<Hypothesis>> {
        let key = source_key(disk);
        let mut statement = self.connection.prepare(&format!(
            "SELECT {COLUMNS} FROM hypotheses WHERE source_key = ?1 AND source_length = ?2
             AND id NOT IN (SELECT hypothesis_id FROM interpretations WHERE listed = 0)
             ORDER BY layer, created_at_millis, id"
        ))?;
        let rows = statement.query_map(params![key, disk.length as i64], row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get(&self, disk: &RawDisk, id: &str) -> Result<Hypothesis> {
        self.list(disk)?
            .into_iter()
            .find(|item| item.id == id)
            .with_context(|| format!("no hypothesis {id} is saved for this source"))
    }

    pub fn create(&mut self, disk: &RawDisk, new: NewHypothesis) -> Result<Hypothesis> {
        let existing = self.list(disk)?;
        let now = now_millis();
        let hypothesis = Hypothesis {
            id: identifier()?,
            label: new
                .label
                .filter(|label| !label.trim().is_empty())
                .unwrap_or_else(|| format!("{} hypothesis", new.format)),
            format: new.format,
            start: new.start,
            length: new.length,
            boot_offset: new.boot_offset,
            virtual_start: new.virtual_start.unwrap_or(new.start),
            confidence: new.confidence.unwrap_or(0.0),
            origin: new.origin.unwrap_or_else(|| "manual".into()),
            enabled: new.enabled.unwrap_or(true),
            layer: existing
                .iter()
                .map(|item| item.layer)
                .max()
                .map_or(0, |top| top + 1),
            created_at_millis: now,
            updated_at_millis: now,
        };
        validate(disk, &hypothesis)?;
        self.connection.execute(
            "INSERT INTO hypotheses VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?15)",
            params![
                hypothesis.id,
                source_key(disk),
                disk.length as i64,
                disk.path.to_string_lossy(),
                hypothesis.label,
                hypothesis.format,
                hypothesis.start as i64,
                hypothesis.length as i64,
                hypothesis.boot_offset.map(|value| value as i64),
                hypothesis.virtual_start as i64,
                hypothesis.confidence,
                hypothesis.origin,
                i64::from(hypothesis.enabled),
                hypothesis.layer,
                now as i64,
            ],
        )?;
        Ok(hypothesis)
    }

    pub fn update(&mut self, disk: &RawDisk, id: &str, patch: Patch) -> Result<Hypothesis> {
        let mut hypothesis = self.get(disk, id)?;
        let moved = patch.start.is_some() || patch.length.is_some();
        if let Some(label) = patch.label {
            hypothesis.label = label;
        }
        if let Some(format) = patch.format {
            hypothesis.format = format;
        }
        if let Some(start) = patch.start {
            hypothesis.start = start;
        }
        if let Some(length) = patch.length {
            hypothesis.length = length;
        }
        if let Some(boot) = patch.boot_offset {
            hypothesis.boot_offset = boot;
        }
        match patch.virtual_start {
            Some(virtual_start) => hypothesis.virtual_start = virtual_start,
            // A segment that was where it came from moves with its extent.
            None if moved && hypothesis.virtual_start == self.get(disk, id)?.start => {
                hypothesis.virtual_start = hypothesis.start;
            }
            None => {}
        }
        if let Some(confidence) = patch.confidence {
            hypothesis.confidence = confidence;
        }
        if let Some(enabled) = patch.enabled {
            hypothesis.enabled = enabled;
        }
        if let Some(layer) = patch.layer {
            hypothesis.layer = layer;
        }
        // Boot evidence that no longer lies inside a moved extent is dropped, not kept stale.
        if moved && patch.boot_offset.is_none() {
            if let Some(boot) = hypothesis.boot_offset {
                if boot < hypothesis.start || boot + BOOT_BYTES > hypothesis.end() {
                    hypothesis.boot_offset = None;
                }
            }
        }
        validate(disk, &hypothesis)?;
        hypothesis.updated_at_millis = now_millis();
        self.connection.execute(
            "UPDATE hypotheses SET label = ?2, format = ?3, src_start = ?4, src_length = ?5,
                boot_offset = ?6, virtual_start = ?7, confidence = ?8, enabled = ?9, layer = ?10,
                updated_at_millis = ?11 WHERE id = ?1",
            params![
                hypothesis.id,
                hypothesis.label,
                hypothesis.format,
                hypothesis.start as i64,
                hypothesis.length as i64,
                hypothesis.boot_offset.map(|value| value as i64),
                hypothesis.virtual_start as i64,
                hypothesis.confidence,
                i64::from(hypothesis.enabled),
                hypothesis.layer,
                hypothesis.updated_at_millis as i64,
            ],
        )?;
        Ok(hypothesis)
    }

    pub fn delete(&mut self, disk: &RawDisk, id: &str) -> Result<()> {
        self.get(disk, id)?;
        // A scan was made under this interpretation. Its results stay with it, so the layer is
        // taken off the list and the record is kept.
        let scanned: i64 = self.connection.query_row(
            "SELECT count(*) FROM scans WHERE interpretation_id = ?1",
            [id],
            |row| row.get(0),
        )?;
        if scanned > 0 {
            self.connection.execute(
                "UPDATE interpretations SET listed = 0 WHERE hypothesis_id = ?1",
                [id],
            )?;
            return Ok(());
        }
        self.connection
            .execute("DELETE FROM hypotheses WHERE id = ?1", [id])
            .map(|_| ())
            .context("deleting the hypothesis")
    }

    /// Puts one hypothesis on top of the others.
    pub fn raise(&mut self, disk: &RawDisk, id: &str) -> Result<Hypothesis> {
        let top = self
            .list(disk)?
            .iter()
            .map(|item| item.layer)
            .max()
            .context("there are no hypotheses")?;
        self.update(
            disk,
            id,
            Patch {
                layer: Some(top + 1),
                ..Patch::default()
            },
        )
    }

    #[cfg(test)]
    fn exists(&self, id: &str) -> bool {
        use rusqlite::OptionalExtension;
        self.connection
            .query_row("SELECT 1 FROM hypotheses WHERE id = ?1", [id], |_| Ok(()))
            .optional()
            .unwrap()
            .is_some()
    }
}

fn validate(disk: &RawDisk, hypothesis: &Hypothesis) -> Result<()> {
    ensure!(
        FORMATS.contains(&hypothesis.format.as_str()),
        "format must be one of {}",
        FORMATS.join(", ")
    );
    ensure!(hypothesis.length > 0, "a hypothesis must not be empty");
    ensure!(
        hypothesis
            .start
            .checked_add(hypothesis.length)
            .is_some_and(|end| end <= disk.length),
        "the extent is outside the source"
    );
    ensure!(
        hypothesis
            .virtual_start
            .checked_add(hypothesis.length)
            .is_some()
            && hypothesis.virtual_start <= i64::MAX as u64 / 2,
        "the virtual position is too large"
    );
    ensure!(
        (0.0..=1.0).contains(&hypothesis.confidence),
        "confidence must be between 0 and 1"
    );
    ensure!(
        hypothesis.label.len() <= MAX_LABEL_BYTES,
        "the label is over {MAX_LABEL_BYTES} bytes"
    );
    if let Some(boot) = hypothesis.boot_offset {
        ensure!(
            boot >= hypothesis.start
                && boot
                    .checked_add(BOOT_BYTES)
                    .is_some_and(|end| end <= hypothesis.end()),
            "boot evidence must lie inside the extent"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- competing layers

/// A stretch of the source covered by the same enabled hypotheses.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Piece {
    pub start: u64,
    pub length: u64,
    /// Every enabled hypothesis over this stretch, top layer first. The first is the one read.
    pub stack: Vec<String>,
}

/// Cuts the source at every edge of an enabled extent, and says what covers each piece. Where
/// more than one hypothesis covers a piece, they compete; the top layer wins.
pub fn competing(hypotheses: &[Hypothesis]) -> Vec<Piece> {
    let enabled: Vec<&Hypothesis> = hypotheses.iter().filter(|item| item.enabled).collect();
    let mut edges: Vec<u64> = enabled
        .iter()
        .flat_map(|item| [item.start, item.end()])
        .collect();
    edges.sort_unstable();
    edges.dedup();
    let mut pieces = Vec::new();
    for pair in edges.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        let mut covering: Vec<&&Hypothesis> = enabled
            .iter()
            .filter(|item| item.start <= start && end <= item.end())
            .collect();
        if covering.is_empty() {
            continue;
        }
        covering.sort_by_key(|item| std::cmp::Reverse((item.layer, item.created_at_millis)));
        pieces.push(Piece {
            start,
            length: end - start,
            stack: covering.iter().map(|item| item.id.clone()).collect(),
        });
    }
    pieces
}

// ---------------------------------------------------------------- the overlay

/// A segment of a layout: a range of the source, shown at a position of the virtual device.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Segment {
    pub id: String,
    pub label: String,
    pub format: String,
    pub source_start: u64,
    pub length: u64,
    pub virtual_start: u64,
    pub layer: i64,
}

/// The virtual partition table: segments that are partitions, by where they sit in the device.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PartitionEntry {
    pub id: String,
    pub label: String,
    pub format: String,
    pub virtual_start: u64,
    pub length: u64,
    pub source_start: u64,
    pub confidence: f64,
    /// Another enabled segment above this one covers part of it in the virtual device.
    pub shadowed: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Gap {
    pub virtual_start: u64,
    pub length: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Layout {
    pub segments: Vec<Segment>,
    pub virtual_length: u64,
    /// Virtual ranges no segment covers. Reading them is refused, not filled with zeros.
    pub gaps: Vec<Gap>,
    /// Source ranges that more than one enabled hypothesis covers, and which one is read.
    pub competing: Vec<Piece>,
    pub partition_table: Vec<PartitionEntry>,
}

impl Layout {
    /// The layout of the enabled hypotheses.
    pub fn of(hypotheses: &[Hypothesis]) -> Self {
        let mut segments: Vec<Segment> = hypotheses
            .iter()
            .filter(|item| item.enabled)
            .map(|item| Segment {
                id: item.id.clone(),
                label: item.label.clone(),
                format: item.format.clone(),
                source_start: item.start,
                length: item.length,
                virtual_start: item.virtual_start,
                layer: item.layer,
            })
            .collect();
        segments.sort_by_key(|segment| (segment.layer, segment.virtual_start));
        let virtual_length = segments
            .iter()
            .map(|segment| segment.virtual_start + segment.length)
            .max()
            .unwrap_or(0);
        let mut covered: Vec<(u64, u64)> = segments
            .iter()
            .map(|segment| {
                (
                    segment.virtual_start,
                    segment.virtual_start + segment.length,
                )
            })
            .collect();
        covered.sort_unstable();
        let mut gaps = Vec::new();
        let mut reached = 0;
        for (start, end) in covered {
            if start > reached {
                gaps.push(Gap {
                    virtual_start: reached,
                    length: start - reached,
                });
            }
            reached = reached.max(end);
        }
        let partition_table = segments
            .iter()
            .map(|segment| PartitionEntry {
                id: segment.id.clone(),
                label: segment.label.clone(),
                format: segment.format.clone(),
                virtual_start: segment.virtual_start,
                length: segment.length,
                source_start: segment.source_start,
                confidence: hypotheses
                    .iter()
                    .find(|item| item.id == segment.id)
                    .map_or(0.0, |item| item.confidence),
                shadowed: segments.iter().any(|other| {
                    other.layer > segment.layer
                        && other.virtual_start < segment.virtual_start + segment.length
                        && segment.virtual_start < other.virtual_start + other.length
                }),
            })
            .collect::<Vec<_>>();
        let mut partition_table = partition_table;
        partition_table.sort_by_key(|entry| (entry.virtual_start, entry.source_start));
        Self {
            segments,
            virtual_length,
            gaps,
            competing: competing(hypotheses)
                .into_iter()
                .filter(|piece| piece.stack.len() > 1)
                .collect(),
            partition_table,
        }
    }
}

/// The virtual device a layout presents, read through the read-only source underneath. There is
/// no write path. Where enabled segments overlap in the virtual device the highest layer is
/// read, and the layers below show through wherever it does not reach.
pub struct LayoutSource {
    source: Arc<dyn BlockSource>,
    segments: Vec<Segment>,
    length: u64,
}

impl LayoutSource {
    pub fn new(source: Arc<dyn BlockSource>, layout: &Layout) -> Self {
        Self {
            source,
            segments: layout.segments.clone(),
            length: layout.virtual_length,
        }
    }

    pub fn length(&self) -> u64 {
        self.length
    }

    /// The segment that is read at a virtual position, and how far that goes before the top
    /// layer changes.
    fn top_at(&self, position: u64) -> Option<(&Segment, u64)> {
        let covering = |segment: &&Segment| {
            segment.virtual_start <= position && position < segment.virtual_start + segment.length
        };
        let top = self
            .segments
            .iter()
            .filter(covering)
            .max_by_key(|segment| segment.layer)?;
        let mut until = top.virtual_start + top.length;
        for other in &self.segments {
            if other.layer > top.layer && other.virtual_start > position {
                until = until.min(other.virtual_start);
            }
        }
        Some((top, until))
    }
}

impl BlockSource for LayoutSource {
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> std::io::Result<()> {
        let end = offset
            .checked_add(buffer.len() as u64)
            .filter(|end| *end <= self.length)
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "read outside the layout")
            })?;
        let mut position = offset;
        while position < end {
            let (segment, until) = self.top_at(position).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("byte {position} of the layout is covered by no segment"),
                )
            })?;
            let stop = until.min(end);
            let target = &mut buffer[(position - offset) as usize..(stop - offset) as usize];
            self.source.read_exact_at(
                segment.source_start + (position - segment.virtual_start),
                target,
            )?;
            position = stop;
        }
        Ok(())
    }

    fn read_stats(&self) -> ReadStats {
        self.source.read_stats()
    }

    fn is_gone(&self) -> bool {
        self.source.is_gone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(length: usize) -> RawDisk {
        let data: Vec<u8> = (0..length).map(|index| (index % 251) as u8).collect();
        RawDisk::with_source(
            Path::new("hypotheses.img"),
            Arc::new(crate::source::MemorySource::new(data)),
            length as u64,
        )
        .unwrap()
    }

    fn store() -> (HypothesisStore, std::path::PathBuf) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "captainsalvage-hypotheses-{}-{}.sqlite",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        (HypothesisStore::open(&path).unwrap(), path)
    }

    fn make(start: u64, length: u64, format: &str) -> NewHypothesis {
        NewHypothesis {
            format: format.into(),
            start,
            length,
            ..NewHypothesis::default()
        }
    }

    #[test]
    fn a_hypothesis_is_saved_changed_and_removed_for_its_source() {
        let disk = disk(100_000);
        let (mut store, path) = store();
        let first = store.create(&disk, make(1000, 5000, "FAT16")).unwrap();
        assert_eq!(first.virtual_start, 1000);
        assert_eq!(first.layer, 0);
        assert!(first.enabled);
        let second = store.create(&disk, make(2000, 5000, "NTFS")).unwrap();
        assert_eq!(second.layer, 1);
        let changed = store
            .update(
                &disk,
                &first.id,
                Patch {
                    start: Some(1500),
                    length: Some(4000),
                    confidence: Some(0.7),
                    ..Patch::default()
                },
            )
            .unwrap();
        // The segment was where it came from, so it moved with the extent.
        assert_eq!(
            (changed.start, changed.virtual_start, changed.length),
            (1500, 1500, 4000)
        );
        let listed = store.list(&disk).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0], changed);
        store.delete(&disk, &first.id).unwrap();
        assert!(!store.exists(&first.id));
        assert_eq!(store.list(&disk).unwrap().len(), 1);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn what_is_saved_for_one_source_is_not_listed_for_another() {
        let disk_a = disk(100_000);
        let other: Vec<u8> = (0..100_000usize).map(|index| (index % 249) as u8).collect();
        let disk_b = RawDisk::with_source(
            Path::new("other.img"),
            Arc::new(crate::source::MemorySource::new(other)),
            100_000,
        )
        .unwrap();
        let (mut store, path) = store();
        store.create(&disk_a, make(0, 1000, "exFAT")).unwrap();
        assert_eq!(store.list(&disk_a).unwrap().len(), 1);
        assert!(store.list(&disk_b).unwrap().is_empty());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn invalid_hypotheses_are_refused() {
        let disk = disk(10_000);
        let (mut store, path) = store();
        assert!(store.create(&disk, make(0, 0, "NTFS")).is_err());
        assert!(store.create(&disk, make(9000, 2000, "NTFS")).is_err());
        assert!(store.create(&disk, make(0, 100, "ext4")).is_err());
        assert!(store.create(&disk, make(u64::MAX, 2, "NTFS")).is_err());
        let mut bad = make(0, 1000, "NTFS");
        bad.confidence = Some(1.5);
        assert!(store.create(&disk, bad).is_err());
        let mut bad = make(0, 1000, "NTFS");
        bad.boot_offset = Some(900);
        assert!(store.create(&disk, bad).is_err());
        let good = store.create(&disk, make(0, 1000, "NTFS")).unwrap();
        assert!(store
            .update(
                &disk,
                &good.id,
                Patch {
                    length: Some(20_000),
                    ..Patch::default()
                }
            )
            .is_err());
        assert!(store.update(&disk, "nothing", Patch::default()).is_err());
        assert!(store.delete(&disk, "nothing").is_err());
        std::fs::remove_file(&path).unwrap();
    }

    fn hypothesis(id: &str, start: u64, length: u64, layer: i64, enabled: bool) -> Hypothesis {
        Hypothesis {
            id: id.into(),
            label: id.into(),
            format: "NTFS".into(),
            start,
            length,
            boot_offset: None,
            virtual_start: start,
            confidence: 0.5,
            origin: "manual".into(),
            enabled,
            layer,
            created_at_millis: 0,
            updated_at_millis: 0,
        }
    }

    #[test]
    fn overlapping_hypotheses_compete_and_the_upper_enabled_layer_wins() {
        let all = [
            hypothesis("a", 0, 100, 0, true),
            hypothesis("b", 50, 100, 1, true),
            hypothesis("c", 60, 10, 2, false),
        ];
        let pieces = competing(&all);
        assert_eq!(
            pieces,
            [
                Piece {
                    start: 0,
                    length: 50,
                    stack: vec!["a".into()]
                },
                Piece {
                    start: 50,
                    length: 50,
                    stack: vec!["b".into(), "a".into()]
                },
                Piece {
                    start: 100,
                    length: 50,
                    stack: vec!["b".into()]
                },
            ]
        );
        // Switching b off lets a be the only reading; nothing was deleted.
        let without_b = [
            hypothesis("a", 0, 100, 0, true),
            hypothesis("b", 50, 100, 1, false),
        ];
        assert_eq!(competing(&without_b).len(), 1);
    }

    #[test]
    fn a_layout_overlays_segments_and_reads_the_source_without_a_write_path() {
        let disk = disk(10_000);
        // Low layer: source 0..1000 at virtual 0. Upper: source 5000..5100 at virtual 200.
        let mut low = hypothesis("low", 0, 1000, 0, true);
        low.virtual_start = 0;
        let mut upper = hypothesis("upper", 5000, 100, 1, true);
        upper.virtual_start = 200;
        let layout = Layout::of(&[low, upper]);
        assert_eq!(layout.virtual_length, 1000);
        assert!(layout.gaps.is_empty());
        let device = LayoutSource::new(Arc::clone(&disk.source), &layout);
        let mut bytes = vec![0; 1000];
        device.read_exact_at(0, &mut bytes).unwrap();
        let source = disk.read(0, 10_000).unwrap();
        assert_eq!(&bytes[..200], &source[..200]);
        assert_eq!(&bytes[200..300], &source[5000..5100]);
        assert_eq!(&bytes[300..], &source[300..1000]);
        assert!(device.read_exact_at(990, &mut [0; 20]).is_err());
    }

    #[test]
    fn bytes_no_segment_covers_are_refused_not_filled() {
        let disk = disk(10_000);
        let mut first = hypothesis("first", 0, 100, 0, true);
        first.virtual_start = 0;
        let mut second = hypothesis("second", 500, 100, 1, true);
        second.virtual_start = 300;
        let layout = Layout::of(&[first, second]);
        assert_eq!(layout.gaps.len(), 1);
        assert_eq!(
            (layout.gaps[0].virtual_start, layout.gaps[0].length),
            (100, 200)
        );
        let device = LayoutSource::new(Arc::clone(&disk.source), &layout);
        assert!(device.read_exact_at(0, &mut [0; 100]).is_ok());
        assert!(device.read_exact_at(90, &mut [0; 20]).is_err());
        assert!(device.read_exact_at(300, &mut [0; 100]).is_ok());
    }

    #[test]
    fn a_virtual_partition_table_lists_partitions_by_position_and_marks_shadowed_ones() {
        let mut one = hypothesis("one", 8000, 2000, 0, true);
        one.virtual_start = 1000;
        let mut two = hypothesis("two", 0, 2000, 1, true);
        two.virtual_start = 2500;
        let off = hypothesis("off", 4000, 1000, 2, false);
        let layout = Layout::of(&[one, two, off]);
        let ids: Vec<_> = layout
            .partition_table
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        assert_eq!(ids, ["one", "two"]);
        // one covers virtual 1000..3000 and two covers 2500..4500: one is shadowed by two.
        assert!(layout.partition_table[0].shadowed);
        assert!(!layout.partition_table[1].shadowed);
        assert_eq!(layout.gaps[0].virtual_start, 0);
        assert_eq!(layout.gaps[0].length, 1000);
    }
}
