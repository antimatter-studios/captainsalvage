use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    pub offset: u64,
    pub length: u64,
    pub format: String,
    pub structure: String,
    pub confidence: String,
    pub volume_start: Option<u64>,
    pub volume_length: Option<u64>,
    pub boot_offset: Option<u64>,
    pub supported: bool,
}

pub fn inspect(data: &[u8], offset: u64) -> Vec<Evidence> {
    let mut found = Vec::new();
    if offset.is_multiple_of(512) && data.len() >= 512 {
        if let Ok(boot) = fs_exfat::boot::BootSector::parse(data) {
            let length = boot.volume_length * boot.bytes_per_sector();
            found.push(boot_evidence(offset, length, "exFAT", true));
            if let Some(start) = offset.checked_sub(12 * boot.bytes_per_sector()) {
                let mut backup = boot_evidence(offset, length, "exFAT", true);
                backup.structure = "possible backup boot sector".into();
                backup.volume_start = Some(start);
                found.push(backup);
            }
        } else if let Ok(boot) = fs_fat32::BootSector::parse(data) {
            let format = match boot.kind {
                fs_fat32::FatKind::Fat12 => "FAT12",
                fs_fat32::FatKind::Fat16 => "FAT16",
                fs_fat32::FatKind::Fat32 => "FAT32",
            };
            found.push(boot_evidence(
                offset,
                u64::from(boot.total_sectors) * u64::from(boot.bytes_per_sector),
                format,
                true,
            ));
        }
        if &data[3..11] == b"NTFS    " && data[510..512] == [0x55, 0xaa] {
            let sector = u16::from_le_bytes(data[11..13].try_into().unwrap());
            let sectors = u64::from_le_bytes(data[40..48].try_into().unwrap());
            if matches!(sector, 512 | 1024 | 2048 | 4096)
                && data[13].is_power_of_two()
                && sectors > 0
            {
                if let Some(length) = sectors.checked_mul(u64::from(sector)) {
                    found.push(boot_evidence(offset, length, "NTFS", false));
                }
            }
        }
    }
    if matches!(data.first(), Some(0x85 | 0x05)) {
        let limits = fs_exfat::primitives::Limits {
            cluster_count: u32::MAX - 2,
            cluster_size: 1,
        };
        if let Some(entry) = fs_exfat::primitives::parse_set(data, 0, limits) {
            if entry.checksum_ok {
                found.push(Evidence {
                    offset,
                    length: (entry.slots * 32) as u64,
                    format: "exFAT".into(),
                    structure: "checksummed directory entry set".into(),
                    confidence: "structure only; volume geometry unknown".into(),
                    volume_start: None,
                    volume_length: None,
                    boot_offset: None,
                    supported: true,
                });
            }
        }
    }
    found
}

fn boot_evidence(offset: u64, length: u64, format: &str, supported: bool) -> Evidence {
    Evidence {
        offset,
        length: 512,
        format: format.into(),
        structure: "boot sector".into(),
        confidence: "plausible geometry; filesystem not yet opened".into(),
        volume_start: Some(offset),
        volume_length: Some(length),
        boot_offset: Some(offset),
        supported,
    }
}
