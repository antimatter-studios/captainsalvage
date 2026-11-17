//! Compatibility exports for the driver's permissive exFAT primitives.

pub use fs_exfat::primitives::{
    boot_checksum, decode_timestamp, find_bitmap_entry, find_sets, parse_set, stored_boot_checksum,
    BootSector, EntrySet, Limits, BOOT_REGION_SECTORS, ENTRY_SIZE, FAT_BAD, FAT_END, FIRST_CLUSTER,
};

pub(crate) fn parse_boot_sector(sector: &[u8]) -> anyhow::Result<BootSector> {
    BootSector::parse(sector).map_err(|error| match error {
        fs_exfat::Error::Invalid(
            reason @ ("boot sector is shorter than 512 bytes"
            | "no exFAT signature at offset 3: this is not an exFAT volume"),
        ) => anyhow::anyhow!(reason),
        error => error.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::parse_boot_sector;
    use fs_exfat::primitives::decode_timestamp_with_offset;

    #[test]
    fn short_boot_sectors_keep_the_exact_app_error_before_checking_the_name() {
        for length in 0..512 {
            let mut sector = vec![0; length];
            assert_eq!(
                parse_boot_sector(&sector).unwrap_err().to_string(),
                "boot sector is shorter than 512 bytes",
                "length {length} with no filesystem name"
            );
            if length >= 11 {
                sector[3..11].copy_from_slice(b"EXFAT   ");
                assert_eq!(
                    parse_boot_sector(&sector).unwrap_err().to_string(),
                    "boot sector is shorter than 512 bytes",
                    "length {length} with an exFAT filesystem name"
                );
            }
        }
    }

    #[test]
    fn every_wrong_filesystem_name_keeps_the_exact_app_error() {
        for offset in 3..11 {
            for byte in 0..=u8::MAX {
                let mut sector = [0; 512];
                sector[3..11].copy_from_slice(b"EXFAT   ");
                if sector[offset] == byte {
                    continue;
                }
                sector[offset] = byte;
                assert_eq!(
                    parse_boot_sector(&sector).unwrap_err().to_string(),
                    "no exFAT signature at offset 3: this is not an exFAT volume",
                    "offset {offset}, byte {byte}"
                );
            }
        }
    }

    #[test]
    fn boot_sectors_with_damaged_geometry_and_trailers_remain_accepted() {
        for length in [512, 513, 4096, 6144] {
            for byte in 0..=u8::MAX {
                let mut sector = vec![byte; length];
                sector[3..11].copy_from_slice(b"EXFAT   ");
                let boot = parse_boot_sector(&sector).expect("main accepts raw geometry");
                assert_eq!(boot.file_system_name, "EXFAT   ");
                assert_eq!(boot.partition_offset, u64::from_le_bytes([byte; 8]));
                assert_eq!(boot.volume_length, u64::from_le_bytes([byte; 8]));
                assert_eq!(boot.fat_offset, u32::from_le_bytes([byte; 4]));
                assert_eq!(boot.fat_length, u32::from_le_bytes([byte; 4]));
                assert_eq!(boot.cluster_heap_offset, u32::from_le_bytes([byte; 4]));
                assert_eq!(boot.cluster_count, u32::from_le_bytes([byte; 4]));
                assert_eq!(boot.root_cluster, u32::from_le_bytes([byte; 4]));
                assert_eq!(boot.volume_serial, u32::from_le_bytes([byte; 4]));
                assert_eq!(boot.revision, u16::from_le_bytes([byte; 2]));
                assert_eq!(boot.volume_flags, u16::from_le_bytes([byte; 2]));
                assert_eq!(boot.bytes_per_sector_shift, byte);
                assert_eq!(boot.sectors_per_cluster_shift, byte);
                assert_eq!(boot.number_of_fats, byte);
                assert_eq!(boot.percent_in_use, byte);
                assert!(!boot.signature_ok);
                sector[510..512].copy_from_slice(&[0x55, 0xAA]);
                assert!(parse_boot_sector(&sector).unwrap().signature_ok);
            }
        }
    }

    #[test]
    fn a_utc_offset_is_kept_only_when_marked_valid() {
        // 2024-05-01 12:00:00, as exFAT packs it.
        let time = (44 << 25) | (5 << 21) | (1 << 16) | (12 << 11);
        assert_eq!(
            decode_timestamp_with_offset(time, 0x00).as_deref(),
            Some("2024-05-01T12:00:00")
        );
        // Valid, plus eight steps of 15 minutes: UTC+2.
        assert_eq!(
            decode_timestamp_with_offset(time, 0x88).as_deref(),
            Some("2024-05-01T12:00:00+02:00")
        );
        // Valid, minus 20 steps: UTC-5, written as seven-bit two's complement.
        assert_eq!(
            decode_timestamp_with_offset(time, 0x80 | (128 - 20)).as_deref(),
            Some("2024-05-01T12:00:00-05:00")
        );
        // Valid with no offset is UTC itself.
        assert_eq!(
            decode_timestamp_with_offset(time, 0x80).as_deref(),
            Some("2024-05-01T12:00:00+00:00")
        );
        // India is UTC+5:30, which is 22 steps.
        assert_eq!(
            decode_timestamp_with_offset(time, 0x80 | 22).as_deref(),
            Some("2024-05-01T12:00:00+05:30")
        );
    }
}
