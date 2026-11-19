/** Bad sectors, as the window draws them: ranges of a drive that the health map says it must not read,
 * because reading them drops the drive (it reset during a read there, or a read there failed, or they
 * lie inside a band a probe jumped over). The drawing logic is here, free of the window, so it is tested
 * without one. The colours and the flashing are in styles.css: a slow pulse, and for people who ask for
 * less motion a solid red with a diagonal hatch instead. */

/** The red of bad sectors, in the map's canvas (the flashing marks and the swatches take it from styles.css). */
export const BAD_RED = "#e5202b";

/** How a bad range was found. `reset`: the drive reset during a read here. `failed`: a read here
 * failed. `suspect`: inside a band the probe jumped over, so it was not read. */
export type BadKind = "reset" | "failed" | "suspect";

/** A bad range in the units of the map it is drawn on (bytes of a source, or clusters of a volume). */
export interface BadRange {
  start: number;
  /** One past the last unit. */
  end: number;
  kind: BadKind;
  /** When it was found, in milliseconds since 1970, if known. */
  foundMillis: number | null;
}

/** What the health map says about a range, from `health_map` or `bad_sectors`. */
export interface HealthLike {
  start: number;
  end: number;
  state: string;
  foundMillis?: number | null;
}

/** A range the health map calls this is bad sectors. Slow ranges are not: they read, slowly. */
export function badKindOf(state: string): BadKind | null {
  switch (state) {
    case "reset-culprit": return "reset";
    case "unreadable": return "failed";
    case "suspect": return "suspect";
    default: return null;
  }
}

/** The ranges of a health map that are bad sectors, in the map's own bytes. */
export function badRanges(ranges: HealthLike[]): BadRange[] {
  const found: BadRange[] = [];
  for (const range of ranges) {
    const kind = badKindOf(range.state);
    if (kind && range.end > range.start) found.push({ start: range.start, end: range.end, kind, foundMillis: range.foundMillis || null });
  }
  return found.sort((left, right) => left.start - right.start);
}

/** The answer of `bad_sectors`: the bad ranges of the open volume, in its clusters. A range before the
 * cluster heap has no clusters and is left out. */
export interface VolumeBad {
  ranges: { state: string; firstCluster: number | null; lastCluster: number | null; foundMillis?: number | null }[];
}

export function volumeBad(answer: VolumeBad): BadRange[] {
  const found: BadRange[] = [];
  for (const range of answer.ranges) {
    const kind = badKindOf(range.state);
    if (kind && range.firstCluster !== null && range.lastCluster !== null) {
      found.push({ start: range.firstCluster, end: range.lastCluster + 1, kind, foundMillis: range.foundMillis || null });
    }
  }
  return found.sort((left, right) => left.start - right.start);
}

/** A red mark on a strip: where it starts and how wide it is, as percentages of the strip. */
export interface Mark {
  left: number;
  width: number;
}

/** Marks tighter than this share of the strip's width are joined, so a drive with thousands of bad
 * ranges does not become thousands of elements. */
const JOIN_PERCENT = 0.15;
const MAX_MARKS = 300;

/** The marks to draw for the part `start..end` of a map: each bad range inside it, clipped to it, with
 * the ones that touch joined. The strip gives each mark a few pixels at least, so a small zone is seen. */
export function marks(ranges: BadRange[], start: number, end: number): Mark[] {
  const span = end - start;
  if (!(span > 0)) return [];
  const clipped = ranges
    .filter((range) => range.end > start && range.start < end)
    .map((range) => ({ from: (Math.max(range.start, start) - start) / span * 100, to: (Math.min(range.end, end) - start) / span * 100 }))
    .sort((left, right) => left.from - right.from);
  let gap = JOIN_PERCENT;
  for (;;) {
    const joined: { from: number; to: number }[] = [];
    for (const item of clipped) {
      const last = joined[joined.length - 1];
      if (last && item.from <= last.to + gap) last.to = Math.max(last.to, item.to);
      else joined.push({ ...item });
    }
    if (joined.length <= MAX_MARKS) return joined.map((item) => ({ left: item.from, width: item.to - item.from }));
    gap *= 2;
  }
}

/** The bad ranges at a place of the map, within `tolerance` units of it: what a tooltip tells. */
export function badAt(ranges: BadRange[], place: number, tolerance: number): BadRange[] {
  return ranges.filter((range) => range.start <= place + tolerance && range.end > place - tolerance);
}

/** The line a tooltip gives for when a range was found, or null when it is not known. */
export function foundAt(range: BadRange): Date | null {
  return range.foundMillis && range.foundMillis > 0 ? new Date(range.foundMillis) : null;
}
