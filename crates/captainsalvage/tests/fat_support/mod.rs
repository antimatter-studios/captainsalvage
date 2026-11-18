//! What the FAT tests share: small FAT12, FAT16 and FAT32 images built by hand, with a
//! fragmented long-named file, a folder, an orphan and a deleted entry.
#![allow(dead_code)]

use captainsalvage::filesystem::FilesystemKind;

pub struct Fixture {
    pub image: Vec<u8>,
    pub kind: FilesystemKind,
    pub fat_sector: usize,
    pub fat_sectors: usize,
    pub data_sector: usize,
    pub root_sector: usize,
    pub contents: Vec<u8>,
}

impl Fixture {
    pub fn new(kind: FilesystemKind) -> Self {
        Self::with_text(kind, b"Captain Salvage fragmented FAT data.\n")
    }

    /// The same volume with a different text repeated through the fragmented file, so two
    /// volumes of one layout can be told apart by their bytes.
    pub fn with_text(kind: FilesystemKind, text: &[u8]) -> Self {
        let (sectors, fat_sectors, reserved, root_entries, root_sector, data_sector) = match kind {
            FilesystemKind::Fat12 => (4000usize, 12usize, 1usize, 224u16, 25usize, 39usize),
            FilesystemKind::Fat16 => (5000, 20, 1, 512, 41, 73),
            FilesystemKind::Fat32 => (67000, 520, 32, 0, 1072, 1072),
            _ => unreachable!(),
        };
        let mut image = vec![0; sectors * 512];
        image[11..13].copy_from_slice(&512u16.to_le_bytes());
        image[13] = 1;
        image[14..16].copy_from_slice(&(reserved as u16).to_le_bytes());
        image[16] = 2;
        image[17..19].copy_from_slice(&root_entries.to_le_bytes());
        if kind == FilesystemKind::Fat32 {
            image[32..36].copy_from_slice(&(sectors as u32).to_le_bytes());
            image[36..40].copy_from_slice(&(fat_sectors as u32).to_le_bytes());
            image[44..48].copy_from_slice(&2u32.to_le_bytes());
            image[67..71].copy_from_slice(&0xCA00_0032u32.to_le_bytes());
        } else {
            image[19..21].copy_from_slice(&(sectors as u16).to_le_bytes());
            image[22..24].copy_from_slice(&(fat_sectors as u16).to_le_bytes());
            image[39..43].copy_from_slice(&0xCA00_0012u32.to_le_bytes());
        }
        image[510..512].copy_from_slice(&[0x55, 0xAA]);
        let contents = text.iter().copied().cycle().take(700).collect::<Vec<_>>();
        let mut fixture = Self {
            image,
            kind,
            fat_sector: reserved,
            fat_sectors,
            data_sector,
            root_sector,
            contents,
        };
        for cluster in [0, 1, 2, 3, 7, 9] {
            fixture.fat(cluster, u32::MAX);
        }
        fixture.fat(5, 9);
        let mut root = long_entry("Captain log.txt", b"CAPTAI~1TXT", 5, 700);
        root.extend(short_entry(b"ARCHIVE    ", 0x10, 3, 0));
        root.extend([0; 32]);
        root.extend(short_entry(b"\xE5OST    TXT", 0x20, 11, 520));
        fixture.put_sector(root_sector, &root);
        fixture.put_cluster(3, &short_entry(b"CHILD   TXT", 0x20, 7, 11));
        fixture.put_cluster(7, b"child bytes");
        fixture.put_cluster(15, &short_entry(b"ORPHAN  TXT", 0x20, 14, 12));
        fixture.put_cluster(14, b"orphan bytes");
        let first = fixture.contents[..512].to_vec();
        let last = fixture.contents[512..].to_vec();
        fixture.put_cluster(5, &first);
        fixture.put_cluster(9, &last);
        fixture.put_cluster(11, &[b'D'; 512]);
        fixture.put_cluster(12, &[b'E'; 8]);
        fixture
    }

    pub fn fat(&mut self, cluster: u32, value: u32) {
        for table in 0..2 {
            let base = (self.fat_sector + table * self.fat_sectors) * 512;
            let cluster = cluster as usize;
            match self.kind {
                FilesystemKind::Fat12 => {
                    let offset = base + cluster * 3 / 2;
                    let old =
                        u16::from_le_bytes(self.image[offset..offset + 2].try_into().unwrap());
                    let value = value as u16 & 0xFFF;
                    let word = if cluster.is_multiple_of(2) {
                        old & 0xF000 | value
                    } else {
                        old & 0xF | value << 4
                    };
                    self.image[offset..offset + 2].copy_from_slice(&word.to_le_bytes());
                }
                FilesystemKind::Fat16 => self.image[base + cluster * 2..base + cluster * 2 + 2]
                    .copy_from_slice(&(value as u16).to_le_bytes()),
                FilesystemKind::Fat32 => self.image[base + cluster * 4..base + cluster * 4 + 4]
                    .copy_from_slice(&value.to_le_bytes()),
                _ => unreachable!(),
            }
        }
    }

    pub fn put_sector(&mut self, sector: usize, bytes: &[u8]) {
        self.image[sector * 512..sector * 512 + bytes.len()].copy_from_slice(bytes);
    }

    pub fn put_cluster(&mut self, cluster: usize, bytes: &[u8]) {
        self.put_sector(self.data_sector + cluster - 2, bytes);
    }
}

pub fn short_entry(name: &[u8; 11], attributes: u8, cluster: u16, size: u32) -> Vec<u8> {
    let mut entry = vec![0; 32];
    entry[..11].copy_from_slice(name);
    entry[11] = attributes;
    entry[26..28].copy_from_slice(&cluster.to_le_bytes());
    entry[28..32].copy_from_slice(&size.to_le_bytes());
    entry
}

pub fn long_entry(name: &str, short: &[u8; 11], cluster: u16, size: u32) -> Vec<u8> {
    let checksum = short
        .iter()
        .fold(0u8, |sum, byte| sum.rotate_right(1).wrapping_add(*byte));
    let words = name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let count = words.len().div_ceil(13);
    let positions = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
    let mut entries = Vec::new();
    for sequence in (1..=count).rev() {
        let mut entry = [0; 32];
        entry[0] = sequence as u8 | if sequence == count { 0x40 } else { 0 };
        entry[11] = 0x0F;
        entry[13] = checksum;
        for (index, position) in positions.iter().enumerate() {
            entry[*position..*position + 2].copy_from_slice(
                &words
                    .get((sequence - 1) * 13 + index)
                    .copied()
                    .unwrap_or(0xFFFF)
                    .to_le_bytes(),
            );
        }
        entries.extend(entry);
    }
    entries.extend(short_entry(short, 0x20, cluster, size));
    entries
}
