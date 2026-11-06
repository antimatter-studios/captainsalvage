import { useCallback, useEffect, useState } from "react";
import { call } from "../api";
import { fromEngine, t } from "../i18n";

export interface MountBlockEntry {
  uuid: string;
  name: string;
}

/** The volumes the person blocked, kept in step with changes made anywhere in the window. */
export function useMountBlocks(): [Set<string>, () => void] {
  const [blocked, setBlocked] = useState<Set<string>>(new Set());
  const reload = useCallback(() => {
    call<{ blocks: MountBlockEntry[] }>("mount_blocks")
      .then((found) => setBlocked(new Set(found.blocks.map((block) => block.uuid.toUpperCase()))))
      .catch(() => undefined);
  }, []);
  useEffect(() => {
    reload();
    window.addEventListener(CHANGED, reload);
    return () => window.removeEventListener(CHANGED, reload);
  }, [reload]);
  return [blocked, reload];
}

const CHANGED = "salvage-mount-blocks-changed";

/**
 * The person's switch for keeping one volume unmounted. While Captain Salvage is open, macOS
 * refuses to mount a blocked volume, however the mount is asked for.
 */
export function MountBlockToggle({ uuid, name, mounted }: { uuid: string; name: string; mounted: boolean }) {
  const [blocked] = useMountBlocks();
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const on = blocked.has(uuid.toUpperCase());
  const change = (next: boolean) => {
    setBusy(true);
    setError(null);
    call("mount_block_set", { uuid, name, blocked: next })
      .then(() => window.dispatchEvent(new Event(CHANGED)))
      .catch((reason) => setError(String(reason)))
      .finally(() => setBusy(false));
  };
  return (
    <div className="mount-block">
      <label className="check">
        <input type="checkbox" checked={on} disabled={busy} onChange={(event) => change(event.target.checked)} />
        {t("Block mounting while Captain Salvage is open")}
      </label>
      {on && mounted && <p className="mount-block-note">{t("It is mounted now. Blocking stops the next mount, so unmount it once.")}</p>}
      {error && <p className="warn">{fromEngine(error.replace(/^Error: /, ""))}</p>}
    </div>
  );
}
