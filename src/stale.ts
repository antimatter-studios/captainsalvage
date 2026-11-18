/**
 * Areas of a scan's stored results that may be out of date.
 *
 * A scan reopened on a probable match (some markers changed, for example because a mount wrote
 * metadata) marks the areas around the changed markers until they are scanned again. The engine
 * decides which rows are inside one and sets `stale` on them; this is what the window does with
 * the regions themselves.
 */

export interface StaleRegion {
  start: number;
  end: number;
  markedAtMillis: number;
  /** The offsets of the changed markers it was made from. */
  markers: number[];
  /** How far a rescan has read it. */
  doneTo: number;
  /** When a rescan finished it. Null while it is stale. */
  clearedAtMillis: number | null;
}

export interface StaleSummary {
  regions: StaleRegion[];
  /** How many are still stale. */
  stale: number;
  staleBytes: number;
}

/** What `rescan_changed_regions` answers. */
export interface RescanResult {
  scan: import("./openedSource").ScanRecord;
  rescan: { regionsRescanned: number; regionsRemaining: number; bytesRescanned?: number; clustersRescanned?: number; resolved?: boolean; markersRerecorded?: boolean; stopped: string | null };
  markersRerecorded: boolean;
  stale: StaleSummary;
  raw?: import("./rawDisk").ScanSnapshot;
}

export function isStale(region: StaleRegion): boolean {
  return region.clearedAtMillis === null;
}

/** The regions still stale. */
export function staleRegions(summary: StaleSummary | null | undefined): StaleRegion[] {
  return summary ? summary.regions.filter(isStale) : [];
}

/** Whether the bytes `offset..offset + length` share a byte with a region that is still stale. */
export function overlapsStale(summary: StaleSummary | null | undefined, offset: number, length: number): boolean {
  const end = offset + Math.max(length, 1);
  return staleRegions(summary).some((region) => offset < region.end && end > region.start);
}

/** How much of the stale regions a rescan has read, 0 to 1. Nothing stale reads as 1. */
export function rescanFraction(summary: StaleSummary | null | undefined): number {
  const stale = staleRegions(summary);
  const total = stale.reduce((sum, region) => sum + (region.end - region.start), 0);
  if (total === 0) return 1;
  const done = stale.reduce((sum, region) => sum + Math.max(0, Math.min(region.doneTo, region.end) - region.start), 0);
  return Math.min(1, done / total);
}

/** What the window shows after a rescan: it finished, or it stopped and left regions stale. */
export function rescanOutcome(result: Pick<RescanResult, "rescan" | "stale">): "done" | "partial" | "nothing" {
  if (result.rescan.regionsRemaining > 0 || result.stale.stale > 0) return "partial";
  return result.rescan.regionsRescanned > 0 ? "done" : "nothing";
}
