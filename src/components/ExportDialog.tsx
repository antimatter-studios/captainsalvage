import { useEffect, useState } from "react";
import { call, type CopyPlan, type CopyStatus, type VolumeChoice } from "../api";
import { bytes, count } from "../format";
import { fromEngine, t } from "../i18n";
import { CopyReport } from "./CopyReport";
import type { ExportItem } from "./ContextMenu";
import { Icon } from "./icons";

/**
 * Exports one file or folder: copies it directly to a place the person chooses, never over an
 * existing file, checking each copy as it is written. It shows the plan first, then the live
 * progress, and opens the copy report afterwards.
 */
export function ExportDialog({ item, copy, onClose }: { item: ExportItem; copy: CopyStatus | null; onClose: () => void }) {
  const [volumes, setVolumes] = useState<VolumeChoice[] | null>(null);
  const [destination, setDestination] = useState<string | null>(null);
  const [includePartial, setIncludePartial] = useState(true);
  const [plan, setPlan] = useState<CopyPlan | null>(null);
  const [checking, setChecking] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [reportOpen, setReportOpen] = useState(false);
  const [started, setStarted] = useState(false);
  const [salvage, setSalvage] = useState<string | null>(null);
  const running = copy?.state === "running";

  useEffect(() => {
    Promise.all([call<VolumeChoice[]>("list_volumes"), call<{ destination: string | null }>("get_salvage_drive")])
      .then(([found, saved]) => {
        setVolumes(found);
        setSalvage(saved.destination);
        const usable = found.filter((volume) => !volume.refused && volume.path.startsWith("/Volumes/")).sort((a, b) => b.freeBytes - a.freeBytes);
        const fallback = usable[0] ? `${usable[0].path}/captain-salvage-recovered` : "";
        setDestination((current) => current ?? saved.destination ?? fallback);
      })
      .catch((reason) => setError(String(reason)));
  }, []);

  useEffect(() => {
    setPlan(null);
    if (destination === null || !destination.trim()) return;
    let current = true;
    setChecking(true);
    const timer = setTimeout(() => {
      call<CopyPlan>("export_plan", { kind: item.kind, target: item.target, destination, includePartial })
        .then((value) => {
          if (current) {
            setPlan(value);
            setError(null);
          }
        })
        .catch((reason) => current && setError(String(reason)))
        .finally(() => current && setChecking(false));
    }, 350);
    return () => {
      current = false;
      clearTimeout(timer);
    };
  }, [item.kind, item.target, destination, includePartial]);

  async function rememberSalvageDrive(path: string | null) {
    setError(null);
    try {
      const saved = await call<{ destination: string | null }>("set_salvage_drive", path ? { destination: path } : {});
      setSalvage(saved.destination);
    } catch (reason) {
      setError(String(reason));
    }
  }

  async function start() {
    setError(null);
    try {
      await call("export_start", { kind: item.kind, target: item.target, destination, includePartial });
      setStarted(true);
    } catch (reason) {
      setError(String(reason));
    }
  }

  const free = plan?.freeBytes ?? 0;
  const fraction = plan && free > 0 ? Math.min(plan.bytes / free, 1) : 0;
  const showCopy = started && copy && copy.state !== "" && copy.destination === destination;
  return (
    <div className="report-backdrop" role="dialog" aria-modal="true" aria-label={t("Export {name}", { name: item.name })} onClick={onClose}>
      <section className="report export-dialog" onClick={(event) => event.stopPropagation()}>
        <header className="report-head">
          <div>
            <h3>{item.kind === "folder" ? t("Export this folder") : t("Export this file")}</h3>
            <div className="muted detail-path">{item.name}</div>
          </div>
          <button className="icon-button" aria-label={t("Close")} onClick={onClose}><Icon name="close" size={13} /></button>
        </header>
        <p className="muted">{t("It is copied to the folder you choose, as it is, and never over a file that is already there. Each copy is checked as it is written. Nothing is written to the disk being recovered.")}</p>
        <label className="field">
          <span>{t("Copy to")}</span>
          <input value={destination ?? ""} onChange={(event) => setDestination(event.target.value)} spellCheck={false} placeholder="/Volumes/YourDisk/recovered" />
        </label>
        {volumes === null && <p className="muted">{t("Finding disks.")}</p>}
        <div className="volume-list">
          {volumes?.map((volume) => (
            <button
              key={volume.path}
              className="link"
              disabled={Boolean(volume.refused)}
              title={volume.refused ? fromEngine(volume.refused) : undefined}
              onClick={() => setDestination(`${volume.path}/captain-salvage-recovered`)}
            >
              {volume.path.split("/").pop()} ({volume.refused ? t("not allowed") : t("{bytes} free", { bytes: bytes(volume.freeBytes) })})
            </button>
          ))}
        </div>
        <div className="salvage">
          {salvage ? (
            <span>
              {t("Salvage drive:")} <span className="mono">{salvage}</span>.{" "}
              <button className="link" onClick={() => void rememberSalvageDrive(null)}>{t("Forget it")}</button>
            </span>
          ) : (
            <span className="muted">{t("No salvage drive set.")}</span>
          )}
          {destination?.trim() && destination !== salvage && (
            <button className="link" onClick={() => void rememberSalvageDrive(destination.trim())}>{t("Use this folder as the salvage drive")}</button>
          )}
        </div>
        <label className="check">
          <input type="checkbox" checked={includePartial} onChange={(event) => setIncludePartial(event.target.checked)} />
          {t("Also copy files with missing or reused clusters")}
        </label>
        {plan && (
          <div className="copy-total">
            <div className="copy-total-line">
              <strong>{t("{count} files, {bytes}", { count: count(plan.files), bytes: bytes(plan.bytes) })}</strong>
              {!plan.refused && <span className="muted">{t("{bytes} free", { bytes: bytes(free) })}</span>}
            </div>
            {!plan.refused && <div className="scan-bar"><div className={plan.fits ? "scan-fill" : "scan-fill scan-fill-over"} style={{ width: `${fraction * 100}%` }} /></div>}
            {!plan.fits && !plan.refused && <div className="warn">{t("This does not fit on that disk.")}</div>}
            {plan.skippedPartial > 0 && <div className="muted">{t("{count} partial files left out, {bytes}.", { count: count(plan.skippedPartial), bytes: bytes(plan.skippedPartialBytes) })}</div>}
          {(plan.badSectorFiles ?? 0) > 0 && <div className="warn"><span className="legend-swatch bad-swatch" /> {t("{count} files have bytes in bad sectors, {bytes} in all. They are copied with zeros there, and the manifest names them.", { count: count(plan.badSectorFiles ?? 0), bytes: bytes(plan.badSectorBytes ?? 0) })}</div>}
            {plan.refused && <p className="warn"><strong>{fromEngine(plan.refused)}</strong></p>}
          </div>
        )}
        {checking && !plan && <div className="muted copy-total">{t("Working out the total.")}</div>}
        {showCopy && (
          <div className="test-report copy-progress">
            <strong>
              {copy.state === "running" ? t("Copying {done} of {total} files", { done: count(copy.filesDone + copy.filesSkipped), total: count(copy.filesTotal) })
                : copy.state === "finished" ? t("Copy finished")
                : copy.state === "stopped" ? t("Copy stopped. Start it again to continue where it stopped.")
                : t("Copy failed")}
            </strong>
            <div className="scan-bar"><div className="scan-fill" style={{ width: `${copy.bytesTotal ? Math.min((copy.bytesDone / copy.bytesTotal) * 100, 100) : 0}%` }} /></div>
            <div>{t("{done} of {total}", { done: bytes(Math.min(copy.bytesDone, copy.bytesTotal)), total: bytes(copy.bytesTotal) })}</div>
            {copy.current && <div className="muted detail-path">{copy.current}</div>}
            {copy.filesSkipped > 0 && <div className="muted">{t("{count} already copied, skipped.", { count: count(copy.filesSkipped) })}</div>}
            {copy.filesPartial > 0 && <div>{t("{count} partial. The manifest says what is missing from each.", { count: count(copy.filesPartial) })}</div>}
            {copy.badClusters > 0 && <div className="warn">{t("{count} unreadable clusters were filled with zeros.", { count: count(copy.badClusters) })}</div>}
            {copy.filesFailed > 0 && <div className="warn">{t("{count} failed.", { count: count(copy.filesFailed) })}</div>}
            {copy.error && <div className="warn">{fromEngine(copy.error)}</div>}
            <div className="detail-actions">
              {running && <button onClick={() => call("copy_stop")}>{t("Stop copying")}</button>}
              <button onClick={() => setReportOpen(true)}>{t("Open the copy report")}</button>
            </div>
          </div>
        )}
        {reportOpen && copy?.destination && <CopyReport destination={copy.destination} onClose={() => setReportOpen(false)} />}
        <div className="detail-actions">
          <button className="primary" onClick={() => void start()} disabled={!plan || Boolean(plan.refused) || !plan.fits || plan.files === 0 || running}>
            {plan ? t("Export {count} files", { count: count(plan.files) }) : t("Export")}
          </button>
          <button onClick={onClose}>{t("Done")}</button>
        </div>
        {error && <p className="warn">{fromEngine(error.replace(/^Error: /, ""))}</p>}
      </section>
    </div>
  );
}
