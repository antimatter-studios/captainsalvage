//! The single door: `open_source`.
//!
//! Every device, partition and disk image opens here, into one workspace that starts from the raw
//! source with an interpretation applied. The interpretation is the one the last scan of a
//! matching source used, otherwise the best detection, otherwise none (raw bytes). Opening
//! starts no scan: what is stored is loaded, and a scan runs when the person starts it.
//!
//! Scope decides which tools the window shows, not how the source opens:
//! - a **partition** (a source that starts with a volume, or one extent of a disk) is narrowed to
//!   one extent, and the filesystem is the first thing shown;
//! - a **whole disk** shows the layer between bytes and filesystems, and each of its extents
//!   opens as a source of its own, linked to the disk by `(parent source, extent)`.

use super::{bool_of, need_u64, str_of, u64_of, Engine, AUTHOR_USER};
use crate::catalog::{Accept, Catalog, ScanKind, ScanRecord, SourceMatch};
use crate::disk::RawDisk;
use crate::opening::{self, family, Candidate};
use crate::raw_scan::Verification;
use crate::session::{Reading, Session};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::sync::Arc;

/// What to open the source as.
enum Plan {
    /// A filesystem scan that is stored. `on_whole` is a scan made on the whole disk, before the
    /// extent was a source of its own.
    Stored { scan: ScanRecord, on_whole: bool },
    /// Read a candidate as a filesystem, finding or making its scan.
    Read(Candidate),
    /// Raw bytes. A stored raw scan is loaded when there is one.
    Raw(Option<ScanRecord>),
}

fn open_accept(args: &Value) -> Accept {
    if bool_of(args, "resumeAnyway", false) {
        Accept::Anyway
    } else if bool_of(args, "acceptChangedMarkers", false) {
        Accept::Probable
    } else {
        Accept::Exact
    }
}

/// The boot evidence of a device as the key of a workspace made before the catalog, if it opens.
fn legacy_key(view: &RawDisk) -> Option<String> {
    let region = crate::disk::DiskRegion {
        start: 0,
        length: view.length,
        format: String::new(),
        label: String::new(),
        boot_offset: 0,
    };
    let mut boot = [0u8; 512];
    view.source.read_exact_at(0, &mut boot).ok()?;
    let kind = crate::filesystem::FilesystemKind::detect_boot_sector(&boot).ok()?;
    let source = view
        .interpretation(&crate::disk::DiskRegion {
            format: kind.to_string(),
            ..region
        })
        .ok()?;
    Some(crate::volume::Volume::open(source).ok()?.workspace_name())
}

/// Tells the health guard of a disk which stored source it is, and the markers the drive is known
/// by, so the same drive is found again after it went away and came back.
fn remember_identity(
    catalog: &Catalog,
    disk: &RawDisk,
    whole: Option<&str>,
    parent: Option<&str>,
    taken: &crate::raw_scan::Fingerprint,
) {
    let Some(view) = disk.health.as_ref() else {
        return;
    };
    match parent {
        None => {
            view.health.set_identity(taken.clone());
            if let Some(id) = whole {
                view.health.adopt_source(id);
            }
        }
        Some(id) => {
            if let Ok(Some(stored)) = catalog.fingerprint_of(id) {
                view.health.set_identity(stored);
            }
            view.health.adopt_source(id);
        }
    }
}

fn matches_json(catalog: &Catalog, matches: &[SourceMatch]) -> Result<Vec<Value>> {
    matches
        .iter()
        .map(|found| {
            let mut entry = serde_json::to_value(found)?;
            entry["scans"] = serde_json::to_value(catalog.scans(&found.source.id)?)?;
            Ok(entry)
        })
        .collect()
}

impl Engine {
    /// The raw source to open: the open one when `diskId` names it, otherwise `path` opened now.
    fn source_disk(&self, args: &Value) -> Result<(Arc<RawDisk>, bool)> {
        if args.get("diskId").is_some() {
            return Ok((self.raw_disk(args)?, false));
        }
        let path = str_of(args, "path")
            .context("open_source needs a path, or the diskId of an open source")?;
        // The drive that went away and came back is the same source: its guard is pointed at the
        // new node, found by its markers, and the node that vanished is let go.
        let rebound = self
            .raw_disk
            .read()
            .ok()
            .and_then(|slot| slot.clone())
            .filter(|current| current.health.is_some() && current.source.is_gone())
            .and_then(|current| current.rebound(std::path::Path::new(path)).ok());
        if let Some(rebound) = rebound {
            let disk = Arc::new(rebound);
            *self
                .raw_disk
                .write()
                .map_err(|_| anyhow::anyhow!("raw disk lock poisoned"))? = Some(disk.clone());
            return Ok((disk, false));
        }
        let disk = Arc::new(self.open_device(std::path::Path::new(path))?);
        *self
            .raw_disk
            .write()
            .map_err(|_| anyhow::anyhow!("raw disk lock poisoned"))? = Some(disk.clone());
        *self
            .source_of_disk
            .write()
            .map_err(|_| anyhow::anyhow!("source lock poisoned"))? = None;
        Ok((disk, true))
    }

    fn clear_session(&self) {
        if let Ok(mut slot) = self.session.write() {
            *slot = None;
        }
    }

    /// The stored source of the whole disk that is open, found or made once and remembered, so an
    /// extent opened from it is linked to the same one however often it is opened.
    fn disk_source(&self, catalog: &Catalog, disk: &RawDisk, accept: Accept) -> Result<String> {
        if let Some((id, source)) = self
            .source_of_disk
            .read()
            .map_err(|_| anyhow::anyhow!("source lock poisoned"))?
            .clone()
        {
            if id == disk.id {
                return Ok(source);
            }
        }
        let resolved = catalog.resolve_source(disk, accept, None)?.source.id;
        *self
            .source_of_disk
            .write()
            .map_err(|_| anyhow::anyhow!("source lock poisoned"))? =
            Some((disk.id.clone(), resolved.clone()));
        Ok(resolved)
    }

    /// Opens a source: a device, a partition, a disk image, one extent of a disk, or a stored
    /// scan. See the module documentation. Returns what the window needs to draw the workspace
    /// and its interpretation picker, or `needsChoice` when only a probable match for a stored
    /// source was found, which is never applied without the person's say.
    pub(super) fn open_source(&self, args: &Value, author: &str) -> Result<Value> {
        let accept = open_accept(args);
        let show_all = bool_of(args, "showAll", false);
        let fresh = bool_of(args, "fresh", false);
        let rescan = bool_of(args, "rescan", false);
        let wanted_scan = str_of(args, "scanId");
        let wanted_format = str_of(args, "format");
        if accept == Accept::Anyway && author != AUTHOR_USER {
            bail!("Only the person using the app resumes a scan on a source that does not match it. Suggest it to them instead.");
        }
        if (wanted_format.is_some() || rescan) && author != AUTHOR_USER {
            bail!("Only the person using the app switches an interpretation or starts a scan again. Suggest it to them instead.");
        }

        let (disk, _) = self.source_disk(args)?;
        // Opening reads the drive in places nothing has read before. Each of those reads is
        // journaled before it starts, so one that resets the device is known the next time.
        let _journal = disk.health.as_ref().map(|view| view.health.journal_scope());
        let extent = match args.get("extent").filter(|value| !value.is_null()) {
            Some(value) => {
                let start = need_u64(value, "start")?;
                let length = need_u64(value, "length")?;
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
        let view: &RawDisk = match extent {
            Some((start, length)) => {
                owned = disk.extent(start, length)?;
                &owned
            }
            None => &disk,
        };
        let catalog = Catalog::for_user()?;
        let mut identification = catalog.identify(view, show_all)?;
        // A stored drive that matches too little to be applied is still said, never passed over:
        // opening goes on as a new source only when the person says so.
        let unmatched = wanted_scan.is_none()
            && !fresh
            && accept == Accept::Exact
            && (identification.hidden > 0 || !identification.matches.is_empty())
            && !identification
                .matches
                .iter()
                .any(|found| found.verification != Verification::Weak);
        if unmatched && identification.hidden > 0 {
            identification = catalog.identify(view, true)?;
        }
        // Nothing matched, and nothing names this drive: the saved scans of its size are offered
        // too, because a drive that cannot be recognised is the one that most needs asking about.
        let same_size = wanted_scan.is_none()
            && !fresh
            && accept == Accept::Exact
            && identification.matches.is_empty()
            && identification.hidden == 0;
        let same_size = if same_size {
            catalog.unmatched_of_size(view)?
        } else {
            Vec::new()
        };
        let unmatched = unmatched || !same_size.is_empty();
        if !same_size.is_empty() {
            identification.matches = same_size;
        }
        let exact = identification
            .matches
            .iter()
            .find(|found| found.verification == Verification::Exact);
        let probable = identification
            .matches
            .iter()
            .find(|found| found.verification == Verification::Probable);
        if wanted_scan.is_none()
            && !fresh
            && accept == Accept::Exact
            && exact.is_none()
            && (probable.is_some() || unmatched)
        {
            return Ok(json!({
                "needsChoice": true,
                "disk": disk.overview(),
                "extent": extent.map(|(start, length)| json!({"start": start, "length": length})),
                "fingerprint": identification.fingerprint,
                "matches": matches_json(&catalog, &identification.matches)?,
                "hidden": identification.hidden,
            }));
        }

        let detection = opening::detect(view)?;
        let scope = if extent.is_some()
            || detection.volume_at_start
            || wanted_format.is_some_and(|format| format != "none")
        {
            "partition"
        } else {
            "disk"
        };
        let parent = match extent {
            Some(_) => Some(self.disk_source(&catalog, &disk, accept)?),
            None => None,
        };
        let wanted = wanted_scan.map(|id| catalog.scan(id)).transpose()?;
        let matched = wanted
            .as_ref()
            .map(|scan| scan.source_id.clone())
            .or_else(|| {
                exact
                    .or(if accept.allows_probable() {
                        probable
                    } else {
                        None
                    })
                    .map(|found| found.source.id.clone())
            });
        // A disk that has reset is recognised from the few markers read in places known to read. What is
        // remembered to find the drive again is the fuller set stored for it, not those few.
        let careful = disk.careful_open();
        let remembered = if careful {
            matched
                .as_deref()
                .and_then(|id| catalog.fingerprint_of(id).ok().flatten())
                .unwrap_or_else(|| identification.fingerprint.clone())
        } else {
            identification.fingerprint.clone()
        };
        remember_identity(
            &catalog,
            &disk,
            if extent.is_none() {
                matched.as_deref()
            } else {
                None
            },
            parent.as_deref(),
            &remembered,
        );
        let stored = match &matched {
            Some(id) => catalog.scans(id)?,
            None => Vec::new(),
        };
        let on_whole: Vec<ScanRecord> = match (&parent, extent) {
            (Some(parent), Some((start, length)))
                if stored.iter().all(|scan| scan.kind != ScanKind::Filesystem) =>
            {
                catalog.whole_source_scans_of(parent, start, length)?
            }
            _ => Vec::new(),
        };
        let saved = match &matched {
            Some(id) => catalog.interpretations(id)?,
            None => Vec::new(),
        };
        let inside = self.hypotheses_inside(&disk, extent);
        let to_candidate = |scan: &ScanRecord| Candidate {
            format: scan.format.clone(),
            start: scan.start,
            length: scan.length,
            boot_offset: scan.boot_offset,
            confidence: scan.confidence,
            origin: scan.origin.clone(),
        };
        let reading_of = |id: &str| -> Result<Candidate> {
            opening::candidate_for(id, &detection, &saved, &inside).with_context(|| {
                format!("No {id} boot evidence was found at the start, and none is saved for it. Select a range on the map and use Analyze this range to look for one.")
            })
        };

        // What to open it as.
        let mut plan = if let Some(scan) = wanted {
            if scan.kind == ScanKind::Filesystem {
                let whole = parent.as_deref() == Some(scan.source_id.as_str());
                Plan::Stored {
                    scan,
                    on_whole: whole,
                }
            } else {
                Plan::Raw(Some(scan))
            }
        } else if let Some(format) = wanted_format {
            let id = family(format);
            if id == "none" {
                Plan::Raw(None)
            } else if let Some(boot) = u64_of(args, "bootOffset") {
                // The person says where the boot evidence is (an NTFS copy in the last sector, or
                // an exFAT backup), which the start of the extent does not show.
                let length = extent.map_or(disk.length, |(_, length)| length);
                ensure!(boot < length, "the boot evidence is outside the extent");
                Plan::Read(Candidate {
                    format: format.to_string(),
                    start: 0,
                    length,
                    boot_offset: (boot != 0).then_some(boot),
                    confidence: 0.0,
                    origin: "manual".into(),
                })
            } else {
                Plan::Read(reading_of(id)?)
            }
        } else if scope == "disk" {
            Plan::Raw(
                stored
                    .iter()
                    .find(|scan| scan.kind == ScanKind::Raw && scan.raw_scan_id.is_some())
                    .cloned(),
            )
        } else if let Some(last) = stored.first() {
            if last.kind == ScanKind::Filesystem {
                Plan::Stored {
                    scan: last.clone(),
                    on_whole: false,
                }
            } else {
                Plan::Raw(Some(last.clone()))
            }
        } else if let Some(old) = on_whole.first() {
            Plan::Stored {
                scan: old.clone(),
                on_whole: true,
            }
        } else if let Some(best) = detection.candidates.first() {
            Plan::Read(best.clone())
        } else {
            Plan::Raw(None)
        };
        if rescan {
            if let Plan::Stored { scan, .. } = &plan {
                plan = Plan::Read(to_candidate(scan));
            }
        }

        let mut raw_snapshot = Value::Null;
        let mut source_id = matched.clone();
        let mut open_error: Option<String> = None;
        let mut opened_scan: Option<ScanRecord> = None;
        let attempt = match &plan {
            Plan::Stored { scan, on_whole } => Some(if *on_whole {
                Session::open_scan_in(&disk, None, &scan.id, accept)
            } else {
                Session::open_scan_in(&disk, extent, &scan.id, accept)
            }),
            Plan::Read(candidate) => {
                let reading = Reading {
                    format: candidate.format.clone(),
                    start: candidate.start,
                    length: candidate.length,
                    boot_offset: candidate.boot_offset,
                    origin: if candidate.origin == "manual" {
                        "manual"
                    } else {
                        "analysis"
                    }
                    .into(),
                    confidence: candidate.confidence,
                    label: format!("{} interpretation", candidate.format),
                };
                Some(Session::open_reading(
                    &disk,
                    extent,
                    &reading,
                    accept,
                    rescan,
                    parent.as_deref(),
                    fresh,
                ))
            }
            Plan::Raw(_) => None,
        };
        let mut session: Option<Arc<Session>> = None;
        match attempt {
            Some(Ok(opened)) => session = Some(opened),
            // Opening is never refused for what a detection or a stored scan cannot read: the
            // bytes open as raw bytes, and the window says why. A reading the person asked for by
            // name is refused with the reason.
            Some(Err(error)) if wanted_scan.is_none() && wanted_format.is_none() => {
                open_error = Some(format!("{error:#}"));
                plan = Plan::Raw(None);
            }
            Some(Err(error)) => return Err(error),
            None => {}
        }
        if let Plan::Raw(scan) = &plan {
            // A raw reading is a source like any other. The source is made only when there is
            // something to attach to it, so a volume with work from before the catalog is still
            // found by its old key when it is opened as a filesystem.
            if source_id.is_none() && extent.is_none() {
                let key = legacy_key(view);
                source_id = Some(
                    catalog
                        .resolve_source(view, accept, key.as_deref())?
                        .source
                        .id,
                );
            }
            if scope == "disk" {
                if let Some(id) = source_id.as_deref() {
                    *self
                        .source_of_disk
                        .write()
                        .map_err(|_| anyhow::anyhow!("source lock poisoned"))? =
                        Some((disk.id.clone(), id.to_string()));
                }
            }
            if let Some(scan) = scan {
                catalog.touch_scan(&scan.id)?;
                if let Some(raw) = scan.raw_scan_id.as_deref() {
                    let mut store = crate::raw_scan::ScanStore::for_user()?;
                    raw_snapshot = serde_json::to_value(if accept == Accept::Anyway {
                        store.resume_anyway(&disk, raw)?
                    } else {
                        store.resume(&disk, raw, true, accept.allows_probable())?
                    })?;
                }
                // After the scan resumed: on a probable match that marks what may be out of date.
                opened_scan = Some(catalog.scan(&scan.id)?);
            } else if let (Some(id), None) = (source_id.as_deref(), extent) {
                if wanted_format.is_some() {
                    let placeholder = catalog.raw_placeholder(id, 0, disk.length)?;
                    catalog.touch_scan(&placeholder.id)?;
                }
            }
        }

        let mut overview = Value::Null;
        let mut created = false;
        match &session {
            Some(session) => {
                overview = session.overview();
                overview["filesystem"] = serde_json::to_value(&session.volume.filesystem)?;
                if let Some(binding) = &session.scan {
                    // Whichever way the scan was found, the extent is the one the disk holds.
                    if let (Some(parent), Some((start, length))) = (parent.as_deref(), extent) {
                        if binding.source_id != parent {
                            catalog.link_extent(parent, &binding.source_id, start, length)?;
                        }
                    }
                    source_id = Some(binding.source_id.clone());
                    created = binding.created;
                    opened_scan = Some(catalog.scan(&binding.scan_id)?);
                }
                self.set_session(session.clone());
                self.emit(
                    "volume-opened",
                    json!({"path": disk.path, "interpretation": extent.is_some()}),
                );
            }
            None => self.clear_session(),
        }

        let stale = match &opened_scan {
            Some(scan) => serde_json::to_value(catalog.stale_summary(&scan.id)?)?,
            None => Value::Null,
        };
        let source = match source_id.as_deref() {
            Some(id) => Some(catalog.source(id)?),
            None => None,
        };
        let scans = match source_id.as_deref() {
            Some(id) => catalog.scans(id)?,
            None => Vec::new(),
        };
        let current = opened_scan
            .as_ref()
            .filter(|scan| scan.kind == ScanKind::Filesystem)
            .map(&to_candidate);
        let picker_saved = match source_id.as_deref() {
            Some(id) => catalog.interpretations(id)?,
            None => Vec::new(),
        };
        let choices = if scope == "partition" {
            opening::choices(&detection, &picker_saved, &inside, &scans, current.as_ref())
        } else {
            Vec::new()
        };
        if extent.is_none() {
            if let Some(id) = source_id.as_deref() {
                *self
                    .source_of_disk
                    .write()
                    .map_err(|_| anyhow::anyhow!("source lock poisoned"))? =
                    Some((disk.id.clone(), id.to_string()));
            }
        }
        let disk_scope_source = if scope == "disk" {
            source_id.clone()
        } else {
            parent.clone()
        };
        let extents = match disk_scope_source.as_deref() {
            Some(id) => serde_json::to_value(catalog.extents_of(id)?)?,
            None => json!([]),
        };
        let parents = match (&source_id, extent) {
            (Some(id), _) => serde_json::to_value(catalog.parents_of(id)?)?,
            _ => json!([]),
        };
        let others: Vec<SourceMatch> = identification
            .matches
            .iter()
            .filter(|found| Some(&found.source.id) != source_id.as_ref())
            .cloned()
            .collect();
        Ok(json!({
            "disk": disk.overview(),
            "scope": scope,
            "extent": extent.map(|(start, length)| json!({"start": start, "length": length})),
            "source": source,
            "parents": parents,
            "extents": extents,
            // Said when the disk has reset: what opening did not do, so a person is not surprised.
            "carefulOpen": careful.then(|| json!({
                "resets": disk.health.as_ref().map_or(0, |view| view.health.resets()),
                "markersRead": identification.fingerprint.samples.len(),
                "markersStored": remembered.samples.len(),
                "analysisSkipped": true,
                "summary": "This drive has reset, so opening it did not read fresh markers across the disk and did not analyse it: it was recognised from the few stored markers that lie in places known to read. Analysing a range is yours to ask for.",
            })),
            // The ranges of the source that must not be read, as extents of kind bad_sector.
            "badSectors": crate::health_tools::bad_extents_in(&disk, 0, u64::MAX),
            "interpretation": opened_scan.as_ref().filter(|scan| scan.kind == ScanKind::Filesystem).map(|scan| json!({
                "format": scan.format, "start": scan.start, "length": scan.length,
                "bootOffset": scan.boot_offset, "origin": scan.origin, "confidence": scan.confidence,
            })),
            "detected": detection.candidates.first(),
            "choices": choices,
            "scan": opened_scan,
            // The areas of this scan's results that may be out of date, because the source was a
            // probable match. Nothing is scanned again until the person asks.
            "stale": stale,
            "created": created,
            "scans": scans,
            "matches": matches_json(&catalog, &others)?,
            "hidden": identification.hidden,
            "fingerprint": identification.fingerprint,
            "raw": raw_snapshot,
            "openError": open_error,
            "overview": overview,
        }))
    }

    /// The saved hypotheses of the open disk that lie inside `extent` (all of the disk when there
    /// is none), as candidates relative to it.
    fn hypotheses_inside(&self, disk: &RawDisk, extent: Option<(u64, u64)>) -> Vec<Candidate> {
        let (start, length) = extent.unwrap_or((0, disk.length));
        let Ok(store) = crate::hypotheses::HypothesisStore::for_user() else {
            return Vec::new();
        };
        let Ok(list) = store.list(disk) else {
            return Vec::new();
        };
        let mut inside: Vec<Candidate> = list
            .into_iter()
            .filter(|found| found.start >= start && found.end() <= start + length)
            .map(|found| Candidate {
                format: found.format,
                start: found.start - start,
                length: found.length,
                boot_offset: found.boot_offset.map(|offset| offset - start),
                confidence: found.confidence,
                origin: found.origin,
            })
            .collect();
        inside.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        inside
    }
}
