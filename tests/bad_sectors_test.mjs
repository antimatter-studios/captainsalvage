import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { BAD_RED, badAt, badKindOf, badRanges, foundAt, marks, volumeBad } from "../src/badSectors.ts";
import { hexSegments, textSegments } from "../src/unreadable.ts";

const GIB = 1024 ** 3;

test("reset, failed and suspect ranges are bad sectors, and good and slow ones are not", () => {
  assert.deepEqual(["reset-culprit", "unreadable", "suspect", "slow", "good", "other"].map(badKindOf), ["reset", "failed", "suspect", null, null, null]);
  const found = badRanges([
    { start: 900, end: 1000, state: "slow", foundMillis: 5 },
    { start: 500, end: 600, state: "reset-culprit", foundMillis: 1_700_000_000_000 },
    { start: 100, end: 200, state: "unreadable" },
    { start: 300, end: 300, state: "suspect" },
    { start: 400, end: 450, state: "suspect", foundMillis: 0 },
  ]);
  assert.deepEqual(found.map((range) => [range.start, range.kind]), [[100, "failed"], [400, "suspect"], [500, "reset"]]);
  assert.equal(found[0].foundMillis, null, "not known");
  assert.equal(foundAt(found[2])?.getTime(), 1_700_000_000_000);
  assert.equal(foundAt(found[0]), null);
});

test("the volume's bad clusters become ranges of clusters, end included, and a range with none is left out", () => {
  const found = volumeBad({ ranges: [
    { state: "reset-culprit", firstCluster: 9, lastCluster: 9, foundMillis: 7 },
    { state: "unreadable", firstCluster: null, lastCluster: null },
    { state: "slow", firstCluster: 20, lastCluster: 30 },
    { state: "suspect", firstCluster: 100, lastCluster: 140 },
  ] });
  assert.deepEqual(found.map((range) => [range.start, range.end, range.kind]), [[9, 10, "reset"], [100, 141, "suspect"]]);
});

test("marks are clipped to the part shown, in percent of it, and touching ones are joined", () => {
  const ranges = [
    { start: 10, end: 20, kind: "reset", foundMillis: null },
    { start: 20, end: 30, kind: "failed", foundMillis: null },
    { start: 90, end: 140, kind: "suspect", foundMillis: null },
    { start: 500, end: 600, kind: "reset", foundMillis: null },
  ];
  assert.deepEqual(marks(ranges, 0, 100), [{ left: 10, width: 20 }, { left: 90, width: 10 }]);
  assert.deepEqual(marks(ranges, 50, 150), [{ left: 40, width: 50 }]);
  assert.deepEqual(marks(ranges, 1000, 2000), []);
  assert.deepEqual(marks(ranges, 5, 5), []);
});

test("a drive with thousands of bad ranges still draws a few hundred marks", () => {
  const many = Array.from({ length: 5000 }, (_, index) => ({ start: index * 1000, end: index * 1000 + 10, kind: "failed", foundMillis: null }));
  const drawn = marks(many, 0, 5_000_000);
  assert.ok(drawn.length <= 300, `${drawn.length}`);
  assert.ok(drawn.length >= 1);
});

test("the bad spots of the real drives are marks, each a few pixels wide in CSS, and apart when they are far", () => {
  // seagate_2tb: bad spots at 134 and 138 GiB of a 2 TB drive (a block each).
  const seagate = badRanges([
    { start: 134 * GIB, end: 134 * GIB + 4096, state: "reset-culprit" },
    { start: 138 * GIB, end: 138 * GIB + 4096, state: "reset-culprit" },
  ]);
  const drawn = marks(seagate, 0, 2e12);
  assert.equal(drawn.length, 2);
  assert.ok(drawn.every((mark) => mark.width < 0.001), "a block is far under a pixel: the strip's minimum width shows it");
  // Untitled: a bad zone about 472.5 to 473.6 GB into the partition, as 590 GB.
  const untitled = badRanges([{ start: 472.5e9, end: 473.6e9, state: "suspect" }]);
  const [zone] = marks(untitled, 0, 590e9);
  assert.ok(Math.abs(zone.left - 80.08) < 0.01 && Math.abs(zone.width - 0.186) < 0.01, JSON.stringify(zone));
  // Magnified onto the zone, it fills the strip.
  const [close] = marks(untitled, 472e9, 474e9);
  assert.ok(close.left > 20 && close.left < 30 && close.width > 50, JSON.stringify(close));
});

test("the pointer finds a bad range from a little way off and says which are there", () => {
  const ranges = [{ start: 100, end: 110, kind: "reset", foundMillis: null }, { start: 200, end: 210, kind: "failed", foundMillis: null }];
  assert.equal(badAt(ranges, 105, 0).length, 1);
  assert.equal(badAt(ranges, 112, 3).length, 1);
  assert.equal(badAt(ranges, 150, 3).length, 0);
  assert.equal(badAt(ranges, 105, 200).length, 2);
});

// What the preview showed before it showed red: the text of the hex dump, which the new one must equal.
function oldHexDump(data, missing, columns) {
  const lines = [];
  for (let offset = 0; offset < data.length; offset += columns) {
    const part = Array.from(data.subarray(offset, offset + columns));
    const gone = (index) => missing[offset + index] === true;
    const hex = part.map((value, index) => (gone(index) ? "??" : value.toString(16).padStart(2, "0"))).join(" ").padEnd(columns * 3 - 1);
    const text = part.map((value, index) => (gone(index) ? "?" : value >= 32 && value < 127 ? String.fromCharCode(value) : ".")).join("");
    lines.push(`${offset.toString(16).padStart(8, "0")}  ${hex}  ${text}`);
  }
  return lines.join("\n");
}

test("the hex preview reads as it did, and marks the bytes that were not read", () => {
  const data = Uint8Array.from({ length: 40 }, (_, index) => 65 + (index % 30));
  const missing = Array.from({ length: 40 }, (_, index) => index >= 3 && index < 6 || index === 35);
  const lines = hexSegments(data, missing, 16);
  assert.equal(lines.map((line) => line.map((part) => part.text).join("")).join("\n"), oldHexDump(data, missing, 16));
  const bad = lines.flat().filter((part) => part.bad).map((part) => part.text);
  assert.deepEqual(bad, ["??", "??", "??", "???", "??", "?"]);
  assert.equal(hexSegments(data, [], 16).flat().some((part) => part.bad), false);
});

test("the text preview marks each byte that was not read with a ? that is flagged bad, and decodes the rest", () => {
  const decode = (bytes) => new TextDecoder().decode(bytes);
  const data = new TextEncoder().encode("hello wörld, and more");
  const parts = textSegments(data, [{ start: 6, end: 9 }], decode);
  assert.deepEqual(parts, [{ text: "hello ", bad: false }, { text: "???", bad: true }, { text: "rld, and more", bad: false }]);
  assert.deepEqual(textSegments(data, [], decode), [{ text: "hello wörld, and more", bad: false }]);
  assert.deepEqual(textSegments(new Uint8Array(), [], decode), []);
  // A real question mark in the text is not bad.
  assert.deepEqual(textSegments(new TextEncoder().encode("a?b"), [], decode), [{ text: "a?b", bad: false }]);
});

test("bad sectors flash red once a second, slow ones do not, and reduced motion gets solid red with a hatch", () => {
  const css = readFileSync(new URL("../src/styles.css", import.meta.url), "utf8");
  assert.match(css, new RegExp(`--bad: ${BAD_RED};`));
  assert.match(css, /\.bad-range \{[^}]*animation: bad-pulse 1s ease-in-out infinite/);
  assert.match(css, /\.bad-swatch \{[^}]*animation: bad-pulse 1s/);
  assert.match(css, /\.bad-bytes \{[^}]*animation: bad-pulse 1s/);
  assert.match(css, /@keyframes bad-pulse \{[^}]*opacity: 1;[^}]*\}[^}]*opacity: 0\.25/);
  const reduced = css.slice(css.indexOf("@media (prefers-reduced-motion: reduce)"));
  const block = reduced.slice(0, reduced.indexOf("\n}\n") + 3);
  assert.match(block, /\.bad-range, \.bad-swatch, \.bad-bytes \{ animation: none; opacity: 1; \}/);
  assert.match(block, /repeating-linear-gradient\(45deg, var\(--bad\)/);
  // Nothing about slow reads animates: they are amber on the lane, as they were.
  assert.doesNotMatch(css, /slow[^{]*\{[^}]*animation/);
});
