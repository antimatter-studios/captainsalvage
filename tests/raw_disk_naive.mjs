import { SOURCE_FORMATS } from "../src/rawDisk.ts";

// The straightforward rasteriser the sweep in src/rawDisk.ts replaced: it scans every interval
// for every bucket and keeps the one painted last. It is the reference the sweep must equal and
// the baseline the benchmark measures against.
export function naiveSourceMap(disk, layer, start, end, buckets) {
  const data = new Uint8Array(buckets);
  const intervals = [];
  const paint = (offset, length, value) => {
    if (length <= 0 || offset >= end || offset + length <= start) return;
    const left = Math.max(0, Math.floor((offset - start) / (end - start) * buckets));
    const right = Math.min(buckets, Math.ceil((offset + length - start) / (end - start) * buckets));
    if (right > left) intervals.push({ left, right, value });
  };
  if (disk.scanStart !== null && disk.nextOffset !== null) paint(disk.scanStart, disk.nextOffset - disk.scanStart, 1);
  if (layer === "partitions") disk.partitions.forEach((item) => paint(item.start, item.length, 3));
  if (layer === "evidence") disk.findings.forEach((item) => paint(item.offset, item.length, 4 + Math.max(0, SOURCE_FORMATS.indexOf(item.format))));
  disk.errors.forEach((item) => paint(item.offset, item.length, 2));
  for (let bucket = 0; bucket < buckets; bucket++) {
    for (const item of intervals) if (item.left <= bucket && bucket < item.right) data[bucket] = item.value;
  }
  return data;
}

// A raw disk with `count` boot-sector findings spread across `length` bytes, a scan that has
// reached `scanned` of it, and a few declared partitions and unreadable regions.
export function crowdedDisk(count, length = 2 ** 40, scanned = 0.6) {
  const formats = SOURCE_FORMATS;
  let seed = 12345;
  const next = () => (seed = (seed * 1103515245 + 12345) % 2147483648) / 2147483648;
  const findings = Array.from({ length: count }, (_, index) => ({
    offset: Math.floor(next() * (length - 512) / 512) * 512, length: 512,
    format: formats[index % formats.length], structure: "boot sector", confidence: "high",
    volumeStart: null, volumeLength: null, bootOffset: null, supported: true,
  }));
  return {
    length,
    partitions: [0, 1, 2, 3].map((index) => ({ start: index * length / 4, length: length / 4 - 4096, label: "", table: "GPT" })),
    findings,
    errors: Array.from({ length: 20 }, (_, index) => ({ offset: index * 4096 * 1024, length: 65536 })),
    scanStart: 0, nextOffset: Math.floor(length * scanned),
  };
}
