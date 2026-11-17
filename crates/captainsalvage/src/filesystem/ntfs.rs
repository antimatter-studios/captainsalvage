use super::driver::RegionDevice;
use crate::disk::RawDisk;
use anyhow::{ensure, Context, Result};
use fs_ntfs::primitives::{AttributeValue, PrimitiveAPI, RecordEvidence};
use fs_ntfs::volume::RawVolume;
use serde::Serialize;

pub struct Recovery {
    volume: RawVolume<RegionDevice>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stream {
    pub attribute: usize,
    pub name: String,
    pub length: u64,
    pub resident: bool,
    pub flags: u16,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub offset: u64,
    pub length: usize,
    pub in_use: bool,
    pub is_directory: bool,
    pub sequence: u16,
    pub validated: bool,
    pub issues: Vec<String>,
    pub streams: Vec<Stream>,
}

impl Recovery {
    pub fn open(disk: &RawDisk, start: u64, length: u64) -> Result<Self> {
        Ok(Self {
            volume: RawVolume::open(RegionDevice::open(disk, start, length)?),
        })
    }

    pub fn inspect(&self, offset: u64, length: usize, sector_size: u16) -> Result<RecordEvidence> {
        Ok(self
            .volume
            .read_file_record_at(offset, length, sector_size)?)
    }

    pub fn describe(record: &RecordEvidence) -> Record {
        let streams = record
            .attributes
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.type_code == 0x80)
            .map(|(index, entry)| {
                let (length, resident) = match &entry.value {
                    AttributeValue::Resident(data) => (data.len() as u64, true),
                    AttributeValue::NonResident { data_size, .. } => (*data_size, false),
                };
                Stream {
                    attribute: index,
                    name: entry.name.clone(),
                    length,
                    resident,
                    flags: entry.flags,
                }
            })
            .collect();
        Record {
            offset: record.offset,
            length: record.raw.len(),
            in_use: record.in_use,
            is_directory: record.is_directory,
            sequence: record.sequence,
            validated: record.fixed.is_some() && record.issues.is_empty(),
            issues: record.issues.clone(),
            streams,
        }
    }

    pub fn read_stream(
        &self,
        record: &RecordEvidence,
        attribute: usize,
        cluster_size: u64,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        ensure!(length <= 64 * 1024, "stream reads are limited to 64 KiB");
        let mut output = vec![0; length];
        let count = self.volume.read_salvage_stream_at(record, attribute, cluster_size, offset, &mut output)
            .context("reading NTFS salvage stream; surviving metadata does not prove its clusters have not been reused")?;
        output.truncate(count);
        Ok(output)
    }
}
