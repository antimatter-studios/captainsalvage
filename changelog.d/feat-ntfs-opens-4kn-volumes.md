### Added

- **An NTFS volume on 4096-byte sectors opens.** Windows formats a disk that presents 4096-byte sectors with file records of 1024 bytes, smaller than a sector, and the app refused such a volume at open. It now reads those records, which are protected by one check word for every 512 bytes of the record, and lists and copies the volume's files like any other NTFS volume.
