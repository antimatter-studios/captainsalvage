import { useEffect, useState } from "react";
import { call, type Status, type StructureChanges } from "../api";
import { bytes, count } from "../format";
import { locale, t } from "../i18n";

/** How often the disk's structures are read again and compared with the baseline. */
const RECHECK_EVERY_MILLIS = 10 * 60_000;

/**
 * Says loudly, on every screen, that something wrote to the disk after its baseline, and what
 * that cost: which clusters now hold other data and which found files used them. It can be
 * folded to a red strip but not dismissed, and a new change unfolds it again.
 */
export function WrittenWarning({ status, clusterSize, onChanged }: { status: Status | null; clusterSize: number; onChanged: () => void }) {
  const [folded, setFolded] = useState<string | null>(null);
  const [details, setDetails] = useState<StructureChanges | null>(null);
  const [showFiles, setShowFiles] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);

  // Reading the structures again is what notices a write made while the app is open.
  useEffect(() => {
    if (!status) return;
    const timer = window.setInterval(() => {
      call("verify_unchanged").then(onChanged).catch(() => undefined);
    }, RECHECK_EVERY_MILLIS);
    return () => window.clearInterval(timer);
  }, [status !== null, onChanged]);

  const changed = status !== null && !status.baseline.unchanged;
  const key = status?.baseline.currentHash ?? "";
  useEffect(() => {
    if (!changed) return;
    call<StructureChanges | null>("structure_changes").then(setDetails).catch(() => undefined);
  }, [changed, key]);

  if (!status || !changed) return null;
  const date = new Date(status.baseline.recordedAtMillis).toLocaleDateString(locale());
  const summary = status.changes;

  if (folded === key) {
    return (
      <button className="written-strip" onClick={() => setFolded(null)}>
        {t("Something wrote to this disk after {date}. Show what changed", { date })}
      </button>
    );
  }

  const run = async (label: string, tool: string) => {
    setBusy(label);
    try {
      await call(tool);
    } finally {
      setBusy(null);
      onChanged();
    }
  };

  return (
    <div className="written-warning" role="alert">
      <strong>{t("Something wrote to this disk after {date}.", { date })}</strong>
      {summary ? (
        <>
          <p>
            {t("{clusters} clusters ({size}) that were free on {date} now hold other data.", {
              clusters: count(summary.writtenClusters),
              size: bytes(summary.writtenClusters * clusterSize),
              date,
            })}{" "}
            {summary.affectedFiles > 0
              ? t("{files} found files ({size}) used those clusters, so that part of them is gone. Rebuild the map to mark them.", {
                  files: count(summary.affectedFiles),
                  size: bytes(summary.affectedBytes),
                })
              : t("No file found on the disk used those clusters.")}
          </p>
          {summary.rootEntries.length > 0 && (
            <p>{t("Changed at the top of the disk: {names}.", { names: summary.rootEntries.join(", ") })}</p>
          )}
          {summary.bootChanged && <p>{t("The boot region changed too.")}</p>}
        </>
      ) : (
        <p>{t("No copy of the disk's structures from {date} is kept, so what changed cannot be shown.", { date })}</p>
      )}
      <p className="written-warning-advice">
        {t("macOS writes like this when it mounts the disk. Plug the disk in only while Captain Salvage is open, so it can keep the disk unmounted.")}
      </p>
      {showFiles && details && details.affected.length > 0 && (
        <table className="written-files">
          <thead>
            <tr><th>{t("File")}</th><th>{t("Size")}</th><th>{t("Clusters written")}</th></tr>
          </thead>
          <tbody>
            {details.affected.map((entry) => (
              <tr key={entry.id}>
                <td title={entry.path ?? undefined}><bdi>{entry.path ? `${entry.path}/${entry.name}` : entry.name}</bdi></td>
                <td>{bytes(entry.length)}</td>
                <td>{count(entry.clustersWritten)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <div className="written-warning-actions">
        {details && details.affected.length > 0 && (
          <button onClick={() => setShowFiles((shown) => !shown)}>{showFiles ? t("Hide the files") : t("Show the files")}</button>
        )}
        {summary && summary.affectedFiles > 0 && (
          <button className="primary" disabled={busy !== null} onClick={() => void run("rebuild", "resolve")}>
            {busy === "rebuild" ? t("Rebuilding") : t("Rebuild the map now")}
          </button>
        )}
        <button disabled={busy !== null} onClick={() => void run("check", "verify_unchanged")}>
          {busy === "check" ? t("Checking") : t("Check again")}
        </button>
        <button className="link" onClick={() => setFolded(key)}>{t("Fold away")}</button>
      </div>
    </div>
  );
}
