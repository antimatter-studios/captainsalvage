// Times the raw-disk graph on a scan with many findings: the straightforward rasteriser it
// replaced against the sweep in src/rawDisk.ts, and what the evidence list renders at once.
//   node scripts/bench-raw-disk-map.mjs [findings]
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { applyScanBatch, sourceMap } from "../src/rawDisk.ts";
import { crowdedDisk, naiveSourceMap } from "../tests/raw_disk_naive.mjs";

const total = Number(process.argv[2] ?? 2000);
const BUCKETS = 4096;
const BATCH = 10;

function median(values) {
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.floor(sorted.length / 2)];
}
function time(run, repeats = 15) {
  run();
  const samples = [];
  for (let index = 0; index < repeats; index++) {
    const started = performance.now();
    run();
    samples.push(performance.now() - started);
  }
  return median(samples);
}

const disk = crowdedDisk(total);
const rasterise = (map) => time(() => map(disk, "evidence", 0, disk.length, BUCKETS));

// A whole scan: every committed batch re-runs the merge and repaints both strips.
function scan(map) {
  let state = { ...crowdedDisk(0), findings: [], nextOffset: 0, scanStart: 0 };
  const batches = Array.from({ length: Math.ceil(total / BATCH) }, (_, index) => ({
    nextOffset: Math.floor(disk.length * (index + 1) / Math.ceil(total / BATCH)),
    findings: disk.findings.slice(index * BATCH, (index + 1) * BATCH), errors: [],
  }));
  const started = performance.now();
  let slowest = 0;
  for (const batch of batches) {
    const step = performance.now();
    state = applyScanBatch(state, batch);
    map(state, "evidence", 0, state.length, BUCKETS);
    map(state, "evidence", 0, state.length / 16, BUCKETS);
    slowest = Math.max(slowest, performance.now() - step);
  }
  return { whole: performance.now() - started, slowest, batches: batches.length };
}

function rows(count) {
  const items = disk.findings.slice(0, count).map((item, index) => createElement("button", { key: index, className: "row" },
    createElement("span", { className: "row-icon" }), createElement("span", { className: "row-main" },
      createElement("span", { className: "row-name" }, `${item.format} · ${item.structure}`),
      createElement("span", { className: "row-path" }, `${item.offset} · ${item.confidence}`))));
  return time(() => renderToStaticMarkup(createElement("div", null, items)), 7);
}

const before = scan(naiveSourceMap);
const after = scan(sourceMap);
console.log(`${total} findings, ${BUCKETS} buckets, ${BATCH} findings per scan batch`);
console.log(`one rasterisation   before ${rasterise(naiveSourceMap).toFixed(2)} ms   after ${rasterise(sourceMap).toFixed(2)} ms`);
console.log(`whole scan (${after.batches} batches)  before ${before.whole.toFixed(0)} ms (slowest batch ${before.slowest.toFixed(1)})   after ${after.whole.toFixed(0)} ms (slowest batch ${after.slowest.toFixed(1)})`);
console.log(`evidence list markup   all ${total} rows ${rows(total).toFixed(1)} ms   first 100 rows ${rows(100).toFixed(1)} ms`);
