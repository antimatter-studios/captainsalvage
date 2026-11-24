import { useEffect, useState } from "react";
import { call, type Capability, type FilesystemDetails } from "../api";
import { t } from "../i18n";
import { NtfsJournalPanel } from "./NtfsJournal";

function Fact({ label, value }: { label: string; value: string | number }) {
  return (
    <>
      <dt>{label}</dt>
      <dd className="mono">{value}</dd>
    </>
  );
}

const SUPPORT_LABEL: Record<Capability["support"], () => string> = {
  supported: () => t("Supported"),
  limited: () => t("Limited"),
  missing: () => t("Missing"),
};

const FEATURE_LABEL: Record<string, () => string> = {
  open: () => t("Open"),
  scan: () => t("Scan"),
  map: () => t("Map"),
  browse: () => t("Browse"),
  preview: () => t("Preview"),
  resume: () => t("Resume"),
  baseline: () => t("Baseline copy"),
  file_tests: () => t("File tests"),
  rebuilds: () => t("Disk Rebuilds"),
  export: () => t("Copy out"),
  mount_block: () => t("Mount block"),
  named_streams: () => t("Alternate data streams"),
  hard_links: () => t("Hard links"),
  attribute_lists: () => t("Attribute lists"),
  compression: () => t("NTFS compression"),
  encryption: () => t("Encryption"),
  journal: () => t("Journal"),
  large_volumes: () => t("Very large volumes"),
};

/** What this app does for the open filesystem, as the engine reports it to the command line and to an AI. */
function CapabilityList() {
  const [items, setItems] = useState<Capability[]>([]);
  useEffect(() => {
    let current = true;
    call<{ capabilities: Capability[] }>("capabilities")
      .then((result) => current && setItems(result.capabilities))
      .catch(() => undefined);
    return () => {
      current = false;
    };
  }, []);
  if (items.length === 0) return null;
  return (
    <>
      <h4>{t("What works for this filesystem")}</h4>
      <dl className="facts capabilities">
        {items.map((item) => (
          <div key={item.feature} className={`capability capability-${item.support}`}>
            <dt>{FEATURE_LABEL[item.feature]?.() ?? item.feature}</dt>
            <dd title={item.note}>
              <strong>{SUPPORT_LABEL[item.support]()}</strong>
            </dd>
          </div>
        ))}
      </dl>
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
      ) : details.kind === "driver" ? (
        <dl className="facts">
          <Fact label={t("Format")} value={details.name} />
          {details.label && <Fact label={t("Volume label")} value={details.label} />}
          {details.serial && <Fact label={t("Serial number")} value={details.serial} />}
          <Fact label={t("Cluster size")} value={details.allocationUnit} />
        </dl>
      ) : details.kind === "ntfs" ? (
        <dl className="facts">
          <Fact label={t("Format")} value="NTFS" />
          {details.label && <Fact label={t("Volume label")} value={details.label} />}
          {details.version && <Fact label={t("NTFS version")} value={details.version} />}
          <Fact label={t("Serial number")} value={details.serial} />
          <Fact label={t("Bytes per sector")} value={details.bytesPerSector} />
          <Fact label={t("Sectors per cluster")} value={details.sectorsPerCluster} />
          <Fact label={t("Cluster size")} value={details.clusterSize} />
          <Fact label={t("MFT starts at cluster")} value={details.mftCluster} />
          <Fact label={t("File record size")} value={details.fileRecordSize} />
          <Fact label={t("Index block size")} value={details.indexBlockSize} />
          <Fact label={t("Total sectors")} value={details.totalSectors} />
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
      {details.kind === "ntfs" && <NtfsJournalPanel />}
      <CapabilityList />
    </>
  );
}
