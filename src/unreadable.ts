/** Bytes of a file that could not be read from the disk. The engine returns zeros for them, which look
 * like real bytes, so a preview that shows the file's bytes marks them. */

export interface ByteRange {
  start: number;
  end: number;
}

/** Longest list of ranges written out; the rest are counted. */
const MAX_RANGES_SHOWN = 6;
const QUESTION_MARK = 0x3f;

/** For each byte of `length` bytes that start at `offset` in the file, whether it is in a range. */
export function unreadableMask(length: number, ranges: ByteRange[], offset = 0): boolean[] {
  const mask = new Array<boolean>(length).fill(false);
  for (const range of ranges) {
    const from = Math.max(range.start - offset, 0);
    const to = Math.min(range.end - offset, length);
    for (let index = from; index < to; index += 1) mask[index] = true;
  }
  return mask;
}

/** A copy of the bytes with each unreadable one replaced by `?`, so text shows where it is missing. */
export function markUnreadable(data: Uint8Array, ranges: ByteRange[], offset = 0): Uint8Array {
  if (ranges.length === 0) return data;
  const marked = data.slice();
  unreadableMask(data.length, ranges, offset).forEach((missing, index) => {
    if (missing) marked[index] = QUESTION_MARK;
  });
  return marked;
}

/** The ranges as a person reads them: byte numbers from the start of the file, both ends included. */
export function describeRanges(ranges: ByteRange[]): string {
  const shown = ranges.slice(0, MAX_RANGES_SHOWN).map((range) => (range.end - range.start === 1 ? `${range.start}` : `${range.start}–${range.end - 1}`));
  const more = ranges.length - shown.length;
  return more > 0 ? `${shown.join(", ")}, +${more}` : shown.join(", ");
}
