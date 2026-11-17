export interface DiskRegion {
  start: number;
  length: number;
  format: string;
  label: string;
  bootOffset: number;
}

export interface DiskEvidence {
  offset: number;
  length: number;
  format: string;
  structure: string;
  confidence: string;
  volumeStart: number | null;
  volumeLength: number | null;
  bootOffset: number | null;
  supported: boolean;
}

export interface RawDiskOverview {
  id: string;
  path: string;
  length: number;
  partitions: { start: number; length: number; label: string; table: string }[];
  warnings: string[];
}

export interface DiscoveryBatch {
  findings: DiskEvidence[];
  errors: { offset: number; length: number; error: string }[];
  nextOffset: number;
  evidenceLimitReached: boolean;
}

export interface RawDiskState extends RawDiskOverview {
  scanId: string | null;
  findings: DiskEvidence[];
  errors: DiscoveryBatch["errors"];
  nextOffset: number | null;
  scanEnd: number | null;
  scanStart: number | null;
}

export interface SavedScanSummary {
  id: string;
  scanStart: number;
  scanEnd: number;
  nextOffset: number;
  updatedAtMillis: number;
  verification: "sampled-match" | "mismatch" | "unverifiable" | "incompatible";
}

export interface SavedScans {
  fingerprint: { sampledSha256: string | null };
  scans: SavedScanSummary[];
}

export interface ScanSnapshot {
  scanId: string;
  scanStart: number;
  scanEnd: number;
  nextOffset: number;
  findings: DiskEvidence[];
  errors: DiscoveryBatch["errors"];
}

export function scanEstimate(start: number, cursor: number, end: number, elapsedMillis: number): { bytesPerSecond: number; secondsLeft: number } | null {
  if (![start, cursor, end, elapsedMillis].every(Number.isFinite) || elapsedMillis <= 0 || cursor <= start || cursor > end) return null;
  const bytesPerSecond = (cursor - start) * 1000 / elapsedMillis;
  const secondsLeft = (end - cursor) / bytesPerSecond;
  return bytesPerSecond > 0 && Number.isFinite(bytesPerSecond) && Number.isFinite(secondsLeft) ? { bytesPerSecond, secondsLeft } : null;
}

export function applyScanBatch(disk: RawDiskState, batch: DiscoveryBatch): RawDiskState {
  const start = disk.nextOffset ?? disk.scanStart ?? 0;
  return {
    ...disk,
    nextOffset: batch.nextOffset,
    findings: [...disk.findings.filter((item) => item.offset < start || item.offset >= batch.nextOffset), ...batch.findings],
    errors: [...disk.errors, ...batch.errors],
  };
}

export const SOURCE_FORMATS = ["exFAT", "FAT12", "FAT16", "FAT32", "NTFS"];
export type SourceLayer = "bytes" | "partitions" | "evidence";

export function sourceMap(disk: RawDiskState, layer: SourceLayer, start: number, end: number, buckets: number): Uint8Array {
  const data = new Uint8Array(buckets);
  const intervals: { start: number; end: number; value: number; order: number }[] = [];
  let order = 0;
  const paint = (offset: number, length: number, value: number) => {
    const currentOrder = order++;
    if (length <= 0 || offset >= end || offset + length <= start) return;
    const left = Math.max(0, Math.floor((offset - start) / (end - start) * buckets));
    const right = Math.min(buckets, Math.ceil((offset + length - start) / (end - start) * buckets));
    if (right > left) intervals.push({ start: left, end: right, value, order: currentOrder });
  };
  if (disk.scanStart !== null && disk.nextOffset !== null) paint(disk.scanStart, disk.nextOffset - disk.scanStart, 1);
  if (layer === "partitions") disk.partitions.forEach((item) => paint(item.start, item.length, 3));
  if (layer === "evidence") disk.findings.forEach((item) => paint(item.offset, item.length, 4 + Math.max(0, SOURCE_FORMATS.indexOf(item.format))));
  disk.errors.forEach((item) => paint(item.offset, item.length, 2));

  intervals.sort((left, right) => left.start - right.start);
  const active: { end: number; value: number; order: number }[] = [];
  let nextInterval = 0;
  for (let bucket = 0; bucket < buckets; bucket++) {
    while (nextInterval < intervals.length && intervals[nextInterval].start === bucket) {
      const item = intervals[nextInterval++];
      active.push(item);
      let index = active.length - 1;
      while (index > 0) {
        const parent = Math.floor((index - 1) / 2);
        if (active[parent].order >= item.order) break;
        active[index] = active[parent];
        index = parent;
      }
      active[index] = item;
    }
    while (active.length > 0 && active[0].end <= bucket) {
      const last = active.pop()!;
      if (active.length === 0) break;
      let index = 0;
      while (index * 2 + 1 < active.length) {
        let child = index * 2 + 1;
        if (child + 1 < active.length && active[child + 1].order > active[child].order) child++;
        if (active[child].order <= last.order) break;
        active[index] = active[child];
        index = child;
      }
      active[index] = last;
    }
    data[bucket] = active[0]?.value ?? 0;
  }
  return data;
}
