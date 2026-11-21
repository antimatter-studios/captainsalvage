//! NTFS in the recovery workspace.
//!
//! Everything about the on-disk format is decoded by the `rust-fs-ntfs` driver: the boot
//! sector, FILE records, their attributes and run lists, and the byte reads. This module
//! only turns what the driver returns into the common entries, geometry and structure
//! copy the rest of the app already uses, and keeps the questions the driver cannot answer
//! (can this stream be read at all, is a cluster numbered the way the map numbers it)
//! in one place.
//!
//! The map numbers an NTFS cluster by its logical cluster number, so cluster 5 starts at
//! byte `5 * cluster size` of the volume. The map's first two clusters hold the boot sector
//! and boot code, which are structures, not data, and are covered by the saved copy.

use super::{
    FilesystemDetails, NtfsEntry, NtfsRun, SalvageEntry, VolumeGeometry, STORAGE_ATTRIBUTE_LIST,
    STORAGE_COMPRESSED, STORAGE_COMPRESSION_UNSUPPORTED, STORAGE_DAMAGED, STORAGE_ENCRYPTED,
    STORAGE_INCOMPLETE, STORAGE_NONE, STORAGE_RESIDENT, STORAGE_RUNS,
};
use crate::changes::{StructureCopy, Unread};
use crate::ranges::RangeList;
use crate::source::BlockSource;
use anyhow::{anyhow, bail, ensure, Context, Result};
use fs_core::BlockRead;
use fs_ntfs::attr_io::attr_flags;
use fs_ntfs::mft_io::{parse_boot_sector, BootParams};
use fs_ntfs::primitives::{
    inspect_file_record, AttributeEvidence, AttributeValue, FileNameEvidence, PrimitiveAPI,
    RecordEvidence,
};
use fs_ntfs::volume::{RawVolume, Volume as NtfsVolume, VolumeAPI};
use serde::Serialize;
use std::ops::Deref;
use std::sync::{Arc, Mutex};

/// The MFT record of the root directory, which the map uses as the root folder's head.
pub const ROOT_RECORD: u64 = 5;
const MFT_RECORD: u64 = 0;
const VOLUME_RECORD: u64 = 3;
const BITMAP_RECORD: u64 = 6;
/// The structures the saved copy keeps: `$MFT`, `$Volume` and the root directory.
const SAVED_RECORDS: [u64; 3] = [MFT_RECORD, VOLUME_RECORD, ROOT_RECORD];
const MAX_BITMAP_BYTES: u64 = 256 * 1024 * 1024;
/// The map numbers clusters with 32 bits, as the other filesystems do.
const FIRST_CLUSTER: u64 = 2;
const DATA: u32 = 0x80;
const FILE_ATTRIBUTE_ENCRYPTED: u32 = 0x4000;

/// A volume's boot sector, as the overview shows it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NtfsBoot {
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u64,
    pub cluster_size: u64,
    pub mft_cluster: u64,
    pub file_record_size: u64,
    pub index_block_size: u32,
    pub total_sectors: u64,
    pub serial: String,
}

impl From<&BootParams> for NtfsBoot {
    fn from(boot: &BootParams) -> Self {
        Self {
            bytes_per_sector: boot.bytes_per_sector,
            sectors_per_cluster: boot.sectors_per_cluster,
            cluster_size: boot.cluster_size,
            mft_cluster: boot.mft_lcn,
            file_record_size: boot.file_record_size,
            index_block_size: boot.index_block_size,
            total_sectors: boot.total_sectors,
            serial: format!("{:016x}", boot.serial_number),
        }
    }
}

/// Lets the driver read through one of the app's read-only sources, borrowed or shared.
pub struct SourceDevice<S> {
    source: S,
    length: u64,
}

impl<S> SourceDevice<S> {
    pub fn new(source: S, length: u64) -> Self {
        Self { source, length }
    }
}

impl<S: Deref + Send + Sync> BlockRead for SourceDevice<S>
where
    S::Target: BlockSource,
{
    fn read_at(&self, offset: u64, output: &mut [u8]) -> fs_core::Result<()> {
        self.source
            .read_exact_at(offset, output)
            .map_err(fs_core::Error::Io)
    }

    fn size_bytes(&self) -> u64 {
        self.length
    }
}

/// The driver reads a stream one cluster at a time. On a real device that is thousands of tiny
/// reads for a file a copy could take in a few, so reads are served from a window of 1 MiB that
/// is read once. A window that cannot be read is not tried again: the exact range asked for is
/// read instead, so one bad sector costs only the reads that touch it.
struct ReadAhead {
    source: Arc<dyn BlockSource>,
    length: u64,
    window: Mutex<Option<(u64, Vec<u8>)>>,
    failed: Mutex<Option<u64>>,
}

const WINDOW_BYTES: u64 = 1024 * 1024;

impl ReadAhead {
    fn new(source: Arc<dyn BlockSource>, length: u64) -> Self {
        Self {
            source,
            length,
            window: Mutex::new(None),
            failed: Mutex::new(None),
        }
    }
}

impl BlockSource for ReadAhead {
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> std::io::Result<()> {
        let end = offset.saturating_add(buffer.len() as u64);
        let start = offset / WINDOW_BYTES * WINDOW_BYTES;
        let window_end = (start + WINDOW_BYTES).min(self.length);
        if buffer.len() as u64 >= WINDOW_BYTES || end > window_end {
            return self.source.read_exact_at(offset, buffer);
        }
        if let Ok(mut window) = self.window.lock() {
            if let Some((at, data)) = window.as_ref() {
                if *at == start {
                    let from = (offset - start) as usize;
                    buffer.copy_from_slice(&data[from..from + buffer.len()]);
                    return Ok(());
                }
            }
            let known_bad = self
                .failed
                .lock()
                .is_ok_and(|failed| *failed == Some(start));
            if !known_bad {
                let mut data = vec![0; (window_end - start) as usize];
                if self.source.read_exact_at(start, &mut data).is_ok() {
                    let from = (offset - start) as usize;
                    buffer.copy_from_slice(&data[from..from + buffer.len()]);
                    *window = Some((start, data));
                    return Ok(());
                }
                if let Ok(mut failed) = self.failed.lock() {
                    *failed = Some(start);
                }
            }
        }
        self.source.read_exact_at(offset, buffer)
    }

    fn read_stats(&self) -> crate::source::ReadStats {
        self.source.read_stats()
    }

    fn is_gone(&self) -> bool {
        self.source.is_gone()
    }
}

/// Where `$MFT` lies, from its own saved record.
pub fn mft_runs(structures: &StructureCopy, geometry: &VolumeGeometry) -> Vec<NtfsRun> {
    let size = geometry.record_size as usize;
    let Some(bytes) = structures.root.get(..size) else {
        return Vec::new();
    };
    let record = inspect_file_record(0, bytes.to_vec(), geometry.bytes_per_sector as u16);
    match data_stream(&record) {
        Some((
            _,
            AttributeEvidence {
                value: AttributeValue::NonResident { runs, .. },
                ..
            },
        )) => runs
            .iter()
            .map(|run| NtfsRun {
                vcn: run.starting_vcn,
                lcn: run.lcn,
                count: run.length,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// What `Inspected` needs from the boot sector and the saved records.
pub struct NtfsInspected {
    pub boot: NtfsBoot,
    pub backup_boot: Option<NtfsBoot>,
    pub geometry: VolumeGeometry,
    pub bitmap: Vec<u8>,
    pub details: FilesystemDetails,
    pub dirty: bool,
    /// False when `$Bitmap` could not be read: `bitmap` is then all zeros, and says nothing about which clusters are free.
    pub allocation_known: bool,
    /// What is wrong with the structures read at open, in words: a damaged `$MFT` record 0, an unreadable `$Bitmap`.
    pub damage: Vec<String>,
}

/// The most clusters an NTFS volume has: a cluster number is 32 bits, and the last two numbers are never given.
/// Windows creates and mounts no volume with more, so this app refuses one at open, before it reads anything else.
pub const MAX_CLUSTERS: u64 = u32::MAX as u64 - 1;

fn geometry_of(boot: &BootParams) -> Result<VolumeGeometry> {
    let sector = u64::from(boot.bytes_per_sector);
    let volume_size = boot
        .total_sectors
        .checked_mul(sector)
        .context("the NTFS boot sector declares a volume larger than 64 bits can count")?;
    let total_clusters = volume_size / boot.cluster_size;
    ensure!(
        total_clusters > FIRST_CLUSTER,
        "the NTFS volume is smaller than three clusters"
    );
    ensure!(
        total_clusters <= MAX_CLUSTERS,
        "the NTFS volume has {total_clusters} clusters, more than the {MAX_CLUSTERS} (2^32 - 2) there can be. Windows itself does not create or mount such a volume (a bigger one uses bigger clusters), so this app does not open it"
    );
    ensure!(
        boot.file_record_size >= 48 && boot.file_record_size.is_multiple_of(sector),
        "the NTFS file record size does not fit its sector size"
    );
    Ok(VolumeGeometry {
        volume_serial: (boot.serial_number ^ (boot.serial_number >> 32)) as u32,
        root_cluster: ROOT_RECORD as u32,
        partition_offset: 0,
        data_offset: FIRST_CLUSTER * boot.cluster_size,
        fat_offset: 0,
        fat_bytes: 0,
        bytes_per_sector: sector,
        cluster_size: boot.cluster_size,
        cluster_count: (total_clusters - FIRST_CLUSTER) as u32,
        volume_size,
        record_size: boot.file_record_size,
    })
}

/// The first bytes of the volume say it is NTFS and valid. Anything else is refused by name.
fn parse_main(main: &[u8]) -> Result<BootParams> {
    parse_boot_sector(main).context("reading the NTFS boot sector")
}

fn record_of<D: BlockRead>(device: &NtfsVolume<D>, number: u64) -> Result<RecordEvidence> {
    device
        .inspect_mft_record(number)
        .with_context(|| format!("reading MFT record {number}"))
}

/// The largest compression unit this reads: NTFS uses 16 clusters, which is 64 KiB at 4 KiB clusters.
const MAX_COMPRESSION_UNIT_BYTES: u64 = 4 * 1024 * 1024;
/// Where a non-resident attribute header says how many clusters make a compression unit, as a power of two.
const COMPRESSION_UNIT_FIELD: usize = 0x22;
const MAX_COMPRESSION_UNIT_EXPONENT: u16 = 8;

/// How many clusters make one compression unit of an LZNT1-compressed non-resident attribute, or
/// none when it is not that: another compression method, a resident value, or no unit given.
fn lznt1_unit_clusters(record: &RecordEvidence, attribute: &AttributeEvidence) -> Option<u64> {
    if attribute.flags & attr_flags::COMPRESSION_MASK != attr_flags::COMPRESSED
        || !matches!(attribute.value, AttributeValue::NonResident { .. })
    {
        return None;
    }
    let header = record.fixed.as_ref()?;
    let field = header.get(
        attribute.offset + COMPRESSION_UNIT_FIELD..attribute.offset + COMPRESSION_UNIT_FIELD + 2,
    )?;
    let exponent = u16::from_le_bytes([field[0], field[1]]);
    (1..=MAX_COMPRESSION_UNIT_EXPONENT)
        .contains(&exponent)
        .then(|| 1u64 << exponent)
}

fn data_stream(record: &RecordEvidence) -> Option<(usize, &AttributeEvidence)> {
    record
        .attributes
        .iter()
        .enumerate()
        .find(|(_, attribute)| attribute.type_code == DATA && attribute.name.is_empty())
}

/// Reads `$MFT`, `$Volume`, the root directory and `$Bitmap` for the saved copy.
pub fn read_structures(source: &dyn BlockSource) -> Result<StructureCopy> {
    let mut main = vec![0; 512];
    source
        .read_exact_at(0, &mut main)
        .context("reading the NTFS boot sector")?;
    let boot = parse_main(&main)?;
    let geometry = geometry_of(&boot)?;
    let device = SourceDevice::new(source, geometry.volume_size);
    let volume = NtfsVolume::open_for_salvage(device)
        .map_err(|error| anyhow!("opening the NTFS volume for salvage: {error}"))?;

    let mut backup = vec![0; 512];
    if source
        .read_exact_at(geometry.volume_size, &mut backup)
        .is_err()
    {
        backup.clear();
    }

    let mut root = Vec::new();
    for number in SAVED_RECORDS {
        let record = record_of(&volume, number)?;
        ensure!(
            record.raw.len() as u64 == geometry.record_size,
            "MFT record {number} has an unexpected length"
        );
        root.extend_from_slice(&record.raw);
    }

    // A volume whose `$Bitmap` cannot be read is still opened: the files are found and read through `$MFT`,
    // and only the knowledge of which clusters are free is lost. The copy says so, and holds no bitmap.
    let (bitmap, unread) = match read_bitmap(&volume, &geometry) {
        Ok(bitmap) => (bitmap, Vec::new()),
        Err(error) => (
            Vec::new(),
            vec![Unread {
                region: "bitmap".to_string(),
                why: format!("{error:#}"),
            }],
        ),
    };

    Ok(StructureCopy {
        main,
        backup,
        fat: Vec::new(),
        bitmap,
        root,
        unread,
    })
}

/// `$Bitmap`, one bit for each cluster of the volume, from the volume's own record.
fn read_bitmap<D: BlockRead>(volume: &NtfsVolume<D>, geometry: &VolumeGeometry) -> Result<Vec<u8>> {
    let bitmap_record = record_of(volume, BITMAP_RECORD)?;
    let (index, attribute) = data_stream(&bitmap_record).context("$Bitmap has no data stream")?;
    let wanted = u64::from(geometry.cluster_count + FIRST_CLUSTER as u32).div_ceil(8);
    let size = match &attribute.value {
        AttributeValue::Resident(data) => data.len() as u64,
        AttributeValue::NonResident { data_size, .. } => *data_size,
    };
    ensure!(
        size >= wanted,
        "$Bitmap holds {size} bytes, fewer than the {wanted} this volume needs"
    );
    ensure!(
        wanted <= MAX_BITMAP_BYTES,
        "the cluster bitmap exceeds the 256 MiB inspection limit"
    );
    let mut bitmap = vec![0; wanted as usize];
    let count = volume
        .read_salvage_stream_at(&bitmap_record, index, geometry.cluster_size, 0, &mut bitmap)
        .map_err(|error| anyhow!("reading $Bitmap: {error}"))?;
    ensure!(count == bitmap.len(), "$Bitmap ended early");
    Ok(bitmap)
}

/// Reads the saved structures back into the facts the rest of the app uses.
pub fn inspect(structures: &StructureCopy) -> Result<NtfsInspected> {
    let boot = parse_main(&structures.main)?;
    let geometry = geometry_of(&boot)?;
    let record_size = geometry.record_size as usize;
    ensure!(
        structures.main.len() == 512
            && structures.fat.is_empty()
            && structures.root.len() == record_size * SAVED_RECORDS.len(),
        "saved NTFS structures do not match the boot sector"
    );
    let total_clusters = u64::from(geometry.cluster_count) + FIRST_CLUSTER;
    let allocation_known = !structures.is_unread("bitmap");
    ensure!(
        !allocation_known || structures.bitmap.len() as u64 >= total_clusters.div_ceil(8),
        "saved $Bitmap is shorter than the volume needs"
    );
    let backup_boot = match structures.backup.len() {
        0 => None,
        512 => parse_boot_sector(&structures.backup)
            .ok()
            .as_ref()
            .map(NtfsBoot::from),
        _ => bail!("saved NTFS backup boot sector has an unexpected length"),
    };
    let volume_record = inspect_file_record(
        0,
        structures.root[record_size..2 * record_size].to_vec(),
        boot.bytes_per_sector,
    );
    let information = volume_record.volume_information();
    let details = FilesystemDetails::Ntfs {
        bytes_per_sector: boot.bytes_per_sector,
        sectors_per_cluster: boot.sectors_per_cluster,
        cluster_size: boot.cluster_size,
        mft_cluster: boot.mft_lcn,
        file_record_size: boot.file_record_size,
        index_block_size: boot.index_block_size,
        total_sectors: boot.total_sectors,
        serial: format!("{:016x}", boot.serial_number),
        version: information.map(|value| format!("{}.{}", value.major, value.minor)),
        label: volume_record
            .volume_name()
            .filter(|label| !label.is_empty()),
    };
    // The map's bit 0 is cluster 2, so the first two bits of the volume's bitmap are dropped.
    let mut bitmap = vec![0u8; (u64::from(geometry.cluster_count)).div_ceil(8) as usize];
    for index in 0..u64::from(geometry.cluster_count) {
        if !allocation_known {
            break;
        }
        let source = index + FIRST_CLUSTER;
        if structures.bitmap[(source / 8) as usize] >> (source % 8) & 1 == 1 {
            bitmap[(index / 8) as usize] |= 1 << (index % 8);
        }
    }
    let mut damage = Vec::new();
    for (position, number) in SAVED_RECORDS.iter().enumerate() {
        let raw = structures.root[position * record_size..(position + 1) * record_size].to_vec();
        if inspect_file_record(*number, raw, boot.bytes_per_sector)
            .fixed
            .is_none()
        {
            damage.push(match *number {
                MFT_RECORD => "$MFT record 0 failed its update-sequence check, so where the rest of $MFT lies is not known from it. Files in later parts of $MFT may be missing or listed as damaged, and $MFTMirr is not consulted.".to_string(),
                VOLUME_RECORD => "$Volume (MFT record 3) failed its update-sequence check, so the volume's label and flags could not be read.".to_string(),
                _ => "The root directory's record (MFT record 5) failed its update-sequence check.".to_string(),
            });
        }
    }
    for unread in structures
        .unread
        .iter()
        .filter(|unread| unread.region == "bitmap")
    {
        damage.push(format!(
            "$Bitmap could not be read ({}), so which clusters are free is unknown. Files are still listed and read through $MFT, and no cluster is counted as free.",
            unread.why
        ));
    }
    Ok(NtfsInspected {
        boot: NtfsBoot::from(&boot),
        backup_boot,
        geometry,
        bitmap,
        details,
        dirty: information.is_some_and(|value| value.is_dirty()),
        allocation_known,
        damage,
    })
}

/// The copy used to compare two moments: the same, with the bitmap in the map's numbering.
pub fn comparison_copy(copy: &StructureCopy) -> Result<StructureCopy> {
    let inspected = inspect(copy)?;
    Ok(StructureCopy {
        // A bitmap that was not read stays empty: zeros would say every cluster is free.
        bitmap: if inspected.allocation_known {
            inspected.bitmap
        } else {
            Vec::new()
        },
        ..copy.clone()
    })
}

fn nt_time(value: u64) -> Option<String> {
    const TICKS_PER_SECOND: u64 = 10_000_000;
    const SECONDS_TO_UNIX_EPOCH: u64 = 11_644_473_600;
    if value == 0 {
        return None;
    }
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
    // NTFS keeps UTC, so the offset is written out and nothing is guessed from this Mac's zone.
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}+00:00",
        remainder / 3600,
        remainder / 60 % 60,
        remainder % 60
    ))
}

/// The name an entry is shown under: a long name over a short one, and a DOS name only
/// when nothing else survives.
fn best_name(names: &[FileNameEvidence]) -> Option<&FileNameEvidence> {
    let rank = |name: &FileNameEvidence| match name.namespace {
        1 | 3 => 0,
        0 => 1,
        _ => 2,
    };
    names
        .iter()
        .filter(|name| !name.name.is_empty())
        .min_by_key(|name| rank(name))
}

/// The entries for a file's further hard links: one for each name other than the one the file is
/// shown under, in the folder that name was linked in, with the file's size, times and data. DOS
/// names are the short form of another name in the same folder, not links of their own. They are
/// in order of (parent record, name), so a rescan gives each the same slot and no entry twice.
fn further_name_entries(record: &RecordEvidence, primary: &SalvageEntry) -> Vec<SalvageEntry> {
    if primary.is_dir {
        return Vec::new();
    }
    let names = record.file_names();
    let key = |name: &FileNameEvidence| (name.parent_record, name.name.clone());
    let shown = best_name(&names).map(key);
    let mut others: Vec<&FileNameEvidence> = names
        .iter()
        .filter(|name| !name.name.is_empty() && name.namespace != 2)
        .filter(|name| Some(key(name)) != shown)
        .collect();
    others.sort_by_key(|name| key(name));
    others.dedup_by_key(|name| key(name));
    others
        .into_iter()
        .take(MAX_FURTHER_NAMES)
        .zip(1u32..)
        .map(|(name, slot)| {
            let mut entry = primary.clone();
            entry.name = name.name.clone();
            entry.name_slot = slot;
            if let Some(ntfs) = entry.ntfs.as_mut() {
                ntfs.parent_record = Some(name.parent_record);
                ntfs.parent_sequence = Some(name.parent_sequence);
            }
            entry
        })
        .collect()
}

/// NTFS gives one file at most 1024 names; the first is the one it is shown under.
const MAX_FURTHER_NAMES: usize = 1023;

struct Data {
    storage: &'static str,
    length: u64,
    valid_length: u64,
    runs: Vec<NtfsRun>,
}

fn describe_data(record: &RecordEvidence, fallback_length: u64, encrypted: bool) -> Data {
    let Some((_, attribute)) = data_stream(record) else {
        let storage = if record.has_attribute_list() {
            STORAGE_ATTRIBUTE_LIST
        } else {
            STORAGE_NONE
        };
        return Data {
            storage,
            length: if storage == STORAGE_NONE {
                0
            } else {
                fallback_length
            },
            valid_length: if storage == STORAGE_NONE {
                0
            } else {
                fallback_length
            },
            runs: Vec::new(),
        };
    };
    let transformed = |fallback: &'static str, compressible: bool| {
        if encrypted || attribute.flags & attr_flags::ENCRYPTED != 0 {
            STORAGE_ENCRYPTED
        } else if compressible && attribute.flags & attr_flags::COMPRESSION_MASK != 0 {
            if lznt1_unit_clusters(record, attribute).is_some() {
                STORAGE_COMPRESSED
            } else {
                STORAGE_COMPRESSION_UNSUPPORTED
            }
        } else {
            fallback
        }
    };
    match &attribute.value {
        // NTFS compresses clusters, never the bytes held in the FILE record. Windows still sets the
        // compression flag on that attribute for a small file in a compressed folder, and its
        // bytes are the file as written.
        AttributeValue::Resident(data) => Data {
            storage: transformed(STORAGE_RESIDENT, false),
            length: data.len() as u64,
            valid_length: data.len() as u64,
            runs: Vec::new(),
        },
        AttributeValue::NonResident {
            runs,
            first_vcn,
            data_size,
            initialized_size,
        } => {
            let mut storage = transformed(STORAGE_RUNS, true);
            // A run list that continues in another record is partial, compressed or not.
            if matches!(storage, STORAGE_RUNS | STORAGE_COMPRESSED)
                && (record.has_attribute_list() || *first_vcn != 0)
            {
                storage = if record.has_attribute_list() {
                    STORAGE_ATTRIBUTE_LIST
                } else {
                    STORAGE_INCOMPLETE
                };
            }
            Data {
                storage,
                length: *data_size,
                valid_length: (*initialized_size).min(*data_size),
                runs: runs
                    .iter()
                    .map(|run| NtfsRun {
                        vcn: run.starting_vcn,
                        lcn: run.lcn,
                        count: run.length,
                    })
                    .collect(),
            }
        }
    }
}

/// The entry one FILE record describes, or none when it is not a base record that
/// survived its fixups well enough to say anything.
pub fn entry_from_record(
    record: &RecordEvidence,
    offset: usize,
    geometry: &VolumeGeometry,
) -> Option<SalvageEntry> {
    if record.fixed.is_none() || record.base_reference != 0 {
        return None;
    }
    let names = record.file_names();
    let chosen = best_name(&names);
    let information = record.standard_information();
    let encrypted = information
        .as_ref()
        .is_some_and(|value| value.file_attributes & FILE_ATTRIBUTE_ENCRYPTED != 0);
    let is_dir = record.is_directory;
    let data = describe_data(
        record,
        chosen.map(|name| name.real_size).unwrap_or(0),
        encrypted,
    );
    let first_cluster = data
        .runs
        .iter()
        .find_map(|run| run.lcn)
        .and_then(|lcn| u32::try_from(lcn).ok())
        .unwrap_or(0);
    let contiguous = data.storage == STORAGE_RUNS
        && data.runs.len() == 1
        && data.runs[0].lcn.is_some()
        && data.runs[0].vcn == 0;
    let (modified, created, accessed) = match (&information, chosen) {
        (Some(value), _) => (value.modified_nt, value.created_nt, value.accessed_nt),
        (None, Some(name)) => (name.modified_nt, name.created_nt, name.accessed_nt),
        (None, None) => (0, 0, 0),
    };
    let length = if is_dir { 0 } else { data.length };
    let valid_length = if is_dir { 0 } else { data.valid_length };
    let ntfs = NtfsEntry {
        record_number: record.record_number,
        sequence: record.sequence,
        parent_record: chosen.map(|name| name.parent_record),
        parent_sequence: chosen.map(|name| name.parent_sequence),
        storage: if is_dir {
            STORAGE_NONE.to_string()
        } else {
            data.storage.to_string()
        },
        runs: if is_dir { Vec::new() } else { data.runs },
    };
    let ranges = RangeList::for_ntfs(&ntfs, geometry, length, valid_length);
    // A record that fixes up and has a name is intact. Its number may be unknown, which only
    // means it cannot be named as a folder's parent until the scan works it out from `$MFT`.
    let intact = record.issues.is_empty() && chosen.is_some();
    Some(SalvageEntry {
        offset,
        name: match chosen {
            Some(name) => name.name.clone(),
            None => match record.record_number {
                Some(number) => format!("(no name, record {number})"),
                None => "(no name)".into(),
            },
        },
        in_use: record.in_use,
        checksum_ok: intact,
        attributes: information
            .as_ref()
            .map(|value| value.file_attributes as u16)
            .unwrap_or(0),
        is_dir,
        first_cluster,
        length,
        valid_length,
        no_fat_chain: contiguous,
        created: nt_time(created),
        modified: nt_time(modified),
        accessed: nt_time(accessed),
        slots: 1,
        ntfs: Some(ntfs),
        ranges,
        name_slot: 0,
    })
}

/// What stands for a FILE record whose update-sequence check failed: nothing in it is trusted,
/// so it has no name, no size and no data, only a place.
fn damaged_placeholder(offset: usize) -> SalvageEntry {
    SalvageEntry {
        offset,
        name: "(damaged FILE record)".into(),
        in_use: false,
        checksum_ok: false,
        attributes: 0,
        is_dir: false,
        first_cluster: 0,
        length: 0,
        valid_length: 0,
        no_fat_chain: false,
        created: None,
        modified: None,
        accessed: None,
        slots: 1,
        ntfs: Some(NtfsEntry {
            record_number: None,
            sequence: 0,
            parent_record: None,
            parent_sequence: None,
            storage: STORAGE_DAMAGED.to_string(),
            runs: Vec::new(),
        }),
        ranges: RangeList::default(),
        name_slot: 0,
    }
}

/// Finds every FILE record that starts in the first `start_limit` bytes of `data`, at any
/// sector boundary: a metadata cluster can be the last of a fragment of the MFT, and an old
/// copy of the MFT in free space is as readable as the live one.
pub fn find_entries(
    geometry: &VolumeGeometry,
    data: &[u8],
    start_limit: usize,
) -> Vec<SalvageEntry> {
    let record_size = geometry.record_size as usize;
    let sector = geometry.bytes_per_sector as usize;
    if record_size < 48 || sector == 0 || !record_size.is_multiple_of(sector) {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut offset = 0;
    while offset < start_limit.min(data.len()) && offset + record_size <= data.len() {
        if &data[offset..offset + 4] == b"FILE" {
            let record = inspect_file_record(
                offset as u64,
                data[offset..offset + record_size].to_vec(),
                sector as u16,
            );
            if let Some(entry) = entry_from_record(&record, offset, geometry) {
                let further = further_name_entries(&record, &entry);
                found.push(entry);
                found.extend(further);
                offset += record_size;
                continue;
            }
            // Something shaped like a record that failed its update-sequence check is kept as
            // evidence that a record was here, and is never read as if it were intact.
            if record.fixed.is_none() {
                found.push(damaged_placeholder(offset));
                offset += record_size;
                continue;
            }
        }
        offset += sector;
    }
    found
}

/// What one entry's bytes need to be read again through the driver.
#[derive(Clone, Debug)]
pub struct StreamReference {
    /// The record's offset from the start of the volume.
    pub record_offset: u64,
    pub record_number: Option<u64>,
    pub sequence: u16,
    pub storage: String,
    pub length: u64,
    pub valid_length: u64,
}

/// Reads entries' bytes by asking the driver about the FILE record again, so a record that
/// changed since the scan is noticed instead of read as if it were the one that was found.
pub struct Access {
    /// Where `$MFT` is, from its own record in the saved structures. A record's number is its
    /// place in this list when its header does not carry one.
    mft: Vec<NtfsRun>,
    volume: RawVolume<SourceDevice<Arc<dyn BlockSource>>>,
    geometry: VolumeGeometry,
    last: Mutex<Option<(u64, Arc<RecordEvidence>)>>,
}

impl Access {
    pub fn new(source: Arc<dyn BlockSource>, geometry: &VolumeGeometry, mft: Vec<NtfsRun>) -> Self {
        Self {
            mft,
            volume: RawVolume::open(SourceDevice::new(
                Arc::new(ReadAhead::new(source, geometry.volume_size)) as Arc<dyn BlockSource>,
                geometry.volume_size,
            )),
            geometry: *geometry,
            last: Mutex::new(None),
        }
    }

    /// The number of the record at `offset` bytes into `cluster`, by where `$MFT` places it. A
    /// record that is not in the current `$MFT`, such as an old copy in free space, has none.
    pub fn record_number_at(&self, cluster: u32, offset: usize) -> Option<u64> {
        let size = self.geometry.cluster_size;
        let at = u64::from(cluster) * size + offset as u64;
        self.mft.iter().find_map(|run| {
            let start = run.lcn? * size;
            let end = start + run.count * size;
            if !(start..end).contains(&at) {
                return None;
            }
            let within = run.vcn * size + (at - start);
            within
                .is_multiple_of(self.geometry.record_size)
                .then(|| within / self.geometry.record_size)
        })
    }

    fn record(&self, offset: u64) -> Result<Arc<RecordEvidence>> {
        let mut last = self
            .last
            .lock()
            .map_err(|_| anyhow!("record cache poisoned"))?;
        if let Some((at, record)) = last.as_ref() {
            if *at == offset {
                return Ok(record.clone());
            }
        }
        let record = Arc::new(
            self.volume
                .read_file_record_at(
                    offset,
                    self.geometry.record_size as usize,
                    self.geometry.bytes_per_sector as u16,
                )
                .map_err(|error| anyhow!("reading the FILE record: {error}"))?,
        );
        *last = Some((offset, record.clone()));
        Ok(record)
    }

    /// Whether the entry's bytes can be read at all, and the record they are in is still the one the
    /// scan found. A stream NTFS compresses, encrypts or keeps in other records is refused here
    /// by name, and so is a record that changed or cannot be read: those are the whole entry's, not
    /// a stretch of it, and nothing is made up for them.
    pub fn verify(&self, entry: &StreamReference) -> Result<()> {
        self.checked_record(entry).map(|_| ())
    }

    fn checked_record(&self, entry: &StreamReference) -> Result<Arc<RecordEvidence>> {
        ensure!(
            matches!(
                entry.storage.as_str(),
                STORAGE_RESIDENT | STORAGE_RUNS | STORAGE_COMPRESSED
            ),
            "this NTFS stream cannot be read: it is {}",
            entry.storage
        );
        let record = self.record(entry.record_offset)?;
        ensure!(
            record.fixed.is_some()
                && record.base_reference == 0
                && record.sequence == entry.sequence
                // A header with no number cannot disagree with the one the scan worked out.
                && (record.record_number.is_none() || record.record_number == entry.record_number),
            "the FILE record no longer matches the one that was found"
        );
        Ok(record)
    }

    /// Reads from an entry's unnamed data stream. Bytes beyond the initialised length are
    /// returned as the zeros NTFS defines them to be; any other failure is an error, never zeros.
    pub fn read(&self, entry: &StreamReference, offset: u64, output: &mut [u8]) -> Result<usize> {
        let record = self.checked_record(entry)?;
        let (index, attribute) =
            data_stream(&record).context("the FILE record has no data stream")?;
        let available = entry.length.saturating_sub(offset).min(output.len() as u64) as usize;
        if available == 0 {
            return Ok(0);
        }
        // Bytes held in the record are the file as written, whatever flag the attribute carries
        // (see describe_data), and the driver refuses a flagged stream before looking at it.
        if let (STORAGE_RESIDENT, AttributeValue::Resident(data)) =
            (entry.storage.as_str(), &attribute.value)
        {
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(data.len());
            let count = available.min(data.len() - start);
            output[..count].copy_from_slice(&data[start..start + count]);
            return Ok(count);
        }
        if entry.storage == STORAGE_COMPRESSED {
            return self.read_compressed(&record, attribute, offset, &mut output[..available]);
        }
        let count = self
            .volume
            .read_salvage_stream_at(
                &record,
                index,
                self.geometry.cluster_size,
                offset,
                &mut output[..available],
            )
            .map_err(|error| anyhow!("{error}"))?;
        Ok(count)
    }

    /// Where `offset..offset + length` of an entry's data stream is on the source, as `(start, end)`
    /// ranges in volume bytes, for a stream held in plain runs. None for any other, which has no
    /// place to be bad.
    pub fn source_ranges(
        &self,
        entry: &StreamReference,
        offset: u64,
        length: u64,
    ) -> Option<Vec<(u64, u64)>> {
        if entry.storage != STORAGE_RUNS {
            return None;
        }
        let record = self.checked_record(entry).ok()?;
        let (_, attribute) = data_stream(&record)?;
        let AttributeValue::NonResident { runs, .. } = &attribute.value else {
            return None;
        };
        let cluster = self.geometry.cluster_size;
        let end = offset.saturating_add(length);
        let mut places = Vec::new();
        let mut at = offset;
        while at < end {
            let within = at % cluster;
            let piece = (cluster - within).min(end - at);
            if let Some(lcn) = fs_ntfs::data_runs::vcn_to_lcn(runs, at / cluster) {
                let start = lcn * cluster + within;
                places.push((start, start + piece));
            }
            at += piece;
        }
        Some(places)
    }

    /// Reads from an LZNT1-compressed stream. NTFS stores such a stream in compression units of a
    /// power-of-two number of clusters (16, nearly always). A unit is held one of three ways: all of
    /// its clusters allocated (the bytes are stored as they are, because compressing did not make
    /// them smaller), none allocated (a hole: zeros), or its first clusters allocated and the rest a
    /// hole (the allocated clusters hold the unit's LZNT1 stream). The decoding of that stream is the
    /// driver's; this only maps a unit to its clusters. A unit that does not decode is an error for
    /// the read that needs it, never zeros.
    fn read_compressed(
        &self,
        record: &RecordEvidence,
        attribute: &AttributeEvidence,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize> {
        let AttributeValue::NonResident {
            runs,
            first_vcn,
            initialized_size,
            ..
        } = &attribute.value
        else {
            bail!("a compressed stream held in its record is not decoded");
        };
        ensure!(
            *first_vcn == 0,
            "the compressed stream continues in another record"
        );
        let unit_clusters = lznt1_unit_clusters(record, attribute)
            .context("this NTFS compression is not LZNT1 with a usable compression unit")?;
        let cluster = self.geometry.cluster_size;
        let unit_bytes = unit_clusters * cluster;
        ensure!(
            unit_bytes <= MAX_COMPRESSION_UNIT_BYTES,
            "the compression unit of {unit_bytes} bytes is larger than the {MAX_COMPRESSION_UNIT_BYTES} this reads"
        );
        let mut next = 0u64;
        for run in runs {
            ensure!(
                run.starting_vcn == next && run.length != 0,
                "the compressed stream's run list is discontinuous"
            );
            next = next
                .checked_add(run.length)
                .context("the compressed stream's run list overflows")?;
            if let Some(lcn) = run.lcn {
                ensure!(
                    lcn.checked_add(run.length)
                        .and_then(|end| end.checked_mul(cluster))
                        .is_some_and(|end| end <= self.geometry.volume_size),
                    "a run of the compressed stream lies outside the volume"
                );
            }
        }
        let lcn_of = |vcn: u64| fs_ntfs::data_runs::vcn_to_lcn(runs, vcn);
        let mut done = 0usize;
        while done < output.len() {
            let position = offset + done as u64;
            if position >= *initialized_size {
                // Past the initialised length NTFS defines the bytes as zeros.
                output[done..].fill(0);
                done = output.len();
                break;
            }
            let unit = position / unit_bytes;
            let first = unit * unit_clusters;
            // The unit's clusters: those allocated first, then the hole that completes it.
            let places: Vec<Option<u64>> = (first..first + unit_clusters).map(lcn_of).collect();
            let stored = places.iter().take_while(|place| place.is_some()).count();
            ensure!(
                places[stored..].iter().all(Option::is_none),
                "compression unit {unit} has an allocated cluster after a hole"
            );
            let bytes = if stored == 0 {
                vec![0u8; unit_bytes as usize]
            } else {
                let mut raw = vec![0u8; stored * cluster as usize];
                for (index, place) in places[..stored].iter().enumerate() {
                    let at = place.unwrap_or(0) * cluster;
                    self.volume
                        .read_volume_bytes(
                            at,
                            &mut raw[index * cluster as usize..][..cluster as usize],
                        )
                        .map_err(|error| {
                            anyhow!("reading a cluster of the compressed stream: {error}")
                        })?;
                }
                if stored as u64 == unit_clusters {
                    raw
                } else {
                    let mut decoded =
                        fs_ntfs::compression::decompress_unit(&raw, unit_bytes as usize).map_err(
                            |error| anyhow!("decoding compression unit {unit}: {error}"),
                        )?;
                    decoded.resize(unit_bytes as usize, 0);
                    decoded
                }
            };
            let within = (position % unit_bytes) as usize;
            let count = (bytes.len() - within).min(output.len() - done);
            output[done..done + count].copy_from_slice(&bytes[within..within + count]);
            done += count;
        }
        Ok(done)
    }

    /// Everything the FILE record says, for the typed NTFS panel: every name and where it was
    /// linked, every stream and how it is held, and what was wrong with the record.
    pub fn details(&self, record_offset: u64) -> Result<RecordDetails> {
        let record = self.record(record_offset)?;
        let decode = |value: u64| nt_time(value);
        let streams = record
            .attributes
            .iter()
            .filter(|attribute| attribute.type_code == DATA)
            .map(|attribute| {
                let (resident, length, initialized, runs) = match &attribute.value {
                    AttributeValue::Resident(data) => {
                        (true, data.len() as u64, data.len() as u64, Vec::new())
                    }
                    AttributeValue::NonResident {
                        runs,
                        data_size,
                        initialized_size,
                        ..
                    } => (
                        false,
                        *data_size,
                        *initialized_size,
                        runs.iter()
                            .map(|run| NtfsRun {
                                vcn: run.starting_vcn,
                                lcn: run.lcn,
                                count: run.length,
                            })
                            .collect(),
                    ),
                };
                StreamDetails {
                    name: attribute.name.clone(),
                    resident,
                    length,
                    initialized_length: initialized,
                    compressed: attribute.flags & attr_flags::COMPRESSION_MASK != 0,
                    encrypted: attribute.flags & attr_flags::ENCRYPTED != 0,
                    sparse: attribute.flags & attr_flags::SPARSE != 0,
                    runs,
                }
            })
            .collect();
        Ok(RecordDetails {
            record_offset,
            record_number: record.record_number,
            sequence: record.sequence,
            in_use: record.in_use,
            is_directory: record.is_directory,
            base_reference: record.base_reference,
            has_attribute_list: record.has_attribute_list(),
            issues: record.issues.clone(),
            names: record
                .file_names()
                .into_iter()
                .map(|name| NameDetails {
                    name: name.name,
                    namespace: match name.namespace {
                        0 => "POSIX",
                        1 => "Win32",
                        2 => "DOS",
                        3 => "Win32 and DOS",
                        _ => "unknown",
                    },
                    parent_record: name.parent_record,
                    parent_sequence: name.parent_sequence,
                    modified: decode(name.modified_nt),
                })
                .collect(),
            streams,
        })
    }
}

/// One `$FILE_NAME` of a record. A file with several was linked in several places, or has a
/// short DOS name beside its long one.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NameDetails {
    pub name: String,
    pub namespace: &'static str,
    pub parent_record: u64,
    pub parent_sequence: u16,
    pub modified: Option<String>,
}

/// One `$DATA` stream. The unnamed one is the file's contents; a named one is an alternate
/// stream, which is listed here and never merged into the file.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamDetails {
    pub name: String,
    pub resident: bool,
    pub length: u64,
    pub initialized_length: u64,
    pub compressed: bool,
    pub encrypted: bool,
    pub sparse: bool,
    pub runs: Vec<NtfsRun>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordDetails {
    pub record_offset: u64,
    pub record_number: Option<u64>,
    pub sequence: u16,
    pub in_use: bool,
    pub is_directory: bool,
    pub base_reference: u64,
    pub has_attribute_list: bool,
    pub issues: Vec<String>,
    pub names: Vec<NameDetails>,
    pub streams: Vec<StreamDetails>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::BlockSource;
    use std::io;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A boot sector that says it is an NTFS volume of `clusters` clusters of 4096 bytes (512-byte sectors).
    fn boot_sector(clusters: u64) -> Vec<u8> {
        let mut boot = vec![0u8; 512];
        boot[3..11].copy_from_slice(b"NTFS    ");
        boot[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        boot[0x0D] = 8;
        boot[0x28..0x30].copy_from_slice(&(clusters * 8).to_le_bytes());
        boot[0x30..0x38].copy_from_slice(&4u64.to_le_bytes());
        boot[0x38..0x40].copy_from_slice(&2u64.to_le_bytes());
        boot[0x40] = 0xF6;
        boot[0x44] = 1;
        boot[0x48..0x50].copy_from_slice(&0x1234_5678_9ABC_DEF0u64.to_le_bytes());
        boot[510..512].copy_from_slice(&[0x55, 0xAA]);
        boot
    }

    /// Only the boot sector exists, and every read is counted.
    struct BootOnly {
        boot: Vec<u8>,
        reads: AtomicU64,
    }

    impl BlockSource for BootOnly {
        fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let end = offset as usize + buffer.len();
            match self.boot.get(offset as usize..end) {
                Some(bytes) => {
                    buffer.copy_from_slice(bytes);
                    Ok(())
                }
                None => Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            }
        }

        fn read_stats(&self) -> crate::source::ReadStats {
            crate::source::ReadStats::default()
        }
    }

    fn geometry(clusters: u64) -> Result<VolumeGeometry> {
        geometry_of(&parse_main(&boot_sector(clusters))?)
    }

    #[test]
    fn a_volume_with_2_to_the_32_clusters_is_refused_with_the_reason_and_so_is_one_below_it() {
        for clusters in [1u64 << 32, (1u64 << 32) - 1, (1u64 << 32) + 12345] {
            let message = format!("{:#}", geometry(clusters).unwrap_err());
            assert!(
                message.contains("Windows itself does not create or mount such a volume"),
                "{clusters}: {message}"
            );
            assert!(message.contains(&clusters.to_string()), "{message}");
            assert!(message.contains("4294967294"), "{message}");
        }
    }

    #[test]
    fn a_volume_with_exactly_4294967294_clusters_is_not_refused_at_the_count_check() {
        let found = geometry(4_294_967_294).expect("the largest volume Windows makes");
        assert_eq!(
            found.cluster_count, 4_294_967_292,
            "the two reserved clusters are not counted"
        );
        // The numbers still fit: the last cluster number, one past, does not overflow.
        assert_eq!(found.last_cluster_exclusive(), 4_294_967_294);
        // And the refusal is by the limit and nothing else: a volume one cluster bigger says so.
        assert!(geometry(4_294_967_295).is_err());
        assert_eq!(MAX_CLUSTERS, 4_294_967_294);
    }

    #[test]
    fn opening_such_a_volume_reads_nothing_but_its_boot_sector() {
        let refused = BootOnly {
            boot: boot_sector(1 << 32),
            reads: AtomicU64::new(0),
        };
        let error = read_structures(&refused).expect_err("refused at open");
        assert!(
            format!("{error:#}").contains("Windows itself does not create or mount"),
            "{error:#}"
        );
        assert_eq!(
            refused.reads.load(Ordering::Relaxed),
            1,
            "only the boot sector was read"
        );
        // The largest allowed volume is past the count check, and fails later for what it lacks (everything but the boot sector).
        let allowed = BootOnly {
            boot: boot_sector(4_294_967_294),
            reads: AtomicU64::new(0),
        };
        let error = format!(
            "{:#}",
            read_structures(&allowed).expect_err("a boot sector alone is not a volume")
        );
        assert!(!error.contains("Windows itself"), "{error}");
        assert!(allowed.reads.load(Ordering::Relaxed) > 1);
    }
}
