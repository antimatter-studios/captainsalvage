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
  fileTests: FileTestStatus;
  writes: DiskWrites;
  diskGone: boolean;
  diskBack: { device: string; mounted: boolean } | null;
  keepAwake: boolean;
}

export interface WriteCounter {
  operations: number;
  bytes: number;
}

export interface DiskWrites {
  atOpen: WriteCounter | null;
  now: WriteCounter | null;
  sinceOpen: WriteCounter;
}

export interface KnownVolume {
  record: { uuid: string | null; name: string; bytes: number; lastOpenedMillis: number };
  /** What was found, by kind of file, largest first. */
  kinds: { group: string; files: number; bytes: number }[];
  lostFolders: number;
  scanned: boolean;
  filesFound: number;
  bytesFound: number;
  filesPassed: number;
  filesDamaged: number;
}

export interface StartupVolume {
  device: string;
  wholeDisk: string;
  name: string;
  bytes: number;
  format: string;
  external: boolean;
  mountPoint: string | null;
  uuid: string | null;
  canOpen: boolean;
  readable: boolean;
  known: KnownVolume | null;
  fstabLine: string | null;
  protected: boolean;
  mountBlocked: boolean;
}

export interface StartupReport {
  volumes: StartupVolume[];
  elsewhere: KnownVolume[];
  diskSleepMinutes: number | null;
}

export interface ZipMember {
  index: number;
  path: string;
  isDir: boolean;
  packed: number | null;
  size: number | null;
  method: number;
  crc: number | null;
  encrypted: boolean;
  modified: string | null;
  offset: number;
}

export interface ZipListing {
  /** zip, tar, tar.gz, gz, 7z and so on. */
  format: string;
  source: "directory" | "headers";
  members: ZipMember[];
  complete: boolean;
  totalBytes: number;
  note: string | null;
}

export interface ZipMemberCheck {
  status: "intact" | "damaged" | "unchecked";
  note: string;
}

export interface MemberPreview {
  kind: "image" | "text" | "none" | "too-big";
  mime: string | null;
  text: string | null;
  dataBase64: string | null;
  bytes: number;
  truncated: boolean;
  check: ZipMemberCheck | null;
}

export interface MemberCopy {
  path: string;
  bytes: number;
  sha256: string;
  check: ZipMemberCheck;
}

export interface MountHistory {
  dates: { date: string; entries: number; bytes: number; clusters: number }[];
  dirtyFlag: boolean;
  baselineRecordedAtMillis: number;
}

export interface FileTestStatus {
  state: string;
  groups: string[];
  total: number;
  done: number;
  bytesDone: number;
  current: string;
  error: string | null;
  passed: number;
  damaged: number;
  untested: number;
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

export interface BasketItem {
  id: number;
  kind: "entry" | "folder";
  target: number;
  position: number;
  name: string;
  path: string | null;
  files: number;
  bytes: number;
  likelyGoodBytes: number;
  verdict: string;
  confidence: number;
  missing: boolean;
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
  /** For a folder that lost its name: what its own files mostly are. */
  contents?: { group: string; share: number; files: number };
  confidence: number;
  linkConfidence: number | null;
}

export interface Page {
  rows: Row[];
  total: number;
}

/** One rebuild of the folder tree, made by placing the pieces the scan found. */
/** Whether the Finder helper is installed, and which of this disk's canvases Finder shows. */
export interface FinderState {
  available: boolean;
  shown: number[];
}

export interface Canvas {
  id: number;
  name: string;
  createdAtMillis: number;
  updatedAtMillis: number;
  items: number;
}

export interface CanvasItem {
  id: number;
  /** "folder" the person made, "piece" placed as a folder, or "merge" joined into its folder. */
  kind: "folder" | "piece" | "merge";
  head: number | null;
  name: string | null;
  parentItem: number | null;
  parentHead: number | null;
}

/** Where a piece or folder goes: an item of the canvas, and optionally a found folder in it. */
export interface CanvasPlace {
  item?: number;
  head?: number;
}

export interface RebuiltFolder {
  pathName: string;
  name: string | null;
  lostHead: number | null;
  origin: "made" | "found" | "placed";
  merged: boolean;
  items: number[];
  heads: number[];
  files: number;
  bytes: number;
  intactFiles: number;
  intactBytes: number;
}

export interface RebuiltFile {
  name: string;
  chosen: Row;
  byPerson: boolean;
  /** For a clash, what picked the copy: person, quality, date, or layer (the later drop). */
  decidedBy: "person" | "quality" | "date" | "layer" | null;
  /** The copies differ but measure the same, so the person should look. */
  needsYou: boolean;
  copies: Row[];
}

export interface CanvasListing {
  path: string[];
  folders: RebuiltFolder[];
  files: RebuiltFile[];
  totalFiles: number;
  clashes: number;
  clashesPicked: number;
  clashesNeedYou: number;
}

/** Where Claude thinks a piece belongs, waiting for the person to accept or dismiss it. */
export interface CanvasSuggestion {
  id: number;
  head: number;
  placeItem: number | null;
  placeHead: number | null;
  merge: boolean;
  note: string;
  author: string;
  createdAtMillis: number;
}

export interface UnusedPieces {
  rows: Row[];
  total: number;
  totalFiles: number;
  totalBytes: number;
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

export type BrowseGroup = "all" | "photos" | "videos" | "music" | "documents" | "archives" | "other";
export type BrowseSort = "best" | "largest" | "newest";

export interface TypeTotal {
  group: BrowseGroup;
  label: string;
  files: number;
  bytes: number;
  likelyGoodFiles: number;
  likelyGoodBytes: number;
}

export interface BrowseRow {
  id: number;
  name: string;
  length: number;
  verdict: string;
  confidence: number;
  firstCluster: number;
  modified: string | null;
  inUse: boolean;
  copies?: number;
}

export interface BrowsePage {
  rows: BrowseRow[];
  total: number;
  hiddenCopies: number;
}

/** Read live from the disk as the window needs it. Nothing is copied. */
export function entryUrl(id: number): string {
  return `captainsalvage://localhost/entry/${id}`;
}

export function thumbUrl(id: number, attempt = 0): string {
  return `captainsalvage://localhost/thumb/${id}${attempt > 0 ? `?try=${attempt}` : ""}`;
}

export interface MediaInfo {
  available: boolean;
  encoder: string | null;
  baseUrl: string;
  token: string;
}

/** Null if the media port did not start. `available` says whether ffmpeg is installed. */
export function mediaInfo(): Promise<MediaInfo | null> {
  return invoke<MediaInfo | null>("media_info");
}

/** Converted by ffmpeg as it plays, a few seconds at a time. Nothing is saved. */
export function convertedUrl(media: MediaInfo, id: number): string {
  return `${media.baseUrl}/hls/${id}/index.m3u8?t=${media.token}`;
}

export async function readEntryRange(id: number, start: number, length: number): Promise<Uint8Array> {
  return new Uint8Array(await invoke<ArrayBuffer>("read_entry_range", { id, start, length }));
}
