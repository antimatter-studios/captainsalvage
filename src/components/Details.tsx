import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useState } from "react";
import {
  call,
  readEntryBytes,
  type ActivityItem,
  type ClusterInfo,
  type CopyPlan,
  type CopyStatus,
  type Extent,
  type Lens,
  type McpDetails,
  type Overlay,
  type Overview,
  type Parameters,
  type Row,
  type Status,
  type VolumeChoice,
} from "../api";
import { bytes, clock, count, displayPath, percent, stamp, VERDICT_LABEL } from "../format";
import { ConfidenceRing, fileIcon, Icon, verdictTone, type IconName } from "./icons";

const PREVIEW_TYPES: Record<string, string> = {
  mp3: "audio/mpeg", wav: "audio/wav", ogg: "audio/ogg", flac: "audio/flac", m4a: "audio/mp4",
  jpg: "image/jpeg", jpeg: "image/jpeg", png: "image/png", gif: "image/gif", bmp: "image/bmp", webp: "image/webp",
  mp4: "video/mp4", m4v: "video/mp4", mov: "video/quicktime", webm: "video/webm",
};
const MAX_TEXT_PREVIEW = 64 * 1024;

const OVERLAY_LABEL: Record<string, string> = {
  accept_entry: "Accept a damaged entry",
  file_extents: "Reassemble a file",
  folder_parent: "Attach a folder",
  directory_cluster: "Join a directory cluster",
  note: "Note",
  struct: "Custom structure",
  lens: "Lens",
  region_label: "Explain a region",
};

function Fact({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <>
      <dt>{label}</dt>
      <dd>{children}</dd>
    </>
  );
}

function EntryDetails({ row, extents, onCluster, onCopy }: { row: Row; extents: Extent[]; onCluster: (cluster: number) => void; onCopy: () => void }) {
  const [preview, setPreview] = useState<{ type: string; url?: string; text?: string } | null>(null);
  const [report, setReport] = useState<Record<string, any> | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const id = row.id as number;
  const extension = row.name.split(".").pop()?.toLowerCase() ?? "";

  useEffect(() => {
    setPreview(null);
    setReport(null);
    setError(null);
  }, [id]);

  useEffect(() => () => {
    if (preview?.url) URL.revokeObjectURL(preview.url);
  }, [preview]);

  async function loadPreview() {
    setBusy("preview");
    setError(null);
    try {
      const data = await readEntryBytes(id);
      const type = PREVIEW_TYPES[extension];
      if (type) {
        setPreview({ type, url: URL.createObjectURL(new Blob([data.slice()], { type })) });
      } else {
        setPreview({ type: "text", text: new TextDecoder("utf-8", { fatal: false }).decode(data.subarray(0, MAX_TEXT_PREVIEW)) });
      }
    } catch (reason) {
      setError(String(reason));
    }
    setBusy(null);
  }

  async function runTest() {
    setBusy("test");
    setError(null);
    try {
      setReport(await call<Record<string, any>>("test_stream", { entryId: id }));
    } catch (reason) {
      setError(String(reason));
    }
    setBusy(null);
  }

  return (
    <div className="detail">
      <h3 className="detail-name">{row.name}</h3>
      {row.path !== null && <div className="muted detail-path">{displayPath(row.path)}</div>}
      <div className="verdict-line">
        <span className={`verdict-chip ${verdictTone(row.verdict)}`}>{VERDICT_LABEL[row.verdict] ?? row.verdict}</span>
        <span className="confidence-text">Confidence {percent(row.confidence)}</span>
      </div>
      <p>{row.reason}</p>
      {row.kind === "file" && row.length > 0 && (
        <div className="detail-actions">
          <button className="primary" onClick={onCopy}>Copy out</button>
          <button onClick={loadPreview} disabled={busy !== null}>{busy === "preview" ? "Loading" : "Preview"}</button>
          <button onClick={runTest} disabled={busy !== null}>{busy === "test" ? "Testing" : "Test format"}</button>
        </div>
      )}
      <dl className="facts">
        {row.kind === "file" && <Fact label="Size">{bytes(row.length)} ({count(row.length)} bytes)</Fact>}
        <Fact label="First cluster">
          <button className="link" onClick={() => onCluster(row.firstCluster)}>{count(row.firstCluster)}</button>
        </Fact>
        {row.kind === "file" && <Fact label="Clusters">{count(row.clustersFound)} found of {count(row.clustersNeeded)} needed</Fact>}
        {row.clustersAllocated > 0 && <Fact label="In current use">{count(row.clustersAllocated)} clusters</Fact>}
        {row.clustersConflict > 0 && <Fact label="Shared">{count(row.clustersConflict)} clusters</Fact>}
        <Fact label="Pieces">{count(extents.length)}</Fact>
        <Fact label="Layout">{row.noFatChain ? "Contiguous, no FAT chain" : "FAT chain"}</Fact>
        <Fact label="Entry state">{row.inUse ? "In use" : "Marked deleted"}{row.checksumOk ? "" : ", checksum failed"}</Fact>
        <Fact label="Created">{stamp(row.created)}</Fact>
        <Fact label="Modified">{stamp(row.modified)}</Fact>
        {row.sourceCluster !== null && (
          <Fact label="Entry is in">
            <button className="link" onClick={() => onCluster(row.sourceCluster as number)}>cluster {count(row.sourceCluster)}</button>
          </Fact>
        )}
      </dl>
      {error && <p className="warn">{error}</p>}
      {preview?.url && preview.type.startsWith("audio") && <audio controls src={preview.url} className="preview" />}
      {preview?.url && preview.type.startsWith("image") && <img src={preview.url} alt={row.name} className="preview" />}
      {preview?.url && preview.type.startsWith("video") && <video controls src={preview.url} className="preview" />}
      {preview?.text !== undefined && <pre className="preview-text">{preview.text}</pre>}
      {report && (
        <div className="test-report">
          <strong>{report.report.format === "unknown" ? "No format walker for this type" : `${report.report.format.toUpperCase()} structure: ${percent(report.report.validFraction)} valid`}</strong>
          <div>{count(report.report.units)} frames or chunks walked. {report.report.reachedEnd ? "Reached the end." : "Did not reach a clean end."}</div>
          {report.breakLocations.map((item: any, index: number) => (
            <div key={index}>
              Breaks at stream byte {count(item.streamOffset)}
              {item.cluster && (
                <> in <button className="link" onClick={() => onCluster(item.cluster)}>cluster {count(item.cluster)}</button></>
              )}
            </div>
          ))}
          {report.report.notes.map((note: string, index: number) => <div key={index} className="muted">{note}</div>)}
        </div>
      )}
    </div>
  );
}

function ClusterDetails({ info, onCluster, onEntry }: { info: ClusterInfo; onCluster: (cluster: number) => void; onEntry: (row: Row) => void }) {
  return (
    <div className="detail">
      <h3 className="detail-name">Cluster {count(info.cluster)}</h3>
      <dl className="facts">
        <Fact label="Bitmap">{info.allocated ? "In use" : "Marked free"}</Fact>
        <Fact label="FAT entry">{info.fatMeaning}</Fact>
        <Fact label="Chain start">
          {info.chainHead === info.cluster ? "This cluster" : <button className="link" onClick={() => onCluster(info.chainHead)}>{count(info.chainHead)}</button>}
        </Fact>
        {info.predecessor !== null && <Fact label="Reached from"><button className="link" onClick={() => onCluster(info.predecessor as number)}>{count(info.predecessor)}</button></Fact>}
        <Fact label="Content">{info.contentClass}</Fact>
        <Fact label="Entropy">{info.entropyBits.toFixed(2)} bits per byte</Fact>
        <Fact label="Explained by">{info.ownership}</Fact>
        <Fact label="Confidence">{percent(info.confidence)}</Fact>
        <Fact label="Partition offset"><span className="mono">{`0x${info.offset.toString(16)}`}</span></Fact>
        {info.diskOffset !== null && <Fact label="Disk offset"><span className="mono">{`0x${info.diskOffset.toString(16)}`}</span></Fact>}
        {info.sets.length > 0 && <Fact label="Entries here">{count(info.sets.length)}, of which {count(info.sets.filter((set) => !set.checksumOk).length)} damaged</Fact>}
      </dl>
      {info.owners.length > 0 && (
        <>
          <h4>Claimed by</h4>
          {info.owners.slice(0, 12).map((owner) => (
            <button key={owner.id} className="row" onClick={() => onEntry(owner)} title={owner.reason}>
              <span className="row-icon"><Icon name={fileIcon(owner.name)} /></span>
              <span className="row-main"><span className="row-name">{owner.name}</span><span className="row-path">{displayPath(owner.path)}</span></span>
              <ConfidenceRing value={owner.confidence} tone={verdictTone(owner.verdict)} label={`${VERDICT_LABEL[owner.verdict] ?? owner.verdict}, confidence ${percent(owner.confidence)}`} />
            </button>
          ))}
        </>
      )}
      {info.notes.map((note) => <p key={note.id} className="note">{note.note}</p>)}
    </div>
  );
}

function FolderDetails({ row, onCopy }: { row: Row; onCopy: () => void }) {
  return (
    <div className="detail">
      <h3 className="detail-name">{row.name || `Folder with no name, at cluster ${count(row.firstCluster)}`}</h3>
      {row.path !== null && <div className="muted detail-path">{displayPath(row.path)}</div>}
      <dl className="facts">
        <Fact label="Holds">{count(row.files)} files, {bytes(row.bytes)}</Fact>
        <Fact label="Likely intact">{count(row.intactFiles)} files, {bytes(row.intactBytes)}</Fact>
        <Fact label="Place in tree">
          {row.nodeKind === "orphan" ? "Parent not found" : row.nodeKind === "attached" ? "Inside a folder whose parent was not found" : "Reachable from the volume root"}
        </Fact>
        {row.linkConfidence !== null && <Fact label="Link to parent">{percent(row.linkConfidence)}</Fact>}
        {row.hint && <Fact label="Contains">{row.hint}</Fact>}
      </dl>
      <div className="detail-actions">
        <button className="primary" onClick={onCopy}>Copy out this folder</button>
      </div>
    </div>
  );
}

function CopyTab({ entry, folder, status, floor }: { entry: Row | null; folder: Row | null; status: CopyStatus | null; floor: number }) {
  const [volumes, setVolumes] = useState<VolumeChoice[]>([]);
  const [destination, setDestination] = useState("");
  const [minConfidence, setMinConfidence] = useState(Math.max(floor, 0.5));
  const [includePartial, setIncludePartial] = useState(false);
  const [plan, setPlan] = useState<CopyPlan | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [salvage, setSalvage] = useState<{ destination: string | null; freeBytes?: number } | null>(null);

  useEffect(() => {
    Promise.all([call<VolumeChoice[]>("list_volumes"), call<{ destination: string | null; freeBytes?: number }>("get_salvage_drive")])
      .then(([items, saved]) => {
        setVolumes(items);
        setSalvage(saved);
        const usable = items.filter((item) => !item.refused && item.path.startsWith("/Volumes/")).sort((a, b) => b.freeBytes - a.freeBytes);
        const fallback = usable[0] ? `${usable[0].path}/exfat-lens-recovered` : "";
        setDestination((current) => current || saved.destination || fallback);
      })
      .catch((reason) => setError(String(reason)));
  }, []);

  async function rememberSalvageDrive(path: string | null) {
    setError(null);
    try {
      setSalvage(await call<{ destination: string | null; freeBytes?: number }>("set_salvage_drive", path ? { destination: path } : {}));
    } catch (reason) {
      setError(String(reason));
    }
  }

  const selection = entry?.id != null ? { entryIds: [entry.id] } : folder ? { folders: [folder.head ?? folder.firstCluster] } : null;
  const selectionName = entry?.name ?? (folder ? folder.name || `Lost folder, cluster ${count(folder.head ?? folder.firstCluster)}` : null);
  const selectionKey = entry?.id != null ? `e${entry.id}` : folder ? `f${folder.head}` : "";
  useEffect(() => setPlan(null), [selectionKey, destination, minConfidence, includePartial]);

  const request = { ...selection, destination, minConfidence, includePartial };
  const running = status?.state === "running";

  async function check() {
    setError(null);
    try {
      setPlan(await call<CopyPlan>("copy_plan", request));
    } catch (reason) {
      setError(String(reason));
    }
  }

  async function start() {
    setError(null);
    try {
      await call("copy_start", request);
      setPlan(null);
    } catch (reason) {
      setError(String(reason));
    }
  }

  return (
    <div className="detail">
      <p className="muted">Files are read from the damaged disk and written only to the folder you choose. Nothing is ever written to the disk being recovered, and nothing already in the folder is overwritten.</p>
      {selection ? <h3 className="detail-name">{selectionName}</h3> : <p className="empty">Select a file or folder to copy.</p>}
      <label className="field">
        <span>Copy to</span>
        <input value={destination} onChange={(event) => setDestination(event.target.value)} spellCheck={false} placeholder="/Volumes/YourDisk/recovered" />
      </label>
      <div className="volume-list">
        {volumes.map((volume) => (
          <button
            key={volume.path}
            className="link"
            disabled={Boolean(volume.refused)}
            title={volume.refused ?? undefined}
            onClick={() => setDestination(`${volume.path}/exfat-lens-recovered`)}
          >
            {volume.path.split("/").pop()} ({volume.refused ? "not allowed" : `${bytes(volume.freeBytes)} free`})
          </button>
        ))}
      </div>
      <div className="salvage">
        {salvage?.destination ? (
          <span>
            Salvage drive: <span className="mono">{salvage.destination}</span>
            {salvage.freeBytes !== undefined && `, ${bytes(salvage.freeBytes)} free`}.{" "}
            <button className="link" onClick={() => rememberSalvageDrive(null)}>Forget it</button>
          </span>
        ) : (
          <span className="muted">No salvage drive set.</span>
        )}
        {destination.trim() && destination !== salvage?.destination && (
          <button className="link" onClick={() => rememberSalvageDrive(destination.trim())}>Use this folder as the salvage drive</button>
        )}
      </div>
      <label className="field">
        <span>Only files with confidence {percent(minConfidence)} and up</span>
        <input type="range" min={0} max={100} value={Math.round(minConfidence * 100)} onChange={(event) => setMinConfidence(Number(event.target.value) / 100)} />
      </label>
      <label className="check">
        <input type="checkbox" checked={includePartial} onChange={(event) => setIncludePartial(event.target.checked)} />
        Also copy files with missing or reused clusters
      </label>
      <div className="detail-actions">
        <button onClick={check} disabled={!selection || !destination.trim() || running}>Check</button>
        <button className="primary" onClick={start} disabled={!plan || Boolean(plan.refused) || !plan.fits || plan.files === 0 || running}>
          {plan ? `Copy ${count(plan.files)} files` : "Copy"}
        </button>
        {running && <button onClick={() => call("copy_stop")}>Stop</button>}
      </div>
      {error && <p className="warn">{error}</p>}
      {plan && (
        <div className="test-report">
          {plan.refused && <p className="warn"><strong>{plan.refused}</strong></p>}
          <div>{count(plan.files)} files, {bytes(plan.bytes)}. The destination has {bytes(plan.freeBytes)} free.</div>
          {!plan.fits && <div className="warn">This does not fit. Raise the confidence floor, or choose less.</div>}
          {plan.belowFloor > 0 && <div className="muted">{count(plan.belowFloor)} files are below the confidence floor.</div>}
          {plan.skippedPartial > 0 && <div className="muted">{count(plan.skippedPartial)} partial files skipped, {bytes(plan.skippedPartialBytes)}.</div>}
          {plan.largest.slice(0, 5).map((item) => (
            <div key={item.name} className="muted">{item.name}: {bytes(item.bytes)}</div>
          ))}
        </div>
      )}
      {status && status.state !== "" && (
        <div className="test-report">
          <strong>
            {status.state === "running" ? `Copying ${count(status.filesDone + status.filesSkipped)} of ${count(status.filesTotal)}`
              : status.state === "finished" ? "Copy finished"
              : status.state === "stopped" ? "Copy stopped. Run it again to continue where it stopped."
              : "Copy failed"}
          </strong>
          <div className="scan-bar"><div className="scan-fill" style={{ width: `${status.bytesTotal ? (status.bytesDone / status.bytesTotal) * 100 : 0}%` }} /></div>
          <div>{bytes(status.bytesDone)} of {bytes(status.bytesTotal)}</div>
          {status.current && <div className="muted">{status.current}</div>}
          {status.filesSkipped > 0 && <div className="muted">{count(status.filesSkipped)} already copied, skipped.</div>}
          {status.filesPartial > 0 && <div>{count(status.filesPartial)} partial. The manifest says what is missing from each.</div>}
          {status.badClusters > 0 && <div className="warn">{count(status.badClusters)} unreadable clusters were filled with zeros.</div>}
          {status.filesFailed > 0 && <div className="warn">{count(status.filesFailed)} failed.</div>}
          {status.error && <div className="warn">{status.error}</div>}
          {status.manifest && <div className="muted detail-path">Manifest: {status.manifest}</div>}
        </div>
      )}
    </div>
  );
}

function VolumeTab({ overview, status, revision, onChanged }: { overview: Overview; status: Status | null; revision: number; onChanged: () => void }) {
  const [parameters, setParameters] = useState<Parameters | null>(null);
  const [busy, setBusy] = useState(false);
  const [verify, setVerify] = useState<string | null>(null);
  useEffect(() => {
    call<Parameters>("get_parameters").then(setParameters).catch(() => undefined);
  }, [revision]);

  async function change(values: Partial<Parameters>) {
    setBusy(true);
    await call("set_parameters", values).catch(() => undefined);
    setParameters(await call<Parameters>("get_parameters"));
    setBusy(false);
    onChanged();
  }

  async function rebuild() {
    setBusy(true);
    await call("resolve").catch(() => undefined);
    setBusy(false);
    onChanged();
  }

  async function runVerify() {
    setVerify("Reading structures from disk");
    const result = await call<{ unchanged: boolean }>("verify_unchanged");
    setVerify(result.unchanged ? "Unchanged: boot regions, FAT, bitmap and root match the baseline." : "Changed: something has written to this volume.");
  }

  const fields: [string, string][] = [
    ["percentInUse", "Percent in use"], ["volumeFlags", "Volume flags"], ["clusterCount", "Clusters"], ["fatOffset", "FAT offset"],
    ["fatLength", "FAT length"], ["clusterHeapOffset", "Heap offset"], ["rootCluster", "Root cluster"], ["volumeSerial", "Serial"],
    ["numberOfFats", "FAT count"], ["sectorsPerClusterShift", "Sectors per cluster, as a power of 2"],
  ];
  const summary = status?.summary;
  return (
    <div className="detail">
      <h4>Space</h4>
      <dl className="facts">
        <Fact label="In use">{bytes(overview.allocationBytes.allocated)}</Fact>
        <Fact label="Free, chain remains">{bytes(overview.allocationBytes.freeWithFat)}</Fact>
        <Fact label="Free, no chain">{bytes(overview.allocationBytes.freeZeroFat)}</Fact>
        <Fact label="Free chains">{count(overview.allocation.freeChains.chains)}</Fact>
        <Fact label="Cross-linked clusters">{count(overview.allocation.crossLinks)}</Fact>
      </dl>
      <h4>Boot sectors</h4>
      <table className="boot-table">
        <thead><tr><th>Field</th><th>Main</th><th>Backup</th></tr></thead>
        <tbody>
          {fields.map(([key, label]) => (
            <tr key={key} className={overview.bootDifferences[key] ? "boot-different" : ""}>
              <td>{label}</td>
              <td className="mono">{String(overview.bootMain[key])}</td>
              <td className="mono">{String(overview.bootBackup[key])}</td>
            </tr>
          ))}
        </tbody>
      </table>
      <p className="muted">Checksums: main {overview.bootCheck.mainStored === overview.bootCheck.mainComputed ? "valid" : "invalid"}, backup {overview.bootCheck.backupStored === overview.bootCheck.backupComputed ? "valid" : "invalid"}.</p>
      {summary && (
        <>
          <h4>What the map holds</h4>
          <table className="boot-table">
            <thead><tr><th>Verdict</th><th>Entries</th><th>Size</th></tr></thead>
            <tbody>
              {summary.verdicts.map((item) => (
                <tr key={item.verdict}>
                  <td>{VERDICT_LABEL[item.verdict] ?? item.verdict}</td>
                  <td className="mono">{count(item.count)}</td>
                  <td className="mono">{bytes(item.bytes)}</td>
                </tr>
              ))}
            </tbody>
          </table>
          <dl className="facts">
            <Fact label="Nameless chains">{count(summary.namelessChains)}, {bytes(summary.namelessClusters * overview.geometry.clusterSize)}</Fact>
            <Fact label="Free space unexplained">{bytes(summary.unexplainedFreeClusters * overview.geometry.clusterSize)}</Fact>
            {summary.labelledFreeClusters > 0 && <Fact label="Explained by hypotheses">{bytes(summary.labelledFreeClusters * overview.geometry.clusterSize)}</Fact>}
            <Fact label="Map rebuilt">{clock(summary.resolvedAtMillis)}</Fact>
          </dl>
        </>
      )}
      <div className="detail-actions">
        <button onClick={rebuild} disabled={busy}>{busy ? "Rebuilding" : "Rebuild the map now"}</button>
        <button onClick={runVerify}>Check disk is unchanged</button>
      </div>
      {verify && <p>{verify}</p>}
      {parameters && (
        <>
          <h4>How damage is weighed</h4>
          <label className="check">
            <input type="checkbox" checked={parameters.chainFallbackContiguous} disabled={busy} onChange={(event) => change({ chainFallbackContiguous: event.target.checked })} />
            When a chain ends early, assume the rest follows on disk
          </label>
          <label className="check">
            <input type="checkbox" checked={parameters.ignoreEntryConflicts} disabled={busy} onChange={(event) => change({ ignoreEntryConflicts: event.target.checked })} />
            Ignore overlaps between found entries
          </label>
          <label className="field">
            <span>Clusters a file may share with others</span>
            <input type="number" min={0} value={parameters.conflictToleranceClusters} disabled={busy} onChange={(event) => change({ conflictToleranceClusters: Number(event.target.value) })} />
          </label>
        </>
      )}
    </div>
  );
}

function LensTab({ overview, lens, onLens, revision, onChanged }: { overview: Overview; lens: Lens | null; onLens: (lens: Lens | null) => void; revision: number; onChanged: () => void }) {
  const [saved, setSaved] = useState<Overlay[]>([]);
  const [name, setName] = useState("");
  useEffect(() => {
    call<Overlay[]>("list_overlays", { onlyActive: true }).then((items) => setSaved(items.filter((item) => item.kind === "lens"))).catch(() => undefined);
  }, [revision]);

  const current = lens ?? {};
  const toggle = (key: "groups" | "ownership" | "allocation", value: number) => {
    const list = current[key] ?? [];
    const next = list.includes(value) ? list.filter((item) => item !== value) : [...list, value];
    onLens({ ...current, [key]: next.length > 0 ? next : undefined });
  };

  async function save() {
    if (!name.trim()) return;
    await call("add_overlay", { kind: "lens", payload: { ...current, name: name.trim() }, note: `Saved lens: ${name.trim()}`, resolve: false });
    setName("");
    onChanged();
  }

  const allocation = ["In use", "Free, chain remains", "Free, no chain"];
  return (
    <div className="detail">
      <p className="muted">A lens brings the clusters you choose into focus on the map. Everything else drops away.</p>
      <h4>Saved lenses</h4>
      {saved.length === 0 && <p className="muted">None saved yet.</p>}
      {saved.map((item) => (
        <button key={item.id} className="row" onClick={() => onLens(item.payload as Lens)}>
          <span className="row-main"><span className="row-name">{String(item.payload.name ?? "Lens")}</span><span className="row-path">By {item.author}</span></span>
        </button>
      ))}
      <h4>Allocation</h4>
      {allocation.map((label, index) => (
        <label key={label} className="check"><input type="checkbox" checked={current.allocation?.includes(index + 1) ?? false} onChange={() => toggle("allocation", index + 1)} /> {label}</label>
      ))}
      <h4>What explains it</h4>
      {overview.ownershipNames.map((label, index) => (
        <label key={label} className="check"><input type="checkbox" checked={current.ownership?.includes(index) ?? false} onChange={() => toggle("ownership", index)} /> {label}</label>
      ))}
      <h4>Content</h4>
      {overview.groupNames.map((label, index) => (
        <label key={label} className="check"><input type="checkbox" checked={current.groups?.includes(index) ?? false} onChange={() => toggle("groups", index)} /> {label}</label>
      ))}
      <div className="detail-actions">
        <input value={name} onChange={(event) => setName(event.target.value)} placeholder="Name this lens" aria-label="Lens name" />
        <button onClick={save} disabled={!name.trim()}>Save lens</button>
        <button onClick={() => onLens(null)}>Clear</button>
      </div>
    </div>
  );
}

function HypothesesTab({ revision, onChanged }: { revision: number; onChanged: () => void }) {
  const [items, setItems] = useState<Overlay[]>([]);
  const [busy, setBusy] = useState(false);
  const load = useCallback(() => {
    call<Overlay[]>("list_overlays").then((all) => setItems(all.filter((item) => item.kind !== "lens"))).catch(() => undefined);
  }, []);
  useEffect(load, [load, revision]);

  async function act(tool: string, args: object) {
    setBusy(true);
    await call(tool, args).catch(() => undefined);
    setBusy(false);
    load();
    onChanged();
  }

  return (
    <div className="detail">
      <p className="muted">Each hypothesis changes how this app reads the disk, never the disk. Switch one off to see the map without it.</p>
      {items.length === 0 && <p className="empty">No hypotheses yet. Ask Claude to investigate, and any it proposes appear here.</p>}
      {items.map((item) => (
        <div key={item.id} className={item.active ? "hypothesis" : "hypothesis hypothesis-off"}>
          <label className="check">
            <input type="checkbox" checked={item.active} disabled={busy} onChange={(event) => act("set_overlay_active", { id: item.id, active: event.target.checked })} />
            <strong>{OVERLAY_LABEL[item.kind] ?? item.kind}</strong>
          </label>
          <p>{item.note}</p>
          <div className="muted hypothesis-meta">
            <span>By {item.author}, {clock(item.createdAtMillis)}{item.target !== null ? `, target ${count(item.target)}` : ""}</span>
            <button className="link" disabled={busy} onClick={() => act("delete_overlay", { id: item.id })}>Delete</button>
          </div>
        </div>
      ))}
    </div>
  );
}

function ActivityTab({ revision }: { revision: number }) {
  const [items, setItems] = useState<ActivityItem[]>([]);
  useEffect(() => {
    call<ActivityItem[]>("activity", { limit: 200 }).then(setItems).catch(() => undefined);
  }, [revision]);
  const findingText = (detail: string) => {
    try {
      return String(JSON.parse(detail).text);
    } catch {
      return detail;
    }
  };
  return (
    <div className="detail">
      {items.length === 0 && <p className="empty">Nothing yet. Every action by you or Claude is listed here.</p>}
      {items.map((item) =>
        item.action === "log_finding" ? (
          <div key={item.id} className="finding">
            <strong>Finding by {item.author}, {clock(item.atMillis)}</strong>
            <p>{findingText(item.detail)}</p>
          </div>
        ) : (
          <div key={item.id} className="activity" title={item.detail}>
            <span className="muted">{clock(item.atMillis)}</span> <span className={item.author === "claude" ? "activity-claude" : ""}>{item.author}</span> <span className="mono">{item.action}</span>
            <div className="mono muted activity-detail">{item.detail}</div>
          </div>
        ),
      )}
    </div>
  );
}

function ConnectTab() {
  const [details, setDetails] = useState<McpDetails | null>(null);
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    invoke<McpDetails>("mcp_info").then(setDetails).catch(() => undefined);
  }, []);
  if (!details) return null;
  if (!details.info) return <div className="detail"><p className="warn">The MCP server did not start: {details.error}</p></div>;
  const command = `claude mcp add --transport http exfat-lens ${details.info.url} --header "Authorization: Bearer ${details.info.token}"`;
  return (
    <div className="detail">
      <p>Connect another MCP client, such as Claude Code or T3 Code, to work with this same view. The server only accepts connections from this Mac.</p>
      <dl className="facts">
        <Fact label="Address"><span className="mono">{details.info.url}</span></Fact>
      </dl>
      <h4>For Claude Code</h4>
      <pre className="preview-text">{command}</pre>
      <div className="detail-actions">
        <button onClick={async () => {
          await navigator.clipboard.writeText(command);
          setCopied(true);
        }}>{copied ? "Copied" : "Copy command"}</button>
      </div>
      <p className="muted">The command holds this server's access token, so treat it like a password.</p>
    </div>
  );
}

type Tab = "selected" | "copy" | "volume" | "hypotheses" | "lens" | "activity" | "connect";

export interface DetailsProps {
  overview: Overview;
  status: Status | null;
  entry: { row: Row; extents: Extent[] } | null;
  folder: Row | null;
  info: ClusterInfo | null;
  revision: number;
  activityRevision: number;
  lens: Lens | null;
  onLens: (lens: Lens | null) => void;
  onCluster: (cluster: number) => void;
  onEntry: (row: Row) => void;
  onChanged: () => void;
  tabRequest: { tab: Tab; at: number } | null;
  floor: number;
}

export function Details(props: DetailsProps) {
  const { overview, status, entry, folder, info, revision, activityRevision, lens, onLens, onCluster, onEntry, onChanged, tabRequest, floor } = props;
  const [tab, setTab] = useState<Tab>("selected");
  useEffect(() => {
    if (tabRequest) setTab(tabRequest.tab);
  }, [tabRequest]);
  const tabs: [Tab, string, IconName][] = [
    ["selected", "Selected", "info"],
    ["copy", "Copy out", "download"],
    ["hypotheses", "Hypotheses", "bulb"],
    ["lens", "Lens", "aperture"],
    ["volume", "Volume", "disk"],
    ["activity", "Activity", "clock"],
    ["connect", "Connect", "plug"],
  ];
  const tabTitle = tabs.find(([id]) => id === tab)?.[1];
  return (
    <section className="details" aria-label="Details">
      <div className="inspector-tabs" role="tablist">
        {tabs.map(([id, label, icon]) => (
          <button key={id} role="tab" aria-selected={tab === id} aria-label={label} title={label} onClick={() => setTab(id)}>
            <Icon name={icon} size={16} />
          </button>
        ))}
      </div>
      <div className="inspector-title">{tabTitle}</div>
      <div className="panel-body">
        {tab === "selected" && (
          <>
            {entry && <EntryDetails row={entry.row} extents={entry.extents} onCluster={onCluster} onCopy={() => setTab("copy")} />}
            {!entry && folder && <FolderDetails row={folder} onCopy={() => setTab("copy")} />}
            {info && <ClusterDetails info={info} onCluster={onCluster} onEntry={onEntry} />}
            {!entry && !folder && !info && <p className="empty">Nothing selected. Click the map or a file to see what is known about it.</p>}
          </>
        )}
        {tab === "copy" && <CopyTab entry={entry?.row ?? null} folder={folder} status={status?.copy ?? null} floor={floor} />}
        {tab === "volume" && <VolumeTab overview={overview} status={status} revision={revision} onChanged={onChanged} />}
        {tab === "hypotheses" && <HypothesesTab revision={revision} onChanged={onChanged} />}
        {tab === "lens" && <LensTab overview={overview} lens={lens} onLens={onLens} revision={revision} onChanged={onChanged} />}
        {tab === "activity" && <ActivityTab revision={activityRevision} />}
        {tab === "connect" && <ConnectTab />}
      </div>
    </section>
  );
}
