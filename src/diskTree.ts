import type { StartupDisk, StartupVolume } from "./api";

/** A physical disk and what is on it: the parent row of the start screen's tree and its children. */
export interface DiskGroup {
  device: string;
  /** Null when macOS named volumes on a disk it did not list as a whole. */
  disk: StartupDisk | null;
  volumes: StartupVolume[];
}

/** The physical disk a volume belongs to: its own disk, or the one an APFS container is made from. */
export function parentOf(volume: StartupVolume): string {
  return volume.physicalDisk ?? volume.wholeDisk;
}

/** A short name for a device: disk4s1 for /dev/rdisk4s1, or the file name for an image. */
export function shortDevice(device: string): string {
  return device.startsWith("/dev/") ? device.replace(/^\/dev\/r?/, "") : device.split("/").pop() || device;
}

/** Every volume sits under the physical disk it is on, and a disk with none is still listed. */
export function diskGroups(disks: StartupDisk[], volumes: StartupVolume[]): DiskGroup[] {
  const groups: DiskGroup[] = disks.map((disk) => ({ device: disk.device, disk, volumes: volumes.filter((volume) => parentOf(volume) === disk.device) }));
  for (const volume of volumes) {
    if (groups.some((group) => group.device === parentOf(volume))) continue;
    groups.push({ device: parentOf(volume), disk: null, volumes: volumes.filter((other) => parentOf(other) === parentOf(volume)) });
  }
  return groups;
}
