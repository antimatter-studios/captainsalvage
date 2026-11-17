import { useState } from "react";
import type { StartupDisk, StartupVolume } from "../api";
import { bytes } from "../format";
import { t } from "../i18n";

/**
 * Chooses what to inspect as raw bytes on one physical disk: the whole disk, which works whether
 * or not its partition table can be read, or one partition of it. The whole disk comes first.
 * Without a whole-disk entry, only the partitions are offered.
 */
export function RawChooser({ disk, volumes, onRaw }: { disk: StartupDisk | null; volumes: StartupVolume[]; onRaw: (path: string) => void }) {
  const devices = [...(disk ? [disk.device] : []), ...volumes.map((volume) => volume.device)];
  const [picked, setPicked] = useState<string | null>(null);
  const device = picked !== null && devices.includes(picked) ? picked : devices[0];
  const partition = volumes.find((volume) => volume.device === device);
  const whole = disk && disk.device === device ? disk : null;
  return (
    <div className="start-raw">
      <div className="start-raw-choose">
        <select value={device} onChange={(event) => setPicked(event.target.value)} aria-label={t("Choose a partition, or the whole disk it belongs to")}>
          {disk && <option value={disk.device}>{t("Whole disk, {bytes}", { bytes: bytes(disk.bytes) })}</option>}
          {volumes.map((item) => <option key={item.device} value={item.device}>{item.name || t("Unnamed volume")}, {item.format}, {bytes(item.bytes)}</option>)}
        </select>
        <button onClick={() => onRaw(device)}>{t("Inspect raw bytes")}</button>
      </div>
      {partition && <p className="start-raw-scope">{t("The map covers only this partition, {bytes}.", { bytes: bytes(partition.bytes) })}</p>}
      {whole && <p className="start-raw-scope">{t("The map covers all {bytes} of the disk, whether or not its partition table can be read.", { bytes: bytes(whole.bytes) })}</p>}
    </div>
  );
}
