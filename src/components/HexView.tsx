import { useEffect, useMemo, useRef, useState } from "react";
import { readRaw, type ClusterInfo, type EntrySet } from "../api";
import { count, hexOffset, stamp } from "../format";
import { Icon } from "./icons";

const BYTES_PER_ROW = 16;
const ROW_HEIGHT = 19;
const OVERSCAN_ROWS = 12;
const ENTRY_BYTES = 32;

const MARK_NONE = 0;
const MARK_SET_ODD = 1;
const MARK_SET_EVEN = 2;
const MARK_DAMAGED = 3;
const MARK_STRUCT = 4;

function exfatTime(value: number): string | null {
  const month = (value >>> 21) & 15;
  const day = (value >>> 16) & 31;
  const hour = (value >>> 11) & 31;
  const minute = (value >>> 5) & 63;
  const second = (value & 31) * 2;
  if (month < 1 || month > 12 || day < 1 || hour > 23 || minute > 59 || second > 59) return null;
  const pad = (number: number) => String(number).padStart(2, "0");
  return `${1980 + (value >>> 25)}-${pad(month)}-${pad(day)} ${pad(hour)}:${pad(minute)}:${pad(second)}`;
}

interface Props {
  cluster: number | null;
  info: ClusterInfo | null;
  infoError: string | null;
  clusterSize: number;
  heapOffset: number;
  lastCluster: number;
  byteRange: [number, number] | null;
  onByteRange: (range: [number, number] | null) => void;
  onGo: (cluster: number) => void;
  onClose: () => void;
}

export function HexView({ cluster, info, infoError, clusterSize, heapOffset, lastCluster, byteRange, onByteRange, onGo, onClose }: Props) {
  const [data, setData] = useState<Uint8Array | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const [viewportHeight, setViewportHeight] = useState(400);
  const [jump, setJump] = useState("");
  const scroller = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (cluster === null) return;
    let current = true;
    setError(null);
    readRaw(heapOffset + (cluster - 2) * clusterSize, clusterSize)
      .then((bytes) => {
        if (current) setData(bytes);
      })
      .catch((reason) => {
        if (current) {
          setData(null);
          setError(String(reason));
        }
      });
    if (scroller.current) scroller.current.scrollTop = 0;
    return () => {
      current = false;
    };
  }, [cluster, clusterSize, heapOffset]);

  useEffect(() => {
    const element = scroller.current;
    if (!element) return;
    const observer = new ResizeObserver(([entry]) => setViewportHeight(entry.contentRect.height));
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  const marks = useMemo(() => {
    const result = new Uint8Array(clusterSize);
    if (!info || info.cluster !== cluster) return result;
    info.sets.forEach((set, index) => {
      const end = Math.min(set.offset + set.slots * ENTRY_BYTES, clusterSize);
      const mark = !set.checksumOk ? MARK_DAMAGED : index % 2 === 0 ? MARK_SET_ODD : MARK_SET_EVEN;
      result.fill(mark, set.offset, end);
    });
    const base = info.offset;
    for (const structure of info.structures) {
      for (const field of structure.fields) {
        const start = field.offset - base;
        if (start >= 0 && start < clusterSize) result.fill(MARK_STRUCT, start, Math.min(start + field.length, clusterSize));
      }
    }
    return result;
  }, [info, cluster, clusterSize]);

  const setAtSelection: EntrySet | undefined = useMemo(() => {
    if (!info || !byteRange) return undefined;
    return info.sets.find((set) => byteRange[0] >= set.offset && byteRange[0] < set.offset + set.slots * ENTRY_BYTES);
  }, [info, byteRange]);

  function selectByte(offset: number, extend: boolean) {
    if (extend && byteRange) {
      onByteRange([Math.min(byteRange[0], offset), Math.max(byteRange[0], offset) + 1]);
    } else {
      onByteRange([offset, offset + 1]);
    }
  }

  function submitJump(event: React.FormEvent) {
    event.preventDefault();
    const text = jump.trim();
    if (!text) return;
    if (/^0x/i.test(text)) {
      const offset = parseInt(text, 16);
      if (Number.isFinite(offset) && offset >= heapOffset) {
        const target = Math.floor((offset - heapOffset) / clusterSize) + 2;
        onGo(target);
        onByteRange([(offset - heapOffset) % clusterSize, ((offset - heapOffset) % clusterSize) + 1]);
      }
    } else {
      const target = Number(text.replace(/[,\s]/g, ""));
      if (Number.isInteger(target)) onGo(target);
    }
    setJump("");
  }

  if (cluster === null) return null;

  const base = heapOffset + (cluster - 2) * clusterSize;
  const rows = Math.ceil((data?.length ?? 0) / BYTES_PER_ROW);
  const firstRow = Math.max(Math.floor(scrollTop / ROW_HEIGHT) - OVERSCAN_ROWS, 0);
  const lastRow = Math.min(Math.ceil((scrollTop + viewportHeight) / ROW_HEIGHT) + OVERSCAN_ROWS, rows);
  const visible = [];
  for (let row = firstRow; row < lastRow && data; row++) {
    const start = row * BYTES_PER_ROW;
    const hexCells = [];
    const textCells = [];
    for (let column = 0; column < BYTES_PER_ROW; column++) {
      const offset = start + column;
      const value = data[offset];
      const selected = byteRange !== null && offset >= byteRange[0] && offset < byteRange[1];
      const mark = marks[offset];
      const className = `byte${selected ? " byte-selected" : ""}${mark !== MARK_NONE ? ` mark-${mark}` : ""}${value === 0 ? " byte-zero" : ""}`;
      const pick = (event: React.MouseEvent) => selectByte(offset, event.shiftKey);
      hexCells.push(
        <span key={column} className={`${className}${column === 8 ? " byte-gap" : ""}`} onClick={pick}>
          {value.toString(16).padStart(2, "0")}
        </span>,
      );
      textCells.push(
        <span key={column} className={className} onClick={pick}>
          {value >= 32 && value < 127 ? String.fromCharCode(value) : "."}
        </span>,
      );
    }
    visible.push(
      <div key={row} className="hex-row" style={{ top: row * ROW_HEIGHT }}>
        <span className="hex-offset">{(base + start).toString(16).padStart(10, "0")}</span>
        <span className="hex-bytes">{hexCells}</span>
        <span className="hex-text">{textCells}</span>
      </div>,
    );
  }

  const nextByFat = info && info.fatValue >= 2 && info.fatValue < lastCluster ? info.fatValue : null;
  const view = data && byteRange ? new DataView(data.buffer, data.byteOffset, data.byteLength) : null;
  const at = byteRange?.[0] ?? 0;
  const remaining = data ? data.length - at : 0;
  const u32 = view && remaining >= 4 ? view.getUint32(at, true) : null;

  return (
    <section className="center-panel" aria-label="Bytes">
      <div className="hex-bar">
        <div className="hex-title">
          <strong>Cluster {count(cluster)}</strong>
          <span className="mono muted">{hexOffset(base)}</span>
        </div>
        <div className="hex-nav">
          <button className="icon-button" disabled={cluster <= 2} onClick={() => onGo(cluster - 1)} title="The cluster just before this one on disk" aria-label="Previous cluster"><Icon name="chevronLeft" size={15} /></button>
          <button className="icon-button" disabled={cluster + 1 >= lastCluster} onClick={() => onGo(cluster + 1)} title="The cluster just after this one on disk" aria-label="Next cluster"><Icon name="chevronRight" size={15} /></button>
          <button disabled={nextByFat === null} onClick={() => nextByFat !== null && onGo(nextByFat)} title={info?.fatMeaning}>Follow FAT</button>
          <button disabled={!info || info.chainHead === cluster} onClick={() => info && onGo(info.chainHead)} title="Go to the start of this chain">Chain start</button>
          <form onSubmit={submitJump}>
            <input value={jump} onChange={(event) => setJump(event.target.value)} placeholder="Cluster, or 0x offset" aria-label="Go to cluster or offset" />
          </form>
          <button className="icon-button" onClick={onClose} title="Back to the recovery overview" aria-label="Back to the recovery overview"><Icon name="grid" size={15} /></button>
        </div>
      </div>
      <div ref={scroller} className="hex-scroll" onScroll={(event) => setScrollTop(event.currentTarget.scrollTop)} tabIndex={0} aria-label="Hex dump">
        {error && <div className="empty">{error}</div>}
        <div className="hex-sheet" style={{ height: rows * ROW_HEIGHT }}>{visible}</div>
      </div>
      <div className="decode">
        {infoError && <div className="decode-line warn">{infoError}</div>}
        {setAtSelection && (
          <div className="decode-line">
            <span>{setAtSelection.checksumOk ? "Entry" : "Damaged entry"}{setAtSelection.inUse ? "" : ", marked deleted"}:</span>{" "}
            <strong>{setAtSelection.name}</strong>
            {setAtSelection.firstCluster >= 2 && setAtSelection.firstCluster < lastCluster && (
              <button className="link" onClick={() => onGo(setAtSelection.firstCluster)}>first cluster {count(setAtSelection.firstCluster)}</button>
            )}
            <span className="muted"> modified {stamp(setAtSelection.modified)}</span>
          </div>
        )}
        {view && byteRange ? (
          <div className="decode-grid mono">
            <span>u8 {view.getUint8(at)}</span>
            {remaining >= 2 && <span>u16 {view.getUint16(at, true)}</span>}
            {u32 !== null && <span>u32 {u32}</span>}
            {remaining >= 8 && <span>u64 {view.getBigUint64(at, true).toString()}</span>}
            {u32 !== null && exfatTime(u32) && <span>exFAT time {exfatTime(u32)}</span>}
            {u32 !== null && u32 >= 2 && u32 < lastCluster && <button className="link" onClick={() => onGo(u32)}>go to cluster {count(u32)}</button>}
            {byteRange[1] - byteRange[0] > 1 && <span>{byteRange[1] - byteRange[0]} bytes selected</span>}
          </div>
        ) : (
          <div className="decode-line muted">Click a byte to decode it. Shift-click to select a range to ask about.</div>
        )}
      </div>
    </section>
  );
}
