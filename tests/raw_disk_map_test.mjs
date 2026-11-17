import assert from "node:assert/strict";
import { test } from "node:test";
import { sourceMap } from "../src/rawDisk.ts";

function disk() {
  return {
    length: 4096,
    partitions: [{ start: 512, length: 1024 }],
    findings: [
      { offset: 3072, length: 512, format: "exFAT" },
      { offset: 0, length: 512, format: "NTFS" },
    ],
    errors: [], scanStart: null, nextOffset: null,
  };
}

test("unexamined source bytes stay unknown, not empty", () => {
  assert.deepEqual([...sourceMap(disk(), "bytes", 0, 4096, 8)], [0, 0, 0, 0, 0, 0, 0, 0]);
});

test("declared partitions do not hide bytes outside their bounds", () => {
  assert.deepEqual([...sourceMap(disk(), "partitions", 0, 4096, 8)], [0, 3, 3, 0, 0, 0, 0, 0]);
});

test("older exFAT and NTFS structures share the same byte graph", () => {
  assert.deepEqual([...sourceMap(disk(), "evidence", 0, 4096, 8)], [8, 0, 0, 0, 0, 0, 4, 0]);
});

test("examined bytes are limited to the actual scan range", () => {
  const source = { ...disk(), scanStart: 1024, nextOffset: 3072 };
  assert.deepEqual([...sourceMap(source, "bytes", 0, 4096, 8)], [0, 0, 1, 1, 1, 1, 0, 0]);
});

test("unreadable regions remain distinct from examined or unknown bytes", () => {
  const source = { ...disk(), scanStart: 1024, nextOffset: 3072, errors: [{ offset: 1536, length: 512 }] };
  assert.deepEqual([...sourceMap(source, "bytes", 0, 4096, 8)], [0, 0, 1, 2, 1, 1, 0, 0]);
});

test("magnified views use absolute source offsets", () => {
  assert.deepEqual([...sourceMap(disk(), "evidence", 3072, 4096, 2)], [4, 0]);
});

test("starting at an unaligned byte does not mark it examined before a read", () => {
  const source = { ...disk(), scanStart: 1537, nextOffset: 1537 };
  assert.deepEqual([...sourceMap(source, "bytes", 0, 4096, 8)], [0, 0, 0, 0, 0, 0, 0, 0]);
});

test("possible volume extents are not painted as observed filesystem structures", () => {
  const source = disk();
  source.findings = [{ offset: 3072, length: 512, format: "exFAT", volumeStart: 1024, volumeLength: 3072 }];
  assert.deepEqual([...sourceMap(source, "evidence", 0, 4096, 8)], [0, 0, 0, 0, 0, 0, 4, 0]);
});
