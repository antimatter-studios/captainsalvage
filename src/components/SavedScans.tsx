import { useEffect, useRef, useState } from "react";
import { call } from "../api";
import { bytes, decimal } from "../format";
import { fromEngine, locale, t } from "../i18n";
import { staleSentence } from "./StaleRegions";
import { canOpenFrom, scanFraction, shownMatches, type ByteExtent, type ScanListing, type ScanRecord, type SourceMatchRecord } from "../openedSource";

function interpretation(scan: ScanRecord): string {
  if (scan.format === "none") return t("No filesystem (raw bytes)");
  const where = scan.start === 0 ? "" : ` ${t("at {offset}", { offset: bytes(scan.start) })}`;
  return `${scan.format}, ${bytes(scan.length)}${where}`;
}

function covered(scan: ScanRecord): string {
  if (scan.status === "not-started") return t("Not scanned yet");
  if (scan.complete) return t("Scanned all of {range}", { range: bytes(scan.rangeEnd - scan.rangeStart) });
  return t("Scanned {percent}% of {range}", { percent: decimal(scanFraction(scan) * 100, 0), range: bytes(scan.rangeEnd - scan.rangeStart) });
}

/** One stored scan: what it reads, when, how much it covers, what it keeps, its name, and a way in. */
function ScanRow({ scan, current, allowed, anyway, onOpen }: { scan: ScanRecord; current: boolean; allowed: boolean; anyway?: boolean; onOpen: () => void }) {
  const [label, setLabel] = useState(scan.label);
  const [failure, setFailure] = useState<string | null>(null);
  const saved = useRef(scan.label);
  function save() {
    const next = label.trim();
    if (next === saved.current) return;
    setFailure(null);
    call("label_scan", { scanId: scan.id, label: next }).then(() => { saved.current = next; }).catch((reason) => setFailure(String(reason)));
  }
  return (
    <li className="saved-scan">
      <div className="saved-scan-facts">
        <strong>{interpretation(scan)}</strong>
        <span>{new Date(scan.updatedAtMillis).toLocaleString(locale())}</span>
        <span>{covered(scan)}</span>
        <span>{t("Stores {size}", { size: bytes(scan.storedBytes) })}</span>
        {(scan.staleRegions ?? 0) > 0 && <span className="warn">{staleSentence(scan.staleRegions ?? 0)}</span>}
      </div>
      <input
        className="saved-scan-label"
        value={label}
        maxLength={128}
        placeholder={t("Name this scan")}
        aria-label={t("Name of this scan")}
        onChange={(event) => setLabel(event.target.value)}
        onBlur={save}
        onKeyDown={(event) => { if (event.key === "Enter") event.currentTarget.blur(); }}
      />
      {current ? <span className="muted saved-scan-now">{t("Open now")}</span> : <button className={anyway ? undefined : "primary"} disabled={!allowed} onClick={onOpen}>{anyway ? t("Resume anyway") : t("Open")}</button>}
      {failure && <span className="warn saved-scan-failure">{fromEngine(failure)}</span>}
    </li>
  );
}

/** A stored source the open one matches, and its scans. A probable match waits for an acknowledgement. */
function MatchBlock({ match, currentScan, onOpen }: { match: SourceMatchRecord; currentScan: string | null; onOpen: (scanId: string, acknowledged: boolean, anyway?: boolean) => void }) {
  const [acknowledged, setAcknowledged] = useState(false);
  const { comparison } = match;
  const state = match.verification;
  const anyway = state === "weak" || state === "different";
  return (
    <section className="saved-source" aria-label={match.label || match.lastPath}>
      <header>
        <strong>{match.label || match.lastPath.split("/").pop() || t("Stored source")}</strong>
        <span className={state === "exact" ? "saved-state saved-state-exact" : "saved-state"}>{state === "exact" ? t("exact") : state === "probable" ? t("probable") : state === "different" ? t("no match") : t("weak")}</span>
        <span className="muted">{t("{percent}% match: {matched} match, {changed} changed, {unreadable} unreadable", { percent: comparison.confidencePercent, matched: comparison.matched, changed: comparison.changed, unreadable: comparison.unreadable })}</span>
      </header>
      {state !== "exact" && <p className="warn">{fromEngine(comparison.summary)}</p>}
      {state === "probable" && (
        <label className="check saved-ack">
          <input type="checkbox" checked={acknowledged} onChange={(event) => setAcknowledged(event.target.checked)} />
          {t("I have read what changed, and want these scans anyway. Results near the changed markers may be out of date.")}
        </label>
      )}
      {state === "probable" && <p className="muted">{t("After opening, the regions near the changed markers are marked, and you can scan just those again. Nothing is scanned until you ask.")}</p>}
      {(state === "weak" || state === "different") && <p className="warn">{t("Too few markers match to say this is the same drive. Resume anyway only if you know it is. The scan then records that it was resumed without a match.")}</p>}
      {match.scans.length === 0 ? <p className="muted">{t("No scans are stored for it.")}</p> : (
        <ul className="saved-scans-list">
          {match.scans.map((scan) => (
            <ScanRow key={scan.id} scan={scan} current={scan.id === currentScan} allowed={anyway || canOpenFrom(state, acknowledged)} anyway={anyway} onOpen={() => onOpen(scan.id, state === "probable" || anyway, anyway)} />
          ))}
        </ul>
      )}
    </section>
  );
}

/**
 * The scans stored for the source that is open or being opened, by how well their markers match.
 * Matches under 60% are hidden unless asked for. An exact match opens its scan at once. A probable
 * one never opens without the person ticking that they read what changed. Opening a scan loads its
 * stored results and scans nothing. Before anything is open (`choice`), the person can also open
 * the drive as a new source, which applies nothing stored.
 */
export function SavedScans({ diskId, extent, currentScan, choice, onOpen, onFresh, onClose }: {
  diskId: string;
  extent: ByteExtent | null;
  currentScan: string | null;
  choice: boolean;
  onOpen: (scanId: string, acceptChangedMarkers: boolean, resumeAnyway?: boolean) => void;
  onFresh?: () => void;
  onClose: () => void;
}) {
  const [showAll, setShowAll] = useState(false);
  const [listing, setListing] = useState<ScanListing | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  useEffect(() => {
    let current = true;
    call<ScanListing>("list_scans", { diskId, showAll: showAll || choice, ...(choice ? { sameSize: true } : {}), ...(extent ? { extent } : {}) })
      .then((result) => { if (current) setListing(result); })
      .catch((reason) => { if (current) setFailure(String(reason)); });
    return () => { current = false; };
  }, [diskId, extent?.start, extent?.length, showAll, choice]);
  const matches = listing ? shownMatches(listing.matches, showAll || choice) : [];
  return (
    <div className="saved-scans-backdrop" role="presentation">
      <div className="saved-scans" role="dialog" aria-modal="true" aria-label={t("Saved scans")}>
        <h2>{choice ? t("This drive looks like one seen before") : t("Saved scans")}</h2>
        <p className="muted">
          {choice
            ? t("A mount or another program may have changed it. Nothing stored is applied until you choose, and nothing is scanned.")
            : t("Opening a scan loads its stored results and scans nothing. Changing the filesystem starts a new scan and keeps these.")}
        </p>
        <p className="muted">{t("A stored source is matched by markers, small blocks spread over it. The match is a confidence, not proof that every byte is identical.")}</p>
        <label className="check saved-show-all"><input type="checkbox" checked={showAll} onChange={(event) => setShowAll(event.target.checked)} /> {t("Show all")}</label>
        {listing && !showAll && listing.hidden > 0 && <p className="muted">{t("{count} stored sources match too little to be listed (under 60%).", { count: listing.hidden })}</p>}
        {matches.map((match) => <MatchBlock key={match.id} match={match} currentScan={currentScan} onOpen={onOpen} />)}
        {listing && matches.length === 0 && <p className="muted">{t("Nothing stored matches this drive.")}</p>}
        {failure && <p role="alert" className="warn">{fromEngine(failure)}</p>}
        <div className="saved-scans-actions">
          {choice && onFresh && <button onClick={onFresh} title={t("Applies nothing stored. The stored source is left as it is.")}>{t("Open as a new source")}</button>}
          <button onClick={onClose}>{choice ? t("Cancel") : t("Close")}</button>
        </div>
      </div>
    </div>
  );
}
