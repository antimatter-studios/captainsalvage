import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useMemo, useState, type ReactNode } from "react";
import { call, type DiskAccess, type KnownVolume, type StartupReport, type StartupVolume } from "../api";
import { bytes, count } from "../format";
import { Command } from "./Command";

const CHART_WIDTH = 640;
const CHART_HEIGHT = 900;
const WRECK_X = 372;
const WRECK_Y = 470;
const CONTOUR_RADII = [64, 118, 176, 240, 312, 392, 480, 580];
const CONTOUR_POINTS = 120;
const SOUNDING_COUNT = 46;
const SOUNDING_CLEAR_RADIUS = 96;
const METRES_PER_PIXEL = 1 / 26;
/** The brand and the promises sit over the chart here, so no figures may land under them. */
const DEFAULT_DISK_SLEEP_MINUTES = 10;
const TEXT_CLEAR_TOP = 225;
const TEXT_CLEAR_BOTTOM = 770;
/** Nor under the wreck's label, which runs out to the right of it. */
const LABEL_CLEAR_RIGHT = 210;
const LABEL_CLEAR_HEIGHT = 44;

function contourPath(radius: number, seed: number): string {
  const points: string[] = [];
  for (let index = 0; index < CONTOUR_POINTS; index += 1) {
    const angle = (index / CONTOUR_POINTS) * Math.PI * 2;
    const wobble = 1 + 0.17 * Math.sin(3 * angle + seed) + 0.08 * Math.sin(5 * angle + seed * 2.1) + 0.04 * Math.sin(9 * angle + seed * 3.3);
    const x = WRECK_X + radius * wobble * Math.cos(angle) * 0.92;
    const y = WRECK_Y + radius * wobble * Math.sin(angle) * 1.08;
    points.push(`${x.toFixed(1)} ${y.toFixed(1)}`);
  }
  return `M${points.join("L")}Z`;
}

/** Depths on a chart get smaller toward a shoal, so these do too. The same chart every time. */
function soundings(): { x: number; y: number; depth: number }[] {
  const found: { x: number; y: number; depth: number }[] = [];
  let state = 20261104;
  const next = () => {
    state = (state * 1664525 + 1013904223) % 4294967296;
    return state / 4294967296;
  };
  while (found.length < SOUNDING_COUNT) {
    const x = next() * CHART_WIDTH;
    const y = next() * CHART_HEIGHT;
    const distance = Math.hypot(x - WRECK_X, y - WRECK_Y);
    const underLabel = x > WRECK_X && x < WRECK_X + LABEL_CLEAR_RIGHT && Math.abs(y - WRECK_Y) < LABEL_CLEAR_HEIGHT;
    if (distance < SOUNDING_CLEAR_RADIUS || underLabel || y < TEXT_CLEAR_TOP || y > TEXT_CLEAR_BOTTOM) continue;
    found.push({ x, y, depth: Math.max(2, Math.round(distance * METRES_PER_PIXEL)) });
  }
  return found;
}

function foundText(known: KnownVolume): string {
  return known.scanned ? `${bytes(known.bytesFound)} found` : "not scanned yet";
}

/** A real chart's symbol for a wreck, with the disk it stands for beside it. */
function Chart({ label, detail }: { label: string; detail: string }) {
  const contours = useMemo(() => CONTOUR_RADII.map((radius, index) => contourPath(radius, 1.3 + index * 0.37)), []);
  const depths = useMemo(soundings, []);
  return (
    <svg className="start-chart-art" viewBox={`0 0 ${CHART_WIDTH} ${CHART_HEIGHT}`} preserveAspectRatio="xMidYMid slice" aria-hidden="true">
      {[...contours].reverse().map((path) => <path key={path} d={path} className="start-contour" />)}
      {depths.map((depth) => (
        <text key={`${depth.x}-${depth.y}`} x={depth.x} y={depth.y} className="start-sounding">{depth.depth}</text>
      ))}
      <circle cx={WRECK_X} cy={WRECK_Y} r="30" className="start-ping" />
      <ellipse cx={WRECK_X} cy={WRECK_Y} rx="27" ry="15" className="start-wreck-ring" />
      <path d={`M${WRECK_X - 18} ${WRECK_Y}H${WRECK_X + 18}M${WRECK_X - 9} ${WRECK_Y - 6}V${WRECK_Y + 6}M${WRECK_X} ${WRECK_Y - 6}V${WRECK_Y + 6}M${WRECK_X + 9} ${WRECK_Y - 6}V${WRECK_Y + 6}`} className="start-wreck" />
      <text x={WRECK_X + 38} y={WRECK_Y - 3} className="start-wreck-label">{label}</text>
      <text x={WRECK_X + 38} y={WRECK_Y + 15} className="start-wreck-detail">{detail}</text>
    </svg>
  );
}

/** What stops the drops and remounts doing harm, and how to undo all of it afterwards. */
function KeepSafe({ volumes, diskSleep }: { volumes: StartupVolume[]; diskSleep: number | null }) {
  const drives = new Set(volumes.filter((volume) => volume.canOpen && volume.external).map((volume) => volume.wholeDisk));
  const onDrive = volumes.filter((volume) => drives.has(volume.wholeDisk) && volume.fstabLine && volume.uuid);
  if (onDrive.length === 0) return null;
  const waiting = onDrive.filter((volume) => !volume.protected);
  const names = onDrive.map((volume) => volume.name || "unnamed volume").join(" and ");
  const sleepsNever = diskSleep === 0;
  const restoreSleep = diskSleep && diskSleep > 0 ? diskSleep : DEFAULT_DISK_SLEEP_MINUTES;
  return (
    <section className="start-safe">
      <h3>Keep this disk safe</h3>
      <p>A damaged disk can drop off and come back. Each time, macOS may mount it and write to it, so set this up before you start.</p>

      <div className={waiting.length === 0 ? "start-step start-step-done" : "start-step"}>
        <span className="start-step-mark" aria-hidden="true">{waiting.length === 0 ? "✓" : "○"}</span>
        <div className="start-step-body">
          {waiting.length === 0 ? (
            <p><strong>macOS will not mount {names}.</strong> After the next reconnect, check that it stayed unmounted.</p>
          ) : (
            <>
              <p><strong>Stop macOS mounting {names}.</strong> Run this in Terminal. It has not been proven on a damaged disk yet, so after the next reconnect, check that it stayed unmounted.</p>
              {waiting.map((volume) => <Command key={volume.device} text={`printf '\\n%s\\n' '${volume.fstabLine}' | sudo tee -a /etc/fstab`} />)}
            </>
          )}
        </div>
      </div>

      <div className={sleepsNever ? "start-step start-step-done" : "start-step"}>
        <span className="start-step-mark" aria-hidden="true">{sleepsNever ? "✓" : "○"}</span>
        <div className="start-step-body">
          {sleepsNever ? (
            <p><strong>macOS will not put disks to sleep.</strong> A drive's own firmware still may.</p>
          ) : (
            <>
              <p><strong>Keep disks awake when this app is closed too.</strong> While it is open, it already stops macOS putting disks to sleep. A drive's own firmware still may.</p>
              <Command text="sudo pmset -a disksleep 0" />
            </>
          )}
        </div>
      </div>

      <h4>To go back to normal afterwards</h4>
      {onDrive.map((volume) => <Command key={volume.device} text={`sudo sed -i '' '/^UUID=${volume.uuid} /d' /etc/fstab`} />)}
      <Command text={`sudo pmset -a disksleep ${restoreSleep}`} />
      <p className="start-safe-after">Then reconnect the disk, and macOS mounts it as usual.</p>
    </section>
  );
}

function lastOpened(known: KnownVolume): string {
  return new Date(known.record.lastOpenedMillis).toLocaleDateString(undefined, { day: "numeric", month: "long", year: "numeric" });
}

function KnownLine({ known }: { known: KnownVolume }) {
  return (
    <p className="start-known">
      Seen before, last opened {lastOpened(known)}.
      {known.scanned && ` ${bytes(known.bytesFound)} found by name, in ${count(known.filesFound)} files.`}
      {known.filesPassed + known.filesDamaged > 0 && ` ${count(known.filesPassed)} tested good, ${count(known.filesDamaged)} damaged.`}
    </p>
  );
}

function VolumeRow({ volume, onOpen }: { volume: StartupVolume; onOpen: (path: string) => void }) {
  const block = volume.device.replace("/dev/rdisk", "/dev/disk");
  const title = volume.name || "Unnamed volume";
  const ready = volume.canOpen && !volume.mountPoint && volume.readable;
  return (
    <li className={volume.canOpen ? "start-volume" : "start-volume start-volume-other"}>
      <div className="start-volume-head">
        <div className="start-volume-title">
          <strong>{title}</strong>
          <span>{bytes(volume.bytes)}, {volume.format}, {volume.device}</span>
        </div>
        {ready && <button className="primary" onClick={() => onOpen(volume.device)}>{volume.known ? "Open again" : "Open"}</button>}
      </div>
      {volume.known ? <KnownLine known={volume.known} /> : volume.canOpen && <p className="start-new">Not seen before.</p>}
      {!volume.canOpen && <p className="start-new">Captain Salvage reads exFAT for now.</p>}
      {volume.canOpen && volume.mountPoint && (
        <div className="start-caution">
          <p>It is mounted, so macOS may be writing to it. Unmounting stops that, but writes a little itself. It has to be unmounted before it can be opened here.</p>
          <Command text={`diskutil unmount ${block}`} />
        </div>
      )}
      {volume.canOpen && !volume.mountPoint && !volume.readable && (
        <div className="start-needs">
          <p>Reading it needs your permission. Run this in Terminal. It gives read-only access, and lasts until the disk is reconnected.</p>
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
        <Chart label={recognised?.record.name || "Uncharted"} detail={recognised ? foundText(recognised) : "your disk goes here"} />
        <div className="start-brand">
          <h1>Captain Salvage</h1>
          <p>Charts what is left on a damaged disk, and helps you bring it home.</p>
        </div>
        <ul className="start-promises">
          <li>It only reads. Nothing in it can write to your disk.</li>
          <li>It maps every cluster, even where the folders are gone.</li>
          <li>It tests what it finds, so you know what is really whole.</li>
        </ul>
        {version && <span className="start-version">v{version}</span>}
      </section>

      <section className="start-panel">
        <div className="start-panel-top" data-tauri-drag-region>
          {onBack && <button onClick={onBack}>Back to the open disk</button>}
          <div className="start-spacer" data-tauri-drag-region />
          {themeSwitch}
        </div>

        <div className="start-heading">
          <h2>Which disk is damaged?</h2>
          <button onClick={() => look(internal)} disabled={looking}>{looking ? "Looking" : "Check again"}</button>
        </div>

        {lookError && <p className="warn">{lookError}</p>}
        {failure && <p className="warn">{failure}</p>}
        {access?.instructions.map((text) => <p key={text} className="instruction">{text}</p>)}

        {volumes.length > 0 ? (
          <ul className="start-volumes">
            {volumes.map((volume) => <VolumeRow key={volume.device} volume={volume} onOpen={onOpen} />)}
          </ul>
        ) : (
          !looking && <p className="start-empty">No disks found here. Plug in the damaged disk, then check again.</p>
        )}

        <label className="check start-internal">
          <input type="checkbox" checked={internal} onChange={(event) => setInternal(event.target.checked)} /> Show internal disks
        </label>

        {report && report.elsewhere.length > 0 && (
          <>
            <h3>Seen before, not attached</h3>
            <ul className="start-volumes">
              {report.elsewhere.map((known) => (
                <li key={`${known.record.uuid}-${known.record.lastOpenedMillis}`} className="start-volume">
                  <div className="start-volume-title">
                    <strong>{known.record.name || "Unnamed volume"}</strong>
                    <span>{bytes(known.record.bytes)}</span>
                  </div>
                  <KnownLine known={known} />
                </li>
              ))}
            </ul>
          </>
        )}

        <section className="start-image">
          <h3>Or open a disk image</h3>
          <p>A raw image of an exFAT partition, or a device by its path.</p>
          <form
            onSubmit={(event) => {
              event.preventDefault();
              onOpen(path.trim());
            }}
          >
            <input value={path} onChange={(event) => setPath(event.target.value)} placeholder="/path/to/disk.img" spellCheck={false} aria-label="Disk image or device path" />
            <button type="button" onClick={() => void invoke<string | null>("choose_disk_image").then((chosen) => chosen && setPath(chosen))}>Choose a file</button>
            <button type="submit" className={path.trim() ? "primary" : undefined} disabled={!path.trim()}>Open</button>
          </form>
        </section>

        <KeepSafe volumes={volumes} diskSleep={report?.diskSleepMinutes ?? null} />

        {report && volumes.length === 0 && (
          <p className="start-warning">
            macOS mounts a disk as soon as it is plugged in, and can write to it while it is mounted. Before you plug in a damaged disk, stop that with an <code>/etc/fstab</code> line. The README shows how.
          </p>
        )}

      </section>
    </main>
  );
}
