import assert from "node:assert/strict";
import { test } from "node:test";
import { allowanceUsed, undoBar } from "../src/agentChanges.ts";

const change = (id, summary) => ({ id, diskRebuild: 1, session: "s", tool: "disk_rebuild_folder", summary, size: 1, atMillis: 0, undone: false, origin: "agent" });
const changes = (last, active) => ({
  changes: last ? [last] : [],
  last,
  sessions: active === 0 ? [] : [{ id: "s", client: null, startedAtMillis: 0, lastAtMillis: 0, changesUsed: active, activeChanges: active, diskRebuilds: [1] }],
  limits: { perCall: 1000, perSession: 10000 },
});

test("there is no bar until an agent has changed something that can be undone", () => {
  assert.equal(undoBar(null, null).show, false);
  assert.equal(undoBar(changes(null, 0), null).show, false);
});

test("the bar names the latest change and says whether more wait", () => {
  const one = undoBar(changes(change(3, "Made the folder X"), 1), null);
  assert.deepEqual([one.show, one.summary, one.several], [true, "Made the folder X", false]);
  assert.equal(undoBar(changes(change(3, "Made the folder X"), 4), null).several, true);
});

test("restoring to before the agent is offered only for a Disk Rebuild an agent changed", () => {
  const scan = changes(change(3, "x"), 2);
  assert.equal(undoBar(scan, null).canRestore, false);
  assert.equal(undoBar(scan, changes(null, 0)).canRestore, false);
  assert.equal(undoBar(scan, changes(change(2, "y"), 1)).canRestore, true);
});

test("the allowance used stays between none and all", () => {
  assert.equal(allowanceUsed(0, 100), 0);
  assert.equal(allowanceUsed(50, 100), 0.5);
  assert.equal(allowanceUsed(500, 100), 1);
  assert.equal(allowanceUsed(5, 0), 1);
});
