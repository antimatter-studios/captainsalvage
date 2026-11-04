import { invoke } from "@tauri-apps/api/core";

export type Layer = "allocation" | "content" | "entropy" | "ownership" | "confidence" | "folders" | "lens";

export interface Lens {
  name?: string;
  allocation?: number[];
  groups?: number[];
  ownership?: number[];
  entropyMin?: number;
  entropyMax?: number;
  confidenceMin?: number;
  startCluster?: number;
  endCluster?: number;
}

export interface VerdictTotal {
  verdict: string;
  count: number;
  bytes: number;
}

export interface MapSummary {
  verdicts: VerdictTotal[];
  nodesByKind: [string, number][];
  resolvedAtMillis: number;
  namelessChains: number;
  namelessClusters: number;
  brokenNamelessChains: number;
  unexplainedFreeClusters: number;
  labelledFreeClusters: number;
}

export interface ScanProgress {
  status: string;
  nextCluster: number;
  totalClusters: number;
  clustersScanned: number;
  directoryClusters: number;
  setsFound: number;
  readErrors: number;
  startedAtMillis: number;
  updatedAtMillis: number;
  bytesPerSecond: number;
}

export interface Status {
  device: string;
  wholeDisk: string | null;
  osDeniesWrites: boolean;
  mounted: string | null;
  baseline: { unchanged: boolean; firstRecording: boolean; recordedAtMillis: number; baselineHash: string; currentHash: string };
  reads: { bytes: number; operations: number; errors: number };
  scan: ScanProgress;
  scanInThisApp: boolean;
  scanHeldElsewhere: boolean;
  scanError: string | null;
  summary: MapSummary | null;
  copy: CopyStatus;
}

export interface CopyStatus {
  state: string;
  destination: string;
  manifest: string;
  filesTotal: number;
  filesDone: number;
  filesSkipped: number;
  filesPartial: number;
  filesFailed: number;
  bytesTotal: number;
  bytesDone: number;
  current: string;
  error: string | null;
  badClusters: number;
}

export interface CopyPlan {
  files: number;
  bytes: number;
  destination: string;
  destinationDevice: string | null;
  freeBytes: number;
  fits: boolean;
  refused: string | null;
  belowFloor: number;
  skippedPartial: number;
  skippedPartialBytes: number;
  largest: { name: string; bytes: number; confidence: number }[];
  byVerdict: Record<string, number>;
}

export interface VolumeChoice {
  path: string;
  device: string;
  freeBytes: number;
  refused: string | null;
}

export interface UnexplainedRun {
  startCluster: number;
  endCluster: number;
  clusters: number;
  bytes: number;
  unexplainedClusters: number;
  mainContent: string;
  mainContentShare: number;
  meanEntropyBits: number;
}

export interface Row {
  kind: "folder" | "file" | "missing-folder";
  id: number | null;
  head: number | null;
  name: string;
  path: string | null;
  length: number;
  validLength: number;
  inUse: boolean;
  checksumOk: boolean;
  verdict: string;
  reason: string;
  firstCluster: number;
  noFatChain: boolean;
  created: string | null;
  modified: string | null;
  sourceCluster: number | null;
  sourceOffset: number | null;
  clustersNeeded: number;
  clustersFound: number;
  clustersAllocated: number;
  clustersConflict: number;
  files: number;
  bytes: number;
  intactFiles: number;
  intactBytes: number;
  nodeKind: string | null;
  hint: string | null;
  confidence: number;
  linkConfidence: number | null;
}

export interface Page {
  rows: Row[];
  total: number;
}

export interface IslandRow {
  head: number;
  tail: number;
  length: number;
  allocated: number;
  endKind: string;
  owners: number;
  headClass: number;
  headClassName: string;
  meanEntropy: number;
}

export interface EntrySet {
  offset: number;
  name: string;
  inUse: boolean;
  checksumOk: boolean;
  attributes: number;
  isDir: boolean;
  firstCluster: number;
  length: number;
  validLength: number;
  noFatChain: boolean;
  created: string | null;
  modified: string | null;
  accessed: string | null;
  slots: number;
}

export interface DecodedField {
  name: string;
  offset: number;
  length: number;
  kind: string;
  value: unknown;
}

export interface ClusterInfo {
  cluster: number;
  offset: number;
  diskOffset: number | null;
  allocated: boolean;
  fatValue: number;
  fatMeaning: string;
  predecessor: number | null;
  chainHead: number;
  island: IslandRow | null;
  contentClass: string;
  entropyBits: number;
  ownership: string;
  confidence: number;
  sets: EntrySet[];
  owners: Row[];
  notes: Overlay[];
  structures: { overlay: number; note: string; fields: DecodedField[] }[];
}

export interface Overlay {
  id: number;
  kind: string;
  target: number | null;
  payload: Record<string, unknown>;
  note: string;
  author: string;
  active: boolean;
  createdAtMillis: number;
}

export interface ActivityItem {
  id: number;
  atMillis: number;
  author: string;
  action: string;
  detail: string;
}

export interface Extent {
  first: number;
  count: number;
}

export interface Overview {
  device: string;
  wholeDisk: string | null;
  workspace: string;
  bootMain: Record<string, unknown>;
  bootBackup: Record<string, unknown>;
  bootDifferences: Record<string, { main: unknown; backup: unknown }>;
  bootCheck: { mainStored: number; mainComputed: number; backupStored: number; backupComputed: number; regionsIdentical: boolean };
  geometry: {
    clusterSize: number;
    clusterCount: number;
    firstCluster: number;
    heapOffset: number;
    fatOffset: number;
    fatBytes: number;
    volumeBytes: number;
    partitionOffsetOnDisk: number;
    rootCluster: number;
  };
  allocation: {
    allocated: number;
    freeWithFat: number;
    freeZeroFat: number;
    crossLinks: number;
    freeChains: { chains: number; clusters: number; longest: number };
    allocatedChains: { chains: number; clusters: number; longest: number };
    mixedChains: { chains: number; clusters: number; longest: number };
  };
  allocationBytes: { allocated: number; freeWithFat: number; freeZeroFat: number };
  structureHash: string;
  classNames: string[];
  groupNames: string[];
  ownershipNames: string[];
}

export interface Parameters {
  chainFallbackContiguous: boolean;
  conflictToleranceClusters: number;
  ignoreEntryConflicts: boolean;
}

export interface McpDetails {
  info: { url: string; token: string; port: number } | null;
  error: string | null;
}

export interface DiskAccess {
  devices: { path: string; exists: boolean; readable: boolean; readError: string | null; osDeniesWrites: boolean; mounted: string | null }[];
  instructions: string[];
  ready: boolean;
}

/** What the person has selected. Claude receives this with every question. */
export type Selection =
  | { kind: "none" }
  | { kind: "cluster"; cluster: number; byteStart?: number; byteEnd?: number }
  | { kind: "entry"; id: number; name: string; path: string | null; firstCluster: number; verdict: string; confidence: number }
  | { kind: "folder"; head: number; name: string; path: string | null }
  | { kind: "chain"; head: number; length: number; headClassName: string }
  | { kind: "region"; startCluster: number; endCluster: number; mainContent: string };

export function call<T = unknown>(tool: string, args: object = {}): Promise<T> {
  return invoke<T>("engine_call", { tool, args });
}

export async function mapLayer(
  layer: Layer,
  start: number,
  end: number,
  buckets: number,
  rows: number,
  floor: number,
  lens: Lens | null,
): Promise<Uint8Array> {
  const buffer = await invoke<ArrayBuffer>("map_layer", { layer, start, end, buckets, rows, floor, lens });
  return new Uint8Array(buffer);
}

export async function readRaw(offset: number, length: number): Promise<Uint8Array> {
  return new Uint8Array(await invoke<ArrayBuffer>("read_raw", { offset, length }));
}

export async function readEntryBytes(id: number): Promise<Uint8Array> {
  return new Uint8Array(await invoke<ArrayBuffer>("read_entry_bytes", { id }));
}
