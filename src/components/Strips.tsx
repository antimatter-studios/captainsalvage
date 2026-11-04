import { useCallback, useEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { mapLayer, type Extent, type Layer, type Lens } from "../api";
import { count, bytes as formatBytes } from "../format";
import { colorTable, describeValue, legendFor, onThemeChange, rampGradient, SEQUENTIAL_CAPTION } from "../palette";

const MIN_WINDOW_CLUSTERS = 64;
const WHEEL_ZOOM_STEP = 1.25;
const DRAG_THRESHOLD_PIXELS = 4;
const MAX_BUCKETS = 8192;
/** Height of one band unit in a stacked map column, in CSS pixels. */
const CSS_PIXELS_PER_ROW = 2;

const LAYERS: { id: Layer; label: string; help: string }[] = [
  { id: "ownership", label: "What explains it", help: "Which clusters a found entry or chain accounts for" },
  { id: "allocation", label: "Allocation", help: "What the bitmap and FAT say about each cluster" },
  { id: "content", label: "Content", help: "What the bytes look like, from the scan" },
  { id: "entropy", label: "Entropy", help: "How random the bytes are, from the scan" },
  { id: "confidence", label: "Confidence", help: "Best score of any file claiming the cluster" },
  { id: "folders", label: "Folders", help: "Where directory clusters sit" },
  { id: "lens", label: "Lens", help: "Clusters matching the current lens" },
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
  clusterSize: number;
  label: string;
  windowBox?: [number, number] | null;
  onPick?: (cluster: number) => void;
  onDragRange?: (start: number, end: number) => void;
  onPan?: (deltaClusters: number) => void;
  onZoom?: (centerCluster: number, factor: number) => void;
}

function Strip(props: StripProps) {
  const { layer, start, end, floor, lens, revision, height, highlight, marker, clusterSize, label, windowBox } = props;
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
          image.data.set([red, green, blue, 255], index * 4);
        });
        context.putImageData(image, 0, 0);
      })
      .catch((reason) => {
        if (current) setError(String(reason));
      });
    return () => {
      current = false;
    };
  }, [layer, start, end, floor, lens, revision, width, height, themeTick]);

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
  }, [highlight, marker, start, end, width, height, themeTick]);

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
            <strong>Cluster {count(hover.cluster)}</strong>
            <div className="strip-tip-sub">{formatBytes((hover.cluster - 2) * clusterSize)} into the volume</div>
            {hover.parts.map((part) => (
              <div key={part.label} className="strip-tip-row">
                {part.color && <span className="legend-swatch" style={{ background: part.color }} />}
                <span>{part.label}{part.share !== null ? `, about ${part.share}%` : ""}</span>
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
  onPick: (cluster: number) => void;
}

/** The disk as one ribbon, plus a magnified view of whichever part is in focus. */
export function Strips(props: StripsProps) {
  const { layer, floor, lens, revision, totalClusters, clusterSize, view, onView, highlight, marker, onPick } = props;
  const first = 2;
  const last = totalClusters + 2;
  const legend = legendFor(layer);
  const caption = SEQUENTIAL_CAPTION[layer];

  function clampView(start: number, end: number): [number, number] {
    let width = Math.max(end - start, MIN_WINDOW_CLUSTERS);
    width = Math.min(width, last - first);
    const left = Math.min(Math.max(start, first), last - width);
    return [Math.floor(left), Math.floor(left + width)];
  }

  const pan = (delta: number) => onView(clampView(view[0] + delta, view[1] + delta));
  const zoom = (center: number, factor: number) => {
    const width = (view[1] - view[0]) * factor;
    const position = (center - view[0]) / (view[1] - view[0]);
    onView(clampView(center - width * position, center + width * (1 - position)));
  };

  return (
    <section className="strips" aria-label="Disk map">
      <div className="strips-controls">
        <div className="segmented" role="tablist" aria-label="Map layer">
          {LAYERS.map((item) => (
            <button
              key={item.id}
              role="tab"
              aria-selected={layer === item.id}
              title={item.help}
              onClick={() => props.onLayer(item.id)}
            >
              {item.label}
            </button>
          ))}
        </div>
        <label className="floor" title="Pulls data below this confidence out of focus, on the map and in the file lists">
          <span>Focus on confidence {Math.round(floor * 100)}% and up</span>
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
        clusterSize={clusterSize}
        label="Whole volume. Drag to choose a region"
        windowBox={view[0] <= first && view[1] >= last ? null : view}
        onPick={(cluster) => {
          const width = view[1] - view[0];
          onView(clampView(cluster - width / 2, cluster + width / 2));
        }}
        onDragRange={(start, end) => onView(clampView(start, end))}
      />
      <Strip
        layer={layer}
        start={view[0]}
        end={view[1]}
        floor={floor}
        lens={lens}
        revision={revision}
        height={74}
        highlight={highlight}
        marker={marker}
        clusterSize={clusterSize}
        label={`Magnified, ${count(view[1] - view[0])} clusters. Scroll to zoom, drag to move, click to inspect`}
        onPick={onPick}
        onPan={pan}
        onZoom={zoom}
      />
      <div className="ruler" aria-hidden="true">
        {[0, 0.2, 0.4, 0.6, 0.8, 1].map((fraction) => (
          <span key={fraction} style={{ left: `${fraction * 100}%` }}>
            {formatBytes(Math.round((view[0] - 2 + (view[1] - view[0]) * fraction) * clusterSize))}
          </span>
        ))}
      </div>
      <div className="legend">
        {legend.map((item) => (
          <span key={item.label} className="legend-item">
            <span className="legend-swatch" style={{ background: item.color }} />
            {item.label}
          </span>
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
