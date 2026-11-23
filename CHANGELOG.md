# Changelog

Each release lists what changed in it. The same notes appear on the
[releases page](https://github.com/antimatter-studios/captainsalvage/releases).

Changes not yet released are fragments in [`changelog.d/`](changelog.d/), one per pull request.
`scripts/create-release` gathers them into a section here.

## [Unreleased]

## [0.27.1] — 2026-11-23

### Maintenance

- The exFAT driver is pinned to rust-fs-exfat main with its three shared layers (eb1c1f7: #244, #245) ([#396](https://github.com/antimatter-studios/captainsalvage/pull/396))

## [0.27.0] — 2026-11-23

### Added

- **An NTFS volume on 4096-byte sectors opens.** Windows formats a disk that presents 4096-byte sectors with file records of 1024 bytes, smaller than a sector, and the app refused such a volume at open. It now reads those records, which are protected by one check word for every 512 bytes of the record, and lists and copies the volume's files like any other NTFS volume.

### Maintenance

- The exFAT driver is pinned to rust-fs-exfat main on rust-fs-core 0.5.1, with the salvage adapter from core (671fa20: #241, #242, #243) ([#394](https://github.com/antimatter-studios/captainsalvage/pull/394))
- The app reads ext4 images with small files stored in their inodes, with the older feature set and with linear directories exactly as the Linux kernel lists them ([#393](https://github.com/antimatter-studios/captainsalvage/pull/393))
- The NTFS driver is pinned to rust-fs-ntfs main that reads Windows 4Kn volumes with 1 KiB records (3068689: #1406, #1407, #1408) ([#391](https://github.com/antimatter-studios/captainsalvage/pull/391))
- The NTFS driver is pinned to rust-fs-ntfs main with stream salvage from salvaged records (6be01d9: #1404, #1405) ([#389](https://github.com/antimatter-studios/captainsalvage/pull/389))
- The NTFS driver is pinned to rust-fs-ntfs main with its three layers and salvaged-record decoding (826f6c0: #1376, #1377, #1380) ([#388](https://github.com/antimatter-studios/captainsalvage/pull/388))
- The app opens, scans, browses, reads and copies EROFS images made by mkfs.erofs exactly as the Linux kernel lists them, with the same SHA-256 for every file and a link as a file that holds its target ([#387](https://github.com/antimatter-studios/captainsalvage/pull/387))
- Adopt harness v0.11.0 resource limits ([#386](https://github.com/antimatter-studios/captainsalvage/pull/386))

## [0.26.0] — 2026-11-22

### Added

- **An EROFS image opens, scans and shows its files.** The app reads it through the filesystem's own driver, as it does SquashFS and ext2/3/4: it finds the image by its superblock (at byte 1024), keeps the superblock so the image still opens if the disk's own is written over, lists its folders and files, previews a file and copies it out. The support table has an EROFS column, and the instructions an MCP client is given name it.

### Maintenance

- Add the Captain Salvage constellation overview and repository roster ([#384](https://github.com/antimatter-studios/captainsalvage/pull/384))
- The engine and window job is four jobs that run side by side, each well under the time the runner lets a job live ([#382](https://github.com/antimatter-studios/captainsalvage/pull/382))
- The app opens, scans, browses, reads and copies ext2, ext3 and ext4 images made by mke2fs exactly as the Linux kernel lists them, with the same SHA-256 for every file and a link as a file that holds its target ([#380](https://github.com/antimatter-studios/captainsalvage/pull/380))

## [0.25.0] — 2026-11-22

### Added

- **An ext2, ext3 or ext4 volume opens and says what it is.** The app reads it through the filesystem's own driver, as it does SquashFS: it finds the volume by its superblock (at byte 1024, past the first sector), keeps the superblock so the volume still opens if the disk's own is written over, and shows its block size, label and serial number. The support table has an ext2/3/4 column, and the instructions an MCP client is given name it.

### Maintenance

- The place-blocking test starts from its own empty folder, so it no longer sees a place flagged by the test before it ([#377](https://github.com/antimatter-studios/captainsalvage/pull/377))

## [0.24.0] — 2026-11-22

### Added

- **An exFAT volume with 1024, 2048 or 4096-byte sectors opens.** It was refused with `unsupported sector size 4096`, which left drives with 4096-byte sectors (4Kn disks, some SD cards and flash media) formatted exFAT showing nothing. The app now lists their files and folders and copies them out, with the same SHA-256 as the Linux kernel reports for a volume `mkfs.exfat` made on a 4096-byte-sector device.

## [0.23.4] — 2026-11-22

### Maintenance

- One damaged compressed data block of a kernel-made SquashFS image costs only that file: every entry is listed, its copy is partial and names the block, the damage report names the data block, and every other file copies with the kernel's SHA-256 ([#373](https://github.com/antimatter-studios/captainsalvage/pull/373))
- SquashFS is pinned to rust-fs-squashfs main (d17048f), which keeps the earlier layer APIs with deprecation notices and reports damage to file data blocks ([#372](https://github.com/antimatter-studios/captainsalvage/pull/372))
- A Disk Rebuild gathers, names and copies out the files of a damaged SquashFS image that lost its directory table, and a driver candidate keeps the record number it was given through a rescan ([#371](https://github.com/antimatter-studios/captainsalvage/pull/371))

## [0.23.3] — 2026-11-21

### Fixed

- **The instructions an MCP client is given name SquashFS.** They said the app reads a damaged exFAT, FAT or NTFS volume and left SquashFS out, so an agent was told the app could not open an image it does open. They now name every filesystem the app reads.

## [0.23.2] — 2026-11-21

### Fixed

- **A damaged or truncated SquashFS image now says what is wrong when you open it.** A copy cut short, or one with a zeroed inode or directory table header, used to open as an empty volume with no damage reported. It now lists what it found, and a truncated copy still shows its files.

## [0.23.1] — 2026-11-21

### Maintenance

- A driver's entries have typed ranges, built from the walk its driver made: readable, encoded, sparse and missing, with holes between ([#365](https://github.com/antimatter-studios/captainsalvage/pull/365))

## [0.23.0] — 2026-11-21

### Added

- **A SquashFS image's symbolic links and special files are listed.** A symbolic link shows as a file that holds its target, so it previews and copies out as that text, and a device or other special file shows as an empty file. The support table's browse note says so.

### Maintenance

- The map's table and structs of records are named for what they hold, not for NTFS, and a driver's registry entry gives its volume's root number, so the record path is the same for every filesystem that has records ([#362](https://github.com/antimatter-studios/captainsalvage/pull/362))

## [0.22.0] — 2026-11-21

### Added

- **A SquashFS image opens, scans and shows its files.** The app reads it through the filesystem's own driver: its folders and files are listed, a file is previewed from any place in it, and a copy writes the bytes the driver decodes (every compressor SquashFS has: gzip, xz, lzo, lz4 and zstd), with each copy hashed and marked complete or partial as for the other filesystems. Symbolic links and special files are not listed yet, and no allocation is read from the image, so its map shows every cluster as unknown rather than free. The support table has a SquashFS column.

## [0.21.0] — 2026-11-21

### Added

- **A SquashFS image now opens instead of being refused.** The app says what it is, from its driver (its block size, label and serial number), keeps its superblock as the baseline, and shows a SquashFS column in the support table. It does not list the image's files yet.

## [0.20.6] — 2026-11-21

### Maintenance

- SquashFS images are listed and copied through the generic layers adapter, and a kernel-made image of each compressor is checked against the kernel's own listing ([#356](https://github.com/antimatter-studios/captainsalvage/pull/356))

## [0.20.5] — 2026-11-21

### Maintenance

- SquashFS is pinned to rust-fs-squashfs's three layers (ef405d4), which brings rust-fs-core 0.5.1, and the generic adapter names a candidate by the driver's own id ([#354](https://github.com/antimatter-studios/captainsalvage/pull/354))

## [0.20.4] — 2026-11-21

### Maintenance

- The copy of an exFAT file whose valid length is short writes zeros past it, and an unreadable cluster there does not make the copy partial ([#352](https://github.com/antimatter-studios/captainsalvage/pull/352))

## [0.20.3] — 2026-11-21

### Maintenance

- The copy of a FAT or exFAT file reads its typed ranges, as its preview does, instead of its cluster list ([#350](https://github.com/antimatter-studios/captainsalvage/pull/350))

## [0.20.2] — 2026-11-21

### Maintenance

- A saved copy of a volume's structures says where on the volume each region was read from, for FAT, exFAT and NTFS ([#348](https://github.com/antimatter-studios/captainsalvage/pull/348))

## [0.20.1] — 2026-11-21

### Maintenance

- The FAT driver is pinned to its next-free-clusters guess for a deleted file (3bc18be), and a deleted file written around a live one is copied byte for byte ([#345](https://github.com/antimatter-studios/captainsalvage/pull/345))

## [0.20.0] — 2026-11-21

### Added

- **A deleted FAT file is now read and copied whole, marked as a guess, where it used to come out as its first cluster alone.** When a FAT file is deleted, the FAT clears its chain, so only its first cluster is known. The FAT driver's walk of what the file's entry claims follows it as one run from that cluster, and that is now what the map, the verdicts, the copy and the file tests use for FAT files, instead of the app following the chain itself. A deleted file is copied in full by default and no longer skipped as partial, its verdict says its clusters are a guess, with the lower confidence a guess gets, and its copy is marked in the manifest (`extentsGuessed`) and counted in the export window. Files the FAT chains are read as before. The earlier FAT adapter stays for one release: set `SALVAGE_FAT_ADAPTER=legacy`.

## [0.19.0] — 2026-11-21

### Added

- **A drive you opened before is recognised as the same drive, so its scan resumes at once.** This holds even when its blocks read differently now or the drive was opened carefully after a reset. A mount that rewrote blocks still marks the regions around them as possibly out of date.
- **A drive is recognised by its model and serial number when the system reports one.** A damaged drive whose first sector cannot be read is still the same drive, and a copy with another serial number is not.
- **When a saved scan cannot be matched to the drive, you are asked.** You can choose Resume anyway or open the drive as a new source. This includes a drive that cannot be recognised at all, for which the saved scans of the same size are offered. A scan is never started again silently.

## [0.18.1] — 2026-11-21

### Fixed

- **Resuming a scan on the same damaged drive now continues from its checkpoint.** You avoid starting over at a block that still cannot be read.

## [0.18.0] — 2026-11-21

### Added

- **FAT volumes are read through the FAT driver's shared layers, and entries it is unsure of are marked as weak candidates.** FAT12, FAT16 and FAT32 entries now come from the driver's own scan instead of the app's earlier reading of every cluster, which finds the same entries. Two things differ, on purpose. An entry in a folder cluster that is free in the FAT is shown as deleted (its data may have been overwritten), where it used to be shown as in use because its own first byte said so. And an entry the driver found by its content only, not through a folder that names it (a lone entry in a cluster nothing points to, or file bytes that happen to look like an entry), is marked as a weak candidate, in the window and in the tools, with half the confidence, and says so in its reason. The overview also tells of damage the driver finds in the FAT's own structures. The earlier FAT adapter stays for one release: set `SALVAGE_FAT_ADAPTER=legacy` to read with it. It also reads a volume the driver does not open, and the overview says so.

## [0.17.3] — 2026-11-21

### Maintenance

- The FAT driver is pinned to rust-fs-fat's content discovery of orphaned directory clusters and lone entries (2272fa2), and the generic-versus-app FAT comparison asserts what it now finds ([#335](https://github.com/antimatter-studios/captainsalvage/pull/335))

## [0.17.2] — 2026-11-21

### Maintenance

- One generic adapter over rust-fs-core's layers, proven on FAT beside the FAT adapter the app uses, with the support rows derived from what a driver says it lacks ([#333](https://github.com/antimatter-studios/captainsalvage/pull/333))

## [0.17.1] — 2026-11-21

### Maintenance

- The FAT driver is pinned to rust-fs-fat's merged three layers (19f4a93), which brings rust-fs-core 0.5.0, and the adapter's devices read through both generations of it ([#331](https://github.com/antimatter-studios/captainsalvage/pull/331))

## [0.17.0] — 2026-11-21

### Added

- **On an NTFS volume whose allocation is unknown, carving and the unexplained-space tools still work, and say what they are searching.** When a volume's `$Bitmap` cannot be read there is no list of free clusters, so these tools used to find nothing. They now search the whole volume, as if it were free space, and say so in every answer: the Unexplained list in the window shows a warning, and the `unexplained_runs`, `unexplained_summary` and `carve_candidates` answers carry `allocationBasis` and `allocationWarning`. Nothing is called free: the warning says that nothing here is known to be free, and that a file current files use may be listed. The answers for a volume whose allocation is known are unchanged. The search is built to narrow to what the file records leave over once allocation can be rebuilt from them, with that rebuild's limits said alongside.

## [0.16.1] — 2026-11-21

### Maintenance

- The pull request jobs install ffmpeg so the tests that need it run, and the path without ffmpeg is a test of its own that hides ffmpeg itself ([#327](https://github.com/antimatter-studios/captainsalvage/pull/327))

## [0.16.0] — 2026-11-21

### Added

- **An NTFS volume whose `$Bitmap` cannot be read now opens, with its allocation unknown.** It used to be refused ("`$Bitmap has no data stream`"), although the files are found and read through `$MFT` and only the knowledge of which clusters are free is lost. Now the volume opens, the overview (the window and the tools) says "Allocation unknown" and why, and every cluster is drawn in a state of its own on the map, which is neither free nor in use. No cluster is called free: unexplained-space totals do not count unknown clusters as free (they are counted apart, so they are not hidden), and a deleted file's verdict says that whether its clusters are in use now is not known. Files are listed, read and copied byte for byte as in the undamaged volume. The saved baseline now says which of its parts could not be read.
- **A damaged NTFS system record is reported, not hidden.** A `$MFT` record 0, `$Volume` record or root directory record that fails its update-sequence check is now named in the overview, with what it means: for `$MFT` record 0, that files in later parts of `$MFT` may be missing or listed as damaged. It used to show only as one more damaged file, with no message. `$MFTMirr` is not consulted yet.

## [0.15.6] — 2026-11-21

### Fixed

- **Block mount works for an NTFS volume that macOS gives no volume UUID.** The mount block named a volume by its UUID, so a volume macOS reports none for (a synthetic NTFS image, a disk image) got no Block mount checkbox and nothing kept it unmounted. Such a volume is now blocked by where it is: the identity of the disk it is on (the device's own GUID, or for a disk with none a fingerprint of the first 4 KiB) with the partition's offset and length. The checkbox appears for it, a block set while it is mounted shows the Unmount warning, and macOS is refused the mount, also when the disk is attached again under another disk number, while a different disk of the same size mounts as usual. A volume that has a UUID, which a real NTFS volume on a drive does, is blocked by it as before. The support table now says NTFS mount blocking is limited, with the reason; an agent names a volume without a UUID by its device and the key `startup_disks` gives it.

## [0.15.5] — 2026-11-21

### Fixed

- **An NTFS volume with more clusters than Windows allows is refused at open, with the reason.** Windows itself does not create or mount an NTFS volume of more than 4,294,967,294 (2^32 - 2) clusters, and the limit here was one cluster too generous and its message did not say why. A boot sector that states more clusters is now refused as soon as it is read, with a message that gives the count and says Windows does not make such a volume, and nothing else on the disk is read. A volume of exactly 4,294,967,294 clusters still opens. The support table says NTFS is limited to that many clusters.

## [0.15.4] — 2026-11-21

### Maintenance

- Gated Linux jobs install missing tools so release publishing works on self-hosted runners ([#319](https://github.com/antimatter-studios/captainsalvage/pull/319))

## [0.15.3] — 2026-11-21

### Maintenance

- A raw scan step through the engine's tools reads a failing drive through the journal without waiting for itself ([#317](https://github.com/antimatter-studios/captainsalvage/pull/317))

## [0.15.2] — 2026-11-20

### Fixed

- **Opening a source no longer fails with "database is locked" when the catalog is busy for a moment.** The health records of a source (its link to the disk, its reset count and its map) are written again after a short lock clears, instead of the open failing or the record being lost. A read of the device still never waits on the catalog.

### Maintenance

- Every job takes its runner from a gate job on the runner gate ([#312](https://github.com/antimatter-studios/captainsalvage/pull/312))

## [0.15.1] — 2026-11-20

### Fixed

- **A raw scan of a drive that has reset no longer freezes the app.** A raw scan step held a write lock on the catalog while it read the disk, and every read it journaled waited on that same lock, half a minute at a time, while holding what every status call needs: the scan sat at 0.0% and the window answered nothing. A step now reads the disk without the lock and takes it only to write; the read journal has a file of its own and gives up after a moment, keeping a write it cannot make in memory until the file is free, so no read is left open to be blamed for a reset.

## [0.15.0] — 2026-11-20

### Added

- **Bad sectors are a kind of extent, `bad_sector`, like the others.** The ranges the health map marks as bad (suspect, unreadable or reset-culprit) are now extents of the source with that kind, kept in the catalog under the source, so they are there when it is opened again and when the drive comes back under another name. The new `list_extents` tool lists them with the source's other extents (filter by kind or range), and `open_source`, `list_hypotheses`, `read_layout` and `analyze_range` answer with the ones that matter to them (`badSectors`). A file whose bytes lie in a bad sector now has them as `BadSector` in its typed ranges: a preview shows them as `?`, and a copy writes zeros there and names the ranges as `badSectorRanges` in its manifest, apart from the bytes that were tried and failed.
- **Free space nobody can read is no longer unexplained.** A free cluster in a bad sector counts as explained by bad sectors, as a region label would explain it, in the cluster map (a new legend entry, in red), in the region counts and in the unexplained totals and runs; the map summary says how many (`badSectorFreeClusters`). It shows after the map is rebuilt.
- **A Disk Rebuild flags what crosses a bad sector, and blocks nothing.** A file, a folder (by everything below it) or a reassembly with bytes in a bad sector shows a flashing red flag with the bytes affected in the tree of the Rebuild tab, and the copy plan counts the files and the bytes. The copy goes ahead, with zeros there.
- **A drive with a known bad zone is still an exact match.** A saved marker that lies in a bad sector is not read and says nothing either way, so it is left out of what the match is counted over: when everything that can be read matches, the source is recognised exactly, and the comparison says how many markers lay in bad sectors.

## [0.14.1] — 2026-11-20

### Added

- **A zone found outside the probe can be marked.** `probe_health` has a `mark_bad` action, and the raw-disk view a "Mark bad sectors" form, to record a zone found some other way, such as by reading the drive with `dd`: its first and last byte, what happened (the drive reset, a read failed, or keep out) and why. It is kept with who marked it and when, and nothing reads it afterwards.
- **A disk that has reset is opened carefully.** Once a disk has a zone where the drive reset, a zone a probe jumped over, or has reset twice, opening it does not read fresh markers across the whole disk and does not run a range analysis: it is recognised from at most five of its stored markers that lie where earlier reads went well, and the window says so. Analysing a range stays the person's to ask for.
- **The bad zones and the reset count belong to the physical disk.** A whole disk and its partitions share one map, one reset count and one failing flag, so a zone found through a partition is avoided from the whole disk and the other way round, and a drive that reset once through each is failing. A GPT disk's own GUID is part of its key, so two disks of one model are two disks. A read that failed because the device had already gone no longer marks the place it was asked for as the cause: only the read in flight when the device went away can.

### Fixed

- **An error under the source header is readable.** An error such as "The disk has been disconnected; scan not started." was squeezed into the narrow column of the progress bar beside "Saved scans", one or two words to a line, and ran off the top of the window. It now has a line of its own under the title row, the whole width of the window, and a very long one wraps and scrolls inside itself.

## [0.14.0] — 2026-11-20

### Added

- **Bad sectors are drawn as flashing red, everywhere the window draws the disk.** Ranges the health map calls reset-culprit (the drive reset during a read there), unreadable (a read there failed) or suspect (inside a band the damage probe jumped over) are drawn in red that pulses once a second: on the raw-disk map (over every row, and in the legend as "Bad sectors: reading here drops the drive"), on the volume map (the bad clusters, on both the whole-volume and the magnified strip), and in a preview, where the `?` for each byte that could not be read is red and pulsing. Resting the pointer on one says where it is, how it was found (the drive reset during a read, a read failed, or a band the probe jumped over) and when. Slow ranges stay amber and do not flash. For people who ask macOS for reduced motion nothing flashes: the red is solid, with a diagonal hatch. The new `bad_sectors` tool gives the volume's bad clusters. The strings are translated into German, Spanish and French.

## [0.13.0] — 2026-11-20

### Added

- **The damage probe jumps over a bad band instead of walking into it, and maps its edges only when you say.** On a drive whose bad area is a wide band with readable islands, every read inside it can reset the drive. `probe_health` now marks a band as soon as a grid read fails, is slow or resets the drive, jumps 16 GiB (`skipBytes`) before it reads again, and counts a read that is far slower than the running median, near the band, as part of it. The whole grid is read first, and the band between the last clean read before it and the first clean read after it is marked suspect: nothing reads it, scans report it as read errors, previews mark it, copies write zeros there and name it, until you forgive it. Mapping the edges of the bands costs resets, so it waits for `action: edges`, and each band reports how many resets it has cost.
- **A drive that keeps resetting is called failing.** The app counts how many times the device went away while a source was read. After two (`failingAfterResets`) the probe pauses and says so, and the source is read carefully from then on: every read journaled, none longer than 64 KiB, suspect bands never read. `health_map` shows the count, and only the person clears it.

## [0.12.0] — 2026-11-20

### Added

- **The app finds the bad zones of a failing drive without resetting it again and again, and then never reads them.** A drive that retries inside a bad area for seconds makes a USB bridge reset the whole device, and each reset hurts a damaged drive further. The app now writes a journal of the reads that can reach an unknown bad spot (opening a source, probing, any large read) before each starts, so a read that never finished, because the device went away during it, is marked as the likely cause the next time the drive is opened, before anything reads it. The new `probe_health` tool finds the zones on purpose and gently, bounded and resumable: it reads one block a GiB, then walks toward each bad block from the good side on both sides and stops at the first slow or failed read, so a zone is bounded without reading into it. `health_map` shows what is known, and the window draws it as a lane of the raw-disk map. Ranges marked unreadable or as a reset are not read again by anything: scans report them as read errors, previews mark them, copies write zeros there and name them in the manifest, and the markers that recognise the drive are taken from outside them.
- **A drive that went away and came back is the same source.** The app lets go of the vanished device node as soon as it finds the device gone, and `rebind_source` (or opening the new path) points the open source at the drive when it returns, found again by its markers, so the open volume and its map read again.

## [0.11.1] — 2026-11-20

### Maintenance

- The coverage baseline is written from CI's numbers, and `baseline` refuses to measure outside CI ([#298](https://github.com/antimatter-studios/captainsalvage/pull/298))

## [0.11.0] — 2026-11-20

### Added

- **A bridge can be verified by a decoder, and a verified one can be kept.** The `media_check` tool (which has ffmpeg read a file and reports its exact errors) and the `materialize` tool now take a reassembly the way `test_stream` does: an entry (or clusters) plus `appendExtents`, `appendChain` or `appendChains`. ffmpeg then reads the clusters in that order as one file, every byte from the disk (a cluster that cannot be read fails the check, it is not filled in), and the answer says what was joined where. A new `disk_rebuild_place_reassembly` tool keeps one: it completes the file with the appended clusters as a hypothesis the map then reads (which `delete_overlay` undoes) and places the file in a Disk Rebuild, so a copy of the Disk Rebuild holds it whole.

## [0.10.0] — 2026-11-20

### Added

- **An agent can summarise all the unexplained space in one call.** The new `unexplained_summary` tool samples the free clusters that nothing explains, across every region (or the biggest N), within a sample budget (2048 clusters by default) and a time limit, and gives the totals in one answer: how many regions and bytes, bytes by main content, and what the sample holds (clusters with valid directory entries, clusters that mix older and newer data, file signatures at a cluster's start and inside it, NTFS records, zeros, read errors), as counts, shares and estimates. For clusters that hold valid directory entries and are still unexplained it also says what the scan made of them (for example, it could not read them) and whether the scan had got that far. It gives no names or text unless asked, and an agent is refused them while the person keeps names back.

## [0.9.0] — 2026-11-20

### Added

- **An agent can test a bridge in one call.** The `test_stream` tool takes `appendChain: {head}` (and `appendChains: [{head}, ...]`) and follows the chain in the FAT itself, so a chain from `bridge_candidates` can be tested without walking it first and passing its extents; with several chains the answer has a `joins` entry for each. `bridge_candidates` takes `test: true` and then tests each of its first candidates as the file's continuation (5 by default, at most 20, within a read budget of 64 MiB unless `testBudgetBytes` says otherwise) and gives each row the verdict: whether the stream crosses the join cleanly and how much of it is valid.

## [0.8.4] — 2026-11-20

### Fixed

- **A bridge test no longer calls a join "clean" when the data is not valid on either side.** The `test_stream` tool (used by agents to test whether a chain continues a broken file) reported `clean` for a join whenever no break happened to fall beside it, even when the stream was not a valid file at all: on a real volume, a file whose data walked as MP3 for 0.0016% of its bytes, with a chain appended, came back `clean`. A clean crossing now needs valid structure running unbroken across the join, a few frames or chunks either side. The other answers are `breaks` (valid on at least one side, not through the join), `invalid-both-sides` (nothing valid there, so the join proves nothing) and `unknown`.

## [0.8.3] — 2026-11-19

### Fixed

- **A Disk Rebuild made from the old treasure list no longer carries the retired name.** Version 0.4.0 turned an old treasure list into a Disk Rebuild called "Imported treasure list", with a folder of the same name inside it, and those names are stored in the map, so they kept showing after the treasure list was gone. When a map is opened, that Disk Rebuild and its folder are renamed once to "Files picked before 0.4" (with " (2)", " (3)" and so on when another Disk Rebuild already has that name). Only exactly that name is changed, and only in a map that had the old list imported into it, so a name you chose is never touched. A copy of the map is made first, next to it, named `<map>.before-retired-name`; the change is checked and undone if the check fails. Nothing else in a map or in the saved scans carries a retired word.

## [0.8.2] — 2026-11-19

### Maintenance

- The preview of a FAT or exFAT file reads the file's typed ranges, not its cluster list ([#284](https://github.com/antimatter-studios/captainsalvage/pull/284))

## [0.8.1] — 2026-11-19

### Maintenance

- The changelog hook lets the integration branch of a release commit the release tool's CHANGELOG.md, so a candidate can be cut from it (#252) ([#282](https://github.com/antimatter-studios/captainsalvage/pull/282))

## [0.8.0] — 2026-11-19

### Added

- **Files that NTFS compressed can be previewed, tested and copied.** On an NTFS volume, a file that Windows compressed (a compressed folder, or `compact /c`) was listed with its size and then refused: it could not be previewed, it was left out of a copy, and it showed as partial. The app now reads such a file: it works out which clusters hold each 64 KiB compression unit and has the NTFS driver decode them. A unit that does not decode is marked in the preview as unreadable bytes, and the copy says so and is partial; nothing is made up. A file that is compressed in another way, or whose data continues in another record, is still listed and refused, as before. This is checked on synthetic volumes laid out the way NTFS lays out compressed files, and on volumes that Windows formatted and compressed files on, where every file has the SHA-256 that Windows reports.

## [0.7.1] — 2026-11-19

### Fixed

- **The app no longer says it can keep an NTFS volume unmounted.** The Block mount switch works by the volume's UUID, and macOS reports no volume UUID for an NTFS volume, so the checkbox never appears for one and nothing is blocked. The support table (in the window, `captainsalvage filesystem capabilities` and the README) said "Supported"; it now says "Missing" and why. macOS mounts NTFS read-only by itself, so only a third-party NTFS driver on the Mac would write to such a volume.

## [0.7.0] — 2026-11-19

### Added

- **NTFS shows and copies what could be read of a damaged file.** When part of an NTFS file's data cannot be read (a damaged or missing run, a run outside the volume, or a read that fails), the text and byte previews now show each unreadable byte as `?` and say which bytes they are, and a copy keeps the file's layout, writes zeros there and the rest as read, is marked partial, and its manifest names the unreadable byte ranges. Before, the whole read failed and a copy made no file. Compressed, encrypted and incomplete streams are not treated this way: they still say why they cannot be read, and no file of zeros is made for them.

## [0.6.0] — 2026-11-19

### Added

- **An NTFS file with several names is listed under each of them.** When one file has more than one name (hard links), each name now appears as a file in the folder it belongs to, with the file's size and bytes, so a copy holds the file under every name. Before, only one name was listed and the others were shown only in the file's record details. Each name is an entry of its own, so scanning again does not list it twice.

## [0.5.1] — 2026-11-19

### Maintenance

- The shared engine has a typed range model, and the FAT, exFAT and NTFS adapters fill it for every entry beside the cluster lists ([#272](https://github.com/antimatter-studios/captainsalvage/pull/272))

## [0.5.0] — 2026-11-19

### Added

- **A preview shows where a file's bytes could not be read.** On exFAT and FAT volumes, when a cluster of a file cannot be read from the disk, the text and byte previews now show each of its bytes as `?` and say which bytes they are, instead of showing zeros that look like the file's own data. A copy of such a file keeps its layout, writes zeros where nothing could be read, is marked partial, and its manifest now names the unreadable byte ranges. Images and films still show those places as zeros.

## [0.4.15] — 2026-11-19

### Maintenance

- A pull request fails when what the tests run of our code falls below a captured baseline ([#267](https://github.com/antimatter-studios/captainsalvage/pull/267))

## [0.4.14] — 2026-11-19

### Documentation

- The README's status note says what is checked against independent tools, names the 4096-byte exFAT limit and no longer says the app is not released ([#265](https://github.com/antimatter-studios/captainsalvage/pull/265))

## [0.4.13] — 2026-11-19

### Fixed

- **The "Something wrote to this disk" box can be closed.** It had no clear way to close: only a small "Fold away" link among its buttons, and folding it left a red strip across the bottom of every screen for as long as the disk was changed. The box now has a close button (an ×) in its top-right corner, and Escape closes it too. Closing it removes it completely, with no strip. What it said is not lost: the red Check disk safety button at the top right stays red while the disk is changed, and its panel now shows the same findings (what changed, how many clusters and files, the advice about macOS) with Show the files, Rebuild the map now and Check again. If something writes to the disk again later, the box comes back once for that new change.

## [0.4.12] — 2026-11-19

### Fixed

- **The parts of a start-screen disk card line up.** Under a disk, the partitions were indented, but the notices below them ("Mounted, so macOS may write to it" and "Reading needs your permission") started at the card's left edge, so the card had three different left edges. The notices now start on the same line as the partitions, and their text and commands start at the same place as the partition names.

## [0.4.11] — 2026-11-19

### Fixed

- **The start screen follows the disks.** The list of disks and volumes was read once, when the screen opened, so a volume you unmounted still showed as mounted, and a disk you plugged in or took out did not appear or go until you pressed Check again. The screen now keeps itself current: it checks every 10 seconds and when the window comes back into view, so a disk attached or removed, or a volume mounted or unmounted outside the app, shows without a button and without the list flickering or moving. When the app itself sends a command to the system (Unmount, or blocking a volume from being mounted), it reads the state back as soon as the command returns and once more a moment later, because macOS can be slow to update, so the row changes at once. It asks the system for its list of disks and mounts, and rebuilds the report (which looks at each volume) only when that list changed, so a damaged disk is not asked about every few seconds.

## [0.4.10] — 2026-11-19

### Fixed

- **Opening a large volume that was scanned and tested before is much faster.** The window asks the app for its status every two seconds, and each of those calls counted every file test result in the map with a sort, which on a large volume (hundreds of thousands of results) kept the app busy so that opening the volume waited behind it for up to a minute. The counts are now read from an index, once, and kept up to date as tests run. The app also no longer asks macOS about the disks every two seconds: it reads the list of mounted volumes from the system without starting a program, runs `diskutil` once for a volume it has not seen before, and checks the disks' write counters in the background, so a status call neither starts a process nor waits on a disk.

## [0.4.9] — 2026-11-19

### Maintenance

- The shared engine asks the filesystem kind what it needs and no longer tests for exFAT, FAT or NTFS by name ([#255](https://github.com/antimatter-studios/captainsalvage/pull/255))

## [0.4.8] — 2026-11-19

### Fixed

- **A file whose short DOS name has accented letters shows them correctly.** On FAT volumes, a file made by DOS, a camera or another old device, with no long name and letters such as é, ä or ö in its 8.3 name, was listed with replacement characters (`���X.TXT`). The name is now read in the DOS code page 437, so it shows as `éäöX.TXT`.

## [0.4.7] — 2026-11-19

### Documentation

- The README lists what each filesystem supports from the table the app reports, and FAT's notes stop claiming a bitmap ([#220](https://github.com/antimatter-studios/captainsalvage/pull/220))

## [0.4.6] — 2026-11-19

### Maintenance

- A window test opens a synthetic NTFS image, scans, browses, previews a fragmented file and copies it out ([#246](https://github.com/antimatter-studios/captainsalvage/pull/246))

## [0.4.5] — 2026-11-19

### Fixed

- **An exFAT file whose entry runs across two folder blocks is no longer left out.** When a file's directory entry started at the end of one block of a folder and finished at the start of the next, and that next block was not the one right after it on the disk, the file was missing from the listing. The scan now reads the rest of the entry from the folder's next block, wherever it is.

## [0.4.4] — 2026-11-19

### Fixed

- **A file whose long name was stored across two folder blocks is listed under its real name.** On FAT12, FAT16 and FAT32, when a long file name began at the end of one block of a folder and the rest of the entry was at the start of the next, the file was listed under its short 8.3 name, such as `ENTRYF~4.TXT`, instead of its real name. Its size and contents were right. The scan now joins the end of the block before it, following the folder's chain, so the real name is shown.

## [0.4.3] — 2026-11-19

### Fixed

- **A file whose data was cross-linked with another file's is no longer shown as recoverable intact.** When two files on a FAT12, FAT16, FAT32, exFAT or NTFS volume claim some of the same clusters, so that one of them would copy out the other's bytes, both are now marked partial, with the shared clusters counted, instead of live.

## [0.4.2] — 2026-11-19

### Maintenance

- A saved copy of a volume's structures carries a versioned manifest of its regions, and copies without one read as version 1 ([#238](https://github.com/antimatter-studios/captainsalvage/pull/238))

## [0.4.1] — 2026-11-19

### Maintenance

- Every merged pull request is released: feat raises the minor and everything else raises the patch ([#236](https://github.com/antimatter-studios/captainsalvage/pull/236))

## [0.4.0] — 2026-11-18

### Added

- **An agent can build a Disk Rebuild for you.** Ask it in the chat to put the pieces back together, and it makes the Disk Rebuild, its folders and its layers itself, then reads the tree back and says which layer's copy of each file wins and why. Everything it makes shows at once in the Rebuild tab with an Agent mark and in the activity log, and you can take it back: **Undo** one step, **Undo all agent changes** of its latest session, or **Restore to before the agent**. The agent only changes the app's own database, never the disk, one call may change at most 1000 rows and a session 10000, and only you can start a copy, accept a suggestion or undo. If you only ask where something belongs, it still suggests.

- **Choose a partition, or the whole disk it belongs to, to inspect.** One chooser on the start screen lists every disk with its partitions. The map then covers exactly the bytes you chose. A disk whose partition table is damaged or missing is still listed, so it can be inspected whole.

- **The raw-disk map shows partitions as bands and findings as lanes.** The top map is one stable view of the bytes: declared partitions are broad background bands, and each filesystem format that was found has its own thin lane. A disk with many findings stays readable, because dense findings are summarised by format and count under the map, and the list can be narrowed to one format.
- **An inspector explains what you pick.** Click a band or a lane on the map, or an item in the list, and the side panel gives its format, byte range, confidence and status: recognised, needs related metadata, or decodable. The second graph magnifies the selected range, and clicking it shows that place in the main area.

- **FAT12, FAT16 and FAT32 volumes open and scan.** They go through the same filesystem adapter as exFAT, so directories, fragmented files, deleted records, saved baselines, scan resumption and recovery use the same workflow. Volume details are shown without the exFAT-only checksum claims. FAT support is validated with synthetic images.
- **NTFS records can be inspected and extracted from the command line and MCP.** Surviving FILE records are read without a working boot sector or namespace, and resident and complete uncompressed non-resident streams can be extracted. Damaged fixups and unresolved or unsupported mappings are reported, never silently repaired.
- **Disks, partitions and images open without a recognised filesystem.** They use the normal workspace and disk graph, working on byte ranges. Declared partitions and overlapping filesystem evidence are separate layers, you can select and label regions, and searching continues after a format is found. FAT, exFAT and NTFS boot evidence is probed, and surviving exFAT backup boot metadata can be used in memory beneath a newer NTFS boot sector without changing the source. The Source bytes tab returns to the underlying source and its findings. The command line and MCP offer the same operations.
- **Raw discovery scans show throughput and time remaining, and can be resumed.** Each completed batch saves its findings and progress under a stable scan ID. A reopened disk, clone or image can load a matching scan after you acknowledge sampled verification. Mismatches and sources that cannot be verified cannot resume, and regions that were unreadable are retried on a new source. Scans in memory from older builds are lost when those builds close.

- **NTFS volumes open, scan and recover in the normal workspace.** A scan reads every cluster for FILE records, so deleted files, old copies of the MFT and files whose folders are gone are found as well as live ones. The folder tree is rebuilt from each record's parent reference. Resident, fragmented, sparse and deleted files are read through the NTFS driver, and can be browsed, previewed, tested, rebuilt, copied and resumed like exFAT files. A saved baseline explains what was written to the volume since, and the volume stays unmounted and unwritten throughout. Volumes made by `mkntfs` and written by the Linux ntfs3 driver or ntfs-3g are read, and what the app lists and reads was checked against both in a Linux VM.
- **NTFS says what it cannot do.** Compressed files, EFS-encrypted files and files whose data continues in other records are listed with their size and marked unreadable, never filled with zeros. A BitLocker volume is refused by name after one sector is read. Alternate data streams and extra hard-link names are listed in the record details, not copied as files. The window, `captainsalvage filesystem capabilities` and the new `captainsalvage_capabilities` MCP tool report the same table for every filesystem.
- **A written-over NTFS boot sector can be replaced in memory by its backup.** Choose the copy in the volume's last sector as the boot evidence when you try an NTFS interpretation of a raw disk. The disk's own bytes are never changed.
- **`captainsalvage_ntfs_entry` shows everything an NTFS record says:** each name and the folder it was linked in, each data stream and whether it is resident, compressed, encrypted or sparse, and any damage found in the record.

- **Analyze a selected range of a raw source.** Choose **Analyze this range** to try every filesystem driver at every start offset of the range and see what decodes: the format, where it starts, how many bytes decode and how sure the app is. It finds structures at odd offsets, and finds FAT directory entries and NTFS MFT records by their shape even when the boot sector and the record signatures are gone. Data that is identified but cannot be placed in a volume is listed as an island, with an offer to search nearby for the missing metadata. For NTFS, the cluster size and where the volume starts are worked out from a surviving `$MFT` record, and the volume is opened read-only with a boot sector rebuilt in memory to check it. The same analysis is `captainsalvage range analyze` on the command line and `captainsalvage_analyze_range` in MCP. Nothing is written to the source.

- **Save what you think a stretch of the disk holds as a hypothesis.** An extent and a format, with a confidence, is kept on this Mac and can be switched on and off, so several guesses over the same bytes compete as layers and the upper one is read. Start and end can be typed or taken from the selection. A hypothesis can come straight from a range analysis finding, and opens as a filesystem with **Try this interpretation**. Nothing is ever written to the disk.
- **Hypotheses make a virtual layout.** Like a Disk Rebuild, a layout overlays segments of the source at positions of a virtual device, and a virtual partition table is a layout whose segments are partitions. Both are views the app reads through; bytes no segment covers are refused, not shown as zeros. `captainsalvage hypothesis` and the `captainsalvage_*_hypothesis` and `captainsalvage_read_layout` MCP tools do the same as the window.

- **A drive that a mount changed can be trusted again.** When you reopen a drive that is only a probable match (some markers changed, for example because a Mac mount wrote Finder data near the start), the findings, entries and test results near the changed markers are now marked **May be out of date**, and the window says how many regions that is. **Rescan changed regions** reads just those regions again, with the same interpretation, and replaces what was stored there a batch at a time. Only when every region is done does the drive get its new markers (the earlier ones are kept), so the next open of it is an exact match. A rescan that stops, fails or loses the drive leaves the regions marked and continues where it stopped. Nothing is scanned again unless you ask, and the drive is only read. The command line has `captainsalvage scan stale-regions` and `scan rescan-changed`, and the MCP tools are `list_stale_regions` (read only) and `rescan_changed_regions` (only the person using the app).

- **Right-click a file or a folder and choose Export.** It copies that one file or folder to the place you choose, directly and as it is, never over a file that is already there. Each copy is checked as it is written. You see the plan first, then live progress, and the same copy report as before.
- **The command line and MCP can export too.** `export_plan` plans an export and `export_start` starts it, for the person only, as before. The tools for a Disk Rebuild are now called `disk_rebuild_*`, with `disk_rebuild_copy_plan` and `disk_rebuild_copy_start` to copy one.

- **A scan is the unit of stored work, found again by the drive and how it was read.** Everything a scan stores (its filesystem map, file tests, islands, read errors and Disk Rebuilds, or its raw findings) now belongs to a scan: a source, the interpretation read there (an extent and a format, or none), its options, the scanner version and the range. Opening a drive finds its stored scans by its markers, so a boot sector written over after scanning no longer loses the work, and trying another format or extent makes a new scan and keeps the old one, which opens again without scanning. Per-block entropy and text caches are shared by every scan of a drive. Each scan lists when it was made, how much it covers and how much it stores on this Mac. Scans and workspaces made by earlier versions are registered the first time the app starts, from where they are, so nothing already scanned is scanned again; the database is copied to `backups` first. The command line has `captainsalvage source`, and the MCP tools `list_sources`, `identify_source`, `list_scans` and `open_stored_scan` read the same records.

- **One way to open: Open.** A disk, a partition, a disk image, one extent of a disk and a saved scan all open with the same button. The raw source is always loaded first, and an interpretation is applied to it: the one the last scan of a matching stored source used, otherwise the best detection from the boot evidence, otherwise raw bytes. Opening starts no scan, and what is stored loads at once.
- **An interpretation picker in the header.** `Filesystem: NTFS (detected, 86%)` lists exFAT, FAT, NTFS and no filesystem. Choosing another makes a new scan and says that the earlier scan stays, and choosing the earlier one again opens it without scanning.
- **Saved scans on Open.** An exact match opens its latest scan. A probable match is never applied silently: a question lists the stored sources by confidence with their changed markers, and a scan opens only after you say you read what changed. Matches under 60% are hidden until Show all. Each scan lists its interpretation, date, how much it covers, what it stores, and a name you can edit.
- **A partition and its extent of a whole disk are one source.** Opening a whole disk shows its bands, lanes and extents, each with its own Open. A partition opened alone and the same extent seen from its disk share one interpretation and one set of results, in either order.

- **A drive is recognised by markers, with a confidence.** A saved raw scan used to be offered only if every sampled block matched, so one block rewritten by a mount made a damaged drive look like a different one. A source now has up to 20 markers of 4 KiB, spread over the whole drive and skipping blank blocks, and each is compared on its own. The saved-scans list shows the confidence ("85%: 17 match, 2 changed, 1 unreadable") and where the changed markers are. An exact match loads as before. A probable match (60% or more) loads only after you accept the changed markers, and the scan records that it resumed on a changed source. Weaker matches are hidden unless you choose **Show all**, and a different drive or a different size is never offered. The CLI has `--accept-changed-markers` and `--show-all`, and the MCP tools `acceptChangedMarkers` and `showAll`. Scans saved by earlier versions are still recognised.

### Changed

- **The start screen says each thing once.** Each volume is one compact row again, as before raw inspection was added, instead of carrying the same two raw-inspection buttons. The notes about mounted volumes, unsupported formats and read permission appear once under the list, with one permission command for every disk that needs it.

- **The map's layer switch is gone.** Source bytes, declared partitions and filesystem evidence used to be three views to switch between; they are now drawn together.

- **The engine package, library and directory are named `captainsalvage`.** The terminal agent tool `salvage-agent` is now `captainsalvage agent`.

- **NTFS volumes can be opened from the start screen.** They were listed but could not be chosen.

- **The treasure list is gone.** There is no list of files to copy any more, no "Add to the treasure list" button or tick box, and no Treasure list tab. A Disk Rebuild is the one way to build what gets copied: drag pieces from the left panel onto a parent in its tree. To copy a single file or folder, right-click it and choose Export.
- **"Canvas" is now called "Disk Rebuild" everywhere**, in the window, the tool descriptions and the engine's messages. The tools for it are named `disk_rebuild_*`, and the argument that names one is `diskRebuild`.

- **The start screen is a tree.** Each physical disk is a parent row with its own Open, and its partitions are nested under it, each with its filesystem, size, status, Block mount and Open. A disk with no readable partition table is still a parent row.

- **The start screen shows each physical disk once, with its volumes under it.** A disk is one card with its name, size and device, and each volume is one short row with its filesystem, size, state and an Open button. Block mount is an inline checkbox on every row with a single note under the list, and a disk says once what it needs (an unmount, or permission to read) instead of repeating it for every volume. Each disk has its own Open, and the disk image form fits on one line. Three disks and their volumes fit on the screen without scrolling.

- **A proper title area across the top of the window.** The left half shows the app icon and "Captain Salvage" as a title, with the version under it. The right half, the toolbar of an open disk, shows the disk or device name as a title in the same size and weight, with its size, Change disk and the Filesystem picker under it. The two halves have the same height and read as one row, and both still drag the window. A long device name is cut short, with its full path as a tooltip.
- **The window shows the current app icon.** The icon in the window header, and the Windows tile icons, still showed the old magnifying glass. They are now made from the same artwork as the app icon, and a test fails if any of them differs from it.

### Removed

- **An old treasure list is dropped, not imported.** Nobody used it, so a list kept by an earlier version does not become a Disk Rebuild, and the app never reads it. A map that still holds the old list's table keeps it, untouched.
- **The old tool names are gone, with no replies pointing to new ones.** `basket_*`, `copy_plan`, `copy_start` and `canvas_*` do not exist; a call to one fails like any other unknown tool. Use `export_plan` and `export_start` (the person's to start) for one file or folder, and `disk_rebuild_*` for a Disk Rebuild, which is named by the `diskRebuild` argument. The old `canvas` argument is no longer read.

### Fixed

- **A live file is no longer marked partially overwritten because of its own clusters.** Copying a live file does not count the allocation of its own clusters as damage.

- **A copy now includes files that have no bytes.** A file that is live on the volume but empty, such as `__init__.py`, `.gitkeep` or a lock file, was left out of every copy and Export, and the status did not say so. It is now made at its destination, empty, with its name and date, counted in the plan and the report, and never over a file that is already there. This holds for exFAT, FAT and NTFS. An empty entry that was deleted is still not made into a file.

- **Files with long entry sets are found.** A file whose directory entry carries more than eighteen secondary entries, as one with vendor extensions can, is now listed and recovered instead of being skipped.
- **Less noise among lost entries.** A candidate entry whose checksum fails and whose reserved bytes are not zero is now treated as noise, not as a damaged file.

## [0.3.0] — 2026-11-17

### Added

- **Choose which agent to ask, and its model.** The Ask agent tab works with Claude Code, Codex and opencode. Agents that are not installed say how to install them, and the choice is remembered.
- **`salvage-agent` asks the same agents from a terminal.**

- **A new app icon.** A radar sweep over the chart: a brass-rimmed screen, three islands, one of them under a red crosshair as the find the sweep has just picked up.

- **See and copy one file from inside an archive.** In the archive view, Show it displays a picture or the start of a text, and Copy it out copies just that file, to your salvage drive by default and never over another file. Both check the file as they unpack it.

- **Look inside more kinds of archive:** tar, tar.gz, tar.xz, tar.bz2, tar.zst, gz, xz, bz2, zst and 7z, as well as zip. A file whose name was lost is recognised by its first bytes, a damaged tar lists as far as it can be read, and each file can be checked.

- **MKV and WebM films are checked element by element.** A new Matroska walker follows every element down to each block in each cluster, including clusters of unknown size, so an overwritten or unwritten cluster shows where it starts. It runs as large films stream, and for small ones in memory.

- **File tests can be aimed.** `file_tests_start` now also takes entry ids, verdicts, reason codes, extensions and a minimum size, so an agent can test, for example, only the contested films over 100 MB. `test_stream` takes `summary` to leave out the cluster runs, which run to megabytes for a film.

- **A more helpful agent chat.** It suggests questions that fit what you have selected, turns cluster numbers in answers into links to that spot on the map, and folds away the tools the agent used.

- **A bridge is judged at its join.** When `test_stream` tests clusters appended to a file, it now says whether the stream runs straight across the join (`clean`), breaks there (`breaks`), or stopped before it (`unknown`). A high valid fraction alone proves nothing, because valid MP3 frames turn up in any audio.

- **Browse files as a list or as icons.** The list shows each file's size, date and verdict.
- **Browse files opens on All**, the first kind, and remembers the kind and layout you chose across restarts.

- **You can copy a rebuilt canvas to another drive, with files placed in the folders you arranged and the selected copies used.** A preview shows the file count, required space, and destination before copying.

- **Rebuild the folder tree from the lost pieces, like a jigsaw.** The Rebuild tab has a canvas: drag lost folders onto it from the sidebar, put them inside one another, merge them, and make up folders the disk lost. Where pieces hold the same file, the best copy wins and you can pick another. Each piece is used once, and the rest can be put away in one step.

- **Claude can suggest where a piece belongs on a canvas.** Its suggestions appear in the Rebuild tab with the reason for each, and you accept or dismiss them. Claude never places a piece itself.

- **Agents can carve lost files** out of space nothing on the disk explains, with `carve_candidates`. It finds where each file ends from its own structure (JPEG, PNG, MP4, AVI, WAV, zip, 7z and PDF), or from where similar clusters stop, and says how far to trust it.

- **Claude now answers in the language you selected, including when you switch languages mid-conversation.** You can chat naturally without repeating your preference.

- **Who else claims a file's clusters.** The new `conflicts` tool lists, for one file, every other entry that claims any of its clusters: its verdict, whether it is deleted, when it was made, its size, the runs of clusters it shares, and who holds them by the map's rule. It also says how many shared clusters hold directory entries and what the shared clusters hold now.
- **The script reference lists every field its functions return**, and warns that Rhai reserves some words such as `shared`. A test checks the reference against the real output, and another checks that the ownership map and `owners` agree.

- **The disconnected-disk warning has a Copy button for each command.** Unmounting the disk or restoring read access is one paste into Terminal.

- **A copy report.** Open it from the treasure list or a canvas copy to see every file copied, complete or partial, and check the copies again: it reads each copy from the destination and finds any that changed or went missing since.

- **Scrub through the clusters with the arrow keys.** Click the magnified map and hold left or right: it starts one cluster at a time and speeds up the longer you hold, and stops as soon as you let go. Shift and an arrow jumps a whole view.

- **Drag a single file from Browse files onto a rebuilt tree**, or a folder from inside a piece in the sidebar. Each joins the folder it lands on as a layer of its own, and clashes with what is there like any other copy.

- **Rebuilt canvases can appear as read-only folders in Finder.** Browse recovered files there without changing the disk.

- **A warning on every screen when a disk you asked to keep unmounted is mounted anyway.** That can happen at startup, before Captain Salvage is running. While the disk stays mounted, macOS may write to it, and the warning has a button to unmount just that volume.

- **The window can be shown in French, German or Spanish.** A switch beside the light and dark switch chooses the language, and the choice is kept. Some messages from the disk analysis are still in English.

- **Lost folders get a label from their files.** A folder that lost its name now shows a guess such as "Mostly videos, 128 files" in the sidebar, in italics, with its cluster number kept beside it.

- **See where macOS wrote on the disk.** In the Overview, each date macOS wrote to the volume has a Show on the map button, and a file whose clusters those writes ran through now warns that parts of it may have been overwritten.

- **A map you can read at a glance.** Click a legend item to show only that category on the map, and rest the pointer on a file in the sidebar to light up its pieces.

- **A tidier map legend, and a line where the scan is.** The legend lists only what the map shows, keeping read errors always, and a dashed line marks where a running scan is reading.

- **MCP clients can check and change the app’s language, appearance, and Finder canvases.** Their changes appear in the window too.

- **Totals without names, for agents.** Every assessed entry now carries a stable reason code, its type group and its extension. The new `breakdown` tool counts and sums found files by verdict, reason, type, extension, size, deleted or not, and folder kind. The new `chain_summary` tool groups the nameless FAT chains by what their first cluster holds and how they end. Neither returns a name. `query_map` now lists every table and column, read from the schema itself.

- **You can block individual volumes from mounting while Captain Salvage is open.** This helps protect damaged disks from unwanted writes.

- **MPEG, FLV and WMV films are tested.** New walkers follow MPEG program streams pack by pack and packet by packet, FLV tag by tag with each tag's back-pointer, and ASF (WMV, WMA) object by object and packet by packet, so damage shows where it starts. Raw MPEG video with no packs is left untested rather than called damaged.
- **A file is walked as what its bytes say it is**, whatever its name: a WMV named `.mpg` is walked as ASF. Up to 4 KB of web headers before an MPEG stream's first pack are skipped, as players do.

- **Keep file names and previews from agents.** A switch in the chat panel, which only the person can change, keeps file names and contents from any agent working through the tools. Names and paths in every answer read `[hidden]` with the extension kept, previews and window screenshots are refused, and SQL from `query_map` and scripts reads name columns as empty. Counts, sizes, types and verdicts stay visible. A script can still read raw bytes, so the switch prevents accidents rather than stopping a determined agent.

- **Plays with gaps.** An MP3 that fails its test but is still at least 80% valid frames is now marked "Plays with gaps" rather than "Failed test", because a player skips each damaged stretch and carries on. On a real damaged volume this was most failed MP3s whose clusters had been reused at their start by small files: everything after each overwritten stretch was intact.

- **Look inside rar archives**, RAR 2.9 to RAR 5 and later, and check each file against its checksum. The archive is read from the disk as needed, never copied whole.

- **The disk's structures are read from the baseline copy once the disk has changed.** When the boot regions, FAT, bitmap or root on the disk no longer match the baseline, the map uses the copy kept on the Mac, so a chain macOS freed or a cluster it took still reads as it was. File data is still read from the disk, and the warning panel says which source the map uses. A baseline recorded before copies were kept gets its copy the next time the disk still matches it.

- **See what a change would do before making it.** Changing how damage is weighed now first shows how many files would move from one verdict to another, with their sizes, and applies only when you choose Apply. The new `resolve_preview` tool does the same for agents, with parameter changes and hypotheses, and saves nothing.

- **The app's name and a home button sit at the top of the sidebar.** The house button returns to the overview from any view, and the sidebar keeps enough width for that row.

- **Find pieces that are near copies of each other.** A lost folder in Details, or a piece on the canvas, can list other pieces holding many of the same files, with which one has the better data, and a piece on the canvas can be swapped for a similar one in its place.

- **Simple and Investigate modes.** Simple, the default, shows only what recovering files needs and never a cluster number. Investigate adds the map, the bytes and every investigation tool. Switch between them in the toolbar.

- **Films of any size are tested.** MP4, MOV and AVI files over 64 MB are now checked as they are read, a piece at a time, instead of being left untested.

- **Messages from the disk analysis are translated too.** Verdict reasons, file test and zip notes, disk access instructions and errors show in French, German or Spanish.

- **Videos and FLAC files can be tested, and results are shown per file type.** The file tests now check MP4, MOV, AVI and FLAC files up to 64 MB by their own structure, and the Overview lists how many files of each type passed, came out damaged or are untested.

- **Clashing copies are picked by quality, then date, then the upper layer**, and each pick says why. Copies that differ but measure the same are marked for you to look at, and a folder says how many clashes were picked for you and how many need you.

- **A workbench to put a broken file back together.** Add a file, chain, cluster or region as pieces, put them in order, test and see the result live from the disk, and keep it as the file's clusters when it looks right.

- **A loud warning when something wrote to the disk.** A copy of the boot regions, FAT, bitmap and root directory is kept beside the baseline. When the disk later differs, a red panel on every screen says how many clusters that were free now hold other data, which top-level entries changed, and which found files used those clusters. The map marks those files as overwritten or partial, a file test from before the write no longer counts for them, and the structures are read again every ten minutes. The `structure_changes` tool gives the same detail.

- **Test results can be forgotten for some formats only.** `file_tests_forget` takes `formats`, so an improved check can re-test, say, only MP3s.

### Changed

- **Ask agent is a tab in the right panel.** The conversation no longer shares the panel's
  height with the details. It keeps going on other tabs, and the tab shows a dot while the
  agent answers and until you read a new reply.
- **The Rebuild tab has a dump truck icon**, which is easier to recognise.

- **The start screen charts your disk as an archipelago.** Each kind of file found is an island round the disk, sized by how much there is, with the lost folders still to fit together.

- **The steps that keep a disk safe moved from the start screen to the overview.** They cover the drive you opened, with a button to check again, and the start screen is less crowded.

- **An MPEG film is walked as a player reads it.** After damage, the MPEG walker now picks the stream up again at the next pack, as players do, and counts only the bytes lost, so a film with a few foreign blocks can be marked "Plays with gaps" instead of failing outright. A last packet cut short at the end of the file is a note rather than a failure.

- **The Finder extension is built into the app.** The separate Captain Salvage Finder app is no longer needed, and installing removes it. The app is signed by the team, so its signature, and network filter rules for it, stay the same from build to build. Its identifier is now com.antimatterstudios.captainsalvage.

- **Rebuild is now a tab in the right panel.** Click a folder in the sidebar to see what is in
  it in the middle, then drag it onto the canvas beside it, without switching views.

- **The recovery basket is now called the treasure list**, with a treasure chest for its icon.

### Fixed

- **AVI films are checked frame by frame.** The AVI walker treated each list, including the one holding every frame, as one opaque chunk, so damage in the middle of a film passed. It now walks inside lists to each video and audio chunk. Padding left after the last RIFF form no longer counts as a break.

- **Owners of a cluster include chains claimed from their middle.** A deleted entry whose first cluster lies inside another file's FAT chain was missed when asking who claims one of that chain's clusters.

- **Copied files get the right modified date.** Copies used to read the disk's time as UTC, so dates could be off by your time zone. They now use the zone the disk recorded, or your Mac's zone when it recorded none, and the window shows the recorded zone beside a time.

- **Copies to an SD card or other slow drive are up to 16 times faster.** A copy no longer waits for the drive to fully commit after every file. If a run is cut off, the next one checks the newest copies again and copies any that came out wrong.

- **A long disk path no longer pushes the safety chip out of the window.** The path shortens from the left instead, so the file name at its end stays readable.

- **Captain Salvage starts keeping your flagged disks unmounted before its window opens.** A disk attached at login was sometimes mounted before the block started, because the window took a few seconds to open.

- **MP3s with tags at the end are no longer marked damaged.** The MP3 test counted the last frame as a break whenever an ID3v1, APE or Lyrics3 tag, another ID3 tag or padding followed it, and also broke on zeros padding out an ID3v2 tag and on an ID3v2.4 footer. Whole MP3s came out as "Plays with gaps" because of it.

- **The app keeps answering while its window is out of sight.** macOS used to put it to sleep, so quitting could hang for minutes and agents using the app got no answer. Your Mac can still sleep as usual.

- **The folder whose files you’re viewing stays marked when you select something else.** You can keep track of where you are while browsing.

- **The chat tab is called Chat with an agent, and no screen asks you to "ask Claude" any more.** The Overview, the Rebuild suggestions and the free-space hint now say "agent", since the chat works with Claude Code, Codex or opencode.
- **Simple mode no longer tells you to click the map.** The map is hidden in Simple mode, so the Overview drops that hint and an empty selection says to click a file.

- **The recovery overview now counts files still visible on the volume as found.** Its totals match the start screen, giving you a clearer picture of what’s available.

- **Large pictures and PDFs now explain why they can’t be previewed.** You can copy them out to open them instead of mistaking them for damaged files.

- **The app now consistently uses its Captain Salvage name and keeps your existing recovery work and copy progress when you upgrade.** Your saved maps, canvases, and interrupted copies remain available under the new name.

- **Files refresh when a quick scan finishes.** You see newly scanned results even if the scan ends between updates.

- **A cluster reused after a deletion belongs to the newer file.** When a deleted entry and a file made after that deletion claim the same cluster, the later file holds it: that file is no longer marked partial for it, and a deleted entry whose clusters were all reused is marked overwritten. A new switch under "How damage is weighed" counts every shared cluster as a conflict again.

- **The disk safety alert opens its checklist once when a problem appears, and the checklist can be dismissed.** The alert stays visible until the problem is resolved.

- **The app can take a picture of its own window after the window has been idle or covered.** Agents asking for a screenshot used to get a timeout instead.

- **Selecting a file or folder opens the sidebar tree to show it and scrolls it into view.** You can see where it belongs without searching through folders.

- **Every file carries one version number.** The version lived in six places, and only a release
  set them, so the v0.2.0 tag shipped with manifests that still said 0.1.0. It now lives in `VERSION`;
  `release.py sync` copies it everywhere and `release.py check` runs on every pull request. The build
  carries that number, not one worked out from unreleased pull requests.

## [0.2.0] — 2026-11-05

### Added

- Release every feature and fix automatically ([#60](https://github.com/antimatter-studios/captainsalvage/pull/60))
- Show the version on the start screen ([#59](https://github.com/antimatter-studios/captainsalvage/pull/59))
- Show how to keep a disk safe on the start screen, and undo it ([#58](https://github.com/antimatter-studios/captainsalvage/pull/58))
- Open on a start screen, and never reopen a disk by itself ([#57](https://github.com/antimatter-studios/captainsalvage/pull/57))
- Show what a zip holds as a file tree, read live from the disk ([#51](https://github.com/antimatter-studios/captainsalvage/pull/51))
- Warn that macOS writes to a damaged volume, and show the evidence ([#50](https://github.com/antimatter-studios/captainsalvage/pull/50))
- Show identical files as one in the browse list ([#46](https://github.com/antimatter-studios/captainsalvage/pull/46))
- Test found files in the background and score them on the result ([#45](https://github.com/antimatter-studios/captainsalvage/pull/45))

### Fixed

- Say when the disk is disconnected, and keep disks from sleeping ([#56](https://github.com/antimatter-studios/captainsalvage/pull/56))
- Report what survives in a damaged file, and stop calling text keys broken zips ([#47](https://github.com/antimatter-studios/captainsalvage/pull/47))

## [0.1.0] — 2026-11-05

### Added

- Show the version, counting builds and releases ([#38](https://github.com/antimatter-studios/captainsalvage/pull/38))
- Mark the selected spot on the map with arrows ([#37](https://github.com/antimatter-studios/captainsalvage/pull/37))
- Name the app Captain Salvage and add a light and dark switch ([#35](https://github.com/antimatter-studios/captainsalvage/pull/35))
- Navigate the sidebar with the arrow keys ([#34](https://github.com/antimatter-studios/captainsalvage/pull/34))
- Drag the edges of the middle column to resize it ([#33](https://github.com/antimatter-studios/captainsalvage/pull/33))
- Widen the details panel and preview files in it automatically ([#32](https://github.com/antimatter-studios/captainsalvage/pull/32))
- Play more formats through an installed ffmpeg ([#31](https://github.com/antimatter-studios/captainsalvage/pull/31))
- Browse files by type and preview them live from the disk ([#30](https://github.com/antimatter-studios/captainsalvage/pull/30))
- Add a recovery basket with live progress from any process ([#24](https://github.com/antimatter-studios/captainsalvage/pull/24))

### Fixed

- Let a resumed copy count files already on the destination ([#26](https://github.com/antimatter-studios/captainsalvage/pull/26))
- Make lens-scan wait for a basket copy to finish ([#25](https://github.com/antimatter-studios/captainsalvage/pull/25))

### Documentation

- Name Chris Thomas as the copyright holder ([#29](https://github.com/antimatter-studios/captainsalvage/pull/29))
- License the project under MIT ([#28](https://github.com/antimatter-studios/captainsalvage/pull/28))

### Other

- Add a changelog, CI and a release pipeline ([#36](https://github.com/antimatter-studios/captainsalvage/pull/36))
