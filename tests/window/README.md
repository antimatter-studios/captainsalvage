# Window tests

These scripts drive the real app window through its MCP server, against small synthetic disk
images. They are for checking a change in the window itself, where the Rust and TypeScript
tests cannot reach. Each script prints `PASS` or `FAIL` per check and a total at the end.

They only ever open the images made here. Never point them at a real disk, and never use them
to open or capture real files.

## Running one

Build the test copy of the app first, then pass the binary inside its bundle:

```sh
scripts/build-test-app.sh
python3 tests/window/modes_test.py "target/test-hooks/release/bundle/macos/Captain Salvage.app/Contents/MacOS/captain-salvage"
```

The tests need hooks that the shipped app does not have: a way to run script in its window
(`ui_eval`), and the environment variables `SALVAGE_TEST_UI`, `SALVAGE_TEST_HIDDEN`,
`SALVAGE_TEST_DARK`, `SALVAGE_TEST_COPY_ROOT`, `SALVAGE_TEST_STARTUP_DISKS` and `SALVAGE_AGENT_<ID>`.
They are compiled in only by the Cargo feature `test-hooks`, which is off in `npm run app`,
`scripts/release.py build` and the release workflow. `scripts/build-test-app.sh` builds the same
commit with the feature, into `target/test-hooks`, so it never replaces the app in
`target/release` or the one you installed. The harness refuses a binary that lacks the hooks.

For a release candidate, run the window tests against a test-hooks build of the same commit as the
candidate. Then prove the app that ships has none of the hooks, with
`scripts/check-test-hooks.sh absent "target/release/bundle/macos/Captain Salvage.app"`. The release
workflow runs that check on its own build, and the same script run with `present` checks the test
build.

Run one test at a time, and not while a build is running. Several at once, or a test beside a
release build, slows the machine enough for the tests to time out.

`open_door_test.py` is the one door. It opens a synthetic whole disk (an MBR and one exFAT
partition) and checks the whole-disk scope, Open on its partition, the interpretation picker (the
detected filesystem marked, a new scan on a change with the earlier one kept, a reason when there
is no evidence), the saved scans list with its editable names, and that the same partition opened
alone from a file of its bytes is the same source and scan. It needs `jigsaw.img` and saves
`door-header.png` here.

`title_area_test.py` checks the row across the top of the window: the header icon is the app icon's file, the
two titles share a size and weight, the left and right halves have the same height and top, both drag the
window, and a long device name is cut short with its path as a tooltip without pushing Change disk, the
picker or the controls out of the row, also at the smallest window width. It needs `jigsaw.img` and saves
`title-area.png`, `title-area-long.png` and `title-area-dark.png` here.

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

`bad_sectors_raw_test.py` writes a made-up health map (a place the drive reset during a read, a failed read, a
suspect band and a slow range) into the test copy's catalog, opens a made-up 24 MiB disk, and checks that the
raw-disk map draws the three kinds of bad sectors as red marks that flash (a one-second pulse, computed style),
not the slow one, that the legend has the "Bad sectors: reading here drops the drive" entry, and that the
tooltip says where, how it was found and when. It saves `bad-sectors-raw.png`. `bad_sectors_volume_test.py`
does the same on the volume map of `jigsaw.img`, with four bad clusters. The reduced-motion look (solid red with a
hatch) is a media query the window test cannot switch; `tests/bad_sectors_test.mjs` checks the stylesheet for it.

`agent_rebuild_test.py` is the Rebuild tab with an agent in it. The agent's side is the MCP tools (it
creates a Disk Rebuild, places pieces, makes a folder, merges a piece); the person's side is the
window. It checks that the change shows at once with the Agent mark and the bar naming the last
change, that the Disk Rebuild is named as made by the agent, that the activity log has it as the
agent's, that an agent is refused `disk_rebuild_copy_start` and `disk_rebuild_undo`, and that
Undo, Undo all agent changes (which removes a Disk Rebuild the agent made) and Restore to before the
agent (after a confirmation, putting the person's own Disk Rebuild back as it was) work. It needs
`jigsaw.img`.

`empty_export_test.py` opens a synthetic exFAT volume with a live file that has no bytes (and a
deleted one), right-clicks the empty file in the Explorer tree, exports it from the menu, and checks
that Export is offered, the plan counts one file, and an empty file arrives with nothing else (issue
#176). It needs `emptyfiles.img`, copies only into this folder, and removes what it copied.

`ntfs_flow_test.py` opens a synthetic NTFS image (issue #171), scans it, opens the root and a folder in
the Explorer tree, reads a file stored in two runs through the window's range read (whole, and a
range across the join of the runs), reads a file in a folder whole, and exports the fragmented file from
the right-click menu, checking the copy byte for byte. It needs `ntfs.img`, never mounts or attaches
anything, copies only into this folder and removes what it copied.

`ntfs_mount_block_test.py` (issue #171) checks the mount block of an NTFS volume that macOS gives no volume UUID. For an
MBR layout and a GPT layout, it makes a throwaway image with `hdiutil create`, writes `ntfs.img` into the attached
image's partition 1 with `dd` (on the node `attach` printed, nothing else), and lists only that one disk
(`SALVAGE_TEST_STARTUP_DISK_FILE`). It checks the volume is listed as NTFS with no UUID and a place key, that the Block
mount checkbox is offered, that a block set while the volume is mounted shows the app's Unmount warning (which only
the person's button clears), that `diskutil mount` is then refused, that the image attached again under another disk
number is not mounted by macOS, that another disk of the same size and layout (another NTFS serial) mounts as usual,
and that the person's click on the checkbox lets the volume mount again. It needs `ntfs.img`, `hdiutil` and `diskutil`,
touches no real disk, and removes its images.

`start_follows_disks_test.py` (issue #250) makes a small synthetic exFAT disk image of its own, starts the app on
the start screen with a list that names only the one disk the test attached (`SALVAGE_TEST_STARTUP_DISK_FILE`, a
file holding that disk's identifier, honoured in a test-hooks build with `SALVAGE_TEST_UI`, so no real disk is ever
listed), and attaches the image, mounts its volume, unmounts it and detaches it with `diskutil`. It checks that the
screen shows each step by itself within the check interval, that a row that did not change is the same element
afterwards, that the list never says "Looking" while following, and that the engine answers `unchanged` to a look at
the fingerprint of the report on screen. Every command names the identifier `attach` printed for this image.

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
| `start_follows_disks_test.py`, `ntfs_flow_test.py`, `agent_rebuild_test.py`, `modes_test.py`, `click_through_test.py`, `copy_guard_test.py`, `empty_export_test.py`, `disk_rebuild_rename_test.py`, `legend_test.py`, `lost_label_test.py`, `types_test.py`, `workbench_test.py`, `isolate_hover_test.py`, `preview_scheme_test.py`, `chat_test.py` | `jigsaw.img` (`emptyfiles.img` for `empty_export_test.py`, `ntfs.img` for `ntfs_flow_test.py`; `start_follows_disks_test.py` makes its own image with `hdiutil`) | the filesystem workspace opened from an image of one volume |
| `macos_writes_test.py` | `macos.img` | what macOS wrote |
| `archive_file_test.py` | `arctest.img` | archives |
| `films_test.py` | `filmtest.img` | films |
| `mounted_warning_test.py` | `hdiutil` | the mounted warning, on a throwaway image |
| `ntfs_mount_block_test.py` | `ntfs.img`, `hdiutil` | the mount block of an NTFS volume with no UUID, on throwaway images |
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
| `emptyfiles.img` | `SALVAGE_EMPTY_IMAGE=$PWD/tests/window/emptyfiles.img cargo test -p captainsalvage --test synthetic write_the_empty` |
| `ntfs.img` | `SALVAGE_NTFS_IMAGE=$PWD/tests/window/ntfs.img cargo test -p captainsalvage --test ntfs write_the_ntfs` |
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
- **Copies stay in the test.** In the test-hooks build, with `SALVAGE_TEST_UI` set, the app lists no `/Volumes` drive as
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
