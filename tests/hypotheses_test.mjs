import assert from "node:assert/strict";
import { test } from "node:test";
import { competition, extentFromEnds, hypothesisFromFinding } from "../src/rawDisk.ts";

const layout = (competing) => ({ segments: [], virtualLength: 0, gaps: [], competing, partitionTable: [] });

test("a hypothesis knows how many rivals it has and whether it is the one read", () => {
  const overlapping = layout([{ start: 50, length: 50, stack: ["b", "a"] }, { start: 100, length: 10, stack: ["b", "c"] }]);
  assert.deepEqual(competition(overlapping, "b"), { rivals: 2, onTop: true });
  assert.deepEqual(competition(overlapping, "a"), { rivals: 1, onTop: false });
  assert.deepEqual(competition(layout([]), "a"), { rivals: 0, onTop: true });
});

test("typed start and end make an extent only when they are whole, ordered and inside the source", () => {
  assert.deepEqual(extentFromEnds("1000", "5000", 10000), { start: 1000, length: 4000 });
  assert.equal(extentFromEnds("5000", "5000", 10000), null);
  assert.equal(extentFromEnds("6000", "5000", 10000), null);
  assert.equal(extentFromEnds("0", "10001", 10000), null);
  assert.equal(extentFromEnds("", "5", 10000), null);
  assert.equal(extentFromEnds("1.5", "5", 10000), null);
  assert.equal(extentFromEnds("-1", "5", 10000), null);
});

test("a finding that names a volume is saved as that volume, with its confidence", () => {
  const saved = hypothesisFromFinding({
    format: "NTFS", structure: "boot sector", status: "volume", offset: 4099, length: 512, decodedBytes: 512, units: 1,
    confidence: 0.95, level: "high", signature: true, volumeStart: 4099, volumeLength: 8_000_000, bootOffset: 4099,
    supported: true, detail: "", names: [], missing: [], offer: null, insideVolume: null,
  }, "NTFS: boot sector");
  assert.deepEqual(saved, { format: "NTFS", start: 4099, length: 8_000_000, confidence: 0.95, bootOffset: 4099, origin: "analysis", label: "NTFS: boot sector" });
});
