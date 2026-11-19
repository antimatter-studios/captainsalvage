import assert from "node:assert/strict";
import { test } from "node:test";
import { createClosedBox } from "../src/writtenClosing.ts";

test("a disk that has not changed shows no box, open or closed", () => {
  const box = createClosedBox();
  assert.equal(box.isOpen(false, ""), false);
});

test("a change opens the box, and closing it closes it for that change", () => {
  const box = createClosedBox();
  assert.equal(box.isOpen(true, "a"), true);
  box.close("a");
  assert.equal(box.isOpen(true, "a"), false);
  assert.equal(box.isOpen(true, "a"), false, "it stays closed on every later look at the same change");
});

test("a new change after closing brings the box back, once", () => {
  const box = createClosedBox();
  box.close("a");
  assert.equal(box.isOpen(true, "b"), true, "a different current hash is a new change");
  box.close("b");
  assert.equal(box.isOpen(true, "b"), false);
  assert.equal(box.isOpen(true, "b"), false);
});

test("when the disk is no longer changed, the next change opens the box even with the same hash", () => {
  const box = createClosedBox();
  box.close("a");
  box.settle(false);
  assert.equal(box.get(), null);
  assert.equal(box.isOpen(true, "a"), true);
});

test("a change that is still there does not forget the closing", () => {
  const box = createClosedBox();
  box.close("a");
  box.settle(true);
  assert.equal(box.isOpen(true, "a"), false);
});

test("listeners hear a close and a forgetting, and not a no-op", () => {
  const box = createClosedBox();
  let heard = 0;
  const stop = box.subscribe(() => (heard += 1));
  box.settle(false);
  assert.equal(heard, 0, "nothing was closed, so nothing changed");
  box.close("a");
  assert.equal(heard, 1);
  box.close("a");
  assert.equal(heard, 1, "closing the same change again changes nothing");
  box.settle(false);
  assert.equal(heard, 2);
  stop();
  box.close("b");
  assert.equal(heard, 2);
});

test("the shared box starts open for a change", async () => {
  const { closedBox } = await import("../src/writtenClosing.ts");
  assert.equal(closedBox.isOpen(true, "x"), true);
});
