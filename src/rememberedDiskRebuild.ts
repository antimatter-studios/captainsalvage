// Which Disk Rebuild was chosen last, kept in the window's storage.
//
// The key used to carry another name. A choice saved under it moves to the new key the first time
// it is read, so nobody picks again after the upgrade. Nothing is ever written under the old key.

export const DISK_REBUILD_STORAGE_KEY = "salvage-disk-rebuild";
const OLD_STORAGE_KEY = "salvage-canvas";

type Storage = Pick<globalThis.Storage, "getItem" | "setItem" | "removeItem">;

export function rememberedDiskRebuild(storage: Storage = localStorage): number | null {
  try {
    const old = storage.getItem(OLD_STORAGE_KEY);
    if (old !== null) {
      if (storage.getItem(DISK_REBUILD_STORAGE_KEY) === null) storage.setItem(DISK_REBUILD_STORAGE_KEY, old);
      storage.removeItem(OLD_STORAGE_KEY);
    }
    const saved = Number(storage.getItem(DISK_REBUILD_STORAGE_KEY));
    return Number.isFinite(saved) && saved > 0 ? saved : null;
  } catch {
    return null;
  }
}
