import { useMemo, useState } from "react";
import type { StartupDisk, StartupVolume } from "../api";
import { bytes } from "../format";
import { t } from "../i18n";

/** Each whole disk with the partitions found on it. A disk with none is still a choice. */
function groups(disks: StartupDisk[], volumes: StartupVolume[]): { disk: StartupDisk; partitions: StartupVolume[] }[] {
  return disks.map((disk) => ({ disk, partitions: volumes.filter((volume) => volume.wholeDisk === disk.device) }));
}

function diskName(disk: StartupDisk): string {
  return disk.device.startsWith("/dev/r") ? `${disk.name}, ${disk.device.replace("/dev/r", "")}` : disk.name;
}

/**
 * Chooses what to inspect as raw bytes, once for every disk on the screen: the whole disk, which
 * works whether or not its partition table can be read, or one partition of it.
 */
export function RawChooser({ disks, volumes, onRaw }: { disks: StartupDisk[]; volumes: StartupVolume[]; onRaw: (path: string) => void }) {
  const choices = useMemo(() => groups(disks, volumes), [disks, volumes]);
  const [picked, setPicked] = useState<string | null>(null);
  const devices = choices.flatMap((choice) => [choice.disk.device, ...choice.partitions.map((partition) => partition.device)]);
  const device = picked !== null && devices.includes(picked) ? picked : (volumes[0]?.device ?? devices[0]);
  const partition = volumes.find((volume) => volume.device === device);
  const disk = disks.find((candidate) => candidate.device === device);
  return (
    <section className="start-raw">
      <h3>{t("Inspect the raw bytes")}</h3>
      <p>{t("No filesystem is needed. Choose a whole disk, even when its partition table is damaged or missing, or one partition on it. The map shows exactly the bytes you choose.")}</p>
      <div className="start-raw-choose">
        <select value={device} onChange={(event) => setPicked(event.target.value)} aria-label={t("Choose a partition, or the whole disk it belongs to")}>
          {choices.map(({ disk: whole, partitions }) => (
            <optgroup key={whole.device} label={diskName(whole)}>
              <option value={whole.device}>{t("Whole disk, {bytes}", { bytes: bytes(whole.bytes) })}</option>
              {partitions.map((item) => <option key={item.device} value={item.device}>{item.name || t("Unnamed volume")}, {item.format}, {bytes(item.bytes)}</option>)}
            </optgroup>
          ))}
        </select>
        <button onClick={() => onRaw(device)}>{t("Inspect raw bytes")}</button>
      </div>
      {partition && <p className="start-raw-scope">{t("The map covers only this partition, {bytes}.", { bytes: bytes(partition.bytes) })}</p>}
      {disk && <p className="start-raw-scope">{t("The map covers all {bytes} of the disk, whether or not its partition table can be read.", { bytes: bytes(disk.bytes) })}</p>}
    </section>
  );
}
