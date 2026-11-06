import { CopyReport } from "./CopyReport";
import { useEffect, useState } from "react";
import { call, type CopyPlan, type CopyStatus } from "../api";
import { bytes, count } from "../format";
import { fromEngine, t } from "../i18n";

/**
 * Copies a canvas's rebuilt tree off the disk: each file goes where the canvas puts it, as the
 * copy the canvas shows for it, in a folder named after the canvas.
 */
export function CanvasCopy({ canvas, copy, revision, onClose }: {
  canvas: number;
  copy: CopyStatus | null;
  revision: number;
  onClose: () => void;
}) {
  const [reportOpen, setReportOpen] = useState(false);
  const [destination, setDestination] = useState<string | null>(null);
  const [includePartial, setIncludePartial] = useState(false);
  const [plan, setPlan] = useState<CopyPlan | null>(null);
  const [checking, setChecking] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const running = copy?.state === "running";

  useEffect(() => {
    call<{ destination: string | null }>("get_salvage_drive")
      .then((saved) => setDestination((current) => current ?? saved.destination ?? ""))
      .catch((reason) => setError(String(reason)));
  }, []);

  useEffect(() => {
    setPlan(null);
    if (destination === null) return;
    let current = true;
    setChecking(true);
    const timer = setTimeout(() => {
      call<CopyPlan>("canvas_copy_plan", { canvas, destination, includePartial })
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
  }, [canvas, destination, includePartial, revision]);

  async function start() {
    setError(null);
    try {
      await call("canvas_copy_start", { canvas, destination, includePartial });
    } catch (reason) {
      setError(String(reason));
    }
  }

  const free = plan?.freeBytes ?? 0;
  const fraction = plan && free > 0 ? Math.min(plan.bytes / free, 1) : 0;
  return (
    <div className="canvas-copy">
      <div className="canvas-copy-head">
        <strong>{t("Copy the rebuilt tree")}</strong>
        <button className="link" onClick={onClose}>{t("Done")}</button>
      </div>
      <p className="muted">{t("Each file goes where this canvas puts it, as the copy shown for it, in a folder named after the canvas. Nothing is written to the disk being recovered.")}</p>
      <label className="field">
        <span>{t("Copy to")}</span>
        <input value={destination ?? ""} onChange={(event) => setDestination(event.target.value)} spellCheck={false} placeholder="/Volumes/YourDisk/recovered" />
      </label>
      <label className="check">
        <input type="checkbox" checked={includePartial} onChange={(event) => setIncludePartial(event.target.checked)} />
        {t("Also copy files with missing or reused clusters")}
      </label>
      {plan && (
        <div className="basket-total">
          <div className="basket-total-line">
            <strong>{t("{count} files, {bytes}", { count: count(plan.files), bytes: bytes(plan.bytes) })}</strong>
            {!plan.refused && <span className="muted">{t("{bytes} free", { bytes: bytes(free) })}</span>}
          </div>
          {!plan.refused && <div className="scan-bar"><div className={plan.fits ? "scan-fill" : "scan-fill scan-fill-over"} style={{ width: `${fraction * 100}%` }} /></div>}
          {!plan.refused && <div className="muted detail-path">{t("It goes in {path}", { path: plan.destination })}</div>}
          {!plan.fits && !plan.refused && <div className="warn">{t("This does not fit on that disk.")}</div>}
          {plan.skippedPartial > 0 && <div className="muted">{t("{count} partial files left out, {bytes}.", { count: count(plan.skippedPartial), bytes: bytes(plan.skippedPartialBytes) })}</div>}
          {plan.refused && <p className="warn"><strong>{fromEngine(plan.refused)}</strong></p>}
        </div>
      )}
      {checking && !plan && <div className="muted basket-total">{t("Working out the total.")}</div>}
      {copy && copy.state !== "" && plan && copy.destination === plan.destination && (
        <div className="test-report copy-progress">
          <strong>
            {copy.state === "running" ? t("Copying {done} of {total} files", { done: count(copy.filesDone + copy.filesSkipped), total: count(copy.filesTotal) })
              : copy.state === "finished" ? t("Copy finished")
              : copy.state === "stopped" ? t("Copy stopped. Start it again to continue where it stopped.")
              : t("Copy failed")}
          </strong>
          <div className="scan-bar"><div className="scan-fill" style={{ width: `${copy.bytesTotal ? Math.min((copy.bytesDone / copy.bytesTotal) * 100, 100) : 0}%` }} /></div>
          <div>{t("{done} of {total}", { done: bytes(Math.min(copy.bytesDone, copy.bytesTotal)), total: bytes(copy.bytesTotal) })}</div>
          {copy.filesPartial > 0 && <div>{t("{count} partial. The manifest says what is missing from each.", { count: count(copy.filesPartial) })}</div>}
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
          {plan ? t("Copy {count} files", { count: count(plan.files) }) : t("Copy")}
        </button>
      </div>
      {error && <p className="warn">{fromEngine(error.replace(/^Error: /, ""))}</p>}
    </div>
  );
}
