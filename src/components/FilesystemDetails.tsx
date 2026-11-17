import type { FilesystemDetails } from "../api";
import { t } from "../i18n";

function Fact({ label, value }: { label: string; value: string | number }) {
  return (
    <>
      <dt>{label}</dt>
      <dd className="mono">{value}</dd>
    </>
  );
}

export function FilesystemDetailsPanel({ details }: { details: FilesystemDetails }) {
  return (
    <>
      <h4>{t("Filesystem")}</h4>
      {details.kind === "exfat" ? (
        <dl className="facts">
          <Fact label={t("Format")} value="exFAT" />
        </dl>
      ) : (
        <dl className="facts">
          <Fact label={t("Format")} value={details.variant} />
          <Fact label={t("Bytes per sector")} value={details.bytesPerSector} />
          <Fact label={t("Sectors per cluster")} value={details.sectorsPerCluster} />
          <Fact label={t("Reserved sectors")} value={details.reservedSectors} />
          <Fact label={t("FAT copies")} value={details.fatCount} />
          <Fact label={t("Root directory entries")} value={details.rootEntryCount} />
          <Fact label={t("Sectors per FAT")} value={details.sectorsPerFat} />
          <Fact label={t("Total sectors")} value={details.totalSectors} />
          <Fact label={t("First data sector")} value={details.firstDataSector} />
          {details.variant === "FAT32" && <Fact label={t("Root cluster")} value={details.rootCluster} />}
        </dl>
      )}
    </>
  );
}
