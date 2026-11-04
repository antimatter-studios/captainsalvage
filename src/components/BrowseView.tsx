import { useCallback, useEffect, useMemo, useRef, useState, type CSSProperties } from "react";
import {
  call,
  convertedUrl,
  entryUrl,
  mediaInfo,
  readEntryRange,
  thumbUrl,
  type BrowseGroup,
  type BrowsePage,
  type BrowseRow,
  type BrowseSort,
  type MediaInfo,
  type TypeTotal,
} from "../api";
import { bytes, count, percent, stamp, VERDICT_LABEL } from "../format";
import { ConfidenceRing, fileIcon, Icon, verdictTone } from "./icons";

const PAGE_ROWS = 120;
const TILE_WIDTH = 150;
const TILE_GAP = 10;
const PHOTO_TILE_HEIGHT = 190;
const FILE_TILE_HEIGHT = 124;
const OVERSCAN_ROWS = 2;
const THUMB_RETRY_MILLIS = 2000;
const SEARCH_WAIT_MILLIS = 300;
const TEXT_PREVIEW_BYTES = 256 * 1024;
const HEX_PREVIEW_BYTES = 4096;
const TEXT_SAMPLE_BYTES = 8192;
const MIN_PRINTABLE_SHARE = 0.9;
const HEX_COLUMNS = 16;

const NO_THUMBNAIL = new Set(["heic", "heif"]);
const IMAGE_EXTENSIONS = new Set(["jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff", "heic", "heif"]);
/** The window plays these itself. Anything inside them it can't play falls back to ffmpeg. */
const NATIVE_VIDEO = new Set(["mp4", "m4v", "mov", "webm", "3gp"]);
const NATIVE_AUDIO = new Set(["mp3", "wav", "flac", "m4a", "aac", "aif", "aiff"]);
/** Only ffmpeg can play these. */
const CONVERTED_VIDEO = new Set(["avi", "mkv", "wmv", "mpg", "mpeg", "flv", "asf", "vob", "m2ts", "divx", "rm", "rmvb", "ogv"]);
const CONVERTED_AUDIO = new Set(["wma", "ogg", "opus"]);
const INSTALL_COMMAND = "brew install ffmpeg";

export interface BrowseScope {
  head: number;
  name: string;
}

function extensionOf(name: string): string {
  return name.includes(".") ? (name.split(".").pop() ?? "").toLowerCase() : "";
}

function Tile({ row, showImage, selected, inBasket, style, onOpen, onBasket }: {
  row: BrowseRow;
  showImage: boolean;
  selected: boolean;
  inBasket: boolean;
  style: CSSProperties;
  onOpen: () => void;
  onBasket: (add: boolean) => void;
}) {
  const [failures, setFailures] = useState(0);
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(timer.current), []);
  const failed = failures >= 2;
  const verdict = VERDICT_LABEL[row.verdict] ?? row.verdict;

  return (
    <div className="tile-wrap" style={style}>
      <button className={selected ? "tile tile-selected" : "tile"} onClick={onOpen} title={row.name}>
        {showImage && (
          <span className="tile-image">
            {!failed ? (
              <img
                src={thumbUrl(row.id, failures)}
                alt=""
                draggable={false}
                onError={() => {
                  // The first failure can just mean the queue was full, so try once more.
                  timer.current = window.setTimeout(() => setFailures((value) => value + 1), failures === 0 ? THUMB_RETRY_MILLIS : 0);
                }}
              />
            ) : NO_THUMBNAIL.has(extensionOf(row.name)) ? (
              <span className="tile-note"><Icon name="image" size={28} />Open to view</span>
            ) : (
              <span className="tile-note tone-serious"><Icon name="broken" size={28} />Would not decode</span>
            )}
          </span>
        )}
        {!showImage && <span className="tile-icon"><Icon name={fileIcon(row.name)} size={32} /></span>}
        <span className="tile-name">{row.name}</span>
        <span className="tile-meta">
          <span>{bytes(row.length)}</span>
          <ConfidenceRing value={row.confidence} tone={verdictTone(row.verdict)} label={`${verdict}, confidence ${percent(row.confidence)}`} />
        </span>
      </button>
      <input
        type="checkbox"
        className="basket-check tile-check"
        checked={inBasket}
        aria-label={`Add ${row.name} to the basket`}
        title="Add to the basket"
        onChange={(event) => onBasket(event.target.checked)}
      />
    </div>
  );
}

function hexDump(data: Uint8Array): string {
  const lines: string[] = [];
  for (let offset = 0; offset < data.length; offset += HEX_COLUMNS) {
    const part = Array.from(data.subarray(offset, offset + HEX_COLUMNS));
    const hex = part.map((value) => value.toString(16).padStart(2, "0")).join(" ").padEnd(HEX_COLUMNS * 3 - 1);
    const text = part.map((value) => (value >= 32 && value < 127 ? String.fromCharCode(value) : ".")).join("");
    lines.push(`${offset.toString(16).padStart(8, "0")}  ${hex}  ${text}`);
  }
  return lines.join("\n");
}

function looksLikeText(data: Uint8Array): boolean {
  const sample = data.subarray(0, TEXT_SAMPLE_BYTES);
  if (sample.length === 0) return false;
  let printable = 0;
  for (const value of sample) {
    if (value === 0) return false;
    if (value >= 32 || value === 9 || value === 10 || value === 13) printable += 1;
  }
  return printable / sample.length >= MIN_PRINTABLE_SHARE;
}

/** Text, or the first bytes when a file has no other way to show itself. */
function RawPreview({ row, note }: { row: BrowseRow; note?: string }) {
  const [shown, setShown] = useState<{ text: string; isText: boolean; total: number } | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let current = true;
    setShown(null);
    setError(null);
    readEntryRange(row.id, 0, TEXT_PREVIEW_BYTES)
      .then((data) => {
        if (!current) return;
        const isText = looksLikeText(data);
        setShown({
          isText,
          total: data.length,
          text: isText ? new TextDecoder("utf-8", { fatal: false }).decode(data) : hexDump(data.subarray(0, HEX_PREVIEW_BYTES)),
        });
      })
      .catch((reason) => current && setError(String(reason)));
    return () => {
      current = false;
    };
  }, [row.id]);
  return (
    <div className="preview-raw">
      {note && <p className="preview-message">{note}</p>}
      {error && <p className="warn">{error}</p>}
      {!shown && !error && <p className="muted">Reading from the disk.</p>}
      {shown && !shown.isText && <p className="muted">First {bytes(Math.min(shown.total, HEX_PREVIEW_BYTES))} of this file, as bytes.</p>}
      {shown && <pre className={shown.isText ? "preview-sheet" : "preview-sheet preview-hex"}>{shown.text}</pre>}
    </div>
  );
}

/** Plays a file directly if the window can, and through ffmpeg if it can't. */
function Player({ row, audio, native, media }: { row: BrowseRow; audio: boolean; native: boolean; media: MediaInfo | null }) {
  const [converting, setConverting] = useState(!native);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    setConverting(!native);
    setFailed(false);
  }, [row.id, native]);
  const extension = extensionOf(row.name);

  if (failed) {
    return <RawPreview row={row} note="This file would not play, so its clusters may be wrong or damaged. Its first bytes are below." />;
  }
  if (converting && !media) {
    return <p className="preview-message preview-wait muted">Checking for ffmpeg.</p>;
  }
  if (converting && !media?.available) {
    return <RawPreview row={row} note={`The window can't play this ${extension ? `.${extension} ` : ""}file by itself. To play it here, install ffmpeg with ${INSTALL_COMMAND}, then open this file again. Everything else works without it.`} />;
  }
  const source = converting && media ? convertedUrl(media, row.id) : entryUrl(row.id);
  const onError = () => {
    if (converting) setFailed(true);
    else setConverting(true);
  };
  return (
    <div className={audio ? "preview-stage preview-audio" : "preview-stage"}>
      {audio && <Icon name="music" size={48} />}
      {audio
        ? <audio key={source} controls preload="metadata" src={source} onError={onError} />
        : <video key={source} controls preload="metadata" src={source} onError={onError} />}
      {converting && <p className="preview-converting muted">Converted by ffmpeg as it plays. Nothing is saved.</p>}
    </div>
  );
}

function PreviewBody({ row }: { row: BrowseRow }) {
  const [failed, setFailed] = useState(false);
  const [loaded, setLoaded] = useState(false);
  const [media, setMedia] = useState<MediaInfo | null>(null);
  useEffect(() => {
    setFailed(false);
    setLoaded(false);
    // Asked each time, so ffmpeg installed a minute ago is found.
    mediaInfo().then(setMedia).catch(() => setMedia(null));
  }, [row.id]);
  const extension = extensionOf(row.name);

  if (NATIVE_VIDEO.has(extension) || CONVERTED_VIDEO.has(extension)) {
    return <Player row={row} audio={false} native={NATIVE_VIDEO.has(extension)} media={media} />;
  }
  if (NATIVE_AUDIO.has(extension) || CONVERTED_AUDIO.has(extension)) {
    return <Player row={row} audio native={NATIVE_AUDIO.has(extension)} media={media} />;
  }
  if (failed) {
    return <RawPreview row={row} note="This file would not open here, so its clusters may be wrong or damaged. Its first bytes are below." />;
  }
  if (IMAGE_EXTENSIONS.has(extension)) {
    return (
      <div className="preview-stage">
        {!loaded && <p className="preview-loading muted">Reading from the disk.</p>}
        <img key={row.id} src={entryUrl(row.id)} alt={row.name} onLoad={() => setLoaded(true)} onError={() => setFailed(true)} />
      </div>
    );
  }
  if (extension === "pdf") {
    return <iframe key={row.id} className="preview-frame" src={entryUrl(row.id)} title={row.name} />;
  }
  return <RawPreview row={row} />;
}

function PreviewPane({ row, position, total, inBasket, onClose, onMove, onBasket, onBytes }: {
  row: BrowseRow;
  position: number | null;
  total: number;
  inBasket: boolean;
  onClose: () => void;
  onMove: (step: number) => void;
  onBasket: (add: boolean) => void;
  onBytes: () => void;
}) {
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      if (target && ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName)) return;
      if (event.key === "Escape") onClose();
      if (event.key === "ArrowLeft") onMove(-1);
      if (event.key === "ArrowRight") onMove(1);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose, onMove]);
  const verdict = VERDICT_LABEL[row.verdict] ?? row.verdict;

  return (
    <div className="preview-pane">
      <div className="preview-bar">
        <button className="icon-button" onClick={onClose} aria-label="Back to the files"><Icon name="chevronLeft" /></button>
        <div className="preview-title">
          <strong>{row.name}</strong>
          <span className="muted">{bytes(row.length)}, {stamp(row.modified)}</span>
        </div>
        <span className={`verdict-chip ${verdictTone(row.verdict)}`}>{verdict}</span>
        {position !== null && (
          <span className="preview-nav">
            <button className="icon-button" disabled={position === 0} onClick={() => onMove(-1)} aria-label="Previous file"><Icon name="chevronLeft" /></button>
            <span className="muted numeric">{count(position + 1)} of {count(total)}</span>
            <button className="icon-button" disabled={position + 1 >= total} onClick={() => onMove(1)} aria-label="Next file"><Icon name="chevronRight" /></button>
          </span>
        )}
        <button onClick={onBytes}>Show bytes</button>
        <button className="primary" onClick={() => onBasket(!inBasket)}>{inBasket ? "In your basket" : "Add to basket"}</button>
      </div>
      <PreviewBody row={row} />
      <div className="preview-foot muted">Read live from the disk. Nothing is copied.</div>
    </div>
  );
}

export function BrowseView({ floor, revision, scope, onScopeClear, standalone, onStandaloneClose, basket, onBasket, onSelect, onBytes }: {
  floor: number;
  revision: number;
  scope: BrowseScope | null;
  onScopeClear: () => void;
  standalone: BrowseRow | null;
  onStandaloneClose: () => void;
  basket: Set<string>;
  onBasket: (kind: "entry" | "folder", target: number, add: boolean) => void;
  onSelect: (row: BrowseRow) => void;
  onBytes: (cluster: number) => void;
}) {
  const [types, setTypes] = useState<TypeTotal[] | null>(null);
  const [group, setGroup] = useState<BrowseGroup>("photos");
  const [sort, setSort] = useState<BrowseSort>("best");
  const [typed, setTyped] = useState("");
  const [text, setText] = useState("");
  const [total, setTotal] = useState<number | null>(null);
  const [pages, setPages] = useState<Map<number, BrowseRow[]>>(new Map());
  const [error, setError] = useState<string | null>(null);
  const [openIndex, setOpenIndex] = useState<number | null>(null);
  const [scroll, setScroll] = useState({ top: 0, width: 0, height: 0 });
  const [hasFfmpeg, setHasFfmpeg] = useState(false);
  const scroller = useRef<HTMLDivElement | null>(null);
  const requested = useRef<Set<number>>(new Set());
  const folder = scope?.head;

  useEffect(() => {
    mediaInfo().then((media) => setHasFfmpeg(Boolean(media?.available))).catch(() => undefined);
  }, [revision, group]);

  useEffect(() => {
    const timer = window.setTimeout(() => setText(typed), SEARCH_WAIT_MILLIS);
    return () => window.clearTimeout(timer);
  }, [typed]);

  useEffect(() => {
    let current = true;
    call<TypeTotal[]>("browse_types", { folder, minConfidence: floor })
      .then((value) => current && setTypes(value))
      .catch((reason) => current && setError(String(reason)));
    return () => {
      current = false;
    };
  }, [folder, floor, revision]);

  // A folder with no photos should open on what it does have.
  useEffect(() => {
    if (!types) return;
    if ((types.find((item) => item.group === group)?.files ?? 0) === 0) {
      const first = types.find((item) => item.files > 0);
      if (first) setGroup(first.group);
    }
  }, [types, group]);

  const fetchPage = useCallback((page: number, reset: boolean) => {
    if (!reset && requested.current.has(page)) return;
    requested.current.add(page);
    call<BrowsePage>("browse_files", { group, folder, minConfidence: floor, sort, text, offset: page * PAGE_ROWS, limit: PAGE_ROWS })
      .then((result) => {
        setTotal(result.total);
        setPages((current) => new Map(reset ? [] : current).set(page, result.rows));
      })
      .catch((reason) => setError(String(reason)));
  }, [group, folder, floor, sort, text]);

  useEffect(() => {
    requested.current = new Set();
    setOpenIndex(null);
    setError(null);
    setTotal(null);
    setPages(new Map());
    scroller.current?.scrollTo({ top: 0 });
    fetchPage(0, true);
  }, [fetchPage, revision]);

  useEffect(() => {
    const element = scroller.current;
    if (!element) return;
    const measure = () => setScroll({ top: element.scrollTop, width: element.clientWidth, height: element.clientHeight });
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  const showImages = group === "photos" || (group === "videos" && hasFfmpeg);
  const tileHeight = showImages ? PHOTO_TILE_HEIGHT : FILE_TILE_HEIGHT;
  const columns = Math.max(1, Math.floor((scroll.width - TILE_GAP) / (TILE_WIDTH + TILE_GAP)));
  const tileWidth = Math.floor((scroll.width - TILE_GAP * (columns + 1)) / columns);
  const rowCount = Math.ceil((total ?? 0) / columns);
  const firstRow = Math.max(0, Math.floor(scroll.top / (tileHeight + TILE_GAP)) - OVERSCAN_ROWS);
  const lastRow = Math.min(rowCount - 1, Math.ceil((scroll.top + scroll.height) / (tileHeight + TILE_GAP)) + OVERSCAN_ROWS);

  const rowAt = useCallback((index: number) => pages.get(Math.floor(index / PAGE_ROWS))?.[index % PAGE_ROWS], [pages]);

  useEffect(() => {
    if (total === null) return;
    const first = firstRow * columns;
    const last = Math.min(total - 1, (lastRow + 1) * columns - 1);
    for (let page = Math.floor(first / PAGE_ROWS); page <= Math.floor(last / PAGE_ROWS); page += 1) {
      if (page >= 0 && !pages.has(page)) fetchPage(page, false);
    }
  }, [firstRow, lastRow, columns, total, pages, fetchPage]);

  const open = useCallback((index: number) => {
    setOpenIndex(index);
    const row = rowAt(index);
    if (row) onSelect(row);
    else fetchPage(Math.floor(index / PAGE_ROWS), false);
  }, [rowAt, onSelect, fetchPage]);

  const move = useCallback((step: number) => {
    setOpenIndex((current) => {
      if (current === null || total === null) return current;
      const next = Math.min(total - 1, Math.max(0, current + step));
      const row = rowAt(next);
      if (row) onSelect(row);
      else fetchPage(Math.floor(next / PAGE_ROWS), false);
      return next;
    });
  }, [total, rowAt, onSelect, fetchPage]);

  // Opening a file that was not yet loaded selects it as soon as it arrives.
  const openRow = openIndex === null ? undefined : rowAt(openIndex);

  const closeOpen = useCallback(() => {
    setOpenIndex((current) => {
      if (current !== null && scroller.current) {
        scroller.current.scrollTop = Math.floor(current / columns) * (tileHeight + TILE_GAP);
      }
      return null;
    });
  }, [columns, tileHeight]);

  const tiles = useMemo(() => {
    if (total === null || scroll.width === 0) return [];
    const out: React.ReactNode[] = [];
    for (let line = firstRow; line <= lastRow; line += 1) {
      for (let column = 0; column < columns; column += 1) {
        const index = line * columns + column;
        if (index >= total) break;
        const style: CSSProperties = {
          left: TILE_GAP + column * (tileWidth + TILE_GAP),
          top: TILE_GAP + line * (tileHeight + TILE_GAP),
          width: tileWidth,
          height: tileHeight,
        };
        const row = rowAt(index);
        out.push(row ? (
          <Tile
            key={row.id}
            row={row}
            showImage={showImages}
            selected={openIndex === index}
            inBasket={basket.has(`entry:${row.id}`)}
            style={style}
            onOpen={() => open(index)}
            onBasket={(add) => onBasket("entry", row.id, add)}
          />
        ) : <div key={`empty-${index}`} className="tile-wrap tile-empty" style={style} />);
      }
    }
    return out;
  }, [total, scroll.width, firstRow, lastRow, columns, tileWidth, tileHeight, rowAt, showImages, openIndex, basket, open, onBasket]);

  const label = types?.find((item) => item.group === group)?.label.toLowerCase() ?? "files";
  const previewRow = standalone ?? openRow ?? null;

  return (
    <section className="center-panel browse" aria-label="Browse files">
      <div className="browse-bar">
        <div className="segmented browse-types" role="tablist">
          {types?.map((item) => (
            <button key={item.group} role="tab" aria-selected={item.group === group} disabled={item.files === 0} onClick={() => setGroup(item.group)}>
              {item.label} <span className="browse-count">{count(item.files)}</span>
            </button>
          ))}
        </div>
        <div className="browse-tools">
          {scope && (
            <button className="browse-scope" onClick={onScopeClear} title="Show files from the whole disk">
              <Icon name="folder" size={13} /> <span>{scope.name}</span> <Icon name="close" size={11} />
            </button>
          )}
          <label className="sidebar-search browse-search">
            <Icon name="search" size={13} />
            <input value={typed} onChange={(event) => setTyped(event.target.value)} placeholder="Filter by name" spellCheck={false} aria-label="Filter by name" />
          </label>
          <select value={sort} onChange={(event) => setSort(event.target.value as BrowseSort)} aria-label="Sort">
            <option value="best">Best first</option>
            <option value="newest">Newest first</option>
            <option value="largest">Largest first</option>
          </select>
        </div>
      </div>
      {error && <p className="warn browse-error">{error}</p>}
      <div
        className="browse-scroll"
        ref={scroller}
        onScroll={(event) => {
          const element = event.currentTarget;
          setScroll({ top: element.scrollTop, width: element.clientWidth, height: element.clientHeight });
        }}
      >
        {total === 0 && <p className="empty">No {label} here{text ? ` with "${text}" in the name` : ""}. Try a lower confidence, or clear the filter.</p>}
        {total === null && !error && <p className="empty">Reading the list of files.</p>}
        <div className="browse-sheet" style={{ height: rowCount * (tileHeight + TILE_GAP) + TILE_GAP }}>{tiles}</div>
      </div>
      {previewRow && (
        <PreviewPane
          row={previewRow}
          position={standalone ? null : openIndex}
          total={total ?? 0}
          inBasket={basket.has(`entry:${previewRow.id}`)}
          onClose={standalone ? onStandaloneClose : closeOpen}
          onMove={standalone ? () => undefined : move}
          onBasket={(add) => onBasket("entry", previewRow.id, add)}
          onBytes={() => onBytes(previewRow.firstCluster)}
        />
      )}
    </section>
  );
}
