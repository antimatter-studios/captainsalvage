//! Scanning the stale regions of a filesystem scan again.
//!
//! The scan read every cluster once. When the source later turned out to have changed near some
//! of its markers (a probable match), the clusters in those regions are read again, with the same
//! reading as the scan, and what the scan stored for them is replaced a chunk at a time, in one
//! transaction per chunk. The entries that are still there keep their ids, so what a person
//! decided about them (corrections, Disk Rebuilds) stays attached. Then the tree is rebuilt once,
//! from the volume as it is, and the regions are cleared.
//!
//! A region the rescan has not finished stays stale and keeps how far it got, so closing the app,
//! a failed read or a drive that goes away leaves things stale and resumable. Only clusters the
//! scan had already read are rescanned: the rest have no results to be out of date.
//!
//! The source is only read.

use super::{
    is_database_busy, lock_scan, open_map_file, read_chunk, ScanOptions, BUSY_RETRY_DELAY,
    BUSY_RETRY_LIMIT, LOOKAHEAD_BYTES, STATE_DIRECTORIES, STATE_NEXT,
};
use crate::catalog::{Catalog, StaleRegion};
use crate::content::CLASS_DIRECTORY;
use crate::map::MapDb;
use crate::session::DISK_GONE;
use crate::stale::Span;
use crate::volume::Volume;
use crate::workspace::Workspace;
use anyhow::{Context, Result};
use fs_exfat::primitives::FIRST_CLUSTER;
use serde::Serialize;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, Ordering};

/// What a rescan of a filesystem scan did.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemRescan {
    /// Regions read to their end and cleared.
    pub regions_rescanned: usize,
    /// Regions still stale.
    pub regions_remaining: usize,
    /// Clusters read again.
    pub clusters_rescanned: u64,
    pub bytes_rescanned: u64,
    /// The tree was rebuilt from what the rescan stored.
    pub resolved: bool,
    /// Why the rescan stopped before it was done, if it did.
    pub stopped: Option<String>,
}

/// The clusters of the volume that share a byte with `start..end` (offsets in the volume), as
/// `first..end` cluster numbers. None when the range lies in the area before the first cluster,
/// where the boot sectors and the allocation tables are.
fn clusters_of(volume: &Volume, start: u64, end: u64) -> Option<(u32, u32)> {
    let heap = volume.cluster_offset(FIRST_CLUSTER);
    let size = volume.geometry.cluster_size();
    if end <= heap || start >= end {
        return None;
    }
    let first = FIRST_CLUSTER + (start.saturating_sub(heap) / size) as u32;
    let last = (FIRST_CLUSTER as u64 + (end - heap).div_ceil(size))
        .min(u64::from(volume.geometry.last_cluster_exclusive())) as u32;
    (first < last).then_some((first, last))
}

/// Reads the stale regions of a scan again and rebuilds the tree.
///
/// `scan_start` is where the scan's interpretation starts in its source: stale regions are in
/// the source's offsets, and the volume's own offsets start there. `on_step` is told of each
/// chunk (the region, how far it has been read) and says whether to go on.
#[allow(clippy::too_many_arguments)]
pub fn rescan_changed(
    volume: &Volume,
    workspace: &Workspace,
    catalog: &Catalog,
    scan_id: &str,
    scan_start: u64,
    regions: &[StaleRegion],
    options: ScanOptions,
    stop: &AtomicBool,
    mut on_step: impl FnMut(&Span, u64) -> bool,
) -> Result<FilesystemRescan> {
    let mut report = FilesystemRescan::default();
    if regions.is_empty() {
        return Ok(report);
    }
    let _lock = lock_scan(workspace)?;
    let mut database = MapDb::open(&workspace.database())?;
    let cluster_count = u64::from(volume.geometry.cluster_count);
    let classes = open_map_file(&workspace.classes(), cluster_count)?;
    let entropy = open_map_file(&workspace.entropy(), cluster_count)?;
    let text_heads = open_map_file(&workspace.text_heads(), cluster_count)?;
    let size = volume.geometry.cluster_size();
    let lookahead_max = LOOKAHEAD_BYTES.max(volume.geometry.record_size);
    let last = volume.geometry.last_cluster_exclusive();
    // Only what the scan has read has results that can be out of date.
    let scanned_to = (database
        .state_u64(STATE_NEXT)?
        .max(u64::from(FIRST_CLUSTER)) as u32)
        .min(last);
    let mut buffer = Vec::new();

    'regions: for region in regions {
        let span = region.span();
        let from = region.done_to.max(span.start);
        let clusters = clusters_of(
            volume,
            from.saturating_sub(scan_start),
            span.end.saturating_sub(scan_start),
        )
        .map(|(first, end)| (first, end.min(scanned_to)))
        .filter(|(first, end)| first < end);
        let Some((first, end)) = clusters else {
            // Nothing in the cluster area to read: the region is the tables before it, which
            // the rebuild below reads from the volume.
            catalog.advance_stale(scan_id, &span, span.end)?;
            continue;
        };
        let mut next = first;
        while next < end {
            if stop.load(Ordering::Relaxed) {
                report.stopped = Some("The rescan was stopped.".into());
                break 'regions;
            }
            let count = options.chunk_clusters.min(end - next);
            let Some(chunk) = read_chunk(volume, &mut buffer, next, count, lookahead_max, last)
            else {
                report.stopped = Some(DISK_GONE.into());
                break 'regions;
            };
            let position = u64::from(next - FIRST_CLUSTER);
            classes.write_all_at(&chunk.classes, position)?;
            entropy.write_all_at(&chunk.entropy, position)?;
            text_heads.write_all_at(&chunk.text_heads, position)?;
            let mut attempts = 0;
            loop {
                match database.replace_chunk(next, next + count, &chunk.found, &chunk.errors) {
                    Ok(()) => break,
                    Err(error) if is_database_busy(&error) && attempts < BUSY_RETRY_LIMIT => {
                        attempts += 1;
                        std::thread::sleep(BUSY_RETRY_DELAY);
                    }
                    Err(error) => return Err(error),
                }
            }
            next += count;
            report.clusters_rescanned += u64::from(count);
            report.bytes_rescanned += u64::from(count) * size;
            let read_to = (scan_start + volume.cluster_offset(next)).min(span.end);
            catalog.advance_stale(scan_id, &span, if next >= end { span.end } else { read_to })?;
            if !on_step(&span, read_to) {
                report.stopped = Some("The rescan was stopped.".into());
                break 'regions;
            }
        }
    }

    // Every region read to its end: the map holds the new entries, and the tree and the totals
    // that depend on them are made again from the volume as it is.
    let finished = catalog
        .stale_regions(scan_id)?
        .iter()
        .all(|region| region.done_to >= region.end);
    if report.stopped.is_none() && finished {
        let map = workspace.read_cluster_map(&workspace.classes(), volume.geometry.cluster_count);
        let directories = map
            .iter()
            .filter(|class| **class == CLASS_DIRECTORY)
            .count();
        database.set_state(STATE_DIRECTORIES, &directories.to_string())?;
        database
            .resolve(volume, workspace)
            .context("rebuilding the tree after the rescan")?;
        report.resolved = true;
        for region in catalog.stale_regions(scan_id)? {
            catalog.clear_stale(scan_id, &region.span())?;
            report.regions_rescanned += 1;
        }
    }
    report.regions_remaining = catalog.stale_regions(scan_id)?.len();
    Ok(report)
}
