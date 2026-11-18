import { useEffect, useRef, useState } from "react";
import { call } from "../api";
import { bytes, count, percent } from "../format";
import { fromEngine, t } from "../i18n";
import { SOURCE_FORMATS, competition, extentFromEnds, type Hypothesis, type HypothesisList } from "../rawDisk";
import type { ByteExtent } from "../openedSource";

/**
 * The hypotheses saved for this source: an extent and a format, with a confidence. They overlap
 * and compete as layers that can be switched on and off, and their start and end can be typed or
 * set from the selection. They are claims kept on this Mac; nothing is ever written to the disk.
 */
export function Hypotheses({ diskId, sourceLength, selectionStart, selectionLength, selectionValid, busy, refresh, onSelect, onOpen }: {
  diskId: string;
  sourceLength: number;
  selectionStart: number;
  selectionLength: number;
  selectionValid: boolean;
  busy: boolean;
  /** Changes when something outside saved a hypothesis, so the list is read again. */
  refresh: number;
  onSelect: (start: number, length: number) => void;
  onOpen: (extent: ByteExtent, reading: { format: string; bootOffset?: number }) => Promise<void>;
}) {
  const [list, setList] = useState<HypothesisList | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [format, setFormat] = useState(SOURCE_FORMATS[0]);
  const [edits, setEdits] = useState<Record<string, { start: string; end: string }>>({});
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);

  async function run(tool: string, args: object) {
    setFailure(null);
    try {
      await call(tool, { diskId, ...args });
      const next = await call<HypothesisList>("list_hypotheses", { diskId });
      if (mounted.current) { setList(next); setEdits({}); }
    } catch (reason) {
      if (mounted.current) setFailure(String(reason));
    }
  }

  useEffect(() => {
    let current = true;
    void call<HypothesisList>("list_hypotheses", { diskId })
      .then((next) => { if (current) setList(next); })
      .catch((reason) => { if (current) setFailure(String(reason)); });
    return () => { current = false; };
  }, [diskId, refresh]);

  const ends = (item: Hypothesis) => edits[item.id] ?? { start: String(item.start), end: String(item.start + item.length) };
  const typed = (item: Hypothesis) => extentFromEnds(ends(item).start, ends(item).end, sourceLength);
  const changed = (item: Hypothesis) => {
    const extent = typed(item);
    return extent !== null && (extent.start !== item.start || extent.length !== item.length);
  };

  return <div className="detail" aria-label={t("Hypotheses")}>
    <h4>{t("Hypotheses")}</h4>
    <p className="muted">{t("A hypothesis is an extent and a format you think it holds, kept with a confidence. Hypotheses can overlap and compete as layers: switch them on and off, and the upper one is read. They are kept on this Mac; nothing is written to the disk.")}</p>
    <label className="field">{t("Format")}<select value={format} disabled={busy} onChange={(event) => setFormat(event.target.value)}>{SOURCE_FORMATS.map((name) => <option key={name}>{name}</option>)}</select></label>
    <div className="detail-actions">
      <button disabled={!selectionValid || busy} onClick={() => void run("save_hypothesis", { format, start: selectionStart, length: selectionLength, label: t("{format} hypothesis", { format }) })}>
        {t("Save selection as hypothesis")}
      </button>
    </div>
    {list?.hypotheses.length === 0 && <p className="empty">{t("No hypotheses saved yet.")}</p>}
    {list && [...list.hypotheses].reverse().map((item) => {
      const rival = competition(list.layout, item.id);
      const typedEnds = ends(item);
      return <div key={item.id} className="detail" role="group" aria-label={item.label}>
        <label className="field">
          <span><input type="checkbox" checked={item.enabled} disabled={busy}
            onChange={(event) => void run("update_hypothesis", { id: item.id, enabled: event.target.checked })} /> {item.label}</span>
        </label>
        <p className="muted">
          {item.format} · {t("{confidence} sure", { confidence: percent(item.confidence) })} · {bytes(item.length)}
          {item.enabled && rival.rivals > 0 && <> · {rival.onTop ? t("on top of {count} others where they overlap", { count: count(rival.rivals) }) : t("covered by another layer where they overlap")}</>}
        </p>
        <label className="field">{t("Start byte")}<input value={typedEnds.start} inputMode="numeric" disabled={busy}
          onChange={(event) => setEdits({ ...edits, [item.id]: { ...typedEnds, start: event.target.value } })} /></label>
        <label className="field">{t("End byte")}<input value={typedEnds.end} inputMode="numeric" disabled={busy}
          onChange={(event) => setEdits({ ...edits, [item.id]: { ...typedEnds, end: event.target.value } })} /></label>
        <div className="detail-actions">
          <button disabled={busy || !changed(item)} onClick={() => { const extent = typed(item); if (extent) void run("update_hypothesis", { id: item.id, ...extent }); }}>{t("Apply start and end")}</button>
          <button disabled={busy || !selectionValid} onClick={() => void run("update_hypothesis", { id: item.id, start: selectionStart, length: selectionLength })}>{t("Use the selection")}</button>
          <button disabled={busy} onClick={() => onSelect(item.start, item.length)}>{t("Select")}</button>
          <button disabled={busy} onClick={() => void onOpen({ start: item.start, length: item.length }, { format: item.format, bootOffset: item.bootOffset === null ? undefined : item.bootOffset - item.start }).catch((reason) => setFailure(String(reason)))}>{t("Open")}</button>
          <button disabled={busy} onClick={() => void run("raise_hypothesis", { id: item.id })}>{t("Put on top")}</button>
          <button disabled={busy} onClick={() => void run("delete_hypothesis", { id: item.id })}>{t("Forget")}</button>
        </div>
      </div>;
    })}
    {list && list.layout.partitionTable.length > 0 && <>
      <h4>{t("Virtual partition table")}</h4>
      <p className="muted">{t("The enabled hypotheses laid out as partitions of a virtual disk. It is a view kept by the app, not a partition table on the disk.")}</p>
      {list.layout.partitionTable.map((entry) => <p className="muted" key={entry.id}>
        {count(entry.virtualStart)} + {count(entry.length)} · {entry.format} · {entry.label}{entry.shadowed ? ` · ${t("partly covered")}` : ""}
      </p>)}
    </>}
    {failure && <p role="alert" className="warn">{fromEngine(failure)}</p>}
  </div>;
}
