import { useEffect, useRef, useState } from "react";
import { call } from "../api";
import { bytes, count, percent } from "../format";
import { fromEngine, t } from "../i18n";
import { findingExtent, mergeFindings, type RangeFinding, type RangeReport } from "../rawDisk";

function structureLabel(structure: string): string {
  switch (structure) {
    case "boot sector": return t("Boot sector");
    case "backup boot sector": return t("Backup boot sector");
    case "FILE records": return t("NTFS file records");
    case "FILE records with damaged magic": return t("NTFS file records, signature damaged");
    case "checksummed directory entry sets": return t("Directory entry sets");
    case "directory entries": return t("Directory entries");
    default: return structure;
  }
}

function statusLabel(status: RangeFinding["status"]): string {
  switch (status) {
    case "volume": return t("Opens as a volume");
    case "inferred": return t("Opens with rebuilt geometry");
    case "structures": return t("Belongs to a volume found here");
    default: return t("Identified, not placed in a volume");
  }
}

/**
 * Analyzes the selected range: every filesystem driver is tried at every start offset, and what
 * decodes is listed with how sure the app is. Nothing is written; a finding is evidence, not proof.
 */
export function RangeAnalysis({ diskId, start, length, valid, busy, onSelect, onFindings }: {
  diskId: string;
  start: number;
  length: number;
  valid: boolean;
  busy: boolean;
  onSelect: (start: number, length: number) => void;
  /** Told of the findings so the caller can offer to save one as a hypothesis. */
  onFindings?: (findings: RangeFinding[]) => void;
}) {
  const [running, setRunning] = useState(false);
  const [report, setReport] = useState<RangeReport | null>(null);
  const [findings, setFindings] = useState<RangeFinding[]>([]);
  const [failure, setFailure] = useState<string | null>(null);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);
  useEffect(() => { setReport(null); setFindings([]); setFailure(null); }, [diskId]);
  useEffect(() => { onFindings?.(findings); }, [findings, onFindings]);

  async function analyze(offset: number, size: number, formats?: string[]) {
    if (running) return;
    setRunning(true);
    setFailure(null);
    try {
      const result = await call<RangeReport>("analyze_range", { diskId, offset, length: size, ...(formats ? { formats } : {}) });
      if (!mounted.current) return;
      if (formats) {
        setFindings((current) => mergeFindings(current, result.findings));
      } else {
        setReport(result);
        setFindings(result.findings);
      }
    } catch (reason) {
      if (mounted.current) setFailure(String(reason));
    } finally {
      if (mounted.current) setRunning(false);
    }
  }

  return <div className="detail" aria-label={t("Range analysis")}>
    <h4>{t("Range analysis")}</h4>
    <p className="muted">{t("Tries every filesystem driver at every start offset of the selected range, and lists what decodes. A finding is evidence, not proof. Nothing is written to the disk.")}</p>
    <div className="detail-actions">
      <button disabled={!valid || busy || running} onClick={() => void analyze(start, length)}>{running ? t("Analyzing") : t("Analyze this range")}</button>
    </div>
    {report && <p className="recovery-note" role="status">
      {t("{offsets} start offsets tried, {step} bytes apart.", { offsets: count(report.offsetsTried), step: count(report.step) })}
    </p>}
    {report?.drivers.map((driver) => <p className="muted" key={driver.driver}>
      {driver.bestOffset === null
        ? t("{driver}: nothing decodes.", { driver: driver.driver })
        : t("{driver}: the most decodes at byte {offset}, {size}.", { driver: driver.driver, offset: count(driver.bestOffset), size: bytes(driver.bestDecodedBytes) })}
    </p>)}
    {report && findings.length === 0 && <p className="empty">{t("Nothing decoded. Unrecognised does not mean empty.")}</p>}
    {findings.map((item) => {
      const extent = findingExtent(item);
      return <div key={`${item.format}:${item.structure}:${item.offset}`} className="raw-entry-actions">
        <button disabled={busy} title={item.names.join(", ")} onClick={() => onSelect(extent.start, Math.max(1, extent.length))}>
          {item.format} · {structureLabel(item.structure)}
        </button>
        <span>
          {t("byte {offset}", { offset: count(item.offset) })} · {bytes(item.decodedBytes)} · {percent(item.confidence)} · {item.level === "high" ? t("high confidence") : item.level === "medium" ? t("medium confidence") : t("low confidence")}
        </span>
        <span className="muted">{statusLabel(item.status)}</span>
        {item.offer && <button disabled={busy || running} onClick={() => void analyze(item.offer!.searchStart, item.offer!.searchLength, item.offer!.formats)}>
          {t("Search for missing metadata")}
        </button>}
      </div>;
    })}
    {report?.unreadable.map((item, index) => <p className="warn" key={index}>{count(item.offset)} + {count(item.length)}: {fromEngine(item.error)}</p>)}
    {failure && <p role="alert" className="warn">{fromEngine(failure)}</p>}
  </div>;
}
