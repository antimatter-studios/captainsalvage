import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent, type UIEvent } from "react";
import { call, type ZipListing, type ZipMember, type ZipMemberCheck } from "../api";
import { bytes, count, stamp } from "../format";
import { fileIcon, Icon } from "./icons";

const ROW_HEIGHT = 28;
const ROWS_BEYOND_VIEW = 12;
const NAVIGATION_KEYS = new Set(["ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Home", "End"]);
const ZIP_EXTENSIONS = new Set(["zip", "jar", "apk", "ipa", "epub", "cbz", "docx", "xlsx", "pptx", "odt", "ods", "odp", "pages", "numbers", "key"]);

/** Names that are zip files inside. Some may not be, and the list then says so. A name
 * starting "._" is a macOS companion file, which only borrows the name of the real one. */
export function isZipName(name: string): boolean {
  return !name.startsWith("._") && ZIP_EXTENSIONS.has(name.split(".").pop()?.toLowerCase() ?? "");
}

interface TreeNode {
  name: string;
  path: string;
  member: ZipMember | null;
  children: Map<string, TreeNode>;
  files: number;
  size: number;
  memberIndexes: number[];
}

function newNode(name: string, path: string): TreeNode {
  return { name, path, member: null, children: new Map(), files: 0, size: 0, memberIndexes: [] };
}

/** A zip lists paths and need not list its folders, so the tree is built from the paths. */
function buildTree(members: ZipMember[]): TreeNode {
  const root = newNode("", "");
  for (const member of members) {
    const parts = member.path.split("/").filter((part) => part !== "");
    let node = root;
    if (!member.isDir) {
      root.files += 1;
      root.size += member.size ?? 0;
      root.memberIndexes.push(member.index);
    }
    parts.forEach((part, position) => {
      const path = parts.slice(0, position + 1).join("/");
      let next = node.children.get(part);
      if (!next) {
        next = newNode(part, path);
        node.children.set(part, next);
      }
      node = next;
      if (position === parts.length - 1) {
        node.member = member;
      } else if (!member.isDir) {
        node.files += 1;
        node.size += member.size ?? 0;
        node.memberIndexes.push(member.index);
      }
    });
  }
  return root;
}

function isFolder(node: TreeNode): boolean {
  return node.children.size > 0 || (node.member?.isDir ?? true);
}

function sortedChildren(node: TreeNode): TreeNode[] {
  return [...node.children.values()].sort((a, b) => {
    const folders = Number(isFolder(b)) - Number(isFolder(a));
    return folders !== 0 ? folders : a.name.localeCompare(b.name, undefined, { numeric: true });
  });
}

interface VisibleRow {
  node: TreeNode;
  depth: number;
  parent: string | null;
}

function visibleRows(root: TreeNode, open: Set<string>): VisibleRow[] {
  const rows: VisibleRow[] = [];
  const walk = (node: TreeNode, depth: number, parent: string | null) => {
    for (const child of sortedChildren(node)) {
      rows.push({ node: child, depth, parent });
      if (open.has(child.path)) walk(child, depth + 1, child.path);
    }
  };
  walk(root, 0, null);
  return rows;
}

const METHODS: Record<number, string> = { 0: "Stored", 8: "Deflate", 9: "Deflate64", 12: "BZIP2", 14: "LZMA", 93: "Zstandard", 95: "XZ" };

function CheckMark({ check, damagedInside }: { check?: ZipMemberCheck; damagedInside?: number }) {
  if (damagedInside) return <span className="zip-mark zip-mark-bad" title={`${count(damagedInside)} damaged inside`}>{count(damagedInside)} !</span>;
  if (!check) return <span className="zip-mark" />;
  if (check.status === "intact") return <span className="zip-mark zip-mark-good" title={check.note}>✓</span>;
  if (check.status === "damaged") return <span className="zip-mark zip-mark-bad" title={check.note}>!</span>;
  return <span className="zip-mark zip-mark-unknown" title={check.note}>?</span>;
}

/** What a zip holds, as a tree, read live from the disk. */
export function ZipView({ id, name, onBack }: { id: number; name: string; onBack: () => void }) {
  const [listing, setListing] = useState<ZipListing | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [open, setOpen] = useState<Set<string>>(new Set());
  const [cursor, setCursor] = useState<string | null>(null);
  const [checks, setChecks] = useState<Map<number, ZipMemberCheck>>(new Map());
  const [checking, setChecking] = useState<{ done: number; total: number } | null>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const [viewHeight, setViewHeight] = useState(600);
  const stopCheck = useRef(false);
  const rowsElement = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    let current = true;
    setListing(null);
    setError(null);
    setChecks(new Map());
    setCursor(null);
    setScrollTop(0);
    stopCheck.current = true;
    call<ZipListing>("zip_contents", { id })
      .then((value) => {
        if (!current) return;
        setListing(value);
        // A single folder at the top holds everything, so it may as well start open.
        const tops = new Set(value.members.map((member) => member.path.split("/")[0]));
        setOpen(tops.size === 1 ? tops : new Set());
      })
      .catch((reason) => current && setError(String(reason)));
    return () => {
      current = false;
    };
  }, [id]);

  useEffect(() => {
    const element = rowsElement.current;
    if (!element) return;
    const observer = new ResizeObserver(() => setViewHeight(element.clientHeight));
    observer.observe(element);
    return () => observer.disconnect();
  }, [listing]);

  const root = useMemo(() => buildTree(listing?.members ?? []), [listing]);
  const rows = useMemo(() => visibleRows(root, open), [root, open]);
  const damaged = useMemo(() => new Set([...checks].filter(([, check]) => check.status === "damaged").map(([index]) => index)), [checks]);
  const intact = useMemo(() => [...checks.values()].filter((check) => check.status === "intact").length, [checks]);
  const cursorIndex = rows.findIndex((row) => row.node.path === cursor);
  const selected = cursorIndex >= 0 ? rows[cursorIndex].node : null;

  const toggle = useCallback((path: string) => {
    setOpen((known) => {
      const next = new Set(known);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  }, []);

  const moveTo = (index: number) => {
    const row = rows[index];
    if (!row) return;
    setCursor(row.node.path);
    const element = rowsElement.current;
    if (!element) return;
    const top = index * ROW_HEIGHT;
    if (top < element.scrollTop) element.scrollTop = top;
    else if (top + ROW_HEIGHT > element.scrollTop + element.clientHeight) element.scrollTop = top + ROW_HEIGHT - element.clientHeight;
  };

  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (!NAVIGATION_KEYS.has(event.key)) return;
    event.preventDefault();
    const index = cursorIndex;
    if (event.key === "ArrowDown") moveTo(index + 1);
    else if (event.key === "ArrowUp") moveTo(Math.max(index - 1, 0));
    else if (event.key === "Home") moveTo(0);
    else if (event.key === "End") moveTo(rows.length - 1);
    else if (index >= 0) {
      const row = rows[index];
      const folder = isFolder(row.node);
      const isOpen = open.has(row.node.path);
      if (event.key === "ArrowRight") {
        if (folder && !isOpen) toggle(row.node.path);
        else if (folder) moveTo(index + 1);
      } else if (folder && isOpen) {
        toggle(row.node.path);
      } else if (row.parent !== null) {
        moveTo(rows.findIndex((other) => other.node.path === row.parent));
      }
    }
  };

  const checkOne = useCallback(async (index: number) => {
    const result = await call<ZipMemberCheck>("zip_member_check", { id, index });
    setChecks((known) => new Map(known).set(index, result));
  }, [id]);

  const checkEvery = useCallback(async () => {
    const files = (listing?.members ?? []).filter((member) => !member.isDir);
    stopCheck.current = false;
    setChecking({ done: 0, total: files.length });
    for (let position = 0; position < files.length; position += 1) {
      if (stopCheck.current) break;
      try {
        await checkOne(files[position].index);
      } catch (reason) {
        setError(String(reason));
        break;
      }
      setChecking({ done: position + 1, total: files.length });
    }
    setChecking(null);
  }, [listing, checkOne]);

  // A check that is still going must not outlive the view it reports to.
  useEffect(() => () => {
    stopCheck.current = true;
  }, []);

  const first = Math.max(Math.floor(scrollTop / ROW_HEIGHT) - ROWS_BEYOND_VIEW, 0);
  const last = Math.min(Math.ceil((scrollTop + viewHeight) / ROW_HEIGHT) + ROWS_BEYOND_VIEW, rows.length);

  return (
    <section className="center-panel zip-view" aria-label={`Inside ${name}`}>
      <div className="zip-head">
        <button className="icon-button" onClick={onBack} aria-label="Back to the files"><Icon name="chevronLeft" /></button>
        <div className="zip-title">
          <strong title={name}>{name}</strong>
          {listing && (
            <span className="muted">
              {count(root.files)} files, {bytes(root.size)} when unpacked, {bytes(listing.totalBytes)} on the disk
            </span>
          )}
        </div>
        {listing && root.files > 0 && (
          checking
            ? <button onClick={() => { stopCheck.current = true; }}>Stop checking {count(checking.done)} of {count(checking.total)}</button>
            : <button onClick={() => void checkEvery()}>Check every file</button>
        )}
      </div>

      {error && <p className="warn zip-message">{error}</p>}
      {!listing && !error && <p className="muted zip-message">Reading the zip from the disk.</p>}
      {listing?.note && <p className="zip-note">{listing.note}</p>}
      {checks.size > 0 && (
        <p className="zip-summary">
          <span className="zip-mark-good">{count(intact)} intact</span>, <span className={damaged.size > 0 ? "zip-mark-bad" : undefined}>{count(damaged.size)} damaged</span>
          {checks.size > intact + damaged.size && `, ${count(checks.size - intact - damaged.size)} could not be checked`}
        </p>
      )}
      {listing && listing.members.length === 0 && !listing.note && <p className="muted zip-message">This zip is empty.</p>}

      {rows.length > 0 && (
        <div
          className="zip-rows"
          ref={rowsElement}
          role="tree"
          aria-label={`Files inside ${name}`}
          tabIndex={0}
          onKeyDown={onKeyDown}
          onScroll={(event: UIEvent<HTMLDivElement>) => setScrollTop(event.currentTarget.scrollTop)}
        >
          <div style={{ height: rows.length * ROW_HEIGHT, position: "relative" }}>
            {rows.slice(first, last).map((row, offset) => {
              const folder = isFolder(row.node);
              const isOpen = open.has(row.node.path);
              const member = row.node.member;
              const damagedInside = folder ? row.node.memberIndexes.filter((index) => damaged.has(index)).length : 0;
              return (
                <div
                  key={row.node.path}
                  role="treeitem"
                  aria-level={row.depth + 1}
                  aria-expanded={folder ? isOpen : undefined}
                  aria-selected={row.node.path === cursor}
                  className={row.node.path === cursor ? "zip-row row-cursor" : "zip-row"}
                  style={{ top: (first + offset) * ROW_HEIGHT, height: ROW_HEIGHT, paddingLeft: 8 + row.depth * 16 }}
                  onClick={() => {
                    setCursor(row.node.path);
                    rowsElement.current?.focus();
                    if (folder) toggle(row.node.path);
                  }}
                >
                  <span className="disclosure">{folder && <Icon name={isOpen ? "chevronDown" : "chevronRight"} size={12} />}</span>
                  <span className="row-icon"><Icon name={folder ? "folder" : fileIcon(row.node.name)} size={15} /></span>
                  <span className="zip-name" title={row.node.path}>{row.node.name}</span>
                  <span className="zip-size">{folder ? `${count(row.node.files)} files` : member?.size === null || member?.size === undefined ? "size unknown" : bytes(member.size)}</span>
                  <span className="zip-date">{folder ? "" : stamp(member?.modified ?? null)}</span>
                  <CheckMark check={member && !folder ? checks.get(member.index) : undefined} damagedInside={damagedInside} />
                </div>
              );
            })}
          </div>
        </div>
      )}

      {selected?.member && !isFolder(selected) && (
        <div className="zip-selected">
          <div className="zip-selected-text">
            <strong title={selected.path}>{selected.path}</strong>
            <span className="muted">
              {selected.member.size === null ? "Size unknown" : bytes(selected.member.size)}
              {selected.member.packed !== null && `, ${bytes(selected.member.packed)} packed`}
              {`, ${METHODS[selected.member.method] ?? `method ${selected.member.method}`}`}
              {selected.member.encrypted && ", encrypted"}
            </span>
            {checks.get(selected.member.index) && <span>{checks.get(selected.member.index)?.note}</span>}
          </div>
          <button disabled={checking !== null} onClick={() => { const index = selected.member?.index; if (index !== undefined) checkOne(index).catch((reason) => setError(String(reason))); }}>Check this file</button>
        </div>
      )}
    </section>
  );
}
