import { useMemo, useState } from "react";
import { call } from "../api";
import { marks, type BadKind, type BadRange } from "../badSectors";
import { count, bytes as formatBytes } from "../format";
import { fromEngine, locale, t } from "../i18n";

/** What a bad range is called everywhere, in the legend and in tooltips. */
export function badSectorsLabel(): string {
  return t("Bad sectors: reading here drops the drive");
}

/** How it was found, in a sentence. */
export function badKindText(kind: BadKind): string {
  switch (kind) {
    case "reset": return t("The drive reset during a read here.");
    case "failed": return t("A read here failed.");
    case "suspect": return t("Inside a band the probe jumped over, so it was not read.");
  }
}

/** When it was found, as the person's own settings write a date and a time. */
export function badFoundText(range: BadRange): string | null {
  if (!range.foundMillis) return null;
  return t("Found {when}", { when: new Date(range.foundMillis).toLocaleString(locale(), { dateStyle: "medium", timeStyle: "short" }) });
}

/** The flashing red marks over a strip: one for each bad stretch of what the strip shows. */
export function BadMarks({ bad, start, end }: { bad: BadRange[]; start: number; end: number }) {
  const shown = useMemo(() => marks(bad, start, end), [bad, start, end]);
  if (shown.length === 0) return null;
  return (
    <div className="strip-bad" aria-hidden="true">
      {shown.map((mark, index) => <span key={index} className="bad-range" style={{ left: `${mark.left}%`, width: `${mark.width}%` }} />)}
    </div>
  );
}

/** The legend entry: its swatch flashes like the marks do, and is hatched when motion is not wanted. */
export function BadLegendItem() {
  return <span className="legend-item" title={t("Ranges the drive dropped out on, or that failed to read, or that a probe jumped over. The app does not read them, to spare the drive.")}>
    <span className="legend-swatch bad-swatch" />{badSectorsLabel()}
  </span>;
}

/** What a tooltip adds when the pointer is on bad sectors: what they are, where, how found and when. */
export function BadTip({ ranges, unit }: { ranges: BadRange[]; unit: "bytes" | "clusters" }) {
  return <>
    {ranges.slice(0, 3).map((range) => {
      const found = badFoundText(range);
      return <div key={`${range.start}:${range.end}`} className="strip-tip-bad">
        <div className="strip-tip-row"><span className="legend-swatch bad-swatch" /><strong>{badSectorsLabel()}</strong></div>
        <div className="strip-tip-sub">{unit === "bytes"
          ? t("{from} to {to}", { from: formatBytes(range.start), to: formatBytes(range.end) })
          : t("Clusters {from} to {to}", { from: count(range.start), to: count(range.end - 1) })}</div>
        <div>{badKindText(range.kind)}{found ? ` ${found}.` : ""}</div>
      </div>;
    })}
    {ranges.length > 3 && <div className="strip-tip-sub">{t("and {count} more", { count: count(ranges.length - 3) })}</div>}
  </>;
}

/** Records a bad zone found outside the probe, such as by reading the drive with dd: its first and last byte (in
 * this source's own bytes), what happened, and why. It is kept with who said so, and nothing reads it after. */
export function MarkBadSectors({ diskId, length, onMarked }: { diskId: string; length: number; onMarked: () => void }) {
  const [start, setStart] = useState("");
  const [end, setEnd] = useState("");
  const [state, setState] = useState("reset-culprit");
  const [why, setWhy] = useState("");
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const [done, setDone] = useState(false);
  const first = Number(start.replace(/[ ,_]/g, ""));
  const last = Number(end.replace(/[ ,_]/g, ""));
  const valid = start.trim() !== "" && end.trim() !== "" && Number.isSafeInteger(first) && Number.isSafeInteger(last) && first >= 0 && last > first && last <= length && why.trim() !== "";
  async function mark() {
    if (!valid || busy) return;
    setBusy(true);
    setProblem(null);
    setDone(false);
    try {
      await call("probe_health", { diskId, action: "mark_bad", start: first, end: last, state, why: why.trim() });
      setDone(true);
      setWhy("");
      onMarked();
    } catch (reason) {
      setProblem(String(reason));
    } finally {
      setBusy(false);
    }
  }
  return <details className="mark-bad">
    <summary>{t("Mark bad sectors")}</summary>
    <p className="recovery-note">{t("A zone found outside the probe, for example by reading the drive with dd, in this source's own bytes. The app does not read it from then on, and keeps who marked it and why.")}</p>
    <div className="mark-bad-fields">
      <label>{t("First byte")}<input value={start} onChange={(event) => setStart(event.target.value)} inputMode="numeric" spellCheck={false} /></label>
      <label>{t("One past the last byte")}<input value={end} onChange={(event) => setEnd(event.target.value)} inputMode="numeric" spellCheck={false} /></label>
      <label>{t("What happened")}<select value={state} onChange={(event) => setState(event.target.value)}>
        <option value="reset-culprit">{t("The drive reset during a read")}</option>
        <option value="unreadable">{t("A read failed")}</option>
        <option value="suspect">{t("Keep out of it")}</option>
      </select></label>
      <label className="mark-bad-why">{t("Why: where it was found")}<input value={why} onChange={(event) => setWhy(event.target.value)} maxLength={500} spellCheck={false} /></label>
    </div>
    <button disabled={!valid || busy} onClick={() => void mark()}>{t("Mark as bad sectors")}</button>
    {done && <span className="recovery-note" role="status"> {t("Marked.")}</span>}
    {problem && <p className="warn" role="alert">{fromEngine(problem)}</p>}
  </details>;
}

/** A small flag for a file or folder of a Disk Rebuild with bytes in a bad sector: the same flashing red,
 * and how much. It is a flag and not a stop: the bytes copy as zeros. */
export function BadFlag({ bytesAffected, files }: { bytesAffected: number; files?: number }) {
  const text = files !== undefined
    ? t("{files} files, {bytes} in bad sectors", { files: count(files), bytes: formatBytes(bytesAffected) })
    : t("{bytes} in bad sectors", { bytes: formatBytes(bytesAffected) });
  return <span className="bad-flag" title={t("Bytes here are in bad sectors, which are not read. A copy writes zeros there and names them in its manifest.")}>
    <span className="legend-swatch bad-swatch" />{text}
  </span>;
}
