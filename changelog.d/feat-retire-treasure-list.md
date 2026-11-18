### Added

- **Right-click a file or a folder and choose Export.** It copies that one file or folder to the place you choose, directly and as it is, never over a file that is already there. Each copy is checked as it is written. You see the plan first, then live progress, and the same copy report as before.
- **The command line and MCP can export too.** `export_plan` plans an export and `export_start` starts it, for the person only, as before. The tools for a Disk Rebuild are now called `disk_rebuild_*`, with `disk_rebuild_copy_plan` and `disk_rebuild_copy_start` to copy one.

### Changed

- **The treasure list is gone.** There is no list of files to copy any more, no "Add to the treasure list" button or tick box, and no Treasure list tab. A Disk Rebuild is the one way to build what gets copied: drag pieces from the left panel onto a parent in its tree. To copy a single file or folder, right-click it and choose Export.
- **Your old treasure list was not lost.** If it held items, it became a Disk Rebuild named "Imported treasure list" for its scan, which you can keep, change or delete.
- **"Canvas" is now called "Disk Rebuild" everywhere.** The old `canvas_*` tools answer with the `disk_rebuild_*` tool that replaces them, and so do the removed `basket_*`, `copy_plan` and `copy_start` tools, for this release only.
