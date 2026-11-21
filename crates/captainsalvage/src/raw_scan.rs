use crate::catalog::StaleSummary;
use crate::disk::{DiscoveryBatch, RawDisk, ReadFailure, DISCOVERY_BATCH_BYTES};
use crate::filesystem::probe::Evidence;
use crate::stale::{overlaps_any, Span, StaleOptions};
use crate::workspace::{now_millis, Workspace};
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

mod markers;
mod rescan;
use markers::hex;
pub use markers::{
    Comparison, Fingerprint, MarkerReader, Sample, Verification, CAREFUL_MARKER_READS,
    MARKER_BYTES, MARKER_COUNT, MIN_MATCHED_MARKERS, PROBABLE_PERCENT,
};
pub use rescan::{RescanReport, RescanStep};

pub(crate) const SCANNER_VERSION: u32 = 1;

fn identifier() -> Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(hex(&bytes))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanSummary {
    pub id: String,
    pub scan_start: u64,
    pub scan_end: u64,
    pub next_offset: u64,
    pub updated_at_millis: u64,
    pub verification: Verification,
    /// Which markers match, changed or could not be read, and what that means.
    pub comparison: Comparison,
    /// Set when this scan was resumed on a source that had changed.
    pub changed_source: Option<ChangedSource>,
    /// How many areas of its findings may be out of date and have not been scanned again.
    pub stale_regions: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedScans {
    /// The markers of the source that is open now.
    pub fingerprint: Fingerprint,
    pub scans: Vec<ScanSummary>,
    /// Saved scans left out because their confidence is below 60 percent. `show_all` lists them.
    pub hidden: usize,
}

/// The record that a scan resumed on a source that was only a probable match. Findings near the
/// changed markers may be stale: they are marked as such (see `stale`) until rescanned, and
/// `ScanStore::rescan_changed` reads those regions again.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChangedSource {
    /// How many times the scan was resumed on a changed source.
    pub resumes: u32,
    pub resumed_at_millis: u64,
    /// The latest comparison.
    pub confidence_percent: u32,
    pub matched: usize,
    pub changed: usize,
    pub unreadable: usize,
    pub total: usize,
    /// Every marker offset that has been seen changed on any of those resumes.
    pub changed_offsets: Vec<u64>,
    pub summary: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanSnapshot {
    pub scan_id: String,
    pub scan_start: u64,
    pub scan_end: u64,
    pub next_offset: u64,
    pub findings: Vec<Evidence>,
    pub errors: Vec<ReadFailure>,
    pub changed_source: Option<ChangedSource>,
    /// The areas whose findings may be out of date, and the ones scanned again since. A finding
    /// inside a stale one has `stale` set.
    pub stale: StaleSummary,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanStep {
    pub scan_id: String,
    #[serde(flatten)]
    pub batch: DiscoveryBatch,
}

/// The tables of saved raw scans. Created here, and by the catalog, which refers to them.
pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS raw_scans (
                 id TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, source_length INTEGER NOT NULL,
                 source_path TEXT NOT NULL, scanner_version INTEGER NOT NULL,
                 scan_start INTEGER NOT NULL, scan_end INTEGER NOT NULL, next_offset INTEGER NOT NULL,
                 source_token TEXT NOT NULL, attempt_start INTEGER NOT NULL,
                 created_at_millis INTEGER NOT NULL, updated_at_millis INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS raw_scans_length ON raw_scans(source_length);
             CREATE TABLE IF NOT EXISTS raw_findings (
                 scan_id TEXT NOT NULL REFERENCES raw_scans(id), offset INTEGER NOT NULL,
                 format TEXT NOT NULL, structure TEXT NOT NULL, evidence TEXT NOT NULL,
                 PRIMARY KEY(scan_id, offset, format, structure)
             );
             CREATE TABLE IF NOT EXISTS raw_failures (
                 scan_id TEXT NOT NULL REFERENCES raw_scans(id), offset INTEGER NOT NULL,
                 length INTEGER NOT NULL, error TEXT NOT NULL, source_token TEXT NOT NULL,
                 PRIMARY KEY(scan_id, offset)
             );
             CREATE TABLE IF NOT EXISTS raw_scan_physical (
                 scan_id TEXT NOT NULL REFERENCES raw_scans(id), physical TEXT NOT NULL,
                 PRIMARY KEY(scan_id, physical)
             );
             CREATE TABLE IF NOT EXISTS raw_scan_changes (
                 scan_id TEXT PRIMARY KEY REFERENCES raw_scans(id), resumes INTEGER NOT NULL,
                 resumed_at_millis INTEGER NOT NULL, confidence_percent INTEGER NOT NULL,
                 matched INTEGER NOT NULL, changed INTEGER NOT NULL, unreadable INTEGER NOT NULL,
                 total INTEGER NOT NULL, changed_offsets TEXT NOT NULL, summary TEXT NOT NULL
             );";

pub struct ScanStore {
    connection: Connection,
    /// Where the app's folder is, when the store is the app's own: scans made are then registered
    /// in the catalog as scans of a source. A store opened on another file does not.
    catalog_root: Option<std::path::PathBuf>,
}

impl ScanStore {
    pub fn for_user() -> Result<Self> {
        Self::open_in(&Workspace::root()?)
    }

    /// The store of the app's folder `root`: its scans are also registered in the catalog.
    pub fn open_in(root: &Path) -> Result<Self> {
        let mut store = Self::open(&root.join("raw-scans.sqlite"))?;
        store.catalog_root = Some(root.to_path_buf());
        Ok(store)
    }

    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;",
        )?;
        connection.execute_batch(SCHEMA)?;
        connection.execute_batch(crate::catalog::SCHEMA)?;
        Ok(Self {
            connection,
            catalog_root: None,
        })
    }

    pub fn list(&self, disk: &RawDisk) -> Result<SavedScans> {
        self.list_matching(disk, false)
    }

    /// Saved scans of sources with this length, each compared with the open source marker by
    /// marker. Scans below 60 percent are left out and counted, unless `show_all` is set, which
    /// lists those above 0 percent. A different source is never listed.
    pub fn list_matching(&self, disk: &RawDisk, show_all: bool) -> Result<SavedScans> {
        let mut reader = MarkerReader::new(disk);
        let fingerprint = Fingerprint::read_with(&mut reader);
        let mut statement = self.connection.prepare(
            "SELECT id, fingerprint, scanner_version, scan_start, scan_end,
                    next_offset, updated_at_millis FROM raw_scans
             WHERE source_length = ?1 ORDER BY updated_at_millis DESC",
        )?;
        let mut rows = statement.query(params![i64::try_from(disk.length)?])?;
        let mut scans = Vec::new();
        let mut hidden = 0;
        while let Some(row) = rows.next()? {
            let saved: Fingerprint = serde_json::from_str(&row.get::<_, String>(1)?)?;
            let saved_id: String = row.get(0)?;
            let comparison = saved.compare(&mut reader);
            let comparison = if self.is_same_drive(disk, &saved_id)? {
                comparison.as_same_drive()
            } else {
                comparison
            };
            let verification = match comparison.state {
                Verification::Different => continue,
                _ if row.get::<_, u32>(2)? != SCANNER_VERSION => Verification::Incompatible,
                state => state,
            };
            if verification == Verification::Weak && !show_all {
                hidden += 1;
                continue;
            }
            let id: String = row.get(0)?;
            scans.push(ScanSummary {
                changed_source: self.changed_source(&id)?,
                stale_regions: self.stale_count_of_raw(&id)?,
                id,
                scan_start: row.get::<_, i64>(3)? as u64,
                scan_end: row.get::<_, i64>(4)? as u64,
                next_offset: row.get::<_, i64>(5)? as u64,
                updated_at_millis: row.get::<_, i64>(6)? as u64,
                verification,
                comparison,
            });
        }
        Ok(SavedScans {
            fingerprint,
            scans,
            hidden,
        })
    }

    pub fn create(&mut self, disk: &RawDisk, start: u64, length: u64) -> Result<ScanSnapshot> {
        ensure!(
            length > 0
                && start
                    .checked_add(length)
                    .is_some_and(|end| end <= disk.length),
            "scan range is outside the raw source"
        );
        ensure!(
            disk.length <= i64::MAX as u64,
            "source exceeds saved scan offset limits"
        );
        let mut bindings = disk
            .scan_tokens
            .lock()
            .map_err(|_| anyhow::anyhow!("scan lock poisoned"))?;
        let fingerprint = Fingerprint::read(disk);
        ensure!(
            !disk.source.is_gone(),
            "The disk has been disconnected; scan not started."
        );
        let id = identifier()?;
        let token = identifier()?;
        let now = now_millis();
        self.connection.execute(
            "INSERT INTO raw_scans VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?6, ?8, ?6, ?9, ?9)",
            params![
                id,
                serde_json::to_string(&fingerprint)?,
                disk.length as i64,
                disk.path.to_string_lossy(),
                SCANNER_VERSION,
                start as i64,
                (start + length) as i64,
                token,
                now as i64
            ],
        )?;
        if let Some(physical) = disk.physical_identity() {
            self.connection.execute(
                "INSERT OR REPLACE INTO raw_scan_physical VALUES (?1, ?2)",
                params![id, physical],
            )?;
        }
        bindings.insert(id.clone(), token);
        self.register(disk, &id, start, length);
        self.snapshot(&id)
    }

    /// Records the scan in the catalog, as a scan of the source the device is. The scan is saved
    /// whether or not this works: the next run of the app registers it from what it saved.
    fn register(&self, disk: &RawDisk, id: &str, start: u64, length: u64) {
        let Some(root) = &self.catalog_root else {
            return;
        };
        let result = crate::catalog::Catalog::open(root)
            .and_then(|catalog| catalog.register_raw_scan(disk, id, start, length));
        if let Err(error) = result {
            eprintln!("could not register the scan in the catalog: {error:#}");
        }
    }

    pub fn resume(
        &mut self,
        disk: &RawDisk,
        id: &str,
        accept_sampled_match: bool,
        accept_changed_markers: bool,
    ) -> Result<ScanSnapshot> {
        ensure!(accept_sampled_match, "Resuming requires acknowledgement that sampled matches do not prove the entire source is identical.");
        self.resume_with(disk, id, accept_changed_markers, false)
    }

    /// Resumes a saved scan on a source that does not match it, because the person said to. The
    /// scan records that it was resumed on a source it could not be matched to.
    pub fn resume_anyway(&mut self, disk: &RawDisk, id: &str) -> Result<ScanSnapshot> {
        self.resume_with(disk, id, true, true)
    }

    /// Whether the saved scan was made on this same part of this same physical drive.
    fn is_same_drive(&self, disk: &RawDisk, id: &str) -> Result<bool> {
        let Some(now) = disk.physical_identity() else {
            return Ok(false);
        };
        Ok(self.connection.query_row(
            "SELECT count(*) > 0 FROM raw_scan_physical WHERE scan_id = ?1 AND physical = ?2",
            params![id, now],
            |row| row.get(0),
        )?)
    }

    fn resume_with(
        &mut self,
        disk: &RawDisk,
        id: &str,
        accept_changed_markers: bool,
        anyway: bool,
    ) -> Result<ScanSnapshot> {
        let mut bindings = disk
            .scan_tokens
            .lock()
            .map_err(|_| anyhow::anyhow!("scan lock poisoned"))?;
        let (fingerprint, scanner_version): (String, u32) = self
            .connection
            .query_row(
                "SELECT fingerprint, scanner_version FROM raw_scans WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .context("saved scan not found")?;
        ensure!(
            scanner_version == SCANNER_VERSION,
            "Saved scan uses an incompatible scanner; start a new scan."
        );
        let saved: Fingerprint = serde_json::from_str(&fingerprint)?;
        // The same part of the same physical drive is the scan's source whatever its blocks read
        // now, so what the markers say never stops it. Markers decide for another device (a
        // clone, an image) or a drive of unknown identity. Either way a marker in a bad zone is
        // skipped, not changed, and the markers that changed mark the regions around them stale.
        // They are read before the database is locked, once each and within a time limit.
        let same_drive = self.is_same_drive(disk, id)?;
        let mut comparison = saved.compare(&mut MarkerReader::new(disk));
        if same_drive {
            comparison = comparison.as_same_drive();
        }
        if anyway
            && !matches!(
                comparison.state,
                Verification::Exact | Verification::Probable
            )
        {
            comparison.summary = format!(
                "Resumed without a match, because it was asked for. {}",
                comparison.summary
            );
            comparison.state = Verification::Probable;
        }
        match comparison.state {
            Verification::Exact => {}
            Verification::Probable => ensure!(
                accept_changed_markers,
                "The source is only a probable match for this saved scan ({}% of markers match). {} Resuming needs explicit acknowledgement of the changed markers.",
                comparison.confidence_percent,
                comparison.summary
            ),
            _ => bail!(
                "Source fingerprint differs or could not be verified; saved scan was not resumed. {}",
                comparison.summary
            ),
        }
        let changed =
            comparison.state == Verification::Probable || !comparison.changed_offsets.is_empty();
        if changed {
            // What was stored near the changed markers may be out of date. This is done before
            // the scan is bound, so a failure to mark is not left behind a resumed scan.
            self.mark_changed(id, &comparison)?;
        }
        // Earlier failures are retried only where they read now (a clone, a repaired drive). On the
        // same damaged drive they fail again, so the checkpoint stays where the scan got to, and
        // those failures stay listed. Rewinding to the first failure of a damaged drive would make
        // every resume start over from it.
        let (retry_from, still_failing) = self.retry_point(disk, id)?;
        let token = identifier()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE raw_scans SET next_offset = min(next_offset, ?5),
                source_token = ?2, source_path = ?3, updated_at_millis = ?4 WHERE id = ?1",
            params![
                id,
                token,
                disk.path.to_string_lossy(),
                now_millis() as i64,
                retry_from
            ],
        )?;
        for offset in still_failing {
            transaction.execute(
                "UPDATE raw_failures SET source_token = ?3 WHERE scan_id = ?1 AND offset = ?2",
                params![id, offset, token],
            )?;
        }
        transaction.execute(
            "UPDATE raw_scans SET attempt_start = next_offset WHERE id = ?1",
            [id],
        )?;
        if changed {
            record_changed_source(&transaction, id, &comparison)?;
        }
        if comparison.state == Verification::Exact {
            if let Some(physical) = disk.physical_identity() {
                // A scan made before drives were recognised by identity learns this one on an exact match.
                transaction.execute(
                    "INSERT OR IGNORE INTO raw_scan_physical VALUES (?1, ?2)",
                    params![id, physical],
                )?;
            }
        }
        transaction.commit()?;
        bindings.insert(id.into(), token);
        self.touch(id);
        self.snapshot(id)
    }

    /// Where a resumed scan goes back to retry earlier read failures: the first one that reads on
    /// this source now (`i64::MAX` when none does), and the offsets of those that still do not. A
    /// failure in a range the health map marks bad, or any failure on a disk that has reset, is
    /// not read again.
    fn retry_point(&self, disk: &RawDisk, id: &str) -> Result<(i64, Vec<i64>)> {
        let mut statement = self.connection.prepare(
            "SELECT offset, length FROM raw_failures WHERE scan_id = ?1 ORDER BY offset",
        )?;
        let failures = statement
            .query_map([id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut retry_from = i64::MAX;
        let mut still_failing = Vec::new();
        for (offset, length) in failures {
            let (start, size) = (offset as u64, length as u64);
            let readable = !disk.careful_open()
                && disk.known_bad(start, size).is_none()
                && size <= DISCOVERY_BATCH_BYTES
                && disk.read(start, size).is_ok();
            if readable {
                retry_from = retry_from.min(offset);
            } else {
                still_failing.push(offset);
            }
        }
        Ok((retry_from, still_failing))
    }

    pub fn current(&self, disk: &RawDisk, id: &str) -> Result<ScanSnapshot> {
        let bindings = disk
            .scan_tokens
            .lock()
            .map_err(|_| anyhow::anyhow!("scan lock poisoned"))?;
        let token = bindings
            .get(id)
            .context("Verify and load this saved scan before continuing.")?;
        let transaction = self.connection.unchecked_transaction()?;
        let saved_token: String = transaction.query_row(
            "SELECT source_token FROM raw_scans WHERE id = ?1",
            [id],
            |row| row.get(0),
        )?;
        ensure!(
            *token == saved_token,
            "This scan was opened elsewhere; verify and load it again."
        );
        let result = self.snapshot(id)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn step(&mut self, disk: &RawDisk, id: &str) -> Result<ScanStep> {
        let bindings = disk
            .scan_tokens
            .lock()
            .map_err(|_| anyhow::anyhow!("scan lock poisoned"))?;
        let token = bindings
            .get(id)
            .context("Verify and load this saved scan before continuing.")?;
        // What the scan has done so far is read without the write lock: the disk is read next, which can take a
        // long time on a failing drive, and nothing else that writes to this catalog (the read journal of the same
        // process among them) may wait for it. The write lock is taken after, for the writes only.
        let (start, end, saved_token, attempt_start): (i64, i64, String, i64) = self.connection.query_row(
            "SELECT next_offset, scan_end, source_token, attempt_start FROM raw_scans WHERE id = ?1", [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        ensure!(
            *token == saved_token,
            "This scan was opened elsewhere; verify and load it again."
        );
        ensure!(start < end, "Scan is already complete.");
        let batch = disk.discover(
            start as u64,
            ((end - start) as u64).min(DISCOVERY_BATCH_BYTES),
        )?;
        ensure!(
            batch.next_offset > start as u64 && batch.next_offset <= end as u64,
            "Discovery did not advance within the saved range."
        );
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (now_start, now_token): (i64, String) = transaction.query_row(
            "SELECT next_offset, source_token FROM raw_scans WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        ensure!(
            now_start == start && *token == now_token,
            "This scan moved while the disk was being read; step again."
        );
        transaction.execute(
            "DELETE FROM raw_findings WHERE scan_id = ?1 AND offset >= ?2 AND offset < ?3",
            params![id, start, batch.next_offset as i64],
        )?;
        for evidence in &batch.findings {
            transaction.execute(
                "INSERT OR REPLACE INTO raw_findings VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id,
                    evidence.offset as i64,
                    evidence.format,
                    evidence.structure,
                    serde_json::to_string(evidence)?
                ],
            )?;
        }
        transaction.execute(
            "DELETE FROM raw_failures WHERE scan_id = ?1 AND source_token != ?2
            AND offset >= ?3 AND offset + length <= ?4",
            params![id, token, attempt_start, batch.next_offset as i64],
        )?;
        for failure in &batch.errors {
            transaction.execute(
                "INSERT OR REPLACE INTO raw_failures VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id,
                    failure.offset as i64,
                    failure.length as i64,
                    failure.error,
                    token
                ],
            )?;
        }
        transaction.execute(
            "UPDATE raw_scans SET next_offset = ?2, updated_at_millis = ?3 WHERE id = ?1",
            params![id, batch.next_offset as i64, now_millis() as i64],
        )?;
        transaction.commit()?;
        Ok(ScanStep {
            scan_id: id.into(),
            batch,
        })
    }

    /// Notes in the catalog that this scan was opened, which makes its interpretation the one
    /// last used.
    fn touch(&self, id: &str) {
        let Some(root) = &self.catalog_root else {
            return;
        };
        if let Ok(catalog) = crate::catalog::Catalog::open(root) {
            if let Ok(Some(scan)) = catalog.scan_of_raw(id) {
                let _ = catalog.touch_scan(&scan.id);
            }
        }
    }

    fn snapshot(&self, id: &str) -> Result<ScanSnapshot> {
        let (start, end, next, token): (i64, i64, i64, String) = self.connection.query_row(
            "SELECT scan_start, scan_end, next_offset, source_token FROM raw_scans WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let stale = match self.catalog_scan_of(id)? {
            Some(scan) => crate::catalog::stale::summary(&self.connection, &scan)?,
            None => StaleSummary::default(),
        };
        let stale_spans: Vec<Span> = stale
            .regions
            .iter()
            .filter(|region| region.is_stale())
            .map(|region| region.span())
            .collect();
        let mut findings: Vec<Evidence> = Vec::new();
        let mut statement = self.connection.prepare("SELECT evidence FROM raw_findings WHERE scan_id = ?1 ORDER BY offset, format, structure")?;
        let mut rows = statement.query([id])?;
        while let Some(row) = rows.next()? {
            let mut evidence: Evidence = serde_json::from_str(&row.get::<_, String>(0)?)?;
            evidence.stale = overlaps_any(&stale_spans, evidence.offset, evidence.length);
            findings.push(evidence);
        }
        let mut statement = self.connection.prepare(
            "SELECT offset, length, error FROM raw_failures
            WHERE scan_id = ?1 AND source_token = ?2 ORDER BY offset",
        )?;
        let errors = statement
            .query_map(params![id, token], |row| {
                Ok(ReadFailure {
                    offset: row.get::<_, i64>(0)? as u64,
                    length: row.get::<_, i64>(1)? as u64,
                    error: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ScanSnapshot {
            scan_id: id.into(),
            scan_start: start as u64,
            scan_end: end as u64,
            next_offset: next as u64,
            findings,
            errors,
            changed_source: self.changed_source(id)?,
            stale,
        })
    }

    /// The catalog's scan that holds this saved raw scan, when it is registered.
    pub(crate) fn catalog_scan_of(&self, raw_scan_id: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "SELECT id FROM scans WHERE raw_scan_id = ?1",
                [raw_scan_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    fn stale_count_of_raw(&self, raw_scan_id: &str) -> Result<u32> {
        match self.catalog_scan_of(raw_scan_id)? {
            Some(scan) => crate::catalog::stale::stale_count(&self.connection, &scan),
            None => Ok(0),
        }
    }

    /// Marks the area around the changed markers stale, for the scans of this scan's source.
    /// Needs the catalog (the store of the app's own folder): a store opened on another file
    /// keeps no sources, so there is nothing to mark.
    fn mark_changed(&self, raw_scan_id: &str, comparison: &Comparison) -> Result<()> {
        let Some(root) = &self.catalog_root else {
            return Ok(());
        };
        let catalog = crate::catalog::Catalog::open(root)?;
        if let Some(scan) = catalog.scan_of_raw(raw_scan_id)? {
            catalog.mark_changed(
                &scan.source_id,
                &comparison.changed_offsets,
                &StaleOptions::default(),
            )?;
        }
        Ok(())
    }

    fn changed_source(&self, id: &str) -> Result<Option<ChangedSource>> {
        self.connection
            .query_row(
                "SELECT resumes, resumed_at_millis, confidence_percent, matched, changed,
                        unreadable, total, changed_offsets, summary
                 FROM raw_scan_changes WHERE scan_id = ?1",
                [id],
                |row| {
                    Ok(ChangedSource {
                        resumes: row.get(0)?,
                        resumed_at_millis: row.get::<_, i64>(1)? as u64,
                        confidence_percent: row.get(2)?,
                        matched: row.get::<_, i64>(3)? as usize,
                        changed: row.get::<_, i64>(4)? as usize,
                        unreadable: row.get::<_, i64>(5)? as usize,
                        total: row.get::<_, i64>(6)? as usize,
                        changed_offsets: serde_json::from_str(&row.get::<_, String>(7)?)
                            .unwrap_or_default(),
                        summary: row.get(8)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }
}

/// Keeps what a probable match changed: the offsets of every marker seen changed, joined over
/// all such resumes, so the regions around them can be rescanned later.
fn record_changed_source(
    transaction: &rusqlite::Transaction,
    id: &str,
    comparison: &Comparison,
) -> Result<()> {
    let earlier: Option<(u32, String)> = transaction
        .query_row(
            "SELECT resumes, changed_offsets FROM raw_scan_changes WHERE scan_id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (resumes, mut offsets) = match earlier {
        Some((resumes, offsets)) => (
            resumes,
            serde_json::from_str::<Vec<u64>>(&offsets).unwrap_or_default(),
        ),
        None => (0, Vec::new()),
    };
    offsets.extend(&comparison.changed_offsets);
    offsets.sort_unstable();
    offsets.dedup();
    transaction.execute(
        "INSERT OR REPLACE INTO raw_scan_changes VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            id,
            resumes + 1,
            now_millis() as i64,
            comparison.confidence_percent,
            comparison.matched as i64,
            comparison.changed as i64,
            comparison.unreadable as i64,
            comparison.total as i64,
            serde_json::to_string(&offsets)?,
            comparison.summary
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
