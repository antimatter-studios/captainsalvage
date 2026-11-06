import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useMemo, useState, type ReactNode } from "react";
import { call, type DiskAccess, type KnownVolume, type StartupReport, type StartupVolume } from "../api";
import { bytes, count } from "../format";
import { fromEngine, locale, t } from "../i18n";
import { Command } from "./Command";
import { MountBlockToggle } from "./MountBlock";

const CHART_WIDTH = 640;
const CHART_HEIGHT = 900;
/** The disk's own island, in the middle of the chart. */
const HOME_X = 360;
const HOME_Y = 480;
/** The islands for each kind of file sit evenly round the disk's island, on this ellipse,
 * clockwise from the upper left, so the course between them runs round the edge. */
const RING_X = 205;
const RING_Y = 232;
const RING_START = (-150 * Math.PI) / 180;
const MAX_ISLANDS = 7;
const HOME_RADIUS = 50;
const SHORE_POINTS = 96;
const SMALLEST_ISLAND = 13;
const ISLAND_GROWTH = 29;
/** The shallow water round an island, as rings at these multiples of its size. */
const SHOAL_RINGS = [1.65, 1.3];
/** How far a label sits below its island's shore. */
const LABEL_GAP = 16;
const SOUNDING_COUNT = 64;
const SOUNDING_SPACE = 6;
const PIXELS_PER_METRE = 9;
/** The brand and the promises sit over the chart here, so nothing busy may land under them. */
const TEXT_CLEAR_TOP = 225;
const TEXT_CLEAR_BOTTOM = 770;
const ROSE_X = 568;
const ROSE_Y = 765;
const ROSE_RADIUS = 30;
const KIND_LABELS: Record<string, () => string> = {
  photos: () => t("Photos"),
  videos: () => t("Videos"),
  music: () => t("Music"),
  documents: () => t("Documents"),
  archives: () => t("Archives"),
  other: () => t("Other"),
};

interface Island {
  x: number;
  y: number;
  radius: number;
  seed: number;
  label: string;
  detail: string;
  home: boolean;
}

/** An island's shore: a circle pushed in and out by a few waves, the same every time. */
function shore(x: number, y: number, radius: number, seed: number): string {
  const points: string[] = [];
  for (let index = 0; index < SHORE_POINTS; index += 1) {
    const angle = (index / SHORE_POINTS) * Math.PI * 2;
    const wobble = 1 + 0.22 * Math.sin(3 * angle + seed) + 0.12 * Math.sin(5 * angle + seed * 1.7) + 0.06 * Math.sin(11 * angle + seed * 2.9);
    points.push(`${(x + radius * wobble * Math.cos(angle)).toFixed(1)} ${(y + radius * wobble * Math.sin(angle)).toFixed(1)}`);
  }
  return `M${points.join("L")}Z`;
}

function slot(index: number, total: number): [number, number] {
  const angle = RING_START + (index / Math.max(total, 1)) * Math.PI * 2;
  return [Math.round(HOME_X + RING_X * Math.cos(angle)), Math.round(HOME_Y + RING_Y * Math.sin(angle))];
}

/** The disk in the middle, and an island for each kind of file found, sized by its bytes.
 * Before anything is known, the same islands stand uncharted, without names. */
function islands(known: KnownVolume | null, label: string, detail: string): Island[] {
  const kinds = (known?.kinds ?? []).filter((kind) => kind.files > 0).slice(0, MAX_ISLANDS - 1);
  const largest = Math.max(1, ...kinds.map((kind) => kind.bytes));
  const found: Island[] = [{ x: HOME_X, y: HOME_Y, radius: HOME_RADIUS, seed: 0.7, label, detail, home: true }];
  if (kinds.length === 0) {
    for (let index = 0; index < MAX_ISLANDS; index += 1) {
      const [x, y] = slot(index, MAX_ISLANDS);
      found.push({ x, y, radius: SMALLEST_ISLAND + 8 + (index % 3) * 6, seed: 1.3 + index * 0.91, label: "", detail: "", home: false });
    }
    return found;
  }
  const total = kinds.length + (known?.lostFolders ? 1 : 0);
  kinds.forEach((kind, index) => {
    const [x, y] = slot(index, total);
    found.push({
      x, y,
      radius: SMALLEST_ISLAND + ISLAND_GROWTH * Math.sqrt(kind.bytes / largest),
      seed: 1.3 + index * 0.91,
      label: (KIND_LABELS[kind.group] ?? KIND_LABELS.other)(),
      detail: t("{count} files, {bytes}", { count: count(kind.files), bytes: bytes(kind.bytes) }),
      home: false,
    });
  });
  if (known?.lostFolders) {
    const [x, y] = slot(kinds.length, total);
    found.push({ x, y, radius: SMALLEST_ISLAND + 6, seed: 5.1, label: t("Lost folders"), detail: t("{count} pieces", { count: count(known.lostFolders) }), home: false });
  }
  return found;
}

/** Depths get smaller toward land, as on a real chart. The same chart every time. */
function soundings(placed: Island[]): { x: number; y: number; depth: number }[] {
  const found: { x: number; y: number; depth: number }[] = [];
  let state = 20261106;
  const next = () => {
    state = (state * 1664525 + 1013904223) % 4294967296;
    return state / 4294967296;
  };
  for (let tries = 0; found.length < SOUNDING_COUNT && tries < 4000; tries += 1) {
    const x = 18 + next() * (CHART_WIDTH - 36);
    const y = TEXT_CLEAR_TOP + 20 + next() * (TEXT_CLEAR_BOTTOM - TEXT_CLEAR_TOP - 40);
    const clear = Math.min(...placed.map((island) => Math.hypot(x - island.x, y - island.y) - island.radius * (SHOAL_RINGS[0] + 0.35)));
    // Nor on a label under an island, nor on the compass rose.
    const onLabel = placed.some((island) => island.label && Math.abs(x - island.x) < 70 && y > island.y && y < island.y + island.radius * 1.15 + LABEL_GAP + 24);
    const onRose = Math.hypot(x - ROSE_X, y - ROSE_Y) < ROSE_RADIUS + 26;
    if (clear < SOUNDING_SPACE || onLabel || onRose) continue;
    found.push({ x, y, depth: Math.max(3, Math.round(clear / PIXELS_PER_METRE)) });
  }
  return found;
}

/** A course from island to island, bowing outward from the disk so it runs round the
 * islands and clear of their labels. */
function course(placed: Island[]): string {
  const stops = placed.filter((island) => !island.home);
  if (stops.length === 0) return "";
  const home = placed[0];
  let path = `M${stops[0].x} ${stops[0].y}`;
  const bow = (from: { x: number; y: number }, to: { x: number; y: number }) => {
    const middle = { x: (from.x + to.x) / 2, y: (from.y + to.y) / 2 };
    const out = Math.hypot(middle.x - home.x, middle.y - home.y) || 1;
    return `Q${(middle.x + ((middle.x - home.x) / out) * 46).toFixed(0)} ${(middle.y + ((middle.y - home.y) / out) * 46).toFixed(0)} ${to.x} ${to.y}`;
  };
  for (let index = 1; index < stops.length; index += 1) path += ` ${bow(stops[index - 1], stops[index])}`;
  return path;
}

function foundText(known: KnownVolume): string {
  return known.scanned ? t("{bytes} found", { bytes: bytes(known.bytesFound) }) : t("not scanned yet");
}

/** A chart of what was found: the disk as an island, with an island for each kind of file
 * round it, sized by how much there is, and a course linking them. */
function Chart({ known, label, detail }: { known: KnownVolume | null; label: string; detail: string }) {
  const placed = useMemo(() => islands(known, label, detail), [known, label, detail]);
  const depths = useMemo(() => soundings(placed), [placed]);
  const route = useMemo(() => course(placed), [placed]);
  const charted = (known?.kinds ?? []).length > 0;
  const home = placed[0];
  return (
    <svg className="start-chart-art" viewBox={`0 0 ${CHART_WIDTH} ${CHART_HEIGHT}`} preserveAspectRatio="xMidYMid slice" aria-hidden="true">
      {[80, 240, 400, 560].map((x) => <line key={`x${x}`} x1={x} y1={0} x2={x} y2={CHART_HEIGHT} className="start-grid" />)}
      {[60, 220, 380, 540, 700, 860].map((y) => <line key={`y${y}`} x1={0} y1={y} x2={CHART_WIDTH} y2={y} className="start-grid" />)}
      {placed.map((island) => SHOAL_RINGS.map((ring) => (
        <path key={`${island.x}-${ring}`} d={shore(island.x, island.y, island.radius * ring, island.seed + ring)} className="start-contour" />
      )))}
      {depths.map((depth) => (
        <text key={`${depth.x}-${depth.y}`} x={depth.x} y={depth.y} className="start-sounding">{depth.depth}</text>
      ))}
      {charted && <path d={route} className="start-course" />}
      {placed.map((island) => (
        <g key={`${island.x}-${island.y}`} className={charted || island.home ? "start-island" : "start-island start-island-uncharted"}>
          <path d={shore(island.x, island.y, island.radius, island.seed)} className="start-land" />
          <path d={shore(island.x, island.y, island.radius * 0.62, island.seed + 0.4)} className="start-land-contour" />
        </g>
      ))}
      {placed.filter((island) => island.label).map((island) => {
        // Under the island, where nothing else on the ring is.
        const y = island.y + island.radius * 1.15 + LABEL_GAP;
        return (
          <g key={`label-${island.x}-${island.y}`} className={island.home ? "start-home-label" : "start-isle-label"} textAnchor="middle">
            <text x={island.x} y={y} className="start-isle-name">{island.label}</text>
            <text x={island.x} y={y + 15} className="start-isle-detail">{island.detail}</text>
          </g>
        );
      })}
      <circle cx={home.x} cy={home.y} r="30" className="start-ping" style={{ transformOrigin: `${home.x}px ${home.y}px` }} />
      <circle cx={home.x} cy={home.y} r="5" className="start-anchorage" />
      <circle cx={home.x} cy={home.y} r="1.6" className="start-anchorage-dot" />
      <g className="start-rose">
        <circle cx={ROSE_X} cy={ROSE_Y} r={ROSE_RADIUS} />
        <circle cx={ROSE_X} cy={ROSE_Y} r={ROSE_RADIUS - 6} strokeDasharray="1 3" />
        {Array.from({ length: 8 }, (_, index) => {
          const angle = (index / 8) * Math.PI * 2 - Math.PI / 2;
          const reach = index % 2 === 0 ? ROSE_RADIUS + 8 : ROSE_RADIUS - 10;
          const tip = [ROSE_X + Math.cos(angle) * reach, ROSE_Y + Math.sin(angle) * reach];
          const left = [ROSE_X + Math.cos(angle + 0.22) * 6, ROSE_Y + Math.sin(angle + 0.22) * 6];
          const right = [ROSE_X + Math.cos(angle - 0.22) * 6, ROSE_Y + Math.sin(angle - 0.22) * 6];
          return <path key={index} d={`M${left.join(" ")}L${tip.join(" ")}L${right.join(" ")}Z`} className={index === 0 ? "start-rose-north" : "start-rose-point"} />;
        })}
        <text x={ROSE_X} y={ROSE_Y - ROSE_RADIUS - 12} className="start-rose-label">N</text>
      </g>
    </svg>
  );
}

function lastOpened(known: KnownVolume): string {
  return new Date(known.record.lastOpenedMillis).toLocaleDateString(locale(), { day: "numeric", month: "long", year: "numeric" });
}

function KnownLine({ known }: { known: KnownVolume }) {
  return (
    <p className="start-known">
      {t("Seen before, last opened {date}.", { date: lastOpened(known) })}
      {known.scanned && ` ${t("{bytes} found by name, in {count} files.", { bytes: bytes(known.bytesFound), count: count(known.filesFound) })}`}
      {known.filesPassed + known.filesDamaged > 0 && ` ${t("{passed} tested good, {damaged} damaged.", { passed: count(known.filesPassed), damaged: count(known.filesDamaged) })}`}
    </p>
  );
}

function VolumeRow({ volume, onOpen }: { volume: StartupVolume; onOpen: (path: string) => void }) {
  const block = volume.device.replace("/dev/rdisk", "/dev/disk");
  const title = volume.name || t("Unnamed volume");
  const ready = volume.canOpen && !volume.mountPoint && volume.readable;
  return (
    <li className={volume.canOpen ? "start-volume" : "start-volume start-volume-other"}>
      <div className="start-volume-head">
        <div className="start-volume-title">
          <strong>{title}</strong>
          <span>{bytes(volume.bytes)}, {volume.format}, {volume.device}</span>
        </div>
        {ready && <button className="primary" onClick={() => onOpen(volume.device)}>{volume.known ? t("Open again") : t("Open")}</button>}
      </div>
      {volume.known ? <KnownLine known={volume.known} /> : volume.canOpen && <p className="start-new">{t("Not seen before.")}</p>}
      {volume.uuid && <MountBlockToggle uuid={volume.uuid} name={title} mounted={Boolean(volume.mountPoint)} />}
      {!volume.canOpen && <p className="start-new">{t("Captain Salvage reads exFAT for now.")}</p>}
      {volume.canOpen && volume.mountPoint && (
        <div className="start-caution">
          <p>{t("It is mounted, so macOS may be writing to it. Unmounting stops that, but writes a little itself. It has to be unmounted before it can be opened here.")}</p>
          <Command text={`diskutil unmount ${block}`} />
        </div>
      )}
      {volume.canOpen && !volume.mountPoint && !volume.readable && (
        <div className="start-needs">
          <p>{t("Reading it needs your permission. Run this in Terminal. It gives read-only access, and lasts until the disk is reconnected.")}</p>
          <Command text={`sudo chgrp staff ${volume.device} ${volume.wholeDisk}`} />
        </div>
      )}
    </li>
  );
}

/** The first thing a person sees: choose the damaged disk, and see what is known about it. */
export function StartScreen({ imagePath, failure, onOpen, onBack, themeSwitch }: {
  imagePath: string;
  failure: string | null;
  onOpen: (path: string) => void;
  onBack?: () => void;
  themeSwitch: ReactNode;
}) {
  const [report, setReport] = useState<StartupReport | null>(null);
  const [looking, setLooking] = useState(true);
  const [lookError, setLookError] = useState<string | null>(null);
  const [internal, setInternal] = useState(false);
  const [path, setPath] = useState(imagePath);
  const [access, setAccess] = useState<DiskAccess | null>(null);
  const [version, setVersion] = useState<string | null>(null);

  useEffect(() => {
    getVersion().then(setVersion).catch(() => undefined);
  }, []);

  const look = useCallback((includeInternal: boolean) => {
    setLooking(true);
    setLookError(null);
    call<StartupReport>("startup_disks", { includeInternal })
      .then(setReport)
      .catch((reason) => setLookError(String(reason)))
      .finally(() => setLooking(false));
  }, []);

  useEffect(() => look(internal), [look, internal]);
  useEffect(() => setPath(imagePath), [imagePath]);
  useEffect(() => {
    if (failure && path) call<DiskAccess>("disk_access", { path }).then(setAccess).catch(() => undefined);
    else setAccess(null);
  }, [failure, path]);

  const recognised = report?.volumes.find((volume) => volume.known)?.known ?? report?.elsewhere[0] ?? null;
  const volumes = report?.volumes ?? [];

  return (
    <main className="start">
      <section className="start-chart" data-tauri-drag-region>
        <Chart known={recognised} label={recognised?.record.name || t("Uncharted")} detail={recognised ? foundText(recognised) : t("your disk goes here")} />
        <div className="start-brand">
          <h1>Captain Salvage</h1>
          <p>{t("Charts what is left on a damaged disk, and helps you bring it home.")}</p>
        </div>
        <ul className="start-promises">
          <li>{t("It only reads. Nothing in it can write to your disk.")}</li>
          <li>{t("It maps every cluster, even where the folders are gone.")}</li>
          <li>{t("It tests what it finds, so you know what is really whole.")}</li>
        </ul>
        {version && <span className="start-version">v{version}</span>}
      </section>

      <section className="start-panel">
        <div className="start-panel-top" data-tauri-drag-region>
          {onBack && <button onClick={onBack}>{t("Back to the open disk")}</button>}
          <div className="start-spacer" data-tauri-drag-region />
          {themeSwitch}
        </div>

        <div className="start-heading">
          <h2>{t("Which disk is damaged?")}</h2>
          <button onClick={() => look(internal)} disabled={looking}>{looking ? t("Looking") : t("Check again")}</button>
        </div>

        {lookError && <p className="warn">{fromEngine(lookError)}</p>}
        {failure && <p className="warn">{fromEngine(failure)}</p>}
        {access?.instructions.map((text) => <p key={text} className="instruction">{fromEngine(text)}</p>)}

        {volumes.length > 0 ? (
          <ul className="start-volumes">
            {volumes.map((volume) => <VolumeRow key={volume.device} volume={volume} onOpen={onOpen} />)}
          </ul>
        ) : (
          !looking && <p className="start-empty">{t("No disks found here. Plug in the damaged disk, then check again.")}</p>
        )}

        <label className="check start-internal">
          <input type="checkbox" checked={internal} onChange={(event) => setInternal(event.target.checked)} /> {t("Show internal disks")}
        </label>

        {report && report.elsewhere.length > 0 && (
          <>
            <h3>{t("Seen before, not attached")}</h3>
            <ul className="start-volumes">
              {report.elsewhere.map((known) => (
                <li key={`${known.record.uuid}-${known.record.lastOpenedMillis}`} className="start-volume">
                  <div className="start-volume-title">
                    <strong>{known.record.name || t("Unnamed volume")}</strong>
                    <span>{bytes(known.record.bytes)}</span>
                  </div>
                  <KnownLine known={known} />
                  {known.record.uuid && <MountBlockToggle uuid={known.record.uuid} name={known.record.name || t("Unnamed volume")} mounted={false} />}
                </li>
              ))}
            </ul>
          </>
        )}

        <section className="start-image">
          <h3>{t("Or open a disk image")}</h3>
          <p>{t("A raw image of an exFAT partition, or a device by its path.")}</p>
          <form
            onSubmit={(event) => {
              event.preventDefault();
              onOpen(path.trim());
            }}
          >
            <input value={path} onChange={(event) => setPath(event.target.value)} placeholder="/path/to/disk.img" spellCheck={false} aria-label={t("Disk image or device path")} />
            <button type="button" onClick={() => void invoke<string | null>("choose_disk_image").then((chosen) => chosen && setPath(chosen))}>{t("Choose a file")}</button>
            <button type="submit" className={path.trim() ? "primary" : undefined} disabled={!path.trim()}>{t("Open")}</button>
          </form>
        </section>

        {report && volumes.length === 0 && (
          <p className="start-warning">
            {t("macOS mounts a disk as soon as it is plugged in, and can write to it while it is mounted. Plug the damaged disk in while Captain Salvage is open, turn on Block mounting for it, and unmount it once. From then on macOS cannot mount it while the app is open.")}
          </p>
        )}

      </section>
    </main>
  );
}
