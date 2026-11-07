import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent, type UIEvent } from "react";
import { call, type MemberCopy, type MemberPreview, type ZipListing, type ZipMember, type ZipMemberCheck } from "../api";
import { bytes, count, stamp } from "../format";
import { fromEngine, t } from "../i18n";
import { fileIcon, Icon } from "./icons";

const ROW_HEIGHT = 28;
const ROWS_BEYOND_VIEW = 12;
const NAVIGATION_KEYS = new Set(["ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Home", "End"]);
const ARCHIVE_EXTENSIONS = new Set([
  "zip", "jar", "apk", "ipa", "epub", "cbz", "docx", "xlsx", "pptx", "odt", "ods", "odp", "pages", "numbers", "key",
  "tar", "gz", "tgz", "xz", "txz", "bz2", "tbz", "tbz2", "zst", "tzst", "7z", "rar",
]);

/** Names of archives the app can look inside. Some may not be, and the list then says so. A
 * name starting "._" is a macOS companion file, which only borrows the name of the real one. */
export function isArchiveName(name: string): boolean {
  return !name.startsWith("._") && ARCHIVE_EXTENSIONS.has(name.split(".").pop()?.toLowerCase() ?? "");
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

const METHODS: Record<number, string> = { 0: "stored", 8: "Deflate", 9: "Deflate64", 12: "BZIP2", 14: "LZMA", 93: "Zstandard", 95: "XZ" };

function CheckMark({ check, damagedInside }: { check?: ZipMemberCheck; damagedInside?: number }) {
  if (damagedInside) return <span className="zip-mark zip-mark-bad" title={`${count(damagedInside)} damaged inside`}>{count(damagedInside)} !</span>;
  if (!check) return <span className="zip-mark" />;
  if (check.status === "intact") return <span className="zip-mark zip-mark-good" title={fromEngine(check.note)}>✓</span>;
  if (check.status === "damaged") return <span className="zip-mark zip-mark-bad" title={fromEngine(check.note)}>!</span>;
  return <span className="zip-mark zip-mark-unknown" title={fromEngine(check.note)}>?</span>;
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
    <section className="center-panel zip-view" aria-label={t("Inside {name}", { name })}>
      <div className="zip-head">
        <button className="icon-button" onClick={onBack} aria-label={t("Back to the files")}><Icon name="chevronLeft" /></button>
        <div className="zip-title">
          <strong title={name}>{name}</strong>
          {listing && (
            <span className="muted">
              {t("{count} files, {unpacked} when unpacked, {packed} on the disk", { count: count(root.files), unpacked: bytes(root.size), packed: bytes(listing.totalBytes) })}
            </span>
          )}
        </div>
        {listing && root.files > 0 && (
          checking
            ? <button onClick={() => { stopCheck.current = true; }}>{t("Stop checking {done} of {total}", { done: count(checking.done), total: count(checking.total) })}</button>
            : <button onClick={() => void checkEvery()}>{t("Check every file")}</button>
        )}
      </div>

      {error && <p className="warn zip-message">{fromEngine(error)}</p>}
      {!listing && !error && <p className="muted zip-message">{t("Reading the zip from the disk.")}</p>}
      {listing?.note && <p className="zip-note">{fromEngine(listing.note)}</p>}
      {checks.size > 0 && (
        <p className="zip-summary">
          <span className="zip-mark-good">{t("{count} intact", { count: count(intact) })}</span>, <span className={damaged.size > 0 ? "zip-mark-bad" : undefined}>{t("{count} damaged", { count: count(damaged.size) })}</span>
          {checks.size > intact + damaged.size && `, ${t("{count} could not be checked", { count: count(checks.size - intact - damaged.size) })}`}
        </p>
      )}
      {listing && listing.members.length === 0 && !listing.note && <p className="muted zip-message">{t("This zip is empty.")}</p>}

      {rows.length > 0 && (
        <div
          className="zip-rows"
          ref={rowsElement}
          role="tree"
          aria-label={t("Files inside {name}", { name })}
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
              {selected.member.size === null ? t("Size unknown") : bytes(selected.member.size)}
              {selected.member.packed !== null && `, ${t("{bytes} packed", { bytes: bytes(selected.member.packed) })}`}
              {listing?.format === "zip" && `, ${METHODS[selected.member.method] === "stored" ? t("stored") : METHODS[selected.member.method] ?? t("method {number}", { number: selected.member.method })}`}
              {selected.member.encrypted && t(", encrypted")}
            </span>
            {checks.get(selected.member.index) && <span>{fromEngine(checks.get(selected.member.index)?.note)}</span>}
          </div>
          <button disabled={checking !== null} onClick={() => { const index = selected.member?.index; if (index !== undefined) checkOne(index).catch((reason) => setError(String(reason))); }}>{t("Check this file")}</button>
        </div>
      )}
      {selected?.member && !isFolder(selected) && (
        <MemberActions
          key={`${id}:${selected.member.index}`}
          id={id}
          member={selected.member}
          name={selected.name}
          onChecked={(check) => {
            const index = selected.member?.index;
            if (index !== undefined) setChecks((known) => new Map(known).set(index, check));
          }}
        />
      )}
    </section>
  );
}

/** Shows one file from inside the archive, or copies it out, from the same unpacking that checks it. */
function MemberActions({ id, member, name, onChecked }: { id: number; member: ZipMember; name: string; onChecked: (check: ZipMemberCheck) => void }) {
  const [preview, setPreview] = useState<MemberPreview | null>(null);
  const [copying, setCopying] = useState(false);
  const [destination, setDestination] = useState("");
  const [copied, setCopied] = useState<MemberCopy | null>(null);
  const [busy, setBusy] = useState<"show" | "copy" | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const size = member.size === null ? t("size unknown") : bytes(member.size);

  const show = async () => {
    setBusy("show");
    setProblem(null);
    try {
      const value = await call<MemberPreview>("zip_member_preview", { id, index: member.index });
      setPreview(value);
      if (value.check) onChecked(value.check);
    } catch (reason) {
      setProblem(String(reason));
    } finally {
      setBusy(null);
    }
  };

  const openCopy = async () => {
    setCopying(true);
    setCopied(null);
    setProblem(null);
    if (!destination) {
      const drive = await call<{ destination: string | null }>("get_salvage_drive").catch(() => ({ destination: null }));
      setDestination(drive.destination ?? "");
    }
  };

  const copy = async () => {
    setBusy("copy");
    setProblem(null);
    try {
      const value = await call<MemberCopy>("zip_member_copy", { id, index: member.index, destination: destination.trim() });
      setCopied(value);
      onChecked(value.check);
    } catch (reason) {
      setProblem(fromEngine(String(reason)));
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="member-actions">
      <div className="member-buttons">
        <button disabled={busy !== null} onClick={() => void show()}>{busy === "show" ? t("Unpacking") : t("Show it")}</button>
        <button disabled={busy !== null} onClick={() => void openCopy()}>{t("Copy it out")}</button>
      </div>
      {copying && (
        <form
          className="member-copy"
          onSubmit={(event) => {
            event.preventDefault();
            void copy();
          }}
        >
          <label>
            <span>{t("Copy {name} ({size}) to this folder", { name, size })}</span>
            <input value={destination} onChange={(event) => setDestination(event.target.value)} placeholder={t("A folder on another drive")} spellCheck={false} />
          </label>
          <button className="primary" type="submit" disabled={busy !== null || !destination.trim()}>{busy === "copy" ? t("Copying") : t("Copy {size}", { size })}</button>
        </form>
      )}
      {copied && (
        <p className={copied.check.status === "damaged" ? "member-note bad" : "member-note"}>
          {t("Copied to {path}.", { path: copied.path })} {fromEngine(copied.check.note)}
        </p>
      )}
      {problem && <p className="member-note bad">{problem}</p>}
      {preview && preview.kind === "none" && <p className="member-note">{t("This kind of file cannot be shown here. Copy it out to open it.")}</p>}
      {preview && preview.kind === "too-big" && <p className="member-note">{t("This picture is too big to show here. Copy it out to open it.")}</p>}
      {preview && preview.kind === "image" && preview.dataBase64 && (
        <div className="member-preview">
          <img src={`data:${preview.mime ?? "image/jpeg"};base64,${preview.dataBase64}`} alt={name} />
        </div>
      )}
      {preview && preview.kind === "text" && (
        <div className="member-preview">
          <pre>{preview.text}</pre>
          {preview.truncated && <p className="member-note">{t("Only the start is shown, {shown} of {total}.", { shown: bytes((preview.text ?? "").length), total: bytes(preview.bytes) })}</p>}
        </div>
      )}
    </div>
  );
}
