//! Compatibility exports for the driver's permissive exFAT primitives.

pub use fs_exfat::primitives::{
    boot_checksum, decode_timestamp, find_bitmap_entry, find_sets, parse_set, stored_boot_checksum,
    BootSector, EntrySet, Limits, BOOT_REGION_SECTORS, ENTRY_SIZE, FAT_BAD, FAT_END, FIRST_CLUSTER,
};

pub(crate) fn parse_boot_sector(sector: &[u8]) -> anyhow::Result<BootSector> {
    BootSector::parse(sector).map_err(|error| match error {
        fs_exfat::Error::Invalid(reason) => anyhow::anyhow!(reason),
        error => error.into(),
    })
}

#[cfg(test)]
mod tests {
    use fs_exfat::primitives::decode_timestamp_with_offset;

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
