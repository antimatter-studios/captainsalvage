//! Format dispatch and common read-only file access across filesystem drivers.
//!
//! This is the adapter boundary for bringing additional filesystem drivers
//! into Captain Salvage. Format-specific forensic structures remain in their
//! drivers; callers use normalized entries and bounded reads here.

pub mod driver;
pub mod ntfs;
pub mod probe;
pub(crate) mod salvage;

use anyhow::{bail, Context, Result};
use fs_core::BlockRead;
use fs_fat32::file::FileAPI as FatFileAPI;
use fs_fat32::primitives::PrimitiveAPI as FatPrimitiveAPI;
use serde::Serialize;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_VOLUME_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilesystemKind {
    Exfat,
    Fat12,
    Fat16,
    Fat32,
    Ntfs,
}

impl fmt::Display for FilesystemKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Exfat => "exFAT",
            Self::Fat12 => "FAT12",
            Self::Fat16 => "FAT16",
            Self::Fat32 => "FAT32",
            Self::Ntfs => "NTFS",
        })
    }
}

impl FilesystemKind {
    pub fn detect_boot_sector(boot_sector: &[u8]) -> Result<Self> {
        if boot_sector.len() < 512 {
            bail!("filesystem boot sector is shorter than 512 bytes");
        }
        if &boot_sector[3..11] == b"EXFAT   " {
            return Ok(Self::Exfat);
        }
        if &boot_sector[3..11] == b"NTFS    " {
            return Ok(Self::Ntfs);
        }
        let boot = fs_fat32::BootSector::parse(boot_sector).context("identifying FAT variant")?;
        Ok(match boot.kind {
            fs_fat32::FatKind::Fat12 => Self::Fat12,
            fs_fat32::FatKind::Fat16 => Self::Fat16,
            fs_fat32::FatKind::Fat32 => Self::Fat32,
        })
    }
}

#[derive(Clone)]
enum DriverEntry {
    Exfat(fs_exfat::directory::DirectoryEntry),
    Fat(fs_fat32::directory::DirectoryEntry),
}

#[derive(Clone)]
pub struct FileEntry {
    pub name: String,
    pub size: u64,
    pub is_directory: bool,
    volume_id: u64,
    driver_entry: DriverEntry,
}

enum DriverVolume<Device: BlockRead> {
    Exfat(Box<fs_exfat::volume::Volume<std::sync::Arc<Device>>>),
    Fat(fs_fat32::Volume<std::sync::Arc<Device>>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VolumeGeometry {
    pub volume_serial: u32,
    pub root_cluster: u32,
    pub partition_offset: u64,
    pub data_offset: u64,
    pub fat_offset: u64,
    pub fat_bytes: u64,
    pub bytes_per_sector: u64,
    pub cluster_size: u64,
    pub cluster_count: u32,
    pub volume_size: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SalvageEntry {
    pub offset: usize,
    pub name: String,
    pub in_use: bool,
    pub checksum_ok: bool,
    pub attributes: u16,
    pub is_dir: bool,
    pub first_cluster: u32,
    pub length: u64,
    pub valid_length: u64,
    pub no_fat_chain: bool,
    pub created: Option<String>,
    pub modified: Option<String>,
    pub accessed: Option<String>,
    pub slots: usize,
}

impl From<fs_exfat::primitives::EntrySet> for SalvageEntry {
    fn from(entry: fs_exfat::primitives::EntrySet) -> Self {
        Self {
            offset: entry.offset,
            name: entry.name,
            in_use: entry.in_use,
            checksum_ok: entry.checksum_ok,
            attributes: entry.attributes,
            is_dir: entry.is_dir,
            first_cluster: entry.first_cluster,
            length: entry.length,
            valid_length: entry.valid_length,
            no_fat_chain: entry.no_fat_chain,
            created: entry.created,
            modified: entry.modified,
            accessed: entry.accessed,
            slots: entry.slots,
        }
    }
}

impl VolumeGeometry {
    pub fn cluster_size(&self) -> u64 {
        self.cluster_size
    }
    pub fn bytes_per_sector(&self) -> u64 {
        self.bytes_per_sector
    }
    pub fn heap_offset(&self) -> u64 {
        self.data_offset
    }
    pub fn last_cluster_exclusive(&self) -> u32 {
        self.cluster_count + 2
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SalvageFatEntry {
    Free,
    Next(u32),
    End,
    Bad,
    Invalid,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum FilesystemDetails {
    Exfat,
    Fat {
        variant: String,
        bytes_per_sector: u16,
        sectors_per_cluster: u8,
        reserved_sectors: u16,
        fat_count: u8,
        root_entry_count: u16,
        sectors_per_fat: u32,
        total_sectors: u32,
        first_data_sector: u32,
        root_cluster: u32,
    },
}

pub struct FilesystemVolume<Device: BlockRead> {
    kind: FilesystemKind,
    volume_id: u64,
    device: std::sync::Arc<Device>,
    driver: DriverVolume<Device>,
}

impl<Device: BlockRead> FilesystemVolume<Device> {
    pub fn open(device: Device) -> Result<Self> {
        let device = std::sync::Arc::new(device);
        let mut boot_sector = [0; 512];
        device
            .read_at(0, &mut boot_sector)
            .context("reading filesystem boot sector")?;
        let kind = FilesystemKind::detect_boot_sector(&boot_sector)?;
        if kind == FilesystemKind::Exfat {
            return Ok(Self {
                kind,
                volume_id: NEXT_VOLUME_ID.fetch_add(1, Ordering::Relaxed),
                device: std::sync::Arc::clone(&device),
                driver: DriverVolume::Exfat(Box::new(
                    fs_exfat::volume::Volume::open(std::sync::Arc::clone(&device))
                        .context("opening exFAT volume")?,
                )),
            });
        }
        if kind == FilesystemKind::Ntfs {
            bail!("NTFS volume detected; NTFS driver is not integrated yet");
        }
        let volume =
            fs_fat32::Volume::open(std::sync::Arc::clone(&device)).context("opening FAT volume")?;
        Ok(Self {
            kind,
            volume_id: NEXT_VOLUME_ID.fetch_add(1, Ordering::Relaxed),
            device: std::sync::Arc::clone(&device),
            driver: DriverVolume::Fat(volume),
        })
    }

    pub fn kind(&self) -> FilesystemKind {
        self.kind
    }

    pub fn details(&self) -> FilesystemDetails {
        match &self.driver {
            DriverVolume::Exfat(_) => FilesystemDetails::Exfat,
            DriverVolume::Fat(volume) => {
                let boot = volume.boot();
                FilesystemDetails::Fat {
                    variant: self.kind.to_string(),
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
        }
    }

    pub fn geometry(&self) -> VolumeGeometry {
        match &self.driver {
            DriverVolume::Exfat(volume) => {
                let boot = volume.boot();
                let bytes_per_sector = boot.bytes_per_sector();
                VolumeGeometry {
                    volume_serial: boot.volume_serial,
                    root_cluster: boot.root_cluster,
                    partition_offset: boot.partition_offset * bytes_per_sector,
                    data_offset: u64::from(boot.cluster_heap_offset) * bytes_per_sector,
                    fat_offset: u64::from(boot.fat_offset) * bytes_per_sector,
                    fat_bytes: u64::from(boot.fat_length) * bytes_per_sector,
                    bytes_per_sector,
                    cluster_size: boot.cluster_size(),
                    cluster_count: boot.cluster_count,
                    volume_size: boot.volume_length * bytes_per_sector,
                }
            }
            DriverVolume::Fat(volume) => {
                let boot = volume.boot();
                VolumeGeometry {
                    volume_serial: boot.volume_serial,
                    root_cluster: boot.root_cluster,
                    partition_offset: u64::from(boot.hidden_sectors)
                        * u64::from(boot.bytes_per_sector),
                    data_offset: u64::from(boot.first_data_sector)
                        * u64::from(boot.bytes_per_sector),
                    fat_offset: (u64::from(boot.reserved_sectors)
                        + u64::from(boot.active_fat) * u64::from(boot.sectors_per_fat))
                        * u64::from(boot.bytes_per_sector),
                    fat_bytes: u64::from(boot.sectors_per_fat) * u64::from(boot.bytes_per_sector),
                    bytes_per_sector: u64::from(boot.bytes_per_sector),
                    cluster_size: boot.cluster_size(),
                    cluster_count: boot.cluster_count,
                    volume_size: u64::from(boot.total_sectors) * u64::from(boot.bytes_per_sector),
                }
            }
        }
    }

    pub fn read_volume_bytes(&self, offset: u64, output: &mut [u8]) -> Result<()> {
        let end = offset
            .checked_add(output.len() as u64)
            .context("volume read range overflow")?;
        if end > self.geometry().volume_size {
            bail!("volume read exceeds the declared filesystem size");
        }
        self.device.read_at(offset, output)?;
        Ok(())
    }

    pub fn cluster_offset(&self, cluster: u32) -> Result<u64> {
        match &self.driver {
            DriverVolume::Exfat(volume) => Ok(volume.boot().cluster_offset(cluster)?),
            DriverVolume::Fat(volume) => Ok(volume.cluster_offset(cluster)?),
        }
    }

    /// Reads one allocation-table entry through the selected filesystem driver.
    pub fn fat_entry(&self, cluster: u32) -> Result<SalvageFatEntry> {
        match &self.driver {
            DriverVolume::Exfat(volume) => Ok(match volume.fat_entry(cluster)? {
                fs_exfat::metadata::FatEntry::Free => SalvageFatEntry::Free,
                fs_exfat::metadata::FatEntry::Next(next) => SalvageFatEntry::Next(next),
                fs_exfat::metadata::FatEntry::End => SalvageFatEntry::End,
                fs_exfat::metadata::FatEntry::Bad => SalvageFatEntry::Bad,
            }),
            DriverVolume::Fat(volume) => Ok(match volume.fat_value(cluster)? {
                fs_fat32::primitives::FatValue::Free => SalvageFatEntry::Free,
                fs_fat32::primitives::FatValue::Next(next) => SalvageFatEntry::Next(next),
                fs_fat32::primitives::FatValue::EndOfChain => SalvageFatEntry::End,
                fs_fat32::primitives::FatValue::Bad => SalvageFatEntry::Bad,
                fs_fat32::primitives::FatValue::Reserved(_)
                | fs_fat32::primitives::FatValue::OutOfRange(_) => SalvageFatEntry::Invalid,
            }),
        }
    }

    /// Returns the clusters reachable from a candidate head, stopping safely at malformed
    /// links, cycles, or the caller's limit rather than treating a damaged chain as fatal.
    pub fn chain_candidates(&self, first: u32, limit: usize) -> Result<Vec<u32>> {
        let mut clusters = Vec::new();
        let mut visited = std::collections::HashSet::new();
        let mut current = first;
        while clusters.len() < limit && visited.insert(current) {
            if current < 2 || current >= self.geometry().cluster_count.saturating_add(2) {
                break;
            }
            let entry = self.fat_entry(current)?;
            clusters.push(current);
            match entry {
                SalvageFatEntry::Next(next) => current = next,
                SalvageFatEntry::Free
                | SalvageFatEntry::End
                | SalvageFatEntry::Bad
                | SalvageFatEntry::Invalid => break,
            }
        }
        Ok(clusters)
    }

    /// Reports allocation consistently while retaining each format's source of truth:
    /// the exFAT bitmap, or non-zero FAT links for FAT12/16/32.
    pub fn is_allocated(&self, cluster: u32) -> Result<bool> {
        match &self.driver {
            DriverVolume::Exfat(volume) => Ok(volume.bitmap().allocated(cluster)?),
            DriverVolume::Fat(volume) => Ok(volume.fat_entry_raw(cluster)? != 0),
        }
    }

    pub fn read_cluster(&self, cluster: u32, output: &mut [u8]) -> Result<()> {
        let cluster_size = self.geometry().cluster_size;
        if output.len() as u64 != cluster_size {
            bail!("cluster buffer length does not match filesystem geometry");
        }
        self.read_volume_bytes(self.cluster_offset(cluster)?, output)
    }

    pub fn root_directory(&self) -> Result<Vec<FileEntry>> {
        match &self.driver {
            DriverVolume::Exfat(volume) => Ok(volume
                .root_directory()?
                .into_iter()
                .map(|entry| FileEntry {
                    name: entry.name().to_owned(),
                    size: entry.stream().data_length(),
                    is_directory: entry.is_directory(),
                    volume_id: self.volume_id,
                    driver_entry: DriverEntry::Exfat(entry),
                })
                .collect()),
            DriverVolume::Fat(volume) => Ok(volume
                .root_directory()?
                .into_iter()
                .map(|entry| FileEntry {
                    name: entry.name.clone(),
                    size: u64::from(entry.size),
                    is_directory: entry.attributes & 0x10 != 0,
                    volume_id: self.volume_id,
                    driver_entry: DriverEntry::Fat(entry),
                })
                .collect()),
        }
    }

    /// Reads a bounded range from an entry returned by this volume's root listing.
    pub fn read_file_at(&self, entry: &FileEntry, offset: u64, output: &mut [u8]) -> Result<usize> {
        if entry.volume_id != self.volume_id {
            bail!("file entry belongs to a different volume");
        }
        match (&self.driver, &entry.driver_entry) {
            (DriverVolume::Exfat(volume), DriverEntry::Exfat(driver_entry)) => {
                Ok(volume.open_file(driver_entry)?.read_at(offset, output)?)
            }
            (DriverVolume::Fat(volume), DriverEntry::Fat(driver_entry)) => {
                Ok(fs_fat32::file::FileAPI::open_file(volume, driver_entry)?
                    .read_at(offset, output)?)
            }
            _ => bail!("file entry belongs to a different filesystem driver"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDisk(Vec<u8>);

    impl BlockRead for TestDisk {
        fn read_at(&self, offset: u64, output: &mut [u8]) -> fs_core::Result<()> {
            let start = usize::try_from(offset).map_err(|_| fs_core::Error::OutOfBounds {
                offset,
                len: output.len() as u64,
                size: self.0.len() as u64,
            })?;
            let end = start
                .checked_add(output.len())
                .ok_or(fs_core::Error::OutOfBounds {
                    offset,
                    len: output.len() as u64,
                    size: self.0.len() as u64,
                })?;
            let bytes = self.0.get(start..end).ok_or(fs_core::Error::OutOfBounds {
                offset,
                len: output.len() as u64,
                size: self.0.len() as u64,
            })?;
            output.copy_from_slice(bytes);
            Ok(())
        }

        fn size_bytes(&self) -> u64 {
            self.0.len() as u64
        }
    }

    fn fat_image(kind: FilesystemKind, contents: &[u8]) -> Vec<u8> {
        let (sectors, fat_sectors, reserved, root_entries, root_sector, data_sector) = match kind {
            FilesystemKind::Fat12 => (4000usize, 12usize, 1usize, 224u16, 25usize, 39usize),
            FilesystemKind::Fat16 => (5000, 20, 1, 512, 41, 73),
            FilesystemKind::Fat32 => (67_000, 520, 32, 0, 1072, 1073),
            FilesystemKind::Exfat | FilesystemKind::Ntfs => panic!("not a FAT variant"),
        };
        let mut image = vec![0; sectors * 512];
        let boot = &mut image[..512];
        boot[11..13].copy_from_slice(&512u16.to_le_bytes());
        boot[13] = 1;
        boot[14..16].copy_from_slice(&(reserved as u16).to_le_bytes());
        boot[16] = 2;
        boot[17..19].copy_from_slice(&root_entries.to_le_bytes());
        if kind == FilesystemKind::Fat32 {
            boot[32..36].copy_from_slice(&(sectors as u32).to_le_bytes());
            boot[36..40].copy_from_slice(&(fat_sectors as u32).to_le_bytes());
            boot[44..48].copy_from_slice(&2u32.to_le_bytes());
        } else {
            boot[19..21].copy_from_slice(&(sectors as u16).to_le_bytes());
            boot[22..24].copy_from_slice(&(fat_sectors as u16).to_le_bytes());
        }
        boot[510..512].copy_from_slice(&[0x55, 0xAA]);

        let fat_offset = reserved * 512;
        match kind {
            FilesystemKind::Fat12 => {
                image[fat_offset..fat_offset + 5].copy_from_slice(&[0xF8, 0xFF, 0xFF, 0xFF, 0x0F])
            }
            FilesystemKind::Fat16 => {
                image[fat_offset + 4..fat_offset + 6].copy_from_slice(&0xFFFFu16.to_le_bytes());
            }
            FilesystemKind::Fat32 => {
                image[fat_offset + 8..fat_offset + 12]
                    .copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
                image[fat_offset + 12..fat_offset + 16]
                    .copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
            }
            FilesystemKind::Exfat | FilesystemKind::Ntfs => unreachable!(),
        }
        let root_start = root_sector * 512;
        image[root_start..root_start + 11].copy_from_slice(b"SAMPLE  TXT");
        image[root_start + 11] = 0x20;
        image[root_start + 26..root_start + 28].copy_from_slice(
            &(if kind == FilesystemKind::Fat32 {
                3u16
            } else {
                2u16
            })
            .to_le_bytes(),
        );
        image[root_start + 28..root_start + 32]
            .copy_from_slice(&(contents.len() as u32).to_le_bytes());
        let data_start = data_sector * 512;
        image[data_start..data_start + contents.len()].copy_from_slice(contents);
        image
    }

    fn assert_fat_image(kind: FilesystemKind, contents: &[u8]) {
        let volume = FilesystemVolume::open(TestDisk(fat_image(kind, contents))).unwrap();
        assert_eq!(volume.kind(), kind);
        let details = serde_json::to_value(volume.details()).unwrap();
        assert_eq!(details["kind"], "fat");
        assert_eq!(details["variant"], kind.to_string());
        assert_eq!(details["bytesPerSector"], 512);
        assert!(details["firstDataSector"].is_number());
        let files = volume.root_directory().unwrap();
        assert_eq!(files[0].name, "SAMPLE.TXT");
        assert_eq!(files[0].size, contents.len() as u64);
        let mut output = vec![0; contents.len()];
        assert_eq!(
            volume.read_file_at(&files[0], 0, &mut output).unwrap(),
            contents.len()
        );
        assert_eq!(output, contents);
    }

    #[test]
    fn fat12_fat16_and_fat32_dispatch_and_read_independent_images() {
        assert_fat_image(FilesystemKind::Fat12, b"from FAT12");
        assert_fat_image(FilesystemKind::Fat16, b"from FAT16");
        assert_fat_image(FilesystemKind::Fat32, b"from FAT32");
    }

    #[test]
    fn multiple_open_images_keep_reads_scoped_to_their_volume() {
        let first =
            FilesystemVolume::open(TestDisk(fat_image(FilesystemKind::Fat12, b"first disk")))
                .unwrap();
        let second =
            FilesystemVolume::open(TestDisk(fat_image(FilesystemKind::Fat16, b"second disk")))
                .unwrap();
        let first_entry = first.root_directory().unwrap().remove(0);
        let second_entry = second.root_directory().unwrap().remove(0);
        let mut first_bytes = [0; 10];
        let mut second_bytes = [0; 11];
        first
            .read_file_at(&first_entry, 0, &mut first_bytes)
            .unwrap();
        second
            .read_file_at(&second_entry, 0, &mut second_bytes)
            .unwrap();
        assert_eq!(&first_bytes, b"first disk");
        assert_eq!(&second_bytes, b"second disk");
        assert!(second
            .read_file_at(&first_entry, 0, &mut first_bytes)
            .is_err());
    }

    #[test]
    fn fat_variants_expose_geometry_bounded_volume_reads_and_cluster_reads() {
        let cases: [(FilesystemKind, &[u8], u32, u64, u32); 3] = [
            (FilesystemKind::Fat12, b"fat12 data".as_slice(), 2, 39, 4000),
            (FilesystemKind::Fat16, b"fat16 data".as_slice(), 2, 73, 5000),
            (
                FilesystemKind::Fat32,
                b"fat32 data".as_slice(),
                3,
                1073,
                67_000,
            ),
        ];
        for (kind, contents, cluster, data_sector, sectors) in cases {
            let volume = FilesystemVolume::open(TestDisk(fat_image(kind, contents))).unwrap();
            let geometry = volume.geometry();
            assert_eq!(geometry.bytes_per_sector, 512);
            assert_eq!(geometry.cluster_size, 512);
            assert!(geometry.cluster_count > 0);
            assert_eq!(geometry.volume_size, u64::from(sectors) * 512);
            assert_eq!(volume.cluster_offset(cluster).unwrap(), data_sector * 512);

            let mut raw_bytes = [0; 10];
            volume
                .read_volume_bytes(data_sector * 512, &mut raw_bytes)
                .unwrap();
            assert_eq!(&raw_bytes, contents);

            let mut cluster_bytes = [0; 512];
            volume.read_cluster(cluster, &mut cluster_bytes).unwrap();
            assert_eq!(&cluster_bytes[..contents.len()], contents);
            assert!(volume
                .read_cluster(geometry.cluster_count + 2, &mut cluster_bytes)
                .is_err());
            assert!(volume.read_cluster(cluster, &mut [0; 511]).is_err());
            assert!(volume
                .read_volume_bytes(geometry.volume_size, &mut [0; 1])
                .is_err());
        }
    }

    #[test]
    fn fat12_fat16_and_fat32_expose_normalized_salvage_primitives() {
        for kind in [
            FilesystemKind::Fat12,
            FilesystemKind::Fat16,
            FilesystemKind::Fat32,
        ] {
            let volume = FilesystemVolume::open(TestDisk(fat_image(kind, b"salvage"))).unwrap();
            let file_cluster = if kind == FilesystemKind::Fat32 { 3 } else { 2 };
            assert_eq!(
                volume.fat_entry(file_cluster).unwrap(),
                SalvageFatEntry::End
            );
            assert!(volume.is_allocated(file_cluster).unwrap());
            assert!(!volume.is_allocated(10).unwrap());
            assert_eq!(
                volume.chain_candidates(file_cluster, 8).unwrap(),
                vec![file_cluster]
            );
        }
    }

    #[test]
    fn exfat_boot_sector_is_recognized_for_driver_dispatch() {
        use fs_exfat::fixtures::{
            bitmap_entry, upcase_entry, Builder, BITMAP_CLUSTER, ROOT_CLUSTER,
        };
        use fs_exfat::primitives::{bitmap_location, BootSector, FAT_END};

        let mut builder = Builder::new();
        for cluster in [BITMAP_CLUSTER, ROOT_CLUSTER, 4] {
            builder.allocate(cluster);
        }
        builder.fat(ROOT_CLUSTER, FAT_END);
        let boot = BootSector::parse(&builder.image).unwrap();
        let bitmap = bitmap_entry(BITMAP_CLUSTER, u64::MAX);
        let location = bitmap_location(&boot, &bitmap).unwrap();
        builder.directory(
            ROOT_CLUSTER,
            &[
                bitmap_entry(location.first_cluster, location.required_length),
                upcase_entry(4, &[0; 100]),
            ],
        );
        builder.put(4, 0, &[0; 100]);
        assert_eq!(
            FilesystemKind::detect_boot_sector(&builder.image[..512]).unwrap(),
            FilesystemKind::Exfat
        );
    }

    #[test]
    fn ntfs_is_identified_but_not_opened_without_its_driver() {
        let mut boot_sector = [0; 512];
        boot_sector[3..11].copy_from_slice(b"NTFS    ");
        assert_eq!(
            FilesystemKind::detect_boot_sector(&boot_sector).unwrap(),
            FilesystemKind::Ntfs
        );
        let error = FilesystemVolume::open(TestDisk(boot_sector.to_vec()))
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("NTFS volume detected"));
    }

    #[test]
    fn common_driver_reads_all_fat_variants() {
        for kind in [
            FilesystemKind::Fat12,
            FilesystemKind::Fat16,
            FilesystemKind::Fat32,
        ] {
            let volume = driver::Driver::open(TestDisk(fat_image(kind, b"treasure"))).unwrap();
            assert_eq!(volume.geometry().format, kind.to_string());
            let files = volume.root_directory().unwrap();
            let mut output = [0; 8];
            assert_eq!(volume.read_file_at(&files[0], 0, &mut output).unwrap(), 8);
            assert_eq!(&output, b"treasure");
        }
    }
}
