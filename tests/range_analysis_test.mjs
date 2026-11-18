import assert from "node:assert/strict";
import { test } from "node:test";
import { findingExtent, mergeFindings } from "../src/rawDisk.ts";

const finding = (overrides) => ({
  format: "FAT", structure: "directory entries", status: "island", offset: 100, length: 192, decodedBytes: 192,
  units: 6, confidence: 0.5, level: "medium", signature: false, volumeStart: null, volumeLength: null,
  bootOffset: null, supported: true, detail: "", names: [], missing: [], offer: null, insideVolume: null, ...overrides,
});

test("a finding that names a volume is selected as that volume, otherwise as its decoded run", () => {
  assert.deepEqual(findingExtent(finding({})), { start: 100, length: 192 });
  assert.deepEqual(findingExtent(finding({ volumeStart: 4096, volumeLength: 8192 })), { start: 4096, length: 8192 });
});

test("a second search adds new findings and keeps one entry for a structure found twice", () => {
  const first = [finding({ offset: 100, confidence: 0.4 })];
  const merged = mergeFindings(first, [finding({ offset: 100 }), finding({ offset: 900, confidence: 0.9 })]);
  assert.equal(merged.length, 2);
  assert.equal(merged[0].offset, 900);
});
