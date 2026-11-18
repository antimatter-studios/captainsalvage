import assert from "node:assert/strict";
import { test } from "node:test";
import { DISK_REBUILD_STORAGE_KEY, rememberedDiskRebuild } from "../src/rememberedDiskRebuild.ts";

const store = (initial) => {
  const items = new Map(Object.entries(initial));
  return {
    items,
    getItem: (key) => (items.has(key) ? items.get(key) : null),
    setItem: (key, value) => items.set(key, String(value)),
    removeItem: (key) => items.delete(key),
  };
};

test("a choice saved under the new key is read", () => {
  assert.equal(rememberedDiskRebuild(store({ [DISK_REBUILD_STORAGE_KEY]: "7" })), 7);
});

test("a choice saved under the old key moves to the new one, once", () => {
  const storage = store({ "salvage-canvas": "12" });
  assert.equal(rememberedDiskRebuild(storage), 12);
  assert.equal(storage.items.get(DISK_REBUILD_STORAGE_KEY), "12");
  assert.equal(storage.items.has("salvage-canvas"), false);
  assert.equal(rememberedDiskRebuild(storage), 12);
});

test("the new key wins over an old one, which is dropped", () => {
  const storage = store({ "salvage-canvas": "12", [DISK_REBUILD_STORAGE_KEY]: "3" });
  assert.equal(rememberedDiskRebuild(storage), 3);
  assert.equal(storage.items.has("salvage-canvas"), false);
});

test("nothing saved, a bad value or storage that throws gives no choice", () => {
  assert.equal(rememberedDiskRebuild(store({})), null);
  assert.equal(rememberedDiskRebuild(store({ [DISK_REBUILD_STORAGE_KEY]: "abc" })), null);
  const broken = { getItem: () => { throw new Error("blocked"); }, setItem() {}, removeItem() {} };
  assert.equal(rememberedDiskRebuild(broken), null);
});
