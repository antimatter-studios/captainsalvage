//! Working a lost NTFS volume's geometry out from the MFT records that survive.
//!
//! The `$MFT` record describes the MFT itself. Its data attribute holds the allocated size of
//! the MFT and the runs that make it up, so the allocated size divided by the clusters in those
//! runs is the cluster size, and the first run's cluster number says how far into the volume
//! the MFT starts. A record found at a known byte offset, whose own record number is in its
//! header, therefore fixes where the volume must begin.
//!
//! The result is a proposal, not a fact. It is tested by opening the volume with a boot sector
//! rebuilt in memory from these numbers and listing its root. The source is not written, and the
//! rebuilt sector exists only in the view the test reads through.

use super::{Finding, Status};
use crate::disk::{RawDisk, RegionSource};
use crate::filesystem::driver::Driver;
use crate::filesystem::ntfs_salvage::SourceDevice;
use crate::range_analysis::probes::{ntfs_record, NTFS_RECORD_SIZES};
use crate::source::BlockSource;
use fs_ntfs::primitives::{inspect_file_record, AttributeValue, RecordEvidence};
use serde::Serialize;
use std::sync::Arc;

const SECTOR: u64 = 512;
const MFT_RECORD: u64 = 0;
const BITMAP_RECORD: u64 = 6;
const DATA_ATTRIBUTE: u32 = 0x80;
/// Where a non-resident attribute keeps its allocated size.
const ALLOCATED_SIZE_AT: usize = 40;
const MAX_CLUSTER_BYTES: u64 = 2 * 1024 * 1024;
const MAX_LITERAL_SECTORS_PER_CLUSTER: u64 = 128;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Geometry {
    pub cluster_size: u64,
    pub record_size: u64,
    /// The cluster the MFT starts at, counted from the volume's first byte.
    pub mft_cluster: u64,
    pub volume_start: u64,
    pub volume_length: u64,
    /// The volume opened and listed its root with a boot sector rebuilt from these numbers.
    pub opens: bool,
    pub basis: String,
}

fn record_at(disk: &RawDisk, offset: u64, size: usize) -> Option<RecordEvidence> {
    let mut raw = disk.read(offset, size as u64).ok()?;
    raw[..4].copy_from_slice(b"FILE");
    let evidence = inspect_file_record(offset, raw, SECTOR as u16);
    (evidence.fixed.is_some() && evidence.issues.is_empty()).then_some(evidence)
}

fn data_attribute(
    record: &RecordEvidence,
) -> Option<(&fs_ntfs::primitives::AttributeEvidence, u64)> {
    let attribute = record
        .attributes
        .iter()
        .find(|attribute| attribute.type_code == DATA_ATTRIBUTE && attribute.name.is_empty())?;
    let fixed = record.fixed.as_ref()?;
    let at = attribute.offset + ALLOCATED_SIZE_AT;
    let allocated = u64::from_le_bytes(fixed.get(at..at + 8)?.try_into().ok()?);
    Some((attribute, allocated))
}

/// The in-memory boot sector for the proposed geometry.
fn boot_sector(
    cluster_size: u64,
    record_size: u64,
    mft_cluster: u64,
    total_sectors: u64,
) -> Option<Vec<u8>> {
    let per_cluster = cluster_size / SECTOR;
    if per_cluster == 0 || per_cluster > MAX_LITERAL_SECTORS_PER_CLUSTER {
        return None;
    }
    let mut boot = vec![0u8; 512];
    boot[..3].copy_from_slice(&[0xeb, 0x52, 0x90]);
    boot[3..11].copy_from_slice(b"NTFS    ");
    boot[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
    boot[13] = per_cluster as u8;
    boot[21] = 0xf8;
    boot[24..26].copy_from_slice(&63u16.to_le_bytes());
    boot[26..28].copy_from_slice(&255u16.to_le_bytes());
    boot[40..48].copy_from_slice(&total_sectors.to_le_bytes());
    boot[48..56].copy_from_slice(&mft_cluster.to_le_bytes());
    boot[56..64].copy_from_slice(&(total_sectors / per_cluster / 2).to_le_bytes());
    boot[64] = if record_size >= cluster_size {
        (record_size / cluster_size) as u8
    } else {
        (-(record_size.trailing_zeros() as i8)) as u8
    };
    boot[68] = if cluster_size <= 4096 {
        (4096 / cluster_size) as u8
    } else {
        (-12i8) as u8
    };
    boot[72..80].copy_from_slice(&0x5341_4c56_4147_4531u64.to_le_bytes());
    boot[510..512].copy_from_slice(&[0x55, 0xaa]);
    Some(boot)
}

fn open_with(disk: &RawDisk, start: u64, length: u64, boot: Vec<u8>) -> Option<usize> {
    let region = RegionSource::new(Arc::clone(&disk.source), disk.length, start, length)
        .ok()?
        .with_prefix(boot);
    let source: Arc<dyn BlockSource> = Arc::new(region);
    let driver = Driver::open(SourceDevice::new(source, length)).ok()?;
    driver.root_directory().ok().map(|entries| entries.len())
}

/// Tries to place the MFT records at `found.offset` in a volume, and fills in the finding.
pub fn infer(disk: &RawDisk, found: &mut Finding) {
    let Ok(window) = disk.read(found.offset, 4096.min(disk.length - found.offset)) else {
        return;
    };
    let Some((size, number)) = NTFS_RECORD_SIZES.iter().find_map(|size| {
        let block = window.get(..*size)?;
        Some((*size, ntfs_record(block)?.record_number?))
    }) else {
        return;
    };
    let record_size = size as u64;
    let Some(mft_start) = number
        .checked_mul(record_size)
        .and_then(|bytes| found.offset.checked_sub(bytes))
    else {
        return;
    };
    let Some(mft) = record_at(disk, mft_start, size).filter(|record| {
        record.record_number == Some(MFT_RECORD)
            && record.file_names().iter().any(|name| name.name == "$MFT")
    }) else {
        found.missing.push(
            "the $MFT record, which would give the cluster size and where the volume starts".into(),
        );
        return;
    };
    let Some((attribute, allocated)) = data_attribute(&mft) else {
        return;
    };
    let AttributeValue::NonResident {
        runs, first_vcn, ..
    } = &attribute.value
    else {
        return;
    };
    let clusters: u64 = runs.iter().map(|run| run.length).sum();
    let Some(first_lcn) = runs.first().and_then(|run| run.lcn) else {
        return;
    };
    if *first_vcn != 0 || clusters == 0 || allocated == 0 || allocated % clusters != 0 {
        return;
    }
    let cluster_size = allocated / clusters;
    if !cluster_size.is_power_of_two() || !(SECTOR..=MAX_CLUSTER_BYTES).contains(&cluster_size) {
        return;
    }
    let Some(volume_start) = first_lcn
        .checked_mul(cluster_size)
        .and_then(|bytes| mft_start.checked_sub(bytes))
    else {
        return;
    };
    // The bitmap's size bounds the volume: it has a bit for every cluster, in whole bytes.
    let bitmap_length = (BITMAP_RECORD * record_size < runs[0].length * cluster_size)
        .then(|| record_at(disk, mft_start + BITMAP_RECORD * record_size, size))
        .flatten()
        .filter(|record| record.record_number == Some(BITMAP_RECORD))
        .and_then(|record| {
            record.attributes.iter().find_map(|attribute| {
                match (&attribute.value, attribute.type_code == DATA_ATTRIBUTE) {
                    (AttributeValue::NonResident { data_size, .. }, true) => Some(*data_size),
                    _ => None,
                }
            })
        });
    let remaining = disk.length - volume_start;
    let (volume_length, basis) = match bitmap_length {
        Some(bytes) => (
            (bytes * 8 * cluster_size).min(remaining),
            "the $MFT record gives the cluster size and where the volume starts; its length is an upper bound from the size of $Bitmap",
        ),
        None => (
            remaining,
            "the $MFT record gives the cluster size and where the volume starts; nothing bounded its length, so it runs to the end of the source",
        ),
    };
    let opens = boot_sector(cluster_size, record_size, first_lcn, volume_length / SECTOR)
        .and_then(|boot| open_with(disk, volume_start, volume_length, boot));
    found.geometry = Some(Geometry {
        cluster_size,
        record_size,
        mft_cluster: first_lcn,
        volume_start,
        volume_length,
        opens: opens.is_some(),
        basis: basis.into(),
    });
    found.volume_start = Some(volume_start);
    found.volume_length = Some(volume_length);
    match opens {
        Some(entries) => {
            found.status = Status::Inferred;
            found.confidence = found.confidence.max(0.9);
            found.decoded_bytes = found.length + entries as u64 * 32;
            found.detail = format!(
                "{}; geometry inferred: {cluster_size} byte clusters, volume starts at byte {volume_start}, and it opens with a boot sector rebuilt in memory, listing {entries} root entries",
                found.detail
            );
            found.missing = vec!["the original boot sector, which is gone: the volume was opened with one rebuilt in memory".into()];
        }
        None => {
            found.confidence = found.confidence.min(0.6);
            found.detail = format!(
                "{}; geometry proposed ({cluster_size} byte clusters, volume starts at byte {volume_start}) but the volume does not open with it",
                found.detail
            );
        }
    }
}
