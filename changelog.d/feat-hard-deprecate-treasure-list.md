### Removed

- **An old treasure list is dropped, not imported.** Nobody used it, so a list kept by an earlier version does not become a Disk Rebuild, and the app never reads it. A map that still holds the old list's table keeps it, untouched.
- **The old tool names are gone, with no replies pointing to new ones.** `basket_*`, `copy_plan`, `copy_start` and `canvas_*` do not exist; a call to one fails like any other unknown tool. Use `export_plan` and `export_start` (the person's to start) for one file or folder, and `disk_rebuild_*` for a Disk Rebuild, which is named by the `diskRebuild` argument. The old `canvas` argument is no longer read.
