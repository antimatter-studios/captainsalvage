import { invoke } from "@tauri-apps/api/core";
import type { ByteRange } from "./unreadable";

export type { ByteRange };

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
  /** Free clusters in a bad sector of the source: nobody can read them, so they are not unexplained. */
  badSectorFreeClusters?: number;
  /** Clusters nothing explains on a volume whose allocation is unknown: not free space, and not counted in `unexplainedFreeClusters`. */
  unexplainedUnknownClusters?: number;
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

/** What was written to the disk since its baseline, in short. */
export interface ChangeSummary {
  detectedAtMillis: number;
  writtenClusters: number;
  fatEntries: number;
  rootEntries: string[];
  bootChanged: boolean;
  affectedFiles: number;
  affectedBytes: number;
}

export interface EntryState {
  inUse: boolean;
  isDir: boolean;
  firstCluster: number;
  length: number;
  created: string | null;
  modified: string | null;
}

export interface StructureChanges {
  baselineRecordedAtMillis: number;
  detectedAtMillis: number;
  currentHash: string;
  bootMainBytes: number;
  bootBackupBytes: number;
  fatEntries: number;
  writtenClusters: number[];
  freedClusters: number;
  rootEntries: { name: string; before: EntryState | null; after: EntryState | null }[];
  affected: { id: number; name: string; path: string | null; length: number; isDir: boolean; clustersWritten: number }[];
}

export interface Status {
  device: string;
  wholeDisk: string | null;
  osDeniesWrites: boolean;
  mounted: string | null;
  baseline: { unchanged: boolean; firstRecording: boolean; recordedAtMillis: number; baselineHash: string; currentHash: string };
  changes: ChangeSummary | null;
  structuresFromBaseline: boolean;
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
  /** The physical disk this volume is on, when `wholeDisk` is a container made from it (APFS). */
  physicalDisk?: string | null;
  name: string;
  bytes: number;
  format: string;
  external: boolean;
  mountPoint: string | null;
  uuid: string | null;
  /** What names the volume in the mount block: its UUID, or, when macOS gives none, its place on the disk. */
  mountBlockKey?: string | null;
  canOpen: boolean;
  readable: boolean;
  known: KnownVolume | null;
  fstabLine: string | null;
  protected: boolean;
  mountBlocked: boolean;
}

export interface StartupDisk {
  device: string;
  name: string;
  bytes: number;
  external: boolean;
  scheme: string | null;
  readable: boolean;
}

export interface StartupReport {
  volumes: StartupVolume[];
  /** Every whole disk, whether or not its partition table could be read. */
  disks: StartupDisk[];
  elsewhere: KnownVolume[];
  diskSleepMinutes: number | null;
  /** What the disks and mounts looked like for this report; ask again with it as `since`. */
  fingerprint?: string;
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
  /** Files copied from a guess: their FAT chain was cleared, so the driver followed their claim as one run. Absent when none. */
  filesGuessed?: number;
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
  /** Files with bytes in a bad sector of the source, and how many bytes: copied as zeros there, never refused. */
  badSectorFiles?: number;
  badSectorBytes?: number;
}

export interface VolumeChoice {
  path: string;
  device: string;
  freeBytes: number;
  refused: string | null;
}

/** The answer of `unexplained_runs`: the list, or, where the volume's free space is not its own bitmap's, the list with where it came from. */
export interface UnexplainedAnswer {
  allocationBasis: "whole-volume" | "derived-from-records";
  allocationWarning: string;
  runs: UnexplainedRun[];
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
  /** A weak candidate: a FAT entry the driver found by its content only (`unchecked`), or as the only entry of its cluster (`lone_entry`). */
  weak?: "unchecked" | "lone_entry";
  /** A FAT file whose clusters the driver guessed: its chain was cleared when it was deleted, so only its first cluster is known. */
  guessed?: boolean;
  /** The source changed near this entry since it was stored, and the area has not been scanned again. */
  stale?: boolean;
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
  /** NTFS only: the FILE record's number and sequence, and how the bytes are held. */
  recordNumber?: number;
  sequence?: number;
  storage?: "resident" | "runs" | "none" | "compressed" | "encrypted" | "attribute-list" | "incomplete";
}

export interface Page {
  rows: Row[];
  total: number;
}

/** One rebuild of the folder tree, made by placing the pieces the scan found. */
/** Whether the Finder helper is installed, and which of this disk's diskRebuilds Finder shows. */
export interface FinderState {
  available: boolean;
  shown: number[];
}

export interface DiskRebuild {
  id: number;
  name: string;
  createdAtMillis: number;
  updatedAtMillis: number;
  items: number;
  /** An agent made or renamed it, and the person has not undone that. */
  madeByAgent: boolean;
}

export interface DiskRebuildItem {
  id: number;
  /** "folder" the person made, "piece" placed as a folder, or "merge" joined into its folder. */
  kind: "folder" | "piece" | "merge";
  head: number | null;
  name: string | null;
  parentItem: number | null;
  parentHead: number | null;
  /** An agent made or changed it, and the person has not undone that. */
  madeByAgent: boolean;
}

/** One change an agent made to a Disk Rebuild, kept so the person can undo it. */
export interface AgentChange {
  id: number;
  diskRebuild: number;
  session: string;
  tool: string;
  summary: string;
  size: number;
  atMillis: number;
  undone: boolean;
  origin: "agent";
}

export interface AgentChanges {
  changes: AgentChange[];
  /** The latest change that can still be undone. */
  last: AgentChange | null;
  sessions: { id: string; client: string | null; startedAtMillis: number; lastAtMillis: number; changesUsed: number; activeChanges: number; diskRebuilds: number[] }[];
  limits: { perCall: number; perSession: number };
}

export interface UndoReport {
  undone: number;
  remaining: number;
  stopped: string | null;
  session: string | null;
  diskRebuilds: number[];
}

/** Where a piece or folder goes: an item of the diskRebuild, and optionally a found folder in it. */
export interface DiskRebuildPlace {
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
  /** An agent made or changed one of the items this folder is made of. */
  madeByAgent: boolean;
  /** Files below it with bytes in a bad sector of the source, and how many bytes. */
  badSectorFiles?: number;
  badSectorBytes?: number;
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
  /** An agent placed this file on its own, or chose its copy. */
  madeByAgent: boolean;
  /** Bytes of the chosen copy in a bad sector of the source. */
  badSectorBytes?: number;
}

export interface DiskRebuildListing {
  path: string[];
  folders: RebuiltFolder[];
  files: RebuiltFile[];
  totalFiles: number;
  clashes: number;
  clashesPicked: number;
  clashesNeedYou: number;
}

/** Where Claude thinks a piece belongs, waiting for the person to accept or dismiss it. */
export interface DiskRebuildSuggestion {
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
  /** The volume's bitmap could not be read, so `allocated` says nothing. */
  allocationUnknown?: boolean;
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
  interpretation?: import("./rawDisk").DiskRegion;
  filesystem: FilesystemDetails;
  wholeDisk: string | null;
  workspace: string;
  bootMain: Record<string, unknown>;
  bootBackup: Record<string, unknown> | null;
  bootDifferences: Record<string, { main: unknown; backup: unknown }>;
  bootCheck: { mainStored: number; mainComputed: number; backupStored: number; backupComputed: number; regionsIdentical: boolean } | null;
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
    /** Clusters whose allocation is not known. Absent when the bitmap was read. */
    unknown?: number;
    crossLinks: number;
    freeChains: { chains: number; clusters: number; longest: number };
    allocatedChains: { chains: number; clusters: number; longest: number };
    mixedChains: { chains: number; clusters: number; longest: number };
  };
  allocationBytes: { allocated: number; freeWithFat: number; freeZeroFat: number; unknown?: number };
  /** False, and only then present, when the volume's bitmap could not be read: no cluster is free or in use. */
  allocationKnown?: boolean;
  allocationNote?: string;
  /** What is wrong with the volume's own structures, in words. Absent when nothing is. */
  structureDamage?: string[];
  /** Which adapter reads a FAT volume's entries: the generic one (the default), or the old one. Only present for FAT. */
  fatAdapter?: "generic" | "legacy";
  structureHash: string;
  classNames: string[];
  groupNames: string[];
  ownershipNames: string[];
}

export type FilesystemDetails =
  | { kind: "exfat" }
  | {
      kind: "fat";
      variant: "FAT12" | "FAT16" | "FAT32";
      bytesPerSector: number;
      sectorsPerCluster: number;
      reservedSectors: number;
      fatCount: number;
      rootEntryCount: number;
      sectorsPerFat: number;
      totalSectors: number;
      firstDataSector: number;
      rootCluster: number;
    }
  | {
      kind: "ntfs";
      bytesPerSector: number;
      sectorsPerCluster: number;
      clusterSize: number;
      mftCluster: number;
      fileRecordSize: number;
      indexBlockSize: number;
      totalSectors: number;
      serial: string;
      version: string | null;
      label: string | null;
    }
  | {
      /** A filesystem read through its driver's layers: what the driver says of it. */
      kind: "driver";
      format: string;
      name: string;
      allocationUnit: number;
      allocationUnits: number;
      declaredBytes: number;
      label: string | null;
      serial: string | null;
    };

/** What the app does for the open filesystem: supported, limited with the limit stated, or missing. */
export interface Capability {
  feature: string;
  support: "supported" | "limited" | "missing";
  note: string;
}

/** What an NTFS entry's FILE record says. */
export interface NtfsRecord {
  recordOffset: number;
  recordNumber: number | null;
  sequence: number;
  inUse: boolean;
  isDirectory: boolean;
  baseReference: number;
  hasAttributeList: boolean;
  issues: string[];
  names: { name: string; namespace: string; parentRecord: number; parentSequence: number; modified: string | null }[];
  streams: { name: string; resident: boolean; length: number; initializedLength: number; compressed: boolean; encrypted: boolean; sparse: boolean; runs: { vcn: number; lcn: number | null; count: number }[] }[];
}

export interface ResolvePreview {
  changes: { from: string; to: string; files: number; bytes: number; sampleIds: number[] }[];
  filesChanged: number;
  bytesChanged: number;
  filesUnchanged: number;
}

export interface Parameters {
  chainFallbackContiguous: boolean;
  conflictToleranceClusters: number;
  ignoreEntryConflicts: boolean;
  ignoreReuseOrder: boolean;
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
  /** The source changed near this file since it was stored, and the area has not been scanned again. */
  stale?: boolean;
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

/** The stretches of a file, within a range, whose clusters could not be read. The bytes there come back
 * as zeros from `readEntryRange`; they are not the file's. */
export function entryUnreadable(id: number, start: number, length: number): Promise<ByteRange[]> {
  return invoke<ByteRange[]>("entry_unreadable", { id, start, length });
}

export async function readEntryRange(id: number, start: number, length: number): Promise<Uint8Array> {
  return new Uint8Array(await invoke<ArrayBuffer>("read_entry_range", { id, start, length }));
}
