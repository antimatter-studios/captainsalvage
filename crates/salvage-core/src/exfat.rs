//! exFAT on-disk structures.
//!
//! Field offsets and checksum rules follow Microsoft's published "exFAT file
//! system specification".

use anyhow::{bail, Result};
use serde::Serialize;

pub const ENTRY_SIZE: usize = 32;
pub const FAT_END: u32 = 0xFFFF_FFFF;
pub const FAT_BAD: u32 = 0xFFFF_FFF7;
pub const FIRST_CLUSTER: u32 = 2;
pub const BOOT_REGION_SECTORS: u64 = 12;
const VOLUME_DIRTY: u16 = 0x0002;

const ATTRIBUTE_DIRECTORY: u16 = 0x10;
const ATTRIBUTE_VALID_MASK: u16 = 0x37;
const TYPE_FILE: u8 = 0x85;
const TYPE_FILE_DELETED: u8 = 0x05;
const TYPE_STREAM: u8 = 0xC0;
const TYPE_STREAM_DELETED: u8 = 0x40;
const TYPE_NAME: u8 = 0xC1;
const TYPE_NAME_DELETED: u8 = 0x41;
const TYPE_BITMAP: u8 = 0x81;
const NAME_CHARS_PER_ENTRY: usize = 15;
const MIN_SECONDARY_COUNT: usize = 2;
const MAX_SECONDARY_COUNT: usize = 18;
const STREAM_FLAG_NO_FAT_CHAIN: u8 = 0x02;

fn u16_at(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

fn u32_at(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().expect("four bytes"))
}

fn u64_at(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().expect("eight bytes"))
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BootSector {
    pub file_system_name: String,
    pub partition_offset: u64,
    pub volume_length: u64,
    pub fat_offset: u32,
    pub fat_length: u32,
    pub cluster_heap_offset: u32,
    pub cluster_count: u32,
    pub root_cluster: u32,
    pub volume_serial: u32,
    pub revision: u16,
    pub volume_flags: u16,
    pub bytes_per_sector_shift: u8,
    pub sectors_per_cluster_shift: u8,
    pub number_of_fats: u8,
    pub percent_in_use: u8,
    pub signature_ok: bool,
}

impl BootSector {
    pub fn parse(sector: &[u8]) -> Result<Self> {
        if sector.len() < 512 {
            bail!("boot sector is shorter than 512 bytes");
        }
        if &sector[3..11] != b"EXFAT   " {
            bail!("no exFAT signature at offset 3: this is not an exFAT volume");
        }
        Ok(Self {
            file_system_name: String::from_utf8_lossy(&sector[3..11]).into_owned(),
            partition_offset: u64_at(sector, 64),
            volume_length: u64_at(sector, 72),
            fat_offset: u32_at(sector, 80),
            fat_length: u32_at(sector, 84),
            cluster_heap_offset: u32_at(sector, 88),
            cluster_count: u32_at(sector, 92),
            root_cluster: u32_at(sector, 96),
            volume_serial: u32_at(sector, 100),
            revision: u16_at(sector, 104),
            volume_flags: u16_at(sector, 106),
            bytes_per_sector_shift: sector[108],
            sectors_per_cluster_shift: sector[109],
            number_of_fats: sector[110],
            percent_in_use: sector[112],
            signature_ok: sector[510] == 0x55 && sector[511] == 0xAA,
        })
    }

    pub fn bytes_per_sector(&self) -> u64 {
        1 << self.bytes_per_sector_shift
    }

    pub fn cluster_size(&self) -> u64 {
        self.bytes_per_sector() << self.sectors_per_cluster_shift
    }

    /// Set while a volume is mounted for writing, and cleared by a clean unmount.
    pub fn volume_dirty(&self) -> bool {
        self.volume_flags & VOLUME_DIRTY != 0
    }

    pub fn heap_offset(&self) -> u64 {
        u64::from(self.cluster_heap_offset) * self.bytes_per_sector()
    }

    pub fn last_cluster_exclusive(&self) -> u32 {
        self.cluster_count + FIRST_CLUSTER
    }
}

/// Checksum over sectors 0-10, skipping VolumeFlags and PercentInUse.
pub fn boot_checksum(region: &[u8], bytes_per_sector: usize) -> u32 {
    let mut checksum = 0u32;
    for (index, &byte) in region[..bytes_per_sector * 11].iter().enumerate() {
        if matches!(index, 106 | 107 | 112) {
            continue;
        }
        checksum = checksum.rotate_right(1).wrapping_add(u32::from(byte));
    }
    checksum
}

/// Stored checksum, held at the start of the twelfth sector of a boot region.
pub fn stored_boot_checksum(region: &[u8], bytes_per_sector: usize) -> u32 {
    u32_at(region, bytes_per_sector * 11)
}

/// Decodes an exFAT timestamp. Returns `None` for zero or impossible values.
pub fn decode_timestamp(value: u32) -> Option<String> {
    if value == 0 {
        return None;
    }
    let year = 1980 + (value >> 25);
    let month = (value >> 21) & 0xF;
    let day = (value >> 16) & 0x1F;
    let hour = (value >> 11) & 0x1F;
    let minute = (value >> 5) & 0x3F;
    let second = (value & 0x1F) * 2;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}"
    ))
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub cluster_count: u32,
    pub cluster_size: u64,
}

impl Limits {
    fn valid_cluster(&self, cluster: u32) -> bool {
        (FIRST_CLUSTER..self.cluster_count + FIRST_CLUSTER).contains(&cluster)
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntrySet {
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

fn set_checksum(set: &[u8]) -> u16 {
    let mut checksum = 0u16;
    for (index, &byte) in set.iter().enumerate() {
        if index == 2 || index == 3 {
            continue;
        }
        // Deleted sets clear InUse. Restoring it lets their stored checksum validate.
        let value = if index % ENTRY_SIZE == 0 {
            byte | 0x80
        } else {
            byte
        };
        checksum = checksum.rotate_right(1).wrapping_add(u16::from(value));
    }
    checksum
}

/// Parses the entry set at `position`. Returns `None` when the bytes there do
/// not have the structure of a file entry set.
pub fn parse_set(data: &[u8], position: usize, limits: Limits) -> Option<EntrySet> {
    let header = data.get(position..position + ENTRY_SIZE * 2)?;
    if header[0] != TYPE_FILE && header[0] != TYPE_FILE_DELETED {
        return None;
    }
    if header[ENTRY_SIZE] != TYPE_STREAM && header[ENTRY_SIZE] != TYPE_STREAM_DELETED {
        return None;
    }
    let secondary_count = usize::from(header[1]);
    if !(MIN_SECONDARY_COUNT..=MAX_SECONDARY_COUNT).contains(&secondary_count) {
        return None;
    }
    let slots = secondary_count + 1;
    let set = data.get(position..position + ENTRY_SIZE * slots)?;
    let stream = &set[ENTRY_SIZE..ENTRY_SIZE * 2];
    let name_length = usize::from(stream[3]);
    if name_length == 0 {
        return None;
    }
    let name_entries = name_length.div_ceil(NAME_CHARS_PER_ENTRY);
    if name_entries + 1 > secondary_count {
        return None;
    }
    let mut units = Vec::with_capacity(name_length);
    for index in 0..name_entries {
        let entry = &set[ENTRY_SIZE * (2 + index)..ENTRY_SIZE * (3 + index)];
        if entry[0] != TYPE_NAME && entry[0] != TYPE_NAME_DELETED {
            return None;
        }
        units.extend(
            entry[2..ENTRY_SIZE]
                .chunks_exact(2)
                .map(|pair| u16_at(pair, 0)),
        );
    }
    units.truncate(name_length);

    let checksum_ok = set_checksum(set) == u16_at(set, 2);
    let attributes = u16_at(set, 4);
    let first_cluster = u32_at(stream, 20);
    let length = u64_at(stream, 24);
    if !checksum_ok {
        // Without a checksum, accept only values that could plausibly be real.
        let capacity = u64::from(limits.cluster_count) * limits.cluster_size;
        let name_ok = units.iter().all(|&unit| unit >= 0x20);
        if attributes & !ATTRIBUTE_VALID_MASK != 0
            || length > capacity
            || !name_ok
            || !(first_cluster == 0 || limits.valid_cluster(first_cluster))
        {
            return None;
        }
    }
    Some(EntrySet {
        offset: position,
        name: String::from_utf16_lossy(&units),
        in_use: set[0] == TYPE_FILE,
        checksum_ok,
        attributes,
        is_dir: attributes & ATTRIBUTE_DIRECTORY != 0,
        first_cluster,
        length,
        valid_length: u64_at(stream, 8),
        no_fat_chain: stream[1] & STREAM_FLAG_NO_FAT_CHAIN != 0,
        created: decode_timestamp(u32_at(set, 8)),
        modified: decode_timestamp(u32_at(set, 12)),
        accessed: decode_timestamp(u32_at(set, 16)),
        slots,
    })
}

/// Finds every entry set starting within `data[..search_end]`. Sets may extend
/// past `search_end` into the rest of `data`, which is how sets that straddle a
/// cluster boundary are read when the next cluster follows on disk.
pub fn find_sets(data: &[u8], search_end: usize, limits: Limits) -> Vec<EntrySet> {
    let mut found = Vec::new();
    let mut position = 0;
    while position < search_end {
        match parse_set(data, position, limits) {
            Some(set) => {
                position += if set.checksum_ok {
                    set.slots * ENTRY_SIZE
                } else {
                    ENTRY_SIZE
                };
                found.push(set);
            }
            None => position += ENTRY_SIZE,
        }
    }
    found
}

/// Location of the allocation bitmap, read from its root directory entry.
pub fn find_bitmap_entry(root: &[u8]) -> Option<(u32, u64)> {
    root.chunks_exact(ENTRY_SIZE)
        .find(|entry| entry[0] == TYPE_BITMAP)
        .map(|entry| (u32_at(entry, 20), u64_at(entry, 24)))
}
