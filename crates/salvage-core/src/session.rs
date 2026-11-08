//! One open volume and everything that can be asked about it.
//!
//! The app and the MCP tools both go through this type, so the person and the AI
//! always see the same data and act on the same map.

use crate::changes::{AffectedEntry, StructureChanges, StructureCopy};
use crate::content::{
    content_group, entropy_bits, CLASS_DIRECTORY, CLASS_NAMES, GROUP_COUNT, GROUP_NAMES,
};
use crate::exfat::{decode_timestamp, find_sets, EntrySet, FAT_BAD, FAT_END, FIRST_CLUSTER};
use crate::export::{CopyControl, CopyStatus};
use crate::map::{
    to_extents, Extent, IslandRow, MapDb, MapSummary, Overlay, Row, OVERLAY_NOTE,
    OVERLAY_REGION_LABEL, OVERLAY_STRUCT, VERDICT_LIVE,
};
use crate::mount::{DiskReturn, DiskWrites, MountHistory, ReturnWatch, WriteWatch};
use crate::resolve::{
    OWNERSHIP_DIRECTORY, OWNERSHIP_LABELLED, OWNERSHIP_NAMELESS_CHAIN, OWNERSHIP_ONE_ENTRY,
    OWNERSHIP_SEVERAL_ENTRIES, OWNERSHIP_UNEXPLAINED,
};
use crate::scan::{read_progress, run_scan, ScanOptions, ScanProgress};
use crate::source::{mount_line, os_denies_writes, BlockSource, FileSource, ReadStats};
use crate::verify::{verify_stream, VerifyReport};
use crate::volume::{Volume, ALLOC_ALLOCATED, ALLOC_FREE_FAT, ALLOC_FREE_ZERO};
use crate::workspace::{BaselineCheck, Workspace};
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const MAX_READ_BYTES: u64 = 16 * 1024 * 1024;
const MAX_QUERY_ROWS: usize = 5000;
/// Written clusters looked up one by one for their owners. Past this, the list is cut short.
const MAX_WRITTEN_CLUSTERS_EXPLAINED: usize = 20_000;
const FIND_CHUNK_BYTES: u64 = 8 * 1024 * 1024;
const FIND_TIME_LIMIT: Duration = Duration::from_secs(90);
const MIN_RATE_SAMPLE: Duration = Duration::from_secs(3);
pub(crate) const DISK_GONE: &str =
    "The disk has been disconnected, so nothing can be read from it.";
const OWNERSHIP_NAMES: [&str; 6] = [
    "unexplained",
    "claimed by one entry",
    "claimed by several entries",
    "directory",
    "nameless FAT chain",
    "explained by a hypothesis",
];

/// Where a byte offset is measured from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Partition,
    WholeDisk,
}

impl Scope {
    pub fn parse(text: Option<&str>) -> Result<Self> {
        match text.unwrap_or("partition") {
            "partition" => Ok(Self::Partition),
            "disk" => Ok(Self::WholeDisk),
            other => bail!("unknown scope {other}: use partition or disk"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Layer {
    Allocation,
    Content,
    Entropy,
    Ownership,
    Confidence,
    Folders,
    Lens,
}

/// A saved view: which clusters are in focus. Unset fields do not filter.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Lens {
    pub allocation: Option<Vec<u8>>,
    pub groups: Option<Vec<u8>>,
    pub ownership: Option<Vec<u8>>,
    pub entropy_min: Option<u8>,
    pub entropy_max: Option<u8>,
    pub confidence_min: Option<f64>,
    pub start_cluster: Option<u32>,
    pub end_cluster: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub device: String,
    pub whole_disk: Option<String>,
    pub os_denies_writes: bool,
    pub mounted: Option<String>,
    pub baseline: BaselineCheck,
    /// What changed since the baseline, when it has changed and a copy of it is kept.
    pub changes: Option<crate::changes::ChangeSummary>,
    /// The map reads the FAT, bitmap and root from the baseline's copy, not from the disk.
    pub structures_from_baseline: bool,
    pub reads: ReadStats,
    pub scan: ScanProgress,
    pub scan_in_this_app: bool,
    pub scan_held_elsewhere: bool,
    pub scan_error: Option<String>,
    pub summary: Option<MapSummary>,
    pub copy: CopyStatus,
    pub file_tests: crate::filetest::FileTestStatus,
    pub writes: DiskWrites,
    pub disk_gone: bool,
    pub disk_back: Option<DiskReturn>,
    pub keep_awake: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DecodedField {
    pub name: String,
    pub offset: u64,
    pub length: u64,
    pub kind: String,
    pub value: Value,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterInfo {
    pub cluster: u32,
    pub offset: u64,
    pub disk_offset: Option<u64>,
    pub allocated: bool,
    pub fat_value: u32,
    pub fat_meaning: String,
    pub predecessor: Option<u32>,
    pub chain_head: u32,
    pub island: Option<IslandRow>,
    pub content_class: String,
    pub entropy_bits: f64,
    pub ownership: String,
    pub confidence: f64,
    pub sets: Vec<EntrySet>,
    pub owners: Vec<Row>,
    pub notes: Vec<Overlay>,
    pub structures: Vec<Value>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FindResult {
    pub hits: Vec<u64>,
    pub next_offset: u64,
    pub finished: bool,
    pub bytes_searched: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamTest {
    pub extents: Vec<Extent>,
    pub clusters: u64,
    pub needed: u64,
    pub length: u64,
    pub allocated_clusters: u64,
    pub report: VerifyReport,
    /// Which cluster of the stream each structural break falls in.
    pub break_locations: Vec<Value>,
    pub profile: Vec<Value>,
}

struct RateSample {
    at: Instant,
    next_cluster: u32,
    bytes_per_second: f64,
}

pub struct Session {
    pub path: PathBuf,
    pub volume: Volume,
    pub workspace: Workspace,
    pub baseline: Mutex<BaselineCheck>,
    changes: Mutex<Option<StructureChanges>>,
    /// The FAT, bitmap and root are read from the baseline's copy, because the disk's own
    /// have changed since.
    structures_from_baseline: bool,
    pub database: Mutex<MapDb>,
    whole_disk: Mutex<Option<Arc<FileSource>>>,
    scan_stop: Arc<AtomicBool>,
    scan_thread: Mutex<Option<JoinHandle<()>>>,
    scan_error: Arc<Mutex<Option<String>>>,
    rate: Mutex<RateSample>,
    pub(crate) copy: CopyControl,
    pub(crate) browse: Mutex<crate::browse::BrowseCache>,
    pub(crate) file_tests: FileTestControl,
    writes: Mutex<WriteWatch>,
    returns: Mutex<ReturnWatch>,
    pub(crate) zip: Mutex<Option<(i64, Arc<crate::archive::ZipListing>)>>,
    /// Where macOS wrote, kept a while: every file selected is checked against it.
    macos: Mutex<Option<(std::time::Instant, Arc<Vec<MacosWrites>>)>>,
}

#[derive(Default)]
pub(crate) struct FileTestControl {
    pub(crate) last_viewed: std::sync::atomic::AtomicU64,
    pub(crate) status: Mutex<crate::filetest::FileTestStatus>,
    pub(crate) stop: AtomicBool,
    pub(crate) thread: Mutex<Option<JoinHandle<()>>>,
}

/// `/dev/rdisk12s2` becomes `/dev/rdisk12`. Anything else has no whole-disk parent.
pub fn whole_disk_path(path: &Path) -> Option<PathBuf> {
    let text = path.to_str()?;
    let rest = text
        .strip_prefix("/dev/rdisk")
        .or_else(|| text.strip_prefix("/dev/disk"))?;
    let (disk, _partition) = rest.split_once('s')?;
    disk.chars()
        .all(|c| c.is_ascii_digit())
        .then(|| PathBuf::from(format!("/dev/rdisk{disk}")))
}

/// What the OS currently allows on a device, and exactly what to run to fix it.
pub fn disk_access_report(path: &Path) -> Value {
    let mut devices = vec![path.to_path_buf()];
    if let Some(whole) = whole_disk_path(path) {
        devices.push(whole);
    }
    let mut unreadable = Vec::new();
    let mut mounted_lines = Vec::new();
    let reports: Vec<Value> = devices
        .iter()
        .map(|device| {
            let exists = device.exists();
            let read = FileSource::open(device).and_then(|source| {
                let mut sector = [0u8; 512];
                source.read_exact_at(0, &mut sector)
            });
            let error = read.as_ref().err().map(ToString::to_string);
            if read.is_err() {
                unreadable.push(device.display().to_string());
            }
            let mounted = mount_line(device);
            if let Some(line) = &mounted {
                mounted_lines.push(line.clone());
            }
            json!({
                "path": device,
                "exists": exists,
                "readable": read.is_ok(),
                "readError": error,
                "osDeniesWrites": os_denies_writes(device),
                "mounted": mounted,
            })
        })
        .collect();

    let mut instructions = Vec::new();
    if !unreadable.is_empty() {
        if devices.iter().all(|device| !device.exists()) {
            instructions.push(
                "The device does not exist at this path. Disk numbers change when a disk is reconnected. \
                 Use the list_disks tool to find the current number."
                    .to_string(),
            );
        } else {
            instructions.push(format!(
                "Ask the user to run this in their own terminal. It gives this account read-only access, \
                 and lasts until the disk is reconnected: sudo chgrp staff {}",
                unreadable.join(" ")
            ));
        }
    }
    if !mounted_lines.is_empty() {
        instructions.push(format!(
            "The volume is mounted, so macOS may write to it. Ask the user to unmount it first: diskutil unmount {}",
            path.display().to_string().replace("/dev/rdisk", "/dev/disk")
        ));
    }
    json!({
        "devices": reports,
        "instructions": instructions,
        "ready": unreadable.is_empty() && mounted_lines.is_empty(),
    })
}

/// Runs a read-only `diskutil` query. Device names are checked, and no shell is involved.
pub fn list_disks(device: Option<&str>) -> Result<String> {
    let mut command = Command::new("/usr/sbin/diskutil");
    match device {
        Some(name) => {
            let bare = name.strip_prefix("/dev/").unwrap_or(name);
            if !bare.chars().all(|c| c.is_ascii_alphanumeric()) {
                bail!("not a device name: {name}");
            }
            command.args(["info", bare]);
        }
        None => {
            command.arg("list");
        }
    }
    let output = command.output().context("running diskutil")?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

impl Session {
    pub fn open(path: &Path) -> Result<Arc<Self>> {
        let source = Arc::new(FileSource::open(path).with_context(|| {
            format!(
                "cannot open {}. Use the disk_access tool to see what needs granting.",
                path.display()
            )
        })?);
        let volume = Volume::open(source)?;
        let workspace = Workspace::for_serial(volume.boot.volume_serial)?;
        let session = Self::assemble(path, volume, workspace)?;
        crate::startup::remember_volume(&session.workspace, path);
        Ok(session)
    }

    /// Builds a session around any source, for tests and for disk images.
    pub fn with_source(
        path: &Path,
        source: Arc<dyn BlockSource>,
        workspace: Workspace,
    ) -> Result<Arc<Self>> {
        Self::assemble(path, Volume::open(source)?, workspace)
    }

    fn assemble(path: &Path, volume: Volume, workspace: Workspace) -> Result<Arc<Self>> {
        let baseline = workspace.check_baseline(&volume.structures)?;
        let check = baseline.clone();
        // When the disk's structures have changed, the copy from the baseline is what the map
        // was built from and what still describes the files, so it is read instead.
        let copy = (!baseline.unchanged)
            .then(|| workspace.baseline_copy())
            .flatten();
        let on_disk = copy.as_ref().map(|_| volume.structures.clone());
        let structures_from_baseline = copy.is_some();
        let volume = match copy {
            Some(copy) => Volume::from_structures(volume.source().clone(), copy)?,
            None => volume,
        };
        let database = MapDb::open(&workspace.database())?;
        let next_cluster = database
            .state_u64("next_cluster")?
            .max(u64::from(FIRST_CLUSTER)) as u32;
        let session = Arc::new(Self {
            path: path.to_path_buf(),
            volume,
            workspace,
            baseline: Mutex::new(baseline),
            changes: Mutex::new(None),
            structures_from_baseline,
            database: Mutex::new(database),
            whole_disk: Mutex::new(None),
            scan_stop: Arc::new(AtomicBool::new(false)),
            scan_thread: Mutex::new(None),
            scan_error: Arc::new(Mutex::new(None)),
            rate: Mutex::new(RateSample {
                at: Instant::now(),
                next_cluster,
                bytes_per_second: 0.0,
            }),
            copy: CopyControl::default(),
            browse: Mutex::default(),
            file_tests: FileTestControl::default(),
            writes: Mutex::new(WriteWatch::open(path)),
            returns: Mutex::new(ReturnWatch::open(path)),
            zip: Mutex::new(None),
            macos: Mutex::new(None),
        });
        // The baseline check still says the volume changed if this fails; only the detail is lost.
        let current = on_disk.as_ref().unwrap_or(&session.volume.structures);
        if let Err(error) = session.explain_changes(current, &check) {
            eprintln!("could not work out what changed since the baseline: {error:#}");
        }
        Ok(session)
    }

    fn lock_database(&self) -> Result<std::sync::MutexGuard<'_, MapDb>> {
        self.database
            .lock()
            .map_err(|_| anyhow!("database lock poisoned"))
    }

    pub fn with_database<T>(&self, action: impl FnOnce(&MapDb) -> Result<T>) -> Result<T> {
        action(&*self.lock_database()?)
    }

    fn whole_source(&self) -> Result<Arc<FileSource>> {
        let mut guard = self
            .whole_disk
            .lock()
            .map_err(|_| anyhow!("disk lock poisoned"))?;
        if let Some(source) = guard.as_ref() {
            return Ok(source.clone());
        }
        let path = whole_disk_path(&self.path)
            .ok_or_else(|| anyhow!("{} has no whole-disk device", self.path.display()))?;
        let source = Arc::new(FileSource::open(&path).with_context(|| {
            format!(
                "cannot open {}. Ask the user to run: sudo chgrp staff {}",
                path.display(),
                path.display()
            )
        })?);
        *guard = Some(source.clone());
        Ok(source)
    }

    pub fn read(&self, scope: Scope, offset: u64, length: u64) -> Result<Vec<u8>> {
        if length > MAX_READ_BYTES {
            bail!("read of {length} bytes exceeds the {MAX_READ_BYTES}-byte limit. Read in smaller pieces.");
        }
        match scope {
            Scope::Partition => self.volume.read_bytes(offset, length as usize),
            Scope::WholeDisk => {
                let mut buffer = vec![0u8; length as usize];
                self.whole_source()?
                    .read_exact_at(offset, &mut buffer)
                    .with_context(|| format!("reading {length} bytes at disk offset {offset}"))?;
                Ok(buffer)
            }
        }
    }

    pub fn status(&self) -> Result<Status> {
        let (disk_gone, disk_back) = self
            .returns
            .lock()
            .map_err(|_| anyhow!("disk watch lock poisoned"))?
            .report(self.volume.source().as_ref());
        let database = self.lock_database()?;
        let mut scan = read_progress(&database, &self.volume)?;
        {
            let mut rate = self
                .rate
                .lock()
                .map_err(|_| anyhow!("rate lock poisoned"))?;
            let elapsed = rate.at.elapsed();
            if elapsed >= MIN_RATE_SAMPLE {
                let moved = u64::from(scan.next_cluster.saturating_sub(rate.next_cluster));
                rate.bytes_per_second =
                    (moved * self.volume.boot.cluster_size()) as f64 / elapsed.as_secs_f64();
                rate.at = Instant::now();
                rate.next_cluster = scan.next_cluster;
            }
            scan.bytes_per_second = rate.bytes_per_second;
        }
        let summary = database.summary()?;
        drop(database);
        let scan_in_this_app = self
            .scan_thread
            .lock()
            .map_err(|_| anyhow!("scan lock poisoned"))?
            .as_ref()
            .is_some_and(|handle| !handle.is_finished());
        Ok(Status {
            device: self.path.display().to_string(),
            whole_disk: whole_disk_path(&self.path).map(|path| path.display().to_string()),
            os_denies_writes: os_denies_writes(&self.path),
            mounted: mount_line(&self.path),
            baseline: self
                .baseline
                .lock()
                .map_err(|_| anyhow!("baseline lock poisoned"))?
                .clone(),
            changes: self
                .changes
                .lock()
                .map_err(|_| anyhow!("changes lock poisoned"))?
                .as_ref()
                .map(StructureChanges::summary),
            structures_from_baseline: self.structures_from_baseline,
            reads: self.volume.source().read_stats(),
            scan,
            scan_in_this_app,
            scan_held_elsewhere: !scan_in_this_app && self.scan_lock_held(),
            scan_error: self
                .scan_error
                .lock()
                .map_err(|_| anyhow!("error lock poisoned"))?
                .clone(),
            summary,
            copy: self.copy_status()?,
            file_tests: self.file_test_status()?,
            writes: self
                .writes
                .lock()
                .map_err(|_| anyhow!("write counter lock poisoned"))?
                .report(),
            disk_gone,
            disk_back,
            keep_awake: crate::mount::disks_kept_awake(),
        })
    }

    /// Refuses to go on once the disk has gone, because what failed reads leave behind says
    /// nothing about the files. A single bad spot is different, and never comes here.
    pub(crate) fn require_disk(&self) -> Result<()> {
        if self.volume.source().is_gone() {
            bail!(DISK_GONE);
        }
        Ok(())
    }

    /// The dates the volume's own system folders changed, as evidence of earlier mounts.
    pub fn mount_history(&self) -> Result<MountHistory> {
        let root = self.volume.boot.root_cluster;
        Ok(MountHistory {
            dates: self.with_database(|database| database.mount_dates(root))?,
            dirty_flag: self.volume.boot.volume_dirty(),
            baseline_recorded_at_millis: self
                .baseline
                .lock()
                .map_err(|_| anyhow!("baseline lock poisoned"))?
                .recorded_at_millis,
        })
    }

    /// Where macOS wrote on this volume, by date: the clusters of the files in its
    /// `.Spotlight-V100` and `.fseventsd` folders, as runs. A lost file that runs through one
    /// may have been overwritten there.
    pub fn macos_writes(&self) -> Result<Vec<MacosWrites>> {
        let root = self.volume.boot.root_cluster;
        let entries = self.with_database(|database| database.macos_system_entries(root))?;
        let mut by_date: std::collections::BTreeMap<String, (u64, Vec<u32>)> = Default::default();
        for (date, row) in entries {
            let clusters =
                self.with_database(|database| database.entry_clusters(&self.volume, &row))?;
            let day = by_date.entry(date).or_default();
            day.0 += 1;
            day.1.extend(clusters);
        }
        Ok(by_date
            .into_iter()
            .map(|(date, (entries, mut clusters))| {
                clusters.sort_unstable();
                clusters.dedup();
                MacosWrites {
                    date,
                    entries,
                    clusters: clusters.len() as u64,
                    extents: crate::map::to_extents(&clusters),
                }
            })
            .collect())
    }

    /// The dates macOS wrote to any of these clusters, oldest first.
    pub fn macos_wrote_over(&self, clusters: &[u32]) -> Result<Vec<String>> {
        let wanted: std::collections::HashSet<u32> = clusters.iter().copied().collect();
        let writes = {
            let mut cache = self
                .macos
                .lock()
                .map_err(|_| anyhow!("macos lock poisoned"))?;
            match cache.as_ref() {
                Some((at, writes)) if at.elapsed() < MACOS_WRITES_LIFETIME => writes.clone(),
                _ => {
                    let writes = Arc::new(self.macos_writes()?);
                    *cache = Some((std::time::Instant::now(), writes.clone()));
                    writes
                }
            }
        };
        Ok(writes
            .iter()
            .filter(|day| {
                day.extents.iter().any(|extent| {
                    (extent.first..extent.first + extent.count)
                        .any(|cluster| wanted.contains(&cluster))
                })
            })
            .map(|day| day.date.clone())
            .collect())
    }

    fn scan_lock_held(&self) -> bool {
        let Ok(file) = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(self.workspace.lock())
        else {
            return false;
        };
        match file.try_lock() {
            Ok(()) => {
                let _ = file.unlock();
                false
            }
            Err(_) => true,
        }
    }

    /// Re-reads the volume's structures from disk and compares them with the
    /// baseline. Any difference means something has written to the volume.
    pub fn verify_unchanged(&self) -> Result<BaselineCheck> {
        let fresh = Volume::read_structures(self.volume.source().as_ref())?;
        let check = self.workspace.check_baseline(&fresh)?;
        *self
            .baseline
            .lock()
            .map_err(|_| anyhow!("baseline lock poisoned"))? = check.clone();
        self.explain_changes(&fresh, &check)?;
        Ok(check)
    }

    /// When the structures differ from the baseline, works out what was written and which
    /// found entries claimed those clusters, and keeps that in the workspace. A change already
    /// explained keeps the time it was first seen.
    pub(crate) fn explain_changes(
        &self,
        current: &StructureCopy,
        check: &BaselineCheck,
    ) -> Result<()> {
        let explained = if check.unchanged {
            None
        } else if let Some(known) = self
            .workspace
            .read_changes()
            .filter(|known| known.current_hash == check.current_hash)
        {
            Some(known)
        } else if let Some(baseline) = self.workspace.baseline_copy() {
            let mut changes = crate::changes::compare(
                &baseline,
                current,
                self.volume.limits(),
                self.volume.boot.last_cluster_exclusive(),
            );
            changes.baseline_recorded_at_millis = check.recorded_at_millis;
            changes.detected_at_millis = crate::workspace::now_millis();
            changes.affected = self.affected_by(&changes.written_clusters)?;
            self.workspace.write_changes(&changes)?;
            Some(changes)
        } else {
            None
        };
        *self
            .changes
            .lock()
            .map_err(|_| anyhow!("changes lock poisoned"))? = explained;
        Ok(())
    }

    /// The found files and folders that claimed any of `clusters`, with how many of them each.
    /// Chains are followed through the structures the map was built from, so a file is found
    /// the way the scan found it.
    fn affected_by(&self, clusters: &[u32]) -> Result<Vec<AffectedEntry>> {
        let volume = &self.volume;
        let mut affected: std::collections::BTreeMap<i64, AffectedEntry> = Default::default();
        self.with_database(|database| {
            for &cluster in clusters.iter().take(MAX_WRITTEN_CLUSTERS_EXPLAINED) {
                for row in database.owners(volume, cluster)? {
                    // A file still visible in the filesystem is what wrote there, not a loss.
                    let Some(id) = row.id.filter(|_| row.verdict != VERDICT_LIVE) else {
                        continue;
                    };
                    affected
                        .entry(id)
                        .or_insert_with(|| AffectedEntry {
                            id,
                            name: row.name.clone(),
                            path: row.path.clone(),
                            length: row.length,
                            is_dir: row.kind != "file",
                            clusters_written: 0,
                        })
                        .clusters_written += 1;
                }
            }
            Ok(())
        })?;
        let mut list: Vec<AffectedEntry> = affected.into_values().collect();
        list.sort_by_key(|entry| std::cmp::Reverse(entry.length));
        Ok(list)
    }

    /// Everything known about what changed since the baseline.
    pub fn structure_changes(&self) -> Result<Option<StructureChanges>> {
        Ok(self
            .changes
            .lock()
            .map_err(|_| anyhow!("changes lock poisoned"))?
            .clone())
    }

    pub fn overview(&self) -> Value {
        let boot = &self.volume.boot;
        let backup = &self.volume.backup_boot;
        let main_json = serde_json::to_value(boot).unwrap_or(Value::Null);
        let backup_json = serde_json::to_value(backup).unwrap_or(Value::Null);
        let mut differences = serde_json::Map::new();
        if let (Value::Object(main), Value::Object(other)) = (&main_json, &backup_json) {
            for (key, value) in main {
                if other.get(key) != Some(value) {
                    differences.insert(
                        key.clone(),
                        json!({"main": value, "backup": other.get(key)}),
                    );
                }
            }
        }
        let size = boot.cluster_size();
        json!({
            "device": self.path,
            "wholeDisk": whole_disk_path(&self.path),
            "workspace": self.workspace.dir(),
            "bootMain": main_json,
            "bootBackup": backup_json,
            "bootDifferences": differences,
            "bootCheck": self.volume.boot_check,
            "geometry": {
                "clusterSize": size,
                "clusterCount": boot.cluster_count,
                "firstCluster": FIRST_CLUSTER,
                "heapOffset": boot.heap_offset(),
                "fatOffset": u64::from(boot.fat_offset) * boot.bytes_per_sector(),
                "fatBytes": u64::from(boot.fat_length) * boot.bytes_per_sector(),
                "volumeBytes": boot.volume_length * boot.bytes_per_sector(),
                "partitionOffsetOnDisk": boot.partition_offset * boot.bytes_per_sector(),
                "rootCluster": boot.root_cluster,
            },
            "allocation": self.volume.stats,
            "allocationBytes": {
                "allocated": self.volume.stats.allocated * size,
                "freeWithFat": self.volume.stats.free_with_fat * size,
                "freeZeroFat": self.volume.stats.free_zero_fat * size,
            },
            "structureHash": self.volume.structure_hash,
            "classNames": CLASS_NAMES,
            "groupNames": GROUP_NAMES,
            "ownershipNames": OWNERSHIP_NAMES,
        })
    }

    fn cluster_maps(&self) -> ClusterMaps {
        let count = self.volume.boot.cluster_count;
        let workspace = &self.workspace;
        ClusterMaps {
            classes: workspace.read_cluster_map(&workspace.classes(), count),
            entropy: workspace.read_cluster_map(&workspace.entropy(), count),
            ownership: workspace.read_cluster_map(&workspace.ownership(), count),
            confidence: workspace.read_cluster_map(&workspace.confidence(), count),
        }
    }

    /// Downsamples one layer to `buckets` bytes for drawing. Value meanings:
    /// allocation is 1-3, content is a group, ownership is its value plus one,
    /// and the rest are 0-255 magnitudes.
    pub fn map_bytes(
        &self,
        layer: Layer,
        start: u32,
        end: u32,
        buckets: usize,
        floor: f64,
        lens: Option<&Lens>,
    ) -> Vec<u8> {
        let first = start.max(FIRST_CLUSTER);
        let last = end.min(self.volume.boot.last_cluster_exclusive());
        if first >= last || buckets == 0 {
            return vec![0; buckets];
        }
        let maps = self.cluster_maps();
        let span = f64::from(last - first);
        let floor_byte = (floor * 255.0).round() as u8;
        (0..buckets)
            .map(|bucket| {
                let from = first + (span * bucket as f64 / buckets as f64) as u32;
                let to = (first + (span * (bucket + 1) as f64 / buckets as f64) as u32)
                    .max(from + 1)
                    .min(last);
                self.bucket_value(layer, from, to, &maps, floor_byte, lens)
            })
            .collect()
    }

    /// Draws a layer as a `buckets` by `rows` raster, top row first. In category
    /// layers each column is a stack whose bands are proportional to what the
    /// bucket holds, and any category present gets at least one row, so small or
    /// rare things stay visible at every zoom level. Other layers repeat one value.
    #[allow(clippy::too_many_arguments)]
    pub fn map_raster(
        &self,
        layer: Layer,
        start: u32,
        end: u32,
        buckets: usize,
        rows: usize,
        floor: f64,
        lens: Option<&Lens>,
    ) -> Vec<u8> {
        let rows = rows.max(1);
        let mut raster = vec![0u8; buckets * rows];
        let first = start.max(FIRST_CLUSTER);
        let last = end.min(self.volume.boot.last_cluster_exclusive());
        if first >= last || buckets == 0 {
            return raster;
        }
        let maps = self.cluster_maps();
        let span = f64::from(last - first);
        let floor_byte = (floor * 255.0).round() as u8;
        for bucket in 0..buckets {
            let from = first + (span * bucket as f64 / buckets as f64) as u32;
            let to = (first + (span * (bucket + 1) as f64 / buckets as f64) as u32)
                .max(from + 1)
                .min(last);
            let indexes = (from - FIRST_CLUSTER) as usize..(to - FIRST_CLUSTER) as usize;
            let column: Vec<u8> = match layer {
                Layer::Allocation => {
                    let mut counts = [0u32; 3];
                    for cluster in from..to {
                        counts[usize::from(self.volume.allocation_class(cluster)) - 1] += 1;
                    }
                    stack_column(&counts, &[1, 2, 3], rows)
                }
                Layer::Content => {
                    let mut counts = [0u32; GROUP_COUNT];
                    for &class in &maps.classes[indexes] {
                        counts[usize::from(content_group(class))] += 1;
                    }
                    let ordered: Vec<u32> = CONTENT_STACK_ORDER
                        .iter()
                        .map(|&group| counts[usize::from(group)])
                        .collect();
                    stack_column(&ordered, &CONTENT_STACK_ORDER, rows)
                }
                Layer::Ownership => {
                    let mut counts = [0u32; 6];
                    for &value in &maps.ownership[indexes] {
                        counts[usize::from(value).min(5)] += 1;
                    }
                    let ordered: Vec<u32> = OWNERSHIP_STACK_ORDER
                        .iter()
                        .map(|&value| counts[usize::from(value)])
                        .collect();
                    let display: Vec<u8> = OWNERSHIP_STACK_ORDER
                        .iter()
                        .map(|&value| value + 1)
                        .collect();
                    stack_column(&ordered, &display, rows)
                }
                _ => vec![self.bucket_value(layer, from, to, &maps, floor_byte, lens); rows],
            };
            for (row, &value) in column.iter().enumerate() {
                raster[row * buckets + bucket] = value;
            }
        }
        raster
    }

    fn bucket_value(
        &self,
        layer: Layer,
        from: u32,
        to: u32,
        maps: &ClusterMaps,
        floor: u8,
        lens: Option<&Lens>,
    ) -> u8 {
        let indexes = (from - FIRST_CLUSTER) as usize..(to - FIRST_CLUSTER) as usize;
        let size = (to - from) as usize;
        match layer {
            Layer::Allocation => {
                let mut counts = [0u32; 4];
                for cluster in from..to {
                    counts[usize::from(self.volume.allocation_class(cluster))] += 1;
                }
                dominant(&counts[1..]) + 1
            }
            Layer::Content => {
                let mut counts = [0u32; GROUP_COUNT];
                for &class in &maps.classes[indexes] {
                    counts[usize::from(content_group(class))] += 1;
                }
                dominant(&counts)
            }
            Layer::Ownership => {
                let mut counts = [0u32; 6];
                for &value in &maps.ownership[indexes] {
                    counts[usize::from(value).min(5)] += 1;
                }
                dominant(&counts) + 1
            }
            Layer::Entropy => {
                let total: u64 = maps.entropy[indexes]
                    .iter()
                    .map(|&value| u64::from(value))
                    .sum();
                (total / size as u64) as u8
            }
            Layer::Confidence => {
                let best = maps.confidence[indexes].iter().copied().max().unwrap_or(0);
                if best >= floor.max(1) {
                    best
                } else {
                    0
                }
            }
            Layer::Folders => {
                let hits = maps.classes[indexes]
                    .iter()
                    .filter(|&&class| class == CLASS_DIRECTORY)
                    .count();
                let scale = ((1 + hits) as f64).ln() / ((1 + size) as f64).ln();
                if hits == 0 {
                    0
                } else {
                    (scale * 254.0).round().max(40.0) as u8
                }
            }
            Layer::Lens => {
                let Some(lens) = lens else { return 0 };
                let hits = (from..to)
                    .filter(|&cluster| self.lens_matches(lens, cluster, maps))
                    .count();
                if hits == 0 {
                    0
                } else {
                    ((hits * 254 / size) as u8).max(30)
                }
            }
        }
    }

    fn lens_matches(&self, lens: &Lens, cluster: u32, maps: &ClusterMaps) -> bool {
        let index = (cluster - FIRST_CLUSTER) as usize;
        lens.start_cluster.is_none_or(|start| cluster >= start)
            && lens.end_cluster.is_none_or(|end| cluster < end)
            && lens
                .allocation
                .as_ref()
                .is_none_or(|allowed| allowed.contains(&self.volume.allocation_class(cluster)))
            && lens
                .groups
                .as_ref()
                .is_none_or(|allowed| allowed.contains(&content_group(maps.classes[index])))
            && lens
                .ownership
                .as_ref()
                .is_none_or(|allowed| allowed.contains(&maps.ownership[index]))
            && lens
                .entropy_min
                .is_none_or(|floor| maps.entropy[index] >= floor)
            && lens
                .entropy_max
                .is_none_or(|ceiling| maps.entropy[index] <= ceiling)
            && lens
                .confidence_min
                .is_none_or(|floor| f64::from(maps.confidence[index]) / 255.0 >= floor)
    }

    /// Counts for a cluster range under every layer: the bird's-eye numbers.
    pub fn region_stats(&self, start: u32, end: u32) -> Value {
        let first = start.max(FIRST_CLUSTER);
        let last = end.min(self.volume.boot.last_cluster_exclusive());
        let maps = self.cluster_maps();
        let mut allocation = [0u64; 4];
        let mut groups = [0u64; GROUP_COUNT];
        let mut ownership = [0u64; 6];
        let mut entropy_bins = [0u64; 16];
        let mut confidence_bins = [0u64; 11];
        for cluster in first..last {
            let index = (cluster - FIRST_CLUSTER) as usize;
            allocation[usize::from(self.volume.allocation_class(cluster))] += 1;
            groups[usize::from(content_group(maps.classes[index]))] += 1;
            ownership[usize::from(maps.ownership[index]).min(5)] += 1;
            entropy_bins[usize::from(maps.entropy[index] >> 4)] += 1;
            confidence_bins[usize::from(maps.confidence[index]) * 10 / 255] += 1;
        }
        json!({
            "startCluster": first,
            "endCluster": last,
            "clusterSize": self.volume.boot.cluster_size(),
            "allocation": {
                "allocated": allocation[usize::from(ALLOC_ALLOCATED)],
                "freeWithFat": allocation[usize::from(ALLOC_FREE_FAT)],
                "freeZeroFat": allocation[usize::from(ALLOC_FREE_ZERO)],
            },
            "contentGroups": GROUP_NAMES.iter().zip(groups).map(|(name, count)| json!({"group": name, "clusters": count})).collect::<Vec<_>>(),
            "ownership": {
                "unexplained": ownership[usize::from(OWNERSHIP_UNEXPLAINED)],
                "claimedByOneEntry": ownership[usize::from(OWNERSHIP_ONE_ENTRY)],
                "claimedBySeveralEntries": ownership[usize::from(OWNERSHIP_SEVERAL_ENTRIES)],
                "directory": ownership[usize::from(OWNERSHIP_DIRECTORY)],
                "namelessChain": ownership[usize::from(OWNERSHIP_NAMELESS_CHAIN)],
                "explainedByHypothesis": ownership[usize::from(OWNERSHIP_LABELLED)],
            },
            "entropyHistogram16Bins": entropy_bins,
            "confidenceHistogramTenths": confidence_bins,
        })
    }

    pub fn fat_meaning(&self, value: u32) -> String {
        match value {
            0 => "zero: no chain information".to_string(),
            FAT_END => "end of chain".to_string(),
            FAT_BAD => "marked bad".to_string(),
            next if self.volume.valid_cluster(next) => format!("next cluster is {next}"),
            other => format!("out of range: {other:#010x}"),
        }
    }

    pub fn inspect_cluster(&self, cluster: u32) -> Result<ClusterInfo> {
        if !self.volume.valid_cluster(cluster) {
            bail!("cluster {cluster} is outside the volume");
        }
        let data = self.volume.read_cluster(cluster)?;
        let lookahead_end = self.volume.boot.last_cluster_exclusive();
        let mut search = data.clone();
        if cluster + 1 < lookahead_end {
            search.extend(
                self.volume
                    .read_bytes(self.volume.cluster_offset(cluster + 1), 1024)?,
            );
        }
        let sets = find_sets(&search, data.len(), self.volume.limits());
        let (class, _) = crate::content::classify(&data, sets.iter().any(|set| set.checksum_ok));
        let index = (cluster - FIRST_CLUSTER) as usize;
        let count = self.volume.boot.cluster_count;
        let ownership = self
            .workspace
            .read_cluster_map(&self.workspace.ownership(), count)[index];
        let confidence = self
            .workspace
            .read_cluster_map(&self.workspace.confidence(), count)[index];
        let fat_value = self.volume.fat[cluster as usize];
        let offset = self.volume.cluster_offset(cluster);
        let chain_head = self.volume.chain_head(cluster);

        let database = self.lock_database()?;
        let owners = database.owners(&self.volume, cluster)?;
        let island = database.island(chain_head)?;
        let overlays = database.overlays(true)?;
        drop(database);
        let size = self.volume.boot.cluster_size();
        let range = offset..offset + size;
        let notes: Vec<Overlay> = overlays
            .iter()
            .filter(|overlay| {
                let here = i64::from(cluster);
                match overlay.kind.as_str() {
                    OVERLAY_NOTE => overlay.target == Some(here),
                    OVERLAY_REGION_LABEL => {
                        let end = overlay
                            .payload
                            .get("end")
                            .and_then(Value::as_i64)
                            .unwrap_or(0);
                        overlay
                            .target
                            .is_some_and(|start| (start..end).contains(&here))
                    }
                    _ => false,
                }
            })
            .cloned()
            .collect();
        let mut structures = Vec::new();
        for overlay in overlays
            .iter()
            .filter(|overlay| overlay.kind == OVERLAY_STRUCT)
        {
            let Some(base) = overlay.target.map(|value| value as u64) else {
                continue;
            };
            if range.contains(&base) {
                let fields = self.decode_struct(&overlay.payload, Scope::Partition, base)?;
                structures
                    .push(json!({"overlay": overlay.id, "note": overlay.note, "fields": fields}));
            }
        }

        Ok(ClusterInfo {
            cluster,
            offset,
            disk_offset: whole_disk_path(&self.path).map(|_| {
                self.volume.boot.partition_offset * self.volume.boot.bytes_per_sector() + offset
            }),
            allocated: self.volume.is_allocated(cluster),
            fat_value,
            fat_meaning: self.fat_meaning(fat_value),
            predecessor: self.volume.predecessor(cluster),
            chain_head,
            island,
            content_class: CLASS_NAMES[usize::from(class)].to_string(),
            entropy_bits: entropy_bits(&data),
            ownership: OWNERSHIP_NAMES[usize::from(ownership).min(5)].to_string(),
            confidence: f64::from(confidence) / 255.0,
            sets,
            owners,
            notes,
            structures,
        })
    }

    /// Reads and labels a caller-defined structure, repeated `count` times.
    pub fn decode_struct(
        &self,
        definition: &Value,
        scope: Scope,
        base: u64,
    ) -> Result<Vec<DecodedField>> {
        let fields = definition
            .get("fields")
            .and_then(Value::as_array)
            .context("definition needs a fields array")?;
        let count = definition
            .get("count")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .clamp(1, 4096);
        let mut extent = 0u64;
        for field in fields {
            let offset = field
                .get("offset")
                .and_then(Value::as_u64)
                .context("field needs an offset")?;
            let kind = field
                .get("type")
                .and_then(Value::as_str)
                .context("field needs a type")?;
            extent = extent.max(offset + field_length(kind, field)?);
        }
        let stride = definition
            .get("stride")
            .and_then(Value::as_u64)
            .unwrap_or(extent);
        let total = stride.saturating_mul(count - 1).saturating_add(extent);
        let data = self.read(scope, base, total)?;
        let mut decoded = Vec::with_capacity(fields.len() * count as usize);
        for instance in 0..count {
            let origin = instance * stride;
            for field in fields {
                let offset = field.get("offset").and_then(Value::as_u64).unwrap_or(0);
                let kind = field.get("type").and_then(Value::as_str).unwrap_or("u8");
                let length = field_length(kind, field)?;
                let name = field.get("name").and_then(Value::as_str).unwrap_or("?");
                let start = (origin + offset) as usize;
                decoded.push(DecodedField {
                    name: if count > 1 {
                        format!("[{instance}].{name}")
                    } else {
                        name.to_string()
                    },
                    offset: base + origin + offset,
                    length,
                    kind: kind.to_string(),
                    value: decode_field(kind, &data[start..start + length as usize]),
                });
            }
        }
        Ok(decoded)
    }

    /// Searches for a byte pattern, stopping at the limit or after a time budget.
    /// Pass `next_offset` back as `start` to continue.
    pub fn find_bytes(
        &self,
        pattern: &[u8],
        scope: Scope,
        start: u64,
        end: u64,
        align: u64,
        limit: usize,
    ) -> Result<FindResult> {
        if pattern.is_empty() {
            bail!("pattern is empty");
        }
        let align = align.max(1);
        let started = Instant::now();
        let overlap = (pattern.len() - 1) as u64;
        let mut hits = Vec::new();
        let mut position = start;
        let mut searched = 0u64;
        while position < end {
            if hits.len() >= limit || started.elapsed() >= FIND_TIME_LIMIT {
                return Ok(FindResult {
                    hits,
                    next_offset: position,
                    finished: false,
                    bytes_searched: searched,
                });
            }
            let length = (end - position).min(FIND_CHUNK_BYTES + overlap);
            let data = self.read(scope, position, length)?;
            for (index, window) in data.windows(pattern.len()).enumerate() {
                let offset = position + index as u64;
                if window == pattern && offset.is_multiple_of(align) && hits.len() < limit {
                    hits.push(offset);
                }
            }
            searched += length;
            position += (length - overlap.min(length.saturating_sub(1))).max(1);
        }
        Ok(FindResult {
            hits,
            next_offset: end,
            finished: true,
            bytes_searched: searched,
        })
    }

    /// Reads the bytes a list of clusters would hold, up to `max_bytes`.
    pub fn read_clusters(&self, clusters: &[u32], length: u64, max_bytes: u64) -> Result<Vec<u8>> {
        let limit = length.min(max_bytes);
        let size = self.volume.boot.cluster_size();
        let mut out = Vec::with_capacity(limit as usize);
        for &cluster in clusters {
            if out.len() as u64 >= limit {
                break;
            }
            if !self.volume.valid_cluster(cluster) {
                bail!("cluster {cluster} is outside the volume");
            }
            let data = self.volume.read_cluster(cluster)?;
            out.extend_from_slice(&data[..(limit - out.len() as u64).min(size) as usize]);
        }
        Ok(out)
    }

    /// Tests a possible reassembly: reads it and walks the file format through it.
    pub fn test_stream(&self, name: &str, clusters: &[u32], length: u64) -> Result<StreamTest> {
        let size = self.volume.boot.cluster_size();
        let data = self.read_clusters(clusters, length, u64::MAX)?;
        let report = verify_stream(name, &data);
        let classes = self
            .workspace
            .read_cluster_map(&self.workspace.classes(), self.volume.boot.cluster_count);
        let entropy = self
            .workspace
            .read_cluster_map(&self.workspace.entropy(), self.volume.boot.cluster_count);
        let extents = to_extents(clusters);
        let mut position = 0u64;
        let profile = extents
            .iter()
            .take(64)
            .map(|extent| {
                let index = (extent.first - FIRST_CLUSTER) as usize;
                let row = json!({
                    "first": extent.first,
                    "count": extent.count,
                    "streamOffset": position,
                    "headClass": CLASS_NAMES[usize::from(classes[index])],
                    "headEntropyBits": f64::from(entropy[index]) * 8.0 / 255.0,
                });
                position += u64::from(extent.count) * size;
                row
            })
            .collect();
        let break_locations = report
            .breaks
            .iter()
            .take(32)
            .map(|&offset| {
                let cluster = clusters.get((offset / size) as usize).copied();
                json!({"streamOffset": offset, "cluster": cluster, "offsetInCluster": offset % size})
            })
            .collect();
        Ok(StreamTest {
            extents,
            clusters: clusters.len() as u64,
            needed: length.div_ceil(size),
            length,
            allocated_clusters: clusters
                .iter()
                .filter(|&&cluster| {
                    self.volume.valid_cluster(cluster) && self.volume.is_allocated(cluster)
                })
                .count() as u64,
            report,
            break_locations,
            profile,
        })
    }

    /// Runs a read-only SQL query against the map. The connection cannot write.
    pub fn query_map(&self, sql: &str) -> Result<Value> {
        self.query_map_private(sql, false)
    }

    /// Runs read-only SQL on the map. With `names_hidden`, name columns read as NULL.
    pub fn query_map_private(&self, sql: &str, names_hidden: bool) -> Result<Value> {
        let mut connection = Connection::open_with_flags(
            self.workspace.database(),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        connection.execute_batch("PRAGMA query_only = ON;")?;
        crate::privacy::guard_connection(&mut connection, names_hidden)?;
        connection.busy_timeout(Duration::from_secs(30))?;
        let mut statement = connection.prepare(sql)?;
        let columns: Vec<String> = statement.column_names().map(ToString::to_string).collect();
        let mut rows = statement.query([])?;
        let mut out = Vec::new();
        let mut truncated = false;
        while let Some(row) = rows.next()? {
            if out.len() >= MAX_QUERY_ROWS {
                truncated = true;
                break;
            }
            let mut object = serde_json::Map::new();
            for (index, column) in columns.iter().enumerate() {
                let value = match row.get_ref(index)? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(number) => json!(number),
                    ValueRef::Real(number) => json!(number),
                    ValueRef::Text(text) => json!(String::from_utf8_lossy(text)),
                    ValueRef::Blob(blob) => {
                        json!(blob.iter().map(|b| format!("{b:02x}")).collect::<String>())
                    }
                };
                object.insert(column.clone(), value);
            }
            out.push(Value::Object(object));
        }
        Ok(json!({"columns": columns, "rows": out, "truncated": truncated}))
    }

    /// Recalculates the tree and scores on a separate connection, so other
    /// queries keep working while it runs.
    pub fn resolve(&self) -> Result<MapSummary> {
        MapDb::open(&self.workspace.database())?.resolve(&self.volume, &self.workspace)
    }

    /// Starts or resumes the scan on a background thread.
    pub fn start_scan(self: &Arc<Self>) -> Result<()> {
        if mount_line(&self.path).is_some() {
            bail!("the volume is mounted, so macOS may write to it. Unmount it before scanning.");
        }
        let mut slot = self
            .scan_thread
            .lock()
            .map_err(|_| anyhow!("scan lock poisoned"))?;
        if slot.as_ref().is_some_and(|handle| !handle.is_finished()) {
            bail!("the scan is already running");
        }
        if self.scan_lock_held() {
            bail!("another process is already scanning this volume");
        }
        self.scan_stop.store(false, Ordering::Relaxed);
        *self
            .scan_error
            .lock()
            .map_err(|_| anyhow!("error lock poisoned"))? = None;
        let session = Arc::clone(self);
        *slot = Some(std::thread::spawn(move || {
            let options = ScanOptions::for_volume(&session.volume);
            if let Err(error) = run_scan(
                &session.volume,
                &session.workspace,
                options,
                &session.scan_stop,
                |_| {},
            ) {
                if let Ok(mut message) = session.scan_error.lock() {
                    *message = Some(format!("{error:#}"));
                }
            }
        }));
        Ok(())
    }

    pub fn stop_scan(&self) {
        self.scan_stop.store(true, Ordering::Relaxed);
    }

    /// Reads an entry's stream through any overlay, capped at `max_bytes`.
    pub fn read_entry(&self, id: i64, max_bytes: u64) -> Result<(Row, Vec<u8>)> {
        let database = self.lock_database()?;
        let row = database
            .entry(id)?
            .with_context(|| format!("no entry with id {id}"))?;
        let clusters = database.entry_clusters(&self.volume, &row)?;
        drop(database);
        let limit = row.valid_length.min(row.length);
        let data = self.read_clusters(&clusters, limit, max_bytes)?;
        Ok((row, data))
    }

    pub fn entropy_map(&self) -> Vec<u8> {
        self.workspace
            .read_cluster_map(&self.workspace.entropy(), self.volume.boot.cluster_count)
    }

    pub fn class_map(&self) -> Vec<u8> {
        self.workspace
            .read_cluster_map(&self.workspace.classes(), self.volume.boot.cluster_count)
    }

    pub fn ownership_map(&self) -> Vec<u8> {
        self.workspace
            .read_cluster_map(&self.workspace.ownership(), self.volume.boot.cluster_count)
    }

    pub fn confidence_map(&self) -> Vec<u8> {
        self.workspace
            .read_cluster_map(&self.workspace.confidence(), self.volume.boot.cluster_count)
    }
}

struct ClusterMaps {
    classes: Vec<u8>,
    entropy: Vec<u8>,
    ownership: Vec<u8>,
    confidence: Vec<u8>,
}

/// Bands from the top of a map column down: rarest and most telling first.
const CONTENT_STACK_ORDER: [u8; GROUP_COUNT] = [10, 9, 3, 4, 5, 6, 7, 8, 2, 1, 0];
const OWNERSHIP_STACK_ORDER: [u8; 6] = [3, 2, 1, 4, 5, 0];

/// Splits `rows` among categories in proportion to their counts. Any category
/// present keeps at least one row. Returns one display value per row.
fn stack_column(counts: &[u32], values: &[u8], rows: usize) -> Vec<u8> {
    let total: u64 = counts.iter().map(|&count| u64::from(count)).sum();
    if total == 0 {
        return vec![0; rows];
    }
    let mut shares: Vec<usize> = counts
        .iter()
        .map(|&count| {
            if count == 0 {
                0
            } else {
                ((u64::from(count) * rows as u64 / total) as usize).max(1)
            }
        })
        .collect();
    loop {
        let assigned: usize = shares.iter().sum();
        if assigned == rows {
            break;
        }
        let largest = (0..shares.len())
            .max_by_key(|&index| counts[index])
            .unwrap_or(0);
        if assigned < rows {
            shares[largest] += 1;
        } else if let Some(index) = (0..shares.len())
            .filter(|&index| shares[index] > 1)
            .max_by_key(|&index| shares[index])
        {
            shares[index] -= 1;
        } else {
            // More categories than rows: keep the first ones that fit.
            let mut column = Vec::with_capacity(rows);
            for (index, &share) in shares.iter().enumerate() {
                if share > 0 && column.len() < rows {
                    column.push(values[index]);
                }
            }
            return column;
        }
    }
    let mut column = Vec::with_capacity(rows);
    for (index, &share) in shares.iter().enumerate() {
        column.extend(std::iter::repeat_n(values[index], share));
    }
    column
}

fn dominant(counts: &[u32]) -> u8 {
    let mut best = 0;
    for (index, &count) in counts.iter().enumerate() {
        if count > counts[best] {
            best = index;
        }
    }
    best as u8
}

fn field_length(kind: &str, field: &Value) -> Result<u64> {
    Ok(match kind {
        "u8" | "i8" => 1,
        "u16le" | "u16be" => 2,
        "u32le" | "u32be" | "exfat_time" | "cluster" => 4,
        "u64le" | "u64be" | "filetime" => 8,
        "ascii" | "bytes" => field.get("length").and_then(Value::as_u64).context("this type needs a length in bytes")?,
        "utf16le" => field.get("length").and_then(Value::as_u64).context("utf16le needs a length in characters")? * 2,
        other => bail!(
            "unknown field type {other}. Use u8, i8, u16le, u16be, u32le, u32be, u64le, u64be, exfat_time, filetime, cluster, ascii, utf16le or bytes."
        ),
    })
}

fn decode_field(kind: &str, data: &[u8]) -> Value {
    let le32 = || u32::from_le_bytes(data[..4].try_into().expect("four bytes"));
    let le64 = || u64::from_le_bytes(data[..8].try_into().expect("eight bytes"));
    match kind {
        "u8" => json!(data[0]),
        "i8" => json!(data[0] as i8),
        "u16le" => json!(u16::from_le_bytes([data[0], data[1]])),
        "u16be" => json!(u16::from_be_bytes([data[0], data[1]])),
        "u32le" | "cluster" => json!(le32()),
        "u32be" => json!(u32::from_be_bytes(
            data[..4].try_into().expect("four bytes")
        )),
        "u64le" => json!(le64()),
        "u64be" => json!(u64::from_be_bytes(
            data[..8].try_into().expect("eight bytes")
        )),
        "exfat_time" => json!(decode_timestamp(le32())),
        "filetime" => json!(filetime_to_iso(le64())),
        "ascii" => json!(String::from_utf8_lossy(data)),
        "utf16le" => {
            let units: Vec<u16> = data
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            json!(String::from_utf16_lossy(&units))
        }
        _ => json!(data
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()),
    }
}

/// Converts a Windows FILETIME, as NTFS uses, to a UTC date and time.
fn filetime_to_iso(value: u64) -> Option<String> {
    const TICKS_PER_SECOND: u64 = 10_000_000;
    const SECONDS_TO_UNIX_EPOCH: u64 = 11_644_473_600;
    let seconds = (value / TICKS_PER_SECOND).checked_sub(SECONDS_TO_UNIX_EPOCH)?;
    let days = (seconds / 86_400) as i64;
    let remainder = seconds % 86_400;
    // Civil-from-days conversion for the proleptic Gregorian calendar.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        remainder / 3600,
        remainder / 60 % 60,
        remainder % 60
    ))
}

/// How long the list of macOS's writes is kept before it is worked out again.
const MACOS_WRITES_LIFETIME: std::time::Duration = std::time::Duration::from_secs(120);

/// The clusters macOS wrote on one date, as runs.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MacosWrites {
    pub date: String,
    pub entries: u64,
    pub clusters: u64,
    pub extents: Vec<crate::map::Extent>,
}
