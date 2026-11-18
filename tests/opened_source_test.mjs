import assert from "node:assert/strict";
import { test } from "node:test";
import { diskGroups, shortDevice } from "../src/diskTree.ts";
import { canOpenFrom, needsAcknowledgement, scanFraction, shownMatches } from "../src/openedSource.ts";

const disk = (device) => ({ device, name: device, bytes: 1, external: true, scheme: null, readable: true });
const volume = (device, wholeDisk, physicalDisk) => ({ device, wholeDisk, physicalDisk });

test("a partition is nested under its disk", () => {
  const groups = diskGroups([disk("/dev/rdisk7"), disk("/dev/rdisk9")], [volume("/dev/rdisk7s1", "/dev/rdisk7"), volume("/dev/rdisk7s2", "/dev/rdisk7")]);
  assert.deepEqual(groups.map((group) => [group.device, group.volumes.map((item) => item.device)]), [
    ["/dev/rdisk7", ["/dev/rdisk7s1", "/dev/rdisk7s2"]],
    ["/dev/rdisk9", []],
  ]);
});

test("a disk with no readable partition table is still a parent row", () => {
  const [group] = diskGroups([disk("/dev/rdisk5")], []);
  assert.equal(group.device, "/dev/rdisk5");
  assert.deepEqual(group.volumes, []);
});

test("an APFS container's volume goes under the physical disk it comes from", () => {
  const groups = diskGroups([disk("/dev/rdisk7")], [volume("/dev/rdisk8s1", "/dev/rdisk8", "/dev/rdisk7")]);
  assert.equal(groups.length, 1);
  assert.deepEqual(groups[0].volumes.map((item) => item.device), ["/dev/rdisk8s1"]);
});

test("a volume on a disk macOS did not list still gets a parent row", () => {
  const groups = diskGroups([], [volume("/dev/rdisk3s1", "/dev/rdisk3")]);
  assert.equal(groups[0].disk, null);
  assert.equal(groups[0].device, "/dev/rdisk3");
});

test("devices and images get short names", () => {
  assert.equal(shortDevice("/dev/rdisk4s1"), "disk4s1");
  assert.equal(shortDevice("/Users/me/images/old.img"), "old.img");
});

test("weak matches stay hidden until all are shown", () => {
  const matches = [{ verification: "exact" }, { verification: "probable" }, { verification: "weak" }];
  assert.deepEqual(shownMatches(matches, false).map((match) => match.verification), ["exact", "probable"]);
  assert.equal(shownMatches(matches, true).length, 3);
});

test("only an exact match opens without the person saying so, and a weak one never does", () => {
  assert.equal(needsAcknowledgement("exact"), false);
  assert.equal(needsAcknowledgement("probable"), true);
  assert.equal(canOpenFrom("exact", false), true);
  assert.equal(canOpenFrom("probable", false), false);
  assert.equal(canOpenFrom("probable", true), true);
  assert.equal(canOpenFrom("weak", true), false);
  assert.equal(canOpenFrom("different", true), false);
});

test("how much of a scan's range has been read", () => {
  const scan = { complete: false, coveredEnd: null, rangeStart: 0, rangeEnd: 100 };
  assert.equal(scanFraction(scan), 0);
  assert.equal(scanFraction({ ...scan, coveredEnd: 25 }), 0.25);
  assert.equal(scanFraction({ ...scan, complete: true }), 1);
});
