# Window tests

These scripts drive the real app window through its MCP server, against small synthetic exFAT
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

## The images

The images and everything else the tests write stay in this folder and are ignored by git.
Make the ones a test needs before running it:

| Image | Made by |
|---|---|
| `jigsaw.img` | `SALVAGE_JIGSAW_IMAGE=$PWD/tests/window/jigsaw.img cargo test -p salvage-core --test synthetic write_the_jigsaw` |
| `macos.img` | `SALVAGE_MACOS_IMAGE=$PWD/tests/window/macos.img cargo test -p salvage-core --test synthetic write_the_macos_visited` |
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
