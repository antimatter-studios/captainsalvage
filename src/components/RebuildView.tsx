import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useState, type DragEvent, type ReactNode } from "react";
import { call, type AgentChanges, type UndoReport, type DiskRebuild, type DiskRebuildItem, type CopyStatus, type FinderState, type DiskRebuildSuggestion, type DiskRebuildListing, type DiskRebuildPlace, type RebuiltFile, type RebuiltFolder, type Row, type UnusedPieces } from "../api";
import { undoBar } from "../agentChanges";
import { bytes, count, lostFolderName, percent, verdictLabel } from "../format";
import { fromEngine, t } from "../i18n";
import { ConfidenceRing, fileIcon, Icon, verdictTone } from "./icons";
import { DiskRebuildCopy } from "./DiskRebuildCopy";
import { SimilarPieces } from "./SimilarPieces";

/** What a dragged sidebar piece carries: the first cluster of its folder. */
export const PIECE_TYPE = "application/x-salvage-piece";
/** What a dragged diskRebuild item carries, to move it. */
const ITEM_TYPE = "application/x-salvage-item";
/** What a file dragged from Browse files carries: its entry id. */
export const ENTRY_TYPE = "application/x-salvage-entry";
const FILES_PAGE = 200;
const INDENT = 14;

/** Sets what a drag from the sidebar carries. */
export function startPieceDrag(event: DragEvent, head: number) {
  event.dataTransfer.setData(PIECE_TYPE, String(head));
  event.dataTransfer.effectAllowed = "copyMove";
}

/** Sets what a file dragged from Browse files carries. */
export function startEntryDrag(event: DragEvent, entry: number) {
  event.dataTransfer.setData(ENTRY_TYPE, String(entry));
  event.dataTransfer.effectAllowed = "copy";
}

interface Dragged {
  piece?: number;
  item?: number;
  entry?: number;
}

function dragged(event: DragEvent): Dragged | null {
  const piece = event.dataTransfer.getData(PIECE_TYPE);
  const item = event.dataTransfer.getData(ITEM_TYPE);
  const entry = event.dataTransfer.getData(ENTRY_TYPE);
  if (piece) return { piece: Number(piece) };
  if (item) return { item: Number(item) };
  if (entry) return { entry: Number(entry) };
  return null;
}

function carriesSomething(event: DragEvent): boolean {
  return [PIECE_TYPE, ITEM_TYPE, ENTRY_TYPE].some((type) => event.dataTransfer.types.includes(type));
}

interface Shared {
  diskRebuild: number;
  items: Map<number, DiskRebuildItem>;
  revision: number;
  changed: () => void;
  fail: (reason: unknown) => void;
  onEntry: (row: Row) => void;
}

function folderLabel(folder: RebuiltFolder): string {
  return folder.name ?? (folder.lostHead !== null ? lostFolderName(folder.lostHead) : t("Unnamed folder"));
}

/** Where a drop on this folder puts things: in its own item, or in its found folder inside one. */
function placeOf(folder: RebuiltFolder, around: number | null): DiskRebuildPlace | null {
  if (folder.items.length > 0) return { item: folder.items[0] };
  if (around !== null && folder.heads.length > 0) return { item: around, head: folder.heads[0] };
  return null;
}

/** Drops a piece or an item onto a place, inside it or merged into it. */
async function drop(shared: Shared, what: Dragged, place: DiskRebuildPlace | undefined, merge: boolean) {
  try {
    if (what.piece !== undefined) {
      await call("disk_rebuild_place", { diskRebuild: shared.diskRebuild, head: what.piece, place: place ?? null, merge });
    } else if (what.item !== undefined) {
      await call("disk_rebuild_move", { diskRebuild: shared.diskRebuild, item: what.item, place: place ?? null, merge });
    } else if (what.entry !== undefined) {
      // A single file always joins the folder it lands on, as a layer of one file.
      await call("disk_rebuild_place_file", { diskRebuild: shared.diskRebuild, entry: what.entry, place: place ?? null });
    }
    shared.changed();
  } catch (reason) {
    shared.fail(reason);
  }
}

/** The two places a drag can land on a folder, shown while something is held over it. */
function DropChoices({ onInside, onMerge }: { onInside: (event: DragEvent) => void; onMerge: (event: DragEvent) => void }) {
  const zone = (label: string, handler: (event: DragEvent) => void, className: string) => (
    <span
      className={`drop-zone ${className}`}
      onDragOver={(event) => {
        event.preventDefault();
        event.stopPropagation();
      }}
      onDrop={(event) => {
        event.preventDefault();
        event.stopPropagation();
        handler(event);
      }}
    >
      {label}
    </span>
  );
  return (
    <span className="drop-choices">
      {zone(t("Put inside"), onInside, "drop-inside")}
      {zone(t("Merge with it"), onMerge, "drop-merge")}
    </span>
  );
}

function NameInput({ initial, onDone }: { initial: string; onDone: (name: string | null) => void }) {
  const [value, setValue] = useState(initial);
  return (
    <form
      className="disk-rebuild-name"
      onSubmit={(event) => {
        event.preventDefault();
        onDone(value.trim() || null);
      }}
    >
      <input
        autoFocus
        value={value}
        onChange={(event) => setValue(event.target.value)}
        onKeyDown={(event) => event.key === "Escape" && onDone(null)}
        onBlur={() => onDone(value.trim() || null)}
        aria-label={t("Folder name")}
        spellCheck={false}
      />
    </form>
  );
}

/** A small mark on what an agent made or changed. */
function AgentMark() {
  return (
    <span className="agent-mark" title={t("Made by the agent")}>
      <Icon name="sparkle" size={11} />
      <span>{t("Agent")}</span>
    </span>
  );
}

/** One folder of the rebuilt tree. Its contents load when it is opened. */
function FolderNode({ folder, path, depth, around, shared, startOpen }: {
  folder: RebuiltFolder;
  path: string[];
  depth: number;
  around: number | null;
  shared: Shared;
  startOpen?: boolean;
}) {
  const [open, setOpen] = useState(Boolean(startOpen));
  const [holding, setHolding] = useState(false);
  const [naming, setNaming] = useState<"new" | "rename" | null>(null);
  const [similar, setSimilar] = useState(false);
  const item = folder.items.length === 1 ? shared.items.get(folder.items[0]) : undefined;
  const place = placeOf(folder, around);
  const inside = folder.items.length > 0 ? folder.items[0] : around;
  const label = folderLabel(folder);

  const onDrop = (merge: boolean) => (event: DragEvent) => {
    setHolding(false);
    const what = dragged(event);
    if (what && place) void drop(shared, what, place, merge).then(() => setOpen(true));
  };

  async function makeFolder(name: string | null) {
    setNaming(null);
    if (!name || !place) return;
    try {
      await call("disk_rebuild_folder", { diskRebuild: shared.diskRebuild, name, place });
      setOpen(true);
      shared.changed();
    } catch (reason) {
      shared.fail(reason);
    }
  }

  async function rename(name: string | null) {
    setNaming(null);
    if (!item) return;
    try {
      await call("disk_rebuild_rename_item", { diskRebuild: shared.diskRebuild, item: item.id, name: name ?? (item.kind === "folder" ? label : null) });
      shared.changed();
    } catch (reason) {
      shared.fail(reason);
    }
  }

  async function swap(piece: Row) {
    if (!item || piece.head === null) return;
    try {
      await call("disk_rebuild_swap", { diskRebuild: shared.diskRebuild, item: item.id, head: piece.head });
      setSimilar(false);
      shared.changed();
    } catch (reason) {
      shared.fail(reason);
    }
  }

  async function remove() {
    if (!item) return;
    try {
      await call("disk_rebuild_remove", { diskRebuild: shared.diskRebuild, item: item.id });
      shared.changed();
    } catch (reason) {
      shared.fail(reason);
    }
  }

  const origin = folder.origin === "made" ? t("A folder you made") : folder.origin === "placed" ? t("A piece you placed") : t("Found inside a piece");
  return (
    <>
      <div
        className={holding ? "row-wrap disk-rebuild-row disk-rebuild-row-holding" : "row-wrap disk-rebuild-row"}
        onDragOver={(event) => {
          if (!place || !carriesSomething(event)) return;
          event.preventDefault();
          setHolding(true);
        }}
        onDragLeave={(event) => {
          if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setHolding(false);
        }}
        onDrop={(event) => {
          event.preventDefault();
          onDrop(false)(event);
        }}
      >
        <button
          className={`row disk-rebuild-folder disk-rebuild-origin-${folder.origin}`}
          style={{ paddingLeft: 6 + depth * INDENT }}
          aria-expanded={open}
          draggable={Boolean(item) && depth > 0}
          onDragStart={(event) => {
            if (!item) return;
            event.dataTransfer.setData(ITEM_TYPE, String(item.id));
            event.dataTransfer.effectAllowed = "move";
          }}
          onClick={() => setOpen(!open)}
          title={origin}
        >
          <span className={open ? "disclosure disclosure-open" : "disclosure"}><Icon name="chevronRight" size={11} /></span>
          <span className={folder.origin === "made" ? "row-icon row-icon-made" : "row-icon row-icon-folder"}><Icon name="folder" /></span>
          <span className="row-main">
            {naming === "rename" ? (
              <NameInput initial={folder.name ?? ""} onDone={(name) => void rename(name)} />
            ) : (
              <span className="row-name">{label}</span>
            )}
            {(folder.merged || folder.origin === "made") && (
              <span className="row-path">{folder.merged ? t("Merged from {count} pieces", { count: count(folder.items.length + folder.heads.length) }) : folder.madeByAgent ? t("Made by the agent") : t("Made by you")}</span>
            )}
          </span>
          {folder.madeByAgent && <AgentMark />}
          <span className="row-size" title={t("{bytes} likely intact", { bytes: bytes(folder.intactBytes) })}>{folder.bytes > 0 ? bytes(folder.bytes) : ""}</span>
        </button>
        {holding ? (
          <DropChoices onInside={onDrop(false)} onMerge={onDrop(true)} />
        ) : (
          <span className="disk-rebuild-actions">
            {place && (
              <button className="icon-button" title={t("New folder inside")} aria-label={t("New folder inside")} onClick={() => setNaming("new")}>
                <Icon name="plus" size={13} />
              </button>
            )}
            {item?.head != null && (
              <button className="icon-button" title={t("Similar pieces")} aria-label={t("Similar pieces")} aria-expanded={similar} onClick={() => setSimilar(!similar)}>
                <Icon name="puzzle" size={13} />
              </button>
            )}
            {item && (
              <button className="icon-button" title={t("Rename")} aria-label={t("Rename")} onClick={() => setNaming("rename")}>
                <Icon name="pencil" size={13} />
              </button>
            )}
            {item && (
              <button className="icon-button" title={t("Take off the Disk Rebuild")} aria-label={t("Take off the Disk Rebuild")} onClick={() => void remove()}>
                <Icon name="close" size={12} />
              </button>
            )}
          </span>
        )}
      </div>
      {naming === "new" && (
        <div className="disk-rebuild-new-folder" style={{ paddingLeft: 26 + (depth + 1) * INDENT }}>
          <Icon name="folder" />
          <NameInput initial="" onDone={(name) => void makeFolder(name)} />
        </div>
      )}
      {similar && item?.head != null && (
        <div className="disk-rebuild-similar" style={{ marginLeft: 23 + depth * INDENT }}>
          <SimilarPieces head={item.head} onSwap={(piece) => void swap(piece)} used={new Set([...shared.items.values()].flatMap((found) => (found.head !== null ? [found.head] : [])))} />
        </div>
      )}
      {open && <FolderContents path={path} depth={depth + 1} around={inside} shared={shared} />}
    </>
  );
}

/** Why a clashing file's copy was picked, in words. */
const DECIDED: Record<NonNullable<RebuiltFile["decidedBy"]>, () => string> = {
  person: () => t("This copy was chosen on purpose."),
  quality: () => t("Picked because it measured better: tested or judged good, all its clusters there, a valid entry, or the size its entry says."),
  date: () => t("These measure the same, so the newer copy was picked. Look at both if it matters."),
  layer: () => t("These measure the same and have the same date, so the copy from the upper layer, the later drop, was picked. Look at both if it matters."),
};

function FileNode({ file, path, depth, shared }: { file: RebuiltFile; path: string[]; depth: number; shared: Shared }) {
  const [choosing, setChoosing] = useState(false);
  const clash = file.copies.length > 1;

  async function choose(entry: number | null) {
    setChoosing(false);
    try {
      await call("disk_rebuild_choose", { diskRebuild: shared.diskRebuild, path: [...path, file.name], entry });
      shared.changed();
    } catch (reason) {
      shared.fail(reason);
    }
  }

  const row = file.chosen;
  return (
    <>
      <div className="row-wrap disk-rebuild-row">
        <button className="row" style={{ paddingLeft: 23 + depth * INDENT }} onClick={() => shared.onEntry(row)} title={fromEngine(row.reason)}>
          <span className="row-icon"><Icon name={fileIcon(file.name)} /></span>
          <span className="row-main">
            <span className="row-name">{file.name}</span>
            {file.byPerson && <span className="row-path">{file.madeByAgent ? t("The copy the agent chose") : t("The copy you chose")}</span>}
          </span>
          {file.madeByAgent && <AgentMark />}
          <span className="row-size">{bytes(row.length)}</span>
          <ConfidenceRing value={row.confidence} tone={verdictTone(row.verdict)} label={t("{verdict}, confidence {percent}", { verdict: verdictLabel(row.verdict), percent: percent(row.confidence) })} />
        </button>
        {clash && (
          <button className={file.needsYou ? "disk-rebuild-clash disk-rebuild-clash-needs-you" : "disk-rebuild-clash"} onClick={() => setChoosing(!choosing)} aria-expanded={choosing} title={file.needsYou ? t("These copies differ, and nothing measured tells them apart. Look, and choose one.") : t("Pieces hold more than one copy of this file")}>
            {file.needsYou ? t("{count} copies, choose", { count: count(file.copies.length) }) : t("{count} copies", { count: count(file.copies.length) })}
          </button>
        )}
      </div>
      {choosing && (
        <div className="disk-rebuild-copies" style={{ marginLeft: 23 + depth * INDENT }}>
          {file.copies.map((copy) => {
            const chosen = copy.id === row.id;
            return (
              <div key={copy.id} className={chosen ? "disk-rebuild-copy disk-rebuild-copy-chosen" : "disk-rebuild-copy"}>
                <ConfidenceRing value={copy.confidence} tone={verdictTone(copy.verdict)} label={verdictLabel(copy.verdict)} />
                <span className="disk-rebuild-copy-text">
                  {verdictLabel(copy.verdict)}, {bytes(copy.length)}{copy.modified ? `, ${copy.modified.replace("T", " ")}` : ""}
                </span>
                {chosen ? (
                  <span className="muted">{file.byPerson ? (file.madeByAgent ? t("Chosen by the agent") : t("Chosen by you")) : t("Best copy")}</span>
                ) : (
                  <button className="link" onClick={() => void choose(copy.id)}>{t("Use this copy")}</button>
                )}
              </div>
            );
          })}
          {file.decidedBy && <p className="disk-rebuild-decided muted">{DECIDED[file.decidedBy]()}</p>}
          {file.byPerson && <button className="link" onClick={() => void choose(null)}>{t("Go back to the best copy")}</button>}
        </div>
      )}
    </>
  );
}

/** The folders and files inside one folder of the rebuilt tree. */
function FolderContents({ path, depth, around, shared }: { path: string[]; depth: number; around: number | null; shared: Shared }) {
  const [listing, setListing] = useState<DiskRebuildListing | null>(null);
  const [limit, setLimit] = useState(FILES_PAGE);
  const key = path.join("\u0000");
  useEffect(() => {
    let current = true;
    call<DiskRebuildListing>("disk_rebuild_tree", { diskRebuild: shared.diskRebuild, path, limit })
      .then((result) => current && setListing(result))
      .catch((reason) => current && shared.fail(reason));
    return () => {
      current = false;
    };
    // The path is compared by its key, as it is a new array on every draw.
  }, [shared.diskRebuild, key, limit, shared.revision]);

  if (!listing) return <div className="empty" style={{ paddingLeft: 23 + depth * INDENT }}>{t("Loading.")}</div>;
  if (listing.folders.length === 0 && listing.files.length === 0) {
    return <div className="empty disk-rebuild-empty-folder" style={{ paddingLeft: 23 + depth * INDENT }}>{t("Empty. Drop a piece here.")}</div>;
  }
  return (
    <>
      {listing.clashes > 0 && (
        <div className={listing.clashesNeedYou > 0 ? "disk-rebuild-clash-summary disk-rebuild-clash-summary-needs-you" : "disk-rebuild-clash-summary"} style={{ paddingLeft: 23 + depth * INDENT }}>
          {listing.clashesNeedYou > 0
            ? t("{picked} clashes picked for you, {needed} need you", { picked: count(listing.clashesPicked), needed: count(listing.clashesNeedYou) })
            : t("{picked} clashes picked for you", { picked: count(listing.clashesPicked) })}
        </div>
      )}
      {listing.folders.map((folder) => (
        <FolderNode key={folder.pathName} folder={folder} path={[...path, folder.pathName]} depth={depth} around={around} shared={shared} />
      ))}
      {listing.files.map((file) => (
        <FileNode key={file.name.toLowerCase()} file={file} path={path} depth={depth} shared={shared} />
      ))}
      {listing.totalFiles > listing.files.length && (
        <button className="more" style={{ marginLeft: 23 + depth * INDENT }} onClick={() => setLimit(limit + FILES_PAGE)}>
          {t("Show more files ({count} left)", { count: count(listing.totalFiles - listing.files.length) })}
        </button>
      )}
    </>
  );
}

/** A found folder's name in the window, or the name the disk lost. */
function foundName(row: Row | null | undefined, head: number): string {
  return row?.name || (row?.nodeKind === "root" ? t("Volume root") : lostFolderName(head));
}

/** Claude's suggestions for this diskRebuild, each with its reason, for the person to take or leave. */
function Suggestions({ diskRebuild, suggestions, items, changed, fail }: {
  diskRebuild: number;
  suggestions: DiskRebuildSuggestion[];
  items: Map<number, DiskRebuildItem>;
  changed: () => void;
  fail: (reason: unknown) => void;
}) {
  const [names, setNames] = useState<Map<number, Row | null>>(new Map());
  // The folders to name, as a key that only changes when they do. Depending on the items map
  // itself would run this on every draw, since the map is rebuilt each time.
  const heads = new Set<number>();
  for (const item of suggestions) {
    heads.add(item.head);
    if (item.placeHead !== null) heads.add(item.placeHead);
    const target = item.placeItem !== null ? items.get(item.placeItem) : undefined;
    if (target?.head != null) heads.add(target.head);
  }
  const key = [...heads].sort((a, b) => a - b).join(",");
  useEffect(() => {
    let current = true;
    const wanted = key ? key.split(",").map(Number) : [];
    Promise.all(wanted.map((head) => call<Row | null>("node", { head }).then((row) => [head, row] as const).catch(() => [head, null] as const)))
      .then((pairs) => current && setNames(new Map(pairs)))
      .catch(() => undefined);
    return () => {
      current = false;
    };
  }, [key]);

  const where = (item: DiskRebuildSuggestion): string => {
    if (item.placeItem === null) return t("as the top of the tree");
    if (item.placeHead !== null) return t("inside {folder}", { folder: foundName(names.get(item.placeHead), item.placeHead) });
    const target = items.get(item.placeItem);
    const name = target?.name ?? (target?.head != null ? foundName(names.get(target.head), target.head) : t("Unnamed folder"));
    return item.merge ? t("merged with {folder}", { folder: name }) : t("inside {folder}", { folder: name });
  };

  async function act(tool: "disk_rebuild_accept" | "disk_rebuild_dismiss", suggestion: number) {
    try {
      await call(tool, { diskRebuild, suggestion });
      changed();
    } catch (reason) {
      fail(reason);
    }
  }

  return (
    <div className="suggestions">
      <h4><Icon name="sparkle" size={13} /> {t("Agent suggests")}</h4>
      {suggestions.map((item) => (
        <div key={item.id} className="suggestion">
          <div className="suggestion-text">
            <strong>{t("Put {piece} {where}", { piece: foundName(names.get(item.head), item.head), where: where(item) })}</strong>
            <span className="muted">{item.note}</span>
          </div>
          <button className="primary" onClick={() => void act("disk_rebuild_accept", item.id)}>{t("Accept")}</button>
          <button onClick={() => void act("disk_rebuild_dismiss", item.id)}>{t("Dismiss")}</button>
        </div>
      ))}
    </div>
  );
}

/** The Rebuild tab in the right panel: one diskRebuild and its rebuilt tree, which pieces are dragged
 * onto from the sidebar while the middle shows what is in them. */
export function RebuildView({ diskRebuilds, diskRebuild, onDiskRebuild, revision, onChanged, onEntry, copy, finder, onFinder, mapRevision }: {
  diskRebuilds: DiskRebuild[];
  copy: CopyStatus | null;
  finder: FinderState;
  onFinder: (state: FinderState) => void;
  mapRevision: number;
  diskRebuild: number | null;
  onDiskRebuild: (id: number | null) => void;
  revision: number;
  onChanged: () => void;
  onEntry: (row: Row) => void;
}) {
  const [items, setItems] = useState<DiskRebuildItem[]>([]);
  const [unused, setUnused] = useState<UnusedPieces | null>(null);
  const [suggestions, setSuggestions] = useState<DiskRebuildSuggestion[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [naming, setNaming] = useState<"diskRebuild" | "rename" | "root" | null>(null);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [holding, setHolding] = useState(false);
  const [copying, setCopying] = useState(false);
  const [finderNote, setFinderNote] = useState<string | null>(null);
  const [finderBusy, setFinderBusy] = useState(false);
  // What agents changed on this scan, and on the Disk Rebuild showing, for the Undo buttons.
  const [agentScan, setAgentScan] = useState<AgentChanges | null>(null);
  const [agentHere, setAgentHere] = useState<AgentChanges | null>(null);
  const [confirmRestore, setConfirmRestore] = useState(false);
  const fail = useCallback((reason: unknown) => setError(String(reason)), []);

  useEffect(() => {
    let current = true;
    setConfirmRestore(false);
    call<AgentChanges>("disk_rebuild_agent_changes", { limit: 1 })
      .then((found) => current && setAgentScan(found))
      .catch(() => current && setAgentScan(null));
    if (diskRebuild === null) {
      setAgentHere(null);
    } else {
      call<AgentChanges>("disk_rebuild_agent_changes", { diskRebuild, limit: 1 })
        .then((found) => current && setAgentHere(found))
        .catch(() => current && setAgentHere(null));
    }
    return () => {
      current = false;
    };
  }, [diskRebuild, revision]);

  useEffect(() => {
    setError(null);
    setConfirmDelete(false);
    setCopying(false);
    if (diskRebuild === null) {
      setItems([]);
      setUnused(null);
      return;
    }
    let current = true;
    Promise.all([
      call<DiskRebuildItem[]>("disk_rebuild_items", { diskRebuild }),
      call<UnusedPieces>("disk_rebuild_unused", { diskRebuild, limit: 1 }),
      call<DiskRebuildSuggestion[]>("disk_rebuild_suggestions", { diskRebuild }),
    ])
      .then(([found, left, ideas]) => {
        if (!current) return;
        setItems(found);
        setUnused(left);
        setSuggestions(ideas);
      })
      .catch((reason) => current && fail(reason));
    return () => {
      current = false;
    };
  }, [diskRebuild, revision, fail]);

  const changed = useCallback(() => {
    setError(null);
    onChanged();
  }, [onChanged]);

  async function createDiskRebuild(name: string | null) {
    setNaming(null);
    if (!name) return;
    try {
      const created = await call<{ id: number }>("disk_rebuild_create", { name });
      onDiskRebuild(created.id);
      changed();
    } catch (reason) {
      fail(reason);
    }
  }

  async function renameDiskRebuild(name: string | null) {
    setNaming(null);
    if (!name || diskRebuild === null) return;
    try {
      await call("disk_rebuild_rename", { diskRebuild, name });
      changed();
    } catch (reason) {
      fail(reason);
    }
  }

  async function deleteDiskRebuild() {
    if (diskRebuild === null) return;
    setConfirmDelete(false);
    try {
      await call("disk_rebuild_delete", { diskRebuild });
      onDiskRebuild(diskRebuilds.find((other) => other.id !== diskRebuild)?.id ?? null);
      changed();
    } catch (reason) {
      fail(reason);
    }
  }

  async function startWithFolder(name: string | null) {
    setNaming(null);
    if (!name || diskRebuild === null) return;
    try {
      await call("disk_rebuild_folder", { diskRebuild, name });
      changed();
    } catch (reason) {
      fail(reason);
    }
  }

  async function putRestAway() {
    if (diskRebuild === null) return;
    try {
      await call("disk_rebuild_leftovers", { diskRebuild, name: t("Leftovers") });
      changed();
    } catch (reason) {
      fail(reason);
    }
  }

  // Taking back what an agent did. The person decides; an agent cannot undo.
  async function undoAgent(tool: "disk_rebuild_undo" | "disk_rebuild_undo_session") {
    try {
      const report = await call<UndoReport>(tool, {});
      if (report.stopped) setError(report.stopped);
      changed();
      if (report.stopped) setError(report.stopped);
    } catch (reason) {
      fail(reason);
    }
  }

  async function restoreBeforeAgent() {
    if (diskRebuild === null) return;
    setConfirmRestore(false);
    try {
      await call("disk_rebuild_restore", { diskRebuild });
      changed();
    } catch (reason) {
      fail(reason);
    }
  }

  const bar = undoBar(agentScan, agentHere);
  const root = items.find((item) => item.parentItem === null);
  const current = diskRebuilds.find((found) => found.id === diskRebuild);
  const inFinder = diskRebuild !== null && finder.shown.includes(diskRebuild);

  // Adding it again when it is already there just opens it.
  async function showInFinder() {
    if (!current) return;
    setFinderBusy(true);
    setFinderNote(null);
    try {
      const shown = await invoke<{ folder: string | null; note?: string }>("finder_show", { diskRebuild: current.id, name: current.name, revision: `${current.updatedAtMillis}-${mapRevision}` });
      onFinder({ available: true, shown: [...finder.shown.filter((id) => id !== current.id), current.id] });
      if (!shown.folder) setFinderNote(t("Turn on Captain Salvage in System Settings, General, Login Items & Extensions, File Providers, then try again."));
    } catch (reason) {
      fail(reason);
    } finally {
      setFinderBusy(false);
    }
  }

  async function removeFromFinder() {
    if (diskRebuild === null) return;
    setFinderBusy(true);
    setFinderNote(null);
    try {
      await invoke("finder_hide", { diskRebuild });
      onFinder({ available: true, shown: finder.shown.filter((id) => id !== diskRebuild) });
    } catch (reason) {
      fail(reason);
    } finally {
      setFinderBusy(false);
    }
  }
  const shared: Shared | null = diskRebuild === null ? null : { diskRebuild, items: new Map(items.map((item) => [item.id, item])), revision, changed, fail, onEntry };

  let body: ReactNode;
  if (diskRebuilds.length === 0 || diskRebuild === null) {
    body = (
      <div className="disk-rebuild-start">
        <h3>{t("Rebuild the folder tree")}</h3>
        <p>{t("Put the lost pieces back together the way you remember them: drag pieces from the sidebar, put them inside one another, merge them, and make up folders the disk lost. Nothing is written to the disk.")}</p>
        {naming === "diskRebuild" ? (
          <NameInput initial={t("My rebuild")} onDone={(name) => void createDiskRebuild(name)} />
        ) : (
          <button className="primary" onClick={() => setNaming("diskRebuild")}>{t("Start a Disk Rebuild")}</button>
        )}
      </div>
    );
  } else if (!root) {
    body = (
      <div
        className={holding ? "disk-rebuild-drop-first disk-rebuild-drop-first-holding" : "disk-rebuild-drop-first"}
        onDragOver={(event) => {
          if (!carriesSomething(event)) return;
          event.preventDefault();
          setHolding(true);
        }}
        onDragLeave={() => setHolding(false)}
        onDrop={(event) => {
          event.preventDefault();
          setHolding(false);
          const what = dragged(event);
          if (what && shared) void drop(shared, what, undefined, false);
        }}
      >
        <Icon name="folder" size={28} />
        <p>{t("Drag a piece from the sidebar here. The first piece becomes the top of the tree.")}</p>
        <p className="muted">{t("The sidebar lists the largest pieces first, so start with those.")}</p>
        <p className="muted">{t("Or ask an agent, in the chat, to build it for you: what it makes shows here marked as made by the agent, and you can undo it. If you only ask where something belongs, its suggestions appear here for you to accept or dismiss.")}</p>
        {naming === "root" ? (
          <NameInput initial="" onDone={(name) => void startWithFolder(name)} />
        ) : (
          <button className="link" onClick={() => setNaming("root")}>{t("Or start with a folder you name")}</button>
        )}
      </div>
    );
  } else if (shared) {
    body = (
      <div
        className="disk-rebuild-tree"
        onDragOver={(event) => {
          if (carriesSomething(event)) event.preventDefault();
        }}
        onDrop={(event) => {
          // Dropped below every folder: inside the top of the tree.
          event.preventDefault();
          const what = dragged(event);
          if (what) void drop(shared, what, { item: root.id }, false);
        }}
      >
        <RootNode root={root} shared={shared} />
      </div>
    );
  }

  return (
    <section className="rebuild" aria-label={t("Rebuild")}>
      <div className="rebuild-bar">
        {diskRebuilds.length > 0 && (
          <select value={diskRebuild ?? ""} onChange={(event) => onDiskRebuild(event.target.value ? Number(event.target.value) : null)} aria-label={t("Disk Rebuild")}>
            {diskRebuilds.map((found) => <option key={found.id} value={found.id}>{found.madeByAgent ? t("{name}, made by the agent", { name: found.name }) : found.name}</option>)}
          </select>
        )}
        {naming === "rename" && current ? (
          <NameInput initial={current.name} onDone={(name) => void renameDiskRebuild(name)} />
        ) : (
          current && <button className="link" onClick={() => setNaming("rename")}>{t("Rename")}</button>
        )}
        {diskRebuilds.length > 0 && (naming === "diskRebuild" ? (
          <NameInput initial={t("Another try")} onDone={(name) => void createDiskRebuild(name)} />
        ) : (
          <button className="link" onClick={() => setNaming("diskRebuild")}>{t("New Disk Rebuild")}</button>
        ))}
        {current && (confirmDelete ? (
          <span className="rebuild-confirm">
            {t("Delete this Disk Rebuild?")} <button className="link" onClick={() => void deleteDiskRebuild()}>{t("Delete")}</button>{" "}
            <button className="link" onClick={() => setConfirmDelete(false)}>{t("Keep it")}</button>
          </span>
        ) : (
          <button className="link" onClick={() => setConfirmDelete(true)}>{t("Delete")}</button>
        ))}
        <span className="toolbar-spacer" />
        {unused && current && (
          <span className="muted rebuild-left">
            {unused.total === 0 ? t("Every piece is placed") : t("{count} pieces left, {bytes}", { count: count(unused.total), bytes: bytes(unused.totalBytes) })}
          </span>
        )}
        {root && unused && unused.total > 0 && (
          <button onClick={() => void putRestAway()} title={t("Put every piece not yet placed into one folder called Leftovers")}>{t("Put the rest away")}</button>
        )}
        {root && finder.available && (inFinder ? (
          <>
            <button disabled={finderBusy} onClick={() => void showInFinder()} title={t("Open this Disk Rebuild's folder in Finder")}>{t("Open in Finder")}</button>
            <button className="link" disabled={finderBusy} onClick={() => void removeFromFinder()}>{t("Remove from Finder")}</button>
          </>
        ) : (
          <button disabled={finderBusy} onClick={() => void showInFinder()} title={t("Show the rebuilt tree as a folder in Finder, read only, straight from the disk")}>{t("Show in Finder")}</button>
        ))}
        {root && !copying && (
          <button onClick={() => setCopying(true)} title={t("Copy every file to where this Disk Rebuild puts it")}>{t("Copy out")}</button>
        )}
      </div>
      {bar.show && agentScan?.last && (
        <div className="agent-bar" role="status">
          <Icon name="sparkle" size={13} />
          <span className="agent-bar-text">{t("The agent changed a Disk Rebuild: {what}", { what: fromEngine(agentScan.last.summary) })}</span>
          <button onClick={() => void undoAgent("disk_rebuild_undo")} title={t("Take back the agent's last change")}>{t("Undo")}</button>
          <button onClick={() => void undoAgent("disk_rebuild_undo_session")} title={t("Take back every change of the agent's latest session")}>{t("Undo all agent changes")}</button>
          {bar.canRestore && (confirmRestore ? (
            <span className="rebuild-confirm">
              {t("Put this Disk Rebuild back as it was before the agent began? What you changed since is lost too.")} <button className="link" onClick={() => void restoreBeforeAgent()}>{t("Restore")}</button>{" "}
              <button className="link" onClick={() => setConfirmRestore(false)}>{t("Keep it")}</button>
            </span>
          ) : (
            <button className="link" onClick={() => setConfirmRestore(true)}>{t("Restore to before the agent")}</button>
          ))}
        </div>
      )}
      {finderNote && <p className="muted rebuild-error">{finderNote}</p>}
      {error && <p className="warn rebuild-error">{fromEngine(error.replace(/^Error: /, ""))}</p>}
      <div className="rebuild-body">
        {copying && root && diskRebuild !== null && <DiskRebuildCopy diskRebuild={diskRebuild} copy={copy} revision={revision} onClose={() => setCopying(false)} />}
        {shared && suggestions.length > 0 && (
          <Suggestions diskRebuild={shared.diskRebuild} suggestions={suggestions} items={shared.items} changed={changed} fail={fail} />
        )}
        {body}
      </div>
      <p className="rebuild-foot muted">{t("This only changes how the app arranges what it found. Nothing is written to the disk.")}</p>
    </section>
  );
}

/** The top of the tree, always open, and the folder everything else goes into. */
function RootNode({ root, shared }: { root: DiskRebuildItem; shared: Shared }) {
  const [rootFolder, setRootFolder] = useState<RebuiltFolder | null>(null);
  useEffect(() => {
    let current = true;
    const describe = async (): Promise<RebuiltFolder> => {
      let name = root.name;
      let lostHead: number | null = null;
      let found: Row | null = null;
      if (root.head !== null) {
        found = await call<Row | null>("node", { head: root.head });
        if (!name) {
          name = found?.name || null;
          if (!name) lostHead = root.head;
        }
      }
      return {
        pathName: "",
        name,
        lostHead,
        origin: root.kind === "folder" ? "made" : "placed",
        merged: false,
        items: [root.id],
        heads: root.head !== null ? [root.head] : [],
        files: found?.files ?? 0,
        bytes: found?.bytes ?? 0,
        intactFiles: found?.intactFiles ?? 0,
        intactBytes: found?.intactBytes ?? 0,
        madeByAgent: root.madeByAgent,
      };
    };
    describe().then((folder) => current && setRootFolder(folder)).catch((reason) => current && shared.fail(reason));
    return () => {
      current = false;
    };
  }, [root.id, root.name, root.head, root.kind, shared.revision]);
  if (!rootFolder) return null;
  return <FolderNode folder={rootFolder} path={[]} depth={0} around={root.id} shared={shared} startOpen />;
}
