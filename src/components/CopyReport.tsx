import { useCallback, useEffect, useState } from "react";
import { call } from "../api";
import { bytes, count } from "../format";
import { fromEngine, t } from "../i18n";
import { Icon } from "./icons";

interface ReportRow {
  id: number;
  name: string;
  sourcePath: string;
  target: string;
  length: number;
  bytesWritten: number;
  status: string;
  check: "same" | "changed" | "missing" | null;
}

interface CopyReportData {
  destination: string;
  totals: { files: number; bytes: number; complete: number; partial: number; checked: number; same: number; changed: number; missing: number };
  checking: boolean;
  rows: ReportRow[];
  matching: number;
}

type Filter = "all" | "partial" | "changed" | "missing" | "unchecked";
const PAGE = 200;
const REFRESH_MILLIS = 1500;

/**
 * What a copy did, file by file, read from the manifest at its destination, with a check
 * that reads every copy again and compares it with what was copied. It never reads the disk
 * being recovered.
 */
export function CopyReport({ destination, onClose }: { destination: string; onClose: () => void }) {
  const [report, setReport] = useState<CopyReportData | null>(null);
  const [filter, setFilter] = useState<Filter>("all");
  const [limit, setLimit] = useState(PAGE);
  const [error, setError] = useState<string | null>(null);
  const load = useCallback(() => {
    call<CopyReportData>("copy_report", { destination, filter, limit })
      .then((found) => {
        setReport(found);
        setError(null);
      })
      .catch((reason) => setError(String(reason)));
  }, [destination, filter, limit]);
  useEffect(load, [load]);
  // While a check runs, its results come in a file at a time.
  useEffect(() => {
    if (!report?.checking) return;
    const timer = window.setInterval(load, REFRESH_MILLIS);
    return () => window.clearInterval(timer);
  }, [report?.checking, load]);

  const totals = report?.totals;
  const filters: [Filter, string, number | undefined][] = [
    ["all", t("All"), totals?.files],
    ["partial", t("Partial"), totals?.partial],
    ["changed", t("Changed since"), totals?.changed],
    ["missing", t("Missing"), totals?.missing],
    ["unchecked", t("Not checked"), totals ? totals.files - totals.checked : undefined],
  ];
  return (
    <div className="report-backdrop" role="dialog" aria-modal="true" aria-label={t("Copy report")} onClick={onClose}>
      <section className="report" onClick={(event) => event.stopPropagation()}>
        <header className="report-head">
          <div>
            <h3>{t("Copy report")}</h3>
            <div className="muted detail-path">{destination}</div>
          </div>
          <button className="icon-button" aria-label={t("Close")} onClick={onClose}><Icon name="close" size={13} /></button>
        </header>
        {error && <p className="warn">{fromEngine(error.replace(/^Error: /, ""))}</p>}
        {totals && (
          <div className="report-summary">
            <span>{t("{count} files, {bytes}", { count: count(totals.files), bytes: bytes(totals.bytes) })}</span>
            <span>{t("{count} complete", { count: count(totals.complete) })}</span>
            {totals.partial > 0 && <span className="warn">{t("{count} partial", { count: count(totals.partial) })}</span>}
            {totals.checked > 0 && <span>{t("Checked again: {same} the same, {changed} changed, {missing} missing", { same: count(totals.same), changed: count(totals.changed), missing: count(totals.missing) })}</span>}
          </div>
        )}
        <div className="report-tools">
          <div className="segmented" role="tablist">
            {filters.map(([id, label, value]) => (
              <button key={id} role="tab" aria-selected={filter === id} onClick={() => setFilter(id)}>
                {label}{value !== undefined ? ` ${count(value)}` : ""}
              </button>
            ))}
          </div>
          <button disabled={!report || report.checking} onClick={() => void call("copy_check", { destination }).then(load).catch((reason) => setError(String(reason)))} title={t("Read every copy again and compare it with what was copied. Only the copies are read.")}>
            {report?.checking ? t("Checking the copies") : t("Check the copies again")}
          </button>
        </div>
        <div className="report-rows">
          {report?.rows.map((row) => (
            <div key={row.id} className="report-row">
              <span className="report-name" title={row.target}>{row.target.startsWith(destination) ? row.target.slice(destination.length).replace(/^\//, "") : row.target}</span>
              <span className="report-size">{bytes(row.bytesWritten)}</span>
              <span className={row.status === "complete" ? "report-status" : "report-status warn"}>{row.status === "complete" ? t("Complete") : fromEngine(row.status)}</span>
              <span className={row.check === "same" ? "report-check" : row.check ? "report-check warn" : "report-check muted"}>
                {row.check === "same" ? t("The same") : row.check === "changed" ? t("Changed since") : row.check === "missing" ? t("Missing") : t("Not checked")}
              </span>
            </div>
          ))}
          {report && report.matching > report.rows.length && (
            <button className="more" onClick={() => setLimit(limit + PAGE)}>{t("Show more ({count} left)", { count: count(report.matching - report.rows.length) })}</button>
          )}
          {report && report.matching === 0 && <p className="empty">{t("No files here.")}</p>}
        </div>
      </section>
    </div>
  );
}
