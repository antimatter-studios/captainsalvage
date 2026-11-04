import { useEffect, useState } from "react";
import { call, type IslandRow, type Page, type Row, type UnexplainedRun } from "../api";
import { bytes, count, displayPath, percent, VERDICT_LABEL } from "../format";
import { ConfidenceRing, fileIcon, Icon, verdictTone } from "./icons";

const PAGE_SIZE = 200;

interface RowProps {
  floor: number;
  revision: number;
  selectedKey: string | null;
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
      title={checked ? "In the recovery basket" : "Add to the recovery basket"}
      onChange={(event) => onChange(event.target.checked)}
    />
  );
}

function FileRow({ row, selected, onSelect, showPath, basket, onBasket }: { row: Row; selected: boolean; onSelect: () => void; showPath?: boolean; basket: Set<string>; onBasket: RowProps["onBasket"] }) {
  const verdict = VERDICT_LABEL[row.verdict] ?? row.verdict;
  const id = row.id as number;
  return (
    <div className="row-wrap">
    <button className={selected ? "row row-selected" : "row"} onClick={onSelect} title={row.reason}>
      <span className="row-icon"><Icon name={row.kind === "file" ? fileIcon(row.name) : "folder"} /></span>
      <span className="row-main">
        <span className="row-name">{row.name}</span>
        {showPath && row.path !== null && <span className="row-path">{displayPath(row.path)}</span>}
      </span>
      <span className="row-size">{row.kind === "file" ? bytes(row.length) : ""}</span>
      <ConfidenceRing value={row.confidence} tone={verdictTone(row.verdict)} label={`${verdict}, confidence ${percent(row.confidence)}`} />
    </button>
    {row.kind === "file" && row.length > 0 && (
      <BasketCheck checked={basket.has(`entry:${id}`)} label={`Add ${row.name} to the recovery basket`} onChange={(add) => onBasket("entry", id, add)} />
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

  const name = row.name || (row.nodeKind === "root" ? "Volume root" : `Lost folder ${count(head)}`);
  const key = `folder:${head}`;
  return (
    <>
      <div className="row-wrap">
      <button
        className={props.selectedKey === key ? "row row-selected" : "row"}
        style={{ paddingLeft: 6 + depth * 14 }}
        aria-expanded={open}
        title={row.hint ? `Contains: ${row.hint}` : undefined}
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
          <ConfidenceRing value={row.linkConfidence} tone="tone-serious" label={`Link to its parent folder: confidence ${percent(row.linkConfidence)}`} />
        ) : null}
      </button>
      {row.nodeKind !== "root" && (
        <BasketCheck checked={props.basket.has(`folder:${head}`)} label={`Add ${name} to the recovery basket`} onChange={(add) => props.onBasket("folder", head, add)} />
      )}
      </div>
      {open && error && <div className="empty" style={{ paddingLeft: 20 + depth * 14 }}>{error}</div>}
      {open &&
        children?.rows.map((child) =>
          child.kind === "folder" ? (
            <FolderRow key={`f${child.head}`} row={child} depth={depth + 1} props={props} />
          ) : (
            <div key={`e${child.id}`} style={{ paddingLeft: depth * 14 + 17 }}>
              <FileRow row={child} selected={props.selectedKey === `entry:${child.id}`} onSelect={() => props.onEntry(child)} basket={props.basket} onBasket={props.onBasket} />
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
  if (!page) return <div className="empty">Loading folders.</div>;
  if (page.rows.length <= 1 && page.rows[0]?.bytes === 0) {
    return <div className="empty">No folders found yet. Folders appear after the scan has found them and the map has been rebuilt.</div>;
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
        <span>Starts with</span>
        <select value={headClass ?? ""} onChange={(event) => setHeadClass(event.target.value === "" ? null : Number(event.target.value))}>
          <option value="">Anything</option>
          {classNames.map((name, index) => (index === 0 ? null : <option key={name} value={index}>{name}</option>))}
        </select>
      </label>
      <p className="hint">FAT chains no entry claims. These can be files whose names were lost.</p>
      {result && result.chains.length === 0 && <div className="empty">No chains match. They appear once the map has been rebuilt.</div>}
      {result?.chains.map((chain) => (
        <button key={chain.head} className={selectedKey === `chain:${chain.head}` ? "row row-selected" : "row"} onClick={() => onChain(chain)}>
          <span className="row-icon"><Icon name={chain.endKind === "eof" ? "link" : "broken"} /></span>
          <span className="row-main">
            <span className="row-name">{chain.headClassName}</span>
            <span className="row-path">Cluster {count(chain.head)}{chain.endKind === "eof" ? "" : `, chain ${chain.endKind === "broken" ? "breaks" : "joins another"}`}</span>
          </span>
          <span className="row-size">{bytes(chain.length * clusterSize)}</span>
        </button>
      ))}
      {result && result.total > limit && <button className="more" onClick={() => setLimit(limit + PAGE_SIZE)}>Show more ({count(result.total - limit)} left)</button>}
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
      <p className="hint">Files whose FAT chain ends before their length says it should. Most missing data first.</p>
      {rows && rows.length === 0 && <div className="empty">No broken chains found so far.</div>}
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
      <p className="hint">Free space no entry or chain accounts for yet. This shrinks as the scan finds more folders. Select a region and ask Claude what it is.</p>
      {error && <div className="empty">{error}</div>}
      {runs && runs.length === 0 && <div className="empty">Nothing unexplained so far. Regions appear after the map is rebuilt.</div>}
      {runs?.map((run) => (
        <button key={run.startCluster} className={selectedKey === `region:${run.startCluster}` ? "row row-selected" : "row"} onClick={() => onRegion(run)}>
          <span className="row-icon"><Icon name="question" /></span>
          <span className="row-main">
            <span className="row-name">{run.mainContent}</span>
            <span className="row-path">Cluster {count(run.startCluster)}, {percent(run.mainContentShare)} of it this type</span>
          </span>
          <span className="row-size">{bytes(run.bytes)}</span>
        </button>
      ))}
    </>
  );
}

function SearchResults({ page, onEntry, onFolder, selectedKey, basket, onBasket }: { page: Page; onEntry: (row: Row) => void; onFolder: (row: Row) => void; selectedKey: string | null; basket: Set<string>; onBasket: RowProps["onBasket"] }) {
  if (page.rows.length === 0) return <div className="empty">No names match. The scan may not have reached them yet.</div>;
  return (
    <>
      {page.rows.map((row) =>
        row.kind === "folder" ? (
          <button key={row.id} className="row" onClick={() => onFolder(row)}>
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
  const tabs: [ExplorerTab, string][] = [["folders", "Folders"], ["nameless", "Nameless"], ["broken", "Broken"], ["unexplained", "Unexplained"]];

  async function runSearch(event: React.FormEvent) {
    event.preventDefault();
    const text = query.trim();
    setResults(text ? await call<Page>("search", { query: text, limit: PAGE_SIZE, minConfidence: props.floor }) : null);
  }

  return (
    <nav className="explorer" aria-label="Found files">
      {props.handle}
      <form className="sidebar-search" onSubmit={runSearch}>
        <Icon name="search" size={14} />
        <input
          value={query}
          onChange={(event) => {
            setQuery(event.target.value);
            if (!event.target.value) setResults(null);
          }}
          placeholder="Search names"
          aria-label="Search names"
          spellCheck={false}
        />
        {query && (
          <button type="button" aria-label="Clear search" onClick={() => { setQuery(""); setResults(null); }}><Icon name="close" size={12} /></button>
        )}
      </form>
      {!results && (
        <div className="segmented" role="tablist">
          {tabs.map(([id, label]) => (
            <button key={id} role="tab" aria-selected={tab === id} onClick={() => setTab(id)}>{label}</button>
          ))}
        </div>
      )}
      <div className="panel-body">
        {results ? (
          <SearchResults page={results} onEntry={props.onEntry} onFolder={props.onFolder} selectedKey={props.selectedKey} basket={props.basket} onBasket={props.onBasket} />
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
