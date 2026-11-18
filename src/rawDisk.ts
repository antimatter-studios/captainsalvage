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
  /** Set when the scan was resumed on a source that had changed. */
  changedSource?: ChangedSource | null;
  findings: DiskEvidence[];
  errors: DiscoveryBatch["errors"];
  nextOffset: number | null;
  scanEnd: number | null;
  scanStart: number | null;
}

/** How well a saved scan's markers match the open source. `exact` is all of them, `probable` is 60% or more. */
export type MatchState = "exact" | "probable" | "weak" | "different" | "incompatible";

export interface MarkerComparison {
  state: MatchState;
  total: number;
  matched: number;
  changed: number;
  unreadable: number;
  /** Matched markers out of all markers, rounded down. */
  confidencePercent: number;
  changedOffsets: number[];
  unreadableOffsets: number[];
  /** What changed and where, in the engine's English. Show it with `fromEngine`. */
  summary: string;
}

/** The record that a scan resumed on a source that was only a probable match. */
export interface ChangedSource {
  resumes: number;
  resumedAtMillis: number;
  confidencePercent: number;
  matched: number;
  changed: number;
  unreadable: number;
  total: number;
  changedOffsets: number[];
  summary: string;
}

export interface SavedScanSummary {
  id: string;
  scanStart: number;
  scanEnd: number;
  nextOffset: number;
  updatedAtMillis: number;
  verification: MatchState;
  comparison: MarkerComparison;
  changedSource: ChangedSource | null;
}

export interface SavedScans {
  fingerprint: { sampledSha256: string | null; samples: { offset: number }[]; markersWanted: number };
  scans: SavedScanSummary[];
  /** Saved scans left out because they match under 60%. Asking for all of them lists these. */
  hidden: number;
}

export interface ScanSnapshot {
  scanId: string;
  scanStart: number;
  scanEnd: number;
  nextOffset: number;
  findings: DiskEvidence[];
  errors: DiscoveryBatch["errors"];
  changedSource: ChangedSource | null;
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

/** What the map paints. Gaps are 0; the rest are what the legend names. */
export const MAP_NOT_SCANNED = 0;
export const MAP_EXAMINED = 1;
export const MAP_UNREADABLE = 2;
export const MAP_BAND = 3;
/** The format lanes start here, one value each in SOURCE_FORMATS order. */
export const MAP_FIRST_FORMAT = 4;
/** Every other partition band, so two bands side by side stay two. */
export const MAP_BAND_ALTERNATE = 12;
/** How many canvas rows the partition bands take. The lanes and the scan row take one each. */
export const BAND_ROWS = 3;

export interface Paint {
  offset: number;
  length: number;
  value: number;
}

/** Paints each item over the buckets it covers, the later item winning where two overlap, in one sweep. */
export function paintBuckets(items: Paint[], start: number, end: number, buckets: number): Uint8Array {
  const data = new Uint8Array(buckets);
  const intervals: { start: number; end: number; value: number; order: number }[] = [];
  items.forEach(({ offset, length, value }, order) => {
    if (length <= 0 || offset >= end || offset + length <= start) return;
    const left = Math.max(0, Math.floor((offset - start) / (end - start) * buckets));
    const right = Math.min(buckets, Math.ceil((offset + length - start) / (end - start) * buckets));
    if (right > left) intervals.push({ start: left, end: right, value, order });
  });

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

function formatValue(format: string): number {
  return MAP_FIRST_FORMAT + Math.max(0, SOURCE_FORMATS.indexOf(format));
}

function scanPaints(disk: RawDiskState): Paint[] {
  const paints: Paint[] = [];
  if (disk.scanStart !== null && disk.nextOffset !== null) paints.push({ offset: disk.scanStart, length: disk.nextOffset - disk.scanStart, value: MAP_EXAMINED });
  return paints;
}

export function sourceMap(disk: RawDiskState, layer: SourceLayer, start: number, end: number, buckets: number): Uint8Array {
  const paints = scanPaints(disk);
  if (layer === "partitions") disk.partitions.forEach((item) => paints.push({ offset: item.start, length: item.length, value: MAP_BAND }));
  if (layer === "evidence") disk.findings.forEach((item) => paints.push({ offset: item.offset, length: item.length, value: formatValue(item.format) }));
  disk.errors.forEach((item) => paints.push({ offset: item.offset, length: item.length, value: MAP_UNREADABLE }));
  return paintBuckets(paints, start, end, buckets);
}

/** The formats that have findings, in a fixed order, each with how many. */
export function evidenceSummary(findings: DiskEvidence[]): { format: string; count: number }[] {
  const counts = new Map<string, number>();
  findings.forEach((item) => counts.set(item.format, (counts.get(item.format) ?? 0) + 1));
  const known = SOURCE_FORMATS.filter((format) => counts.has(format));
  const other = [...counts.keys()].filter((format) => !SOURCE_FORMATS.includes(format)).sort();
  return [...known, ...other].map((format) => ({ format, count: counts.get(format)! }));
}

/**
 * The map as rows: what has been scanned on top, then the declared partitions as broad bands, then a
 * thin lane for each format that has findings. Each row is `buckets` long, one after another.
 */
export function laneMap(disk: RawDiskState, lanes: string[], start: number, end: number, buckets: number): Uint8Array {
  const scan = scanPaints(disk);
  disk.errors.forEach((item) => scan.push({ offset: item.offset, length: item.length, value: MAP_UNREADABLE }));
  const bands = disk.partitions.map((item, index): Paint => ({ offset: item.start, length: item.length, value: index % 2 === 0 ? MAP_BAND : MAP_BAND_ALTERNATE }));
  const rows = [paintBuckets(scan, start, end, buckets)];
  const band = paintBuckets(bands, start, end, buckets);
  for (let row = 0; row < BAND_ROWS; row++) rows.push(band);
  for (const lane of lanes) {
    rows.push(paintBuckets(disk.findings.filter((item) => item.format === lane).map((item) => ({ offset: item.offset, length: item.length, value: formatValue(item.format) })), start, end, buckets));
  }
  const data = new Uint8Array(rows.length * buckets);
  rows.forEach((row, index) => data.set(row, index * buckets));
  return data;
}

export type EvidenceStatus = "recognised" | "needs-metadata" | "decodable";

/**
 * How far a finding can be taken. A boot sector with plausible geometry can be tried as a filesystem.
 * A fragment of a directory says nothing of where its volume starts or ends, so it needs related
 * metadata first. Anything else the probe knew by its signature is recognised, no more.
 */
export function evidenceStatus(item: Pick<DiskEvidence, "supported" | "volumeLength" | "volumeStart">): EvidenceStatus {
  if (!item.supported) return "recognised";
  if (item.volumeLength === null && item.volumeStart === null) return "needs-metadata";
  return "decodable";
}

/** What sits at a byte of the map: a finding within `tolerance` bytes of it, else the partition holding it. */
export function itemAt(disk: RawDiskState, offset: number, tolerance: number): { kind: "evidence"; item: DiskEvidence } | { kind: "partition"; index: number } | null {
  let nearest: DiskEvidence | null = null;
  let nearestDistance = Infinity;
  for (const item of disk.findings) {
    const distance = offset < item.offset ? item.offset - offset : offset >= item.offset + item.length ? offset - (item.offset + item.length - 1) : 0;
    if (distance <= tolerance && distance < nearestDistance) {
      nearest = item;
      nearestDistance = distance;
    }
  }
  if (nearest) return { kind: "evidence", item: nearest };
  const index = disk.partitions.findIndex((item) => offset >= item.start && offset < item.start + item.length);
  return index >= 0 ? { kind: "partition", index } : null;
}

export interface RangeFinding {
  format: string;
  structure: string;
  status: "volume" | "inferred" | "structures" | "island";
  offset: number;
  length: number;
  decodedBytes: number;
  units: number;
  confidence: number;
  level: "high" | "medium" | "low";
  signature: boolean;
  volumeStart: number | null;
  volumeLength: number | null;
  bootOffset: number | null;
  supported: boolean;
  detail: string;
  names: string[];
  missing: string[];
  offer: { formats: string[]; looksFor: string[]; searchStart: number; searchLength: number } | null;
  insideVolume: number | null;
}

export interface RangeDriver {
  driver: string;
  offsetsTried: number;
  decodedAt: number;
  bestOffset: number | null;
  bestDecodedBytes: number;
  bestConfidence: number;
}

export interface RangeReport {
  start: number;
  length: number;
  step: number;
  signatures: boolean;
  offsetsTried: number;
  drivers: RangeDriver[];
  findings: RangeFinding[];
  unreadable: { offset: number; length: number; error: string }[];
  notes: string[];
}

/** Where a finding says the thing starts and how far it reaches: the volume when it names one. */
export function findingExtent(finding: RangeFinding): { start: number; length: number } {
  return finding.volumeStart !== null && finding.volumeLength !== null
    ? { start: finding.volumeStart, length: finding.volumeLength }
    : { start: finding.offset, length: finding.length };
}

/** The same finding found twice, as a second search can: one entry per structure and place. */
export function mergeFindings(current: RangeFinding[], added: RangeFinding[]): RangeFinding[] {
  const seen = new Set(current.map((item) => `${item.format}:${item.structure}:${item.offset}`));
  return [...current, ...added.filter((item) => !seen.has(`${item.format}:${item.structure}:${item.offset}`))]
    .sort((left, right) => right.confidence - left.confidence || left.offset - right.offset);
}

export interface Hypothesis {
  id: string;
  label: string;
  format: string;
  start: number;
  length: number;
  bootOffset: number | null;
  virtualStart: number;
  confidence: number;
  origin: "analysis" | "manual";
  enabled: boolean;
  layer: number;
}

export interface HypothesisLayout {
  segments: { id: string; virtualStart: number; length: number }[];
  virtualLength: number;
  gaps: { virtualStart: number; length: number }[];
  competing: { start: number; length: number; stack: string[] }[];
  partitionTable: { id: string; label: string; format: string; virtualStart: number; length: number; shadowed: boolean }[];
}

export interface HypothesisList {
  hypotheses: Hypothesis[];
  layout: HypothesisLayout;
}

/** Whether another enabled hypothesis is read where this one is, and which is on top there. */
export function competition(layout: HypothesisLayout, id: string): { rivals: number; onTop: boolean } {
  const stacks = layout.competing.filter((piece) => piece.stack.includes(id));
  const rivals = new Set(stacks.flatMap((piece) => piece.stack.filter((other) => other !== id))).size;
  return { rivals, onTop: stacks.every((piece) => piece.stack[0] === id) };
}

/** The end byte a typed start and end make, or null when they do not make an extent. */
export function extentFromEnds(start: string, end: string, sourceLength: number): { start: number; length: number } | null {
  if (start.trim() === "" || end.trim() === "") return null;
  const first = Number(start);
  const last = Number(end);
  if (!Number.isSafeInteger(first) || !Number.isSafeInteger(last) || first < 0 || last <= first || last > sourceLength) return null;
  return { start: first, length: last - first };
}

export function hypothesisFromFinding(finding: RangeFinding, label: string): object {
  const extent = findingExtent(finding);
  return {
    format: finding.format,
    start: extent.start,
    length: extent.length,
    confidence: finding.confidence,
    bootOffset: finding.bootOffset,
    origin: "analysis",
    label,
  };
}
