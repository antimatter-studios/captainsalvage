import { useState } from "react";
import { call, type Extent, type Selection } from "../api";
import { bytes, count, percent } from "../format";
import { fromEngine, t } from "../i18n";
import { Icon } from "./icons";

/** One piece of a reassembly: some clusters, in the order they are to be read. */
interface Piece {
  label: string;
  extents: Extent[];
}

interface StreamTest {
  report: { format: string; validFraction: number; reachedEnd: boolean; breaks: number[]; notes: string[] };
  /** Which cluster of the disk each break falls in. */
  breakLocations: { streamOffset: number; cluster: number }[];
}

interface Materialized {
  kind: "image" | "text" | "none" | "too-big";
  mime?: string;
  text?: string;
  dataBase64?: string;
  bytes: number;
  truncated?: boolean;
}

function clustersIn(extents: Extent[]): number {
  return extents.reduce((sum, extent) => sum + extent.count, 0);
}

/** Where a break falls: its byte in the reassembled file, and the disk's cluster holding it. */
function breakText(test: StreamTest): string {
  const found = test.breakLocations[0];
  return found
    ? t("byte {offset}, in cluster {cluster}", { offset: count(found.streamOffset), cluster: count(found.cluster) })
    : t("byte {offset}", { offset: count(test.report.breaks[0] ?? 0) });
}

/**
 * Puts a file back together by hand: pick clusters or chains, put them in order, and test and
 * show the result live from the disk, without copying anything. A result that looks right can
 * be kept as a hypothesis, so copying and browsing use the same reassembly.
 */
export function Workbench({ selection, clusterSize, onShowClusters, onChanged }: {
  selection: Selection;
  clusterSize: number;
  onShowClusters: (extents: Extent[]) => void;
  onChanged: () => void;
}) {
  const [pieces, setPieces] = useState<Piece[]>([]);
  const [name, setName] = useState("");
  const [length, setLength] = useState("");
  const [fromEntry, setFromEntry] = useState<{ id: number; name: string } | null>(null);
  const [tested, setTested] = useState<StreamTest | null>(null);
  const report = tested?.report ?? null;
  const [shown, setShown] = useState<Materialized | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [saved, setSaved] = useState<string | null>(null);

  const all = pieces.flatMap((piece) => piece.extents);
  const total = clustersIn(all);
  const wanted = length.trim() ? Number(length.replace(/\D/g, "")) : null;

  const changed = (next: Piece[]) => {
    setPieces(next);
    setTested(null);
    setShown(null);
    setSaved(null);
  };

  const addSelection = async () => {
    setProblem(null);
    try {
      let piece: Piece | null = null;
      if (selection.kind === "entry") {
        const result = await call<{ entry: { name: string; length: number }; extents: Extent[] }>("entry", { id: selection.id });
        piece = { label: result.entry.name, extents: result.extents };
        if (pieces.length === 0) {
          setName(result.entry.name);
          setLength(String(result.entry.length));
          setFromEntry({ id: selection.id, name: result.entry.name });
        }
      } else if (selection.kind === "chain") {
        const result = await call<{ extents: Extent[] }>("follow_chain", { cluster: selection.head, max: selection.length });
        piece = { label: t("Chain at cluster {cluster}", { cluster: count(selection.head) }), extents: result.extents };
      } else if (selection.kind === "cluster") {
        piece = { label: t("Cluster {cluster}", { cluster: count(selection.cluster) }), extents: [{ first: selection.cluster, count: 1 }] };
      } else if (selection.kind === "region") {
        piece = {
          label: t("Clusters {start} to {end}", { start: count(selection.startCluster), end: count(selection.endCluster - 1) }),
          extents: [{ first: selection.startCluster, count: selection.endCluster - selection.startCluster }],
        };
      }
      if (piece && piece.extents.length > 0) changed([...pieces, piece]);
    } catch (reason) {
      setProblem(fromEngine(String(reason)));
    }
  };

  const move = (index: number, by: number) => {
    const next = [...pieces];
    const [item] = next.splice(index, 1);
    next.splice(index + by, 0, item);
    changed(next);
  };

  const args = () => ({ extents: all, name: name.trim(), ...(wanted ? { length: wanted } : {}) });

  const run = async (what: "test" | "show" | "save") => {
    setBusy(what);
    setProblem(null);
    try {
      if (what === "test") setTested(await call<StreamTest>("test_stream", args()));
      if (what === "show") setShown(await call<Materialized>("materialize", args()));
      if (what === "save" && fromEntry) {
        await call("add_overlay", {
          kind: "file_extents",
          target: fromEntry.id,
          payload: { extents: all },
          note: t("Put back together in the workbench from {count} pieces.", { count: count(pieces.length) }),
        });
        setSaved(t("Saved as the clusters of {name}. Copies and previews now use them.", { name: fromEntry.name }));
        onChanged();
      }
    } catch (reason) {
      setProblem(fromEngine(String(reason)));
    } finally {
      setBusy(null);
    }
  };

  const canAdd = selection.kind === "entry" || selection.kind === "chain" || selection.kind === "cluster" || selection.kind === "region";

  return (
    <div className="workbench">
      <p className="muted">{t("Put a file back together by hand. Select a file, a chain, a cluster or a region, add it here, and put the pieces in order. Testing and showing read them live from the disk and copy nothing.")}</p>
      <div className="detail-actions">
        <button disabled={!canAdd} onClick={() => void addSelection()}><Icon name="plus" size={13} /> {t("Add the selection")}</button>
        <button className="link" disabled={pieces.length === 0} onClick={() => { changed([]); setFromEntry(null); setName(""); setLength(""); }}>{t("Start again")}</button>
      </div>
      {pieces.length > 0 && (
        <ol className="workbench-pieces">
          {pieces.map((piece, index) => (
            <li key={index} className="workbench-piece">
              <span className="workbench-piece-label">{piece.label}</span>
              <span className="muted">{t("{count} clusters", { count: count(clustersIn(piece.extents)) })}</span>
              <button className="icon-button" aria-label={t("Move up")} disabled={index === 0} onClick={() => move(index, -1)}><Icon name="chevronUp" size={12} /></button>
              <button className="icon-button" aria-label={t("Move down")} disabled={index === pieces.length - 1} onClick={() => move(index, 1)}><Icon name="chevronDown" size={12} /></button>
              <button className="icon-button" aria-label={t("Remove")} onClick={() => changed(pieces.filter((_, other) => other !== index))}><Icon name="close" size={12} /></button>
            </li>
          ))}
        </ol>
      )}
      {pieces.length > 0 && (
        <>
          <div className="workbench-fields">
            <label>
              <span>{t("Name, for its type")}</span>
              <input value={name} onChange={(event) => { setName(event.target.value); setTested(null); setShown(null); }} placeholder="photo.jpg" spellCheck={false} />
            </label>
            <label>
              <span>{t("Length in bytes")}</span>
              <input value={length} onChange={(event) => { setLength(event.target.value); setTested(null); setShown(null); }} placeholder={String(total * clusterSize)} inputMode="numeric" />
            </label>
          </div>
          <p className="muted">{t("{count} clusters, {bytes} in all.", { count: count(total), bytes: bytes(total * clusterSize) })}</p>
          <div className="detail-actions">
            <button disabled={busy !== null} onClick={() => void run("test")}>{busy === "test" ? t("Testing") : t("Test it")}</button>
            <button disabled={busy !== null || !name.trim()} onClick={() => void run("show")}>{busy === "show" ? t("Reading") : t("Show it")}</button>
            <button className="link" onClick={() => onShowClusters(all)}>{t("Show on the map")}</button>
            {fromEntry && (
              <button className="primary" disabled={busy !== null || report === null} title={report === null ? t("Test it first") : undefined} onClick={() => void run("save")}>
                {t("Keep as the clusters of {name}", { name: fromEntry.name })}
              </button>
            )}
          </div>
        </>
      )}
      {report && (
        <div className={report.breaks.length === 0 && report.reachedEnd ? "workbench-report good" : "workbench-report bad"}>
          <strong>
            {report.format === "unknown"
              ? t("There is no test for this type, so only showing it can tell.")
              : t("{percent} of its {format} structure is valid.", { percent: percent(report.validFraction), format: report.format.toUpperCase() })}
          </strong>
          {report.breaks.length > 0 && tested && <span>{t("It breaks at {place}.", { place: breakText(tested) })}</span>}
          {report.notes.slice(0, 3).map((note) => <span key={note} className="muted">{fromEngine(note)}</span>)}
        </div>
      )}
      {saved && <p className="workbench-report good">{saved}</p>}
      {problem && <p className="workbench-report bad">{problem}</p>}
      {shown?.kind === "none" && <p className="muted">{t("This kind of file cannot be shown here.")}</p>}
      {shown?.kind === "too-big" && <p className="muted">{t("This picture is too big to show here.")}</p>}
      {shown?.kind === "image" && shown.dataBase64 && (
        <div className="member-preview"><img src={`data:${shown.mime ?? "image/jpeg"};base64,${shown.dataBase64}`} alt={name} /></div>
      )}
      {shown?.kind === "text" && <div className="member-preview"><pre>{shown.text}</pre></div>}
    </div>
  );
}
