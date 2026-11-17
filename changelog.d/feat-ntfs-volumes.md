### Added

- **NTFS volumes open, scan and recover in the normal workspace.** A scan reads every cluster for FILE records, so deleted files, old copies of the MFT and files whose folders are gone are found as well as live ones. The folder tree is rebuilt from each record's parent reference. Resident, fragmented, sparse and deleted files are read through the NTFS driver, and can be browsed, previewed, tested, rebuilt, copied and resumed like exFAT files. A saved baseline explains what was written to the volume since, and the volume stays unmounted and unwritten throughout.
- **NTFS says what it cannot do.** Compressed files, EFS-encrypted files and files whose data continues in other records are listed with their size and marked unreadable, never filled with zeros. A BitLocker volume is refused by name after one sector is read. Alternate data streams and extra hard-link names are listed in the record details, not copied as files. The window, `captainsalvage filesystem capabilities` and the new `captainsalvage_capabilities` MCP tool report the same table for every filesystem.
- **A written-over NTFS boot sector can be replaced in memory by its backup.** Choose the copy in the volume's last sector as the boot evidence when you try an NTFS interpretation of a raw disk. The disk's own bytes are never changed.
- **`captainsalvage_ntfs_entry` shows everything an NTFS record says:** each name and the folder it was linked in, each data stream and whether it is resident, compressed, encrypted or sparse, and any damage found in the record.

### Changed

- **NTFS volumes can be opened from the start screen.** They were listed but could not be chosen.
