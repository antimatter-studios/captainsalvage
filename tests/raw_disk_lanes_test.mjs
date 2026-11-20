import assert from "node:assert/strict";
import { test } from "node:test";
import { BAND_ROWS, MAP_BAND, MAP_BAND_ALTERNATE, MAP_EXAMINED, MAP_FIRST_FORMAT, MAP_HEALTH_CULPRIT, MAP_HEALTH_SLOW, MAP_HEALTH_UNREADABLE, MAP_UNREADABLE, evidenceStatus, evidenceSummary, itemAt, laneMap, mapRowCount } from "../src/rawDisk.ts";

function finding(offset, format, extra = {}) {
  return { offset, length: 512, format, structure: "boot sector", confidence: "plausible geometry; filesystem not yet opened",
    volumeStart: null, volumeLength: 4096, bootOffset: null, supported: true, ...extra };
}

function disk() {
  return {
    length: 8192,
    partitions: [{ start: 0, length: 2048, label: "", table: "GPT" }, { start: 2048, length: 2048, label: "", table: "GPT" }],
    findings: [finding(0, "NTFS"), finding(6144, "exFAT"), finding(6656, "exFAT")],
    errors: [{ offset: 4096, length: 1024 }], scanStart: 0, nextOffset: 6144,
  };
}

function rows(data, buckets) {
  return Array.from({ length: data.length / buckets }, (_, row) => [...data.slice(row * buckets, (row + 1) * buckets)]);
}

test("the map is a scan row, broad partition bands, then a thin lane for each format", () => {
  const lanes = ["exFAT", "NTFS"];
  const map = rows(laneMap(disk(), lanes, 0, 8192, 8), 8);
  assert.equal(map.length, 1 + BAND_ROWS + lanes.length);
  assert.deepEqual(map[0], [MAP_EXAMINED, MAP_EXAMINED, MAP_EXAMINED, MAP_EXAMINED, MAP_UNREADABLE, MAP_EXAMINED, 0, 0]);
  for (let row = 1; row <= BAND_ROWS; row++) assert.deepEqual(map[row], [MAP_BAND, MAP_BAND, MAP_BAND_ALTERNATE, MAP_BAND_ALTERNATE, 0, 0, 0, 0]);
  assert.deepEqual(map[1 + BAND_ROWS], [0, 0, 0, 0, 0, 0, MAP_FIRST_FORMAT, 0]);
  assert.deepEqual(map[2 + BAND_ROWS], [MAP_FIRST_FORMAT + 4, 0, 0, 0, 0, 0, 0, 0]);
});

test("neighbouring partitions keep two bands, not one", () => {
  const map = rows(laneMap(disk(), [], 0, 8192, 8), 8);
  assert.notEqual(map[1][1], map[1][2]);
});

test("a lane holds only its own format, however many findings there are", () => {
  const crowd = { ...disk(), findings: Array.from({ length: 900 }, (_, index) => finding(index * 8, index % 3 === 0 ? "NTFS" : "exFAT")) };
  const map = rows(laneMap(crowd, ["exFAT", "NTFS"], 0, 8192, 8), 8);
  assert.ok(map[1 + BAND_ROWS].every((value) => value === MAP_FIRST_FORMAT));
  assert.ok(map[2 + BAND_ROWS].every((value) => value === MAP_FIRST_FORMAT + 4));
});

test("findings are summarised by format and count, in a fixed order", () => {
  const found = [finding(0, "NTFS"), finding(1, "exFAT"), finding(2, "NTFS"), finding(3, "Strange"), finding(4, "FAT32")];
  assert.deepEqual(evidenceSummary(found), [{ format: "exFAT", count: 1 }, { format: "FAT32", count: 1 }, { format: "NTFS", count: 2 }, { format: "Strange", count: 1 }]);
  assert.deepEqual(evidenceSummary([]), []);
});

test("a boot sector with geometry is decodable, a directory fragment needs related metadata, a locked volume is only recognised", () => {
  assert.equal(evidenceStatus(finding(0, "NTFS")), "decodable");
  assert.equal(evidenceStatus(finding(0, "exFAT", { volumeStart: 0, volumeLength: null })), "decodable");
  assert.equal(evidenceStatus(finding(0, "exFAT", { volumeStart: null, volumeLength: null, structure: "checksummed directory entry set" })), "needs-metadata");
  assert.equal(evidenceStatus(finding(0, "NTFS", { supported: false, volumeLength: null })), "recognised");
});

test("a click picks a finding nearby before the partition holding it", () => {
  const source = disk();
  assert.deepEqual(itemAt(source, 100, 0), { kind: "evidence", item: source.findings[0] });
  assert.deepEqual(itemAt(source, 700, 300), { kind: "evidence", item: source.findings[0] });
  assert.deepEqual(itemAt(source, 1500, 100), { kind: "partition", index: 0 });
  assert.deepEqual(itemAt(source, 3000, 0), { kind: "partition", index: 1 });
  assert.equal(itemAt(source, 5000, 10), null);
});

test("the finding under the click wins over a nearer edge of another", () => {
  const source = disk();
  assert.equal(itemAt(source, 6600, 200).item, source.findings[1]);
  assert.equal(itemAt(source, 7200, 200).item, source.findings[2]);
  assert.equal(itemAt(source, 7300, 100), null);
});

test("a drive with known damage gets a health lane under the scan row, worst state on top, and none otherwise", () => {
  const damaged = { ...disk(), health: [
    { start: 0, end: 4096, state: "slow", latencyMs: 900, note: "" },
    { start: 2048, end: 3072, state: "reset-culprit", latencyMs: null, note: "" },
    { start: 5120, end: 6144, state: "unreadable", latencyMs: null, note: "" },
    { start: 7168, end: 8192, state: "good", latencyMs: 3, note: "" },
  ] };
  const lanes = ["exFAT", "NTFS"];
  assert.equal(mapRowCount(damaged, lanes.length), 2 + BAND_ROWS + lanes.length);
  const map = rows(laneMap(damaged, lanes, 0, 8192, 8), 8);
  assert.equal(map.length, 2 + BAND_ROWS + lanes.length);
  assert.deepEqual(map[1], [MAP_HEALTH_SLOW, MAP_HEALTH_SLOW, MAP_HEALTH_CULPRIT, MAP_HEALTH_SLOW, 0, MAP_HEALTH_UNREADABLE, 0, 0]);
  // The rows below are the ones there were: bands, then a lane for each format.
  for (let row = 2; row < 2 + BAND_ROWS; row++) assert.deepEqual(map[row], [MAP_BAND, MAP_BAND, MAP_BAND_ALTERNATE, MAP_BAND_ALTERNATE, 0, 0, 0, 0]);
  // Good ranges alone draw nothing, and a source without a health map keeps the map it had.
  const fine = { ...disk(), health: [{ start: 0, end: 4096, state: "good", latencyMs: 2, note: "" }] };
  assert.equal(mapRowCount(fine, 2), 1 + BAND_ROWS + 2);
  assert.equal(laneMap(fine, lanes, 0, 8192, 8).length, (1 + BAND_ROWS + 2) * 8);
  assert.equal(mapRowCount(disk(), 2), 1 + BAND_ROWS + 2);
});
