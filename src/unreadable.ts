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

/** A stretch of a preview, and whether its bytes could not be read (they come from a bad sector). */
export interface Segment {
  text: string;
  bad: boolean;
}

function joinSegments(parts: Segment[]): Segment[] {
  const joined: Segment[] = [];
  for (const part of parts) {
    if (part.text === "") continue;
    const last = joined[joined.length - 1];
    if (last && last.bad === part.bad) last.text += part.text;
    else joined.push({ ...part });
  }
  return joined;
}

/** A text preview as stretches: what was read, decoded, and a `?` for each byte that was not. */
export function textSegments(data: Uint8Array, ranges: ByteRange[], decode: (bytes: Uint8Array) => string): Segment[] {
  const mask = unreadableMask(data.length, ranges);
  const parts: Segment[] = [];
  let from = 0;
  for (let index = 1; index <= data.length; index += 1) {
    if (index === data.length || mask[index] !== mask[from]) {
      parts.push(mask[from] ? { text: "?".repeat(index - from), bad: true } : { text: decode(data.subarray(from, index)), bad: false });
      from = index;
    }
  }
  return joinSegments(parts);
}

/** A hex preview as lines of stretches, `columns` bytes to a line: the offset, each byte in hex and
 * then as a character, with `??` and `?` and `bad` for the bytes that were not read. */
export function hexSegments(data: Uint8Array, missing: boolean[], columns: number): Segment[][] {
  const lines: Segment[][] = [];
  for (let offset = 0; offset < data.length; offset += columns) {
    const part = Array.from(data.subarray(offset, offset + columns));
    const gone = (index: number) => missing[offset + index] === true;
    const parts: Segment[] = [{ text: `${offset.toString(16).padStart(8, "0")}  `, bad: false }];
    part.forEach((value, index) => {
      parts.push({ text: gone(index) ? "??" : value.toString(16).padStart(2, "0"), bad: gone(index) });
      if (index < part.length - 1) parts.push({ text: " ", bad: false });
    });
    parts.push({ text: " ".repeat((columns - part.length) * 3) + "  ", bad: false });
    part.forEach((value, index) => parts.push({ text: gone(index) ? "?" : value >= 32 && value < 127 ? String.fromCharCode(value) : ".", bad: gone(index) }));
    lines.push(joinSegments(parts));
  }
  return lines;
}
