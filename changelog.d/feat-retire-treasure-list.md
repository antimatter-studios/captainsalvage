### Added

- **Right-click a file or a folder and choose Export.** It copies that one file or folder to the place you choose, directly and as it is, never over a file that is already there. Each copy is checked as it is written. You see the plan first, then live progress, and the same copy report as before.
- **The command line and MCP can export too.** `export_plan` plans an export and `export_start` starts it, for the person only, as before. The tools for a Disk Rebuild are now called `disk_rebuild_*`, with `disk_rebuild_copy_plan` and `disk_rebuild_copy_start` to copy one.

### Changed

- **The treasure list is gone.** There is no list of files to copy any more, no "Add to the treasure list" button or tick box, and no Treasure list tab. A Disk Rebuild is the one way to build what gets copied: drag pieces from the left panel onto a parent in its tree. To copy a single file or folder, right-click it and choose Export.
- **"Canvas" is now called "Disk Rebuild" everywhere**, in the window, the tool descriptions and the engine's messages. The tools for it are named `disk_rebuild_*`, and the argument that names one is `diskRebuild`.
