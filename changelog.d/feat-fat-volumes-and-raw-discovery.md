### Added

- **FAT12, FAT16 and FAT32 volumes open and scan.** They go through the same filesystem adapter as exFAT, so directories, fragmented files, deleted records, saved baselines, scan resumption and recovery use the same workflow. Volume details are shown without the exFAT-only checksum claims. FAT support is validated with synthetic images.
- **NTFS records can be inspected and extracted from the command line and MCP.** Surviving FILE records are read without a working boot sector or namespace, and resident and complete uncompressed non-resident streams can be extracted. Damaged fixups and unresolved or unsupported mappings are reported, never silently repaired.
- **Disks, partitions and images open without a recognised filesystem.** They use the normal workspace and disk graph, working on byte ranges. Declared partitions and overlapping filesystem evidence are separate layers, you can select and label regions, and searching continues after a format is found. FAT, exFAT and NTFS boot evidence is probed, and surviving exFAT backup boot metadata can be used in memory beneath a newer NTFS boot sector without changing the source. The Source bytes tab returns to the underlying source and its findings. The command line and MCP offer the same operations.
- **Raw discovery scans show throughput and time remaining, and can be resumed.** Each completed batch saves its findings and progress under a stable scan ID. A reopened disk, clone or image can load a matching scan after you acknowledge sampled verification. Mismatches and sources that cannot be verified cannot resume, and regions that were unreadable are retried on a new source. Scans in memory from older builds are lost when those builds close.

### Fixed

- **A live file is no longer marked partially overwritten because of its own clusters.** Copying a live file does not count the allocation of its own clusters as damage.

### Changed

- **The engine package, library and directory are named `captainsalvage`.** The terminal agent tool `salvage-agent` is now `captainsalvage agent`.
