import { useCallback, useEffect, useState, type DragEvent, type ReactNode } from "react";
import { call, type Canvas, type CanvasItem, type CopyStatus, type CanvasSuggestion, type CanvasListing, type CanvasPlace, type RebuiltFile, type RebuiltFolder, type Row, type UnusedPieces } from "../api";
import { bytes, count, lostFolderName, percent, verdictLabel } from "../format";
import { fromEngine, t } from "../i18n";
import { ConfidenceRing, fileIcon, Icon, verdictTone } from "./icons";
import { CanvasCopy } from "./CanvasCopy";
import { SimilarPieces } from "./SimilarPieces";

/** What a dragged sidebar piece carries: the first cluster of its folder. */
export const PIECE_TYPE = "application/x-salvage-piece";
/** What a dragged canvas item carries, to move it. */
const ITEM_TYPE = "application/x-salvage-item";
const FILES_PAGE = 200;
const INDENT = 14;

/** Sets what a drag from the sidebar carries. */
export function startPieceDrag(event: DragEvent, head: number) {
  event.dataTransfer.setData(PIECE_TYPE, String(head));
  event.dataTransfer.effectAllowed = "copyMove";
}

interface Dragged {
  piece?: number;
  item?: number;
}

function dragged(event: DragEvent): Dragged | null {
  const piece = event.dataTransfer.getData(PIECE_TYPE);
  const item = event.dataTransfer.getData(ITEM_TYPE);
  if (piece) return { piece: Number(piece) };
  if (item) return { item: Number(item) };
  return null;
}

function carriesSomething(event: DragEvent): boolean {
  return event.dataTransfer.types.includes(PIECE_TYPE) || event.dataTransfer.types.includes(ITEM_TYPE);
}

interface Shared {
  canvas: number;
  items: Map<number, CanvasItem>;
  revision: number;
  changed: () => void;
  fail: (reason: unknown) => void;
  onEntry: (row: Row) => void;
}

function folderLabel(folder: RebuiltFolder): string {
  return folder.name ?? (folder.lostHead !== null ? lostFolderName(folder.lostHead) : t("Unnamed folder"));
}

/** Where a drop on this folder puts things: in its own item, or in its found folder inside one. */
function placeOf(folder: RebuiltFolder, around: number | null): CanvasPlace | null {
  if (folder.items.length > 0) return { item: folder.items[0] };
  if (around !== null && folder.heads.length > 0) return { item: around, head: folder.heads[0] };
  return null;
}

/** Drops a piece or an item onto a place, inside it or merged into it. */
async function drop(shared: Shared, what: Dragged, place: CanvasPlace | undefined, merge: boolean) {
  try {
    if (what.piece !== undefined) {
      await call("canvas_place", { canvas: shared.canvas, head: what.piece, place: place ?? null, merge });
    } else if (what.item !== undefined) {
      await call("canvas_move", { canvas: shared.canvas, item: what.item, place: place ?? null, merge });
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
      className="canvas-name"
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
      await call("canvas_folder", { canvas: shared.canvas, name, place });
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
      await call("canvas_rename_item", { canvas: shared.canvas, item: item.id, name: name ?? (item.kind === "folder" ? label : null) });
      shared.changed();
    } catch (reason) {
      shared.fail(reason);
    }
  }

  async function swap(piece: Row) {
    if (!item || piece.head === null) return;
    try {
      await call("canvas_swap", { canvas: shared.canvas, item: item.id, head: piece.head });
      setSimilar(false);
      shared.changed();
    } catch (reason) {
      shared.fail(reason);
    }
  }

  async function remove() {
    if (!item) return;
    try {
      await call("canvas_remove", { canvas: shared.canvas, item: item.id });
      shared.changed();
    } catch (reason) {
      shared.fail(reason);
    }
  }

  const origin = folder.origin === "made" ? t("A folder you made") : folder.origin === "placed" ? t("A piece you placed") : t("Found inside a piece");
  return (
    <>
      <div
        className={holding ? "row-wrap canvas-row canvas-row-holding" : "row-wrap canvas-row"}
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
          className={`row canvas-folder canvas-origin-${folder.origin}`}
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
              <span className="row-path">{folder.merged ? t("Merged from {count} pieces", { count: count(folder.items.length + folder.heads.length) }) : t("Made by you")}</span>
            )}
          </span>
          <span className="row-size" title={t("{bytes} likely intact", { bytes: bytes(folder.intactBytes) })}>{folder.bytes > 0 ? bytes(folder.bytes) : ""}</span>
        </button>
        {holding ? (
          <DropChoices onInside={onDrop(false)} onMerge={onDrop(true)} />
        ) : (
          <span className="canvas-actions">
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
              <button className="icon-button" title={t("Take off the canvas")} aria-label={t("Take off the canvas")} onClick={() => void remove()}>
                <Icon name="close" size={12} />
              </button>
            )}
          </span>
        )}
      </div>
      {naming === "new" && (
        <div className="canvas-new-folder" style={{ paddingLeft: 26 + (depth + 1) * INDENT }}>
          <Icon name="folder" />
          <NameInput initial="" onDone={(name) => void makeFolder(name)} />
        </div>
      )}
      {similar && item?.head != null && (
        <div className="canvas-similar" style={{ marginLeft: 23 + depth * INDENT }}>
          <SimilarPieces head={item.head} onSwap={(piece) => void swap(piece)} used={new Set([...shared.items.values()].flatMap((found) => (found.head !== null ? [found.head] : [])))} />
        </div>
      )}
      {open && <FolderContents path={path} depth={depth + 1} around={inside} shared={shared} />}
    </>
  );
}

function FileNode({ file, path, depth, shared }: { file: RebuiltFile; path: string[]; depth: number; shared: Shared }) {
  const [choosing, setChoosing] = useState(false);
  const clash = file.copies.length > 1;

  async function choose(entry: number | null) {
    setChoosing(false);
    try {
      await call("canvas_choose", { canvas: shared.canvas, path: [...path, file.name], entry });
      shared.changed();
    } catch (reason) {
      shared.fail(reason);
    }
  }

  const row = file.chosen;
  return (
    <>
      <div className="row-wrap canvas-row">
        <button className="row" style={{ paddingLeft: 23 + depth * INDENT }} onClick={() => shared.onEntry(row)} title={fromEngine(row.reason)}>
          <span className="row-icon"><Icon name={fileIcon(file.name)} /></span>
          <span className="row-main">
            <span className="row-name">{file.name}</span>
            {file.byPerson && <span className="row-path">{t("The copy you chose")}</span>}
          </span>
          <span className="row-size">{bytes(row.length)}</span>
          <ConfidenceRing value={row.confidence} tone={verdictTone(row.verdict)} label={t("{verdict}, confidence {percent}", { verdict: verdictLabel(row.verdict), percent: percent(row.confidence) })} />
        </button>
        {clash && (
          <button className="canvas-clash" onClick={() => setChoosing(!choosing)} aria-expanded={choosing} title={t("Pieces hold more than one copy of this file")}>
            {t("{count} copies", { count: count(file.copies.length) })}
          </button>
        )}
      </div>
      {choosing && (
        <div className="canvas-copies" style={{ marginLeft: 23 + depth * INDENT }}>
          {file.copies.map((copy) => {
            const chosen = copy.id === row.id;
            return (
              <div key={copy.id} className={chosen ? "canvas-copy canvas-copy-chosen" : "canvas-copy"}>
                <ConfidenceRing value={copy.confidence} tone={verdictTone(copy.verdict)} label={verdictLabel(copy.verdict)} />
                <span className="canvas-copy-text">
                  {verdictLabel(copy.verdict)}, {bytes(copy.length)}{copy.modified ? `, ${copy.modified.replace("T", " ")}` : ""}
                </span>
                {chosen ? (
                  <span className="muted">{file.byPerson ? t("Chosen by you") : t("Best copy")}</span>
                ) : (
                  <button className="link" onClick={() => void choose(copy.id)}>{t("Use this copy")}</button>
                )}
              </div>
            );
          })}
          {file.byPerson && <button className="link" onClick={() => void choose(null)}>{t("Go back to the best copy")}</button>}
        </div>
      )}
    </>
  );
}

/** The folders and files inside one folder of the rebuilt tree. */
function FolderContents({ path, depth, around, shared }: { path: string[]; depth: number; around: number | null; shared: Shared }) {
  const [listing, setListing] = useState<CanvasListing | null>(null);
  const [limit, setLimit] = useState(FILES_PAGE);
  const key = path.join("\u0000");
  useEffect(() => {
    let current = true;
    call<CanvasListing>("canvas_tree", { canvas: shared.canvas, path, limit })
      .then((result) => current && setListing(result))
      .catch((reason) => current && shared.fail(reason));
    return () => {
      current = false;
    };
    // The path is compared by its key, as it is a new array on every draw.
  }, [shared.canvas, key, limit, shared.revision]);

  if (!listing) return <div className="empty" style={{ paddingLeft: 23 + depth * INDENT }}>{t("Loading.")}</div>;
  if (listing.folders.length === 0 && listing.files.length === 0) {
    return <div className="empty canvas-empty-folder" style={{ paddingLeft: 23 + depth * INDENT }}>{t("Empty. Drop a piece here.")}</div>;
  }
  return (
    <>
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

/** Claude's suggestions for this canvas, each with its reason, for the person to take or leave. */
function Suggestions({ canvas, suggestions, items, changed, fail }: {
  canvas: number;
  suggestions: CanvasSuggestion[];
  items: Map<number, CanvasItem>;
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

  const where = (item: CanvasSuggestion): string => {
    if (item.placeItem === null) return t("as the top of the tree");
    if (item.placeHead !== null) return t("inside {folder}", { folder: foundName(names.get(item.placeHead), item.placeHead) });
    const target = items.get(item.placeItem);
    const name = target?.name ?? (target?.head != null ? foundName(names.get(target.head), target.head) : t("Unnamed folder"));
    return item.merge ? t("merged with {folder}", { folder: name }) : t("inside {folder}", { folder: name });
  };

  async function act(tool: "canvas_accept" | "canvas_dismiss", suggestion: number) {
    try {
      await call(tool, { canvas, suggestion });
      changed();
    } catch (reason) {
      fail(reason);
    }
  }

  return (
    <div className="suggestions">
      <h4><Icon name="sparkle" size={13} /> {t("Claude suggests")}</h4>
      {suggestions.map((item) => (
        <div key={item.id} className="suggestion">
          <div className="suggestion-text">
            <strong>{t("Put {piece} {where}", { piece: foundName(names.get(item.head), item.head), where: where(item) })}</strong>
            <span className="muted">{item.note}</span>
          </div>
          <button className="primary" onClick={() => void act("canvas_accept", item.id)}>{t("Accept")}</button>
          <button onClick={() => void act("canvas_dismiss", item.id)}>{t("Dismiss")}</button>
        </div>
      ))}
    </div>
  );
}

/** The Rebuild view: one canvas, its rebuilt tree, and the pieces left to place. */
export function RebuildView({ canvases, canvas, onCanvas, revision, onChanged, onEntry, copy }: {
  canvases: Canvas[];
  copy: CopyStatus | null;
  canvas: number | null;
  onCanvas: (id: number | null) => void;
  revision: number;
  onChanged: () => void;
  onEntry: (row: Row) => void;
}) {
  const [items, setItems] = useState<CanvasItem[]>([]);
  const [unused, setUnused] = useState<UnusedPieces | null>(null);
  const [suggestions, setSuggestions] = useState<CanvasSuggestion[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [naming, setNaming] = useState<"canvas" | "rename" | "root" | null>(null);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [holding, setHolding] = useState(false);
  const [copying, setCopying] = useState(false);
  const fail = useCallback((reason: unknown) => setError(String(reason)), []);

  useEffect(() => {
    setError(null);
    setConfirmDelete(false);
    setCopying(false);
    if (canvas === null) {
      setItems([]);
      setUnused(null);
      return;
    }
    let current = true;
    Promise.all([
      call<CanvasItem[]>("canvas_items", { canvas }),
      call<UnusedPieces>("canvas_unused", { canvas, limit: 1 }),
      call<CanvasSuggestion[]>("canvas_suggestions", { canvas }),
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
  }, [canvas, revision, fail]);

  const changed = useCallback(() => {
    setError(null);
    onChanged();
  }, [onChanged]);

  async function createCanvas(name: string | null) {
    setNaming(null);
    if (!name) return;
    try {
      const created = await call<{ id: number }>("canvas_create", { name });
      onCanvas(created.id);
      changed();
    } catch (reason) {
      fail(reason);
    }
  }

  async function renameCanvas(name: string | null) {
    setNaming(null);
    if (!name || canvas === null) return;
    try {
      await call("canvas_rename", { canvas, name });
      changed();
    } catch (reason) {
      fail(reason);
    }
  }

  async function deleteCanvas() {
    if (canvas === null) return;
    setConfirmDelete(false);
    try {
      await call("canvas_delete", { canvas });
      onCanvas(canvases.find((other) => other.id !== canvas)?.id ?? null);
      changed();
    } catch (reason) {
      fail(reason);
    }
  }

  async function startWithFolder(name: string | null) {
    setNaming(null);
    if (!name || canvas === null) return;
    try {
      await call("canvas_folder", { canvas, name });
      changed();
    } catch (reason) {
      fail(reason);
    }
  }

  async function putRestAway() {
    if (canvas === null) return;
    try {
      await call("canvas_leftovers", { canvas, name: t("Leftovers") });
      changed();
    } catch (reason) {
      fail(reason);
    }
  }

  const root = items.find((item) => item.parentItem === null);
  const current = canvases.find((found) => found.id === canvas);
  const shared: Shared | null = canvas === null ? null : { canvas, items: new Map(items.map((item) => [item.id, item])), revision, changed, fail, onEntry };

  let body: ReactNode;
  if (canvases.length === 0 || canvas === null) {
    body = (
      <div className="canvas-start">
        <h3>{t("Rebuild the folder tree")}</h3>
        <p>{t("Put the lost pieces back together the way you remember them: drag pieces from the sidebar, put them inside one another, merge them, and make up folders the disk lost. Nothing is written to the disk.")}</p>
        {naming === "canvas" ? (
          <NameInput initial={t("My rebuild")} onDone={(name) => void createCanvas(name)} />
        ) : (
          <button className="primary" onClick={() => setNaming("canvas")}>{t("Start a canvas")}</button>
        )}
      </div>
    );
  } else if (!root) {
    body = (
      <div
        className={holding ? "canvas-drop-first canvas-drop-first-holding" : "canvas-drop-first"}
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
        <p className="muted">{t("Or ask Claude, in the chat, where the pieces belong. Its suggestions appear here for you to accept or dismiss.")}</p>
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
        className="canvas-tree"
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
    <section className="center-panel rebuild" aria-label={t("Rebuild")}>
      <div className="rebuild-bar">
        {canvases.length > 0 && (
          <select value={canvas ?? ""} onChange={(event) => onCanvas(event.target.value ? Number(event.target.value) : null)} aria-label={t("Canvas")}>
            {canvases.map((found) => <option key={found.id} value={found.id}>{found.name}</option>)}
          </select>
        )}
        {naming === "rename" && current ? (
          <NameInput initial={current.name} onDone={(name) => void renameCanvas(name)} />
        ) : (
          current && <button className="link" onClick={() => setNaming("rename")}>{t("Rename")}</button>
        )}
        {canvases.length > 0 && (naming === "canvas" ? (
          <NameInput initial={t("Another try")} onDone={(name) => void createCanvas(name)} />
        ) : (
          <button className="link" onClick={() => setNaming("canvas")}>{t("New canvas")}</button>
        ))}
        {current && (confirmDelete ? (
          <span className="rebuild-confirm">
            {t("Delete this canvas?")} <button className="link" onClick={() => void deleteCanvas()}>{t("Delete")}</button>{" "}
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
        {root && !copying && (
          <button onClick={() => setCopying(true)} title={t("Copy every file to where this canvas puts it")}>{t("Copy out")}</button>
        )}
      </div>
      {error && <p className="warn rebuild-error">{fromEngine(error.replace(/^Error: /, ""))}</p>}
      <div className="rebuild-body">
        {copying && root && canvas !== null && <CanvasCopy canvas={canvas} copy={copy} revision={revision} onClose={() => setCopying(false)} />}
        {shared && suggestions.length > 0 && (
          <Suggestions canvas={shared.canvas} suggestions={suggestions} items={shared.items} changed={changed} fail={fail} />
        )}
        {body}
      </div>
      <p className="rebuild-foot muted">{t("This only changes how the app arranges what it found. Nothing is written to the disk.")}</p>
    </section>
  );
}

/** The top of the tree, always open, and the folder everything else goes into. */
function RootNode({ root, shared }: { root: CanvasItem; shared: Shared }) {
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
