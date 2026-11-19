import assert from "node:assert/strict";
import { test } from "node:test";
import { DiskFollower } from "../src/startupFollow.ts";

function engine(reports) {
  const asked = [];
  let current = reports[0];
  const ask = async (since) => {
    asked.push(since);
    return since !== null && since === current.fingerprint ? { unchanged: true } : current;
  };
  return { asked, ask, set: (report) => (current = report) };
}

test("a look the person asks for builds a report, and the next quiet look asks with its fingerprint", async () => {
  const shown = [];
  const disks = engine([{ fingerprint: "a", volumes: 1 }]);
  const follower = new DiskFollower(disks.ask, (report) => shown.push(report));
  await follower.look();
  assert.deepEqual(disks.asked, [null]);
  assert.equal(await follower.refresh(), "unchanged");
  assert.deepEqual(disks.asked, [null, "a"]);
  assert.equal(shown.length, 1, "nothing was shown again");
});

test("a change in the disks is shown by the next quiet look and not by the one after", async () => {
  const shown = [];
  const disks = engine([{ fingerprint: "a", volumes: 1 }]);
  const follower = new DiskFollower(disks.ask, (report) => shown.push(report));
  await follower.look();
  disks.set({ fingerprint: "b", volumes: 0 });
  assert.equal(await follower.refresh(), "applied");
  assert.deepEqual(shown.map((report) => report.fingerprint), ["a", "b"]);
  assert.equal(await follower.refresh(), "unchanged");
  assert.equal(shown.length, 2);
});

test("two looks never run at once", async () => {
  let release;
  const slow = new Promise((resolve) => (release = resolve));
  let calls = 0;
  const follower = new DiskFollower(
    async () => {
      calls += 1;
      await slow;
      return { fingerprint: "a" };
    },
    () => undefined,
  );
  const first = follower.look();
  assert.equal(await follower.refresh(), "skipped");
  release();
  await first;
  assert.equal(calls, 1);
});

test("the answer to a look that a newer one overtook is dropped", async () => {
  const shown = [];
  const answers = [];
  const follower = new DiskFollower(
    (since) => new Promise((resolve) => answers.push({ since, resolve })),
    (report) => shown.push(report.fingerprint),
  );
  const old = follower.look();
  const newer = follower.look();
  answers[1].resolve({ fingerprint: "new" });
  await newer;
  answers[0].resolve({ fingerprint: "old" });
  await old;
  assert.deepEqual(shown, ["new"]);
});

test("a failed quiet look leaves the screen as it was and the next one tries again", async () => {
  const shown = [];
  let fail = false;
  const follower = new DiskFollower(
    async () => {
      if (fail) throw new Error("diskutil is busy");
      return { fingerprint: "a" };
    },
    (report) => shown.push(report),
  );
  await follower.look();
  fail = true;
  assert.equal(await follower.refresh(), "skipped");
  fail = false;
  assert.equal(await follower.refresh(), "applied");
  assert.equal(shown.length, 2);
});

test("a list with no fingerprint (a made-up one) is asked for in full every time", async () => {
  const asked = [];
  const follower = new DiskFollower(
    async (since) => {
      asked.push(since);
      return { volumes: [] };
    },
    () => undefined,
  );
  await follower.look();
  await follower.refresh();
  assert.deepEqual(asked, [null, null]);
});

test("a queued look that met a running one is made when that one is over", async () => {
  let release;
  const slow = new Promise((resolve) => (release = resolve));
  const asked = [];
  const follower = new DiskFollower(
    async (since) => {
      asked.push(since);
      if (asked.length === 1) await slow;
      return since === "b" ? { unchanged: true } : { fingerprint: "b" };
    },
    () => undefined,
  );
  const first = follower.look();
  assert.equal(await follower.refresh({ queue: true }), "skipped");
  assert.equal(await follower.refresh(), "skipped");
  release();
  await first;
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(asked, [null, "b"], "one look after the first, with its fingerprint, and the plain skipped look was not kept");
});

test("a look that is not queued is not made later", async () => {
  let release;
  const slow = new Promise((resolve) => (release = resolve));
  const asked = [];
  const follower = new DiskFollower(
    async (since) => {
      asked.push(since);
      await slow;
      return { fingerprint: "a" };
    },
    () => undefined,
  );
  const first = follower.look();
  await follower.refresh();
  release();
  await first;
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(asked, [null]);
});
