### Fixed

- **A file whose data was cross-linked with another file's is no longer shown as recoverable intact.** When two files on a FAT12, FAT16, FAT32, exFAT or NTFS volume claim some of the same clusters, so that one of them would copy out the other's bytes, both are now marked partial, with the shared clusters counted, instead of live.
