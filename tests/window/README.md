# Window tests

These scripts drive the real app window through its MCP server, against small synthetic disk
images. They are for checking a change in the window itself, where the Rust and TypeScript
tests cannot reach. Each script prints `PASS` or `FAIL` per check and a total at the end.

They only ever open the images made here. Never point them at a real disk, and never use them
to open or capture real files.

## Running one

Build the app first (`npm run app`, see the README), then pass the binary inside the bundle:

```sh
python3 tests/window/modes_test.py "target/release/bundle/macos/Captain Salvage.app/Contents/MacOS/captain-salvage"
```

Run one test at a time, and not while a build is running. Several at once, or a test beside a
release build, slows the machine enough for the tests to time out.

`open_door_test.py` is the one door. It opens a synthetic whole disk (an MBR and one exFAT
partition) and checks the whole-disk scope, Open on its partition, the interpretation picker (the
detected filesystem marked, a new scan on a change with the earlier one kept, a reason when there
is no evidence), the saved scans list with its editable names, and that the same partition opened
alone from a file of its bytes is the same source and scan. It needs `jigsaw.img` and saves
`door-header.png` here.

`raw_disk_test.py` derives a throwaway raw image from `jigsaw.img`, with NTFS boot evidence
and an older exFAT volume beyond a declared partition. It verifies that raw sources use the
normal workspace and shared graph, that evidence discovery reaches both formats, that byte
inspection works before filesystem recognition, and that interpretations return to the same
workspace without modifying the source. It saves a screenshot to
`/tmp/captainsalvage-raw-workspace.png` for visual inspection.

`raw_disk_crowded_test.py` scans a sparse image with 1500 NTFS boot sectors and checks that the
window never stops answering for 400 ms. It measures the longest gap between two turns of a
message loop in the window, not a timer: a covered window or a locked screen makes the web view
run timers once a second, so a 20 ms timer reads about 980 ms with nothing wrong (issue #198).
`tests/raw_disk_crowded_budget_test.mjs` times the same work without a window, and CI runs it.

`raw_scan_test.py` checks raw-scan speed and ETA, pauses between bounded batches, restarts its
isolated test app, and opens a renamed clone, which loads the saved scan by itself because the
match is exact. It verifies that one changed marker leaves a probable match that asks first (the
saved scans question) and needs its own acknowledgement, and that a source whose markers nearly
all changed is never offered, in both the window and the backend. Its temporary source images,
database and app home never use real disks or the user's application data.

`raw_disk_crowded_test.py` scans a sparse synthetic image holding 1500 NTFS boot sectors and
checks that the window never stalls, that the evidence list stays one page long, and that "Show
more" adds a page. `scripts/bench-raw-disk-map.mjs` times the same load without a window.

`raw_disk_lanes_test.py` scans a synthetic 24 MiB image with three partitions, NTFS and exFAT
evidence and a dense run of findings, and checks the map's bands and lanes, the format counts, the
inspector for a picked band and a picked lane, the magnified second graph and the format filter.
It needs `jigsaw.img` and saves `raw-lanes.png` here.

`device_picker_test.py` gives the start screen a made-up list of disks through
`SALVAGE_TEST_STARTUP_DISKS` (a JSON file in the shape `startup_disks` returns, honoured only
while `SALVAGE_TEST_UI` is set), so no real disk is listed. Its "devices" are synthetic images:
a whole disk with no partition table, a disk with three partitions and a card reader with one.
It checks that the start screen is a tree (each physical disk a parent row with its own Open and its
volumes nested under it, each row with the same one Open button and no raw control), that every
volume has an inline "Block mount" checkbox that sets and clears the block, that the list has one
note about it, that a disk says once what it needs, that Open on a whole disk and on a partition
each opens exactly its own bytes, and that the three disks fit on the screen at the default
window size. It saves `picker-start.png` and `picker-start-notices.png` here.

`disk_rebuild_rename_test.py` scans the jigsaw image, makes a Disk Rebuild, stops the app, turns
the saved map back into what an older build wrote (the old table, column and index names), and
starts the app again on the same throwaway home. It checks the map is renamed on open, the copy
`map.sqlite.before-disk-rebuild-rename` is made and equals the old map, the Disk Rebuild is still
listed in the Rebuild tab, and every row is where it was. It needs `jigsaw.img`.

## Which tests to run, and when

CI does not run the window tests (they need a built app and a screen), so nothing else catches a
change that breaks them. A pull request that changes how a source opens, the start screen, the
header or `harness.py` must say in its description which of these it ran and the pass count of
each. **Before a release candidate is built, run all of them, one at a time, on that build:**

| Tests | Needs | Covers |
|---|---|---|
| `open_door_test.py`, `device_picker_test.py`, `raw_disk_test.py`, `raw_scan_test.py`, `raw_disk_lanes_test.py`, `raw_disk_crowded_test.py` | `jigsaw.img` for the first three and the lanes test | the one Open, the picker, the saved scans, the start screen tree, raw maps and scans |
| `modes_test.py`, `click_through_test.py`, `copy_guard_test.py`, `disk_rebuild_rename_test.py`, `legend_test.py`, `lost_label_test.py`, `types_test.py`, `workbench_test.py`, `isolate_hover_test.py`, `preview_scheme_test.py`, `chat_test.py` | `jigsaw.img` | the filesystem workspace opened from an image of one volume |
| `macos_writes_test.py` | `macos.img` | what macOS wrote |
| `archive_file_test.py` | `arctest.img` | archives |
| `films_test.py` | `filmtest.img` | films |
| `mounted_warning_test.py` | `hdiutil` | the mounted warning, on a throwaway image |
| `browse_probe.py`, `browse_first_probe.py`, `mount_block_probe.py`, `mount_block_race.py` | as above | probes: they print and have no pass count; read their output |

A test that opens an image through `harness.open_image` or `scan_and_reopen` opens **an image of one
volume**: its first bytes are a recognised volume boot sector, so it opens its filesystem straight
away, as a person opening a partition image does. `jigsaw.img` is such an image. The harness stops,
saying why, if an image opens as a whole disk instead; whole disks are driven by `open_door_test.py`.

## The images

The images and everything else the tests write stay in this folder and are ignored by git.
Make the ones a test needs before running it:

| Image | Made by |
|---|---|
| `jigsaw.img` | `SALVAGE_JIGSAW_IMAGE=$PWD/tests/window/jigsaw.img cargo test -p captainsalvage --test synthetic write_the_jigsaw` |
| `macos.img` | `SALVAGE_MACOS_IMAGE=$PWD/tests/window/macos.img cargo test -p captainsalvage --test synthetic write_the_macos_visited` |
| `arctest.img` | `tests/window/make_arctest.sh` (archives) |
| `filmtest.img` | `tests/window/make_filmtest.sh` (two 70 MB films) |

The mount-block tests (`mounted_warning_test.py`, `mount_block_probe.py`, `mount_block_race.py`)
make and attach their own throwaway disk images with `hdiutil`.

## What the harness does

`harness.py` starts a hidden copy of the app and talks to it:

- **A throwaway home.** The copy runs with `HOME` set to a new folder here, so it never sees
  your settings, workspaces or flagged disks. `SALVAGE_TEST_HOME` names a folder to use instead
  and keeps it afterwards.
- **Copies stay in the test.** With `SALVAGE_TEST_UI` set, the app lists no `/Volumes` drive as
  a place to copy to, and refuses any copy or copy check outside its home, the temp folder or
  `SALVAGE_TEST_COPY_ROOT` (this folder). `copy_guard_test.py` checks this.
- **Its own identity.** The bundle is copied to `testapp/` under a new bundle id each run, so a
  network filter's rule for the real app does not hold the test's local ports, and its browser
  storage is its own. `SALVAGE_TEST_KEEP_ID` runs the binary as built instead.
- **No App Nap.** The copy is started with `-NSAppSleepDisabled YES`, because a hidden app that
  naps stops answering and the test stalls with "the window never answered". Set
  `SALVAGE_TEST_NAP` to leave App Nap on when reproducing a nap problem.

`start`, `open_image` and `scan_and_reopen` get the window to a scanned image; `call` runs an
MCP tool; `js` runs code in the window; `probe` and `wait_for` read a value back from it; `check`
records a result; `finish` stops the app and prints the total.

## When a test fails to start

- "the test copy never published its address": the app did not start. Run the binary by hand
  with the same environment to see why, and check that the build finished.
- "the window never answered": App Nap or a busy machine. Check that nothing else heavy is
  running, and that `SALVAGE_TEST_NAP` is not set.
- A test that stops part way leaves the copy running only until `finish` runs. If it was killed,
  quit any "Salvage Test" app left behind before starting another.
