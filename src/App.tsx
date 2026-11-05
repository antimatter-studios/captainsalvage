import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  call,
  type BasketItem,
  type BrowseRow,
  type ClusterInfo,
  type DiskAccess,
  type Extent,
  type IslandRow,
  type Layer,
  type Lens,
  type Overview,
  type Row,
  type Selection,
  type Status,
  type UnexplainedRun,
} from "./api";
import { useColumnWidths } from "./columns";
import { BrowseView, type BrowseScope } from "./components/BrowseView";
import { Chat } from "./components/Chat";
import { ColumnHandle } from "./components/ColumnHandle";
import { Details } from "./components/Details";
import { Explorer } from "./components/Explorer";
import { HexView } from "./components/HexView";
import { Icon, type IconName } from "./components/icons";
import { RecoveryOverview } from "./components/RecoveryOverview";
import { Command } from "./components/Command";
import { StartScreen } from "./components/StartScreen";
import { isZipName, ZipView } from "./components/ZipView";
import { Strips } from "./components/Strips";
import { bytes, count, duration } from "./format";
import { onThemeChange } from "./palette";
import appIcon from "../src-tauri/icons/64x64.png";

const STATUS_POLL_MILLIS = 2000;
/** While a scan runs, the content layers keep filling in at this pace. */
const MAP_REFRESH_DURING_SCAN_MILLIS = 15000;
/** While a scan runs, the tree and totals are rebuilt from what it has found. */
const REBUILD_DURING_SCAN_MILLIS = 5 * 60 * 1000;

type CenterView = "overview" | "browse" | "bytes" | "zip";
const DISK_ACCESS_CHECK_MS = 3000;

function asBrowseRow(row: Row): BrowseRow {
  return { ...row, id: row.id as number };
}

/** Says plainly that the disk has gone, so failed reads are not taken for damaged files. */
function DiskGone({ status, onOpen }: { status: Status; onOpen: (path: string) => void }) {
  const back = status.diskBack;
  const [access, setAccess] = useState<DiskAccess | null>(null);
  useEffect(() => {
    setAccess(null);
    if (!back || back.mounted) return;
    const check = () => {
      call<DiskAccess>("disk_access", { path: back.device }).then(setAccess).catch(() => undefined);
    };
    check();
    const timer = window.setInterval(check, DISK_ACCESS_CHECK_MS);
    return () => window.clearInterval(timer);
  }, [back?.device, back?.mounted]);
  // Devices that are there but this account cannot read. One command fixes them all.
  const unreadable = access?.devices.filter((device) => device.exists && !device.readable).map((device) => device.path) ?? [];

  return (
    <div className="disk-gone" role="alert">
      <strong>The disk has been disconnected.</strong>
      {!back && <p>Files cannot be read until it is back, so a file that will not open now is not necessarily damaged. If you plug it back in, macOS may mount it and write to it.</p>}
      {back?.mounted && (
        <>
          <p>It is back as {back.device}, and macOS has mounted it, so macOS may be writing to it. Unmount it now, in Terminal:</p>
          <Command text={`diskutil unmount ${back.device.replace("/dev/rdisk", "/dev/disk")}`} />
        </>
      )}
      {back && !back.mounted && (
        <>
          <p>It is back as {back.device}, and it is not mounted.</p>
          {unreadable.length > 0 ? (
            <>
              <p>To read it again, give this account read-only access in Terminal. It lasts until the disk is reconnected.</p>
              <Command text={`sudo chgrp staff ${unreadable.join(" ")}`} />
            </>
          ) : (
            access?.instructions.map((text) => <p key={text} className="instruction">{text}</p>)
          )}
          <button className="primary" disabled={!access?.ready} onClick={() => onOpen(back.device)}>Open it again</button>
        </>
      )}
    </div>
  );
}

/**
 * One calm chip while the disk is safe. Any problem turns it red and opens the list once, so it
 * is seen. The list then closes like any popover, and the chip stays red while the problem lasts.
 */
function Safety({ status }: { status: Status }) {
  const { now: counted, sinceOpen } = status.writes;
  const mountedAgain = status.diskBack?.mounted ?? false;
  const baselineDate = new Date(status.baseline.recordedAtMillis).toLocaleDateString();
  const checks = [
    { good: !status.diskGone, text: "The disk is connected", danger: "The disk has been disconnected" },
    { good: status.osDeniesWrites, text: "The OS denies writes to this device", danger: "This account has write permission on the device" },
    { good: status.mounted === null && !mountedAgain, text: status.diskGone ? "The volume is not mounted, as far as can be seen" : "The volume is not mounted", danger: "Mounted, so macOS may write to it. Unmount it now" },
    { good: status.baseline.unchanged, text: `Boot, FAT, bitmap and root are unchanged since ${baselineDate}`, danger: `Structures have changed since the baseline of ${baselineDate}` },
    { good: status.reads.errors === 0, text: "No read errors", danger: `${count(status.reads.errors)} read errors` },
    { good: status.keepAwake, text: "macOS will not put disks to sleep while this app is open", danger: "macOS may put this disk to sleep, and it can drop off when woken" },
    ...(counted
      ? [{
          good: sinceOpen.operations === 0,
          text: "No writes counted on this disk since it was opened here",
          danger: `${count(sinceOpen.operations)} writes (${bytes(sinceOpen.bytes)}) counted on this disk since it was opened here. Something other than this app is writing to it`,
        }]
      : []),
  ];
  const safe = checks.every((check) => check.good);
  // Failing checks by their fixed wording, so a count that keeps growing does not reopen the list.
  const failing = checks.filter((check) => !check.good).map((check) => check.text);
  const failingKey = failing.join("\n");
  const [open, setOpen] = useState(false);
  const chip = useRef<HTMLDivElement>(null);
  const seen = useRef<string[]>([]);
  // Opens when a check newly fails. failingKey stands in for failing, a new array every render.
  useEffect(() => {
    if (failing.some((text) => !seen.current.includes(text))) setOpen(true);
    seen.current = failing;
  }, [failingKey]);
  useEffect(() => {
    if (!open) return;
    const away = (event: PointerEvent) => {
      if (!chip.current?.contains(event.target as Node)) setOpen(false);
    };
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpen(false);
    };
    document.addEventListener("pointerdown", away);
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("pointerdown", away);
      document.removeEventListener("keydown", escape);
    };
  }, [open]);
  return (
    <div ref={chip} className={safe ? "safety-chip" : "safety-chip safety-chip-bad"}>
      <button
        className="safety-button"
        aria-expanded={open}
        aria-controls="safety-list"
        aria-label={safe ? "Disk protected. Show the checks" : "Disk safety problem. Show the checks"}
        onClick={() => setOpen((shown) => !shown)}
      >
        <Icon name={safe ? "shield" : "alert"} size={15} /> {safe ? "Disk protected" : "Check disk safety"}
      </button>
      {open && <div id="safety-list" className="safety-list">
        {checks.map((check) => (
          <div key={check.text} className={check.good ? "safety-item" : "safety-item safety-item-bad"}>
            <span className="safety-mark" aria-hidden="true">{check.good ? "✓" : "!"}</span>
            {check.good ? check.text : check.danger}
          </div>
        ))}
        <p className="safety-note">The baseline only covers changes after it was recorded.</p>
        {counted && (
          <p className="safety-note">
            macOS has counted {count(counted.operations)} writes ({bytes(counted.bytes)}) on this disk since it was attached. That covers the whole disk and has no dates, so it is evidence and not proof.
          </p>
        )}
      </div>}
    </div>
  );
}

type ThemeChoice = "system" | "light" | "dark";
const THEME_CHOICES: { id: ThemeChoice; label: string; icon: IconName }[] = [
  { id: "system", label: "Match macOS", icon: "auto" },
  { id: "light", label: "Light", icon: "sun" },
  { id: "dark", label: "Dark", icon: "moon" },
];

function ThemeSwitch() {
  const [choice, setChoice] = useState<ThemeChoice>("system");
  useEffect(() => {
    invoke<ThemeChoice>("get_theme").then(setChoice).catch(() => undefined);
  }, []);
  return (
    <div className="segmented theme-switch" role="radiogroup" aria-label="Appearance">
      {THEME_CHOICES.map((item) => (
        <button
          key={item.id}
          role="radio"
          aria-checked={choice === item.id}
          aria-selected={choice === item.id}
          aria-label={item.label}
          title={item.label}
          onClick={() => {
            setChoice(item.id);
            void invoke("set_theme", { theme: item.id });
          }}
        >
          <Icon name={item.icon} size={14} />
        </button>
      ))}
    </div>
  );
}

/**
 * The top of the sidebar, beside the window controls: the app's name, and a way home to the
 * overview. The row is part of the window's drag area, so only the button takes clicks.
 */
function SidebarBrand({ onHome }: { onHome: () => void }) {
  const [version, setVersion] = useState<string | null>(null);
  useEffect(() => {
    getVersion().then(setVersion).catch(() => undefined);
  }, []);
  return (
    <div className="sidebar-brand">
      <img className="sidebar-brand-icon" src={appIcon} alt="" width={20} height={20} />
      <strong>Captain Salvage</strong>
      {version && <span className="brand-version">v{version}</span>}
      <button className="sidebar-home" onClick={onHome} aria-label="Go to the overview" title="Overview">
        <Icon name="home" size={16} />
      </button>
    </div>
  );
}

function Toolbar({ status, overview, onChanged, onChangeDisk }: { onChangeDisk: () => void; status: Status | null; overview: Overview; onChanged: () => void }) {
  const scan = status?.scan;
  const fraction = scan ? scan.clustersScanned / scan.totalClusters : 0;
  const running = Boolean(status && (status.scanInThisApp || status.scanHeldElsewhere));
  const left = scan && scan.bytesPerSecond > 0 ? ((scan.totalClusters - scan.clustersScanned) * overview.geometry.clusterSize) / scan.bytesPerSecond : Infinity;

  let scanText = "Scan not started";
  if (scan?.status === "complete") scanText = "Scan complete";
  else if (scan?.status === "resolving") scanText = "Building the map";
  else if (scan?.status === "measuring-text" && running) scanText = "Checking text files found so far";
  else if (scan && running) {
    scanText = `Scan ${(fraction * 100).toFixed(1)}%${scan.bytesPerSecond > 0 ? `, ${bytes(scan.bytesPerSecond)}/s, ${duration(left)} left` : ""}`;
  } else if (scan && scan.clustersScanned > 0) scanText = `Scan paused at ${(fraction * 100).toFixed(1)}%`;

  return (
    <header className="toolbar" data-tauri-drag-region>
      <div className="brand">
        <strong>{overview.device}</strong>
        <span>{bytes(overview.geometry.volumeBytes)} <button className="link brand-change" onClick={onChangeDisk}>Change disk</button></span>
      </div>
      <div className="scan">
        <div className="scan-text" title={scan ? `${count(scan.directoryClusters)} directory clusters and ${count(scan.setsFound)} entries found so far` : undefined}>{scanText}</div>
        <div className="scan-bar" role="progressbar" aria-valuenow={Math.round(fraction * 100)} aria-valuemin={0} aria-valuemax={100}>
          <div className="scan-fill" style={{ width: `${fraction * 100}%` }} />
        </div>
        {status?.scanError && <div className="warn">{status.scanError}</div>}
      </div>
      <div className="toolbar-spacer" data-tauri-drag-region />
      {status?.fileTests.state === "running" && (
        <span className="scan-note muted">Testing {count(status.fileTests.done)} of {count(status.fileTests.total)}</span>
      )}
      {status?.copy.state === "running" && (
        <span className="scan-note muted">Copying {count(status.copy.filesDone + status.copy.filesSkipped)} of {count(status.copy.filesTotal)}</span>
      )}
      {status && scan?.status !== "complete" && (
        status.scanInThisApp ? (
          <button onClick={() => call("stop_scan").then(onChanged)}>Pause scan</button>
        ) : status.scanHeldElsewhere ? (
          <span className="muted scan-note" title="The scan runs in a separate process. This window follows its progress.">Scanning in another process</span>
        ) : (
          <button onClick={() => call("start_scan").then(onChanged)}>{scan && scan.clustersScanned > 0 ? "Resume scan" : "Start scan"}</button>
        )
      )}
      <ThemeSwitch />
      {status && <Safety status={status} />}
    </header>
  );
}

export default function App() {
  const [overview, setOverview] = useState<Overview | null>(null);
  const [startPath, setStartPath] = useState<string | null>(null);
  const [choosing, setChoosing] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const [status, setStatus] = useState<Status | null>(null);
  const [layer, setLayer] = useState<Layer>("ownership");
  const [floor, setFloor] = useState(0);
  const [lens, setLens] = useState<Lens | null>(null);
  const [view, setView] = useState<[number, number]>([2, 3]);
  const [revision, setRevision] = useState(0);
  const [activityRevision, setActivityRevision] = useState(0);
  const [cluster, setCluster] = useState<number | null>(null);
  const [info, setInfo] = useState<ClusterInfo | null>(null);
  const [infoError, setInfoError] = useState<string | null>(null);
  const [entry, setEntry] = useState<{ row: Row; extents: Extent[] } | null>(null);
  const [folder, setFolder] = useState<Row | null>(null);
  const [chain, setChain] = useState<IslandRow | null>(null);
  const [region, setRegion] = useState<UnexplainedRun | null>(null);
  const [highlight, setHighlight] = useState<Extent[]>([]);
  const [byteRange, setByteRange] = useState<[number, number] | null>(null);
  const [basket, setBasket] = useState<BasketItem[]>([]);
  const [tabRequest, setTabRequest] = useState<{ tab: "lens"; at: number } | null>(null);
  const [center, setCenter] = useState<CenterView>("overview");
  const [browseScope, setBrowseScope] = useState<BrowseScope | null>(null);
  const [standalone, setStandalone] = useState<BrowseRow | null>(null);
  const [previewInCenter, setPreviewInCenter] = useState<number | null>(null);
  const columns = useColumnWidths();
  const [, setThemeTick] = useState(0);
  const centerNow = useRef<CenterView>("overview");
  centerNow.current = center;

  const bump = useCallback(() => setRevision((value) => value + 1), []);

  // Legends and the overview's bar read their colours while drawing, so redraw them.
  useEffect(() => onThemeChange(() => setThemeTick((tick) => tick + 1)), []);

  const loadBasket = useCallback(() => {
    call<BasketItem[]>("basket_list").then(setBasket).catch(() => undefined);
  }, []);

  const changeBasket = useCallback((kind: "entry" | "folder", target: number, add: boolean) => {
    const existing = basket.find((item) => item.kind === kind && item.target === target);
    if (add && !existing) void call("basket_add", { kind, target });
    if (!add && existing) void call("basket_remove", { id: existing.id });
  }, [basket]);

  const basketKeys = useMemo(() => new Set(basket.map((item) => `${item.kind}:${item.target}`)), [basket]);

  const open = useCallback(async (path: string) => {
    setFailure(null);
    setStartPath(path);
    try {
      const result = await call<Overview>("open_volume", { path });
      setOverview(result);
      setView([2, result.geometry.clusterCount + 2]);
      setChoosing(false);
      setEntry(null);
      setFolder(null);
      setChain(null);
      setRegion(null);
      setStandalone(null);
      setBrowseScope(null);
      setCluster(null);
      setCenter("overview");
      bump();
    } catch (reason) {
      setFailure(String(reason));
    }
  }, []);

  useEffect(() => {
    invoke<string | null>("last_device")
      .then((path) => {
        // Never reopens by itself: disk numbers change, so the same path can be another disk.
        setStartPath(path && !path.startsWith("/dev/") ? path : "");
      })
      .catch(() => setStartPath(""));
  }, [open]);

  useEffect(() => {
    if (!overview) return;
    let active = true;
    const poll = () => call<Status>("status").then((value) => active && setStatus(value)).catch(() => undefined);
    void poll();
    const timer = setInterval(poll, STATUS_POLL_MILLIS);
    return () => {
      active = false;
      clearInterval(timer);
    };
  }, [overview]);

  // Finished tests change scores and folder totals, so everything shown is read again.
  const testState = status?.fileTests.state;
  const lastTestState = useRef(testState);
  useEffect(() => {
    if (lastTestState.current === "running" && testState !== "running") bump();
    lastTestState.current = testState;
  }, [testState, bump]);

  const scanning = Boolean(status && (status.scanInThisApp || status.scanHeldElsewhere));
  useEffect(() => {
    if (!scanning) return;
    const timer = setInterval(bump, MAP_REFRESH_DURING_SCAN_MILLIS);
    return () => clearInterval(timer);
  }, [scanning, bump]);

  useEffect(() => {
    if (!scanning) return;
    const rebuild = () => void call("resolve").then(bump).catch(() => undefined);
    rebuild();
    const timer = setInterval(rebuild, REBUILD_DURING_SCAN_MILLIS);
    return () => clearInterval(timer);
  }, [scanning, bump]);

  const moveHexTo = useCallback((target: number) => {
    setCluster(target);
    setByteRange(null);
  }, []);

  const showCluster = useCallback((target: number) => {
    moveHexTo(target);
    setStandalone(null);
    setCenter("bytes");
  }, [moveHexTo]);

  /** Selects a file for the details and for Claude, without changing what the middle shows. */
  const noteEntry = useCallback(async (id: number) => {
    const result = await call<{ entry: Row; extents: Extent[] }>("entry", { id });
    setEntry({ row: result.entry, extents: result.extents });
    setFolder(null);
    setChain(null);
    setRegion(null);
    setHighlight(result.extents);
    if (result.entry.firstCluster >= 2) moveHexTo(result.entry.firstCluster);
    return result.entry;
  }, [moveHexTo]);

  const selectEntry = useCallback(async (row: Row) => {
    if (row.id === null) return;
    await noteEntry(row.id);
    setCenter("bytes");
  }, [noteEntry]);

  /** A file picked in the sidebar opens as itself, unless the bytes are already showing. */
  const openEntry = useCallback(async (row: Row) => {
    if (row.id === null) return;
    const full = await noteEntry(row.id);
    if (centerNow.current === "bytes") return;
    if (isZipName(full.name)) {
      setCenter("zip");
      return;
    }
    setStandalone(asBrowseRow(full));
    setCenter("browse");
  }, [noteEntry]);

  const selectFolder = useCallback(async (row: Row) => {
    const head = row.head ?? row.firstCluster;
    const full = (await call<Row | null>("node", { head })) ?? row;
    setFolder(full);
    setEntry(null);
    setChain(null);
    setRegion(null);
    setHighlight([{ first: head, count: 1 }]);
    moveHexTo(head);
    setCenter("bytes");
  }, [moveHexTo]);

  /** A folder picked in the sidebar shows its files, unless the bytes are already showing. */
  const openFolder = useCallback(async (row: Row) => {
    const head = row.head ?? row.firstCluster;
    const full = (await call<Row | null>("node", { head })) ?? row;
    setFolder(full);
    setEntry(null);
    setChain(null);
    setRegion(null);
    setHighlight([{ first: head, count: 1 }]);
    moveHexTo(head);
    setStandalone(null);
    setBrowseScope({ head, name: full.name || `Lost folder ${count(head)}` });
    if (centerNow.current !== "bytes") setCenter("browse");
  }, [moveHexTo]);

  const selectChain = useCallback(async (item: IslandRow) => {
    setChain(item);
    setEntry(null);
    setFolder(null);
    setRegion(null);
    const result = await call<{ extents: Extent[] }>("follow_chain", { cluster: item.head, max: item.length });
    setHighlight(result.extents);
    showCluster(item.head);
  }, [showCluster]);

  const previewEntry = useCallback((row: Row) => {
    setStandalone(asBrowseRow(row));
    setCenter("browse");
  }, []);

  const browseFolder = useCallback((row: Row) => {
    setBrowseScope({ head: row.head ?? row.firstCluster, name: row.name || `Lost folder ${count(row.head ?? row.firstCluster)}` });
    setStandalone(null);
    setCenter("browse");
  }, []);

  const selectRegion = useCallback((run: UnexplainedRun) => {
    setRegion(run);
    setEntry(null);
    setFolder(null);
    setChain(null);
    setHighlight([{ first: run.startCluster, count: run.clusters }]);
    const margin = Math.max(Math.floor(run.clusters / 4), 32);
    setView([Math.max(run.startCluster - margin, 2), run.endCluster + margin]);
    showCluster(run.startCluster);
  }, [showCluster]);

  const pickCluster = useCallback((target: number) => {
    setEntry(null);
    setFolder(null);
    setChain(null);
    setRegion(null);
    setHighlight([]);
    showCluster(target);
  }, [showCluster]);

  // Claude can move the view with show_in_app, and changes reach the map at once.
  useEffect(() => {
    const stops = [
      listen("lens-map-changed", bump),
      listen("lens-basket-changed", loadBasket),
      listen("lens-activity", () => setActivityRevision((value) => value + 1)),
      listen<{ kind: string; value?: number; start?: number; end?: number }>("lens-focus", async ({ payload }) => {
        if (payload.kind === "cluster" && payload.value !== undefined) {
          pickCluster(payload.value);
          const half = 256;
          setView((current) => (payload.value! < current[0] || payload.value! >= current[1] ? [Math.max(2, payload.value! - half), payload.value! + half] : current));
        } else if (payload.kind === "entry" && payload.value !== undefined) {
          void selectEntry({ id: payload.value } as Row);
        } else if (payload.kind === "folder" && payload.value !== undefined) {
          void selectFolder({ head: payload.value, firstCluster: payload.value } as Row);
        } else if (payload.kind === "range" && payload.start !== undefined && payload.end !== undefined) {
          setView([payload.start, payload.end]);
        } else if (payload.kind === "lens" && payload.value !== undefined) {
          const overlays = await call<{ id: number; payload: Lens }[]>("list_overlays");
          const found = overlays.find((item) => item.id === payload.value);
          if (found) {
            setLens(found.payload);
            setLayer("lens");
          }
        }
      }),
    ];
    return () => {
      stops.forEach((stop) => stop.then((unlisten) => unlisten()));
    };
  }, [bump, loadBasket, pickCluster, selectEntry, selectFolder]);

  useEffect(() => {
    if (overview) loadBasket();
  }, [overview, revision, loadBasket]);

  useEffect(() => {
    if (cluster === null) return;
    let current = true;
    setInfoError(null);
    call<ClusterInfo>("inspect_cluster", { cluster })
      .then((value) => current && setInfo(value))
      .catch((reason) => {
        if (current) {
          setInfo(null);
          setInfoError(String(reason));
        }
      });
    return () => {
      current = false;
    };
  }, [cluster, revision]);

  const selection: Selection = useMemo(() => {
    if (entry && entry.row.id !== null) {
      const { id, name, path, firstCluster, verdict, confidence } = entry.row;
      if (!byteRange) return { kind: "entry", id: id as number, name, path, firstCluster, verdict, confidence };
    }
    if (folder && !byteRange) return { kind: "folder", head: folder.head ?? folder.firstCluster, name: folder.name, path: folder.path };
    if (chain && !byteRange) return { kind: "chain", head: chain.head, length: chain.length, headClassName: chain.headClassName };
    if (region && !byteRange) return { kind: "region", startCluster: region.startCluster, endCluster: region.endCluster, mainContent: region.mainContent };
    if (cluster !== null) {
      return byteRange ? { kind: "cluster", cluster, byteStart: byteRange[0], byteEnd: byteRange[1] } : { kind: "cluster", cluster };
    }
    return { kind: "none" };
  }, [entry, folder, chain, region, cluster, byteRange]);

  // The AI reads this through get_selection, whichever client it connects from.
  useEffect(() => {
    if (!overview) return;
    void call("set_selection", { selection, layer, mapView: { startCluster: view[0], endCluster: view[1] }, focusFloor: floor, lens, hexCluster: cluster });
  }, [overview, selection, layer, view, floor, lens, cluster]);

  if (!overview || choosing) {
    return (
      <StartScreen
        imagePath={startPath ?? ""}
        failure={failure}
        onOpen={(path) => void open(path)}
        onBack={overview ? () => setChoosing(false) : undefined}
        themeSwitch={<ThemeSwitch />}
      />
    );
  }

  const zipEntry = entry && entry.row.kind === "file" && isZipName(entry.row.name) ? entry.row : null;
  const selectedKey = entry ? `entry:${entry.row.id}` : folder ? `folder:${folder.head}` : chain ? `chain:${chain.head}` : region ? `region:${region.startCluster}` : null;

  return (
    <div
      className="app"
      style={{ gridTemplateColumns: columns.columns, "--left-width": `${columns.widths.left}px` } as React.CSSProperties}
    >
      <div className="sidebar-drag" data-tauri-drag-region>
        <SidebarBrand onHome={() => setCenter("overview")} />
      </div>
      <Toolbar status={status} overview={overview} onChanged={bump} onChangeDisk={() => setChoosing(true)} />
      <Strips
        layer={layer}
        onLayer={(next) => {
          setLayer(next);
          if (next === "lens") setTabRequest({ tab: "lens", at: Date.now() });
        }}
        floor={floor}
        onFloor={setFloor}
        lens={lens}
        revision={revision}
        totalClusters={overview.geometry.clusterCount}
        clusterSize={overview.geometry.clusterSize}
        view={view}
        onView={setView}
        highlight={highlight}
        marker={cluster}
        onPick={pickCluster}
      />
      <Explorer
        floor={floor}
        revision={revision}
        selectedKey={selectedKey}
        openKey={browseScope ? `folder:${browseScope.head}` : null}
        onEntry={openEntry}
        onFolder={openFolder}
        onChain={selectChain}
        onRegion={selectRegion}
        basket={basketKeys}
        onBasket={changeBasket}
        classNames={overview.classNames}
        clusterSize={overview.geometry.clusterSize}
        handle={<ColumnHandle label="Resize the sidebar" edge="left-panel" onWidth={columns.setLeft} onReset={columns.resetLeft} />}
      />
      <div className="center-stack">
        {status?.diskGone && <DiskGone status={status} onOpen={(path) => void open(path).then(bump)} />}
        <div className="center-tabs">
          <div className="segmented" role="tablist">
            <button role="tab" aria-selected={center === "overview"} onClick={() => setCenter("overview")}>Overview</button>
            <button role="tab" aria-selected={center === "browse"} onClick={() => setCenter("browse")}>Browse files</button>
            <button role="tab" aria-selected={center === "bytes"} disabled={cluster === null} onClick={() => setCenter("bytes")}>Bytes</button>
            <button role="tab" aria-selected={center === "zip"} disabled={!zipEntry} onClick={() => setCenter("zip")}>Inside zip</button>
          </div>
        </div>
        {center === "zip" && zipEntry?.id != null && <ZipView id={zipEntry.id} name={zipEntry.name} onBack={() => setCenter("browse")} />}
        {center === "zip" && !zipEntry && <section className="center-panel"><p className="muted zip-message">Choose a zip file to see inside it.</p></section>}
        {center === "overview" && <RecoveryOverview overview={overview} status={status} revision={revision} onChanged={bump} />}
        {center === "browse" && (
          <BrowseView
            floor={floor}
            revision={revision}
            scope={browseScope}
            onScopeClear={() => setBrowseScope(null)}
            standalone={standalone}
            onStandaloneClose={() => setStandalone(null)}
            basket={basketKeys}
            onBasket={changeBasket}
            onSelect={(row) => void noteEntry(row.id)}
            onBytes={showCluster}
            onInsideZip={(row) => void noteEntry(row.id).then(() => setCenter("zip"))}
            onPreviewShown={setPreviewInCenter}
          />
        )}
        {center === "bytes" && cluster !== null && (
          <HexView
            cluster={cluster}
            info={info}
            infoError={infoError}
            clusterSize={overview.geometry.clusterSize}
            heapOffset={overview.geometry.heapOffset}
            lastCluster={overview.geometry.clusterCount + 2}
            byteRange={byteRange}
            onByteRange={setByteRange}
            onGo={showCluster}
            onClose={() => {
              setCluster(null);
              setCenter("overview");
            }}
          />
        )}
      </div>
      <div className="side">
        <ColumnHandle label="Resize the details panel" edge="right-panel" onWidth={columns.setRight} onReset={columns.resetRight} />
        <Details
          overview={overview}
          status={status}
          entry={entry}
          folder={folder}
          info={cluster === null ? null : info}
          revision={revision}
          activityRevision={activityRevision}
          lens={lens}
          onLens={setLens}
          onCluster={showCluster}
          onEntry={selectEntry}
          onChanged={bump}
          tabRequest={tabRequest}
          floor={floor}
          basket={basket}
          onBasket={changeBasket}
          onPreview={previewEntry}
          onInsideZip={() => setCenter("zip")}
          onBrowseFolder={browseFolder}
          previewInCenter={previewInCenter}
        />
        <Chat selection={selection} />
      </div>
    </div>
  );
}
