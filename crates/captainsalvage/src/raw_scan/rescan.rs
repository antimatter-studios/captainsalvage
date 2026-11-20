//! Scanning the stale regions of a saved raw scan again.
//!
//! A scan resumed on a probable match has findings near the markers that changed that may be out
//! of date. This reads those regions again, with the same scanner and the same range rules as
//! the scan itself, and replaces the findings and read failures inside each batch in one
//! transaction, together with the note of how far the region has been read. Closing the app or a
//! failed read leaves the region stale and the rescan resumes where it stopped. Only when every
//! region of the scan has been read does the source get the markers of the drive as it is now,
//! and the markers it had are kept in its history.
//!
//! Nothing is written to the source: reads go through `RawDisk::discover`, as a scan does.

use super::{identifier, ScanStore, Verification, SCANNER_VERSION};
use crate::catalog::stale;
use crate::disk::{RawDisk, DISCOVERY_BATCH_BYTES};
use crate::raw_scan::{Fingerprint, MarkerReader};
use crate::stale::{Span, StaleOptions};
use crate::workspace::now_millis;
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{params, TransactionBehavior};
use serde::Serialize;

/// One batch of a rescan.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RescanStep {
    pub region: Span,
    /// The batch read from here...
    pub read_from: u64,
    /// ...up to here.
    pub read_to: u64,
    /// The region has been read to its end and is no longer stale.
    pub cleared: bool,
    /// Findings now stored for the batch, replacing the ones that were there.
    pub findings: usize,
    pub errors: usize,
}

/// What a rescan did.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RescanReport {
    /// Regions read to their end and cleared.
    pub regions_rescanned: usize,
    /// Regions still stale, because the rescan stopped or a read failed.
    pub regions_remaining: usize,
    /// Bytes read again.
    pub bytes_rescanned: u64,
    /// The source has the markers of the drive as it is now, and kept the earlier ones.
    pub markers_rerecorded: bool,
    /// Why the rescan stopped before it was done, if it did.
    pub stopped: Option<String>,
}

impl ScanStore {
    /// Binds a saved raw scan to this process for a rescan, as `resume` does for a scan, but
    /// without moving the scan's own position. The source must match its markers exactly, or
    /// probably, in which case the stale regions are marked now too, so the rescan has them to
    /// read whichever way the scan was opened.
    pub fn rescan_begin(&mut self, disk: &RawDisk, id: &str) -> Result<()> {
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
        let comparison = saved.compare(&mut MarkerReader::new(disk));
        match comparison.state {
            Verification::Exact => {}
            Verification::Probable => self.mark_changed(id, &comparison)?,
            _ => bail!(
                "The source does not match this saved scan, so its regions were not scanned again. {}",
                comparison.summary
            ),
        }
        let token = identifier()?;
        self.connection.execute(
            "UPDATE raw_scans SET source_token = ?2, source_path = ?3 WHERE id = ?1",
            params![id, token, disk.path.to_string_lossy()],
        )?;
        bindings.insert(id.into(), token);
        Ok(())
    }

    /// Widens the stale regions of a saved raw scan with other options, from the markers that
    /// made them. Regions are never made smaller.
    pub fn widen_stale(&self, id: &str, options: &StaleOptions) -> Result<()> {
        let Some(root) = &self.catalog_root else {
            return Ok(());
        };
        let Some(scan) = self.catalog_scan_of(id)? else {
            return Ok(());
        };
        let markers: Vec<u64> = stale::stale_regions(&self.connection, &scan)?
            .into_iter()
            .flat_map(|region| region.markers)
            .collect();
        let catalog = crate::catalog::Catalog::open(root)?;
        let record = catalog.scan(&scan)?;
        catalog.mark_changed(&record.source_id, &markers, options)?;
        Ok(())
    }

    /// Reads the next batch of the first stale region of a saved raw scan again. The scan must
    /// have been bound by `rescan_begin` or `resume`. None when no region is stale.
    pub fn rescan_step(&mut self, disk: &RawDisk, id: &str) -> Result<Option<RescanStep>> {
        let bindings = disk
            .scan_tokens
            .lock()
            .map_err(|_| anyhow::anyhow!("scan lock poisoned"))?;
        let token = bindings
            .get(id)
            .context("Verify and load this saved scan before continuing.")?;
        let scan = self
            .catalog_scan_of(id)?
            .context("This scan is not registered in the catalog, so it has no stale regions.")?;
        // Read without the write lock, as `step` does: the disk is read before it is taken, and what was read is
        // only written when the region is still where it was.
        let saved_token: String = self.connection.query_row(
            "SELECT source_token FROM raw_scans WHERE id = ?1",
            [id],
            |row| row.get(0),
        )?;
        ensure!(
            *token == saved_token,
            "This scan was opened elsewhere; verify and load it again."
        );
        let Some(region) = stale::stale_regions(&self.connection, &scan)?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        let span = region.span();
        let position = region.done_to.clamp(span.start, span.end);
        let length = (span.end - position).min(DISCOVERY_BATCH_BYTES);
        ensure!(length > 0, "the region has nothing left to read");
        let batch = disk.discover(position, length)?;
        ensure!(
            batch.next_offset > position && batch.next_offset <= span.end,
            "Discovery did not advance within the stale region."
        );
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let again = stale::stale_regions(&transaction, &scan)?
            .into_iter()
            .next();
        ensure!(
            again.is_some_and(
                |now| now.span() == span && now.done_to.clamp(span.start, span.end) == position
            ),
            "The stale region moved while the disk was being read; step again."
        );
        transaction.execute(
            "DELETE FROM raw_findings WHERE scan_id = ?1 AND offset >= ?2 AND offset < ?3",
            params![id, position as i64, batch.next_offset as i64],
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
            "DELETE FROM raw_failures WHERE scan_id = ?1 AND offset >= ?2 AND offset < ?3",
            params![id, position as i64, batch.next_offset as i64],
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
        let cleared = batch.next_offset >= span.end;
        stale::advance(&transaction, &scan, &span, batch.next_offset)?;
        if cleared {
            stale::clear(&transaction, &scan, &span)?;
        }
        transaction.execute(
            "UPDATE raw_scans SET updated_at_millis = ?2 WHERE id = ?1",
            params![id, now_millis() as i64],
        )?;
        transaction.commit()?;
        Ok(Some(RescanStep {
            region: span,
            read_from: position,
            read_to: batch.next_offset,
            cleared,
            findings: batch.findings.len(),
            errors: batch.errors.len(),
        }))
    }

    /// Reads every stale region of a saved raw scan again, a batch at a time, then gives the
    /// source the markers of the drive as it is now. `on_step` sees each batch and returns
    /// whether to go on; a read that fails or a drive that goes away stops the rescan with the
    /// regions still stale, and the next rescan resumes from where this one stopped.
    pub fn rescan_changed(
        &mut self,
        disk: &RawDisk,
        id: &str,
        mut on_step: impl FnMut(&RescanStep) -> bool,
    ) -> Result<RescanReport> {
        let scan = self
            .catalog_scan_of(id)?
            .context("This scan is not registered in the catalog, so it has no stale regions.")?;
        let mut report = RescanReport::default();
        let mut stepped = false;
        loop {
            match self.rescan_step(disk, id) {
                Ok(Some(step)) => {
                    stepped = true;
                    report.bytes_rescanned += step.read_to - step.read_from;
                    if step.cleared {
                        report.regions_rescanned += 1;
                    }
                    if !on_step(&step) {
                        report.stopped = Some("The rescan was stopped.".into());
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    report.stopped = Some(format!("{error:#}"));
                    break;
                }
            }
        }
        report.regions_remaining = stale::stale_count(&self.connection, &scan)? as usize;
        // Nothing stale means nothing was read again, and the markers are as they were.
        if stepped && report.regions_remaining == 0 && report.stopped.is_none() {
            report.markers_rerecorded = self.rerecord(disk, &scan)?;
        }
        Ok(report)
    }

    /// Gives the scan's source the markers of the drive as it is now. Called only when every
    /// stale region of the scan has been read again.
    fn rerecord(&self, disk: &RawDisk, scan: &str) -> Result<bool> {
        if disk.source.is_gone() {
            return Ok(false);
        }
        let source: String = self.connection.query_row(
            "SELECT source_id FROM scans WHERE id = ?1",
            [scan],
            |row| row.get(0),
        )?;
        let current = Fingerprint::read(disk);
        if current.samples.is_empty() {
            return Ok(false);
        }
        stale::rerecord_markers(
            &self.connection,
            &source,
            &current,
            "rescan of changed regions",
        )
    }
}
