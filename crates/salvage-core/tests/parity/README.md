# Pre-driver parity gate

These references were captured from Captain Salvage main
`21ecbaab2178006a81829a807e1c46e1bd657e31`, before the rust-fs-exfat driver
rewire. Only tests and tooling were added for the capture; production code and
dependencies are unchanged. This implements rust-fs-exfat issue #29, not #28.

## Run

From the repository root:

```sh
CARGO_TARGET_DIR=/Users/chris/projects/captainsalvage/target bash scripts/parity.sh
```

`CARGO_TARGET_DIR` is a local shared-build-directory convention, not required by
the gate. Other checkouts can leave it unset or choose their own directory.
The script can also be called from another directory by its absolute path.
An optional first argument selects another reference directory.

The script generates fresh outputs in a temporary directory and uses `diff -u`
to compare both files byte-for-byte. Any test failure, missing reference, or
difference fails the command. It never updates the references. Temporary outputs
are removed on exit. Ordinary `cargo test --locked -p salvage-core` also compares
both references, so the existing CI test job enforces parity without a new job.

## What is frozen

`reference/synthetic.json` reuses the existing `synthetic.rs` builders:

- The live/orphan tree with all damage classes, conflicting/reused entries,
  the volume macOS visited, and the jigsaw volume.
- Completed scan counters, every cluster's content classification, and full
  allocation, content, entropy, ownership, confidence, and folder map layers.
- All `sets`, `nodes`, and `islands` map rows, queried in primary-key order;
  verdict summaries; every entry's public detail, extents, confidence, and
  a SHA-256 of its readable file bytes; source-image SHA-256 hashes.
- Roots, MP3 search, broken files, nameless chains, chain summaries, verdict/reason
  breakdowns, downsampled map rows, volume geometry/boot checks, and chain reports.
- Resolve previews with no changes, a proposed bridge, and a parameter change,
  including the stored entry and overlay list afterwards.
- MCP JSON-RPC tool responses for roots, chain summaries, resolve preview, and
  valid/invalid cluster inspection, including in-band error responses.
- ZIP listing, member checking, and intact/damaged ZIP verifier reports.

A scan stopped after one chunk and resumed must match the *entire* damage-fixture
capture, not just its entry count.

`reference/archives.json` reuses `archives.rs`'s synthetic payloads and archive
builders. It freezes listings and every file-member check for plain tar,
gzip/xz/bzip2/zstd tar, single compressed files, RAR 2.9/5.0 including solid RAR,
and 7z. It includes damaged compressed files, damaged RAR/7z, and a truncated tar.
7z uses the same two synthetic payloads but an in-memory writer, avoiding host
file timestamps. No disk images or archive binaries are committed.

## Canonical form and exclusions

JSON object keys are recursively sorted; arrays retain their public order.
SQL row order is explicitly selected. UTF-8 pretty JSON ends in one newline.
JSON inside MCP text content is canonicalized too, while retaining the envelope,
text-content encoding, request ID, and `isError`. Request and database entry IDs
are fixed/deterministic, not random session IDs.

The exact metadata exclusions are `startedAtMillis`, `updatedAtMillis`,
`resolvedAtMillis`, `recordedAtMillis`, `bytesPerSecond`, `created`, `modified`,
and `accessed`. These remove clocks, scan speed, and file timestamps. Device,
whole-disk, and workspace paths are removed only from `volume_overview`. Logical
synthetic volume-root `path` values use a `volume:` prefix; their complete path
is retained without a host-absolute spelling. Any other absolute-path string
fails the capture rather than being silently stripped.
No confidence values, verdicts, errors, lengths, hashes, or unknown future fields
are normalized away. Fixture dates that are part of explanatory strings remain
synthetic and fixed. The normalization contract has its own tests.

This is a finite regression corpus, not a proof about every possible damaged
volume. It does not snapshot every existing test scenario, every MCP tool or
tool schema, MCP HTTP/auth transport, GUI/media rendering, platform mounting or
device enumeration, copy/export workflows, session/activity clocks, or file
timestamps. Existing tests still cover those independently. It does not open,
mount, unmount, write, or inspect a real device or real private media; all source
bytes come from the existing synthetic builders in memory.

## Reference policy

Keep these committed, small, synthetic-only JSON files unchanged when doing the
driver rewire. A failure must be explained and fixed, not blessed by recapturing
post-rewire output. If an intentional behavior change later needs a new baseline,
review its diff and provenance explicitly. To inspect an uncommitted capture,
create an empty temporary directory and set `SALVAGE_PARITY_OUTPUT_DIR` when
running the two `parity::` test modules. Outputs use create-new mode and cannot
overwrite an existing reference.

The previously ignored `write_the_macos_visited_volume_for_window_tests` and
`write_the_jigsaw_volume_for_window_tests` now run normally. Each writes to its
own temporary workspace, reads back the exact bytes, opens them as an in-memory
volume, and checks geometry and boot checksum. The explicit environment variables
still export those synthetic images for window tests; `--ignored` is no longer
needed. Neither test is removed or silently skipped.
