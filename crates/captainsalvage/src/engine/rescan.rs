//! `rescan_changed_regions`: reading the stale regions of a scan again.
//!
//! A scan opened on a probable match has regions that may be out of date (see [`crate::stale`]).
//! This tool is the person's action to read them again, with the same interpretation and options
//! the scan was made with, and nothing else. It never runs by itself: opening a scan only marks
//! the regions and says how many there are.
//!
//! A raw scan replaces its findings batch by batch in the saved scan's database. A filesystem scan
//! replaces the entries of the clusters it reads again, chunk by chunk, then rebuilds its tree.
//! Either way a region that is not finished stays stale and resumes, and only when every region
//! of the scan is done does the source get the markers of the drive as it is now (the earlier
//! ones kept in its history), so that the next open of the same drive is an exact match.

use super::{need_str, Engine, AUTHOR_USER};
use crate::catalog::{Accept, Catalog, ScanKind};
use crate::raw_scan::{Fingerprint, ScanStore};
use crate::scan::ScanOptions;
use crate::session::Session;
use crate::stale::StaleOptions;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;

impl Engine {
    /// Reads the stale regions of a scan again. Only the person using the app does this.
    pub(super) fn rescan_changed_regions(&self, args: &Value, author: &str) -> Result<Value> {
        if author != AUTHOR_USER {
            bail!("Only the person using the app scans the changed regions again. Suggest it to them instead.");
        }
        let disk = self.raw_disk(args)?;
        let scan_id = need_str(args, "scanId")?;
        let extent = match args.get("extent").filter(|value| !value.is_null()) {
            Some(value) => {
                let start = value
                    .get("start")
                    .and_then(Value::as_u64)
                    .context("extent needs start")?;
                let length = value
                    .get("length")
                    .and_then(Value::as_u64)
                    .context("extent needs length")?;
                ensure!(
                    length > 0
                        && start
                            .checked_add(length)
                            .is_some_and(|end| end <= disk.length),
                    "the extent is outside the source"
                );
                (!(start == 0 && length == disk.length)).then_some((start, length))
            }
            None => None,
        };
        let owned;
        let view: &crate::disk::RawDisk = match extent {
            Some((start, length)) => {
                owned = disk.extent(start, length)?;
                &owned
            }
            None => &disk,
        };
        let catalog = Catalog::for_user()?;
        let scan = catalog.scan(scan_id)?;
        let widen = args.get("radiusBytes").is_some() || args.get("headBytes").is_some();
        let options = StaleOptions::from_arguments(args);

        match scan.kind {
            ScanKind::Raw => {
                let raw = scan
                    .raw_scan_id
                    .as_deref()
                    .context("this scan has not stored anything to scan again")?;
                let mut store = ScanStore::for_user()?;
                store.rescan_begin(view, raw)?;
                if widen {
                    store.widen_stale(raw, &options)?;
                }
                let report = store.rescan_changed(view, raw, |_| true)?;
                let snapshot = store.current(view, raw)?;
                Ok(json!({
                    "scan": catalog.scan(scan_id)?,
                    "rescan": report,
                    "markersRerecorded": report.markers_rerecorded,
                    "stale": catalog.stale_summary(scan_id)?,
                    "raw": snapshot,
                }))
            }
            ScanKind::Filesystem => {
                let open = self.session().ok().filter(|session| {
                    session
                        .scan
                        .as_ref()
                        .is_some_and(|binding| binding.scan_id == scan_id)
                });
                let session = match open {
                    Some(session) => session,
                    None => Session::open_scan_in(&disk, extent, scan_id, Accept::Probable)?,
                };
                if widen {
                    let markers: Vec<u64> = catalog
                        .stale_regions(scan_id)?
                        .into_iter()
                        .flat_map(|region| region.markers)
                        .collect();
                    catalog.mark_changed(&scan.source_id, &markers, &options)?;
                }
                let regions = catalog.stale_regions(scan_id)?;
                let start = session.scan.as_ref().map_or(0, |binding| binding.start);
                let report = crate::scan::rescan_changed(
                    &session.volume,
                    &session.workspace,
                    &catalog,
                    scan_id,
                    start,
                    &regions,
                    ScanOptions::for_volume(&session.volume),
                    &AtomicBool::new(false),
                    |_, _| true,
                )?;
                let mut rerecorded = false;
                if report.regions_remaining == 0 && report.stopped.is_none() && report.resolved {
                    let current = Fingerprint::read(view);
                    if !view.source.is_gone() && !current.samples.is_empty() {
                        rerecorded = catalog.rerecord_markers(
                            &scan.source_id,
                            &current,
                            "rescan of changed regions",
                        )?;
                    }
                }
                session.refresh_stale()?;
                self.emit("map-changed", json!({"reason": "rescan"}));
                Ok(json!({
                    "scan": catalog.scan(scan_id)?,
                    "rescan": report,
                    "markersRerecorded": rerecorded,
                    "stale": catalog.stale_summary(scan_id)?,
                }))
            }
        }
    }
}
