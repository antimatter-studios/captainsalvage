# Media layout

- `changelog/v1/`, `changelog/v2/`, `changelog/v3/`: unchanged historical screenshots,
  grouped by the interface revision they illustrate. The root README uses these to
  show development over time; they are not illustrations of the current release.
- `manual/`: current user-manual screenshots. All disk contents and filenames are
  synthetic. The manual at `docs/manual/index.html` links here with relative paths.

Keep future historical screenshots under `changelog/<revision>/` and current manual
images under `manual/`. When publishing the manual, preserve `docs/manual/` and
`media/manual/` relative to the same site root. No bundling or remote assets are needed.

## How the manual screenshots were made

The source was built with `CARGO_TARGET_DIR="$PWD/.work/target" python3 scripts/release.py build`,
without signing credentials. The local, git-excluded `.work/window-tests/harness.py`
launched an independently renamed, ad-hoc-signed copy of the resulting app with a
throwaway HOME and an off-screen window. The owner's installed app was not controlled.

Only temporary copies of the synthetic `jigsaw.img`, `arctest.img`, and `macos.img`
fixtures were opened and scanned. In the temporary `jigsaw.img` copy only, the
signature-only JPEG payloads were replaced with a generated 64 × 64 colour gradient,
encoded by macOS `sips` and padded to the existing cluster size. This makes the
otherwise deliberately incomplete test pictures decodable without changing their
directory entries; the overwritten example remains intentionally undecodable.
The original fixtures were not changed. Images were opened as ordinary files, not
mounted. The native window stayed at its default 1560 × 980 logical pixels. The
`set_theme` tool selected light appearance, and `screenshot_app` captured each view.
The optional dark example uses the same window. PNGs retain the capture's resolution.

Attached-device rows were omitted from the start-screen illustration; the capture
session hid destination-volume choices and used an explicitly entered folder
inside its temporary directory for the real synthetic copy and verification. The
Connect illustration replaces the session's bearer token with `REDACTED`. The Chat
with an agent illustration shows the idle composer: no external agent was invoked.
Its screenshot retains the earlier tab label. These
are documentation-only presentation safeguards, not changes to the app.
The Activity illustration omits capture-driver calls so recovery operations remain
readable. The red safety indicator is expected for writable, disposable image files.

Local source and destination paths are strongly blurred in every manual screenshot,
including the start-screen field, window headers, copy fields, copy report, and
Activity details. Pixels outside those path-text boxes remain unchanged.

Every committed PNG was opened for visual inspection before inclusion and contains
only synthetic recovery data. The test instances opened only the synthetic source
images. No real recovery disk was opened, mounted, or scanned. Do not use the legacy
`scripts/take-screenshots.py` for this procedure: it inherits an existing session,
which is unsuitable when a private disk is attached.
