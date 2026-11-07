import { useCallback, useEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { mapLayer, type Extent, type Layer, type Lens } from "../api";
import { count, bytes as formatBytes } from "../format";
import { t } from "../i18n";
import { colorTable, describeValue, legendFor, onThemeChange, rampGradient, sequentialCaption } from "../palette";

const MIN_WINDOW_CLUSTERS = 64;
const WHEEL_ZOOM_STEP = 1.25;
const DRAG_THRESHOLD_PIXELS = 4;
const MAX_BUCKETS = 8192;
/** Fast scrubbing: how often a held arrow steps, how long each speed lasts, and how much
 * each stage multiplies the step by. The step never passes one magnified view's width. */
const SCRUB_TICK_MILLIS = 60;
const SCRUB_STAGE_MILLIS = 1500;
const SCRUB_STAGE_FACTOR = 10;
/** The selection behind the marker follows at most this often, so the bytes view keeps up. */
const SCRUB_PICK_MILLIS = 150;
/** Height of one band unit in a stacked map column, in CSS pixels. */
const CSS_PIXELS_PER_ROW = 2;

/** How faint the other categories are drawn while one is isolated, out of 255. */
const ISOLATED_OTHERS_ALPHA = 40;

const LAYERS: { id: Layer; label: () => string; help: () => string }[] = [
  { id: "ownership", label: () => t("What explains it"), help: () => t("Which clusters a found entry or chain accounts for") },
  { id: "allocation", label: () => t("Allocation"), help: () => t("What the bitmap and FAT say about each cluster") },
  { id: "content", label: () => t("Content"), help: () => t("What the bytes look like, from the scan") },
  { id: "entropy", label: () => t("Entropy"), help: () => t("How random the bytes are, from the scan") },
  { id: "confidence", label: () => t("Confidence"), help: () => t("Best score of any file claiming the cluster") },
  { id: "folders", label: () => t("Folders"), help: () => t("Where directory clusters sit") },
  { id: "lens", label: () => t("Lens"), help: () => t("Clusters matching the current lens") },
];

/** A notched arrowhead, pointing down. Flipped by its class to point up from below. */
function MarkerArrow({ below, left }: { below?: boolean; left: string }) {
  return (
    <svg className={below ? "strip-arrow strip-arrow-below" : "strip-arrow"} style={{ left }} viewBox="0 0 12 8" aria-hidden="true">
      <path d="M0 0 L6 8 L12 0 L6 2.5 Z" />
    </svg>
  );
}

interface StripProps {
  layer: Layer;
  start: number;
  end: number;
  floor: number;
  lens: Lens | null;
  revision: number;
  height: number;
  highlight: Extent[];
  marker: number | null;
  /** Where the scan is reading now, drawn as a line while it runs. */
  scanAt?: number | null;
  /** One legend value to show on its own: the rest are drawn faint. */
  isolate?: number | null;
  /** Hears which values this strip's picture holds, so the legend can leave out the rest. */
  onValues?: (present: Set<number>) => void;
  clusterSize: number;
  label: string;
  windowBox?: [number, number] | null;
  onPick?: (cluster: number) => void;
  onDragRange?: (start: number, end: number) => void;
  onPan?: (deltaClusters: number) => void;
  onZoom?: (centerCluster: number, factor: number) => void;
}

function Strip(props: StripProps) {
  const { layer, start, end, floor, lens, revision, height, highlight, marker, scanAt, isolate, onValues, clusterSize, label, windowBox } = props;
  const wrapper = useRef<HTMLDivElement>(null);
  const base = useRef<HTMLCanvasElement>(null);
  const marks = useRef<HTMLCanvasElement>(null);
  const values = useRef<{ data: Uint8Array; buckets: number; rows: number }>({ data: new Uint8Array(), buckets: 0, rows: 1 });
  const dragging = useRef<{ x: number; moved: boolean; lastX: number } | null>(null);
  const [width, setWidth] = useState(0);
  const [hover, setHover] = useState<{ x: number; cluster: number; parts: { label: string; color?: string; share: number | null }[] } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [dragBox, setDragBox] = useState<[number, number] | null>(null);
  const [themeTick, setThemeTick] = useState(0);

  // The map's colours are painted into a canvas, so a new theme has to repaint it.
  useEffect(() => onThemeChange(() => setThemeTick((tick) => tick + 1)), []);

  useEffect(() => {
    const element = wrapper.current;
    if (!element) return;
    const observer = new ResizeObserver(([entry]) => setWidth(Math.floor(entry.contentRect.width)));
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    if (width === 0) return;
    let current = true;
    const buckets = Math.min(Math.ceil(width * window.devicePixelRatio), MAX_BUCKETS);
    const rows = legendFor(layer).length > 0 ? Math.max(Math.floor(height / CSS_PIXELS_PER_ROW), 1) : 1;
    mapLayer(layer, start, end, buckets, rows, floor, lens)
      .then((data) => {
        if (!current || !base.current) return;
        setError(null);
        values.current = { data, buckets, rows };
        const canvas = base.current;
        canvas.width = buckets;
        canvas.height = rows;
        const context = canvas.getContext("2d");
        if (!context) return;
        const table = colorTable(layer);
        const image = context.createImageData(buckets, rows);
        data.forEach((value, index) => {
          const [red, green, blue] = table[value];
          // With one category isolated, the others stay as a faint outline of the disk.
          const alpha = isolate === null || isolate === undefined || value === isolate ? 255 : ISOLATED_OTHERS_ALPHA;
          image.data.set([red, green, blue, alpha], index * 4);
        });
        context.putImageData(image, 0, 0);
        onValues?.(new Set(data));
      })
      .catch((reason) => {
        if (current) setError(String(reason));
      });
    return () => {
      current = false;
    };
  }, [layer, start, end, floor, lens, revision, width, height, themeTick, isolate]);

  useEffect(() => {
    const canvas = marks.current;
    if (!canvas || width === 0) return;
    const ratio = window.devicePixelRatio;
    canvas.width = width * ratio;
    canvas.height = height * ratio;
    const context = canvas.getContext("2d");
    if (!context) return;
    context.clearRect(0, 0, canvas.width, canvas.height);
    const span = end - start;
    const toX = (cluster: number) => ((cluster - start) / span) * canvas.width;
    const accent = getComputedStyle(canvas).getPropertyValue("--accent-mark").trim() || "#ffffff";
    context.fillStyle = accent;
    const tickHeight = canvas.height * 0.3;
    for (const extent of highlight) {
      const left = toX(extent.first);
      const right = toX(extent.first + extent.count);
      if (right < 0 || left > canvas.width) continue;
      context.fillRect(left, 0, Math.max(right - left, ratio), tickHeight);
      context.fillRect(left, canvas.height - tickHeight, Math.max(right - left, ratio), tickHeight);
    }
    if (marker !== null && marker >= start && marker < end) {
      context.fillRect(toX(marker) - ratio / 2, 0, Math.max(ratio, 1.5 * ratio), canvas.height);
    }
    if (scanAt !== null && scanAt !== undefined && scanAt >= start && scanAt < end) {
      // A dashed line, so it is not taken for the selected cluster's solid one.
      context.strokeStyle = getComputedStyle(canvas).getPropertyValue("--accent").trim() || accent;
      context.lineWidth = Math.max(ratio, 1.5 * ratio);
      context.setLineDash([3 * ratio, 2 * ratio]);
      context.beginPath();
      context.moveTo(toX(scanAt), 0);
      context.lineTo(toX(scanAt), canvas.height);
      context.stroke();
      context.setLineDash([]);
    }
  }, [highlight, marker, scanAt, start, end, width, height, themeTick]);

  const clusterAt = useCallback(
    (clientX: number) => {
      const rectangle = wrapper.current?.getBoundingClientRect();
      if (!rectangle) return start;
      const fraction = Math.min(Math.max((clientX - rectangle.left) / rectangle.width, 0), 0.999999);
      return Math.floor(start + fraction * (end - start));
    },
    [start, end],
  );

  function pointerDown(event: ReactPointerEvent<HTMLDivElement>) {
    event.currentTarget.setPointerCapture(event.pointerId);
    dragging.current = { x: event.clientX, moved: false, lastX: event.clientX };
  }

  function pointerMove(event: ReactPointerEvent<HTMLDivElement>) {
    const rectangle = wrapper.current?.getBoundingClientRect();
    if (!rectangle) return;
    const cluster = clusterAt(event.clientX);
    const { data, buckets, rows } = values.current;
    const column = Math.min(Math.max(Math.floor(((event.clientX - rectangle.left) / rectangle.width) * buckets), 0), Math.max(buckets - 1, 0));
    const tally = new Map<number, number>();
    for (let row = 0; row < rows; row++) {
      const value = data[row * buckets + column] ?? 0;
      tally.set(value, (tally.get(value) ?? 0) + 1);
    }
    const parts = [...tally.entries()].sort((a, b) => b[1] - a[1]);
    const legend = legendFor(layer);
    setHover({
      x: event.clientX - rectangle.left,
      cluster,
      parts: parts.slice(0, 5).map(([value, share]) => ({
        label: describeValue(layer, value),
        color: legend.find((item) => item.value === value)?.color,
        share: rows === 1 ? null : Math.round((share / rows) * 100),
      })),
    });

    const drag = dragging.current;
    if (!drag) return;
    if (Math.abs(event.clientX - drag.x) > DRAG_THRESHOLD_PIXELS) drag.moved = true;
    if (drag.moved && props.onDragRange) {
      const from = clusterAt(drag.x);
      setDragBox(from < cluster ? [from, cluster] : [cluster, from]);
    }
    if (drag.moved && props.onPan) {
      const pixels = event.clientX - drag.lastX;
      props.onPan(-(pixels / rectangle.width) * (end - start));
      drag.lastX = event.clientX;
    }
  }

  function pointerUp(event: ReactPointerEvent<HTMLDivElement>) {
    const drag = dragging.current;
    dragging.current = null;
    setDragBox(null);
    if (!drag) return;
    if (!drag.moved) {
      props.onPick?.(clusterAt(event.clientX));
    } else if (props.onDragRange) {
      const from = clusterAt(drag.x);
      const to = clusterAt(event.clientX);
      props.onDragRange(Math.min(from, to), Math.max(from, to));
    }
  }

  function wheel(event: React.WheelEvent<HTMLDivElement>) {
    if (!props.onZoom) return;
    props.onZoom(clusterAt(event.clientX), event.deltaY > 0 ? WHEEL_ZOOM_STEP : 1 / WHEEL_ZOOM_STEP);
  }

  const span = end - start;
  const box = dragBox ?? windowBox ?? null;
  return (
    <div className="strip-row">
      <div className="strip-caption">
        <span>{label}</span>
        <span className="strip-range">
          {start <= 2 ? "0" : formatBytes((start - 2) * clusterSize)} to {formatBytes((end - 2) * clusterSize)}
        </span>
      </div>
      <div
        ref={wrapper}
        className="strip"
        style={{ height }}
        onPointerDown={pointerDown}
        onPointerMove={pointerMove}
        onPointerUp={pointerUp}
        onPointerLeave={() => setHover(null)}
        onWheel={wheel}
      >
        <canvas ref={base} className="strip-base" />
        <canvas ref={marks} className="strip-marks" />
        {marker !== null && marker >= start && marker < end && (
          <>
            <MarkerArrow left={`${((marker - start) / span) * 100}%`} />
            <MarkerArrow below left={`${((marker - start) / span) * 100}%`} />
          </>
        )}
        {box && (
          <div
            className="strip-window"
            style={{ left: `${((box[0] - start) / span) * 100}%`, width: `${((box[1] - box[0]) / span) * 100}%` }}
          />
        )}
        {hover && (
          <div className={`strip-tip ${hover.x > width - 240 ? "strip-tip-left" : ""}`} style={{ left: hover.x }}>
            <strong>{t("Cluster {cluster}", { cluster: count(hover.cluster) })}</strong>
            <div className="strip-tip-sub">{t("{bytes} into the volume", { bytes: formatBytes((hover.cluster - 2) * clusterSize) })}</div>
            {hover.parts.map((part) => (
              <div key={part.label} className="strip-tip-row">
                {part.color && <span className="legend-swatch" style={{ background: part.color }} />}
                <span>{part.share !== null ? t("{label}, about {share}%", { label: part.label, share: part.share }) : part.label}</span>
              </div>
            ))}
          </div>
        )}
        {error && <div className="strip-error">{error}</div>}
      </div>
    </div>
  );
}

export interface StripsProps {
  layer: Layer;
  onLayer: (layer: Layer) => void;
  floor: number;
  onFloor: (floor: number) => void;
  lens: Lens | null;
  revision: number;
  totalClusters: number;
  clusterSize: number;
  view: [number, number];
  onView: (view: [number, number]) => void;
  highlight: Extent[];
  marker: number | null;
  /** Where a running scan is reading, or null when none runs. */
  scanAt: number | null;
  onPick: (cluster: number) => void;
}

/** The disk as one ribbon, plus a magnified view of whichever part is in focus. */
export function Strips(props: StripsProps) {
  const { layer, floor, lens, revision, totalClusters, clusterSize, view, onView, highlight, marker, onPick } = props;
  const first = 2;
  const last = totalClusters + 2;
  // Only categories either strip actually shows are listed. Read errors always are, because
  // a few bad clusters can be too small to win a pixel and must not look absent.
  const [present, setPresent] = useState<{ layer: Layer; whole: Set<number>; focus: Set<number> } | null>(null);
  const seen = present && present.layer === layer ? new Set([...present.whole, ...present.focus]) : null;
  const legend = legendFor(layer).filter((item) => !seen || seen.has(item.value) || item.always);
  // A legend item clicked shows that category on its own, until clicked again or the layer
  // changes.
  const [isolated, setIsolated] = useState<{ layer: Layer; value: number } | null>(null);
  const isolate = isolated && isolated.layer === layer ? isolated.value : null;
  const caption = sequentialCaption(layer);
  const heard = (which: "whole" | "focus") => (values: Set<number>) =>
    setPresent((known) => {
      const base = known && known.layer === layer ? known : { layer, whole: new Set<number>(), focus: new Set<number>() };
      return { ...base, [which]: values };
    });

  function clampView(start: number, end: number): [number, number] {
    let width = Math.max(end - start, MIN_WINDOW_CLUSTERS);
    width = Math.min(width, last - first);
    const left = Math.min(Math.max(start, first), last - width);
    return [Math.floor(left), Math.floor(left + width)];
  }

  const pan = (delta: number) => onView(clampView(view[0] + delta, view[1] + delta));

  // Fast scrubbing: hold an arrow on the magnified view to move through the clusters. The step
  // grows by stages while held, up to one view's width, and stops the moment the key is let go.
  const [scrubStep, setScrubStep] = useState<number | null>(null);
  const [scrubAt, setScrubAt] = useState<number | null>(null);
  const scrub = useRef<{ timer: number; at: number; step: number; direction: number; stageAt: number; pickedAt: number } | null>(null);
  const shown = scrubAt ?? marker;
  // A held key's timer outlives the draw it started in, so it reads the view from here.
  const viewNow = useRef(view);
  viewNow.current = view;
  const moveTo = (cluster: number, final: boolean) => {
    const target = Math.min(Math.max(cluster, first), last - 1);
    const [left, right] = viewNow.current;
    const width = right - left;
    if (target < left || target >= right) {
      const next = clampView(target - width / 2, target + width / 2);
      viewNow.current = next;
      onView(next);
    }
    const held = scrub.current;
    if (final || !held || performance.now() - held.pickedAt >= SCRUB_PICK_MILLIS) {
      onPick(target);
      if (held) held.pickedAt = performance.now();
    }
    return target;
  };
  const stopScrub = () => {
    const held = scrub.current;
    if (!held) return;
    window.clearInterval(held.timer);
    scrub.current = null;
    setScrubStep(null);
    setScrubAt(null);
    moveTo(held.at, true);
  };
  useEffect(() => () => {
    if (scrub.current) window.clearInterval(scrub.current.timer);
  }, []);
  const scrubKey = (event: React.KeyboardEvent<HTMLDivElement>) => {
    if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
    event.preventDefault();
    const direction = event.key === "ArrowLeft" ? -1 : 1;
    const width = Math.max(1, view[1] - view[0]);
    const from = shown ?? Math.floor((view[0] + view[1]) / 2);
    if (event.shiftKey) {
      // One whole view at once, for when it is far.
      moveTo(from + direction * width, true);
      return;
    }
    if (event.repeat || scrub.current) return;
    const held = { timer: 0, at: from, step: 1, direction, stageAt: performance.now(), pickedAt: 0 };
    const tick = () => {
      const now = performance.now();
      if (now - held.stageAt >= SCRUB_STAGE_MILLIS) {
        held.step = Math.min(held.step * SCRUB_STAGE_FACTOR, Math.max(1, viewNow.current[1] - viewNow.current[0]));
        held.stageAt = now;
        setScrubStep(held.step);
      }
      held.at = moveTo(held.at + held.direction * held.step, false);
      setScrubAt(held.at);
    };
    scrub.current = held;
    setScrubStep(1);
    tick();
    held.timer = window.setInterval(tick, SCRUB_TICK_MILLIS);
  };
  const zoom = (center: number, factor: number) => {
    const width = (view[1] - view[0]) * factor;
    const position = (center - view[0]) / (view[1] - view[0]);
    onView(clampView(center - width * position, center + width * (1 - position)));
  };

  return (
    <section className="strips" aria-label={t("Disk map")}>
      <div className="strips-controls">
        <div className="segmented" role="tablist" aria-label={t("Map layer")}>
          {LAYERS.map((item) => (
            <button
              key={item.id}
              role="tab"
              aria-selected={layer === item.id}
              title={item.help()}
              onClick={() => props.onLayer(item.id)}
            >
              {item.label()}
            </button>
          ))}
        </div>
        <label className="floor" title={t("Pulls data below this confidence out of focus, on the map and in the file lists")}>
          <span>{t("Focus on confidence {percent}% and up", { percent: Math.round(floor * 100) })}</span>
          <input
            type="range"
            min={0}
            max={100}
            value={Math.round(floor * 100)}
            onChange={(event) => props.onFloor(Number(event.target.value) / 100)}
          />
        </label>
      </div>
      <Strip
        layer={layer}
        start={first}
        end={last}
        floor={floor}
        lens={lens}
        revision={revision}
        height={30}
        highlight={highlight}
        marker={marker}
        scanAt={props.scanAt}
        isolate={isolate}
        onValues={heard("whole")}
        clusterSize={clusterSize}
        label={t("Whole volume. Drag to choose a region")}
        windowBox={view[0] <= first && view[1] >= last ? null : view}
        onPick={(cluster) => {
          const width = view[1] - view[0];
          onView(clampView(cluster - width / 2, cluster + width / 2));
        }}
        onDragRange={(start, end) => onView(clampView(start, end))}
      />
      <div
        className="strip-focus"
        tabIndex={0}
        aria-label={t("Magnified view. Hold the left or right arrow to move through the clusters, faster the longer you hold. Shift and an arrow jumps a whole view.")}
        onKeyDown={scrubKey}
        onKeyUp={(event) => {
          if (event.key === "ArrowLeft" || event.key === "ArrowRight") stopScrub();
        }}
        onBlur={stopScrub}
        onPointerDown={(event) => event.currentTarget.focus()}
      >
      {scrubStep !== null && <span className="scrub-speed" aria-live="polite">×{count(scrubStep)}</span>}
      <Strip
        layer={layer}
        start={view[0]}
        end={view[1]}
        floor={floor}
        lens={lens}
        revision={revision}
        height={74}
        highlight={highlight}
        marker={shown}
        scanAt={props.scanAt}
        isolate={isolate}
        onValues={heard("focus")}
        clusterSize={clusterSize}
        label={t("Magnified, {count} clusters. Scroll to zoom, drag to move, click to inspect, arrows to scrub", { count: count(view[1] - view[0]) })}
        onPick={onPick}
        onPan={pan}
        onZoom={zoom}
      />
      </div>
      <div className="ruler" aria-hidden="true">
        {[0, 0.2, 0.4, 0.6, 0.8, 1].map((fraction) => (
          <span key={fraction} style={{ left: `${fraction * 100}%` }}>
            {formatBytes(Math.round((view[0] - 2 + (view[1] - view[0]) * fraction) * clusterSize))}
          </span>
        ))}
      </div>
      <div className="legend">
        {legend.map((item) => (
          <button
            key={item.label}
            className={isolate === null ? "legend-item legend-button" : isolate === item.value ? "legend-item legend-button legend-on" : "legend-item legend-button legend-off"}
            aria-pressed={isolate === item.value}
            title={isolate === item.value ? t("Show every category again") : t("Show only this on the map")}
            onClick={() => setIsolated(isolate === item.value ? null : { layer, value: item.value })}
          >
            <span className="legend-swatch" style={{ background: item.color }} />
            {item.label}
          </button>
        ))}
        {caption && (
          <span className="legend-item">
            {caption[0]}
            <span className="legend-ramp" style={{ background: rampGradient() }} />
            {caption[1]}
          </span>
        )}
      </div>
    </section>
  );
}
