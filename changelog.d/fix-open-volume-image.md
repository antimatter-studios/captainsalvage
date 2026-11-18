### Fixed

- **An image of one small volume opens its filesystem again.** A file that starts with a volume's boot sector, such as a small exFAT, FAT or NTFS partition image, opened as raw bytes with nothing to browse since the single Open arrived. It now opens its filesystem straight away, as before, with the interpretation picker and saved scans. A source that does not start with a volume, such as a whole disk image with a partition table, still opens as raw bytes with its extents to Open.
