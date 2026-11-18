import { useState } from "react";
import { call } from "../api";
import { fromEngine, t } from "../i18n";
import type { OpenedSource } from "../openedSource";
import { rescanOutcome, type RescanResult } from "../stale";

/** The small mark on a finding, an entry or a test result that may be out of date. */
export function StaleMark() {
  return (
    <span className="stale-mark" title={t("The drive changed near here after this was stored. Scan the changed regions again to check it.")}>
      {t("May be out of date")}
    </span>
  );
}

/** How many regions may be out of date, in one clear sentence. */
export function staleSentence(count: number): string {
  return count === 1 ? t("One region may be out of date.") : t("{count} regions may be out of date.", { count });
}

/**
 * Shown after a scan was applied to a source that was only a probable match: how many regions of
 * its results may be out of date, and the person's action to scan them again. Nothing is scanned
 * until they press it. A rescan that stops leaves the regions marked, and pressing it again
 * continues where it stopped.
 */
export function StaleBanner({ opened, onRescanned }: { opened: OpenedSource; onRescanned: (result: RescanResult) => void }) {
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<{ text: string; warn: boolean } | null>(null);
  const scan = opened.scan;
  const stale = opened.stale?.stale ?? 0;
  if (!scan) return null;
  if (stale === 0 && !note) return null;

  async function rescan() {
    if (!scan) return;
    setBusy(true);
    setNote(null);
    try {
      const result = await call<RescanResult>("rescan_changed_regions", { diskId: opened.disk.id, scanId: scan.id, ...(opened.extent ? { extent: opened.extent } : {}) });
      const outcome = rescanOutcome(result);
      setNote(outcome === "partial"
        ? { text: t("The rescan stopped before it was done. The regions stay marked, and rescanning continues where it stopped.") + (result.rescan.stopped ? ` ${fromEngine(result.rescan.stopped)}` : ""), warn: true }
        : { text: t("The changed regions were scanned again, and the drive's markers were recorded again, so this drive matches exactly next time."), warn: false });
      onRescanned(result);
    } catch (reason) {
      setNote({ text: fromEngine(String(reason)), warn: true });
    } finally {
      setBusy(false);
    }
  }

  return (
    <span className="interp interp-stale" role="status">
      {stale > 0 && (
        <>
          <span className="warn" title={t("The source changed near the markers that no longer match, so what was stored there may be out of date. Nothing is scanned again until you ask.")}>{staleSentence(stale)}</span>
          <button className="primary" disabled={busy} onClick={() => void rescan()}>{busy ? t("Scanning the changed regions again") : t("Rescan changed regions")}</button>
        </>
      )}
      {note && <span className={note.warn ? "interp-note warn" : "interp-note"}>{note.text}</span>}
    </span>
  );
}
