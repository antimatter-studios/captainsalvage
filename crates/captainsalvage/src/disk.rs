use crate::filesystem::probe::{self, Evidence};
use crate::health::{Health, HealthStore};
use crate::raw_scan::{MarkerReader, Verification};
use crate::source::{BlockSource, ReadStats};
use anyhow::{ensure, Context, Result};
use fs_core::BlockRead;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

pub const DISCOVERY_BATCH_BYTES: u64 = 16 * 1024 * 1024;
const SCAN_CHUNK_BYTES: u64 = 64 * 1024;
const IO_ALIGNMENT: u64 = 4096;
const LOOKAHEAD_BYTES: u64 = 1024;
const MAX_EVIDENCE: usize = 256;
static NEXT_DISK_ID: AtomicU64 = AtomicU64::new(1);

struct DiskFile {
    /// The open device, until it goes: then the descriptor is closed, so the app does not keep a
    /// node that no longer exists open (and a disk that comes back is opened again by name).
    device: RwLock<Option<Arc<fs_core::FileDevice>>>,
    size: u64,
    bytes: AtomicU64,
    operations: AtomicU64,
    errors: AtomicU64,
    gone: AtomicBool,
}

impl BlockSource for DiskFile {
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        if self.is_gone() {
            return Err(io::Error::from_raw_os_error(libc::ENXIO));
        }
        let size = self.size;
        let Some(device) = self.device.read().ok().and_then(|device| device.clone()) else {
            return Err(io::Error::from_raw_os_error(libc::ENXIO));
        };
        let end = offset
            .checked_add(buffer.len() as u64)
            .filter(|end| *end <= size)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "read outside disk"))?;
        if buffer.is_empty() {
            return Ok(());
        }
        let aligned_start = offset / IO_ALIGNMENT * IO_ALIGNMENT;
        let aligned_end = end
            .div_ceil(IO_ALIGNMENT)
            .saturating_mul(IO_ALIGNMENT)
            .min(size);
        let mut aligned = vec![0; (aligned_end - aligned_start) as usize];
        let result = device
            .read_at(aligned_start, &mut aligned)
            .map_err(|error| match error {
                fs_core::Error::Io(error) => error,
                error => io::Error::other(error),
            });
        self.operations.fetch_add(1, Ordering::Relaxed);
        match &result {
            Ok(()) => {
                let skip = (offset - aligned_start) as usize;
                buffer.copy_from_slice(&aligned[skip..skip + buffer.len()]);
                self.bytes.fetch_add(buffer.len() as u64, Ordering::Relaxed);
            }
            Err(error) => {
                self.errors.fetch_add(1, Ordering::Relaxed);
                if crate::source::means_device_gone(error) {
                    drop(device);
                    self.release();
                }
            }
        }
        result
    }

    fn read_stats(&self) -> ReadStats {
        ReadStats {
            bytes: self.bytes.load(Ordering::Relaxed),
            operations: self.operations.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }

    fn is_gone(&self) -> bool {
        self.gone.load(Ordering::Relaxed)
    }

    #[cfg(feature = "test-hooks")]
    fn simulate_disconnect(&self) {
        self.release();
    }
}

impl DiskFile {
    /// The device has gone: remember it, and let go of the node that no longer exists, so the app
    /// does not keep it open.
    fn release(&self) {
        self.gone.store(true, Ordering::Relaxed);
        if let Ok(mut open) = self.device.write() {
            *open = None;
        }
    }

    fn open(path: &Path) -> Result<Self> {
        let device = fs_core::FileDevice::open(path)
            .with_context(|| format!("opening {} read-only", path.display()))?;
        let size = device.size_bytes();
        Ok(Self {
            device: RwLock::new(Some(Arc::new(device))),
            size,
            bytes: AtomicU64::new(0),
            operations: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            gone: AtomicBool::new(false),
        })
    }

    /// Whether the descriptor on the device is still held.
    #[cfg(test)]
    fn holds_descriptor(&self) -> bool {
        self.device.read().is_ok_and(|device| device.is_some())
    }
}

pub struct RegionSource {
    source: Arc<dyn BlockSource>,
    start: u64,
    length: u64,
    prefix: Vec<u8>,
}

impl RegionSource {
    pub fn new(
        source: Arc<dyn BlockSource>,
        disk_length: u64,
        start: u64,
        length: u64,
    ) -> Result<Self> {
        ensure!(length > 0, "a region must not be empty");
        ensure!(
            start
                .checked_add(length)
                .is_some_and(|end| end <= disk_length),
            "region is outside the raw source"
        );
        Ok(Self {
            source,
            start,
            length,
            prefix: Vec::new(),
        })
    }
}

impl RegionSource {
    /// Shows `prefix` in place of the region's first bytes, as a saved boot sector would. The
    /// source underneath is not touched.
    pub fn with_prefix(mut self, prefix: Vec<u8>) -> Self {
        self.prefix = prefix;
        self
    }
}

impl BlockSource for RegionSource {
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        let end = offset
            .checked_add(buffer.len() as u64)
            .filter(|end| *end <= self.length)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "read outside interpretation")
            })?;
        if self.is_gone() {
            return Err(io::Error::from_raw_os_error(libc::ENXIO));
        }
        let prefix_end = end.min(self.prefix.len() as u64);
        let copied = prefix_end.saturating_sub(offset) as usize;
        if copied > 0 {
            buffer[..copied].copy_from_slice(&self.prefix[offset as usize..prefix_end as usize]);
        }
        if copied < buffer.len() {
            self.source
                .read_exact_at(self.start + offset + copied as u64, &mut buffer[copied..])?;
        }
        Ok(())
    }

    fn read_stats(&self) -> ReadStats {
        self.source.read_stats()
    }
    fn is_gone(&self) -> bool {
        self.source.is_gone()
    }
    fn bad_ranges(&self) -> Vec<crate::health::HealthRange> {
        let end = self.start + self.length;
        self.source
            .bad_ranges()
            .into_iter()
            .filter(|range| range.end > self.start && range.start < end)
            .map(|range| crate::health::HealthRange {
                start: range.start.max(self.start) - self.start,
                end: range.end.min(end) - self.start,
                ..range
            })
            .collect()
    }
    #[cfg(feature = "test-hooks")]
    fn simulate_disconnect(&self) {
        self.source.simulate_disconnect();
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskRegion {
    pub start: u64,
    pub length: u64,
    pub format: String,
    pub label: String,
    pub boot_offset: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Partition {
    pub start: u64,
    pub length: u64,
    pub label: String,
    pub table: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskOverview {
    pub id: String,
    pub path: PathBuf,
    pub length: u64,
    pub partitions: Vec<Partition>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadFailure {
    pub offset: u64,
    pub length: u64,
    pub error: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryBatch {
    pub findings: Vec<Evidence>,
    pub errors: Vec<ReadFailure>,
    pub next_offset: u64,
    pub evidence_limit_reached: bool,
}

/// For a partition device such as /dev/rdisk7s1, the physical disk /dev/rdisk7 it is part of: that disk's key
/// and where the partition starts in it, when the disk can be opened, its partition table has a partition of
/// this length, and the first sector there is this device's own. None otherwise (a disk image, a disk that
/// cannot be opened), and the device then has a map of its own until a whole disk links it.
fn whole_disk_of(path: &Path, length: u64, own_key: &str) -> Option<(String, u64)> {
    let name = path.file_name()?.to_str()?;
    let (disk, slice) = name.rsplit_once('s')?;
    let digits = disk
        .strip_prefix("rdisk")
        .or_else(|| disk.strip_prefix("disk"))?;
    if digits.is_empty()
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
        || slice.is_empty()
        || !slice.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let whole = path.with_file_name(disk);
    let file = Arc::new(DiskFile::open(&whole).ok()?);
    let whole_length = file.size;
    let whole_key = crate::health::source_key(file.as_ref(), whole_length);
    if whole_key == own_key {
        return None;
    }
    let table = RawDisk::with_source(&whole, file.clone(), whole_length).ok()?;
    let mut warnings = Vec::new();
    for partition in table.partitions(&mut warnings).ok()? {
        if partition.length != length {
            continue;
        }
        let region = RegionSource::new(
            file.clone(),
            whole_length,
            partition.start,
            partition.length,
        )
        .ok()?;
        if crate::health::source_key(&region, length) == own_key {
            return Some((whole_key, partition.start));
        }
    }
    None
}

/// The health of the source a [`RawDisk`] reads, and where this view starts in it.
#[derive(Clone)]
pub struct HealthView {
    pub health: Arc<Health>,
    /// This view's byte 0 is the source's byte `base`.
    pub base: u64,
}

pub struct RawDisk {
    pub id: String,
    pub path: PathBuf,
    pub length: u64,
    pub source: Arc<dyn BlockSource>,
    pub(crate) scan_tokens: Mutex<HashMap<String, String>>,
    /// Set when the source reads through the guard of the health map ([`crate::health`]).
    pub health: Option<HealthView>,
    /// What the drive's hardware says it is, when the system said (see [`crate::hardware`]).
    pub hardware: Option<crate::catalog::Hardware>,
}

impl RawDisk {
    pub fn open(path: &Path) -> Result<Self> {
        let source = Arc::new(DiskFile::open(path)?);
        let length = source.size;
        Self::with_source(path, source, length)
    }

    /// Opens `path` with its health guard: what is known to be bad on this drive is never read,
    /// and what the opening finds out is remembered. Reads that started and never ended in an
    /// earlier run are marked before anything reads the drive.
    pub fn open_guarded(path: &Path, store: Arc<HealthStore>) -> Result<Self> {
        let source = Arc::new(DiskFile::open(path)?);
        let length = source.size;
        let own_key = crate::health::source_key(source.as_ref(), length);
        // What was stored before a GPT disk's GUID was part of its key moves to the new key, once.
        let legacy = crate::health::legacy_source_key(source.as_ref(), length);
        let _ = store.rekey(&legacy, &own_key);
        // A partition (/dev/rdisk7s1) is a stretch of a physical disk: it shares that disk's map, so a bad
        // zone found through one is avoided through the other.
        let link = match store.link_of(&own_key) {
            Ok(Some(link)) => Some(link),
            _ => whole_disk_of(path, length, &own_key),
        };
        Self::guarded(path, source, length, store, own_key, link)
    }

    /// `with_source`, read through the guard of the health map.
    pub fn with_source_guarded(
        path: &Path,
        source: Arc<dyn BlockSource>,
        length: u64,
        store: Arc<HealthStore>,
    ) -> Result<Self> {
        ensure!(length > 0, "raw source is empty");
        let own_key = crate::health::source_key(source.as_ref(), length);
        let link = store.link_of(&own_key).ok().flatten();
        Self::guarded(path, source, length, store, own_key, link)
    }

    fn guarded(
        path: &Path,
        source: Arc<dyn BlockSource>,
        length: u64,
        store: Arc<HealthStore>,
        own_key: String,
        link: Option<(String, u64)>,
    ) -> Result<Self> {
        ensure!(length > 0, "raw source is empty");
        let health = Health::attach_in(source, length, store, own_key, link)?;
        let mut disk = Self::with_source(path, health.guard(), length)?;
        disk.health = Some(HealthView { health, base: 0 });
        disk.hardware = Some(crate::hardware::read(path));
        Ok(disk)
    }

    /// A whole disk tells the store which of its stretches are its partitions, by the first sector of each,
    /// so that a partition opened alone later (or earlier: what was stored for it moves into the disk's)
    /// shares the disk's health. Reads only the first sector of each partition, never one in a bad zone.
    /// Returns how many were linked.
    pub fn register_partition_links(&self) -> usize {
        let Some(view) = self
            .health
            .as_ref()
            .filter(|view| view.base == 0 && view.health.base() == 0)
        else {
            return 0;
        };
        let mut warnings = Vec::new();
        let Ok(partitions) = self.partitions(&mut warnings) else {
            return 0;
        };
        let _journal = view.health.journal_scope();
        let mut linked = 0;
        for partition in partitions {
            if partition.length == 0
                || view
                    .health
                    .blocked(partition.start, partition.start + 512.min(partition.length))
                    .is_some()
            {
                continue;
            }
            let Ok(region) = RegionSource::new(
                self.source.clone(),
                self.length,
                partition.start,
                partition.length,
            ) else {
                continue;
            };
            let child = crate::health::source_key(&region, partition.length);
            if view
                .health
                .store()
                .link(&child, view.health.key(), partition.start, partition.length)
                .is_ok()
            {
                linked += 1;
            }
        }
        if linked > 0 {
            view.health.reload();
        }
        linked
    }

    /// This source as it is after the drive came back at `path`: the same ID and guard, so what
    /// holds the source keeps reading, now from the new node. The new device must be the same
    /// drive, found by its markers; when it is not, the old device is put back and an error says
    /// how far the markers agree.
    pub fn rebound(&self, path: &Path) -> Result<Self> {
        let view = self
            .health
            .as_ref()
            .context("this source was opened without a health guard, so it cannot be rebound")?;
        ensure!(
            view.base == 0,
            "only a whole source is rebound, not one extent of it"
        );
        ensure!(
            self.source.is_gone(),
            "the device still answers; a source is rebound only after its device has gone"
        );
        ensure!(
            view.health.identity().is_some(),
            "the markers of this source were not taken, so the drive cannot be recognised"
        );
        let device = Arc::new(DiskFile::open(path)?);
        ensure!(
            device.size == self.length,
            "{} is {} bytes and the source that went away was {}: it is another drive",
            path.display(),
            device.size,
            self.length
        );
        self.rebound_with(device, path)
    }

    /// [`RawDisk::rebound`], with the device already open.
    pub(crate) fn rebound_with(&self, device: Arc<dyn BlockSource>, path: &Path) -> Result<Self> {
        let view = self
            .health
            .as_ref()
            .context("this source was opened without a health guard, so it cannot be rebound")?;
        ensure!(
            view.base == 0,
            "only a whole source is rebound, not one extent of it"
        );
        let identity = view.health.identity().context(
            "the markers of this source were not taken, so the drive cannot be recognised",
        )?;
        let before = view.health.device_handle();
        view.health.rebind(device);
        let comparison = identity.compare(&mut MarkerReader::new(self));
        if !matches!(
            comparison.state,
            Verification::Exact | Verification::Probable
        ) {
            view.health.rebind(before);
            anyhow::bail!(
                "{} does not look like the drive that went away: {}",
                path.display(),
                comparison.summary
            );
        }
        Ok(Self {
            id: self.id.clone(),
            path: path.into(),
            length: self.length,
            source: self.source.clone(),
            scan_tokens: Mutex::new(
                self.scan_tokens
                    .lock()
                    .map(|tokens| tokens.clone())
                    .unwrap_or_default(),
            ),
            health: self.health.clone(),
            hardware: self.hardware.clone(),
        })
    }

    /// Says what the hardware of this source is, for a test or a caller that knows it.
    pub fn with_hardware(mut self, hardware: crate::catalog::Hardware) -> Self {
        self.hardware = Some(hardware);
        self
    }

    /// Which part of which physical drive this source is: the drive, and where this view starts
    /// in it and how long it is. The drive is named by its model and serial number when the
    /// hardware gave a serial number that tells it from others, and otherwise by its key (the one
    /// its health map is kept under: its length, its first sector and a GPT disk's GUID). The same
    /// drive opened again, whatever its device node is called and however its blocks read now,
    /// gives the same text. None when the source has no health guard, or nothing names the drive:
    /// no serial number, and a first sector that could not be read.
    pub fn physical_identity(&self) -> Option<String> {
        let view = self.health.as_ref()?;
        let place = format!("@{}+{}", view.health.base() + view.base, self.length);
        if let Some(hardware) = self.hardware.as_ref().and_then(|found| found.identity()) {
            return Some(format!("hw:{hardware}{place}"));
        }
        let key = view.health.key();
        (!key.contains("unreadable-head")).then(|| format!("{key}{place}"))
    }

    /// The disk has reset (see [`crate::health::Health::careful_open`]): opening it spares it.
    pub fn careful_open(&self) -> bool {
        self.health
            .as_ref()
            .is_some_and(|view| view.health.careful_open())
    }

    /// Whether `offset..offset + length` of this view is known to read, from earlier reads.
    pub fn known_good(&self, offset: u64, length: u64) -> bool {
        match self.health.as_ref() {
            Some(view) => view.health.known_good(view.base + offset, length),
            None => true,
        }
    }

    /// The bytes of `offset..offset + length` of this view that are marked bad, as the first
    /// such range, in the view's own offsets.
    pub fn known_bad(&self, offset: u64, length: u64) -> Option<(u64, u64)> {
        let view = self.health.as_ref()?;
        let start = view.base + offset;
        let bad = view.health.blocked(start, start + length)?;
        Some((
            bad.start.saturating_sub(view.base),
            bad.end.saturating_sub(view.base),
        ))
    }

    pub fn with_source(path: &Path, source: Arc<dyn BlockSource>, length: u64) -> Result<Self> {
        ensure!(length > 0, "raw source is empty");
        Ok(Self {
            id: NEXT_DISK_ID.fetch_add(1, Ordering::Relaxed).to_string(),
            path: path.into(),
            source,
            length,
            scan_tokens: Mutex::new(HashMap::new()),
            health: None,
            hardware: None,
        })
    }

    /// `start..start + length` of this source, as a source of its own. Its markers are taken from
    /// the extent's own bytes, so a partition opened alone and the same extent seen from its disk
    /// are recognised as one.
    pub fn extent(&self, start: u64, length: u64) -> Result<RawDisk> {
        let region = RegionSource::new(self.source.clone(), self.length, start, length)?;
        let mut view = Self::with_source(&self.path, Arc::new(region), length)?;
        view.hardware = self.hardware.clone();
        view.health = self.health.as_ref().map(|health| HealthView {
            health: health.health.clone(),
            base: health.base + start,
        });
        Ok(view)
    }

    pub fn overview(&self) -> DiskOverview {
        let mut warnings = Vec::new();
        let partitions = self.partitions(&mut warnings).unwrap_or_else(|error| {
            warnings.push(format!(
                "Partition table unreadable: {error:#}. Raw access remains available."
            ));
            Vec::new()
        });
        DiskOverview {
            id: self.id.clone(),
            path: self.path.clone(),
            length: self.length,
            partitions,
            warnings,
        }
    }

    pub fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>> {
        ensure!(
            length <= DISCOVERY_BATCH_BYTES,
            "raw read exceeds 16 MiB limit"
        );
        ensure!(
            offset
                .checked_add(length)
                .is_some_and(|end| end <= self.length),
            "raw read is outside source"
        );
        let mut bytes = vec![0; length as usize];
        self.source.read_exact_at(offset, &mut bytes)?;
        Ok(bytes)
    }

    pub fn discover(&self, offset: u64, length: u64) -> Result<DiscoveryBatch> {
        ensure!(
            length > 0 && length <= DISCOVERY_BATCH_BYTES,
            "discovery batch must be 1 byte to 16 MiB"
        );
        let end = offset
            .checked_add(length)
            .filter(|end| *end <= self.length)
            .context("scan is outside raw source")?;
        let mut result = DiscoveryBatch {
            findings: Vec::new(),
            errors: Vec::new(),
            next_offset: offset,
            evidence_limit_reached: false,
        };
        let mut position = offset;
        while position < end {
            ensure!(
                !self.source.is_gone(),
                "The disk has been disconnected; discovery stopped."
            );
            let next = position.saturating_add(SCAN_CHUNK_BYTES).min(end);
            let read_end = next.saturating_add(LOOKAHEAD_BYTES).min(self.length);
            match self.read(position, read_end - position) {
                Ok(data) => {
                    let mut at = position.div_ceil(32) * 32;
                    while at < next {
                        result
                            .findings
                            .extend(probe::inspect(&data[(at - position) as usize..], at));
                        if result.findings.len() >= MAX_EVIDENCE {
                            result.next_offset = (at + 32).min(end);
                            result.evidence_limit_reached = true;
                            return Ok(result);
                        }
                        at += 32;
                    }
                }
                Err(error) => {
                    ensure!(
                        !self.source.is_gone(),
                        "The disk has been disconnected; discovery stopped."
                    );
                    result.errors.push(ReadFailure {
                        offset: position,
                        length: next - position,
                        error: format!("{error:#}"),
                    });
                }
            }
            position = next;
            result.next_offset = position;
        }
        Ok(result)
    }

    pub fn interpretation(&self, region: &DiskRegion) -> Result<Arc<dyn BlockSource>> {
        let mut source = RegionSource::new(
            self.source.clone(),
            self.length,
            region.start,
            region.length,
        )?;
        ensure!(
            region.label.len() <= 512,
            "interpretation label exceeds 512 bytes"
        );
        let end = region.start + region.length;
        ensure!(
            region.boot_offset >= region.start
                && region
                    .boot_offset
                    .checked_add(512)
                    .is_some_and(|boot_end| boot_end <= end),
            "boot evidence is outside selected region"
        );
        if region.boot_offset != region.start && region.format.eq_ignore_ascii_case("ntfs") {
            // The copy of an NTFS boot sector in the volume's last sector stands in for a
            // main one that was written over. Only the view changes; the source is untouched.
            let backup = self.read(region.boot_offset, 512)?;
            let boot = fs_ntfs::mft_io::parse_boot_sector(&backup)
                .context("the backup NTFS boot sector is not valid")?;
            ensure!(
                region.start.checked_add(boot.volume_bytes()) == Some(region.boot_offset),
                "the NTFS backup boot sector must be one sector past the end of its volume"
            );
            source.prefix = backup;
        } else if region.boot_offset != region.start {
            ensure!(
                region.format.eq_ignore_ascii_case("exfat"),
                "alternate boot regions currently require exFAT or NTFS"
            );
            let boot_bytes = self.read(region.boot_offset, 512)?;
            let boot = fs_exfat::boot::BootSector::parse(&boot_bytes)?;
            let boot_region_length = 12 * boot.bytes_per_sector();
            ensure!(
                region.start.checked_add(boot_region_length) == Some(region.boot_offset),
                "exFAT backup boot must be 12 sectors after the selected start"
            );
            ensure!(
                region
                    .boot_offset
                    .checked_add(boot_region_length)
                    .is_some_and(|boot_end| boot_end <= end),
                "backup boot region exceeds interpretation"
            );
            source.prefix = self.read(region.boot_offset, boot_region_length)?;
        }
        let boot =
            crate::filesystem::read_head(|offset, buffer| source.read_exact_at(offset, buffer))?;
        let detected = crate::filesystem::FilesystemKind::detect_boot_sector(&boot)?;
        ensure!(
            detected.to_string().eq_ignore_ascii_case(&region.format),
            "selected {} interpretation does not match the available {} boot metadata",
            region.format,
            detected
        );
        Ok(Arc::new(source))
    }

    fn partitions(&self, warnings: &mut Vec<String>) -> Result<Vec<Partition>> {
        if self.length < 512 {
            return Ok(Vec::new());
        }
        let mbr = self.read(0, 512)?;
        for sector in [512, 4096] {
            if self.length < sector + 92 {
                continue;
            }
            let header = self.read(sector, 92)?;
            if &header[..8] == b"EFI PART" {
                return self.gpt_partitions(sector, warnings);
            }
        }
        let mut found = Vec::new();
        if mbr[510..512] != [0x55, 0xaa] {
            return Ok(found);
        }
        for entry in mbr[446..510].chunks_exact(16) {
            let kind = entry[4];
            if kind == 0 {
                continue;
            }
            if matches!(kind, 0x05 | 0x0f | 0x85 | 0xee) {
                warnings.push(format!("MBR entry type {kind:02x} requires extended/backup table discovery; it is not treated as empty."));
                continue;
            }
            let start = u64::from(u32::from_le_bytes(entry[8..12].try_into().unwrap())) * 512;
            let length = u64::from(u32::from_le_bytes(entry[12..16].try_into().unwrap())) * 512;
            self.add_partition(
                &mut found,
                warnings,
                start,
                length,
                format!("MBR type {kind:02x}"),
                "MBR",
            );
        }
        Ok(found)
    }

    fn gpt_partitions(&self, sector: u64, warnings: &mut Vec<String>) -> Result<Vec<Partition>> {
        let header = self.read(sector, sector)?;
        let size = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
        ensure!(
            (92..=sector as usize).contains(&size),
            "invalid GPT header size"
        );
        let mut checked = header[..size].to_vec();
        checked[16..20].fill(0);
        ensure!(
            crc32(&checked) == u32::from_le_bytes(header[16..20].try_into().unwrap()),
            "GPT header checksum mismatch"
        );
        let lba = u64::from_le_bytes(header[72..80].try_into().unwrap());
        let count = u32::from_le_bytes(header[80..84].try_into().unwrap());
        let entry_size = u32::from_le_bytes(header[84..88].try_into().unwrap());
        ensure!(
            entry_size >= 128 && entry_size.is_multiple_of(128),
            "invalid GPT entry size"
        );
        let length = u64::from(count)
            .checked_mul(u64::from(entry_size))
            .context("GPT entry size overflow")?;
        let entries = self.read(
            lba.checked_mul(sector)
                .context("GPT table offset overflow")?,
            length,
        )?;
        ensure!(
            crc32(&entries) == u32::from_le_bytes(header[88..92].try_into().unwrap()),
            "GPT table checksum mismatch"
        );
        let mut found = Vec::new();
        for entry in entries.chunks_exact(entry_size as usize) {
            if entry[..16].iter().all(|byte| *byte == 0) {
                continue;
            }
            let first = u64::from_le_bytes(entry[32..40].try_into().unwrap());
            let last = u64::from_le_bytes(entry[40..48].try_into().unwrap());
            let start = first
                .checked_mul(sector)
                .context("GPT partition offset overflow")?;
            let length = last
                .checked_sub(first)
                .and_then(|value| value.checked_add(1))
                .and_then(|value| value.checked_mul(sector));
            let Some(length) = length else {
                warnings.push("Invalid GPT partition extent ignored.".into());
                continue;
            };
            let units: Vec<u16> = entry[56..128]
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes(pair.try_into().unwrap()))
                .take_while(|unit| *unit != 0)
                .collect();
            self.add_partition(
                &mut found,
                warnings,
                start,
                length,
                String::from_utf16_lossy(&units),
                "GPT",
            );
        }
        Ok(found)
    }

    fn add_partition(
        &self,
        found: &mut Vec<Partition>,
        warnings: &mut Vec<String>,
        start: u64,
        length: u64,
        label: String,
        table: &str,
    ) {
        if length == 0
            || start
                .checked_add(length)
                .is_none_or(|end| end > self.length)
        {
            warnings.push(format!(
                "{table} entry has an invalid or out-of-source extent."
            ));
        } else {
            found.push(Partition {
                start,
                length,
                label,
                table: table.into(),
            });
        }
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut checksum = u32::MAX;
    for byte in bytes {
        checksum ^= u32::from(*byte);
        for _ in 0..8 {
            checksum = (checksum >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(checksum & 1));
        }
    }
    !checksum
}

#[cfg(test)]
mod tests;
