# Changelog

Each release lists what changed in it. The same notes appear on the
[releases page](https://github.com/antimatter-studios/captainsalvage/releases).

Changes not yet released are fragments in [`changelog.d/`](changelog.d/), one per pull request.
`scripts/create-release` gathers them into a section here.

## [Unreleased]

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
