import type { MarkerComparison, MatchState } from "./rawDisk";

/** What the picker offers, in this order. FAT is whichever width the boot sector says. */
export type ChoiceId = "exFAT" | "FAT" | "NTFS" | "none";

export interface InterpretationChoice {
  id: ChoiceId;
  /** What it opens as: FAT12, FAT16 or FAT32 for FAT. */
  format: string;
  /** There is something to open: boot evidence or a saved interpretation. Raw bytes always is. */
  available: boolean;
  /** The detection says this one. */
  detected: boolean;
  confidence: number | null;
  /** A scan is stored for it, so choosing it reopens that scan without scanning. */
  stored: boolean;
  current: boolean;
  origin: string | null;
}

export interface ScanRecord {
  id: string;
  sourceId: string;
  kind: "raw" | "filesystem";
  label: string;
  format: string;
  start: number;
  length: number;
  bootOffset: number | null;
  origin: string;
  confidence: number;
  rangeStart: number;
  rangeEnd: number;
  coveredEnd: number | null;
  complete: boolean;
  status: "not-started" | "running" | "paused" | "complete";
  storedBytes: number;
  createdAtMillis: number;
  updatedAtMillis: number;
  openedAtMillis: number;
  migrated: boolean;
}

export interface SourceRecord {
  id: string;
  label: string;
  length: number;
  markers: number;
  lastPath: string;
  scans: number;
}

export interface SourceMatchRecord extends Omit<SourceRecord, "scans"> {
  verification: MatchState;
  comparison: MarkerComparison;
  scans: ScanRecord[];
}

export interface ByteExtent {
  start: number;
  length: number;
}

export interface LinkedExtent extends ByteExtent {
  source: SourceRecord;
  scans: ScanRecord[];
}

/** What `open_source` answers, once something is opened. */
export interface OpenedSource {
  needsChoice?: false;
  disk: { id: string; path: string; length: number; partitions: { start: number; length: number; label: string; table: string }[]; warnings: string[] };
  /** A partition is one extent holding a filesystem. A whole disk has the layer above it. */
  scope: "disk" | "partition";
  /** The extent of the disk this is narrowed to. Null for a whole source. */
  extent: ByteExtent | null;
  source: SourceRecord | null;
  parents: { start: number; length: number; parent: SourceRecord }[];
  extents: LinkedExtent[];
  interpretation: { format: string; start: number; length: number; bootOffset: number | null; origin: string; confidence: number } | null;
  detected: { format: string; confidence: number } | null;
  choices: InterpretationChoice[];
  scan: ScanRecord | null;
  created: boolean;
  scans: ScanRecord[];
  raw: import("./rawDisk").ScanSnapshot | null;
  /** Why the filesystem that was found or stored could not be applied, when the bytes opened as raw instead. */
  openError: string | null;
  /** The markers that identify this source, for saying when too few could be used. */
  fingerprint: { samples: { offset: number }[]; markersWanted: number };
  overview: import("./api").Overview | null;
}

/** What `open_source` answers when only a probable match was found, which it never applies. */
export interface NeedsChoice {
  needsChoice: true;
  disk: OpenedSource["disk"];
  extent: ByteExtent | null;
  matches: SourceMatchRecord[];
  hidden: number;
}

export type OpenResult = OpenedSource | NeedsChoice;

export interface ScanListing {
  matches: SourceMatchRecord[];
  hidden: number;
}

/** Matches worth showing: all of them, or the ones that are at least probable. */
export function shownMatches<T extends { verification: MatchState }>(matches: T[], showAll: boolean): T[] {
  return showAll ? matches : matches.filter((match) => match.verification === "exact" || match.verification === "probable");
}

/** A match that is not exact is applied only after the person says they read what changed. */
export function needsAcknowledgement(verification: MatchState): boolean {
  return verification !== "exact";
}

/** Whether a stored scan can be opened from this match: exact, or probable and acknowledged. */
export function canOpenFrom(verification: MatchState, acknowledged: boolean): boolean {
  return verification === "exact" || (verification === "probable" && acknowledged);
}

/** The scan to open without being asked: the first listed, which is the last used. */
export function latestScan(scans: ScanRecord[]): ScanRecord | null {
  return scans[0] ?? null;
}

/** How much of a scan's range has been read, 0 to 1. */
export function scanFraction(scan: ScanRecord): number {
  if (scan.complete) return 1;
  if (scan.coveredEnd === null || scan.rangeEnd <= scan.rangeStart) return 0;
  return Math.max(0, Math.min(1, (scan.coveredEnd - scan.rangeStart) / (scan.rangeEnd - scan.rangeStart)));
}
