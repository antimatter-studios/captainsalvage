# Plan: rebuilt views, layers and the treasure list

For review before building. It turns the Rebuild canvas into a stack of layers, makes the
treasure list a selection inside a view, and gives copying a job with checking that can be
watched. Nothing here writes to the disk being recovered: views live in the app's database,
and copies go only to the destination the person chooses.

## What exists today

- **Canvas** (`canvas.rs`): items are made-up folders, pieces placed as folders, and pieces
  merged into a folder (`ITEM_FOLDER`, `ITEM_PIECE`, `ITEM_MERGE`), each with a parent and a
  position. The tree is worked out from the map each time it is read.
- **Clashes**: where two pieces hold a file of the same name, the best-quality copy is shown,
  and `canvas_choices` records a copy the person chose instead.
- **Treasure list** (`basket`): a flat list of entries and folders, each copied out under its
  old path. It knows nothing about canvases.
- **Copy out**: a canvas copies whole to `destination/<canvas name>`. The basket copies on its
  own. Both write a manifest, resume, never overwrite, and hash each file as it is read and
  again from the destination.

## The model

### A view is a canvas, built from layers

- **Made-up folders** stay as they are: the skeleton the person remembers, such as `Music`.
- **A drop is a layer.** Dropping a segment from the left column onto a folder lays its files
  and folders over that place. Dropping several segments onto the same folder stacks them.
  The order they were dropped in is the layer order, upper last. The person can drag layers
  to reorder them.
- **Folders merge, files clash.** A folder of the same name in two layers is one folder,
  holding both layers' contents. A file of the same name in two layers is a clash.
- **Today's "place as a folder" stays.** A segment can still be placed as a folder of its own
  instead of being laid over a place.
- **Single files and folders from inside a segment** can be added too, for the "I just want
  these three photos" case. Each one is a small layer of its own.

### Clashes are resolved for the person

When files clash, the app picks one, in this order:

1. Tested good, ahead of untested, ahead of failed to decode.
2. Whole chain, ahead of partial or reused clusters.
3. A valid entry checksum.
4. The size the entry says matches what was read.
5. A newer modified date.
6. If everything ties, the upper layer.

- **Identical copies aren't clashes.** Same size and same content hash means the choice
  doesn't matter, so nothing is shown.
- **Clashes are shown where they are.** A folder shows "412 picked for you, 3 need you". The
  three are those the measures cannot separate, but whose bytes differ.
- **Every file shows its layer**, and the copies below it, and the person can override any pick.
  That's today's `canvas_choices`, kept.

### The treasure list is a selection inside a view

- In the view's tree, the person ticks folders and files to recover. A ticked folder brings
  everything in it.
- Each item lands at its path relative to the view's root. Per item, the person can make it
  its own top level instead: **Copy as its own folder**.
- The treasure list tab shows what is ticked, its size, and anything that clashes or is
  damaged, before anything is copied.
- Today's flat basket becomes ticks in a view the app keeps for loose picks, called **Picks**.
  Browse files' "Add to the treasure list" adds there, or to the view open in Rebuild. Your
  existing basket moves into it, so nothing is lost.

### Process the treasure list: copy, verify, report

- **Process treasure list** asks for the destination: the salvage drive by default, never the
  disk being recovered. It starts a job.
- **Copy:** each file goes to its path, from the copy the view picked. It resumes after a
  stop, and never overwrites.
- **Verify:** after copying, every file is read back from the destination and compared with
  the hash taken when it was read from the disk. A file whose source was partial or damaged
  is marked so, not called good.
- **Report:** a progress window the person can open at any time. It shows files done, bytes,
  speed and time left, then a per-file list (copied, verified, mismatch, damaged at the
  source) with filters. It's saved with the job, so it can be read again after a restart.
- Starting and stopping stay the person's, as now. An agent can plan a job and read its report.

## Engine changes

| Area | Change |
|---|---|
| `canvas_items` | Add a `layer` order per drop, and an item kind for a file or folder from inside a segment |
| Tree | Merge folders across layers, collect clashes, apply the pick order, skip identical files |
| Clashes | `canvas_clashes(canvas, folder?)` lists clashes with each copy's measures and the pick |
| Treasure list | New `treasure` table: (view, path, own_root), replacing `basket`. The old basket moves into the Picks view on first start |
| Jobs | New `jobs` and `job_files` tables. Copy and verify run as one job, with status per file, so a report survives a restart |
| MCP | Read: clashes, layers, the treasure list and job reports. Plan a job. Starting and stopping stay the person's |

## Window changes

- **Rebuild tab:** clash badges on folders, a layer list for the selected folder (drag to
  reorder), the tick column, and the per-file layer and pick.
- **Treasure list tab:** the ticked items by view, sizes, warnings, own-root switches, and
  Process treasure list.
- **Progress window:** opens from the job's status in the toolbar, and from the treasure list.

## Pull requests, in order

1. **Layers:** drop as a layer, layer order, merging folders, clash detection with the pick
   order, identical files skipped, `canvas_clashes`. The window shows badges and layers.
2. **Single files and folders** from inside a segment, as small layers.
3. **Treasure list as ticks in views:** the `treasure` table, the Picks view, moving the old
   basket over, own-root per item, the new treasure list tab.
4. **Jobs:** copy, then verify, then report, with the progress window. Canvas copy-out and
   the treasure list both use it.
5. **Agent tools** for all of the above, and the README.

## Decisions

- **Layer order:** the latest drop is on top, and clashing files are picked by the measures
  above, the upper layer only breaking a tie.
- **Verify:** always on. It reads every copied file a second time, from the destination
  only, never the damaged disk.
- **Loose picks from Browse files:** still open. The choices are a Picks view the app keeps,
  adding only to the view open in Rebuild, or a treasure list holding view ticks and loose
  finds side by side.
