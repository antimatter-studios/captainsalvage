//! Interpretations and scans: what a source was read as, and what each reading stored.

use super::{
    canonical, identifier, Accept, Catalog, Resolution, SourceRecord, FILESYSTEM_SCANNER_VERSION,
};
use crate::disk::RawDisk;
use crate::raw_scan::{Comparison, MarkerReader, Verification};
use crate::stale::StaleOptions;
use crate::workspace::{now_millis, Workspace};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

/// The format of an interpretation that reads no filesystem: raw bytes only.
pub const NO_FORMAT: &str = "none";
/// The first cluster a map is indexed from, which is where a filesystem scan's coverage starts.
const FIRST_CLUSTER: u64 = 2;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ScanKind {
    /// Evidence found by reading every byte of a range (`raw_findings`).
    Raw,
    /// A filesystem read as an interpretation says: its map, tests, islands and Disk Rebuilds.
    Filesystem,
}

impl ScanKind {
    fn text(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Filesystem => "filesystem",
        }
    }

    fn parse(text: &str) -> Self {
        if text == "raw" {
            Self::Raw
        } else {
            Self::Filesystem
        }
    }
}

/// An extent of a source and the format read there, with where the claim came from.
#[derive(Clone, Debug, PartialEq)]
pub struct InterpretationRequest {
    /// exFAT, FAT12, FAT16, FAT32, NTFS, or `none` for raw bytes only.
    pub format: String,
    pub start: u64,
    pub length: u64,
    pub boot_offset: Option<u64>,
    /// `analysis` when boot evidence or a range analysis found it, `manual` when the person chose.
    pub origin: String,
    pub confidence: f64,
}

/// A saved hypothesis that is tied to a source.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InterpretationRecord {
    /// The hypothesis ID, usable as a foreign key.
    pub id: String,
    pub format: String,
    pub start: u64,
    pub length: u64,
    pub boot_offset: Option<u64>,
    pub origin: String,
    pub confidence: f64,
    /// Whether it shows as a layer in the hypotheses of the source. One made by opening is kept
    /// for its scans, and does not.
    pub listed: bool,
    /// False for what a migration guessed from an old workspace, until the device is seen.
    pub verified: bool,
}

#[derive(Clone, Debug)]
pub struct ScanRequest {
    pub source_id: String,
    pub kind: ScanKind,
    pub interpretation: InterpretationRequest,
    pub options: Value,
    pub scanner_version: u32,
    pub range_start: u64,
    pub range_end: u64,
    /// The old key of the volume (its workspace name), kept so it is still found by it.
    pub volume_key: Option<String>,
    /// The saved raw scan that holds a raw scan's findings.
    pub raw_scan_id: Option<String>,
    pub block_size: u64,
    pub block_count: u64,
}

/// One scan, as the lists show it. The format, extent and origin are what it was made under, and
/// do not change when the hypothesis it came from is edited later.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanRecord {
    pub id: String,
    pub source_id: String,
    pub kind: ScanKind,
    pub label: String,
    pub interpretation_id: Option<String>,
    pub format: String,
    pub start: u64,
    pub length: u64,
    pub boot_offset: Option<u64>,
    pub origin: String,
    pub confidence: f64,
    pub options: Value,
    pub scanner_version: u32,
    pub range_start: u64,
    pub range_end: u64,
    /// How far the scan has read, as an offset in the source. None until it has started.
    pub covered_end: Option<u64>,
    pub complete: bool,
    /// `not-started`, `running`, `paused` or `complete`.
    pub status: String,
    /// What the scan keeps on this Mac, in bytes, as last measured.
    pub stored_bytes: u64,
    pub raw_scan_id: Option<String>,
    /// The scan's own folder, relative to the app's folder. Filesystem scans only.
    pub workspace: Option<String>,
    pub volume_key: Option<String>,
    /// Made from what an older version stored, rather than by this one.
    pub migrated: bool,
    /// How many areas of its results may be out of date, because the source changed since it was
    /// scanned and they have not been rescanned.
    pub stale_regions: u32,
    pub created_at_millis: u64,
    pub updated_at_millis: u64,
    pub opened_at_millis: u64,
    #[serde(skip)]
    pub block_size: u64,
    #[serde(skip)]
    pub block_count: u64,
}

/// What opening a filesystem needs to find or make its scan.
#[derive(Clone, Debug)]
pub struct FilesystemOpen {
    pub interpretation: InterpretationRequest,
    /// The workspace name of the volume, the key everything was stored under before scans.
    pub volume_key: String,
    pub block_size: u64,
    pub block_count: u64,
    pub options: Value,
    pub accept: Accept,
    /// The person chose to open this as a source of its own. The volume's old key is not used to
    /// find a stored source, so a probable match that was not acknowledged is not applied that way.
    pub fresh: bool,
}

pub struct OpenedScan {
    pub scan: ScanRecord,
    pub source: SourceRecord,
    pub resolution: Resolution,
    /// A scan was made for this open. False when stored results were found.
    pub created: bool,
    pub workspace: Workspace,
}

const SCAN_COLUMNS: &str = "scans.id, scans.source_id, scans.kind, scans.label, \
    scans.interpretation_id, scans.format, scans.ext_start, scans.ext_length, scans.boot_offset, \
    scans.origin, scans.confidence, scans.options, scans.scanner_version, scans.range_start, \
    scans.range_end, scans.raw_scan_id, scans.data_dir, scans.volume_key, \
    scans.legacy_key IS NOT NULL, scans.stored_bytes, scans.created_at_millis, \
    max(scans.updated_at_millis, coalesce((SELECT r.updated_at_millis FROM raw_scans r \
        WHERE r.id = scans.raw_scan_id), 0)), \
    scans.opened_at_millis, scans.block_size, scans.block_count";

fn scan_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ScanRecord> {
    let options: String = row.get(11)?;
    Ok(ScanRecord {
        id: row.get(0)?,
        source_id: row.get(1)?,
        kind: ScanKind::parse(&row.get::<_, String>(2)?),
        label: row.get(3)?,
        interpretation_id: row.get(4)?,
        format: row.get(5)?,
        start: row.get::<_, i64>(6)? as u64,
        length: row.get::<_, i64>(7)? as u64,
        boot_offset: row.get::<_, Option<i64>>(8)?.map(|value| value as u64),
        origin: row.get(9)?,
        confidence: row.get(10)?,
        options: serde_json::from_str(&options).unwrap_or(Value::Null),
        scanner_version: row.get(12)?,
        range_start: row.get::<_, i64>(13)? as u64,
        range_end: row.get::<_, i64>(14)? as u64,
        covered_end: None,
        complete: false,
        status: "not-started".into(),
        stored_bytes: row.get::<_, i64>(19)? as u64,
        raw_scan_id: row.get(15)?,
        workspace: row.get(16)?,
        volume_key: row.get(17)?,
        migrated: row.get(18)?,
        stale_regions: 0,
        created_at_millis: row.get::<_, i64>(20)? as u64,
        updated_at_millis: row.get::<_, i64>(21)? as u64,
        opened_at_millis: row.get::<_, i64>(22)? as u64,
        block_size: row.get::<_, i64>(23)? as u64,
        block_count: row.get::<_, i64>(24)? as u64,
    })
}

fn immediate(connection: &Connection) -> Result<Transaction<'_>> {
    Ok(Transaction::new_unchecked(
        connection,
        TransactionBehavior::Immediate,
    )?)
}

/// The size of everything under a folder.
fn folder_bytes(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => folder_bytes(&entry.path()),
            Ok(meta) => meta.len(),
            Err(_) => 0,
        })
        .sum()
}

impl Catalog {
    // ---- interpretations ----

    /// The interpretation of this source with this extent, format and boot evidence: the one
    /// saved before, or a new one. A hypothesis saved before the catalog, with the same
    /// extent and format on a source with the same sampled hash, is the interpretation, and
    /// shows as a layer as it did.
    pub fn interpretation(
        &self,
        source_id: &str,
        request: &InterpretationRequest,
    ) -> Result<InterpretationRecord> {
        let transaction = immediate(&self.connection)?;
        let found = ensure_interpretation(&transaction, source_id, request)?;
        transaction.commit()?;
        Ok(found)
    }

    pub fn interpretations(&self, source_id: &str) -> Result<Vec<InterpretationRecord>> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {INTERPRETATION_COLUMNS} FROM interpretations i
             JOIN hypotheses h ON h.id = i.hypothesis_id
             WHERE i.source_id = ?1 ORDER BY h.created_at_millis, h.id"
        ))?;
        let rows = statement.query_map([source_id], interpretation_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- scans ----

    /// The scan for exactly this combination, or a new one. Changing the source, the extent, the
    /// format, the boot evidence, the options, the scanner version or the range gives a new scan
    /// and leaves the old one as it is. The bool says whether a scan was made.
    pub fn ensure_scan(&self, request: &ScanRequest) -> Result<(ScanRecord, bool)> {
        let transaction = immediate(&self.connection)?;
        let result = ensure_scan(&transaction, request)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn scan(&self, id: &str) -> Result<ScanRecord> {
        let mut scan = self
            .connection
            .query_row(
                &format!("SELECT {SCAN_COLUMNS} FROM scans WHERE scans.id = ?1"),
                [id],
                scan_row,
            )
            .optional()?
            .with_context(|| format!("no scan {id} is stored"))?;
        self.measure(&mut scan);
        Ok(scan)
    }

    /// Every scan of a source, the latest scan of the interpretation last used first, then the
    /// rest of that interpretation, then the others, newest first. Each is measured: how far it
    /// has read and how much it stores, which is written back so a list shows the size without
    /// reading the scan.
    pub fn scans(&self, source_id: &str) -> Result<Vec<ScanRecord>> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {SCAN_COLUMNS} FROM scans WHERE source_id = ?1
             ORDER BY updated_at_millis DESC, scans.id"
        ))?;
        let mut scans = statement
            .query_map([source_id], scan_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for scan in &mut scans {
            self.measure(scan);
        }
        let last_used = scans
            .iter()
            .max_by_key(|scan| (scan.opened_at_millis, scan.updated_at_millis))
            .map(|scan| {
                (
                    scan.format.clone(),
                    scan.start,
                    scan.length,
                    scan.boot_offset,
                )
            });
        scans.sort_by_key(|scan| {
            let same = Some((
                scan.format.clone(),
                scan.start,
                scan.length,
                scan.boot_offset,
            )) == last_used;
            (!same, std::cmp::Reverse(scan.updated_at_millis))
        });
        Ok(scans)
    }

    pub fn label_scan(&self, id: &str, label: &str) -> Result<ScanRecord> {
        ensure!(label.len() <= 128, "a name can be at most 128 bytes");
        let changed = self.connection.execute(
            "UPDATE scans SET label = ?2 WHERE id = ?1",
            params![id, label.trim()],
        )?;
        ensure!(changed == 1, "no scan {id} is stored");
        self.scan(id)
    }

    /// Notes that a scan was opened, which makes its interpretation the one last used.
    pub fn touch_scan(&self, id: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE scans SET opened_at_millis = ?2 WHERE id = ?1",
            params![id, now_millis() as i64],
        )?;
        Ok(())
    }

    /// Notes that a scan stored something, so lists order it by when it last did.
    pub fn touch_updated(&self, id: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE scans SET updated_at_millis = ?2 WHERE id = ?1",
            params![id, now_millis() as i64],
        )?;
        Ok(())
    }

    /// How far a scan has read and what it stores, from the scan's own files.
    fn measure(&self, scan: &mut ScanRecord) {
        let (bytes, covered, status) = match scan.kind {
            ScanKind::Raw => self.measure_raw(scan),
            ScanKind::Filesystem => self.measure_filesystem(scan),
        };
        scan.updated_at_millis = scan.updated_at_millis.max(self.stored_at(scan));
        scan.stale_regions = super::stale::stale_count(&self.connection, &scan.id).unwrap_or(0);
        scan.covered_end = covered;
        scan.complete = status == "complete";
        scan.status = status;
        if let Some(bytes) = bytes {
            if bytes != scan.stored_bytes {
                scan.stored_bytes = bytes;
            }
            let _ = self.connection.execute(
                "UPDATE scans SET stored_bytes = ?2, stored_at_millis = ?3 WHERE id = ?1",
                params![scan.id, bytes as i64, now_millis() as i64],
            );
        }
    }

    fn measure_raw(&self, scan: &ScanRecord) -> (Option<u64>, Option<u64>, String) {
        let Some(raw) = scan.raw_scan_id.as_deref() else {
            return (None, None, "not-started".into());
        };
        let next: Option<i64> = self
            .connection
            .query_row(
                "SELECT next_offset FROM raw_scans WHERE id = ?1",
                [raw],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten();
        let bytes: Option<i64> = self
            .connection
            .query_row(
                "SELECT (SELECT coalesce(sum(length(evidence) + length(format) + length(structure) + 24), 0)
                         FROM raw_findings WHERE scan_id = ?1)
                      + (SELECT coalesce(sum(length(error) + 40), 0) FROM raw_failures WHERE scan_id = ?1)",
                [raw],
                |row| row.get(0),
            )
            .ok();
        let status = match next {
            Some(next) if next as u64 >= scan.range_end => "complete",
            Some(next) if next as u64 > scan.range_start => "paused",
            Some(_) => "not-started",
            None => "not-started",
        };
        (
            bytes.map(|value| value as u64),
            next.map(|value| value as u64),
            status.into(),
        )
    }

    /// When a filesystem scan last wrote its map.
    fn stored_at(&self, scan: &ScanRecord) -> u64 {
        let Some(dir) = scan.workspace.as_deref() else {
            return 0;
        };
        let map = self.root.join(dir).join("map.sqlite");
        let wal = self.root.join(dir).join("map.sqlite-wal");
        [map, wal]
            .iter()
            .filter_map(|path| std::fs::metadata(path).ok()?.modified().ok())
            .filter_map(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_millis() as u64)
            .max()
            .unwrap_or(0)
    }

    fn measure_filesystem(&self, scan: &ScanRecord) -> (Option<u64>, Option<u64>, String) {
        let Some(dir) = scan.workspace.as_deref() else {
            return (None, None, "not-started".into());
        };
        let folder = self.root.join(dir);
        let bytes = folder_bytes(&folder);
        let state = |key: &str| -> Option<String> {
            let connection = Connection::open_with_flags(
                folder.join("map.sqlite"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .ok()?;
            connection
                .query_row(
                    "SELECT value FROM scan_state WHERE key = ?1",
                    [key],
                    |row| row.get(0),
                )
                .ok()
        };
        let next: Option<u64> = state("next_cluster").and_then(|text| text.parse().ok());
        let status = state("status").unwrap_or_else(|| "not-started".into());
        let covered = next.map(|next| {
            let read = next.saturating_sub(FIRST_CLUSTER) * scan.block_size.max(1);
            scan.start + read.min(scan.length)
        });
        let status = if scan.block_count > 0
            && next.is_some_and(|next| next >= scan.block_count + FIRST_CLUSTER)
        {
            "complete".to_string()
        } else {
            status
        };
        (Some(bytes), covered, status)
    }

    // ---- opening ----

    /// Finds or makes the scan of a filesystem that is being opened, and the files it keeps.
    ///
    /// The source is the stored one the device matches, or a new one (see
    /// [`Catalog::resolve_source`]). Opening the same source as the same format and extent finds
    /// the scan it made, with its results; a scan is only made when none is stored for that
    /// combination. A workspace from before the catalog, found by the volume's old key, is that
    /// scan: it is used where it is, and its guessed format and extent are replaced by what the
    /// device says.
    pub fn open_filesystem(&self, disk: &RawDisk, open: &FilesystemOpen) -> Result<OpenedScan> {
        // A workspace an older version made since the catalog last looked is registered first,
        // so it is found and not scanned again.
        let known: bool = self.connection.query_row(
            "SELECT count(*) > 0 FROM scans WHERE legacy_key = ?1",
            [&open.volume_key],
            |row| row.get(0),
        )?;
        if !known && self.root.join(&open.volume_key).is_dir() {
            super::migrate(self)?;
        }
        let key = (!open.fresh).then_some(open.volume_key.as_str());
        let resolution = self.resolve_source(disk, open.accept, key)?;
        if let Some(comparison) = resolution
            .comparison
            .as_ref()
            .filter(|comparison| comparison.state == Verification::Probable)
        {
            // Stored work was applied on a probable match: what it holds near the changed
            // markers may be out of date.
            self.mark_changed(
                &resolution.source.id,
                &comparison.changed_offsets,
                &StaleOptions::default(),
            )?;
        }
        let request = ScanRequest {
            source_id: resolution.source.id.clone(),
            kind: ScanKind::Filesystem,
            interpretation: open.interpretation.clone(),
            options: open.options.clone(),
            scanner_version: FILESYSTEM_SCANNER_VERSION,
            range_start: open.interpretation.start,
            range_end: open.interpretation.start + open.interpretation.length,
            volume_key: Some(open.volume_key.clone()),
            raw_scan_id: None,
            block_size: open.block_size,
            block_count: open.block_count,
        };
        let transaction = immediate(&self.connection)?;
        let (scan, created) = match adopt_unverified(&transaction, &request)? {
            Some(scan) => (scan, false),
            None => ensure_scan(&transaction, &request)?,
        };
        transaction.execute(
            "UPDATE scans SET opened_at_millis = ?2, block_size = ?3, block_count = ?4 WHERE id = ?1",
            params![
                scan.id,
                now_millis() as i64,
                open.block_size as i64,
                open.block_count as i64
            ],
        )?;
        transaction.commit()?;
        let scan = self.scan(&scan.id)?;
        let workspace = self.workspace_for(&scan, open.block_size)?;
        Ok(OpenedScan {
            source: self.source(&scan.source_id)?,
            scan,
            resolution,
            created,
            workspace,
        })
    }

    /// The files of a scan: its own folder, and the caches of its source that this scan's block
    /// grid shares. A grid is the extent's start and the block size, because the same bytes cut
    /// into other blocks are other facts. The first scan of a grid decides where they are: a
    /// workspace from before the catalog keeps them where they already are.
    pub fn workspace_for(&self, scan: &ScanRecord, block_size: u64) -> Result<Workspace> {
        let dir = scan
            .workspace
            .as_deref()
            .context("a raw scan has no workspace folder")?;
        let grid = format!("{}-{}", scan.start, block_size);
        let known: Option<String> = self
            .connection
            .query_row(
                "SELECT dir FROM source_blocks WHERE source_id = ?1 AND grid = ?2",
                params![scan.source_id, grid],
                |row| row.get(0),
            )
            .optional()?;
        let shared = match known {
            Some(shared) => shared,
            None => {
                let shared = if scan.migrated && !dir.starts_with("sources/") {
                    dir.to_string()
                } else {
                    format!("sources/{}/blocks/{grid}", scan.source_id)
                };
                self.connection.execute(
                    "INSERT OR IGNORE INTO source_blocks (source_id, grid, dir) VALUES (?1, ?2, ?3)",
                    params![scan.source_id, grid, shared],
                )?;
                self.connection.query_row(
                    "SELECT dir FROM source_blocks WHERE source_id = ?1 AND grid = ?2",
                    params![scan.source_id, grid],
                    |row| row.get(0),
                )?
            }
        };
        Workspace::split(self.root.join(dir), self.root.join(shared))
    }

    /// Opens a stored scan on the device that is open now, which must be its source. An exact
    /// match opens it. A probable one opens it only when the person acknowledged the changed
    /// markers, and the comparison says which they are. A weak or different one never does, and
    /// neither does a source with no markers yet, which a volume's old key finds when it is opened.
    pub fn open_scan(
        &self,
        disk: &RawDisk,
        scan_id: &str,
        accept: Accept,
    ) -> Result<(ScanRecord, SourceRecord, Comparison)> {
        let scan = self.scan(scan_id)?;
        let saved = self.fingerprint_of(&scan.source_id)?.context(
            "The source of this scan has not been seen since it was stored, so it cannot be \
             told from other drives yet. Open the volume itself, and it is found by its serial number.",
        )?;
        let comparison = saved.compare(&mut MarkerReader::new(disk));
        match comparison.state {
            Verification::Exact => {}
            Verification::Probable => ensure!(
                accept == Accept::Probable,
                "The source is only a probable match for this scan ({}% of markers match). {} Opening it needs explicit acknowledgement of the changed markers.",
                comparison.confidence_percent,
                comparison.summary
            ),
            _ => anyhow::bail!(
                "The source does not match this scan, so it was not opened. {}",
                comparison.summary
            ),
        }
        // A probable match: what was stored near the changed markers may be out of date.
        if comparison.state == Verification::Probable {
            self.mark_changed(
                &scan.source_id,
                &comparison.changed_offsets,
                &StaleOptions::default(),
            )?;
        }
        self.touch_scan(scan_id)?;
        self.see(&scan.source_id, &disk.path.to_string_lossy())?;
        Ok((
            self.scan(scan_id)?,
            self.source(&scan.source_id)?,
            comparison,
        ))
    }

    // ---- raw scans ----

    /// Records a saved raw scan as a scan of a source. The source is the stored one the device
    /// matches, exactly or probably (nothing stored is applied by this: the scan is new and
    /// empty), or a new one. The interpretation is the scanned range read as raw bytes.
    pub fn register_raw_scan(
        &self,
        disk: &RawDisk,
        raw_scan_id: &str,
        start: u64,
        length: u64,
    ) -> Result<ScanRecord> {
        let resolution = self.resolve_source(disk, Accept::Probable, None)?;
        let transaction = immediate(&self.connection)?;
        let request = ScanRequest {
            source_id: resolution.source.id,
            kind: ScanKind::Raw,
            interpretation: InterpretationRequest {
                format: NO_FORMAT.into(),
                start,
                length,
                boot_offset: None,
                origin: "manual".into(),
                confidence: 0.0,
            },
            options: json!({}),
            scanner_version: crate::raw_scan::SCANNER_VERSION,
            range_start: start,
            range_end: start + length,
            volume_key: None,
            raw_scan_id: Some(raw_scan_id.into()),
            block_size: 0,
            block_count: 0,
        };
        let (scan, _) = ensure_scan(&transaction, &request)?;
        transaction.commit()?;
        self.scan(&scan.id)
    }

    /// The scan of an extent read as raw bytes (no filesystem), made when a person chooses that
    /// reading and before any raw scan has run on it. Starting a raw scan over the same extent
    /// later attaches to this row, so the choice is one scan and not two.
    pub fn raw_placeholder(&self, source_id: &str, start: u64, length: u64) -> Result<ScanRecord> {
        let transaction = immediate(&self.connection)?;
        let request = ScanRequest {
            source_id: source_id.into(),
            kind: ScanKind::Raw,
            interpretation: InterpretationRequest {
                format: NO_FORMAT.into(),
                start,
                length,
                boot_offset: None,
                origin: "manual".into(),
                confidence: 0.0,
            },
            options: json!({}),
            scanner_version: crate::raw_scan::SCANNER_VERSION,
            range_start: start,
            range_end: start + length,
            volume_key: None,
            raw_scan_id: None,
            block_size: 0,
            block_count: 0,
        };
        let (scan, _) = ensure_scan(&transaction, &request)?;
        transaction.commit()?;
        self.scan(&scan.id)
    }

    /// The scan that holds a saved raw scan, when it is registered.
    pub fn scan_of_raw(&self, raw_scan_id: &str) -> Result<Option<ScanRecord>> {
        Ok(self
            .connection
            .query_row(
                &format!("SELECT {SCAN_COLUMNS} FROM scans WHERE raw_scan_id = ?1"),
                [raw_scan_id],
                scan_row,
            )
            .optional()?)
    }
}

const INTERPRETATION_COLUMNS: &str = "h.id, h.format, h.src_start, h.src_length, h.boot_offset, \
    h.origin, h.confidence, i.listed, i.verified";

fn interpretation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<InterpretationRecord> {
    Ok(InterpretationRecord {
        id: row.get(0)?,
        format: row.get(1)?,
        start: row.get::<_, i64>(2)? as u64,
        length: row.get::<_, i64>(3)? as u64,
        boot_offset: row.get::<_, Option<i64>>(4)?.map(|value| value as u64),
        origin: row.get(5)?,
        confidence: row.get(6)?,
        listed: row.get::<_, i64>(7)? != 0,
        verified: row.get::<_, i64>(8)? != 0,
    })
}

/// Hypotheses saved before the catalog existed, for the source's sampled hash and length, become
/// interpretations of it. They stay listed, as the person saved them.
pub(super) fn adopt_saved_hypotheses(connection: &Connection, source_id: &str) -> Result<()> {
    connection.execute(
        "INSERT OR IGNORE INTO interpretations (hypothesis_id, source_id, listed, verified, created_at_millis)
         SELECT h.id, s.id, 1, 1, h.created_at_millis FROM hypotheses h
         JOIN sources s ON s.length = h.source_length
           AND json_extract(s.fingerprint, '$.sampledSha256') = h.source_key
         WHERE s.id = ?1 AND h.id NOT IN (SELECT hypothesis_id FROM interpretations)",
        [source_id],
    )?;
    Ok(())
}

pub(super) fn ensure_interpretation(
    connection: &Connection,
    source_id: &str,
    request: &InterpretationRequest,
) -> Result<InterpretationRecord> {
    ensure!(request.length > 0, "an interpretation must not be empty");
    ensure!(
        request.format == NO_FORMAT
            || crate::hypotheses::FORMATS.contains(&request.format.as_str()),
        "format must be one of {} or {NO_FORMAT}",
        crate::hypotheses::FORMATS.join(", ")
    );
    adopt_saved_hypotheses(connection, source_id)?;
    let found = connection
        .query_row(
            &format!(
                "SELECT {INTERPRETATION_COLUMNS} FROM interpretations i
                 JOIN hypotheses h ON h.id = i.hypothesis_id
                 WHERE i.source_id = ?1 AND h.format = ?2 AND h.src_start = ?3 AND h.src_length = ?4
                   AND coalesce(h.boot_offset, -1) = coalesce(?5, -1)
                 ORDER BY i.listed DESC, i.verified DESC, h.created_at_millis LIMIT 1"
            ),
            params![
                source_id,
                request.format,
                request.start as i64,
                request.length as i64,
                request.boot_offset.map(|value| value as i64)
            ],
            interpretation_row,
        )
        .optional()?;
    if let Some(found) = found {
        return Ok(found);
    }
    let (key, length, path): (Option<String>, i64, String) = connection.query_row(
        "SELECT json_extract(fingerprint, '$.sampledSha256'), length, last_path FROM sources WHERE id = ?1",
        [source_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let id = identifier()?;
    let now = now_millis() as i64;
    // Not enabled and not listed: opening a source is not a claim about its layout.
    connection.execute(
        "INSERT INTO hypotheses VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 0, 0, ?13, ?13)",
        params![
            id,
            key.unwrap_or_else(|| format!("source:{source_id}")),
            length,
            path,
            format!("{} at {}", request.format, request.start),
            request.format,
            request.start as i64,
            request.length as i64,
            request.boot_offset.map(|value| value as i64),
            request.start as i64,
            request.confidence,
            request.origin,
            now
        ],
    )?;
    connection.execute(
        "INSERT INTO interpretations (hypothesis_id, source_id, listed, verified, created_at_millis)
         VALUES (?1, ?2, 0, 1, ?3)",
        params![id, source_id, now],
    )?;
    Ok(InterpretationRecord {
        id,
        format: request.format.clone(),
        start: request.start,
        length: request.length,
        boot_offset: request.boot_offset,
        origin: request.origin.clone(),
        confidence: request.confidence,
        listed: false,
        verified: true,
    })
}

pub(super) fn ensure_scan(
    connection: &Connection,
    request: &ScanRequest,
) -> Result<(ScanRecord, bool)> {
    ensure!(
        request.range_end > request.range_start,
        "a scan's range must not be empty"
    );
    let interpretation =
        ensure_interpretation(connection, &request.source_id, &request.interpretation)?;
    let options = canonical(&request.options);
    if request.raw_scan_id.is_none() {
        let found = connection
            .query_row(
                &format!(
                    "SELECT {SCAN_COLUMNS} FROM scans
                     WHERE scans.source_id = ?1 AND scans.kind = ?2 AND scans.format = ?3
                       AND scans.ext_start = ?4 AND scans.ext_length = ?5
                       AND coalesce(scans.boot_offset, -1) = coalesce(?6, -1)
                       AND scans.options = ?7 AND scans.scanner_version = ?8
                       AND scans.range_start = ?9 AND scans.range_end = ?10
                     ORDER BY scans.updated_at_millis DESC LIMIT 1"
                ),
                params![
                    request.source_id,
                    request.kind.text(),
                    request.interpretation.format,
                    request.interpretation.start as i64,
                    request.interpretation.length as i64,
                    request.interpretation.boot_offset.map(|value| value as i64),
                    options,
                    request.scanner_version,
                    request.range_start as i64,
                    request.range_end as i64
                ],
                scan_row,
            )
            .optional()?;
        if let Some(found) = found {
            return Ok((found, false));
        }
    } else if let Some(existing) = connection
        .query_row(
            &format!("SELECT {SCAN_COLUMNS} FROM scans WHERE raw_scan_id = ?1"),
            [request.raw_scan_id.as_deref()],
            scan_row,
        )
        .optional()?
    {
        return Ok((existing, false));
    } else if let Some(placeholder) = connection
        .query_row(
            &format!(
                "SELECT {SCAN_COLUMNS} FROM scans
                 WHERE scans.source_id = ?1 AND scans.kind = 'raw' AND scans.raw_scan_id IS NULL
                   AND scans.ext_start = ?2 AND scans.ext_length = ?3 AND scans.scanner_version = ?4
                 ORDER BY scans.created_at_millis LIMIT 1"
            ),
            params![
                request.source_id,
                request.interpretation.start as i64,
                request.interpretation.length as i64,
                request.scanner_version
            ],
            scan_row,
        )
        .optional()?
    {
        // The extent was already chosen as raw bytes: the scan that now runs is that choice.
        connection.execute(
            "UPDATE scans SET raw_scan_id = ?2, updated_at_millis = ?3 WHERE id = ?1",
            params![placeholder.id, request.raw_scan_id, now_millis() as i64],
        )?;
        let attached = connection.query_row(
            &format!("SELECT {SCAN_COLUMNS} FROM scans WHERE scans.id = ?1"),
            [&placeholder.id],
            scan_row,
        )?;
        return Ok((attached, false));
    }
    let id = identifier()?;
    let now = now_millis() as i64;
    let data_dir = (request.kind == ScanKind::Filesystem)
        .then(|| format!("sources/{}/scans/{id}", request.source_id));
    connection.execute(
        "INSERT INTO scans (id, source_id, interpretation_id, kind, format, ext_start, ext_length,
            boot_offset, origin, confidence, options, scanner_version, range_start, range_end,
            raw_scan_id, data_dir, volume_key, block_size, block_count,
            created_at_millis, updated_at_millis)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?20)",
        params![
            id,
            request.source_id,
            interpretation.id,
            request.kind.text(),
            request.interpretation.format,
            request.interpretation.start as i64,
            request.interpretation.length as i64,
            request.interpretation.boot_offset.map(|value| value as i64),
            request.interpretation.origin,
            request.interpretation.confidence,
            options,
            request.scanner_version,
            request.range_start as i64,
            request.range_end as i64,
            request.raw_scan_id,
            data_dir,
            request.volume_key,
            request.block_size as i64,
            request.block_count as i64,
            now
        ],
    )?;
    let scan = connection.query_row(
        &format!("SELECT {SCAN_COLUMNS} FROM scans WHERE scans.id = ?1"),
        [&id],
        scan_row,
    )?;
    Ok((scan, true))
}

/// A scan made from an old workspace, found by the volume's old key, whose format and extent
/// were guessed. What the device says replaces the guess, and the scan is kept: it is the same
/// work. A scan the device was already seen as is not touched.
fn adopt_unverified(
    transaction: &Transaction,
    request: &ScanRequest,
) -> Result<Option<ScanRecord>> {
    let Some(key) = request.volume_key.as_deref() else {
        return Ok(None);
    };
    let found = transaction
        .query_row(
            &format!(
                "SELECT {SCAN_COLUMNS} FROM scans
                 JOIN interpretations i ON i.hypothesis_id = scans.interpretation_id
                 WHERE scans.source_id = ?1 AND scans.kind = 'filesystem' AND scans.volume_key = ?2
                   AND scans.legacy_key IS NOT NULL AND i.verified = 0
                 ORDER BY scans.updated_at_millis DESC LIMIT 1"
            ),
            params![request.source_id, key],
            scan_row,
        )
        .optional()?;
    let Some(found) = found else {
        return Ok(None);
    };
    let interpretation =
        ensure_interpretation(transaction, &request.source_id, &request.interpretation)?;
    transaction.execute(
        "UPDATE scans SET interpretation_id = ?2, format = ?3, ext_start = ?4, ext_length = ?5,
            boot_offset = ?6, origin = ?7, confidence = ?8, range_start = ?9, range_end = ?10,
            scanner_version = ?11, options = ?12 WHERE id = ?1",
        params![
            found.id,
            interpretation.id,
            request.interpretation.format,
            request.interpretation.start as i64,
            request.interpretation.length as i64,
            request.interpretation.boot_offset.map(|value| value as i64),
            request.interpretation.origin,
            request.interpretation.confidence,
            request.range_start as i64,
            request.range_end as i64,
            request.scanner_version,
            canonical(&request.options),
        ],
    )?;
    transaction.execute(
        "UPDATE interpretations SET verified = 1 WHERE hypothesis_id = ?1",
        [&interpretation.id],
    )?;
    let scan = transaction.query_row(
        &format!("SELECT {SCAN_COLUMNS} FROM scans WHERE scans.id = ?1"),
        [&found.id],
        scan_row,
    )?;
    Ok(Some(scan))
}
