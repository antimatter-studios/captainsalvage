import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useMemo, useState } from "react";
import {
  call,
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
import { Chat } from "./components/Chat";
import { Details } from "./components/Details";
import { Explorer } from "./components/Explorer";
import { HexView } from "./components/HexView";
import { Icon } from "./components/icons";
import { RecoveryOverview } from "./components/RecoveryOverview";
import { Strips } from "./components/Strips";
import { bytes, count, duration } from "./format";

const STATUS_POLL_MILLIS = 2000;
/** While a scan runs, the content layers keep filling in at this pace. */
const MAP_REFRESH_DURING_SCAN_MILLIS = 15000;

function OpenVolume({ initialPath, failure, onOpen }: { initialPath: string; failure: string | null; onOpen: (path: string) => void }) {
  const [path, setPath] = useState(initialPath);
  const [access, setAccess] = useState<DiskAccess | null>(null);
  const [disks, setDisks] = useState<string | null>(null);

  useEffect(() => {
    if (failure && path) call<DiskAccess>("disk_access", { path }).then(setAccess).catch(() => undefined);
  }, [failure, path]);

  return (
    <main className="open">
      <h1>Open a damaged exFAT volume</h1>
      <p>This app only reads. It opens the device read-only, and nothing in it can write to the disk.</p>
      <form
        onSubmit={(event) => {
          event.preventDefault();
          onOpen(path.trim());
        }}
      >
        <label className="field">
          <span>Partition device or disk image</span>
          <input value={path} onChange={(event) => setPath(event.target.value)} placeholder="/dev/rdisk12s2" spellCheck={false} />
        </label>
        <div className="detail-actions">
          <button className="primary" type="submit" disabled={!path.trim()}>Open volume</button>
          <button
            type="button"
            onClick={() => call<{ output: string }>("list_disks").then((result) => setDisks(result.output))}
          >
            List disks
          </button>
        </div>
      </form>
      {failure && <p className="warn">{failure}</p>}
      {access?.instructions.map((text) => <p key={text} className="instruction">{text}</p>)}
      {disks && <pre className="preview-text">{disks}</pre>}
    </main>
  );
}

/** One calm chip while the disk is safe. Any problem turns it red and opens the list. */
function Safety({ status }: { status: Status }) {
  const checks = [
    { good: status.osDeniesWrites, text: "The OS denies writes to this device", danger: "This account has write permission on the device" },
    { good: status.mounted === null, text: "The volume is not mounted", danger: "Mounted, so macOS may write to it. Unmount it now" },
    { good: status.baseline.unchanged, text: "Boot, FAT, bitmap and root are unchanged", danger: "Structures have changed since the baseline" },
    { good: status.reads.errors === 0, text: "No read errors", danger: `${count(status.reads.errors)} read errors` },
  ];
  const safe = checks.every((check) => check.good);
  return (
    <div className={safe ? "safety-chip" : "safety-chip safety-chip-bad"}>
      <button className="safety-button" aria-label={safe ? "Disk protected. Show the checks" : "Disk safety problem"}>
        <Icon name={safe ? "shield" : "alert"} size={15} /> {safe ? "Disk protected" : "Check disk safety"}
      </button>
      <div className="safety-list">
        {checks.map((check) => (
          <div key={check.text} className={check.good ? "safety-item" : "safety-item safety-item-bad"}>
            <span className="safety-mark" aria-hidden="true">{check.good ? "✓" : "!"}</span>
            {check.good ? check.text : check.danger}
          </div>
        ))}
      </div>
    </div>
  );
}

function Toolbar({ status, overview, onChanged }: { status: Status | null; overview: Overview; onChanged: () => void }) {
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
        <strong>exFAT Lens</strong>
        <span>{overview.device}, {bytes(overview.geometry.volumeBytes)}</span>
      </div>
      <div className="scan">
        <div className="scan-text" title={scan ? `${count(scan.directoryClusters)} directory clusters and ${count(scan.setsFound)} entries found so far` : undefined}>{scanText}</div>
        <div className="scan-bar" role="progressbar" aria-valuenow={Math.round(fraction * 100)} aria-valuemin={0} aria-valuemax={100}>
          <div className="scan-fill" style={{ width: `${fraction * 100}%` }} />
        </div>
        {status?.scanError && <div className="warn">{status.scanError}</div>}
      </div>
      <div className="toolbar-spacer" data-tauri-drag-region />
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
      {status && <Safety status={status} />}
    </header>
  );
}

export default function App() {
  const [overview, setOverview] = useState<Overview | null>(null);
  const [startPath, setStartPath] = useState<string | null>(null);
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
  const [tabRequest, setTabRequest] = useState<{ tab: "lens"; at: number } | null>(null);

  const bump = useCallback(() => setRevision((value) => value + 1), []);

  const open = useCallback(async (path: string) => {
    setFailure(null);
    setStartPath(path);
    try {
      const result = await call<Overview>("open_volume", { path });
      setOverview(result);
      setView([2, result.geometry.clusterCount + 2]);
    } catch (reason) {
      setFailure(String(reason));
    }
  }, []);

  useEffect(() => {
    invoke<string | null>("last_device")
      .then((path) => {
        if (path) void open(path);
        else setStartPath("");
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

  const scanning = Boolean(status && (status.scanInThisApp || status.scanHeldElsewhere));
  useEffect(() => {
    if (!scanning) return;
    const timer = setInterval(bump, MAP_REFRESH_DURING_SCAN_MILLIS);
    return () => clearInterval(timer);
  }, [scanning, bump]);

  const showCluster = useCallback((target: number) => {
    setCluster(target);
    setByteRange(null);
  }, []);

  const selectEntry = useCallback(async (row: Row) => {
    if (row.id === null) return;
    const result = await call<{ entry: Row; extents: Extent[] }>("entry", { id: row.id });
    setEntry({ row: result.entry, extents: result.extents });
    setFolder(null);
    setChain(null);
    setRegion(null);
    setHighlight(result.extents);
    if (result.entry.firstCluster >= 2) showCluster(result.entry.firstCluster);
  }, [showCluster]);

  const selectFolder = useCallback(async (row: Row) => {
    const head = row.head ?? row.firstCluster;
    const full = (await call<Row | null>("node", { head })) ?? row;
    setFolder(full);
    setEntry(null);
    setChain(null);
    setRegion(null);
    setHighlight([{ first: head, count: 1 }]);
    showCluster(head);
  }, [showCluster]);

  const selectChain = useCallback(async (item: IslandRow) => {
    setChain(item);
    setEntry(null);
    setFolder(null);
    setRegion(null);
    const result = await call<{ extents: Extent[] }>("follow_chain", { cluster: item.head, max: item.length });
    setHighlight(result.extents);
    showCluster(item.head);
  }, [showCluster]);

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
  }, [bump, pickCluster, selectEntry, selectFolder]);

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

  if (!overview) {
    if (startPath === null) return <main className="open"><p>Opening.</p></main>;
    return <OpenVolume initialPath={startPath} failure={failure} onOpen={open} />;
  }

  const selectedKey = entry ? `entry:${entry.row.id}` : folder ? `folder:${folder.head}` : chain ? `chain:${chain.head}` : region ? `region:${region.startCluster}` : null;

  return (
    <div className="app">
      <div className="sidebar-drag" data-tauri-drag-region />
      <Toolbar status={status} overview={overview} onChanged={bump} />
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
        onEntry={selectEntry}
        onFolder={selectFolder}
        onChain={selectChain}
        onRegion={selectRegion}
        classNames={overview.classNames}
        clusterSize={overview.geometry.clusterSize}
      />
      {cluster === null ? (
        <RecoveryOverview overview={overview} status={status} revision={revision} />
      ) : (
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
          onClose={() => setCluster(null)}
        />
      )}
      <div className="side">
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
        />
        <Chat selection={selection} />
      </div>
    </div>
  );
}
