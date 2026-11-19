//! Stale regions of a scan, and the markers of a source as they were before a rescan.
//!
//! A scan resumed on a probable match has stored results near the markers that changed. Those
//! areas are recorded here as stale regions of the scan, in the offsets of the scan's source,
//! until a rescan has read them again. The rows only add: nothing a scan stored is changed by
//! marking, and a region is cleared (kept, with the time) and never deleted by a rescan.
//!
//! The functions take a connection, so the saved raw scans (which hold their own connection to
//! the same database file) and the catalog share them.

use super::Catalog;
use crate::raw_scan::Fingerprint;
use crate::stale::{clip, derive_regions, merge, Span, StaleOptions};
use crate::workspace::now_millis;
use anyhow::{ensure, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

/// One area of a scan that may be out of date.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StaleRegion {
    pub start: u64,
    pub end: u64,
    /// When it was marked.
    pub marked_at_millis: u64,
    /// The offsets of the changed markers it was made from.
    pub markers: Vec<u64>,
    /// How far a rescan has read it: `start` when none has.
    pub done_to: u64,
    /// When a rescan finished it. None while the region is stale.
    pub cleared_at_millis: Option<u64>,
}

impl StaleRegion {
    pub fn span(&self) -> Span {
        Span {
            start: self.start,
            end: self.end,
        }
    }

    pub fn is_stale(&self) -> bool {
        self.cleared_at_millis.is_none()
    }
}

/// What a window or a tool needs to say that results may be out of date.
#[derive(Clone, Debug, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct StaleSummary {
    /// The regions still stale, and the ones cleared since.
    pub regions: Vec<StaleRegion>,
    /// How many are still stale.
    pub stale: usize,
    /// Their size together, in bytes.
    pub stale_bytes: u64,
}

fn region_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StaleRegion> {
    Ok(StaleRegion {
        start: row.get::<_, i64>(0)? as u64,
        end: row.get::<_, i64>(1)? as u64,
        marked_at_millis: row.get::<_, i64>(2)? as u64,
        markers: serde_json::from_str(&row.get::<_, String>(3)?).unwrap_or_default(),
        done_to: row.get::<_, i64>(4)? as u64,
        cleared_at_millis: row.get::<_, Option<i64>>(5)?.map(|value| value as u64),
    })
}

const REGION_COLUMNS: &str = "start, end, marked_at_millis, markers, done_to, cleared_at_millis";

/// Every region of a scan, stale ones first, by position.
pub(crate) fn regions(connection: &Connection, scan_id: &str) -> Result<Vec<StaleRegion>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {REGION_COLUMNS} FROM scan_stale_regions WHERE scan_id = ?1
         ORDER BY cleared_at_millis IS NOT NULL, start, end"
    ))?;
    let rows = statement.query_map([scan_id], region_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The regions of a scan that are still stale, by position.
pub(crate) fn stale_regions(connection: &Connection, scan_id: &str) -> Result<Vec<StaleRegion>> {
    Ok(regions(connection, scan_id)?
        .into_iter()
        .filter(StaleRegion::is_stale)
        .collect())
}

pub(crate) fn summary(connection: &Connection, scan_id: &str) -> Result<StaleSummary> {
    let regions = regions(connection, scan_id)?;
    let stale: Vec<&StaleRegion> = regions.iter().filter(|region| region.is_stale()).collect();
    Ok(StaleSummary {
        stale: stale.len(),
        stale_bytes: stale.iter().map(|region| region.span().len()).sum(),
        regions,
    })
}

/// How many regions of a scan are stale.
pub(crate) fn stale_count(connection: &Connection, scan_id: &str) -> Result<u32> {
    Ok(connection.query_row(
        "SELECT count(*) FROM scan_stale_regions WHERE scan_id = ?1 AND cleared_at_millis IS NULL",
        [scan_id],
        |row| row.get::<_, i64>(0),
    )? as u32)
}

/// Marks `spans` stale for a scan, joined with the regions already stale. Marking the same
/// regions again changes nothing, and keeps how far a rescan had read them. A region that grows
/// (another marker changed near it) starts its rescan over. Returns how many regions are stale.
pub(crate) fn mark_stale(
    connection: &Connection,
    scan_id: &str,
    spans: &[Span],
    markers: &[u64],
) -> Result<usize> {
    ensure!(
        spans.iter().all(|span| !span.is_empty()),
        "a stale region must not be empty"
    );
    let transaction =
        rusqlite::Transaction::new_unchecked(connection, rusqlite::TransactionBehavior::Immediate)?;
    let existing = stale_regions(&transaction, scan_id)?;
    let mut all: Vec<Span> = existing.iter().map(StaleRegion::span).collect();
    all.extend_from_slice(spans);
    let joined = merge(all);
    let now = now_millis() as i64;
    for span in &joined {
        let inside: Vec<&StaleRegion> = existing
            .iter()
            .filter(|region| region.start >= span.start && region.end <= span.end)
            .collect();
        if inside.len() == 1 && inside[0].span() == *span {
            continue;
        }
        let mut all_markers: Vec<u64> = inside
            .iter()
            .flat_map(|region| region.markers.iter().copied())
            .chain(markers.iter().copied())
            .collect();
        all_markers.sort_unstable();
        all_markers.dedup();
        transaction.execute(
            "DELETE FROM scan_stale_regions WHERE scan_id = ?1 AND start >= ?2 AND end <= ?3
             AND cleared_at_millis IS NULL",
            params![scan_id, span.start as i64, span.end as i64],
        )?;
        // A region cleared earlier with the same bounds is replaced: it is stale again, and the
        // time it was marked is what results made before it are compared with.
        transaction.execute(
            "DELETE FROM scan_stale_regions WHERE scan_id = ?1 AND start = ?2 AND end = ?3",
            params![scan_id, span.start as i64, span.end as i64],
        )?;
        transaction.execute(
            "INSERT INTO scan_stale_regions
             (scan_id, start, end, marked_at_millis, markers, done_to, cleared_at_millis)
             VALUES (?1, ?2, ?3, ?4, ?5, ?2, NULL)",
            params![
                scan_id,
                span.start as i64,
                span.end as i64,
                now,
                serde_json::to_string(&all_markers)?
            ],
        )?;
    }
    let count = stale_count(&transaction, scan_id)? as usize;
    transaction.commit()?;
    Ok(count)
}

/// Notes how far a rescan has read a region. Never moves backwards.
pub(crate) fn advance(
    connection: &Connection,
    scan_id: &str,
    region: &Span,
    done_to: u64,
) -> Result<()> {
    connection.execute(
        "UPDATE scan_stale_regions SET done_to = max(done_to, ?4)
         WHERE scan_id = ?1 AND start = ?2 AND end = ?3 AND cleared_at_millis IS NULL",
        params![
            scan_id,
            region.start as i64,
            region.end as i64,
            done_to as i64
        ],
    )?;
    Ok(())
}

/// Clears a region: a rescan has read all of it.
pub(crate) fn clear(connection: &Connection, scan_id: &str, region: &Span) -> Result<()> {
    connection.execute(
        "UPDATE scan_stale_regions SET done_to = end, cleared_at_millis = ?4
         WHERE scan_id = ?1 AND start = ?2 AND end = ?3 AND cleared_at_millis IS NULL",
        params![
            scan_id,
            region.start as i64,
            region.end as i64,
            now_millis() as i64
        ],
    )?;
    Ok(())
}

/// Keeps the markers a source had, and gives it the ones of the drive as it is now. Every saved
/// raw scan of the source is given them too, so each one resumes as an exact match. The stale
/// regions of the scans stay as they are: they say what is not yet read again, whatever the
/// markers say. Does nothing when the source has no stored markers to replace.
pub(crate) fn rerecord_markers(
    connection: &Connection,
    source_id: &str,
    current: &Fingerprint,
    reason: &str,
) -> Result<bool> {
    let transaction =
        rusqlite::Transaction::new_unchecked(connection, rusqlite::TransactionBehavior::Immediate)?;
    let earlier: Option<Option<String>> = transaction
        .query_row(
            "SELECT fingerprint FROM sources WHERE id = ?1",
            [source_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(earlier) = earlier else {
        return Ok(false);
    };
    // A drive that was read carefully gave few markers: they do not replace the fuller set the source has.
    if let Some(saved) = earlier
        .as_deref()
        .and_then(|text| serde_json::from_str::<Fingerprint>(text).ok())
    {
        if current.samples.len() < saved.samples.len() {
            return Ok(false);
        }
    }
    let now = now_millis() as i64;
    if let Some(earlier) = earlier {
        transaction.execute(
            "INSERT INTO source_marker_history (source_id, fingerprint, replaced_at_millis, reason)
             VALUES (?1, ?2, ?3, ?4)",
            params![source_id, earlier, now, reason],
        )?;
    }
    let text = serde_json::to_string(current)?;
    transaction.execute(
        "UPDATE sources SET fingerprint = ?2, last_seen_millis = ?3 WHERE id = ?1",
        params![source_id, text, now],
    )?;
    // The saved raw scans of this source compare themselves with their own markers.
    transaction.execute(
        "UPDATE raw_scans SET fingerprint = ?2 WHERE id IN
         (SELECT raw_scan_id FROM scans WHERE source_id = ?1 AND raw_scan_id IS NOT NULL)",
        params![source_id, text],
    )?;
    transaction.execute(
        "DELETE FROM raw_scan_changes WHERE scan_id IN
         (SELECT raw_scan_id FROM scans WHERE source_id = ?1 AND raw_scan_id IS NOT NULL)",
        [source_id],
    )?;
    transaction.commit()?;
    Ok(true)
}

/// The markers a source had before each rescan that replaced them, newest first.
pub(crate) fn marker_history(
    connection: &Connection,
    source_id: &str,
) -> Result<Vec<MarkerHistory>> {
    let mut statement = connection.prepare(
        "SELECT replaced_at_millis, reason, fingerprint FROM source_marker_history
         WHERE source_id = ?1 ORDER BY id DESC",
    )?;
    let rows = statement.query_map([source_id], |row| {
        let text: String = row.get(2)?;
        Ok(MarkerHistory {
            replaced_at_millis: row.get::<_, i64>(0)? as u64,
            reason: row.get(1)?,
            markers: serde_json::from_str::<Fingerprint>(&text)
                .map(|fingerprint| fingerprint.samples.len())
                .unwrap_or(0),
            fingerprint: serde_json::from_str(&text).ok(),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// A set of markers a source had before a rescan replaced it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkerHistory {
    pub replaced_at_millis: u64,
    pub reason: String,
    pub markers: usize,
    pub fingerprint: Option<Fingerprint>,
}

impl Catalog {
    /// Marks the area around the changed markers stale, for every scan of the source that has
    /// stored results there. A scan covers part of its range at most as far as it has read, and
    /// only that part is marked: the rest has no results yet, and will be read as it is now.
    /// Returns how many scans were marked.
    ///
    /// Run when a source is reopened on a probable match. Marking again with the same markers
    /// changes nothing.
    pub fn mark_changed(
        &self,
        source_id: &str,
        changed_offsets: &[u64],
        options: &StaleOptions,
    ) -> Result<usize> {
        if changed_offsets.is_empty() {
            return Ok(0);
        }
        let source = self.source(source_id)?;
        let spans = derive_regions(changed_offsets, source.length, options);
        let mut marked = 0;
        for scan in self.scans(source_id)? {
            let Some(covered_end) = scan.covered_end else {
                continue;
            };
            let covered = clip(&spans, scan.range_start, covered_end.min(scan.range_end));
            if covered.is_empty() {
                continue;
            }
            mark_stale(&self.connection, &scan.id, &covered, changed_offsets)?;
            marked += 1;
        }
        Ok(marked)
    }

    /// The regions of a scan that may be out of date, and the ones rescanned since.
    pub fn stale_summary(&self, scan_id: &str) -> Result<StaleSummary> {
        summary(&self.connection, scan_id)
    }

    /// The regions of a scan that are still stale.
    pub fn stale_regions(&self, scan_id: &str) -> Result<Vec<StaleRegion>> {
        stale_regions(&self.connection, scan_id)
    }

    /// Notes how far a rescan has read a region of a scan.
    pub fn advance_stale(&self, scan_id: &str, region: &Span, done_to: u64) -> Result<()> {
        advance(&self.connection, scan_id, region, done_to)
    }

    /// Clears a region of a scan: a rescan has read all of it.
    pub fn clear_stale(&self, scan_id: &str, region: &Span) -> Result<()> {
        clear(&self.connection, scan_id, region)
    }

    /// Replaces the stored markers of a source with the ones of the device as it is now, keeping
    /// the earlier markers in its history. Only a rescan that finished does this.
    pub fn rerecord_markers(
        &self,
        source_id: &str,
        current: &Fingerprint,
        reason: &str,
    ) -> Result<bool> {
        rerecord_markers(&self.connection, source_id, current, reason)
    }

    /// The markers a source had before each rescan replaced them.
    pub fn marker_history(&self, source_id: &str) -> Result<Vec<MarkerHistory>> {
        marker_history(&self.connection, source_id)
    }
}
