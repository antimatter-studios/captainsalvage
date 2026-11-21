use super::{DriverDevice, FileEntry, FilesystemKind, FilesystemVolume};
use crate::disk::{RawDisk, RegionSource};
use anyhow::{bail, Result};
use fs_core::BlockRead;
use fs_ntfs::file::FileAPI;
use fs_ntfs::volume::Volume;
use fs_ntfs::VolumeAPI;
use serde::Serialize;
use std::sync::Arc;

pub struct RegionDevice {
    source: RegionSource,
    length: u64,
}

impl RegionDevice {
    pub fn open(disk: &RawDisk, start: u64, length: u64) -> Result<Self> {
        Ok(Self {
            source: RegionSource::new(Arc::clone(&disk.source), disk.length, start, length)?,
            length,
        })
    }
}

impl BlockRead for RegionDevice {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> fs_core::Result<()> {
        crate::source::BlockSource::read_exact_at(&self.source, offset, output)
            .map_err(fs_core::Error::Io)
    }
    fn size_bytes(&self) -> u64 {
        self.length
    }
}

impl fs_core_layers::BlockRead for RegionDevice {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> fs_core_layers::Result<()> {
        crate::source::BlockSource::read_exact_at(&self.source, offset, output)
            .map_err(fs_core_layers::Error::Io)
    }
    fn size_bytes(&self) -> u64 {
        self.length
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Geometry {
    pub format: String,
    pub volume_size: u64,
    pub bytes_per_sector: u64,
    pub cluster_size: u64,
    pub serial: String,
}

enum Implementation<Device: DriverDevice> {
    FatFamily(FilesystemVolume<Device>),
    Ntfs(Volume<Device>),
}

pub struct Driver<Device: DriverDevice> {
    implementation: Implementation<Device>,
}

enum Entry {
    FatFamily(FileEntry),
    Ntfs(Option<fs_ntfs::file::FileHandle>),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct File {
    pub name: String,
    pub size: u64,
    pub is_directory: bool,
    #[serde(skip)]
    entry: Entry,
}

impl<Device: DriverDevice> Driver<Device> {
    pub fn open(device: Device) -> Result<Self> {
        let mut boot = [0; 512];
        BlockRead::read_at(&device, 0, &mut boot)?;
        let implementation = match FilesystemKind::detect_boot_sector(&boot)? {
            FilesystemKind::Ntfs => Implementation::Ntfs(Volume::open(device)?),
            _ => Implementation::FatFamily(FilesystemVolume::open(device)?),
        };
        Ok(Self { implementation })
    }

    pub fn geometry(&self) -> Geometry {
        match &self.implementation {
            Implementation::FatFamily(volume) => {
                let geometry = volume.geometry();
                Geometry {
                    format: volume.kind().to_string(),
                    volume_size: geometry.volume_size,
                    bytes_per_sector: geometry.bytes_per_sector,
                    cluster_size: geometry.cluster_size,
                    serial: format!("{:08x}", geometry.volume_serial),
                }
            }
            Implementation::Ntfs(volume) => {
                let boot = volume.boot();
                Geometry {
                    format: "NTFS".into(),
                    volume_size: volume.size_bytes(),
                    bytes_per_sector: u64::from(boot.bytes_per_sector),
                    cluster_size: boot.cluster_size,
                    serial: format!("{:016x}", boot.serial_number),
                }
            }
        }
    }

    pub fn root_directory(&self) -> Result<Vec<File>> {
        match &self.implementation {
            Implementation::FatFamily(volume) => Ok(volume
                .root_directory()?
                .into_iter()
                .map(|entry| File {
                    name: entry.name.clone(),
                    size: entry.size,
                    is_directory: entry.is_directory,
                    entry: Entry::FatFamily(entry),
                })
                .collect()),
            Implementation::Ntfs(volume) => volume
                .read_directory("/")?
                .into_iter()
                .map(|entry| {
                    let path = format!("/{}", entry.name);
                    let metadata = volume.metadata(&path)?;
                    Ok(File {
                        name: entry.name,
                        size: metadata.size,
                        is_directory: metadata.is_dir,
                        entry: Entry::Ntfs(if metadata.is_dir {
                            None
                        } else {
                            Some(volume.open_file(&path)?)
                        }),
                    })
                })
                .collect(),
        }
    }

    pub fn read_file_at(&self, file: &File, offset: u64, output: &mut [u8]) -> Result<usize> {
        match (&self.implementation, &file.entry) {
            (Implementation::FatFamily(volume), Entry::FatFamily(entry)) => {
                volume.read_file_at(entry, offset, output)
            }
            (Implementation::Ntfs(volume), Entry::Ntfs(Some(handle))) => {
                Ok(volume.read_file_at(handle, offset, output)?)
            }
            _ => bail!("entry is a directory or belongs to another driver"),
        }
    }
}
