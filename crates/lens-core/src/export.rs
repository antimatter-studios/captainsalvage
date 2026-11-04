//! Copies recovered files off the volume, to a destination the user chose.
//!
//! This is the only code in the crate that writes user-visible files, and it
//! writes only under the destination. It refuses any destination on the disk
//! being examined, checks free space first, and never overwrites a file. Each copy
//! is hashed while it is read and hashed again after it is written. Reading stays
//! best-effort: unreadable clusters become zeros and are recorded, so one bad spot
//! never costs the rest of a file. A manifest says exactly what was recovered.

use crate::map::Row;
use crate::session::{list_disks, whole_disk_path, Session};
use crate::verify::verify_stream;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const MANIFEST_NAME: &str = "exfat-lens-manifest.jsonl";
const MAX_RUN_BYTES: u64 = 8 * 1024 * 1024;
const HASH_BUFFER_BYTES: usize = 1024 * 1024;
/// Files larger than this are copied and hashed, but not walked by format.
const MAX_FORMAT_CHECK_BYTES: u64 = 256 * 1024 * 1024;
const MAX_NAME_BYTES: usize = 240;
const LARGEST_SHOWN: usize = 10;
const STATE_SALVAGE_DRIVE: &str = "salvage_destination";
const STATE_COPY_STATUS: &str = "copy_status";
const STATE_COPY_STOP: &str = "copy_stop_requested";
const COPY_REPORT_INTERVAL: Duration = Duration::from_secs(1);
/// A copy that has said nothing for this long is no longer running. A single
/// read on a busy disk can take seconds, so this allows plenty of room.
const COPY_STALE_MILLIS: u64 = 5 * 60 * 1000;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CopyRequest {
    pub destination: String,
    pub entry_ids: Vec<i64>,
    pub folders: Vec<u32>,
    pub min_confidence: f64,
    /// Also copy files with missing clusters, or clusters reused by current files.
    pub include_partial: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyPlan {
    pub files: u64,
    pub bytes: u64,
    /// The part of `bytes` a previous run already put on the destination.
    pub already_copied_bytes: u64,
    pub destination: String,
    pub destination_device: Option<String>,
    pub free_bytes: u64,
    pub fits: bool,
    /// Why this destination must not be used. Starting a copy is refused if set.
    pub refused: Option<String>,
    pub below_floor: u64,
    pub skipped_partial: u64,
    pub skipped_partial_bytes: u64,
    pub largest: Vec<Value>,
    pub by_verdict: HashMap<String, u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CopyStatus {
    pub state: String,
    pub destination: String,
    pub manifest: String,
    pub files_total: u64,
    pub files_done: u64,
    pub files_skipped: u64,
    pub files_partial: u64,
    pub files_failed: u64,
    pub bytes_total: u64,
    pub bytes_done: u64,
    pub current: String,
    pub error: Option<String>,
    pub bad_clusters: u64,
    pub started_at_millis: u64,
    pub updated_at_millis: u64,
}

#[derive(Default)]
pub struct CopyControl {
    status: Mutex<CopyStatus>,
    stop: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
    last_saved: Mutex<Option<Instant>>,
    last_stop_check: Mutex<Option<Instant>>,
}

/// One manifest line: what happened to one file.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestRow {
    pub id: i64,
    pub name: String,
    pub source_path: String,
    pub target: String,
    pub length: u64,
    pub bytes_written: u64,
    pub sha256: String,
    pub status: String,
    pub verdict: String,
    pub confidence: f64,
    pub reason: String,
    pub first_cluster: u32,
    pub clusters_found: u64,
    pub clusters_needed: u64,
    pub unreadable_clusters: u64,
    pub format: Option<String>,
    pub format_valid_fraction: Option<f64>,
    pub format_breaks: Option<u64>,
    pub copied_at_millis: u64,
}

/// What earlier runs recorded, by entry id. A missing manifest means nothing is done.
fn read_manifest(path: &Path) -> HashMap<i64, ManifestRow> {
    let mut done = HashMap::new();
    if let Ok(file) = File::open(path) {
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if let Ok(row) = serde_json::from_str::<ManifestRow>(&line) {
                done.insert(row.id, row);
            }
        }
    }
    done
}

fn still_on_destination(previous: &ManifestRow) -> bool {
    fs::metadata(&previous.target).is_ok_and(|metadata| metadata.len() == previous.bytes_written)
}

struct Filesystem {
    device: String,
    free_bytes: u64,
    read_only: bool,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn nearest_existing(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    while !current.exists() {
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => break,
        }
    }
    current
}

fn filesystem_of(path: &Path) -> Result<Filesystem> {
    let existing = nearest_existing(path);
    let text = CString::new(existing.as_os_str().as_bytes()).context("path holds a NUL byte")?;
    // SAFETY: `info` is plain data the call fills in, and `text` outlives the call.
    let info = unsafe {
        let mut info: libc::statfs = std::mem::zeroed();
        if libc::statfs(text.as_ptr(), &mut info) != 0 {
            bail!("cannot read filesystem details for {}", existing.display());
        }
        info
    };
    // SAFETY: statfs returns a NUL-terminated device name in a fixed buffer.
    let device = unsafe { CStr::from_ptr(info.f_mntfromname.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    Ok(Filesystem {
        device,
        free_bytes: u64::from(info.f_bavail as u32).max(info.f_bavail) * u64::from(info.f_bsize),
        read_only: info.f_flags & libc::MNT_RDONLY as u32 != 0,
    })
}

/// `disk5s1` becomes `disk5`.
fn whole_of(device: &str) -> Option<String> {
    let bare = device.strip_prefix("/dev/").unwrap_or(device);
    let number = bare
        .strip_prefix("rdisk")
        .or_else(|| bare.strip_prefix("disk"))?;
    let digits: String = number.chars().take_while(char::is_ascii_digit).collect();
    (!digits.is_empty()).then(|| format!("disk{digits}"))
}

/// Physical disks behind a device, read from `diskutil info` text. An APFS volume
/// lives on a synthesized disk, so its physical store has to be named as well.
pub fn parse_physical_disks(device: &str, info: &str) -> Vec<String> {
    let mut disks: Vec<String> = whole_of(device).into_iter().collect();
    for line in info.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if matches!(key.trim(), "Part of Whole" | "APFS Physical Store") {
            if let Some(whole) = whole_of(value.trim()) {
                if !disks.contains(&whole) {
                    disks.push(whole);
                }
            }
        }
    }
    disks
}

/// Returns why a destination must not be used, if there is a reason. Anything
/// that cannot be established counts as a reason: this check fails closed.
pub fn destination_problem(
    source_whole_disk: Option<&str>,
    device: &str,
    info: Option<&str>,
) -> Option<String> {
    let source = source_whole_disk?;
    if !device.starts_with("/dev/") {
        return None;
    }
    let Some(info) = info else {
        return Some(format!(
            "Cannot tell which physical disk {device} is on, so it is refused."
        ));
    };
    parse_physical_disks(device, info).iter().any(|disk| disk == source).then(|| {
        format!(
            "This destination is on {source}, the disk being recovered. Writing there could destroy the data you are \
             trying to save. Choose a different disk."
        )
    })
}

fn sanitize_segment(segment: &str) -> String {
    let clean: String = segment
        .chars()
        .map(|c| {
            if c == '/' || c == ':' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let mut trimmed = clean.trim().to_string();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        trimmed = format!("_{trimmed}");
    }
    while trimmed.len() > MAX_NAME_BYTES {
        trimmed.pop();
    }
    trimmed
}

/// Turns a map path into a safe relative path under the destination. Nothing a
/// damaged entry says can move a file outside it.
pub fn relative_target(path: &str, name: &str) -> PathBuf {
    let mut target = PathBuf::new();
    for (index, segment) in path.split('/').enumerate() {
        if segment.is_empty() {
            if index == 0 {
                target.push("Volume root");
            }
            continue;
        }
        match segment
            .strip_prefix("[orphan @")
            .and_then(|rest| rest.strip_suffix(']'))
        {
            Some(cluster) => target.push(format!("Lost folder {cluster}")),
            None => target.push(sanitize_segment(segment)),
        }
    }
    target.push(sanitize_segment(name));
    target
}

fn is_partial(row: &Row) -> bool {
    row.clusters_found < row.clusters_needed || row.clusters_allocated > 0
}

/// Parses an entry's stored date. exFAT dates are local, so this is approximate
/// by up to the time zone difference, and is used only as the copy's date.
fn modified_time(text: &str) -> Option<SystemTime> {
    let digits: Vec<i64> = text
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect();
    let [year, month, day, hour, minute, second] = digits[..] else {
        return None;
    };
    let shifted = if month <= 2 { year - 1 } else { year };
    let era = shifted.div_euclid(400);
    let year_of_era = shifted.rem_euclid(400);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second;
    u64::try_from(seconds)
        .ok()
        .map(|value| UNIX_EPOCH + Duration::from_secs(value))
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; HASH_BUFFER_BYTES];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Picks a path that does not exist yet, so nothing is ever overwritten.
fn unused_path(path: &Path, id: i64) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path
        .file_stem()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default();
    let renamed = match path.extension() {
        Some(extension) => format!("{stem} (entry {id}).{}", extension.to_string_lossy()),
        None => format!("{stem} (entry {id})"),
    };
    path.with_file_name(renamed)
}

impl Session {
    fn copy_rows(&self, request: &CopyRequest) -> Result<Vec<Row>> {
        self.with_database(|database| {
            let mut rows = Vec::new();
            for &id in &request.entry_ids {
                if let Some(row) = database.entry(id)? {
                    if row.kind == "file" && row.length > 0 {
                        rows.push(row);
                    }
                }
            }
            for &folder in &request.folders {
                rows.extend(database.files_under(folder)?);
            }
            rows.sort_by_key(|row| row.id);
            rows.dedup_by_key(|row| row.id);
            Ok(rows)
        })
    }

    fn source_whole_disk(&self) -> Option<String> {
        whole_disk_path(&self.path).and_then(|path| whole_of(&path.display().to_string()))
    }

    /// The folder copies go to when a request names none.
    pub fn salvage_drive(&self) -> Result<Option<String>> {
        self.with_database(|database| database.state(STATE_SALVAGE_DRIVE))
            .map(|value| value.filter(|path| !path.is_empty()))
    }

    /// Remembers a salvage drive for this volume. An unsafe destination is refused
    /// here, so the problem shows when it is chosen and not at copy time.
    pub fn set_salvage_drive(&self, destination: &str) -> Result<CopyPlan> {
        let request = CopyRequest {
            destination: destination.to_string(),
            ..CopyRequest::default()
        };
        let plan = self.copy_plan(&request)?;
        if let Some(reason) = &plan.refused {
            bail!("{reason}");
        }
        self.with_database(|database| database.set_state(STATE_SALVAGE_DRIVE, destination))?;
        Ok(plan)
    }

    pub fn clear_salvage_drive(&self) -> Result<()> {
        self.with_database(|database| database.set_state(STATE_SALVAGE_DRIVE, ""))
    }

    /// Fills in the salvage drive when a request names no destination.
    fn with_destination(&self, request: &CopyRequest) -> Result<CopyRequest> {
        let mut filled = request.clone();
        if filled.destination.trim().is_empty() {
            filled.destination = self.salvage_drive()?.unwrap_or_default();
        }
        Ok(filled)
    }

    /// Works out what a copy would do, without writing anything.
    pub fn copy_plan(&self, request: &CopyRequest) -> Result<CopyPlan> {
        let request = &self.with_destination(request)?;
        let (selected, below_floor, skipped_partial, skipped_partial_bytes) = {
            let mut selected = Vec::new();
            let (mut below, mut partial, mut partial_bytes) = (0, 0, 0);
            for row in self.copy_rows(request)? {
                if row.confidence < request.min_confidence {
                    below += 1;
                } else if is_partial(&row) && !request.include_partial {
                    partial += 1;
                    partial_bytes += row.length;
                } else {
                    selected.push(row);
                }
            }
            (selected, below, partial, partial_bytes)
        };
        let bytes: u64 = selected.iter().map(|row| row.length).sum();
        let mut by_verdict: HashMap<String, u64> = HashMap::new();
        for row in &selected {
            *by_verdict.entry(row.verdict.clone()).or_insert(0) += 1;
        }
        let mut biggest: Vec<&Row> = selected.iter().collect();
        biggest.sort_by_key(|row| std::cmp::Reverse(row.length));
        let largest = biggest
            .iter()
            .take(LARGEST_SHOWN)
            .map(|row| serde_json::json!({"name": row.name, "bytes": row.length, "confidence": row.confidence}))
            .collect();

        let destination = PathBuf::from(&request.destination);
        let (device, free_bytes, refused) = if request.destination.trim().is_empty() {
            (
                None,
                0,
                Some("Choose a destination, or set a salvage drive, first.".to_string()),
            )
        } else if !destination.is_absolute() {
            (
                None,
                0,
                Some("The destination must be a full path.".to_string()),
            )
        } else {
            let filesystem = filesystem_of(&destination)?;
            let bare = filesystem
                .device
                .strip_prefix("/dev/")
                .unwrap_or(&filesystem.device)
                .to_string();
            let info = list_disks(Some(&bare))
                .ok()
                .filter(|text| !text.trim().is_empty());
            let problem = destination_problem(
                self.source_whole_disk().as_deref(),
                &filesystem.device,
                info.as_deref(),
            )
            .or_else(|| {
                filesystem
                    .read_only
                    .then(|| "This destination is read-only.".to_string())
            });
            (Some(filesystem.device), filesystem.free_bytes, problem)
        };
        // A resumed copy skips these, and they already count against the free space.
        let already_copied_bytes = if destination.is_absolute() {
            let done = read_manifest(&destination.join(MANIFEST_NAME));
            selected
                .iter()
                .filter(|row| {
                    row.id
                        .and_then(|id| done.get(&id))
                        .is_some_and(still_on_destination)
                })
                .map(|row| row.length)
                .sum()
        } else {
            0
        };

        Ok(CopyPlan {
            files: selected.len() as u64,
            bytes,
            already_copied_bytes,
            destination: request.destination.clone(),
            destination_device: device,
            free_bytes,
            fits: bytes.saturating_sub(already_copied_bytes) <= free_bytes,
            refused,
            below_floor,
            skipped_partial,
            skipped_partial_bytes,
            largest,
            by_verdict,
        })
    }

    /// The newest copy this volume knows about, whichever process is running it. A
    /// copy recorded as running that has stopped reporting is shown as stopped.
    pub fn copy_status(&self) -> Result<CopyStatus> {
        let own = self
            .copy
            .status
            .lock()
            .map_err(|_| anyhow!("copy lock poisoned"))?
            .clone();
        let mut stored: CopyStatus = self
            .with_database(|database| database.state(STATE_COPY_STATUS))?
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        if stored.state == "running"
            && now_millis().saturating_sub(stored.updated_at_millis) > COPY_STALE_MILLIS
        {
            stored.state = "stopped".into();
        }
        Ok(if own.updated_at_millis >= stored.updated_at_millis {
            own
        } else {
            stored
        })
    }

    /// Asks the running copy to stop, including one in another process.
    pub fn copy_stop(&self) {
        self.copy.stop.store(true, Ordering::Relaxed);
        let _ = self.with_database(|database| database.set_state(STATE_COPY_STOP, "1"));
    }

    fn should_stop(&self) -> bool {
        if self.copy.stop.load(Ordering::Relaxed) {
            return true;
        }
        let Ok(mut checked) = self.copy.last_stop_check.lock() else {
            return false;
        };
        if checked.is_some_and(|at| at.elapsed() < COPY_REPORT_INTERVAL) {
            return false;
        }
        *checked = Some(Instant::now());
        drop(checked);
        let requested = self
            .with_database(|database| database.state(STATE_COPY_STOP))
            .ok()
            .flatten()
            .is_some_and(|value| value == "1");
        if requested {
            self.copy.stop.store(true, Ordering::Relaxed);
        }
        requested
    }

    fn update_copy(&self, change: impl FnOnce(&mut CopyStatus)) {
        let Ok(mut status) = self.copy.status.lock() else {
            return;
        };
        change(&mut status);
        status.updated_at_millis = now_millis();
        let snapshot = status.clone();
        drop(status);
        self.save_copy_status(&snapshot);
    }

    /// Records progress where the app's window can read it, at most once a second
    /// while running, so it adds almost nothing to the copy's own work.
    fn save_copy_status(&self, status: &CopyStatus) {
        let Ok(mut saved) = self.copy.last_saved.lock() else {
            return;
        };
        let running = status.state == "running";
        if running && saved.is_some_and(|at| at.elapsed() < COPY_REPORT_INTERVAL) {
            return;
        }
        *saved = Some(Instant::now());
        drop(saved);
        if let Ok(text) = serde_json::to_string(status) {
            let _ = self.with_database(|database| database.set_state(STATE_COPY_STATUS, &text));
        }
    }

    /// Starts copying on a background thread.
    pub fn copy_start(self: &Arc<Self>, request: CopyRequest) -> Result<CopyPlan> {
        self.copy_start_groups(vec![request])
    }

    /// Starts copying several selections, one after another. Within each one,
    /// files are read in disk order. A file in more than one is copied once.
    pub fn copy_start_groups(self: &Arc<Self>, groups: Vec<CopyRequest>) -> Result<CopyPlan> {
        let first = groups.first().context("nothing to copy")?;
        let mut request = self.with_destination(first)?;
        let groups: Vec<CopyRequest> = groups
            .iter()
            .map(|group| CopyRequest {
                destination: request.destination.clone(),
                ..group.clone()
            })
            .collect();
        request.entry_ids = groups
            .iter()
            .flat_map(|group| group.entry_ids.clone())
            .collect();
        request.folders = groups
            .iter()
            .flat_map(|group| group.folders.clone())
            .collect();
        let plan = self.copy_plan(&request)?;
        if let Some(reason) = &plan.refused {
            bail!("{reason}");
        }
        if !plan.fits {
            bail!(
                "{} bytes selected, but the destination has only {} bytes free.",
                plan.bytes,
                plan.free_bytes
            );
        }
        if plan.files == 0 {
            bail!("Nothing to copy: no files match this selection.");
        }
        let mut slot = self
            .copy
            .thread
            .lock()
            .map_err(|_| anyhow!("copy lock poisoned"))?;
        if slot.as_ref().is_some_and(|handle| !handle.is_finished()) {
            bail!("A copy is already running.");
        }
        self.copy.stop.store(false, Ordering::Relaxed);
        self.with_database(|database| database.set_state(STATE_COPY_STOP, "0"))?;
        let destination = PathBuf::from(&request.destination);
        fs::create_dir_all(&destination)
            .with_context(|| format!("creating {}", destination.display()))?;
        let manifest = destination.join(MANIFEST_NAME);
        *self
            .copy
            .status
            .lock()
            .map_err(|_| anyhow!("copy lock poisoned"))? = CopyStatus {
            state: "running".into(),
            destination: request.destination.clone(),
            manifest: manifest.display().to_string(),
            files_total: plan.files,
            bytes_total: plan.bytes,
            started_at_millis: now_millis(),
            updated_at_millis: now_millis(),
            ..CopyStatus::default()
        };
        let session = Arc::clone(self);
        *slot = Some(std::thread::spawn(move || {
            let mut seen = HashSet::new();
            let mut outcome = Ok(true);
            for group in &groups {
                outcome = session.run_copy(group, &destination, &manifest, &mut seen);
                if !matches!(outcome, Ok(true)) {
                    break;
                }
            }
            session.update_copy(|status| {
                status.current.clear();
                match outcome {
                    Ok(true) => status.state = "finished".into(),
                    Ok(false) => status.state = "stopped".into(),
                    Err(error) => {
                        status.state = "failed".into();
                        status.error = Some(format!("{error:#}"));
                    }
                }
            });
        }));
        Ok(plan)
    }

    /// Returns false if stopped early.
    fn run_copy(
        &self,
        request: &CopyRequest,
        destination: &Path,
        manifest_path: &Path,
        seen: &mut HashSet<i64>,
    ) -> Result<bool> {
        let mut rows = self.copy_rows(request)?;
        rows.retain(|row| {
            row.confidence >= request.min_confidence
                && (request.include_partial || !is_partial(row))
                && row.id.is_some_and(|id| seen.insert(id))
        });
        // Disk order keeps the head moving forward, which is far faster on a spinning disk.
        rows.sort_by_key(|row| row.first_cluster);

        let done = read_manifest(manifest_path);
        let mut manifest = OpenOptions::new()
            .create(true)
            .append(true)
            .open(manifest_path)?;

        for row in rows {
            if self.should_stop() {
                return Ok(false);
            }
            let id = row.id.context("entry has no id")?;
            if done.get(&id).is_some_and(still_on_destination) {
                self.update_copy(|status| {
                    status.files_skipped += 1;
                    status.bytes_done += row.length;
                });
                continue;
            }
            self.update_copy(|status| status.current = row.name.clone());
            match self.copy_one(&row, id, destination) {
                Ok(Some(result)) => {
                    writeln!(manifest, "{}", serde_json::to_string(&result)?)?;
                    manifest.flush()?;
                    self.update_copy(|status| {
                        status.files_done += 1;
                        // The rest of a file whose chain ended early, so the total still adds up.
                        status.bytes_done += row.length.saturating_sub(result.bytes_written);
                        status.bad_clusters += result.unreadable_clusters;
                        if result.status != "complete" {
                            status.files_partial += 1;
                        }
                    });
                }
                Ok(None) => return Ok(false),
                Err(error) => {
                    // One bad file must not end the copy, unless the destination is the problem.
                    let free = filesystem_of(destination)
                        .map(|filesystem| filesystem.free_bytes)
                        .unwrap_or(0);
                    if free < row.length {
                        bail!("The destination is out of space: {error:#}");
                    }
                    self.update_copy(|status| {
                        status.files_failed += 1;
                        status.bytes_done += row.length;
                        status.error = Some(format!("{}: {error:#}", row.name));
                    });
                }
            }
        }
        Ok(true)
    }

    /// Copies one file. Returns `None` if asked to stop partway.
    fn copy_one(&self, row: &Row, id: i64, destination: &Path) -> Result<Option<ManifestRow>> {
        let clusters = self.with_database(|database| database.entry_clusters(&self.volume, row))?;
        let source_path = row.path.clone().unwrap_or_default();
        let wanted = unused_path(
            &destination.join(relative_target(&source_path, &row.name)),
            id,
        );
        let parent = wanted.parent().context("target has no parent")?;
        fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(".exfat-lens-partial-{id}"));
        let _ = fs::remove_file(&temporary);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;

        let cluster_size = self.volume.boot.cluster_size();
        let mut hasher = Sha256::new();
        let mut written = 0u64;
        let mut unreadable = 0u64;
        let valid = row.valid_length.min(row.length);
        let mut index = 0;
        while index < clusters.len() && written < row.length {
            if self.should_stop() {
                drop(file);
                let _ = fs::remove_file(&temporary);
                return Ok(None);
            }
            // Group clusters that follow each other on disk into one read.
            let mut run = 1;
            while index + run < clusters.len()
                && clusters[index + run] == clusters[index + run - 1] + 1
                && (run as u64 + 1) * cluster_size <= MAX_RUN_BYTES
            {
                run += 1;
            }
            let run_bytes = run as u64 * cluster_size;
            let data = match self.volume.read_bytes(
                self.volume.cluster_offset(clusters[index]),
                run_bytes as usize,
            ) {
                Ok(data) => data,
                Err(_) => {
                    let mut pieces = Vec::with_capacity(run_bytes as usize);
                    for &cluster in &clusters[index..index + run] {
                        match self.volume.read_cluster(cluster) {
                            Ok(data) => pieces.extend_from_slice(&data),
                            Err(_) => {
                                unreadable += 1;
                                pieces.resize(pieces.len() + cluster_size as usize, 0);
                            }
                        }
                    }
                    pieces
                }
            };
            let take = (row.length - written).min(run_bytes) as usize;
            let mut chunk = data[..take].to_vec();
            // Bytes past the valid length are undefined on disk, and read as zeros.
            if written + take as u64 > valid {
                let keep = valid.saturating_sub(written) as usize;
                chunk[keep..].fill(0);
            }
            file.write_all(&chunk)?;
            hasher.update(&chunk);
            written += take as u64;
            index += run;
            self.update_copy(|status| status.bytes_done += take as u64);
        }
        file.sync_all()?;
        drop(file);

        let sha256: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if hash_file(&temporary)? != sha256 {
            let _ = fs::remove_file(&temporary);
            bail!("the copy on the destination does not match what was read, so it was discarded");
        }
        let target = unused_path(&wanted, id);
        fs::rename(&temporary, &target)?;
        if let Some(time) = row.modified.as_deref().and_then(modified_time) {
            if let Ok(handle) = File::options().write(true).open(&target) {
                let _ = handle.set_modified(time);
            }
        }

        let (format, valid_fraction, breaks) = if written <= MAX_FORMAT_CHECK_BYTES {
            let report = verify_stream(&row.name, &fs::read(&target)?);
            if report.format == "unknown" {
                (None, None, None)
            } else {
                (
                    Some(report.format.clone()),
                    Some(report.valid_fraction),
                    Some(report.breaks.len() as u64),
                )
            }
        } else {
            (None, None, None)
        };
        let status = if written < row.length {
            "partial: chain ended early"
        } else if unreadable > 0 {
            "partial: unreadable clusters filled with zeros"
        } else if row.clusters_allocated > 0 {
            "partial: some clusters are in use by current files"
        } else {
            "complete"
        };
        Ok(Some(ManifestRow {
            id,
            name: row.name.clone(),
            source_path,
            target: target.display().to_string(),
            length: row.length,
            bytes_written: written,
            sha256,
            status: status.to_string(),
            verdict: row.verdict.clone(),
            confidence: row.confidence,
            reason: row.reason.clone(),
            first_cluster: row.first_cluster,
            clusters_found: clusters.len() as u64,
            clusters_needed: row.length.div_ceil(cluster_size),
            unreadable_clusters: unreadable,
            format,
            format_valid_fraction: valid_fraction,
            format_breaks: breaks,
            copied_at_millis: now_millis(),
        }))
    }
}

/// Mounted volumes that could receive a copy, with how much room each has.
pub fn list_volumes(source: Option<&Path>) -> Result<Vec<Value>> {
    let source_whole = source
        .and_then(whole_disk_path)
        .and_then(|path| whole_of(&path.display().to_string()));
    let mut volumes = Vec::new();
    let mut candidates = vec![std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?];
    for item in fs::read_dir("/Volumes")?.flatten() {
        candidates.push(item.path());
    }
    for path in candidates {
        let Ok(filesystem) = filesystem_of(&path) else {
            continue;
        };
        let bare = filesystem
            .device
            .strip_prefix("/dev/")
            .unwrap_or(&filesystem.device)
            .to_string();
        let info = list_disks(Some(&bare))
            .ok()
            .filter(|text| !text.trim().is_empty());
        let refused =
            destination_problem(source_whole.as_deref(), &filesystem.device, info.as_deref())
                .or_else(|| {
                    filesystem
                        .read_only
                        .then(|| "This destination is read-only.".to_string())
                });
        volumes.push(serde_json::json!({
            "path": path,
            "device": filesystem.device,
            "freeBytes": filesystem.free_bytes,
            "refused": refused,
        }));
    }
    Ok(volumes)
}
