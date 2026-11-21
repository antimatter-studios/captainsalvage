use super::ntfs_salvage::{self, NtfsBoot};
use super::{FilesystemDetails, FilesystemKind, SalvageEntry, VolumeGeometry};
use crate::allocation;
use crate::changes::StructureCopy;
use crate::ranges::RangeList;
use crate::source::BlockSource;
use anyhow::{bail, Context, Result};
use fs_exfat::primitives::{
    self, BootCheck, BootSector, StructureError, StructureGeometry, FAT_BAD, FAT_END,
};
use fs_fat32::{BootSector as FatBoot, FatKind};
use serde::Serialize;

const MAX_STRUCTURE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum FilesystemBoot {
    Exfat(BootSector),
    Fat(FilesystemDetails),
    Ntfs(NtfsBoot),
    /// A driver's volume: what its own geometry says of it.
    Driver(FilesystemDetails),
}

impl FilesystemBoot {
    pub fn exfat(&self) -> Option<&BootSector> {
        match self {
            Self::Exfat(boot) => Some(boot),
            Self::Fat(_) | Self::Ntfs(_) | Self::Driver(_) => None,
        }
    }
}

pub struct Inspected {
    pub boot: FilesystemBoot,
    pub backup_boot: Option<FilesystemBoot>,
    pub boot_check: Option<BootCheck>,
    pub geometry: VolumeGeometry,
    pub fat: Vec<u32>,
    pub bitmap: Vec<u8>,
    pub root_clusters: Vec<u32>,
    pub kind: FilesystemKind,
    pub details: FilesystemDetails,
    pub dirty_flag: bool,
    /// Whether the allocation bitmap was read. It is only not for an NTFS volume whose `$Bitmap` is unreadable,
    /// and then `bitmap` is all zeros and says nothing.
    pub allocation_known: bool,
    /// What is wrong with the volume's own structures that the person is told of, in words.
    pub damage: Vec<String>,
}

fn structure_error(error: StructureError) -> anyhow::Error {
    match error {
        StructureError::Read { operation, source } => anyhow::Error::new(source).context(operation),
        StructureError::Boot { region, source } => {
            let error = match source {
                fs_exfat::Error::Invalid(reason) => anyhow::anyhow!(reason),
                error => error.into(),
            };
            error.context(format!("{region} boot sector"))
        }
        error => anyhow::anyhow!(error.to_string()),
    }
}

fn fat_geometry(boot: &FatBoot) -> VolumeGeometry {
    let sector = u64::from(boot.bytes_per_sector);
    VolumeGeometry {
        bytes_per_sector: sector,
        cluster_size: boot.cluster_size(),
        cluster_count: boot.cluster_count,
        volume_size: u64::from(boot.total_sectors) * sector,
        volume_serial: boot.volume_serial,
        root_cluster: boot.root_cluster,
        partition_offset: u64::from(boot.hidden_sectors) * sector,
        data_offset: u64::from(boot.first_data_sector) * sector,
        fat_offset: (u64::from(boot.reserved_sectors)
            + u64::from(boot.active_fat) * u64::from(boot.sectors_per_fat))
            * sector,
        fat_bytes: u64::from(boot.sectors_per_fat) * sector,
        record_size: 0,
    }
}

fn fat_details(boot: &FatBoot) -> FilesystemDetails {
    FilesystemDetails::Fat {
        variant: fat_kind(boot.kind).to_string(),
        bytes_per_sector: boot.bytes_per_sector,
        sectors_per_cluster: boot.sectors_per_cluster,
        reserved_sectors: boot.reserved_sectors,
        fat_count: boot.fat_count,
        root_entry_count: boot.root_entry_count,
        sectors_per_fat: boot.sectors_per_fat,
        total_sectors: boot.total_sectors,
        first_data_sector: boot.first_data_sector,
        root_cluster: boot.root_cluster,
    }
}

fn fat_kind(kind: FatKind) -> FilesystemKind {
    match kind {
        FatKind::Fat12 => FilesystemKind::Fat12,
        FatKind::Fat16 => FilesystemKind::Fat16,
        FatKind::Fat32 => FilesystemKind::Fat32,
    }
}

fn decode_fat(boot: &FatBoot, bytes: &[u8]) -> Result<Vec<u32>> {
    let size = usize::try_from(u64::from(boot.sectors_per_fat) * u64::from(boot.bytes_per_sector))?;
    let start = usize::from(boot.active_fat) * size;
    let active = bytes
        .get(start..start + size)
        .context("saved FAT does not contain the active table")?;
    (0..boot.cluster_count + 2)
        .map(|cluster| {
            let raw = fs_fat32::primitives::decode_fat_entry(boot.kind, active, cluster)?;
            use fs_fat32::primitives::FatValue;
            Ok(
                match fs_fat32::primitives::classify_fat_value(
                    boot.kind,
                    raw,
                    boot.cluster_count + 1,
                ) {
                    FatValue::Free => 0,
                    FatValue::Next(next) => next,
                    FatValue::EndOfChain => FAT_END,
                    FatValue::Bad => FAT_BAD,
                    FatValue::Reserved(_) | FatValue::OutOfRange(_) => u32::MAX - 2,
                },
            )
        })
        .collect()
}

fn read_region(source: &dyn BlockSource, offset: u64, length: u64) -> Result<Vec<u8>> {
    if length > MAX_STRUCTURE_BYTES {
        bail!("filesystem structure exceeds the 256 MiB inspection limit");
    }
    let mut bytes = vec![0; usize::try_from(length)?];
    source.read_exact_at(offset, &mut bytes)?;
    Ok(bytes)
}

pub fn read_structures(source: &dyn BlockSource) -> Result<StructureCopy> {
    let sector = super::read_head(|offset, buffer| source.read_exact_at(offset, buffer))
        .context("reading main boot region")?;
    if &sector[3..11] == b"NTFS    " {
        return ntfs_salvage::read_structures(source);
    }
    if &sector[3..11] == super::BITLOCKER_OEM_ID {
        bail!("{}", super::BITLOCKER_MESSAGE);
    }
    if let Some(driver) = super::drivers::recognise(&sector) {
        return super::drivers::read_structures(source, driver);
    }
    let boot = match FatBoot::parse(&sector) {
        Ok(boot) if &sector[3..11] != b"EXFAT   " => boot,
        _ => {
            return primitives::read_structures(|offset, buffer| {
                source.read_exact_at(offset, buffer)
            })
            .map(StructureCopy::from)
            .map_err(structure_error)
        }
    };
    let geometry = fat_geometry(&boot);
    let main =
        read_region(source, 0, geometry.bytes_per_sector).context("reading FAT boot sector")?;
    let reserved_bytes = u64::from(boot.reserved_sectors) * geometry.bytes_per_sector;
    let backup = read_region(
        source,
        geometry.bytes_per_sector,
        reserved_bytes - geometry.bytes_per_sector,
    )
    .context("reading FAT reserved sectors")?;
    let fat = read_region(
        source,
        reserved_bytes,
        geometry.fat_bytes * u64::from(boot.fat_count),
    )
    .context("reading FAT tables")?;
    let decoded = decode_fat(&boot, &fat)?;
    let root = if boot.kind == FatKind::Fat32 {
        let chain = allocation::root_chain(&geometry, &decoded);
        if chain.len() as u64 * geometry.cluster_size > MAX_STRUCTURE_BYTES {
            bail!("FAT root directory exceeds inspection limit");
        }
        let mut root = Vec::new();
        for cluster in chain {
            root.extend(read_region(
                source,
                allocation::cluster_offset(&geometry, cluster),
                geometry.cluster_size,
            )?);
        }
        root
    } else {
        read_region(
            source,
            reserved_bytes + geometry.fat_bytes * u64::from(boot.fat_count),
            u64::from(boot.root_dir_sectors) * geometry.bytes_per_sector,
        )?
    };
    Ok(StructureCopy {
        main,
        backup,
        fat,
        bitmap: Vec::new(),
        root,
        unread: Vec::new(),
    })
}

/// Where on the volume each region of a copy was read from, in the order of the manifest (the main boot region, its backup, the
/// allocation table, the allocation bitmap, the root): the first byte of each, from the geometry the copy itself decodes to. A
/// region the geometry does not place, or a copy that does not decode, has none.
pub fn region_offsets(copy: &StructureCopy) -> [Option<u64>; 5] {
    let mut offsets = [None; 5];
    if copy.main.is_empty() {
        return offsets;
    }
    offsets[0] = Some(0);
    if super::drivers::recognise(&copy.main).is_some() {
        // A driver's volume saves the bytes its structures are in, from the first, and no other region.
        return offsets;
    }
    if copy.main.get(3..11) == Some(b"NTFS    ") {
        if let Ok(inspected) = ntfs_salvage::inspect(copy) {
            // The backup boot sector is the volume's last sector; the root holds records 0, 3 and 5 of `$MFT`, the first of which
            // starts where the boot sector says `$MFT` does.
            offsets[1] = (!copy.backup.is_empty()).then_some(inspected.geometry.volume_size);
            offsets[4] = Some(inspected.boot.mft_cluster * inspected.boot.cluster_size);
        }
        return offsets;
    }
    let Ok(inspected) = inspect(copy) else {
        return [None; 5];
    };
    let geometry = &inspected.geometry;
    let sector = geometry.bytes_per_sector;
    if !copy.backup.is_empty() {
        offsets[1] = Some(if inspected.kind == FilesystemKind::Exfat {
            // The backup boot region follows the main one, which is twelve sectors.
            fs_exfat::primitives::BOOT_REGION_SECTORS * sector
        } else {
            // A FAT volume's reserved sectors after the boot sector.
            sector
        });
    }
    if !copy.fat.is_empty() {
        offsets[2] = Some(geometry.fat_offset);
    }
    offsets[4] = match inspected.kind {
        FilesystemKind::Exfat => Some(allocation::cluster_offset(geometry, geometry.root_cluster)),
        FilesystemKind::Fat32 => Some(allocation::cluster_offset(geometry, geometry.root_cluster)),
        FilesystemKind::Fat12 | FilesystemKind::Fat16 if !copy.root.is_empty() => {
            // The fixed root directory follows the allocation table's copies.
            let copies = copy.fat.len() as u64 / geometry.fat_bytes.max(1);
            Some(geometry.fat_offset + geometry.fat_bytes * copies)
        }
        _ => None,
    };
    offsets
}

pub fn inspect(structures: &StructureCopy) -> Result<Inspected> {
    if let Some(driver) = super::drivers::recognise(&structures.main) {
        return super::drivers::inspect(structures, driver);
    }
    if structures.main.get(3..11) == Some(b"NTFS    ") {
        let inspected = ntfs_salvage::inspect(structures)?;
        return Ok(Inspected {
            boot: FilesystemBoot::Ntfs(inspected.boot),
            backup_boot: inspected.backup_boot.map(FilesystemBoot::Ntfs),
            boot_check: None,
            geometry: inspected.geometry,
            fat: Vec::new(),
            bitmap: inspected.bitmap,
            root_clusters: Vec::new(),
            kind: FilesystemKind::Ntfs,
            details: inspected.details,
            dirty_flag: inspected.dirty,
            allocation_known: inspected.allocation_known,
            damage: inspected.damage,
        });
    }
    if structures.main.get(3..11) != Some(b"EXFAT   ") {
        if let Ok(boot) = FatBoot::parse(&structures.main) {
            let geometry = fat_geometry(&boot);
            if structures.main.len() as u64 != geometry.bytes_per_sector
                || structures.backup.len() as u64
                    != (u64::from(boot.reserved_sectors) - 1) * geometry.bytes_per_sector
                || structures.fat.len() as u64 != geometry.fat_bytes * u64::from(boot.fat_count)
            {
                bail!("saved FAT structures do not match BPB region lengths");
            }
            let fat = decode_fat(&boot, &structures.fat)?;
            let root_clusters = allocation::root_chain(&geometry, &fat);
            let expected_root = if boot.kind == FatKind::Fat32 {
                root_clusters.len() as u64 * geometry.cluster_size
            } else {
                u64::from(boot.root_dir_sectors) * geometry.bytes_per_sector
            };
            if structures.root.len() as u64 != expected_root {
                bail!("saved FAT root does not match its geometry and chain");
            }
            let mut bitmap = vec![0; usize::try_from(u64::from(boot.cluster_count).div_ceil(8))?];
            for cluster in 2..geometry.last_cluster_exclusive() {
                if fat[cluster as usize] != 0 {
                    bitmap[(cluster as usize - 2) / 8] |= 1 << ((cluster - 2) % 8);
                }
            }
            let dirty_flag = match boot.kind {
                FatKind::Fat12 => false,
                FatKind::Fat16 | FatKind::Fat32 => {
                    let size = geometry.fat_bytes as usize;
                    let active = &structures.fat[usize::from(boot.active_fat) * size..][..size];
                    let raw = fs_fat32::primitives::decode_fat_entry(boot.kind, active, 1)?;
                    raw & if boot.kind == FatKind::Fat16 {
                        1 << 15
                    } else {
                        1 << 27
                    } == 0
                }
            };
            let details = fat_details(&boot);
            return Ok(Inspected {
                boot: FilesystemBoot::Fat(details.clone()),
                backup_boot: None,
                boot_check: None,
                geometry,
                fat,
                bitmap,
                root_clusters,
                kind: fat_kind(boot.kind),
                details,
                dirty_flag,
                allocation_known: true,
                damage: Vec::new(),
            });
        }
    }
    let raw = structures.clone().into();
    let StructureGeometry {
        boot,
        backup_boot,
        boot_check,
        fat,
        root_clusters,
    } = primitives::inspect_structures(&raw).map_err(structure_error)?;
    let sector = boot.bytes_per_sector();
    let geometry = VolumeGeometry {
        bytes_per_sector: sector,
        cluster_size: boot.cluster_size(),
        cluster_count: boot.cluster_count,
        volume_size: boot.volume_length * sector,
        volume_serial: boot.volume_serial,
        root_cluster: boot.root_cluster,
        partition_offset: boot.partition_offset * sector,
        data_offset: boot.heap_offset(),
        fat_offset: u64::from(boot.fat_offset) * sector,
        fat_bytes: u64::from(boot.fat_length) * sector,
        record_size: 0,
    };
    let dirty_flag = boot.volume_dirty();
    Ok(Inspected {
        boot: FilesystemBoot::Exfat(boot),
        backup_boot: Some(FilesystemBoot::Exfat(backup_boot)),
        boot_check: Some(boot_check),
        geometry,
        fat,
        bitmap: structures.bitmap.clone(),
        root_clusters,
        kind: FilesystemKind::Exfat,
        details: FilesystemDetails::Exfat,
        dirty_flag,
        allocation_known: true,
        damage: Vec::new(),
    })
}

pub fn find_entries(
    kind: FilesystemKind,
    geometry: &VolumeGeometry,
    fat: &[u32],
    data: &[u8],
    start_limit: usize,
) -> Vec<SalvageEntry> {
    let fat_kind = match kind {
        FilesystemKind::Exfat => {
            return primitives::find_sets(
                data,
                start_limit,
                primitives::Limits {
                    cluster_count: geometry.cluster_count,
                    cluster_size: geometry.cluster_size,
                },
            )
            .into_iter()
            .map(|set| {
                let mut entry = SalvageEntry::from(set);
                entry.ranges = RangeList::for_cluster_stream(
                    geometry,
                    fat,
                    entry.first_cluster,
                    entry.length,
                    entry.valid_length,
                    entry.no_fat_chain,
                );
                entry
            })
            .collect()
        }
        FilesystemKind::Fat12 => FatKind::Fat12,
        FilesystemKind::Fat16 => FatKind::Fat16,
        FilesystemKind::Fat32 => FatKind::Fat32,
        FilesystemKind::Ntfs => return ntfs_salvage::find_entries(geometry, data, start_limit),
        // A driver's entries are the driver's candidates, found once by `Volume`, not in the clusters.
        FilesystemKind::Squashfs | FilesystemKind::Ext4 => return Vec::new(),
    };
    let aligned = &data[..data.len() / 32 * 32];
    fs_fat32::directory::scan_directory(aligned, fat_kind)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|record| {
            let entry = record.entry;
            if record.offset >= start_limit || matches!(entry.name.as_str(), "." | "..") {
                return None;
            }
            let is_dir = entry.attributes & 0x10 != 0;
            let cluster_valid = allocation::valid_cluster(geometry, entry.first_cluster);
            if (!cluster_valid && (is_dir || entry.size != 0 || entry.first_cluster != 0))
                || u64::from(entry.size) > geometry.volume_size
            {
                return None;
            }
            let length = if is_dir {
                allocation::clusters_for(
                    geometry,
                    fat,
                    entry.first_cluster,
                    u64::from(geometry.cluster_count) * geometry.cluster_size,
                    false,
                )
                .clusters
                .len() as u64
                    * geometry.cluster_size
            } else {
                u64::from(entry.size)
            };
            let stamp = |date: usize, time: Option<usize>| {
                let date = u32::from(u16::from_le_bytes(
                    record.raw[date..date + 2].try_into().unwrap(),
                ));
                let time = time
                    .map(|offset| {
                        u32::from(u16::from_le_bytes(
                            record.raw[offset..offset + 2].try_into().unwrap(),
                        ))
                    })
                    .unwrap_or(0);
                primitives::decode_timestamp((date << 16) | time)
            };
            Some(SalvageEntry {
                offset: record.offset,
                name: entry.name,
                in_use: !entry.deleted,
                checksum_ok: true,
                attributes: u16::from(entry.attributes),
                is_dir,
                first_cluster: entry.first_cluster,
                length,
                valid_length: length,
                no_fat_chain: false,
                created: stamp(16, Some(14)),
                modified: stamp(24, Some(22)),
                accessed: stamp(18, None),
                slots: 1,
                record: None,
                ranges: RangeList::for_cluster_stream(
                    geometry,
                    fat,
                    entry.first_cluster,
                    length,
                    length,
                    false,
                ),
                name_slot: 0,
                weak: None,
                walk: None,
                driver: None,
            })
        })
        .collect()
}

pub fn comparison_copy(copy: &StructureCopy) -> Result<StructureCopy> {
    if copy.main.get(3..11) == Some(b"NTFS    ") {
        return ntfs_salvage::comparison_copy(copy);
    }
    let inspected = inspect(copy)?;
    let fat = if inspected.kind == FilesystemKind::Exfat {
        copy.fat.clone()
    } else {
        inspected
            .fat
            .iter()
            .flat_map(|entry| entry.to_le_bytes())
            .collect()
    };
    Ok(StructureCopy {
        fat,
        bitmap: inspected.bitmap,
        ..copy.clone()
    })
}

pub fn changed_fat_entries(before: &StructureCopy, after: &StructureCopy) -> Result<u64> {
    if before.main.get(3..11) == Some(b"NTFS    ") {
        // NTFS has no allocation table of links; its bitmap changes are reported as clusters.
        return Ok(0);
    }
    let old = FatBoot::parse(&before.main)?;
    let new = FatBoot::parse(&after.main)?;
    let entry = |copy: &StructureCopy, boot: &FatBoot, table: u8, cluster: u32| {
        if table >= boot.fat_count || cluster >= boot.cluster_count + 2 {
            return None;
        }
        let size = u64::from(boot.sectors_per_fat) * u64::from(boot.bytes_per_sector);
        let start = u64::from(table) * size;
        let bytes = copy.fat.get(start as usize..(start + size) as usize)?;
        fs_fat32::primitives::decode_fat_entry(boot.kind, bytes, cluster).ok()
    };
    Ok((0..old.cluster_count.max(new.cluster_count) + 2)
        .filter(|&cluster| {
            (0..old.fat_count.max(new.fat_count)).any(|table| {
                entry(before, &old, table, cluster) != entry(after, &new, table, cluster)
            })
        })
        .count() as u64)
}
