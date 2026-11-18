//! The link between a source and an extent of it that is a source in its own right.
//!
//! A partition opened alone (`/dev/rdisk7s2`) and the same extent seen from its whole disk
//! (`/dev/rdisk7`, bytes `start..start + length`) are the same bytes, so they must be the same
//! source, with one interpretation and one set of results. The link is made by markers, never by
//! the device number: the extent is read as a source of its own (the markers are taken from the
//! extent's bytes), so the partition device finds it by the same markers from the other side.
//! `source_extents` records where the child sits in each parent it was seen in, so the whole disk
//! can list the scans of its extents, and the partition can say which disk it belongs to.

use super::{Accept, Catalog, Resolution, ScanRecord, SourceRecord};
use crate::disk::RawDisk;
use crate::workspace::now_millis;
use anyhow::{ensure, Result};
use rusqlite::params;
use serde::Serialize;

/// An extent of a source that is a source of its own, with the scans stored for it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkedExtent {
    pub start: u64,
    pub length: u64,
    pub source: SourceRecord,
    pub scans: Vec<ScanRecord>,
}

/// A whole source that holds this one as an extent.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParentLink {
    pub start: u64,
    pub length: u64,
    pub parent: SourceRecord,
}

impl Catalog {
    /// Records that `child` is the extent `start..start + length` of `parent`. Saying it again
    /// changes nothing.
    pub fn link_extent(&self, parent: &str, child: &str, start: u64, length: u64) -> Result<()> {
        ensure!(parent != child, "a source is not an extent of itself");
        ensure!(length > 0, "an extent must not be empty");
        self.connection().execute(
            "INSERT OR IGNORE INTO source_extents
                (child_source_id, parent_source_id, ext_start, ext_length, created_at_millis)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                child,
                parent,
                start as i64,
                length as i64,
                now_millis() as i64
            ],
        )?;
        Ok(())
    }

    /// The source an extent of an open source is: the stored one its own markers match, or a new
    /// one, and linked to the parent either way. Like any source it is only the stored one when
    /// the match is exact, or probable and acknowledged (`accept`); otherwise it is not applied
    /// and comes back as `declined`. Nothing is read beyond the extent's markers.
    pub fn extent_source(
        &self,
        parent_disk: &RawDisk,
        parent_id: &str,
        start: u64,
        length: u64,
        accept: Accept,
    ) -> Result<(Resolution, RawDisk)> {
        let view = parent_disk.extent(start, length)?;
        let resolution = self.resolve_source(&view, accept, None)?;
        self.link_extent(parent_id, &resolution.source.id, start, length)?;
        Ok((resolution, view))
    }

    /// The extents of `parent` known as sources, with their scans, in disk order.
    pub fn extents_of(&self, parent: &str) -> Result<Vec<LinkedExtent>> {
        let mut statement = self.connection().prepare(
            "SELECT child_source_id, ext_start, ext_length FROM source_extents
             WHERE parent_source_id = ?1 ORDER BY ext_start, ext_length",
        )?;
        let rows = statement
            .query_map([parent], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? as u64,
                    row.get::<_, i64>(2)? as u64,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(child, start, length)| {
                Ok(LinkedExtent {
                    start,
                    length,
                    source: self.source(&child)?,
                    scans: self.scans(&child)?,
                })
            })
            .collect()
    }

    /// The whole sources this one was seen inside of.
    pub fn parents_of(&self, child: &str) -> Result<Vec<ParentLink>> {
        let mut statement = self.connection().prepare(
            "SELECT parent_source_id, ext_start, ext_length FROM source_extents
             WHERE child_source_id = ?1 ORDER BY created_at_millis DESC",
        )?;
        let rows = statement
            .query_map([child], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? as u64,
                    row.get::<_, i64>(2)? as u64,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(parent, start, length)| {
                Ok(ParentLink {
                    start,
                    length,
                    parent: self.source(&parent)?,
                })
            })
            .collect()
    }

    /// Filesystem scans made on the whole source itself for this extent, before the extent was a
    /// source of its own. They are still found, and open as they did.
    pub fn whole_source_scans_of(
        &self,
        parent: &str,
        start: u64,
        length: u64,
    ) -> Result<Vec<ScanRecord>> {
        Ok(self
            .scans(parent)?
            .into_iter()
            .filter(|scan| {
                scan.kind == super::ScanKind::Filesystem
                    && scan.start == start
                    && scan.length == length
            })
            .collect())
    }
}
