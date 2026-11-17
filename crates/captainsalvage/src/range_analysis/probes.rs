//! One probe per filesystem driver, each able to say how much of a byte window decodes.
//!
//! A probe is given the bytes that start at one candidate offset and reports the run of
//! structures that decode from the first byte on, or nothing. The analysis slides the start
//! offset over the selected range and asks every probe at every offset, so a structure is found
//! wherever it begins. Signatures (boot sector identifiers, the `FILE` record magic) are only the
//! cheap way to reject most offsets at once and to raise confidence. The structural decoders do
//! not depend on them, so a record whose magic was overwritten still decodes by its update
//! sequence array and attribute chain, and FAT directory entries have no signature at all.

use fs_exfat::primitives::{parse_set, Limits};

/// What kind of thing decoded at an offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A boot sector: it names a whole volume and its geometry.
    Boot,
    /// Metadata structures (directory entries, FILE records) with no volume around them.
    Structures,
    /// Recognised but unreadable, such as an encrypted volume.
    Encrypted,
}

/// The run of structures that decodes from the first byte of a window.
#[derive(Clone, Debug)]
pub struct Decoded {
    pub format: &'static str,
    pub structure: &'static str,
    pub kind: Kind,
    /// Bytes covered by the structures that decoded.
    pub bytes: u64,
    /// How many structures that is: records, entry sets, directory entries.
    pub units: u64,
    /// Whether a signature identified it. False when it decoded by structure alone.
    pub signature: bool,
    pub confidence: f64,
    /// For a boot sector, the length of the volume it declares.
    pub volume_length: Option<u64>,
    pub detail: String,
    /// A few names the structures hold. Names are private: tool answers hide them from agents.
    pub names: Vec<String>,
    /// Whether the run reached the end of the window, so more may decode beyond it.
    pub reached_end: bool,
}

pub trait FormatProbe: Sync {
    /// The driver this probe stands for, as the report names it.
    fn family(&self) -> &'static str;
    /// Decodes from `window[0]`. `signatures` false turns every signature check off.
    fn decode_at(&self, window: &[u8], signatures: bool) -> Option<Decoded>;
}

pub fn probes() -> Vec<Box<dyn FormatProbe>> {
    vec![
        Box::new(ExfatProbe),
        Box::new(FatProbe),
        Box::new(NtfsProbe),
        Box::new(BitlockerProbe),
    ]
}

const ENTRY: usize = 32;
const MAX_NAMES: usize = 3;
const SECTOR: usize = 512;

fn confidence(base: f64, per_unit: f64, units: u64, cap: f64) -> f64 {
    (base + per_unit * units as f64).min(cap)
}

fn u16_at(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().unwrap())
}

// ---------------------------------------------------------------- exFAT

struct ExfatProbe;

impl FormatProbe for ExfatProbe {
    fn family(&self) -> &'static str {
        "exFAT"
    }

    fn decode_at(&self, window: &[u8], signatures: bool) -> Option<Decoded> {
        let first = *window.first()?;
        if signatures && first == 0xeb && window.len() >= SECTOR && &window[3..11] == b"EXFAT   " {
            if let Some(boot) = exfat_boot(window) {
                return Some(boot);
            }
        }
        // 0x85 and 0x05 are file entry sets; 0x81, 0x82 and 0x83 are the volume entries a root
        // directory starts with.
        if !matches!(first, 0x85 | 0x05 | 0x81 | 0x82 | 0x83) {
            return None;
        }
        exfat_sets(window)
    }
}

fn exfat_boot(window: &[u8]) -> Option<Decoded> {
    let boot = fs_exfat::boot::BootSector::parse(window).ok()?;
    let sector = boot.bytes_per_sector();
    if !(512..=4096).contains(&sector) || boot.sectors_per_cluster_shift > 25 {
        return None;
    }
    let length = boot.volume_length.checked_mul(sector)?;
    // The boot region is twelve sectors, and the twelfth holds a checksum of the first eleven.
    let region = (sector as usize) * 12;
    let checksum = (window.len() >= region)
        .then(|| {
            let computed = fs_exfat::boot::boot_checksum(window, sector as usize).ok()?;
            let stored = u32_at(window, (sector as usize) * 11);
            Some(computed == stored)
        })
        .flatten();
    let (value, note) = match checksum {
        Some(true) => (0.97, "boot region checksum matches"),
        Some(false) => (0.8, "boot region checksum does not match"),
        None => (0.88, "boot region checksum could not be read"),
    };
    Some(Decoded {
        format: "exFAT",
        structure: "boot sector",
        kind: Kind::Boot,
        bytes: sector,
        units: 1,
        signature: true,
        confidence: value,
        volume_length: Some(length),
        detail: format!("exFAT boot sector, {note}"),
        names: Vec::new(),
        reached_end: false,
    })
}

fn exfat_sets(window: &[u8]) -> Option<Decoded> {
    let limits = Limits {
        cluster_count: u32::MAX - 2,
        cluster_size: 1,
    };
    let mut position = 0;
    let mut sets = 0u64;
    let mut names = Vec::new();
    let mut last_set_end = 0;
    let mut pending_volume_entries = 0;
    loop {
        if let Some(set) = parse_set(window, position, limits).filter(|set| set.checksum_ok) {
            position += set.slots * ENTRY;
            last_set_end = position;
            pending_volume_entries = 0;
            sets += 1;
            if names.len() < MAX_NAMES && !set.name.is_empty() {
                names.push(set.name);
            }
            continue;
        }
        // A root directory starts with its bitmap, upcase and label entries, which are not
        // sets but are part of the same directory.
        match window.get(position) {
            Some(0x81..=0x83) if pending_volume_entries < 3 => {
                pending_volume_entries += 1;
                position += ENTRY;
            }
            _ => break,
        }
    }
    if sets == 0 {
        return None;
    }
    Some(Decoded {
        format: "exFAT",
        structure: "checksummed directory entry sets",
        kind: Kind::Structures,
        bytes: last_set_end as u64,
        units: sets,
        signature: false,
        confidence: confidence(0.5, 0.08, sets, 0.95),
        volume_length: None,
        detail: format!("{sets} directory entry sets whose checksums match"),
        names,
        reached_end: last_set_end + 2 * ENTRY > window.len(),
    })
}

// ---------------------------------------------------------------- FAT

struct FatProbe;

impl FormatProbe for FatProbe {
    fn family(&self) -> &'static str {
        "FAT"
    }

    fn decode_at(&self, window: &[u8], signatures: bool) -> Option<Decoded> {
        let first = *window.first()?;
        if signatures && matches!(first, 0xeb | 0xe9) && window.len() >= SECTOR {
            if let Some(boot) = fat_boot(window) {
                return Some(boot);
            }
        }
        fat_directory(window)
    }
}

fn fat_boot(window: &[u8]) -> Option<Decoded> {
    if &window[3..11] == b"NTFS    "
        || &window[3..11] == b"EXFAT   "
        || &window[3..11] == crate::filesystem::BITLOCKER_OEM_ID
    {
        return None;
    }
    let boot = fs_fat32::BootSector::parse(window).ok()?;
    if boot.fat_count == 0 || boot.fat_count > 2 || boot.total_sectors == 0 {
        return None;
    }
    let format = match boot.kind {
        fs_fat32::FatKind::Fat12 => "FAT12",
        fs_fat32::FatKind::Fat16 => "FAT16",
        fs_fat32::FatKind::Fat32 => "FAT32",
    };
    Some(Decoded {
        format,
        structure: "boot sector",
        kind: Kind::Boot,
        bytes: SECTOR as u64,
        units: 1,
        signature: true,
        // A FAT boot sector is plain numbers with no checksum, so it is weaker evidence than an
        // exFAT or NTFS one until the volume opens.
        confidence: 0.6,
        volume_length: Some(u64::from(boot.total_sectors) * u64::from(boot.bytes_per_sector)),
        detail: format!("{format} boot sector with plausible geometry"),
        names: Vec::new(),
        reached_end: false,
    })
}

/// How one 32-byte unit reads as a FAT directory entry.
enum FatEntry {
    End,
    Invalid,
    Long,
    Short { name: Option<String> },
}

fn fat_date_ok(value: u16) -> bool {
    let day = value & 0x1f;
    let month = (value >> 5) & 0xf;
    value == 0 || ((1..=31).contains(&day) && (1..=12).contains(&month))
}

fn fat_time_ok(value: u16) -> bool {
    (value >> 11) < 24 && ((value >> 5) & 0x3f) < 60 && (value & 0x1f) < 30
}

fn fat_name_byte_ok(byte: u8) -> bool {
    byte >= 0x20
        && !matches!(
            byte,
            b'"' | b'*'
                | b'+'
                | b','
                | b'/'
                | b':'
                | b';'
                | b'<'
                | b'='
                | b'>'
                | b'?'
                | b'['
                | b'\\'
                | b']'
                | b'|'
                | 0x7f
        )
}

fn fat_entry(entry: &[u8]) -> FatEntry {
    let first = entry[0];
    if first == 0 {
        return FatEntry::End;
    }
    let attributes = entry[11];
    if attributes == 0x0f {
        // A long-name entry: sequence number, a zero type byte and a zero first-cluster field.
        let sequence = first & 0x3f;
        return if entry[12] == 0 && u16_at(entry, 26) == 0 && (1..=20).contains(&sequence) {
            FatEntry::Long
        } else {
            FatEntry::Invalid
        };
    }
    if attributes & 0xc0 != 0 {
        return FatEntry::Invalid;
    }
    let name = &entry[..11];
    let dot = name[0] == b'.' && name[1..].iter().all(|byte| *byte == b' ' || *byte == b'.');
    if !dot {
        let start = usize::from(first == 0x05 || first == 0xe5);
        if name[start..]
            .iter()
            .any(|byte| !fat_name_byte_ok(*byte) && *byte < 0x80)
            || name[0] == 0x20
        {
            return FatEntry::Invalid;
        }
    }
    if !fat_date_ok(u16_at(entry, 16))
        || !fat_date_ok(u16_at(entry, 24))
        || !fat_time_ok(u16_at(entry, 14))
        || !fat_time_ok(u16_at(entry, 22))
    {
        return FatEntry::Invalid;
    }
    // A directory's size is always zero.
    if attributes & 0x10 != 0 && u32_at(entry, 28) != 0 {
        return FatEntry::Invalid;
    }
    // A volume label carries no cluster; anything else with a size has one.
    if attributes & 0x08 == 0
        && attributes & 0x10 == 0
        && u32_at(entry, 28) != 0
        && u16_at(entry, 26) == 0
        && u16_at(entry, 20) == 0
    {
        return FatEntry::Invalid;
    }
    let readable = (first != 0xe5 && !dot && attributes & 0x08 == 0).then(|| {
        let base = String::from_utf8_lossy(&name[..8]).trim_end().to_string();
        let extension = String::from_utf8_lossy(&name[8..]).trim_end().to_string();
        if extension.is_empty() {
            base
        } else {
            format!("{base}.{extension}")
        }
    });
    FatEntry::Short { name: readable }
}

/// A run of FAT directory entries starting at the first byte. FAT directories carry no
/// signature, so this is the decode that still works when everything around them is gone.
fn fat_directory(window: &[u8]) -> Option<Decoded> {
    // Most offsets fail on the first entry, so reject cheaply before counting anything.
    if window.len() < ENTRY
        || matches!(
            fat_entry(&window[..ENTRY]),
            FatEntry::End | FatEntry::Invalid
        )
    {
        return None;
    }
    let mut units = 0u64;
    let mut short = 0u64;
    let mut names = Vec::new();
    let mut position = 0;
    let mut reached_end = false;
    loop {
        let Some(entry) = window.get(position..position + ENTRY) else {
            reached_end = true;
            break;
        };
        match fat_entry(entry) {
            FatEntry::End | FatEntry::Invalid => break,
            FatEntry::Long => units += 1,
            FatEntry::Short { name } => {
                units += 1;
                short += 1;
                if let Some(name) = name.filter(|name| !name.is_empty()) {
                    if names.len() < MAX_NAMES {
                        names.push(name);
                    }
                }
            }
        }
        position += ENTRY;
    }
    // Four plain entries in a row are very unlikely by chance; fewer are not evidence.
    if units < 4 || short < 2 {
        return None;
    }
    Some(Decoded {
        format: "FAT",
        structure: "directory entries",
        kind: Kind::Structures,
        bytes: position as u64,
        units,
        signature: false,
        confidence: confidence(0.1, 0.07, units, 0.85),
        volume_length: None,
        detail: format!("{units} FAT directory entries ({short} with 8.3 names)"),
        names,
        reached_end,
    })
}

// ---------------------------------------------------------------- NTFS

struct NtfsProbe;

/// MFT record sizes in use: 1 KiB almost always, 4 KiB on 4Kn media.
pub(crate) const NTFS_RECORD_SIZES: [usize; 2] = [1024, 4096];
const MAX_BLANK_RECORDS: usize = 16;
const FILE_MAGIC: &[u8; 4] = b"FILE";

impl FormatProbe for NtfsProbe {
    fn family(&self) -> &'static str {
        "NTFS"
    }

    fn decode_at(&self, window: &[u8], signatures: bool) -> Option<Decoded> {
        if signatures && window.len() >= SECTOR && &window[3..11] == b"NTFS    " {
            if let Some(boot) = ntfs_boot(window) {
                return Some(boot);
            }
        }
        for size in NTFS_RECORD_SIZES {
            if let Some(found) = ntfs_records(window, size, signatures) {
                return Some(found);
            }
        }
        None
    }
}

fn ntfs_boot(window: &[u8]) -> Option<Decoded> {
    if window[510..512] != [0x55, 0xaa] {
        return None;
    }
    let boot = fs_ntfs::mft_io::parse_boot_sector(window).ok()?;
    Some(Decoded {
        format: "NTFS",
        structure: "boot sector",
        kind: Kind::Boot,
        bytes: SECTOR as u64,
        units: 1,
        signature: true,
        confidence: 0.9,
        volume_length: Some(boot.volume_bytes()),
        detail: format!(
            "NTFS boot sector: {} byte clusters, MFT at cluster {}",
            boot.cluster_size, boot.mft_lcn
        ),
        names: Vec::new(),
        reached_end: false,
    })
}

/// Cheap checks that a record-sized block could be a FILE record, without its magic: the update
/// sequence array is where NTFS puts it, every sector ends with its sequence number, and the
/// first attribute starts where the header says.
fn ntfs_record_shape(block: &[u8]) -> bool {
    let size = block.len();
    let array_offset = usize::from(u16_at(block, 4));
    let array_count = usize::from(u16_at(block, 6));
    let sectors = size / SECTOR;
    if array_count != sectors + 1
        || !(0x28..=0x40).contains(&array_offset)
        || array_offset % 2 != 0
        || array_offset + 2 * array_count > SECTOR - 2
    {
        return false;
    }
    let sequence = &block[array_offset..array_offset + 2];
    if (0..sectors)
        .any(|sector| &block[sector * SECTOR + SECTOR - 2..(sector + 1) * SECTOR] != sequence)
    {
        return false;
    }
    let first_attribute = usize::from(u16_at(block, 20));
    let used = u32_at(block, 24) as usize;
    let allocated = u32_at(block, 28) as usize;
    first_attribute >= array_offset + 2 * array_count
        && first_attribute.is_multiple_of(8)
        && first_attribute < SECTOR
        && used <= size
        && used > first_attribute
        && allocated == size
}

pub(crate) struct RecordRead {
    pub magic: bool,
    pub record_number: Option<u64>,
    pub name: Option<String>,
}

/// Decodes one MFT record-sized block, whether or not its magic survived.
pub(crate) fn ntfs_record(block: &[u8]) -> Option<RecordRead> {
    if !ntfs_record_shape(block) {
        return None;
    }
    let magic = &block[..4] == FILE_MAGIC;
    // A record whose magic was overwritten still decodes if the rest of it holds together, so
    // decode a copy with the magic put back.
    let mut raw = block.to_vec();
    raw[..4].copy_from_slice(FILE_MAGIC);
    let evidence = fs_ntfs::primitives::inspect_file_record(0, raw, SECTOR as u16);
    if evidence.fixed.is_none() || !evidence.issues.is_empty() || evidence.attributes.is_empty() {
        return None;
    }
    let name = evidence
        .file_names()
        .into_iter()
        .find(|name| !name.is_dos_only())
        .map(|name| name.name);
    Some(RecordRead {
        magic,
        record_number: evidence.record_number,
        name,
    })
}

fn ntfs_records(window: &[u8], size: usize, signatures: bool) -> Option<Decoded> {
    if window.len() < size {
        return None;
    }
    // The first record has to decode. Strict mode wants the magic as well.
    let first = ntfs_record(&window[..size])?;
    let mut records = 1u64;
    let mut magic = u64::from(signatures && first.magic);
    let mut names = Vec::new();
    let mut named = first.name.is_some();
    names.extend(first.name.into_iter().take(MAX_NAMES));
    let mut position = size;
    let mut end = size;
    let mut blank = 0;
    let mut first_number = first.record_number;
    while position + size <= window.len() {
        let block = &window[position..position + size];
        if block.iter().all(|byte| *byte == 0) {
            blank += 1;
            if blank > MAX_BLANK_RECORDS {
                break;
            }
            position += size;
            continue;
        }
        let Some(record) = ntfs_record(block) else {
            break;
        };
        blank = 0;
        records += 1;
        magic += u64::from(signatures && record.magic);
        first_number = first_number.or(record.record_number);
        named |= record.name.is_some();
        if names.len() < MAX_NAMES {
            names.extend(record.name);
        }
        position += size;
        end = position;
    }
    let all_magic = magic == records;
    // A lone record with no magic and no name is too little to call a structure.
    if !all_magic && records < 2 && !named {
        return None;
    }
    let value = if all_magic {
        confidence(0.55, 0.06, records, 0.97)
    } else {
        confidence(0.25, 0.06, records, 0.75)
    };
    Some(Decoded {
        format: "NTFS",
        structure: if all_magic {
            "FILE records"
        } else {
            "FILE records with damaged magic"
        },
        kind: Kind::Structures,
        bytes: end as u64,
        units: records,
        signature: all_magic,
        confidence: value,
        volume_length: None,
        detail: match first_number {
            Some(number) => {
                format!("{records} MFT records of {size} bytes, the first numbered {number}")
            }
            None => format!("{records} MFT records of {size} bytes"),
        },
        names,
        reached_end: end + size > window.len(),
    })
}

// ---------------------------------------------------------------- BitLocker

struct BitlockerProbe;

impl FormatProbe for BitlockerProbe {
    fn family(&self) -> &'static str {
        "BitLocker"
    }

    fn decode_at(&self, window: &[u8], signatures: bool) -> Option<Decoded> {
        if !signatures
            || window.len() < SECTOR
            || &window[3..11] != crate::filesystem::BITLOCKER_OEM_ID
            || window[510..512] != [0x55, 0xaa]
        {
            return None;
        }
        Some(Decoded {
            format: "BitLocker",
            structure: "boot sector",
            kind: Kind::Encrypted,
            bytes: SECTOR as u64,
            units: 1,
            signature: true,
            confidence: 0.9,
            volume_length: None,
            detail: "encrypted volume; its files cannot be read".into(),
            names: Vec::new(),
            reached_end: false,
        })
    }
}
