import assert from "node:assert/strict";
import { test } from "node:test";
import { isStale, overlapsStale, rescanFraction, rescanOutcome, staleRegions } from "../src/stale.ts";

const region = (start, end, extra = {}) => ({ start, end, markedAtMillis: 1, markers: [start], doneTo: start, clearedAtMillis: null, ...extra });
const summary = (regions) => ({ regions, stale: regions.filter(isStale).length, staleBytes: 0 });

test("only regions that are not cleared are stale", () => {
  const found = summary([region(0, 100), region(200, 300, { clearedAtMillis: 5 })]);
  assert.deepEqual(staleRegions(found).map((item) => item.start), [0]);
  assert.deepEqual(staleRegions(null), []);
  assert.deepEqual(staleRegions(undefined), []);
});

test("a byte range is stale when it shares a byte with a stale region", () => {
  const found = summary([region(100, 200), region(500, 600, { clearedAtMillis: 9 })]);
  assert.equal(overlapsStale(found, 199, 1), true);
  assert.equal(overlapsStale(found, 50, 51), true);
  assert.equal(overlapsStale(found, 50, 50), false, "ending where the region starts is outside it");
  assert.equal(overlapsStale(found, 200, 10), false);
  assert.equal(overlapsStale(found, 150, 0), true, "a point inside counts");
  assert.equal(overlapsStale(found, 550, 10), false, "a cleared region marks nothing");
  assert.equal(overlapsStale(null, 0, 10), false);
});

test("how far a rescan has read counts only the stale regions", () => {
  assert.equal(rescanFraction(summary([])), 1, "nothing stale reads as done");
  const half = summary([region(0, 100, { doneTo: 50 })]);
  assert.equal(rescanFraction(half), 0.5);
  const mixed = summary([region(0, 100, { doneTo: 100 }), region(200, 300, { doneTo: 200 }), region(400, 500, { clearedAtMillis: 3, doneTo: 500 })]);
  assert.equal(rescanFraction(mixed), 0.5);
  assert.equal(rescanFraction(summary([region(0, 100, { doneTo: 900 })])), 1, "never above one");
});

test("a rescan is done, partial or had nothing to do", () => {
  const none = summary([]);
  assert.equal(rescanOutcome({ rescan: { regionsRescanned: 2, regionsRemaining: 0, stopped: null }, stale: none }), "done");
  assert.equal(rescanOutcome({ rescan: { regionsRescanned: 0, regionsRemaining: 0, stopped: null }, stale: none }), "nothing");
  assert.equal(rescanOutcome({ rescan: { regionsRescanned: 1, regionsRemaining: 1, stopped: "The rescan was stopped." }, stale: summary([region(0, 10)]) }), "partial");
  assert.equal(rescanOutcome({ rescan: { regionsRescanned: 1, regionsRemaining: 0, stopped: null }, stale: summary([region(0, 10)]) }), "partial", "a region left stale is never reported as done");
});
