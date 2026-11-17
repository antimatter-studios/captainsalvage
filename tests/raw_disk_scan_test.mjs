import assert from "node:assert/strict";
import { test } from "node:test";
import { applyScanBatch, scanEstimate } from "../src/rawDisk.ts";

test("raw scan ETA uses active bytes and elapsed time", () => {
  assert.deepEqual(scanEstimate(0, 1000, 4000, 2000), { bytesPerSecond: 500, secondsLeft: 6 });
});

test("resumed ETA excludes previously scanned bytes and paused time", () => {
  assert.deepEqual(scanEstimate(2000, 3000, 5000, 1000), { bytesPerSecond: 1000, secondsLeft: 2 });
});

test("ETA is unavailable before a usable sample", () => {
  assert.equal(scanEstimate(0, 0, 5000, 1000), null);
  assert.equal(scanEstimate(0, 1000, 5000, 0), null);
  assert.equal(scanEstimate(0, 1000, 5000, -1), null);
});

test("completed ETA is zero and invalid inputs never show infinity", () => {
  assert.deepEqual(scanEstimate(1000, 5000, 5000, 2000), { bytesPerSecond: 2000, secondsLeft: 0 });
  for (const sample of [[0, 2, 1, 1], [2, 1, 3, 1], [0, 1, 3, NaN], [0, Infinity, 3, 1], [NaN, 1, 3, 1], [0, 1, Infinity, 1], [0, 1, 3, Number.MIN_VALUE]]) {
    assert.equal(scanEstimate(...sample), null);
  }
});

test("committed batches advance the saved scan without changing its identity", () => {
  const disk = { scanId: "scan-1", nextOffset: 512, scanStart: 0, scanEnd: 4096, findings: [{ offset: 32 }], errors: [] };
  const batch = { nextOffset: 1024, findings: [{ offset: 768 }], errors: [{ offset: 512, length: 64 }] };
  const updated = applyScanBatch(disk, batch);
  assert.equal(updated.scanId, "scan-1");
  assert.equal(updated.scanEnd, 4096);
  assert.equal(updated.nextOffset, 1024);
  assert.deepEqual(updated.findings, [{ offset: 32 }, { offset: 768 }]);
  assert.deepEqual(updated.errors, batch.errors);
  assert.equal(disk.nextOffset, 512);
});

test("retrying old failures replaces evidence in the reread range without duplicates", () => {
  const disk = { nextOffset: 512, scanStart: 0, findings: [{ offset: 0 }, { offset: 600 }, { offset: 1024 }], errors: [] };
  const updated = applyScanBatch(disk, { nextOffset: 1024, findings: [{ offset: 600, format: "exFAT" }], errors: [] });
  assert.deepEqual(updated.findings, [{ offset: 0 }, { offset: 1024 }, { offset: 600, format: "exFAT" }]);
});

test("batch application uses the saved start when no checkpoint exists", () => {
  const disk = { nextOffset: null, scanStart: 512, findings: [{ offset: 32 }, { offset: 600 }], errors: [] };
  assert.deepEqual(applyScanBatch(disk, { nextOffset: 1024, findings: [], errors: [] }).findings, [{ offset: 32 }]);
  assert.deepEqual(applyScanBatch({ ...disk, scanStart: null }, { nextOffset: 1024, findings: [], errors: [] }).findings, []);
});
