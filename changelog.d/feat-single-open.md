### Added

- **One way to open: Open.** A disk, a partition, a disk image, one extent of a disk and a saved scan all open with the same button. The raw source is always loaded first, and an interpretation is applied to it: the one the last scan of a matching stored source used, otherwise the best detection from the boot evidence, otherwise raw bytes. Opening starts no scan, and what is stored loads at once.
- **An interpretation picker in the header.** `Filesystem: NTFS (detected, 86%)` lists exFAT, FAT, NTFS and no filesystem. Choosing another makes a new scan and says that the earlier scan stays, and choosing the earlier one again opens it without scanning.
- **Saved scans on Open.** An exact match opens its latest scan. A probable match is never applied silently: a question lists the stored sources by confidence with their changed markers, and a scan opens only after you say you read what changed. Matches under 60% are hidden until Show all. Each scan lists its interpretation, date, how much it covers, what it stores, and a name you can edit.
- **A partition and its extent of a whole disk are one source.** Opening a whole disk shows its bands, lanes and extents, each with its own Open. A partition opened alone and the same extent seen from its disk share one interpretation and one set of results, in either order.

### Changed

- **The start screen is a tree.** Each physical disk is a parent row with its own Open, and its partitions are nested under it, each with its filesystem, size, status, Block mount and Open. A disk with no readable partition table is still a parent row. The separate raw chooser, Inspect raw and the image form's Inspect raw are gone: Open does it all.
