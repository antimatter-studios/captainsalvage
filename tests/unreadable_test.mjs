import assert from "node:assert/strict";
import { test } from "node:test";
import { describeRanges, markUnreadable, unreadableMask } from "../src/unreadable.ts";

test("no unreadable range leaves the bytes as they are", () => {
  const data = new Uint8Array([1, 2, 3]);
  assert.equal(markUnreadable(data, []), data);
  assert.deepEqual(unreadableMask(3, []), [false, false, false]);
});

test("each unreadable byte becomes a question mark and the rest are kept", () => {
  const data = new Uint8Array([65, 66, 0, 0, 0, 70]);
  const marked = markUnreadable(data, [{ start: 2, end: 5 }]);
  assert.equal(new TextDecoder().decode(marked), "AB???F");
  assert.deepEqual([...data], [65, 66, 0, 0, 0, 70], "the bytes read are not changed");
});

test("ranges are in the file's bytes and the window may start part way into it", () => {
  assert.deepEqual(unreadableMask(4, [{ start: 512, end: 700 }], 510), [false, false, true, true]);
  assert.deepEqual(unreadableMask(4, [{ start: 0, end: 600 }], 598), [true, true, false, false]);
  assert.deepEqual(unreadableMask(2, [{ start: 10, end: 20 }]), [false, false]);
});

test("ranges are written with both ends included, and a long list is counted", () => {
  assert.equal(describeRanges([{ start: 512, end: 700 }]), "512–699");
  assert.equal(describeRanges([{ start: 5, end: 6 }, { start: 8, end: 10 }]), "5, 8–9");
  const many = Array.from({ length: 9 }, (_, index) => ({ start: index * 10, end: index * 10 + 5 }));
  assert.equal(describeRanges(many), "0–4, 10–14, 20–24, 30–34, 40–44, 50–54, +3");
});
