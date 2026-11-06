import { invoke } from "@tauri-apps/api/core";
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
import { bytes, count, percent, stamp, verdictLabel } from "../format";
import { fromEngine, t, useLanguage } from "../i18n";
import { ConfidenceRing, fileIcon, Icon, verdictTone } from "./icons";
import { isArchiveName } from "./ZipView";

const PAGE_ROWS = 120;
const TILE_WIDTH = 150;
const TILE_GAP = 10;
const PHOTO_TILE_HEIGHT = 190;
const FILE_TILE_HEIGHT = 124;
const OVERSCAN_ROWS = 2;
const LIST_ROW_HEIGHT = 30;
/** Where the window remembers the kind of file and the layout, across restarts. */
const BROWSE_SETTING = "browse";
type BrowseLayout = "icons" | "list";

/** One file as a line of the list layout. */
function ListRow({ row, selected, inBasket, style, onOpen, onBasket }: {
  row: BrowseRow;
  selected: boolean;
  inBasket: boolean;
  style: CSSProperties;
  onOpen: () => void;
  onBasket: (add: boolean) => void;
}) {
  const verdict = verdictLabel(row.verdict);
  return (
    <div className={selected ? "list-row list-row-selected" : "list-row"} style={style}>
      <input
        type="checkbox"
        className="basket-check"
        checked={inBasket}
        aria-label={t("Add {name} to the treasure list", { name: row.name })}
        title={t("Add to the treasure list")}
        onChange={(event) => onBasket(event.target.checked)}
      />
      <button className="list-row-main" onClick={onOpen} title={row.name}>
        <Icon name={fileIcon(row.name)} size={15} />
        <span className="list-row-name">{row.name}</span>
        {(row.copies ?? 1) > 1 && <span className="tile-copies" title={t("{count} identical files", { count: count(row.copies ?? 1) })}>×{count(row.copies ?? 1)}</span>}
        <span className="list-row-size">{bytes(row.length)}</span>
        <span className="list-row-date">{row.modified ? stamp(row.modified) : ""}</span>
        <span className="list-row-verdict">
          <ConfidenceRing value={row.confidence} tone={verdictTone(row.verdict)} label={t("{verdict}, confidence {percent}", { verdict, percent: percent(row.confidence) })} />
          {verdict}
        </span>
      </button>
    </div>
  );
}
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
const MAX_INLINE_IMAGE_BYTES = 64 * 1024 * 1024;
const INLINE_TEXT_BYTES = 16 * 1024;
const PDF_MAGIC = "%PDF-";

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
  const verdict = verdictLabel(row.verdict);

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
              <span className="tile-note"><Icon name="image" size={28} />{t("Open to view")}</span>
            ) : (
              <span className="tile-note tone-serious"><Icon name="broken" size={28} />{t("Would not decode")}</span>
            )}
          </span>
        )}
        {!showImage && <span className="tile-icon"><Icon name={fileIcon(row.name)} size={32} /></span>}
        <span className="tile-name">{row.name}</span>
        <span className="tile-meta">
          <span>
            {bytes(row.length)}
            {(row.copies ?? 1) > 1 && <span className="tile-copies" title={t("{count} identical files", { count: count(row.copies ?? 1) })}> ×{count(row.copies ?? 1)}</span>}
          </span>
          <ConfidenceRing value={row.confidence} tone={verdictTone(row.verdict)} label={t("{verdict}, confidence {percent}", { verdict, percent: percent(row.confidence) })} />
        </span>
      </button>
      <input
        type="checkbox"
        className="basket-check tile-check"
        checked={inBasket}
        aria-label={t("Add {name} to the treasure list", { name: row.name })}
        title={t("Add to the treasure list")}
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
      {error && <p className="warn">{fromEngine(error)}</p>}
      {!shown && !error && <p className="muted">{t("Reading from the disk.")}</p>}
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
    return <RawPreview row={row} note={t("This file would not play, so its clusters may be wrong or damaged. Its first bytes are below.")} />;
  }
  if (converting && !media) {
    return <p className="preview-message preview-wait muted">{t("Checking for ffmpeg.")}</p>;
  }
  if (converting && !media?.available) {
    return <RawPreview row={row} note={extension
      ? t("The window can't play this .{extension} file by itself. To play it here, install ffmpeg with {command}, then open this file again. Everything else works without it.", { extension, command: INSTALL_COMMAND })
      : t("The window can't play this file by itself. To play it here, install ffmpeg with {command}, then open this file again. Everything else works without it.", { command: INSTALL_COMMAND })} />;
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
      {converting && <p className="preview-converting muted">{t("Converted by ffmpeg as it plays. Nothing is saved.")}</p>}
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
    return <RawPreview row={row} note={t("This file would not open here, so its clusters may be wrong or damaged. Its first bytes are below.")} />;
  }
  if (IMAGE_EXTENSIONS.has(extension)) {
    return (
      <div className="preview-stage">
        {!loaded && <p className="preview-loading muted">{t("Reading from the disk.")}</p>}
        <img key={row.id} src={entryUrl(row.id)} alt={row.name} onLoad={() => setLoaded(true)} onError={() => setFailed(true)} />
      </div>
    );
  }
  if (extension === "pdf") {
    return <iframe key={row.id} className="preview-frame" src={entryUrl(row.id)} title={row.name} />;
  }
  return <RawPreview row={row} />;
}

/**
 * The preview in the details panel. It shows itself only once a file has all its
 * clusters and has really loaded, so what appears here is known to work.
 */
export function InlinePreview({ id, name, length, complete, onOpen }: {
  id: number;
  name: string;
  length: number;
  complete: boolean;
  onOpen: () => void;
}) {
  const extension = extensionOf(name);
  const isImage = IMAGE_EXTENSIONS.has(extension);
  const isVideo = NATIVE_VIDEO.has(extension) || CONVERTED_VIDEO.has(extension);
  const isAudio = NATIVE_AUDIO.has(extension) || CONVERTED_AUDIO.has(extension);
  const native = NATIVE_VIDEO.has(extension) || NATIVE_AUDIO.has(extension);
  const [ready, setReady] = useState(false);
  const [failed, setFailed] = useState(false);
  const [converting, setConverting] = useState(!native);
  const [media, setMedia] = useState<MediaInfo | null | undefined>(undefined);
  const [text, setText] = useState<string | null>(null);
  const [isPdf, setIsPdf] = useState(false);

  useEffect(() => {
    if (!complete) return;
    let current = true;
    if (isVideo || isAudio) {
      mediaInfo().then((value) => current && setMedia(value)).catch(() => current && setMedia(null));
    } else if (!isImage) {
      readEntryRange(id, 0, INLINE_TEXT_BYTES)
        .then((data) => {
          if (!current) return;
          const head = new TextDecoder("latin1").decode(data.subarray(0, PDF_MAGIC.length));
          if (extension === "pdf") setIsPdf(head === PDF_MAGIC);
          else if (looksLikeText(data)) setText(new TextDecoder("utf-8", { fatal: false }).decode(data));
        })
        .catch(() => undefined);
    }
    return () => {
      current = false;
    };
  }, [id, complete, isVideo, isAudio, isImage, extension]);

  if (!complete || length === 0) return null;

  const onPlayerError = () => {
    if (!converting) setConverting(true);
    else setFailed(true);
  };
  const shown = ready ? "" : "inline-waiting";

  if (failed) {
    return <p className="inline-note tone-serious">{t("This would not open, so its clusters may be wrong or damaged.")}</p>;
  }
  if (isImage) {
    return (
      <button className={`inline-preview inline-image ${shown}`} onClick={onOpen} title="Open large">
        <img
          src={length > MAX_INLINE_IMAGE_BYTES ? thumbUrl(id) : entryUrl(id)}
          alt={name}
          draggable={false}
          onLoad={() => setReady(true)}
          onError={() => setFailed(true)}
        />
      </button>
    );
  }
  if (isVideo || isAudio) {
    if (media === undefined) return null;
    if (converting && !media?.available) {
      return <p className="inline-note muted">Install ffmpeg to play this here: {INSTALL_COMMAND}</p>;
    }
    const source = converting && media ? convertedUrl(media, id) : entryUrl(id);
    return isVideo ? (
      <video key={source} className={`inline-preview ${shown}`} controls preload="metadata" src={source} onLoadedMetadata={() => setReady(true)} onError={onPlayerError} />
    ) : (
      <audio key={source} className={`inline-preview ${shown}`} controls preload="metadata" src={source} onLoadedMetadata={() => setReady(true)} onError={onPlayerError} />
    );
  }
  if (isPdf) return <iframe className="inline-preview inline-frame" src={entryUrl(id)} title={name} />;
  if (text !== null) return <pre className="inline-preview inline-text">{text}</pre>;
  return null;
}

function PreviewPane({ row, position, total, inBasket, onClose, onMove, onBasket, onBytes, onInside }: {
  onInside: () => void;
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
  const verdict = verdictLabel(row.verdict);

  return (
    <div className="preview-pane">
      <div className="preview-bar">
        <button className="icon-button" onClick={onClose} aria-label={t("Back to the files")}><Icon name="chevronLeft" /></button>
        <div className="preview-title">
          <strong>{row.name}</strong>
          <span className="muted">{bytes(row.length)}, {stamp(row.modified)}</span>
        </div>
        <span className={`verdict-chip ${verdictTone(row.verdict)}`}>{verdict}</span>
        {position !== null && (
          <span className="preview-nav">
            <button className="icon-button" disabled={position === 0} onClick={() => onMove(-1)} aria-label={t("Previous file")}><Icon name="chevronLeft" /></button>
            <span className="muted numeric">{t("{position} of {total}", { position: count(position + 1), total: count(total) })}</span>
            <button className="icon-button" disabled={position + 1 >= total} onClick={() => onMove(1)} aria-label={t("Next file")}><Icon name="chevronRight" /></button>
          </span>
        )}
        {isArchiveName(row.name) && <button onClick={onInside}>{t("Look inside")}</button>}
        <button onClick={onBytes}>{t("Show bytes")}</button>
        <button className="primary" onClick={() => onBasket(!inBasket)}>{inBasket ? t("On the treasure list") : t("Add to the treasure list")}</button>
      </div>
      <PreviewBody row={row} />
      <div className="preview-foot muted">{t("Read live from the disk. Nothing is copied.")}</div>
    </div>
  );
}

export function BrowseView({ floor, revision, scope, onScopeClear, standalone, onStandaloneClose, basket, onBasket, onSelect, onBytes, onInsideZip, onPreviewShown }: {
  onInsideZip: (row: BrowseRow) => void;
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
  onPreviewShown: (id: number | null) => void;
}) {
  const [types, setTypes] = useState<TypeTotal[] | null>(null);
  const [group, setGroup] = useState<BrowseGroup>("all");
  const [layout, setLayout] = useState<BrowseLayout>("icons");
  const remembered = useRef(false);
  useEffect(() => {
    invoke<Record<string, any>>("ui_settings")
      .then((settings) => {
        const saved = settings[BROWSE_SETTING] as { group?: BrowseGroup; layout?: BrowseLayout } | undefined;
        if (saved?.group) setGroup(saved.group);
        if (saved?.layout === "list" || saved?.layout === "icons") setLayout(saved.layout);
      })
      .catch(() => undefined)
      .finally(() => {
        remembered.current = true;
      });
  }, []);
  useEffect(() => {
    if (remembered.current) void invoke("set_ui_setting", { key: BROWSE_SETTING, value: { group, layout } }).catch(() => undefined);
  }, [group, layout]);
  const [sort, setSort] = useState<BrowseSort>("best");
  const [typed, setTyped] = useState("");
  const [text, setText] = useState("");
  const [total, setTotal] = useState<number | null>(null);
  const [pages, setPages] = useState<Map<number, BrowseRow[]>>(new Map());
  const [error, setError] = useState<string | null>(null);
  const [openIndex, setOpenIndex] = useState<number | null>(null);
  const [scroll, setScroll] = useState({ top: 0, width: 0, height: 0 });
  const [hasFfmpeg, setHasFfmpeg] = useState(false);
  const [hideCopies, setHideCopies] = useState(true);
  const [hiddenCopies, setHiddenCopies] = useState(0);
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
    call<BrowsePage>("browse_files", { group, folder, minConfidence: floor, sort, text, hideCopies, offset: page * PAGE_ROWS, limit: PAGE_ROWS })
      .then((result) => {
        setTotal(result.total);
        setHiddenCopies(result.hiddenCopies);
        setPages((current) => new Map(reset ? [] : current).set(page, result.rows));
      })
      .catch((reason) => setError(String(reason)));
  }, [group, folder, floor, sort, text, hideCopies]);

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

  const listed = layout === "list";
  // All shows pictures for the photos and videos among it, and icons for the rest.
  const showImages = group === "photos" || group === "all" || (group === "videos" && hasFfmpeg);
  const imageFor = useCallback((row: BrowseRow) => {
    const kind = fileIcon(row.name);
    return group === "photos" || (group === "videos" && hasFfmpeg) || (group === "all" && (kind === "image" || (kind === "film" && hasFfmpeg)));
  }, [group, hasFfmpeg]);
  const tileHeight = listed ? LIST_ROW_HEIGHT : showImages ? PHOTO_TILE_HEIGHT : FILE_TILE_HEIGHT;
  const columns = listed ? 1 : Math.max(1, Math.floor((scroll.width - TILE_GAP) / (TILE_WIDTH + TILE_GAP)));
  const tileWidth = listed ? scroll.width - TILE_GAP * 2 : Math.floor((scroll.width - TILE_GAP * (columns + 1)) / columns);
  const lineGap = listed ? 0 : TILE_GAP;
  const rowCount = Math.ceil((total ?? 0) / columns);
  const firstRow = Math.max(0, Math.floor(scroll.top / (tileHeight + lineGap)) - OVERSCAN_ROWS);
  const lastRow = Math.min(rowCount - 1, Math.ceil((scroll.top + scroll.height) / (tileHeight + lineGap)) + OVERSCAN_ROWS);

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
        scroller.current.scrollTop = Math.floor(current / columns) * (tileHeight + lineGap);
      }
      return null;
    });
  }, [columns, tileHeight, lineGap]);

  // The tiles are kept between draws, so their words must be redrawn when the language changes.
  const { language } = useLanguage();
  const tiles = useMemo(() => {
    if (total === null || scroll.width === 0) return [];
    const out: React.ReactNode[] = [];
    for (let line = firstRow; line <= lastRow; line += 1) {
      for (let column = 0; column < columns; column += 1) {
        const index = line * columns + column;
        if (index >= total) break;
        const style: CSSProperties = {
          left: TILE_GAP + column * (tileWidth + TILE_GAP),
          top: TILE_GAP + line * (tileHeight + lineGap),
          width: tileWidth,
          height: tileHeight,
        };
        const row = rowAt(index);
        out.push(row && listed ? (
          <ListRow
            key={row.id}
            row={row}
            selected={openIndex === index}
            inBasket={basket.has(`entry:${row.id}`)}
            style={style}
            onOpen={() => open(index)}
            onBasket={(add) => onBasket("entry", row.id, add)}
          />
        ) : row ? (
          <Tile
            key={row.id}
            row={row}
            showImage={imageFor(row)}
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
  }, [total, scroll.width, firstRow, lastRow, columns, tileWidth, tileHeight, lineGap, listed, rowAt, imageFor, openIndex, basket, open, onBasket, language]);

  const previewRow = standalone ?? openRow ?? null;
  const previewId = previewRow?.id ?? null;
  useEffect(() => {
    onPreviewShown(previewId);
    return () => onPreviewShown(null);
  }, [previewId, onPreviewShown]);

  return (
    <section className="center-panel browse" aria-label={t("Browse files")}>
      <div className="browse-bar">
        <div className="segmented browse-types" role="tablist">
          {types?.map((item) => (
            <button key={item.group} role="tab" aria-selected={item.group === group} disabled={item.files === 0} onClick={() => setGroup(item.group)}>
              {fromEngine(item.label)} <span className="browse-count">{count(item.files)}</span>
            </button>
          ))}
        </div>
        <div className="browse-tools">
          {scope && (
            <button className="browse-scope" onClick={onScopeClear} title={t("Show files from the whole disk")}>
              <Icon name="folder" size={13} /> <span>{scope.name}</span> <Icon name="close" size={11} />
            </button>
          )}
          <label className="sidebar-search browse-search">
            <Icon name="search" size={13} />
            <input value={typed} onChange={(event) => setTyped(event.target.value)} placeholder={t("Filter by name")} spellCheck={false} aria-label={t("Filter by name")} />
          </label>
          <label className="browse-toggle" title={t("Show one file for each set of identical bytes, among files already tested")}>
            <input type="checkbox" checked={hideCopies} onChange={(event) => setHideCopies(event.target.checked)} />
            {t("Hide copies")}{hideCopies && hiddenCopies > 0 ? ` (${count(hiddenCopies)})` : ""}
          </label>
          <div className="segmented browse-layout" role="radiogroup" aria-label={t("Layout")}>
            <button role="radio" aria-checked={layout === "icons"} aria-label={t("Icons")} title={t("Icons")} onClick={() => setLayout("icons")}><Icon name="grid" size={13} /></button>
            <button role="radio" aria-checked={layout === "list"} aria-label={t("List")} title={t("List")} onClick={() => setLayout("list")}><Icon name="list" size={13} /></button>
          </div>
          <select value={sort} onChange={(event) => setSort(event.target.value as BrowseSort)} aria-label={t("Sort")}>
            <option value="best">{t("Best first")}</option>
            <option value="newest">{t("Newest first")}</option>
            <option value="largest">{t("Largest first")}</option>
          </select>
        </div>
      </div>
      {error && <p className="warn browse-error">{fromEngine(error)}</p>}
      <div
        className="browse-scroll"
        ref={scroller}
        onScroll={(event) => {
          const element = event.currentTarget;
          setScroll({ top: element.scrollTop, width: element.clientWidth, height: element.clientHeight });
        }}
      >
        {total === 0 && <p className="empty">{text
          ? t("No files of this type have “{text}” in the name. Try a lower confidence, or clear the filter.", { text })
          : t("No files of this type here. Try a lower confidence.")}</p>}
        {total === null && !error && <p className="empty">{t("Reading the list of files.")}</p>}
        <div className="browse-sheet" style={{ height: rowCount * (tileHeight + lineGap) + TILE_GAP * 2 }}>{tiles}</div>
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
          onInside={() => onInsideZip(previewRow)}
        />
      )}
    </section>
  );
}
