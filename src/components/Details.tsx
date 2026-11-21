import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import {
  call,
  type ActivityItem,
  type ClusterInfo,
  type Extent,
  type Lens,
  type McpDetails,
  type Overlay,
  type Overview,
  type Parameters,
  type Row,
  type Status,
  type ResolvePreview,
} from "../api";
import { bytes, clock, count, decimal, displayPath, percent, stamp, verdictLabel } from "../format";
import { useSimple } from "../mode";
import { fromEngine, t } from "../i18n";
import { InlinePreview } from "./BrowseView";
import { FilesystemDetailsPanel } from "./FilesystemDetails";
import { NtfsRecordPanel } from "./NtfsRecord";
import { SimilarPieces } from "./SimilarPieces";
import { isArchiveName } from "./ZipView";
import { ConfidenceRing, fileIcon, Icon, verdictTone, type IconName } from "./icons";

const MAX_COPIES_SHOWN = 20;

const OVERLAY_LABEL: Record<string, () => string> = {
  accept_entry: () => t("Accept a damaged entry"),
  file_extents: () => t("Reassemble a file"),
  folder_parent: () => t("Attach a folder"),
  directory_cluster: () => t("Join a directory cluster"),
  note: () => t("Note"),
  struct: () => t("Custom structure"),
  lens: () => t("Lens"),
  region_label: () => t("Explain a region"),
};

function Fact({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <>
      <dt>{label}</dt>
      <dd>{children}</dd>
    </>
  );
}

function EntryDetails({ row, extents, macosWroteOn, onCluster, onPreview, onInsideZip, previewElsewhere, onEntry }: { onInsideZip: () => void; row: Row; extents: Extent[]; macosWroteOn: string[]; onCluster: (cluster: number) => void; onPreview: (row: Row) => void; previewElsewhere: boolean; onEntry: (row: Row) => void }) {
  const simple = useSimple();
  const [copies, setCopies] = useState<Row[]>([]);
  const [report, setReport] = useState<Record<string, any> | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const id = row.id as number;

  useEffect(() => {
    let current = true;
    setCopies([]);
    call<Row[]>("file_copies", { id, limit: MAX_COPIES_SHOWN }).then((rows) => current && setCopies(rows)).catch(() => undefined);
    return () => {
      current = false;
    };
  }, [id]);

  useEffect(() => {
    setReport(null);
    setError(null);
  }, [id]);

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
      {row.path !== null && <div className="muted detail-path">{displayPath(row.path, simple)}</div>}
      {row.kind === "file" && !previewElsewhere && (
        <InlinePreview
          key={id}
          id={id}
          name={row.name}
          length={row.length}
          complete={row.clustersFound >= row.clustersNeeded}
          onOpen={() => onPreview(row)}
        />
      )}
      <div className="verdict-line">
        <span className={`verdict-chip ${verdictTone(row.verdict)}`}>{verdictLabel(row.verdict)}</span>
        <span className="confidence-text">{t("Confidence {percent}", { percent: percent(row.confidence) })}</span>
      </div>
      <p>{fromEngine(row.reason)}</p>
      {macosWroteOn.length > 0 && (
        <p className="detail-macos-writes">
          {t("macOS wrote to some of its clusters on {dates}, for its own index and logs, so parts of it may have been overwritten.", { dates: macosWroteOn.join(", ") })}
        </p>
      )}
      {row.kind === "file" && row.length > 0 && (
        <div className="detail-actions">
          <button className="primary" onClick={() => onPreview(row)}>{t("Open large")}</button>
          {isArchiveName(row.name) && <button onClick={onInsideZip}>{t("Look inside")}</button>}
          <button onClick={runTest} disabled={busy !== null}>{busy === "test" ? t("Testing") : t("Test format")}</button>
        </div>
      )}
      <dl className="facts">
        {row.kind === "file" && <Fact label={t("Size")}>{t("{size} ({count} bytes)", { size: bytes(row.length), count: count(row.length) })}</Fact>}
        {/* Where it lies on the disk is for investigating, and Simple mode leaves it out. */}
        {!simple && (
          <>
            {row.storage !== "resident" && (
              <Fact label={t("First cluster")}>
                <button className="link" onClick={() => onCluster(row.firstCluster)}>{count(row.firstCluster)}</button>
              </Fact>
            )}
            {row.kind === "file" && <Fact label={t("Clusters")}>{t("{found} found of {needed} needed", { found: count(row.clustersFound), needed: count(row.clustersNeeded) })}</Fact>}
            {row.clustersAllocated > 0 && <Fact label={t("In current use")}>{t("{count} clusters", { count: count(row.clustersAllocated) })}</Fact>}
            {row.clustersConflict > 0 && <Fact label={t("Shared")}>{t("{count} clusters", { count: count(row.clustersConflict) })}</Fact>}
            <Fact label={t("Pieces")}>{count(extents.length)}</Fact>
            <Fact label={t("Layout")}>{row.storage === "resident" ? t("Inside its FILE record") : row.storage !== undefined ? t("Run list") : row.noFatChain ? t("Contiguous, no FAT chain") : t("FAT chain")}</Fact>
            {row.weak && <Fact label={t("Found by")}>{row.weak === "lone_entry" ? t("Its content only, a lone entry") : t("Its content only")}</Fact>}
            <Fact label={t("Entry state")}>{row.inUse ? t("In use") : t("Marked deleted")}{row.checksumOk ? "" : row.storage !== undefined ? t(", record damaged") : t(", checksum failed")}</Fact>
          </>
        )}
        <Fact label={t("Created")}>{stamp(row.created)}</Fact>
        <Fact label={t("Modified")}>{stamp(row.modified)}</Fact>
        {!simple && row.sourceCluster !== null && (
          <Fact label={t("Entry is in")}>
            <button className="link" onClick={() => onCluster(row.sourceCluster as number)}>{t("cluster {number}", { number: count(row.sourceCluster) })}</button>
          </Fact>
        )}
      </dl>
      {row.storage !== undefined && <NtfsRecordPanel id={id} />}
      {copies.length > 0 && (
        <>
          <h4>{copies.length >= MAX_COPIES_SHOWN ? t("Same bytes also at (first {count})", { count: count(MAX_COPIES_SHOWN) }) : t("Same bytes also at")}</h4>
          {copies.map((copy) => (
            <button key={copy.id} className="row" onClick={() => onEntry(copy)} title={fromEngine(copy.reason)}>
              <span className="row-icon"><Icon name={fileIcon(copy.name)} /></span>
              <span className="row-main"><span className="row-name">{copy.name}</span><span className="row-path">{displayPath(copy.path, simple)}</span></span>
            </button>
          ))}
        </>
      )}
      {error && <p className="warn">{fromEngine(error)}</p>}
      {report && (
        <div className="test-report">
          <strong>{report.report.format === "unknown" ? t("No format walker for this type") : t("{format} structure: {percent} valid", { format: report.report.format.toUpperCase(), percent: percent(report.report.validFraction) })}</strong>
          <div>{t("{count} frames or chunks walked.", { count: count(report.report.units) })} {report.report.reachedEnd ? t("Reached the end.") : t("Did not reach a clean end.")}</div>
          {report.breakLocations.map((item: any, index: number) => (
            <div key={index}>
              {t("Breaks at stream byte {offset}", { offset: count(item.streamOffset) })}
              {item.cluster && !simple && (
                <> {t("in")} <button className="link" onClick={() => onCluster(item.cluster)}>{t("cluster {number}", { number: count(item.cluster) })}</button></>
              )}
            </div>
          ))}
          {report.report.notes.map((note: string, index: number) => <div key={index} className="muted">{fromEngine(note)}</div>)}
        </div>
      )}
    </div>
  );
}

function ClusterDetails({ info, onCluster, onEntry }: { info: ClusterInfo; onCluster: (cluster: number) => void; onEntry: (row: Row) => void }) {
  const simple = useSimple();
  return (
    <div className="detail">
      <h3 className="detail-name">{t("Cluster {cluster}", { cluster: count(info.cluster) })}</h3>
      <dl className="facts">
        <Fact label={t("Bitmap")}>{info.allocationUnknown ? t("Allocation unknown") : info.allocated ? t("In use") : t("Marked free")}</Fact>
        <Fact label={t("FAT entry")}>{fromEngine(info.fatMeaning)}</Fact>
        <Fact label={t("Chain start")}>
          {info.chainHead === info.cluster ? t("This cluster") : <button className="link" onClick={() => onCluster(info.chainHead)}>{count(info.chainHead)}</button>}
        </Fact>
        {info.predecessor !== null && <Fact label={t("Reached from")}><button className="link" onClick={() => onCluster(info.predecessor as number)}>{count(info.predecessor)}</button></Fact>}
        <Fact label={t("Content")}>{fromEngine(info.contentClass)}</Fact>
        <Fact label={t("Entropy")}>{t("{bits} bits per byte", { bits: decimal(info.entropyBits, 2) })}</Fact>
        <Fact label={t("Explained by")}>{fromEngine(info.ownership)}</Fact>
        <Fact label={t("Confidence")}>{percent(info.confidence)}</Fact>
        <Fact label={t("Partition offset")}><span className="mono">{`0x${info.offset.toString(16)}`}</span></Fact>
        {info.diskOffset !== null && <Fact label={t("Disk offset")}><span className="mono">{`0x${info.diskOffset.toString(16)}`}</span></Fact>}
        {info.sets.length > 0 && <Fact label={t("Entries here")}>{t("{count}, of which {damaged} damaged", { count: count(info.sets.length), damaged: count(info.sets.filter((set) => !set.checksumOk).length) })}</Fact>}
      </dl>
      {info.owners.length > 0 && (
        <>
          <h4>{t("Claimed by")}</h4>
          {info.owners.slice(0, 12).map((owner) => (
            <button key={owner.id} className="row" onClick={() => onEntry(owner)} title={fromEngine(owner.reason)}>
              <span className="row-icon"><Icon name={fileIcon(owner.name)} /></span>
              <span className="row-main"><span className="row-name">{owner.name}</span><span className="row-path">{displayPath(owner.path, simple)}</span></span>
              <ConfidenceRing value={owner.confidence} tone={verdictTone(owner.verdict)} label={t("{verdict}, confidence {percent}", { verdict: verdictLabel(owner.verdict), percent: percent(owner.confidence) })} />
            </button>
          ))}
        </>
      )}
      {info.notes.map((note) => <p key={note.id} className="note">{note.note}</p>)}
    </div>
  );
}

function FolderDetails({ row, onBrowse }: { row: Row; onBrowse: (row: Row) => void }) {
  const simple = useSimple();
  const [similar, setSimilar] = useState(false);
  useEffect(() => setSimilar(false), [row.head]);
  return (
    <div className="detail">
      <h3 className="detail-name">{row.name || t("Folder with no name, at cluster {cluster}", { cluster: count(row.firstCluster) })}</h3>
      {row.path !== null && <div className="muted detail-path">{displayPath(row.path, simple)}</div>}
      <dl className="facts">
        <Fact label={t("Holds")}>{t("{count} files, {bytes}", { count: count(row.files), bytes: bytes(row.bytes) })}</Fact>
        <Fact label={t("Likely intact")}>{t("{count} files, {bytes}", { count: count(row.intactFiles), bytes: bytes(row.intactBytes) })}</Fact>
        <Fact label={t("Place in tree")}>
          {row.nodeKind === "orphan" ? t("Parent not found") : row.nodeKind === "attached" ? t("Inside a folder whose parent was not found") : t("Reachable from the volume root")}
        </Fact>
        {row.linkConfidence !== null && <Fact label={t("Link to parent")}>{percent(row.linkConfidence)}</Fact>}
        {row.hint && <Fact label={t("Contains")}>{row.hint}</Fact>}
      </dl>
      <div className="detail-actions">
        <button className="primary" onClick={() => onBrowse(row)}>{t("Browse files")}</button>
        {(row.nodeKind === "orphan" || row.nodeKind === "root") && !similar && (
          <button onClick={() => setSimilar(true)}>{t("Find similar pieces")}</button>
        )}
      </div>
      {similar && row.head !== null && (
        <>
          <h4>{t("Similar pieces")}</h4>
          <SimilarPieces head={row.head} onOpen={onBrowse} />
        </>
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

  // A change is previewed first, and applied only when the person agrees.
  const [pending, setPending] = useState<{ values: Partial<Parameters>; preview: ResolvePreview | null } | null>(null);
  const asked = useRef(0);

  async function change(values: Partial<Parameters>) {
    const merged = { ...(pending?.values ?? {}), ...values };
    const ask = ++asked.current;
    setPending({ values: merged, preview: null });
    const preview = await call<ResolvePreview>("resolve_preview", { parameters: merged }).catch(() => null);
    if (asked.current === ask) setPending({ values: merged, preview });
  }

  async function apply() {
    if (!pending) return;
    setBusy(true);
    await call("set_parameters", pending.values).catch(() => undefined);
    setParameters(await call<Parameters>("get_parameters"));
    asked.current++;
    setPending(null);
    setBusy(false);
    onChanged();
  }

  function cancel() {
    asked.current++;
    setPending(null);
  }
  const shown = parameters ? { ...parameters, ...(pending?.values ?? {}) } : null;

  async function rebuild() {
    setBusy(true);
    await call("resolve").catch(() => undefined);
    setBusy(false);
    onChanged();
  }

  async function runVerify() {
    setVerify(t("Reading structures from disk"));
    const result = await call<{ unchanged: boolean }>("verify_unchanged");
    setVerify(result.unchanged ? t("Unchanged: boot regions, FAT, bitmap and root match the baseline.") : t("Changed: something has written to this volume."));
  }

  const fields: [string, string][] = [
    ["percentInUse", t("Percent in use")], ["volumeFlags", t("Volume flags")], ["clusterCount", t("Clusters")], ["fatOffset", t("FAT offset")],
    ["fatLength", t("FAT length")], ["clusterHeapOffset", t("Heap offset")], ["rootCluster", t("Root cluster")], ["volumeSerial", t("Serial")],
    ["numberOfFats", t("FAT count")], ["sectorsPerClusterShift", t("Sectors per cluster, as a power of 2")],
  ];
  const summary = status?.summary;
  return (
    <div className="detail">
      <FilesystemDetailsPanel details={overview.filesystem} />
      {(overview.structureDamage ?? []).map((text) => <p key={text} className="warn">{fromEngine(text)}</p>)}
      {overview.fatAdapter === "legacy" && <p className="hint">{t("The old FAT adapter reads this volume. It is kept for one release.")}</p>}
      <h4>{t("Space")}</h4>
      <dl className="facts">
        {overview.allocationKnown === false && <Fact label={t("Allocation unknown")}>{bytes(overview.allocationBytes.unknown ?? 0)}</Fact>}
        {overview.allocationKnown !== false && <Fact label={t("In use")}>{bytes(overview.allocationBytes.allocated)}</Fact>}
        {overview.allocationKnown === false ? null : overview.filesystem.kind === "ntfs" ? (
          <Fact label={t("Free")}>{bytes(overview.allocationBytes.freeZeroFat)}</Fact>
        ) : (
          <>
            <Fact label={t("Free, chain remains")}>{bytes(overview.allocationBytes.freeWithFat)}</Fact>
            <Fact label={t("Free, no chain")}>{bytes(overview.allocationBytes.freeZeroFat)}</Fact>
            <Fact label={t("Free chains")}>{count(overview.allocation.freeChains.chains)}</Fact>
            <Fact label={t("Cross-linked clusters")}>{count(overview.allocation.crossLinks)}</Fact>
          </>
        )}
      </dl>
      {overview.filesystem.kind === "exfat" && overview.bootCheck && <>
      <h4>{t("Boot sectors")}</h4>
      <table className="boot-table">
        <thead><tr><th>{t("Field")}</th><th>{t("Main")}</th><th>{t("Backup")}</th></tr></thead>
        <tbody>
          {fields.map(([key, label]) => (
            <tr key={key} className={overview.bootDifferences[key] ? "boot-different" : ""}>
              <td>{label}</td>
              <td className="mono">{String(overview.bootMain[key])}</td>
              <td className="mono">{String(overview.bootBackup?.[key])}</td>
            </tr>
          ))}
        </tbody>
      </table>
      <p className="muted">{t("Checksums: main {main}, backup {backup}.", {
        main: overview.bootCheck.mainStored === overview.bootCheck.mainComputed ? t("valid") : t("invalid"),
        backup: overview.bootCheck.backupStored === overview.bootCheck.backupComputed ? t("valid") : t("invalid"),
      })}</p>
      </>}
      {summary && (
        <>
          <h4>{t("What the map holds")}</h4>
          <table className="boot-table">
            <thead><tr><th>{t("Verdict")}</th><th>{t("Entries")}</th><th>{t("Size")}</th></tr></thead>
            <tbody>
              {summary.verdicts.map((item) => (
                <tr key={item.verdict}>
                  <td>{verdictLabel(item.verdict)}</td>
                  <td className="mono">{count(item.count)}</td>
                  <td className="mono">{bytes(item.bytes)}</td>
                </tr>
              ))}
            </tbody>
          </table>
          <dl className="facts">
            <Fact label={t("Nameless chains")}>{count(summary.namelessChains)}, {bytes(summary.namelessClusters * overview.geometry.clusterSize)}</Fact>
            {overview.allocationKnown === false ? (
              <Fact label={t("Unexplained, allocation unknown")}>{bytes((summary.unexplainedUnknownClusters ?? 0) * overview.geometry.clusterSize)}</Fact>
            ) : (
              <Fact label={t("Free space unexplained")}>{bytes(summary.unexplainedFreeClusters * overview.geometry.clusterSize)}</Fact>
            )}
            {(summary.badSectorFreeClusters ?? 0) > 0 && <Fact label={t("Free space in bad sectors")}>{bytes((summary.badSectorFreeClusters ?? 0) * overview.geometry.clusterSize)}</Fact>}
            {summary.labelledFreeClusters > 0 && <Fact label={t("Explained by hypotheses")}>{bytes(summary.labelledFreeClusters * overview.geometry.clusterSize)}</Fact>}
            <Fact label={t("Map rebuilt")}>{clock(summary.resolvedAtMillis)}</Fact>
          </dl>
        </>
      )}
      <div className="detail-actions">
        <button onClick={rebuild} disabled={busy}>{busy ? t("Rebuilding") : t("Rebuild the map now")}</button>
        <button onClick={runVerify}>{t("Check disk is unchanged")}</button>
      </div>
      {verify && <p>{verify}</p>}
      {shown && (
        <>
          <h4>{t("How damage is weighed")}</h4>
          <label className="check">
            <input type="checkbox" checked={shown.chainFallbackContiguous} disabled={busy} onChange={(event) => change({ chainFallbackContiguous: event.target.checked })} />
            {t("When a chain ends early, assume the rest follows on disk")}
          </label>
          <label className="check">
            <input type="checkbox" checked={shown.ignoreEntryConflicts} disabled={busy} onChange={(event) => change({ ignoreEntryConflicts: event.target.checked })} />
            {t("Ignore overlaps between found entries")}
          </label>
          <label className="check">
            <input type="checkbox" checked={shown.ignoreReuseOrder} disabled={busy} onChange={(event) => change({ ignoreReuseOrder: event.target.checked })} />
            {t("Count clusters reused by newer files as conflicts")}
          </label>
          <label className="field">
            <span>{t("Clusters a file may share with others")}</span>
            <input type="number" min={0} value={shown.conflictToleranceClusters} disabled={busy} onChange={(event) => change({ conflictToleranceClusters: Number(event.target.value) })} />
          </label>
          {pending && (
            <div className="weigh-preview" role="status">
              {pending.preview === null ? (
                <p>{t("Working out what would change")}</p>
              ) : pending.preview.filesChanged === 0 ? (
                <p>{t("No file would change verdict.")}</p>
              ) : (
                <>
                  <p>{t("{files} files ({size}) would change verdict:", { files: count(pending.preview.filesChanged), size: bytes(pending.preview.bytesChanged) })}</p>
                  <ul>
                    {pending.preview.changes.slice(0, 6).map((item) => (
                      <li key={`${item.from}>${item.to}`}>
                        {t("{from} to {to}: {files} files, {size}", { from: verdictLabel(item.from), to: verdictLabel(item.to), files: count(item.files), size: bytes(item.bytes) })}
                      </li>
                    ))}
                  </ul>
                </>
              )}
              <div className="detail-actions">
                <button className="primary" disabled={busy || pending.preview === null} onClick={() => void apply()}>{t("Apply")}</button>
                <button onClick={cancel}>{t("Cancel")}</button>
              </div>
            </div>
          )}
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

  const allocation = [t("In use"), t("Free, chain remains"), t("Free, no chain"), ...(overview.allocationKnown === false ? [t("Allocation unknown")] : [])];
  return (
    <div className="detail">
      <p className="muted">{t("A lens brings the clusters you choose into focus on the map. Everything else drops away.")}</p>
      <h4>{t("Saved lenses")}</h4>
      {saved.length === 0 && <p className="muted">{t("None saved yet.")}</p>}
      {saved.map((item) => (
        <button key={item.id} className="row" onClick={() => onLens(item.payload as Lens)}>
          <span className="row-main"><span className="row-name">{String(item.payload.name ?? t("Lens"))}</span><span className="row-path">{t("By {author}", { author: item.author })}</span></span>
        </button>
      ))}
      <h4>{t("Allocation")}</h4>
      {allocation.map((label, index) => (
        <label key={label} className="check"><input type="checkbox" checked={current.allocation?.includes(index + 1) ?? false} onChange={() => toggle("allocation", index + 1)} /> {label}</label>
      ))}
      <h4>{t("What explains it")}</h4>
      {overview.ownershipNames.map((label, index) => (
        <label key={label} className="check"><input type="checkbox" checked={current.ownership?.includes(index) ?? false} onChange={() => toggle("ownership", index)} /> {fromEngine(label)}</label>
      ))}
      <h4>{t("Content")}</h4>
      {overview.groupNames.map((label, index) => (
        <label key={label} className="check"><input type="checkbox" checked={current.groups?.includes(index) ?? false} onChange={() => toggle("groups", index)} /> {fromEngine(label)}</label>
      ))}
      <div className="detail-actions">
        <input value={name} onChange={(event) => setName(event.target.value)} placeholder={t("Name this lens")} aria-label={t("Lens name")} />
        <button onClick={save} disabled={!name.trim()}>{t("Save lens")}</button>
        <button onClick={() => onLens(null)}>{t("Clear")}</button>
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
      <p className="muted">{t("Each hypothesis changes how this app reads the disk, never the disk. Switch one off to see the map without it.")}</p>
      {items.length === 0 && <p className="empty">{t("No hypotheses yet. Ask an agent to investigate, and any it proposes appear here.")}</p>}
      {items.map((item) => (
        <div key={item.id} className={item.active ? "hypothesis" : "hypothesis hypothesis-off"}>
          <label className="check">
            <input type="checkbox" checked={item.active} disabled={busy} onChange={(event) => act("set_overlay_active", { id: item.id, active: event.target.checked })} />
            <strong>{OVERLAY_LABEL[item.kind]?.() ?? item.kind}</strong>
          </label>
          <p>{item.note}</p>
          <div className="muted hypothesis-meta">
            <span>{item.target !== null
              ? t("By {author}, {time}, target {target}", { author: item.author, time: clock(item.createdAtMillis), target: count(item.target) })
              : t("By {author}, {time}", { author: item.author, time: clock(item.createdAtMillis) })}</span>
            <button className="link" disabled={busy} onClick={() => act("delete_overlay", { id: item.id })}>{t("Delete")}</button>
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
      {items.length === 0 && <p className="empty">{t("Nothing yet. Every action by you or an agent is listed here.")}</p>}
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
  if (!details.info) return <div className="detail"><p className="warn">{t("The MCP server did not start: {error}", { error: fromEngine(details.error) })}</p></div>;
  const command = `claude mcp add --transport http captain-salvage ${details.info.url} --header "Authorization: Bearer ${details.info.token}"`;
  return (
    <div className="detail">
      <p>{t("Connect another MCP client, such as Claude Code or T3 Code, to work with this same view. The server only accepts connections from this Mac.")}</p>
      <dl className="facts">
        <Fact label={t("Address")}><span className="mono">{details.info.url}</span></Fact>
      </dl>
      <h4>{t("For Claude Code")}</h4>
      <pre className="preview-text">{command}</pre>
      <div className="detail-actions">
        <button onClick={async () => {
          await navigator.clipboard.writeText(command);
          setCopied(true);
        }}>{copied ? t("Copied") : t("Copy command")}</button>
      </div>
      <p className="muted">{t("The command holds this server's access token, so treat it like a password.")}</p>
    </div>
  );
}

/** The tabs Simple mode keeps. */
const SIMPLE_TABS: DetailsTab[] = ["selected", "rebuild", "agent"];

export type DetailsTab = "selected" | "rebuild" | "agent" | "workbench" | "volume" | "hypotheses" | "lens" | "activity" | "connect";

export interface DetailsProps {
  overview: Overview;
  status: Status | null;
  entry: { row: Row; extents: Extent[]; macosWroteOn: string[] } | null;
  folder: Row | null;
  info: ClusterInfo | null;
  revision: number;
  activityRevision: number;
  lens: Lens | null;
  onLens: (lens: Lens | null) => void;
  onCluster: (cluster: number) => void;
  onEntry: (row: Row) => void;
  onChanged: () => void;
  tabRequest: { tab: DetailsTab; at: number } | null;
  /** Says which tab is showing, so the sidebar can mark the pieces a diskRebuild has used. */
  onTab: (tab: DetailsTab) => void;
  /** The Rebuild tab's diskRebuild, which pieces are dragged onto from the sidebar. */
  rebuild: ReactNode;
  /** The conversation with Claude. It stays mounted on other tabs, so a reply keeps arriving. */
  agent: ReactNode;
  /** Kept mounted, like the agent, so pieces gathered there survive a look at another tab. */
  workbench: ReactNode;
  /** Claude is answering, or has answered since the tab was last open. */
  agentActive: boolean;
  floor: number;
  onPreview: (row: Row) => void;
  onInsideZip: () => void;
  onBrowseFolder: (row: Row) => void;
  previewInCenter: number | null;
}

export function Details(props: DetailsProps) {
  const { overview, status, entry, folder, info, revision, activityRevision, lens, onLens, onCluster, onEntry, onChanged, tabRequest, onPreview, onInsideZip, onBrowseFolder, previewInCenter, onTab, rebuild, agent, workbench, agentActive } = props;
  const [tab, setTab] = useState<DetailsTab>("selected");
  useEffect(() => {
    if (tabRequest) setTab(tabRequest.tab);
  }, [tabRequest]);
  useEffect(() => onTab(tab), [tab, onTab]);
  const tabs: [DetailsTab, string, IconName][] = [
    ["selected", t("Selected"), "info"],
    ["rebuild", t("Rebuild"), "truck"],
    ["agent", t("Chat with an agent"), "robot"],
    ["workbench", t("Workbench"), "puzzle"],
    ["hypotheses", t("Hypotheses"), "bulb"],
    ["lens", t("Lens"), "aperture"],
    ["volume", t("Volume"), "disk"],
    ["activity", t("Activity"), "clock"],
    ["connect", t("Connect"), "plug"],
  ];
  // Simple mode keeps what recovering files needs.
  const simple = useSimple();
  const shownTabs = simple ? tabs.filter(([id]) => SIMPLE_TABS.includes(id)) : tabs;
  useEffect(() => {
    if (simple && !SIMPLE_TABS.includes(tab)) setTab("selected");
  }, [simple, tab]);
  const tabTitle = tabs.find(([id]) => id === tab)?.[1];
  return (
    <section className="details" aria-label={t("Details")}>
      <div className="inspector-tabs" role="tablist">
        {shownTabs.map(([id, label, icon]) => (
          <button key={id} role="tab" aria-selected={tab === id} aria-label={label} title={label} onClick={() => setTab(id)}>
            <Icon name={icon} size={16} />
            {id === "agent" && agentActive && tab !== "agent" && <span className="tab-dot" />}
          </button>
        ))}
      </div>
      <div className="inspector-title">{tabTitle}</div>
      {tab === "rebuild" && rebuild}
      <div className="details-agent" hidden={tab !== "agent"}>{agent}</div>
      <div className="panel-body" hidden={tab !== "workbench"}>{workbench}</div>
      <div className="panel-body" hidden={tab === "rebuild" || tab === "agent" || tab === "workbench"}>
        {tab === "selected" && (
          <>
            {entry && (
              <EntryDetails
                row={entry.row}
                extents={entry.extents}
                macosWroteOn={entry.macosWroteOn}
                onCluster={onCluster}
                onPreview={onPreview}
                onInsideZip={onInsideZip}
                previewElsewhere={previewInCenter === entry.row.id}
                onEntry={onEntry}
              />
            )}
            {!entry && folder && (
              <FolderDetails
                row={folder}
                onBrowse={onBrowseFolder}
              />
            )}
            {/* A cluster's own details are for investigating. */}
            {info && !simple && <ClusterDetails info={info} onCluster={onCluster} onEntry={onEntry} />}
            {!entry && !folder && !info && <p className="empty">{simple ? t("Nothing selected. Click a file to see what is known about it.") : t("Nothing selected. Click the map or a file to see what is known about it.")}</p>}
          </>
        )}
        {tab === "volume" && <VolumeTab overview={overview} status={status} revision={revision} onChanged={onChanged} />}
        {tab === "hypotheses" && <HypothesesTab revision={revision} onChanged={onChanged} />}
        {tab === "lens" && <LensTab overview={overview} lens={lens} onLens={onLens} revision={revision} onChanged={onChanged} />}
        {tab === "activity" && <ActivityTab revision={activityRevision} />}
        {tab === "connect" && <ConnectTab />}
      </div>
    </section>
  );
}
