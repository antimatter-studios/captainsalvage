import assert from "node:assert/strict";
import { test } from "node:test";
import { sourceMap } from "../src/rawDisk.ts";
import { crowdedDisk, naiveSourceMap } from "./raw_disk_naive.mjs";

for (const layer of ["bytes", "partitions", "evidence"]) {
  test(`the sweep paints the same ${layer} map as the straightforward rasteriser`, () => {
    const disk = crowdedDisk(1500);
    for (const [start, end, buckets] of [[0, disk.length, 4096], [0, disk.length / 8, 1000], [disk.length / 3, disk.length / 3 + 2 ** 22, 640]]) {
      assert.deepEqual(sourceMap(disk, layer, start, end, buckets), naiveSourceMap(disk, layer, start, end, buckets));
    }
  });
}

test("a full evidence list of 2000 findings rasterises within a frame at the widest strip", () => {
  const disk = crowdedDisk(2000);
  sourceMap(disk, "evidence", 0, disk.length, 4096);
  const started = performance.now();
  for (let pass = 0; pass < 20; pass++) sourceMap(disk, "evidence", 0, disk.length, 4096);
  const each = (performance.now() - started) / 20;
  assert.ok(each < 16, `${each.toFixed(2)} ms per rasterisation`);
});
