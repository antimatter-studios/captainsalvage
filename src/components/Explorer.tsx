import { useEffect, useRef, useState } from "react";
import { call, type IslandRow, type Page, type Row, type UnexplainedRun } from "../api";
import { bytes, count, displayPath, lostFolderName, percent, verdictLabel } from "../format";
import { fromEngine, t } from "../i18n";
import { startPieceDrag } from "./RebuildView";
import { ConfidenceRing, fileIcon, Icon, verdictTone } from "./icons";

const PAGE_SIZE = 200;
/** Holding an arrow key should not open every file it passes. */
const KEY_SELECT_WAIT_MILLIS = 120;
const RECENT_KEY_MILLIS = 1000;
const NAVIGATION_KEYS = new Set(["ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Home", "End"]);

/** A folder's click also opens it, so the arrow keys reach these instead. */
interface FolderActions {
  isOpen: boolean;
  setOpen: (open: boolean) => void;
  select: () => void;
}
const folderActions = new WeakMap<Element, FolderActions>();

function depthOf(row: Element): number | null {
  const text = (row as HTMLElement).dataset.depth;
  return text === undefined ? null : Number(text);
}

/** A selected row looks selected. The open folder keeps a quieter mark when something else is. */
function rowClass(key: string, selectedKey: string | null, openKey: string | null): string {
  if (selectedKey === key) return "row row-selected";
  return openKey === key ? "row row-open" : "row";
}

interface RowProps {
  floor: number;
  revision: number;
  selectedKey: string | null;
  /** The folder whose files are showing. It stays marked while other things are selected. */
  openKey: string | null;
  /** While a canvas is showing, top-level folders are its pieces: unused ones can be dragged. */
  canvasPieces: Set<number> | null;
  onEntry: (row: Row) => void;
  onFolder: (row: Row) => void;
  basket: Set<string>;
  onBasket: (kind: "entry" | "folder", target: number, add: boolean) => void;
}

function BasketCheck({ checked, label, onChange }: { checked: boolean; label: string; onChange: (add: boolean) => void }) {
  return (
    <input
      className="basket-check"
      type="checkbox"
      checked={checked}
      aria-label={label}
      title={checked ? t("On the treasure list") : t("Add to the treasure list")}
      onChange={(event) => onChange(event.target.checked)}
    />
  );
}

function FileRow({ row, selected, onSelect, showPath, basket, onBasket, depth }: { row: Row; selected: boolean; onSelect: () => void; showPath?: boolean; basket: Set<string>; onBasket: RowProps["onBasket"]; depth?: number }) {
  const verdict = verdictLabel(row.verdict);
  const id = row.id as number;
  return (
    <div className="row-wrap">
    <button className={selected ? "row row-selected" : "row"} onClick={onSelect} title={fromEngine(row.reason)} data-depth={depth}>
      <span className="row-icon"><Icon name={row.kind === "file" ? fileIcon(row.name) : "folder"} /></span>
      <span className="row-main">
        <span className="row-name">{row.name}</span>
        {showPath && row.path !== null && <span className="row-path">{displayPath(row.path)}</span>}
      </span>
      <span className="row-size">{row.kind === "file" ? bytes(row.length) : ""}</span>
      <ConfidenceRing value={row.confidence} tone={verdictTone(row.verdict)} label={t("{verdict}, confidence {percent}", { verdict, percent: percent(row.confidence) })} />
    </button>
    {row.kind === "file" && row.length > 0 && (
      <BasketCheck checked={basket.has(`entry:${id}`)} label={t("Add {name} to the treasure list", { name: row.name })} onChange={(add) => onBasket("entry", id, add)} />
    )}
    </div>
  );
}

function FolderRow({ row, depth, props }: { row: Row; depth: number; props: RowProps }) {
  const [open, setOpen] = useState(false);
  const [children, setChildren] = useState<Page | null>(null);
  const [loaded, setLoaded] = useState(PAGE_SIZE);
  const [error, setError] = useState<string | null>(null);
  const head = row.head as number;

  useEffect(() => {
    if (!open) return;
    let current = true;
    call<Page>("children", { node: head, limit: loaded, minConfidence: props.floor })
      .then((page) => {
        if (current) {
          setChildren(page);
          setError(null);
        }
      })
      .catch((reason) => current && setError(String(reason)));
    return () => {
      current = false;
    };
  }, [open, head, loaded, props.floor, props.revision]);

  const name = row.name || (row.nodeKind === "root" ? t("Volume root") : lostFolderName(head));
  // Top-level folders are the pieces. A folder inside one can be placed too, as a layer of its
  // own, and is only marked used when it was placed itself.
  const piece = props.canvasPieces !== null;
  const used = piece && (props.canvasPieces?.has(head) ?? false);
  const key = `folder:${head}`;
  return (
    <>
      <div className="row-wrap">
      <button
        ref={(element) => {
          if (element) folderActions.set(element, { isOpen: open, setOpen, select: () => props.onFolder(row) });
        }}
        className={rowClass(key, props.selectedKey, props.openKey) + (used ? " row-used" : "")}
        aria-current={props.openKey === key ? "true" : undefined}
        style={{ paddingLeft: 6 + depth * 14 }}
        aria-expanded={open}
        data-depth={depth}
        draggable={piece && !used}
        onDragStart={(event) => startPieceDrag(event, head)}
        title={used ? t("Already on the canvas") : piece ? t("Drag onto the canvas") : row.hint ? t("Contains: {hint}", { hint: row.hint }) : undefined}
        onClick={() => {
          setOpen(!open);
          props.onFolder(row);
        }}
      >
        <span className={open ? "disclosure disclosure-open" : "disclosure"}><Icon name="chevronRight" size={11} /></span>
        <span className="row-icon row-icon-folder"><Icon name="folder" /></span>
        <span className="row-main">
          <span className="row-name">{name}</span>
          {!row.name && row.hint && <span className="row-path">{row.hint}</span>}
        </span>
        <span className="row-size">{row.bytes > 0 ? bytes(row.bytes) : ""}</span>
        {row.linkConfidence !== null && row.linkConfidence < 1 ? (
          <ConfidenceRing value={row.linkConfidence} tone="tone-serious" label={t("Link to its parent folder: confidence {percent}", { percent: percent(row.linkConfidence) })} />
        ) : null}
      </button>
      {row.nodeKind !== "root" && (
        <BasketCheck checked={props.basket.has(`folder:${head}`)} label={t("Add {name} to the treasure list", { name })} onChange={(add) => props.onBasket("folder", head, add)} />
      )}
      </div>
      {open && error && <div className="empty" style={{ paddingLeft: 20 + depth * 14 }}>{fromEngine(error)}</div>}
      {open &&
        children?.rows.map((child) =>
          child.kind === "folder" ? (
            <FolderRow key={`f${child.head}`} row={child} depth={depth + 1} props={props} />
          ) : (
            <div key={`e${child.id}`} style={{ paddingLeft: depth * 14 + 17 }}>
              <FileRow row={child} selected={props.selectedKey === `entry:${child.id}`} onSelect={() => props.onEntry(child)} basket={props.basket} onBasket={props.onBasket} depth={depth + 1} />
            </div>
          ),
        )}
      {open && children && children.total > loaded && (
        <button className="more" style={{ marginLeft: 20 + depth * 14 }} onClick={() => setLoaded(loaded + PAGE_SIZE)}>
          Show more files ({count(children.total - loaded)} left)
        </button>
      )}
    </>
  );
}

function Folders(props: RowProps) {
  const [page, setPage] = useState<Page | null>(null);
  const [limit, setLimit] = useState(PAGE_SIZE);
  useEffect(() => {
    let current = true;
    call<Page>("roots", { limit }).then((result) => current && setPage(result)).catch(() => undefined);
    return () => {
      current = false;
    };
  }, [limit, props.revision]);
  if (!page) return <div className="empty">{t("Loading folders.")}</div>;
  if (page.rows.length <= 1 && page.rows[0]?.bytes === 0) {
    return <div className="empty">{t("No folders found yet. Folders appear after the scan has found them and the map has been rebuilt.")}</div>;
  }
  return (
    <>
      {page.rows.map((row) => (
        <FolderRow key={row.head} row={row} depth={0} props={props} />
      ))}
      {page.total > limit && (
        <button className="more" onClick={() => setLimit(limit + PAGE_SIZE)}>
          Show more folders ({count(page.total - limit)} left)
        </button>
      )}
    </>
  );
}

function Nameless({ onChain, selectedKey, revision, classNames, clusterSize }: { onChain: (chain: IslandRow) => void; selectedKey: string | null; revision: number; classNames: string[]; clusterSize: number }) {
  const [result, setResult] = useState<{ chains: IslandRow[]; total: number } | null>(null);
  const [headClass, setHeadClass] = useState<number | null>(null);
  const [limit, setLimit] = useState(PAGE_SIZE);
  useEffect(() => {
    let current = true;
    const args = headClass === null ? { limit } : { limit, headClass };
    call<{ chains: IslandRow[]; total: number }>("nameless_chains", args).then((value) => current && setResult(value)).catch(() => undefined);
    return () => {
      current = false;
    };
  }, [headClass, limit, revision]);
  return (
    <>
      <label className="field">
        <span>{t("Starts with")}</span>
        <select value={headClass ?? ""} onChange={(event) => setHeadClass(event.target.value === "" ? null : Number(event.target.value))}>
          <option value="">{t("Anything")}</option>
          {classNames.map((name, index) => (index === 0 ? null : <option key={name} value={index}>{fromEngine(name)}</option>))}
        </select>
      </label>
      <p className="hint">{t("FAT chains no entry claims. These can be files whose names were lost.")}</p>
      {result && result.chains.length === 0 && <div className="empty">{t("No chains match. They appear once the map has been rebuilt.")}</div>}
      {result?.chains.map((chain) => (
        <button key={chain.head} className={selectedKey === `chain:${chain.head}` ? "row row-selected" : "row"} onClick={() => onChain(chain)}>
          <span className="row-icon"><Icon name={chain.endKind === "eof" ? "link" : "broken"} /></span>
          <span className="row-main">
            <span className="row-name">{fromEngine(chain.headClassName)}</span>
            <span className="row-path">{chain.endKind === "eof" ? t("Cluster {cluster}", { cluster: count(chain.head) }) : chain.endKind === "broken" ? t("Cluster {cluster}, chain breaks", { cluster: count(chain.head) }) : t("Cluster {cluster}, chain joins another", { cluster: count(chain.head) })}</span>
          </span>
          <span className="row-size">{bytes(chain.length * clusterSize)}</span>
        </button>
      ))}
      {result && result.total > limit && <button className="more" onClick={() => setLimit(limit + PAGE_SIZE)}>{t("Show more ({count} left)", { count: count(result.total - limit) })}</button>}
    </>
  );
}

function Broken({ onEntry, selectedKey, revision, basket, onBasket }: { onEntry: (row: Row) => void; selectedKey: string | null; revision: number; basket: Set<string>; onBasket: RowProps["onBasket"] }) {
  const [rows, setRows] = useState<Row[] | null>(null);
  useEffect(() => {
    let current = true;
    call<Row[]>("broken_files", { limit: PAGE_SIZE }).then((value) => current && setRows(value)).catch(() => undefined);
    return () => {
      current = false;
    };
  }, [revision]);
  return (
    <>
      <p className="hint">{t("Files whose FAT chain ends before their length says it should. Most missing data first.")}</p>
      {rows && rows.length === 0 && <div className="empty">{t("No broken chains found so far.")}</div>}
      {rows?.map((row) => (
        <FileRow key={row.id} row={row} showPath selected={selectedKey === `entry:${row.id}`} onSelect={() => onEntry(row)} basket={basket} onBasket={onBasket} />
      ))}
    </>
  );
}

function Unexplained({ onRegion, selectedKey, revision }: { onRegion: (run: UnexplainedRun) => void; selectedKey: string | null; revision: number }) {
  const [runs, setRuns] = useState<UnexplainedRun[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let current = true;
    call<UnexplainedRun[]>("unexplained_runs", { limit: PAGE_SIZE })
      .then((value) => current && setRuns(value))
      .catch((reason) => current && setError(String(reason)));
    return () => {
      current = false;
    };
  }, [revision]);
  return (
    <>
      <p className="hint">{t("Free space no entry or chain accounts for yet. This shrinks as the scan finds more folders. Select a region and ask Claude what it is.")}</p>
      {error && <div className="empty">{fromEngine(error)}</div>}
      {runs && runs.length === 0 && <div className="empty">{t("Nothing unexplained so far. Regions appear after the map is rebuilt.")}</div>}
      {runs?.map((run) => (
        <button key={run.startCluster} className={selectedKey === `region:${run.startCluster}` ? "row row-selected" : "row"} onClick={() => onRegion(run)}>
          <span className="row-icon"><Icon name="question" /></span>
          <span className="row-main">
            <span className="row-name">{fromEngine(run.mainContent)}</span>
            <span className="row-path">{t("Cluster {cluster}, {percent} of it this type", { cluster: count(run.startCluster), percent: percent(run.mainContentShare) })}</span>
          </span>
          <span className="row-size">{bytes(run.bytes)}</span>
        </button>
      ))}
    </>
  );
}

function SearchResults({ page, onEntry, onFolder, selectedKey, openKey, basket, onBasket }: { page: Page; onEntry: (row: Row) => void; onFolder: (row: Row) => void; selectedKey: string | null; openKey: string | null; basket: Set<string>; onBasket: RowProps["onBasket"] }) {
  if (page.rows.length === 0) return <div className="empty">{t("No names match. The scan may not have reached them yet.")}</div>;
  return (
    <>
      {page.rows.map((row) =>
        row.kind === "folder" ? (
          <button
            key={row.id}
            className={rowClass(`folder:${row.head ?? row.firstCluster}`, selectedKey, openKey)}
            aria-current={openKey === `folder:${row.head ?? row.firstCluster}` ? "true" : undefined}
            onClick={() => onFolder(row)}
          >
            <span className="row-icon row-icon-folder"><Icon name="folder" /></span>
            <span className="row-main">
              <span className="row-name">{row.name}</span>
              <span className="row-path">{displayPath(row.path)}</span>
            </span>
          </button>
        ) : (
          <FileRow key={row.id} row={row} showPath selected={selectedKey === `entry:${row.id}`} onSelect={() => onEntry(row)} basket={basket} onBasket={onBasket} />
        ),
      )}
    </>
  );
}

type ExplorerTab = "folders" | "nameless" | "broken" | "unexplained";

export interface ExplorerProps extends RowProps {
  onChain: (chain: IslandRow) => void;
  onRegion: (run: UnexplainedRun) => void;
  classNames: string[];
  clusterSize: number;
  handle: React.ReactNode;
}

export function Explorer(props: ExplorerProps) {
  const [tab, setTab] = useState<ExplorerTab>("folders");
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<Page | null>(null);
  const tabs: [ExplorerTab, string][] = [["folders", t("Folders")], ["nameless", t("Nameless")], ["broken", t("Broken")], ["unexplained", t("Unexplained")]];
  const panel = useRef<HTMLDivElement>(null);
  const selectTimer = useRef<number | undefined>(undefined);
  const lastKey = useRef(0);

  const clearCursor = () => panel.current?.querySelectorAll(".row-cursor").forEach((row) => row.classList.remove("row-cursor"));

  useEffect(() => {
    panel.current?.focus({ preventScroll: true });
    return () => window.clearTimeout(selectTimer.current);
  }, []);

  // A selection made elsewhere, such as on the map, takes over from the keys.
  useEffect(() => {
    if (Date.now() - lastKey.current > RECENT_KEY_MILLIS) clearCursor();
  }, [props.selectedKey]);

  const moveTo = (row: Element) => {
    clearCursor();
    row.classList.add("row-cursor");
    row.scrollIntoView({ block: "nearest" });
    window.clearTimeout(selectTimer.current);
    selectTimer.current = window.setTimeout(() => {
      const actions = folderActions.get(row);
      if (actions) actions.select();
      else (row as HTMLElement).click();
    }, KEY_SELECT_WAIT_MILLIS);
  };

  const onKeyDown = (event: React.KeyboardEvent) => {
    if (!NAVIGATION_KEYS.has(event.key) || (event.target as HTMLElement).closest("input, select, textarea")) return;
    const element = panel.current;
    if (!element) return;
    const rows = Array.from(element.querySelectorAll("button.row"));
    if (rows.length === 0) return;
    // Handled here, so an open preview does not also move to its next file.
    event.preventDefault();
    event.stopPropagation();
    lastKey.current = Date.now();
    const current = element.querySelector(".row-cursor") ?? element.querySelector(".row-selected");
    const index = current ? rows.indexOf(current) : -1;
    const actions = current ? folderActions.get(current) : undefined;
    const depth = current ? depthOf(current) : null;
    let next: Element | undefined;
    if (event.key === "Home") next = rows[0];
    else if (event.key === "End") next = rows[rows.length - 1];
    else if (index < 0) next = rows[0];
    else if (event.key === "ArrowDown") next = rows[index + 1];
    else if (event.key === "ArrowUp") next = rows[index - 1];
    else if (event.key === "ArrowRight" && actions) {
      if (!actions.isOpen) actions.setOpen(true);
      else if (rows[index + 1] && depth !== null && (depthOf(rows[index + 1]) ?? 0) > depth) next = rows[index + 1];
    } else if (event.key === "ArrowLeft") {
      if (actions?.isOpen) actions.setOpen(false);
      else if (depth !== null) next = rows.slice(0, index).reverse().find((row) => (depthOf(row) ?? depth) < depth);
    }
    if (next) moveTo(next);
  };

  async function runSearch(event: React.FormEvent) {
    event.preventDefault();
    const text = query.trim();
    setResults(text ? await call<Page>("search", { query: text, limit: PAGE_SIZE, minConfidence: props.floor }) : null);
  }

  return (
    <nav className="explorer" aria-label={t("Found files")}>
      {props.handle}
      <form className="sidebar-search" onSubmit={runSearch}>
        <Icon name="search" size={14} />
        <input
          value={query}
          onChange={(event) => {
            setQuery(event.target.value);
            if (!event.target.value) setResults(null);
          }}
          placeholder={t("Search names")}
          aria-label={t("Search names")}
          spellCheck={false}
        />
        {query && (
          <button type="button" aria-label={t("Clear search")} onClick={() => { setQuery(""); setResults(null); }}><Icon name="close" size={12} /></button>
        )}
      </form>
      {!results && (
        <div className="segmented" role="tablist">
          {tabs.map(([id, label]) => (
            <button key={id} role="tab" aria-selected={tab === id} onClick={() => setTab(id)}>{label}</button>
          ))}
        </div>
      )}
      <div
        className="panel-body explorer-rows"
        ref={panel}
        tabIndex={0}
        aria-label={t("Found files. Use the arrow keys to move through them.")}
        onKeyDown={onKeyDown}
        onClick={(event) => {
          clearCursor();
          if (!(event.target as HTMLElement).closest("input, select, textarea")) panel.current?.focus({ preventScroll: true });
        }}
      >
        {results ? (
          <SearchResults page={results} onEntry={props.onEntry} onFolder={props.onFolder} selectedKey={props.selectedKey} openKey={props.openKey} basket={props.basket} onBasket={props.onBasket} />
        ) : (
          <>
            {tab === "folders" && <Folders {...props} />}
            {tab === "nameless" && <Nameless onChain={props.onChain} selectedKey={props.selectedKey} revision={props.revision} classNames={props.classNames} clusterSize={props.clusterSize} />}
            {tab === "broken" && <Broken onEntry={props.onEntry} selectedKey={props.selectedKey} revision={props.revision} basket={props.basket} onBasket={props.onBasket} />}
            {tab === "unexplained" && <Unexplained onRegion={props.onRegion} selectedKey={props.selectedKey} revision={props.revision} />}
          </>
        )}
      </div>
    </nav>
  );
}
