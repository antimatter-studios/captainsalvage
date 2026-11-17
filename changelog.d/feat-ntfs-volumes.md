### Added

- **NTFS volumes open, scan and recover in the normal workspace.** A scan reads every cluster for FILE records, so deleted files, old copies of the MFT and files whose folders are gone are found as well as the live ones. The folder tree is rebuilt from each record's parent reference. Resident files, fragmented files and deleted files with untouched clusters are read through the NTFS driver and can be browsed, previewed, tested, rebuilt, copied and resumed like exFAT files, with the volume mounted nowhere and nothing written to it.
