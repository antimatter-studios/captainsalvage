import assert from "node:assert/strict";
import { test } from "node:test";
import { applyScanBatch, BAND_ROWS, evidenceSummary, itemAt, laneMap } from "../src/rawDisk.ts";
import { crowdedDisk } from "./raw_disk_naive.mjs";

// What the window does with a crowded raw scan, timed without the window. tests/window/
// raw_disk_crowded_test.py runs the real thing on a built app, which CI cannot, so this is the
// check that fails in CI when the work behind the view grows slow again. The limits are many
// times what a Mac needs (a paint takes under 1 ms, a whole scan about 0.1 s), so a slow CI
// machine passes and a return to a painter that scans every finding for every pixel does not
// (that one needed 34-63 ms a paint and 2.4 s a scan, see #181).
const BUCKETS = 4096;
const FRAME_MILLIS = 16;
const FINDINGS = 2000;
const BATCH = 10;

function median(values) {
  return [...values].sort((left, right) => left - right)[Math.floor(values.length / 2)];
}

function laneNames(findings) {
  return evidenceSummary(findings).map((item) => item.format);
}

test("one paint of the lanes and bands with 2000 findings fits in a frame", () => {
  const disk = crowdedDisk(FINDINGS);
  const lanes = laneNames(disk.findings);
  const picture = laneMap(disk, lanes, 0, disk.length, BUCKETS);
  assert.equal(picture.length, (1 + BAND_ROWS + lanes.length) * BUCKETS);
  const samples = [];
  for (let pass = 0; pass < 15; pass++) {
    const started = performance.now();
    laneMap(disk, lanes, 0, disk.length, BUCKETS);
    samples.push(performance.now() - started);
  }
  assert.ok(median(samples) < FRAME_MILLIS, `${median(samples).toFixed(2)} ms for one paint of the lanes`);
});

test("a whole scan that finds 2000 filesystems keeps every batch under a frame, both strips repainted", () => {
  const source = crowdedDisk(FINDINGS);
  const batches = FINDINGS / BATCH;
  let disk = { ...crowdedDisk(0), findings: [], nextOffset: 0, scanStart: 0 };
  const each = [];
  const started = performance.now();
  for (let index = 0; index < batches; index++) {
    // Each batch finds its own findings inside the range it read, as a scan does.
    const first = Math.floor(source.length * index / batches);
    const last = Math.floor(source.length * (index + 1) / batches);
    const batch = {
      nextOffset: last,
      findings: source.findings.slice(index * BATCH, (index + 1) * BATCH).map((item, place) => ({
        ...item, offset: first + Math.floor((last - first) * place / BATCH / 512) * 512,
      })),
      errors: [],
      evidenceLimitReached: false,
    };
    const before = performance.now();
    disk = applyScanBatch(disk, batch);
    const lanes = laneNames(disk.findings);
    laneMap(disk, lanes, 0, disk.length, BUCKETS);
    laneMap(disk, lanes, 0, disk.length / 8, BUCKETS);
    each.push(performance.now() - before);
  }
  const total = performance.now() - started;
  assert.equal(disk.findings.length, FINDINGS);
  assert.ok(median(each) < FRAME_MILLIS, `${median(each).toFixed(2)} ms for a typical batch`);
  assert.ok(total < 1000, `${total.toFixed(0)} ms for the whole scan`);
});

test("picking what is under a click is quick with 2000 findings", () => {
  const disk = crowdedDisk(FINDINGS);
  const started = performance.now();
  for (let pass = 0; pass < 100; pass++) itemAt(disk, disk.length * (pass / 100), disk.length / 1200 * 3);
  const each = (performance.now() - started) / 100;
  assert.ok(each < FRAME_MILLIS, `${each.toFixed(2)} ms for one pick`);
});
