# Captain Salvage

Find the lost treasure on your old, broken drives.

Captain Salvage is a read-only disk recovery tool for macOS. When a repair tool
says a volume is fine but your files are gone, the data is often still there,
with only the links to it cut. This app reads the disk directly, maps what it
finds, and helps you get your files back, without ever writing to the disk.

> **Early development.** It works on one filesystem, exFAT, and has been tested
> on one real damaged volume plus synthetic test volumes. It is not released yet.

## Development so far

Each version gets new screenshots, taken from the real damaged test disk. File names
on that disk are private, so they are blurred.

### Latest

The overview says what has been found, and the map shows what explains every part of
the disk. Blue is claimed by a named file, orange is a FAT chain with no name yet, and
grey is not explained yet.

![Recovery overview, with a large figure for data found by name above a breakdown of the disk](media/v3/overview.png)

Pick any file to read its bytes, see why it got its score, and copy it out. This film's
header shows a healthy AVI file, in 3,032 pieces with none missing.

![A lost film selected, with its hex dump and confidence details](media/v3/file-and-bytes.png)

Select anything and ask Claude, who checks with the same tools the app uses.

![Claude answering a question about the selected cluster in the chat sidebar](media/v3/ask-claude.png)

Copy out checks the destination, and refuses the disk being recovered.

![The Copy out tab, with its destination, salvage drive and confidence floor](media/v3/copy-out.png)

The Unexplained list zooms the map to free space nothing accounts for yet.

![An unexplained region selected, zoomed on the map, with its bytes showing an MP4 header](media/v3/unexplained-region.png)

It follows your Mac's dark mode too.

![The recovery overview in dark mode](media/v3/overview-dark.png)

### Earlier versions

The first version looked like this. It worked, but the map hid the 32 GB in use,
because each column showed only its largest category.

![The first version, with a mostly blue map and plain grey panels](media/v1/first-version.png)

Next, each map column became a stack showing its real mix, so small things stay visible.

![The second version, whose map shows a dark band of in-use data among the blue](media/v2/proportional-map.png)

## What makes it different

Most tools give a yes or no. A checksum fails, so the file is gone. Captain Salvage
treats damage as something to route around:

- **It weighs damage.** A damaged entry gets a lower confidence score, not a
  rejection. If it points at a real folder, the trail continues.
- **It shows the whole disk.** The map shows what explains every cluster: a named
  file, a folder, a FAT chain with no name, or nothing yet. Zoom from the whole
  volume down to single bytes.
- **It keeps your theories separate from the disk.** A hypothesis, such as "these
  clusters belong to this file", is an overlay you can switch off. The map
  recalculates, and the disk is never touched.
- **It tests its guesses.** It walks PNG chunks, JPEG markers, MP3 frames and zip
  CRCs through a reassembled file, and reports where the file breaks.
- **It looks inside archives.** Open a zip, tar, tar.gz, tar.xz, tar.bz2, tar.zst, gz, xz,
  bz2, zst or 7z to see its files as a tree, read live from the disk. A file whose name was
  lost is recognised by its first bytes. If a zip has lost its directory, the list is rebuilt
  from the headers of the files inside. A damaged tar lists as far as it can be read. Each
  file can be checked: against its own checksum in a zip or 7z, and against the stream's
  checksum for a compressed file.
- **An AI can investigate with you.** Select anything and ask an agent about it, in the
  Ask agent tab: Claude Code, Codex or opencode, with the model you choose. The agent works
  through the same tools as the app, over MCP, and has no write path to the disk.

## Safety

This app never writes to the disk being recovered. That is the rule the design starts from.
macOS can, though, and that can happen before the app is open. See
[Before you plug the disk in](#before-you-plug-the-disk-in).

- Devices are only ever opened read-only, and the read interface has no write method.
- The volume stays unmounted during a scan, because macOS writes to a volume when it mounts it.
- The app checks that the boot regions, FAT, bitmap and root directory still match
  the hash first recorded here. That only covers changes after it was recorded.
- The app follows the kernel's write counter for the disk, and warns if it moves while
  the volume is open. The counter covers the whole disk and has no dates, so it is evidence
  and not proof.
- After a scan, the Overview lists each date the volume's `.Spotlight-V100` and `.fseventsd`
  folders changed, as a record of when macOS last wrote there.
- While the app is open, macOS is told not to put disks to sleep. A damaged old disk can
  drop off USB when woken, and each return lets macOS mount it and write to it again. A
  drive's own firmware may still sleep.
- If the disk is disconnected, the app says so, and stops scans, file tests and copies
  instead of treating failed reads as damage. When the volume comes back under a new disk
  number, the app finds it and warns if macOS has mounted it.
- Copy out refuses any destination on the disk being recovered, including through
  an APFS container, and refuses if it cannot tell which disk a destination is on.
- Copy out never overwrites a file. Each copy is hashed as it is read, then hashed
  again from the destination.
- Only the person using the app can start a copy or choose a salvage drive. An AI can
  plan a copy, but not start one.
- While the app is open, macOS cannot mount a volume the person blocked. See
  [Before you plug the disk in](#before-you-plug-the-disk-in).

### Before you plug the disk in

macOS mounts a volume as soon as its disk is attached, and writes to it while it stays
mounted. On one real recovery, a damaged volume stayed mounted for 13 minutes before anyone
unmounted it. In that time macOS changed 36 directory entries, about 0.8 MB, inside
`.Spotlight-V100` and `.fseventsd`, and created files on clusters that had been unclaimed.
Those clusters could have held lost data.

Captain Salvage does not unmount anything itself, because unmounting writes to the disk too.
Set this up before you start:

1. Block mounting. Each volume on the start screen, and on the Overview of the open disk, has
   **Block mounting while Captain Salvage is open**. While the app is open, macOS refuses
   every mount of a blocked volume: when the disk is plugged in, from Finder, from Disk Utility
   and from `diskutil mount`. It needs no administrator rights and changes nothing on the
   system. When the app quits, macOS mounts as usual again, so plug the disk in only with the
   app open. Volumes you have opened before can be blocked before the disk is attached. A new
   one is mounted once when first plugged in: block it, then unmount it once.

   An `/etc/fstab` line with `noauto` is not enough. On one real recovery, macOS mounted the
   volume about 20 seconds after it was attached, with the line in place.
2. Keep disks awake when the app is closed too. While it is open, it does this for you.

   ```
   sudo pmset -a disksleep 0
   ```

3. Do not run First Aid or `fsck` on it, because both of them write.

If the volume is already mounted, the choice is yours. Staying mounted lets macOS keep writing,
and unmounting flushes pending writes, so both change the disk. One real unmount added 23 write
operations and 98 KB. If the disk keeps dropping off, check the cable and power, and avoid hubs.

An agent can block a volume over MCP, because that only protects it. Only the person using
the app can turn a block off.

### Going back to normal

Turn **Block mounting** off for the volume, and set disk sleep back:

```
sudo pmset -a disksleep 10
```

Use the value `disksleep` had before if it was not 10. If you added an `/etc/fstab` line for
the volume with an earlier version, remove it with
`sudo sed -i '' '/^UUID=YOUR-VOLUME-UUID /d' /etc/fstab`.

## How far to trust the results

Confidence scores are estimates. Testing on one real damaged volume found:

| Test | Result |
|---|---|
| PNG files, checksums on every chunk | 25 of 25 fully valid |
| JPEG files, structure only, because JPEG has no checksums | 25 of 25 at least 99.99% valid |
| Zip archives, CRC on every member | 6 of 7 fully valid |
| A 1.1 GB film in 4,042 pieces, full decode | Decoded, with frames checked by eye |

The seventh zip had been scored at 100% confidence, yet one of its 376 members fails
its CRC. Bad data inside a file can look perfect from the outside, so test or open
what matters. Nothing is recovered until it is safely on another disk and you have
checked it. Some files will not come back.

## Running it

The app opens on a start screen, and never reopens a disk by itself, because disk numbers
change on each attach and the same path can be a different disk. Choose the damaged disk
there. A volume you have opened before is recognised by its UUID, with what was found on it.

You need macOS, Rust and Node.js.

```sh
npm install
npm run app
npm run install:user
open "$HOME/Applications/Captain Salvage.app"
```

`npm run install:user` copies the build into `~/Applications`, for this user only.
`npm run install:system` copies it into `/Applications`, for everyone on the Mac, and needs an
administrator account. Either replaces the copy already there, and stops if that copy is
running. To try a build without installing it, open
`target/release/bundle/macos/Captain Salvage.app`.

**First, keep the damaged volume unmounted, and don't run First Aid or `fsck` on it.**
A repair may be what made your files disappear, and it could overwrite what is left.

```sh
diskutil list                          # find the disk, such as disk4s2
diskutil unmount /dev/disk4s2
sudo chgrp staff /dev/rdisk4 /dev/rdisk4s2
```

The last command gives your account read-only access to the raw device until the disk
is reconnected. Reconnecting also mounts the volume again and can change its number.

Then open the app, and enter the partition's device, such as `/dev/rdisk4s2`.

### Scanning from a terminal

```sh
cargo build --release -p salvage-core
./target/release/salvage-scan /dev/rdisk4s2
```

A scan reads the whole partition once, and can stop and resume at any time.

### Tests

```sh
cargo test -p salvage-core
```

### Screenshots

```sh
scripts/take-screenshots.py media/v4
```

This drives a second copy of the app off screen, with file names blurred. Look at every
image before committing it, because bytes and chat answers can still hold names.

## Showing a canvas in Finder

A canvas in the Rebuild tab of the right panel can appear in Finder as a folder, with the
rebuilt tree inside. It is read only: folders are listed and files are read through the app's
private port, straight from the map, and nothing done in Finder reaches the disk. The app must
be open on that disk for Finder to read it.

This needs **Captain Salvage Finder**, a small helper app that holds the File Provider
extension. macOS only runs such an extension when the team's signature and provisioning
profiles are on it, so it is built separately, and the profiles and key stay outside the repo:

```sh
CS_SIGN_KEY=development.key.pem CS_SIGN_CERT=development.cer.pem \
CS_APP_PROFILE=app.provisionprofile CS_EXT_PROFILE=fileprovider.provisionprofile \
  macos/finder/build.sh --install
```

The first time, turn it on in System Settings, General, Login Items & Extensions, File
Providers. When the helper is installed, the Rebuild tab shows **Show in Finder**.

Finder lets a program write into the folder, but the extension refuses every change, so a file
saved there stays on this Mac only and is never sent anywhere.

## Using it from an MCP client

The app runs an MCP server on localhost, protected by a token. Its **Connect** tab shows
the command that adds it to Claude Code. There are more than 40 tools, covering raw reads,
FAT chains, pattern search, read-only SQL on the map, format tests, overlays and sandboxed
scripts.


### Asking an agent from a terminal

`salvage-agent` asks the same agents as the Ask agent tab, about the disk open in the app.
The app must be running.

```sh
cargo run -p salvage-core --bin salvage-agent -- list
cargo run -p salvage-core --bin salvage-agent -- models codex
cargo run -p salvage-core --bin salvage-agent -- ask --agent codex "How many clusters does this volume have?"
```

The answer prints as it arrives, each tool the agent uses shows on standard error, and the
last line there is the session, to continue with `--session`. `--json` prints every event as
one line of JSON instead. Each agent sees only the app's tools. Claude Code gets no built-in
tools at all. opencode's own file, shell and web tools are turned off. Codex keeps its shell,
but in its read-only sandbox.

## Libraries and licenses

Captain Salvage is meant to be sold, so everything linked into it must be usable under a
permissive license. MIT is preferred. Apache-2.0, BSD, ISC, Zlib, Unicode and public-domain
style licenses are allowed too. MPL-2.0 is allowed for libraries used unmodified. Nothing
under the GPL family. CI runs `scripts/licenses.py check` on everything the app ships, and
`THIRD-PARTY-LICENSES.md`, bundled inside the app, holds each library's license text.

The libraries the app names itself:

<!-- dependencies:start -->
| Library | Version | License | Used by |
|---|---|---|---|
| `@tauri-apps/api` | 2.12.4 | Apache-2.0 OR MIT | window |
| `anyhow` | 1.0.104 | MIT OR Apache-2.0 | captain-salvage, salvage-core |
| `block2` | 0.6.2 | MIT | captain-salvage |
| `bzip2-rs` | 0.1.2 | MIT/Apache-2.0 | salvage-core |
| `flate2` | 1.1.10 | MIT OR Apache-2.0 | salvage-core |
| `image` | 0.25.10 | MIT OR Apache-2.0 | salvage-core |
| `libc` | 0.2.191 | MIT OR Apache-2.0 | salvage-core |
| `objc2` | 0.6.5 | MIT | captain-salvage |
| `objc2-foundation` | 0.3.2 | MIT | captain-salvage |
| `react` | 19.3.1 | MIT | window |
| `react-dom` | 19.3.1 | MIT | window |
| `rhai` | 1.27.0 | MIT OR Apache-2.0 | salvage-core |
| `rusqlite` | 0.41.1 | MIT | salvage-core |
| `ruzstd` | 0.9.1 | MIT | salvage-core |
| `salvage-core` | 0.1.0 | MIT | captain-salvage |
| `serde` | 1.0.229 | MIT OR Apache-2.0 | captain-salvage, salvage-core |
| `serde_json` | 1.0.154 | MIT OR Apache-2.0 | captain-salvage, salvage-core |
| `sevenz-rust2` | 0.24.2 | Apache-2.0 | salvage-core |
| `sha2` | 0.11.1 | MIT OR Apache-2.0 | salvage-core |
| `tar` | 0.4.46 | MIT OR Apache-2.0 | salvage-core |
| `tauri` | 2.12.5 | Apache-2.0 OR MIT | captain-salvage |
| `tiny_http` | 0.12.0 | MIT OR Apache-2.0 | salvage-core |
| `xz4rust` | 0.2.3 | MIT | salvage-core |
| `zip` | 9.4.1 | MIT | salvage-core |
<!-- dependencies:end -->

Run `python3 scripts/licenses.py write` after changing a dependency, to update this list and
the bundled notices.

## Distribution

The aim is the Mac App Store, but **the current design cannot ship there**. See
[#1](https://github.com/antimatter-studios/captainsalvage/issues/1). In short, store apps
must be sandboxed and cannot ask for root, and this app reads raw devices, runs the `claude`
command and uses a private window API. A build signed and notarized for direct download
does not have these limits.

## Playing more formats

The window plays common formats by itself. Install [ffmpeg](https://ffmpeg.org) with
`brew install ffmpeg` and the app finds it and also plays AVI, MKV, MPG, FLV, WMV, WMA and
others, converting them as they play. It also shows a frame from each video in the grid.
ffmpeg is optional, is not bundled, and nothing it converts is saved.

## Changelog

<!-- changelog:start -->

The latest versions. [CHANGELOG.md](CHANGELOG.md) has the full history.

### v0.2.0 — 2026-11-05

**Added**

- Release every feature and fix automatically ([#60](https://github.com/antimatter-studios/captainsalvage/pull/60))
- Show the version on the start screen ([#59](https://github.com/antimatter-studios/captainsalvage/pull/59))
- Show how to keep a disk safe on the start screen, and undo it ([#58](https://github.com/antimatter-studios/captainsalvage/pull/58))
- Open on a start screen, and never reopen a disk by itself ([#57](https://github.com/antimatter-studios/captainsalvage/pull/57))
- Show what a zip holds as a file tree, read live from the disk ([#51](https://github.com/antimatter-studios/captainsalvage/pull/51))
- Warn that macOS writes to a damaged volume, and show the evidence ([#50](https://github.com/antimatter-studios/captainsalvage/pull/50))
- Show identical files as one in the browse list ([#46](https://github.com/antimatter-studios/captainsalvage/pull/46))
- Test found files in the background and score them on the result ([#45](https://github.com/antimatter-studios/captainsalvage/pull/45))

**Fixed**

- Say when the disk is disconnected, and keep disks from sleeping ([#56](https://github.com/antimatter-studios/captainsalvage/pull/56))
- Report what survives in a damaged file, and stop calling text keys broken zips ([#47](https://github.com/antimatter-studios/captainsalvage/pull/47))

### v0.1.0 — 2026-11-05

**Added**

- Show the version, counting builds and releases ([#38](https://github.com/antimatter-studios/captainsalvage/pull/38))
- Mark the selected spot on the map with arrows ([#37](https://github.com/antimatter-studios/captainsalvage/pull/37))
- Name the app Captain Salvage and add a light and dark switch ([#35](https://github.com/antimatter-studios/captainsalvage/pull/35))
- Navigate the sidebar with the arrow keys ([#34](https://github.com/antimatter-studios/captainsalvage/pull/34))
- Drag the edges of the middle column to resize it ([#33](https://github.com/antimatter-studios/captainsalvage/pull/33))
- Widen the details panel and preview files in it automatically ([#32](https://github.com/antimatter-studios/captainsalvage/pull/32))
- Play more formats through an installed ffmpeg ([#31](https://github.com/antimatter-studios/captainsalvage/pull/31))
- Browse files by type and preview them live from the disk ([#30](https://github.com/antimatter-studios/captainsalvage/pull/30))
- Add a recovery basket with live progress from any process ([#24](https://github.com/antimatter-studios/captainsalvage/pull/24))

**Fixed**

- Let a resumed copy count files already on the destination ([#26](https://github.com/antimatter-studios/captainsalvage/pull/26))
- Make lens-scan wait for a basket copy to finish ([#25](https://github.com/antimatter-studios/captainsalvage/pull/25))

**Documentation**

- Name Chris Thomas as the copyright holder ([#29](https://github.com/antimatter-studios/captainsalvage/pull/29))
- License the project under MIT ([#28](https://github.com/antimatter-studios/captainsalvage/pull/28))

**Other**

- Add a changelog, CI and a release pipeline ([#36](https://github.com/antimatter-studios/captainsalvage/pull/36))

<!-- changelog:end -->

## Releases

Each version is on the [releases page](https://github.com/antimatter-studios/captainsalvage/releases),
built and tested by GitHub Actions, with its notes from `CHANGELOG.md`.

Releases after v0.2.0 have no build attached, because the app will be sold and anything on
GitHub can be downloaded by anyone who can read the repository. Builds will go to private storage.
To build one yourself, run `python3 scripts/release.py build`. Builds are for Apple Silicon and are
not signed yet, so macOS asks you to approve the app the first time.

**Version numbers** follow what was merged since the last release:

| Merged | Version |
|---|---|
| A new feature (`feat:`) | The minor number goes up: 0.1.0 to 0.2.0 |
| A fix (`fix:`) | The patch number goes up: 0.2.0 to 0.2.1 |
| Anything else (`docs:`, `test:`, `chore:`) | No change |

Several merges take the largest of these, once. Whoever opens a pull request, person or agent,
assesses whether it fixes something broken or adds something new, and titles it to match.
[AGENTS.md](AGENTS.md) has the rule.

Changes not yet released are fragments in [`changelog.d/`](changelog.d/), one per pull request.
Run `scripts/install-hooks.sh` once per clone: on a `feat/` or `fix/` branch, the pre-commit hook
writes the first one from what is staged, using an agent for the wording.

To cut a release, run `scripts/create-release`. It shows what would go out and stops for a yes,
then opens the release pull request. It combines the fragments into `CHANGELOG.md`, copies the
newest ten versions into this README, and sets the version. When that has merged, run
`scripts/create-release --tag v0.3.0`, which tags `main` and pushes. The tag starts the pipeline.

Going to 1.0.0 is a deliberate choice: run `scripts/create-release 1.0.0`.

To try the pipeline without publishing anything, run the `release` workflow by hand.

## Roadmap

Bugs, ideas and progress are all in the
[issue tracker](https://github.com/antimatter-studios/captainsalvage/issues).

## License

MIT. See [LICENSE](LICENSE).
