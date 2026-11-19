import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { call } from "../api";
import { bytes, count, decimal, duration } from "../format";
import { fromEngine, t } from "../i18n";
import { useSimple } from "../mode";
import { bandColors, legendFor, type LegendItem, type Rgb } from "../palette";
import { MAP_HEALTH_CULPRIT, MAP_HEALTH_SLOW, MAP_HEALTH_UNREADABLE, mapRowCount, type HealthRange, MAP_BAND, MAP_BAND_ALTERNATE, MAP_EXAMINED, MAP_FIRST_FORMAT, MAP_NOT_SCANNED, MAP_UNREADABLE, SOURCE_FORMATS, applyScanBatch, evidenceStatus, evidenceSummary, itemAt, laneMap, scanEstimate, type DiscoveryBatch, type DiskEvidence, type EvidenceStatus, type RawDiskState, type ScanSnapshot } from "../rawDisk";
import type { ByteExtent, OpenedSource } from "../openedSource";
import { Icon } from "./icons";
import { StaleMark } from "./StaleRegions";
import { Hypotheses } from "./Hypotheses";
import { RangeAnalysis } from "./RangeAnalysis";
import { Strip, type StripSource } from "./Strips";
import { WorkspaceToolbar } from "./WorkspaceToolbar";
import "./RawDiskView.css";

const MAX_FINDINGS = 2000;
const FINDINGS_PAGE_SIZE = 100;
/** Each row of the map is this tall, so the partition bands (several rows) read broad and each lane thin. */
const ROW_PIXELS = 9;
const MAGNIFIED_ROW_PIXELS = 18;
/** A click this close to a finding, in strip pixels, picks it. */
const PICK_PIXELS = 3;
const STRIP_PIXELS = 1200;
export function RawDiskView({ disk, onUpdate, opened, picker, onOpen, onBack, onReturn, center, onCenter, controls, leftHandle, rightHandle }: {
  disk: RawDiskState;
  onUpdate: (disk: RawDiskState) => void;
  /** What opening the source answered: its scope, its scans and the extents it is known to hold. */
  opened: OpenedSource | null;
  /** The interpretation picker, which sits in the header beside the source's name. */
  picker: ReactNode;
  /** Opens an extent of this source as a source of its own, as the format given or the one detected. */
  onOpen: (extent: ByteExtent, reading?: { format: string; bootOffset?: number }) => Promise<void>;
  onBack: () => void;
  onReturn?: () => void;
  center: "overview" | "bytes";
  onCenter: (center: "overview" | "bytes") => void;
  controls: ReactNode;
  leftHandle: ReactNode;
  rightHandle: ReactNode;
}) {
  const simple = useSimple();
  const [start, setStart] = useState("0");
  const [length, setLength] = useState(String(disk.length));
  const [format, setFormat] = useState("exFAT");
  const [label, setLabel] = useState("");
  const [bootOffset, setBootOffset] = useState("0");
  const [running, setRunning] = useState(false);
  const [opening, setOpening] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const [hex, setHex] = useState<string | null>(null);
  const [estimate, setEstimate] = useState<ReturnType<typeof scanEstimate>>(null);
  const stop = useRef(false);
  const mounted = useRef(true);
  const [selected, setSelected] = useState("source");
  const [filter, setFilter] = useState<string | null>(null);
  const [inspector, setInspector] = useState<"selected" | "volume">("selected");
  const [visibleFindings, setVisibleFindings] = useState(FINDINGS_PAGE_SIZE);
  const [visibleErrors, setVisibleErrors] = useState(FINDINGS_PAGE_SIZE);
  const [hypothesesRevision, setHypothesesRevision] = useState(0);
  useEffect(() => {
    mounted.current = true;
    setVisibleFindings(FINDINGS_PAGE_SIZE);
    setVisibleErrors(FINDINGS_PAGE_SIZE);
    return () => { mounted.current = false; stop.current = true; };
  }, [disk.id]);

  const busy = running || opening;
  const first = Number(start);
  const size = Number(length);
  const boot = Number(bootOffset);
  const valid = start.trim() !== "" && length.trim() !== "" && bootOffset.trim() !== ""
    && [first, size, boot].every(Number.isSafeInteger)
    && first >= 0 && size > 0 && first + size <= disk.length && boot >= 0 && boot < disk.length;

  function selectRange(offset: number, count: number, key = "region") {
    setStart(String(offset));
    setLength(String(count));
    setBootOffset(String(offset));
    setHex(null);
    setSelected(key);
    setInspector("selected");
  }

  /** Opens a declared partition as a source of its own: the detected filesystem, or raw bytes. */
  async function openPartition(index: number) {
    const partition = disk.partitions[index];
    if (running || opening) return;
    setOpening(true);
    setFailure(null);
    try {
      await onOpen({ start: partition.start, length: partition.length });
    } catch (reason) {
      setFailure(String(reason));
    } finally {
      setOpening(false);
    }
  }

  function selectPartition(index: number) {
    const partition = disk.partitions[index];
    selectRange(partition.start, partition.length, `partition:${index}`);
    setLabel(partition.label);
  }

  function selectEvidence(evidence: DiskEvidence) {
    const offset = evidence.volumeStart ?? evidence.offset;
    selectRange(offset, Math.min(evidence.volumeLength ?? evidence.length, disk.length - offset), evidenceKey(evidence));
    setFormat(evidence.format);
    setLabel(`${evidence.format}: ${evidence.structure}`);
    setBootOffset(String(evidence.bootOffset ?? offset));
  }

  async function discover(resume: boolean) {
    if ((!resume && !valid) || running || opening) return;
    stop.current = false;
    setRunning(true);
    setFailure(null);
    setEstimate(null);
    try {
      let current: RawDiskState = {
        ...disk, ...await call<ScanSnapshot>(resume ? "raw_scan_status" : "create_raw_scan", resume
          ? { diskId: disk.id, scanId: disk.scanId }
          : { diskId: disk.id, offset: first, length: size }),
      };
      if (!mounted.current) return;
      if (current.scanId === null || current.nextOffset === null || current.scanEnd === null) throw new Error("No saved scan is loaded.");
      const attemptStart = current.nextOffset;
      const end = current.scanEnd;
      const scanId = current.scanId;
      let cursor = current.nextOffset;
      const started = performance.now();
      onUpdate(current);
      while (cursor < end && !stop.current) {
        const result = await call<DiscoveryBatch>("scan_raw_batch", { diskId: disk.id, scanId });
        if (!mounted.current) return;
        if (result.nextOffset <= cursor) throw new Error("Discovery did not advance.");
        cursor = result.nextOffset;
        current = applyScanBatch(current, result);
        setEstimate(scanEstimate(attemptStart, cursor, end, performance.now() - started));
        onUpdate(current);
        if (current.findings.length >= MAX_FINDINGS || current.errors.length >= MAX_FINDINGS) {
          setFailure(t("Display limit reached. Select a smaller region before continuing."));
          break;
        }
      }
    } catch (reason) {
      if (mounted.current) setFailure(String(reason));
    } finally {
      if (mounted.current) setRunning(false);
    }
  }

  async function openInterpretation() {
    if (!valid || running) return;
    setOpening(true);
    setFailure(null);
    try {
      await onOpen({ start: first, length: size }, { format, bootOffset: boot - first });
    } catch (reason) {
      setFailure(String(reason));
    } finally {
      setOpening(false);
    }
  }

  const summary = useMemo(() => evidenceSummary(disk.findings), [disk.findings]);
  const shown = useMemo(() => filter === null ? disk.findings : disk.findings.filter((item) => item.format === filter), [disk.findings, filter]);
  const lanes = useMemo(() => summary.map((item) => item.format), [summary]);
  const complete = disk.nextOffset !== null && disk.scanEnd !== null && disk.nextOffset >= disk.scanEnd;
  const fraction = disk.nextOffset !== null && disk.scanStart !== null && disk.scanEnd !== null
    ? (disk.nextOffset - disk.scanStart) / Math.max(1, disk.scanEnd - disk.scanStart) : 0;
  const scanText = complete ? t("Scan complete") : disk.nextOffset === null ? t("Scan not started")
    : running && estimate ? t("Scan {percent}%, {speed}/s, {time} left", { percent: decimal(fraction * 100, 1), speed: bytes(estimate.bytesPerSecond), time: duration(estimate.secondsLeft) })
    : running ? t("Scan {percent}%", { percent: decimal(fraction * 100, 1) })
    : t("Scan paused at {percent}%", { percent: decimal(fraction * 100, 1) });
  // What the read journal and the probe found wrong with the drive, drawn as a lane of the map. A source
  // opened without a health guard has none, and the lane is not drawn.
  const [health, setHealth] = useState<HealthRange[]>([]);
  useEffect(() => {
    let current = true;
    const load = () => call<{ ranges: HealthRange[] }>("health_map", { diskId: disk.id })
      .then((value) => { if (current) setHealth(value.ranges.filter((range) => range.state !== "good")); })
      .catch(() => { if (current) setHealth([]); });
    void load();
    const timer = window.setInterval(() => void load(), 5000);
    return () => { current = false; window.clearInterval(timer); };
  }, [disk.id]);
  const drawn = useMemo(() => ({ ...disk, health }), [disk, health]);
  const mapSource = useMemo<StripSource>(() => {
    const legend = (): LegendItem[] => {
      const palette = legendFor("content");
      const [bandA, bandB] = bandColors();
      return [
        { value: MAP_NOT_SCANNED, label: t("Not scanned yet"), color: palette.find((item) => item.value === 0)!.color },
        { value: MAP_EXAMINED, label: t("Examined bytes"), color: palette.find((item) => item.value === 1)!.color },
        { value: MAP_UNREADABLE, label: t("Unreadable regions"), color: palette.find((item) => item.value === 10)!.color },
        { value: MAP_BAND, label: t("Declared partitions"), color: bandA },
        { value: MAP_BAND_ALTERNATE, label: t("Declared partitions"), color: bandB },
        ...SOURCE_FORMATS.map((name, index) => ({ value: MAP_FIRST_FORMAT + index, label: name, color: palette[index].color })),
        { value: MAP_HEALTH_SLOW, label: t("Slow to read"), color: palette.find((item) => item.value === 5)!.color },
        { value: MAP_HEALTH_UNREADABLE, label: t("Marked unreadable"), color: palette.find((item) => item.value === 10)!.color },
        { value: MAP_HEALTH_CULPRIT, label: t("The drive reset here"), color: palette.find((item) => item.value === 9)!.color },
      ];
    };
    const hypotheses = new Map<string, { first: number; count: number }>();
    disk.findings.forEach((item) => {
      if (item.volumeStart !== null && item.volumeLength !== null) {
        hypotheses.set(`${item.volumeStart}:${item.volumeLength}`, { first: item.volumeStart, count: item.volumeLength });
      }
    });
    return {
      read: (begin, end, buckets) => laneMap(drawn, lanes, begin, end, buckets),
      rows: mapRowCount(drawn, lanes.length),
      lanes: true,
      legend,
      colors: () => {
        const items = legend();
        return Array.from({ length: 256 }, (_, value): Rgb => {
          const color = items.find((item) => item.value === value)?.color ?? items[0].color;
          const rgb = Number.parseInt(color.slice(1), 16);
          return [(rgb >> 16) & 255, (rgb >> 8) & 255, rgb & 255];
        });
      },
      hypotheses: [...hypotheses.values()],
    };
  }, [disk, drawn, lanes]);

  useEffect(() => {
    if (center !== "bytes" || !valid) return;
    let current = true;
    setHex(null);
    setFailure(null);
    void call<{ hex: string }>("read_raw_disk", { diskId: disk.id, offset: first, length: Math.min(512, size) })
      .then((value) => { if (current) setHex(value.hex); })
      .catch((reason) => { if (current) setFailure(String(reason)); });
    return () => { current = false; };
  }, [disk.id, first, size, valid, center]);

  function changeView(begin: number, end: number) {
    if (running || opening) return;
    const width = Math.min(disk.length, Math.max(Math.min(512, disk.length), Math.round(end - begin)));
    const offset = Math.max(0, Math.min(disk.length - width, Math.round(begin)));
    selectRange(offset, width);
  }

  const mapProps = { layer: "ownership" as const, floor: 0, lens: null, revision: 0,
    highlight: [], marker: null, scanAt: running ? disk.nextOffset : null, clusterSize: 1, source: mapSource };
  const mapRows = mapRowCount(drawn, lanes.length);

  /** A click on the top map picks what is there: a finding, else the partition, else it moves the magnified view. */
  function pickOnMap(offset: number) {
    if (busy) return;
    const found = itemAt(disk, offset, disk.length / STRIP_PIXELS * PICK_PIXELS);
    if (found?.kind === "evidence") selectEvidence(found.item);
    else if (found?.kind === "partition") selectPartition(found.index);
    else changeView(offset - size / 2, offset + size / 2);
  }
  const selectedEvidence = selected.startsWith("evidence:") ? disk.findings.find((item) => evidenceKey(item) === selected) ?? null : null;
  const selectedPartition = selected.startsWith("partition:") ? disk.partitions[Number(selected.slice("partition:".length))] ?? null : null;
  const hexRows = hex?.match(/.{1,32}/g) ?? [];

  return <>
    <WorkspaceToolbar path={disk.path} length={disk.length} description={picker} progress={{ text: scanText, fraction, error: failure }} onChangeDisk={onBack}>
      {running ? <button onClick={() => { stop.current = true; }}>{t("Pause scan")}</button>
        : <button disabled={((disk.nextOffset === null || complete) && !valid) || opening} title={t("Search for filesystem evidence")} onClick={() => void discover(disk.nextOffset !== null && !complete)}>
          {disk.nextOffset !== null && !complete ? t("Resume scan") : t("Start scan")}
        </button>}
      {controls}
    </WorkspaceToolbar>
    {!simple && <section className="strips source-strips" aria-label={t("Raw disk map")}>
      <Strip {...mapProps} start={0} end={disk.length} height={mapRows * ROW_PIXELS} label={t("Source bytes")}
        windowBox={valid && size < disk.length ? [first, first + size] : null}
        onDragRange={changeView} onPick={pickOnMap} />
      {valid && <Strip {...mapProps} start={first} end={first + size} height={Math.max(74, mapRows * MAGNIFIED_ROW_PIXELS)} label={t("Selected region")}
        onPan={(delta) => changeView(first + delta, first + size + delta)}
        onZoom={(offset, factor) => changeView(offset - (offset - first) * factor, offset + (first + size - offset) * factor)}
        onPick={(offset) => { if (!busy) { selectRange(offset, Math.min(512, disk.length - offset)); onCenter("bytes"); } }} />}
      <div className="ruler" aria-hidden="true">
        {[0, 0.25, 0.5, 0.75, 1].map((position) => <span key={position} style={{ left: `${position * 100}%` }}>{bytes(Math.round((valid ? first : 0) + (valid ? size : disk.length) * position))}</span>)}
      </div>
      <div className="legend" title={t("Solid marks show observed structures; dashed outlines show possible volume extents.")}>
        {mapSource.legend().filter((item) => item.value === MAP_NOT_SCANNED || item.value === MAP_EXAMINED || item.value === MAP_UNREADABLE && disk.errors.length > 0 || item.value === MAP_BAND && disk.partitions.length > 0 || item.value === MAP_HEALTH_SLOW && health.some((range) => range.state === "slow") || item.value === MAP_HEALTH_UNREADABLE && health.some((range) => range.state === "unreadable") || item.value === MAP_HEALTH_CULPRIT && health.some((range) => range.state === "reset-culprit")).map((item) =>
          <span className="legend-item" key={item.value}><span className="legend-swatch" style={{ background: item.color }} />{item.label}</span>)}
        {summary.map((item) => {
          const entry = mapSource.legend().find((candidate) => candidate.label === item.format);
          return <span className="legend-item" key={item.format}><span className="legend-swatch" style={{ background: entry?.color }} />{item.format} · {count(item.count)}</span>;
        })}
      </div>
    </section>}
    <nav className="explorer" aria-label={t("Source bytes")}>
      {leftHandle}
      <div className="segmented" role="tablist">
        <button role="tab" aria-selected="true">{t("Source bytes")}</button>
      </div>
      <div className="panel-body">
        <button className={`row${selected === "source" ? " row-selected" : ""}`} disabled={busy} onClick={() => { selectRange(0, disk.length, "source"); setLabel(""); }}>
          <span className="row-icon"><Icon name="disk" size={18} /></span>
          <span className="row-main"><span className="row-name">{disk.path.split("/").pop()}</span><span className="row-path">{t("Source bytes")}</span></span>
          <span className="row-size">{bytes(disk.length)}</span>
        </button>
        <h3 className="source-section-label">{t("Declared partitions")}</h3>
        {disk.partitions.map((partition, index) => {
          const known = opened?.extents.find((item) => item.start === partition.start && item.length === partition.length);
          return <div key={index} className="row-with-open">
            <button className={`row${selected === `partition:${index}` ? " row-selected" : ""}`} disabled={busy}
              onClick={() => { selectRange(partition.start, partition.length, `partition:${index}`); setLabel(partition.label); }}>
              <span className="row-icon"><Icon name="disk" size={16} /></span>
              <span className="row-main"><span className="row-name">{partition.label || partition.table}</span><span className="row-path">{partition.table} · {bytes(partition.start)}{known && known.scans.length > 0 ? ` · ${t("{count} saved scans", { count: known.scans.length })}` : ""}</span></span>
              <span className="row-size">{bytes(partition.length)}</span>
            </button>
            <button className="row-open" disabled={busy} title={t("Opens this partition as a source of its own, as the filesystem found there or as raw bytes.")} onClick={() => void openPartition(index)}>{t("Open")}</button>
          </div>;
        })}
        <h3 className="source-section-label">{t("Filesystem evidence")}</h3>
        {summary.length > 1 && <div className="format-filter" role="group" aria-label={t("Filesystem evidence")}>
          <button aria-pressed={filter === null} onClick={() => { setFilter(null); setVisibleFindings(FINDINGS_PAGE_SIZE); }}>{t("All")} · {count(disk.findings.length)}</button>
          {summary.map((item) => <button key={item.format} aria-pressed={filter === item.format} onClick={() => { setFilter(item.format); setVisibleFindings(FINDINGS_PAGE_SIZE); }}>{item.format} · {count(item.count)}</button>)}
        </div>}
        {shown.slice(0, visibleFindings).map((item, index) => <button key={index} className={`row${selected === evidenceKey(item) ? " row-selected" : ""}`} disabled={busy} onClick={() => selectEvidence(item)}>
          <span className="row-icon"><Icon name="search" size={16} /></span>
          <span className="row-main"><span className="row-name">{item.format} · {fromEngine(item.structure)}{item.stale && <StaleMark />}</span><span className="row-path">{bytes(item.offset)} · {fromEngine(item.confidence)}</span></span>
        </button>)}
        {visibleFindings < shown.length && <button className="row" onClick={() => setVisibleFindings((current) => current + FINDINGS_PAGE_SIZE)}>
          <span className="row-main">{t("Show more ({count} left)", { count: count(shown.length - visibleFindings) })}</span>
        </button>}
        {disk.findings.length === 0 && <p className="empty">{t("Current probes cover FAT, exFAT boot and directory structures, and NTFS boot metadata. Other formats remain unknown, not empty.")}</p>}
      </div>
    </nav>
    <div className="center-stack">
      <div className="center-tabs">
        <div className="segmented" role="tablist">
          <button role="tab" aria-selected={center === "overview"} onClick={() => onCenter("overview")}>{t("Overview")}</button>
          <button role="tab" aria-selected="false" disabled={!onReturn || busy} onClick={onReturn} title={t("A hypothesis, not a disk modification. Opening requires surviving boot metadata; directory fragments alone do not establish file locations.")}>{t("Browse files")}</button>
          {!simple && <button role="tab" aria-selected={center === "bytes"} disabled={!valid || busy} onClick={() => onCenter("bytes")}>{t("Bytes")}</button>}
        </div>
      </div>
      {center === "overview" && <section className="center-panel recovery" aria-label={t("Recovery overview")}>
        <div className="recovery-inner">
          <div className="recovery-headline">{bytes(disk.length)}</div>
          <p className="recovery-sub">{t("Raw disk investigation")}</p>
          <p className="recovery-note">{t("Read-only bytes. Partition boundaries and filesystem evidence are separate, overlapping layers. Unrecognised does not mean empty.")}</p>
          <table className="recovery-table source-summary"><tbody>
            <tr><td>{t("Declared partitions")}</td><td className="numeric">{count(disk.partitions.length)}</td></tr>
            <tr><td>{t("Filesystem evidence")}</td><td className="numeric">{count(disk.findings.length)}</td></tr>
            <tr><td>{t("Unreadable regions")}</td><td className="numeric">{count(disk.errors.length)}</td></tr>
          </tbody></table>
          <div className="detail-actions">
            <button className="primary" disabled={!valid || busy} onClick={() => void discover(false)}>{t("Search for filesystem evidence")}</button>
            {disk.nextOffset !== null && !complete && !running && <button disabled={opening} onClick={() => void discover(true)}>{t("Continue scan")}</button>}
            {running && <button onClick={() => { stop.current = true; }}>{t("Pause")}</button>}
          </div>
          {disk.nextOffset !== null && <p className="recovery-note" role="status">{t("Examined through byte {offset}. No findings is not proof of empty space.", { offset: disk.nextOffset })}</p>}
          {disk.scanId && <p className="recovery-note" title={disk.scanId}>{t("Scan {id}. Each completed batch is saved on this Mac.", { id: disk.scanId })}</p>}
          {opened && opened.fingerprint.samples.length === 0 && <p className="warn">{t("No part of this source could be used to recognise it. Progress can be saved, but a saved scan cannot be matched to this source after reopening.")}</p>}
          {opened && opened.fingerprint.samples.length > 0 && opened.fingerprint.samples.length < opened.fingerprint.markersWanted && <p className="recovery-note">{t("Only {count} of {wanted} markers could be used, because the source is small, mostly blank or partly unreadable.", { count: opened.fingerprint.samples.length, wanted: opened.fingerprint.markersWanted })}</p>}
          {disk.changedSource && <p className="warn" role="status">{t("This scan was resumed on a source that had changed, so findings near the changed markers may be out of date.")} {fromEngine(disk.changedSource.summary)}</p>}
          {!simple && <p className="recovery-note">{t("Drag across the source row to select a region, or enter exact byte offsets below.")}</p>}
          <p className="warn">{t("This app only reads. macOS or other programs may still write to a mounted source.")}</p>
          {disk.warnings.map((warning, index) => <p className="warn" key={index}>{fromEngine(warning)}</p>)}
          {disk.errors.length > 0 && <div className="recovery-tests"><h3>{t("Unreadable regions")}</h3>
            {disk.errors.slice(0, visibleErrors).map((error, index) => <p className="warn" key={index}>{error.offset} + {error.length}: {fromEngine(error.error)}</p>)}
            {visibleErrors < disk.errors.length && <button onClick={() => setVisibleErrors((shown) => shown + FINDINGS_PAGE_SIZE)}>{t("Show more ({count} left)", { count: count(disk.errors.length - visibleErrors) })}</button>}
          </div>}
        </div>
      </section>}
      {center === "bytes" && <section className="center-panel" aria-label={t("Raw bytes")}>
        <div className="hex-bar"><div className="hex-title"><strong>{t("Raw bytes")}</strong><span className="mono">{first.toString(16).padStart(10, "0")}</span></div></div>
        <div className="hex-scroll" aria-label={t("Raw bytes")}>
          {hex === null && !failure && <p className="empty">{t("Reading from the disk.")}</p>}
          <div className="hex-sheet" style={{ height: hexRows.length * 20 }}>
            {hexRows.map((row, index) => <div className="hex-row" style={{ top: index * 20 }} key={index}>
              <span className="hex-offset">{(first + index * 16).toString(16).padStart(10, "0")}</span>
              <span className="hex-bytes">{row.match(/../g)?.map((value, column) => <span className={`byte${value === "00" ? " byte-zero" : ""}${column === 8 ? " byte-gap" : ""}`} key={column}>{value}</span>)}</span>
              <span className="hex-text">{row.match(/../g)?.map((value) => { const code = Number.parseInt(value, 16); return code >= 32 && code < 127 ? String.fromCharCode(code) : "."; }).join("")}</span>
            </div>)}
          </div>
        </div>
      </section>}
    </div>
    <div className="side">
      {rightHandle}
      <section className="details" aria-label={t("Details")}>
        <div className="inspector-tabs" role="tablist">
          <button role="tab" aria-selected={inspector === "selected"} aria-label={t("Selected region")} title={t("Selected region")} onClick={() => setInspector("selected")}><Icon name="info" size={16} /></button>
          <button role="tab" aria-selected={inspector === "volume"} aria-label={t("Source bytes")} title={t("Source bytes")} onClick={() => setInspector("volume")}><Icon name="disk" size={16} /></button>
        </div>
        <div className="inspector-title">{inspector === "selected" ? t("Selected region") : t("Source bytes")}</div>
        <div className="panel-body">
          {inspector === "selected" ? <>
            <div className="detail inspected" aria-label={t("Selected item")}>
              {selectedEvidence ? <>
                <div className="detail-name">{selectedEvidence.format} · {fromEngine(selectedEvidence.structure)}</div>
                <dl className="facts">
                  <dt>{t("Format")}</dt><dd>{selectedEvidence.format}</dd>
                  <dt>{t("Byte range")}</dt><dd>{byteRange(selectedEvidence.offset, selectedEvidence.length)}</dd>
                  <dt>{t("Confidence")}</dt><dd>{fromEngine(selectedEvidence.confidence)}</dd>
                  <dt>{t("Status")}</dt><dd className="status" data-status={evidenceStatus(selectedEvidence)}>{statusLabel(evidenceStatus(selectedEvidence))}</dd>
                  {selectedEvidence.volumeStart !== null && selectedEvidence.volumeLength !== null && <><dt>{t("Possible volume")}</dt><dd>{byteRange(selectedEvidence.volumeStart, selectedEvidence.volumeLength)}</dd></>}
                </dl>
                <p className="muted">{statusExplanation(evidenceStatus(selectedEvidence))}</p>
              </> : selectedPartition ? <>
                <div className="detail-name">{selectedPartition.label || selectedPartition.table}</div>
                <dl className="facts">
                  <dt>{t("Format")}</dt><dd>{t("Declared partition")}</dd>
                  <dt>{t("Byte range")}</dt><dd>{byteRange(selectedPartition.start, selectedPartition.length)}</dd>
                  <dt>{t("Confidence")}</dt><dd>{t("Declared by the {table} partition table", { table: selectedPartition.table })}</dd>
                  <dt>{t("Status")}</dt><dd className="status" data-status="recognised">{statusLabel("recognised")}</dd>
                </dl>
                <p className="muted">{t("A partition the table declares. Its filesystem has not been checked.")}</p>
                <div className="detail-actions"><button className="primary" disabled={busy} onClick={() => void openPartition(Number(selected.slice("partition:".length)))}>{t("Open")}</button></div>
              </> : <>
                <div className="detail-name">{t("Selected region")}</div>
                <dl className="facts"><dt>{t("Byte range")}</dt><dd>{valid ? byteRange(first, size) : "—"}</dd></dl>
                <p className="muted">{t("Pick a partition band or a finding on the map, or in the list, to see what it is.")}</p>
              </>}
            </div>
            <div className="detail">
              <div className="detail-name">{valid ? bytes(size) : t("Selected region")}</div>
              <div className="detail-path">{disk.path}</div>
              <label className="field">{t("Start byte")}<input value={start} inputMode="numeric" disabled={busy} onChange={(event) => { setStart(event.target.value); setBootOffset(event.target.value); setSelected("region"); }} /></label>
              <label className="field">{t("Length in bytes")}<input value={length} inputMode="numeric" disabled={busy} onChange={(event) => { setLength(event.target.value); setSelected("region"); }} /></label>
              {!simple && <div className="detail-actions"><button disabled={!valid || busy} onClick={() => onCenter("bytes")}>{t("Show the bytes")}</button></div>}
            </div>
            <div className="detail">
              <h4>{t("Open this range as a filesystem")}</h4>
              <p className="muted">{t("A hypothesis, not a disk modification. Opening requires surviving boot metadata; directory fragments alone do not establish file locations.")}</p>
              <label className="field">{t("Format")}<select value={format} disabled={busy} onChange={(event) => setFormat(event.target.value)}>{SOURCE_FORMATS.map((name) => <option key={name}>{name}</option>)}</select></label>
              <label className="field">{t("Label")}<input value={label} disabled={busy} onChange={(event) => setLabel(event.target.value)} maxLength={128} /></label>
              <label className="field">{t("Boot evidence byte")}<input value={bootOffset} inputMode="numeric" disabled={busy} onChange={(event) => setBootOffset(event.target.value)} /></label>
              <div className="detail-actions"><button className="primary" disabled={!valid || busy} onClick={() => void openInterpretation()}>{opening ? t("Opening") : t("Open")}</button></div>
              {format === "NTFS" && <p className="muted">{t("If the main NTFS boot sector was overwritten, give the byte of the copy in the volume's last sector as the boot evidence.")}</p>}
            </div>
            <RangeAnalysis diskId={disk.id} start={first} length={size} valid={valid} busy={busy} onSelect={(offset, count) => selectRange(offset, Math.min(count, disk.length - offset))} onSaved={() => setHypothesesRevision((revision) => revision + 1)} />
            <Hypotheses diskId={disk.id} sourceLength={disk.length} selectionStart={first} selectionLength={size} selectionValid={valid} busy={busy} refresh={hypothesesRevision}
              onSelect={(offset, count) => selectRange(offset, Math.min(count, disk.length - offset))} onOpen={onOpen} />
          </> : <div className="detail">
            <div className="detail-name">{t("Source bytes")}</div><div className="detail-path">{disk.path}</div>
            <dl className="facts"><dt>{t("Length in bytes")}</dt><dd>{count(disk.length)}</dd><dt>{t("Declared partitions")}</dt><dd>{count(disk.partitions.length)}</dd></dl>
            <p className="muted">{t("Current probes cover FAT, exFAT boot and directory structures, and NTFS boot metadata. Other formats remain unknown, not empty.")}</p>
          </div>}
          {failure && <p role="alert" className="warn">{fromEngine(failure)}</p>}
        </div>
      </section>
    </div>
  </>;
}

function byteRange(offset: number, length: number): string {
  return `${count(offset)}–${count(offset + length)} (${bytes(length)})`;
}

function statusLabel(status: EvidenceStatus): string {
  return status === "decodable" ? t("Decodable") : status === "needs-metadata" ? t("Needs related metadata") : t("Recognised");
}

function statusExplanation(status: EvidenceStatus): string {
  return status === "decodable" ? t("The metadata needed to try opening it as a filesystem is present.")
    : status === "needs-metadata" ? t("Only a fragment. Its volume's boot or related metadata is needed before its files can be placed.")
    : t("The structure is recognised, but this app cannot decode it.");
}

function evidenceKey(evidence: DiskEvidence): string {
  return `evidence:${evidence.offset}:${evidence.format}:${evidence.structure}`;
}
