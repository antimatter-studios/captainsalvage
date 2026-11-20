import { useCallback, useEffect, useState } from "react";
import { call } from "../api";
import { fromEngine, t } from "../i18n";
import { afterSystemCommand } from "../systemCommand";

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
 * refuses to mount a blocked volume, however the mount is asked for. The compact form is a small
 * inline checkbox for a row; its note and error are siblings for the row to lay out below it.
 * `uuid` is the volume's UUID, or the key of its place when macOS gives it none, and then `device` names
 * the volume so that its place is read when it is blocked.
 */
export function MountBlockToggle({ uuid, device, name, mounted, compact = false }: { uuid: string; device?: string; name: string; mounted: boolean; compact?: boolean }) {
  const [blocked] = useMountBlocks();
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const on = blocked.has(uuid.toUpperCase());
  const change = (next: boolean) => {
    setBusy(true);
    setError(null);
    afterSystemCommand(() => call("mount_block_set", { uuid, device, name, blocked: next }))
      .then(() => window.dispatchEvent(new Event(CHANGED)))
      .catch((reason) => setError(String(reason)))
      .finally(() => setBusy(false));
  };
  if (compact) {
    return (
      <>
        <label className="check mount-block-inline" title={t("Stop macOS from mounting this volume while Captain Salvage is open.")}>
          <input type="checkbox" checked={on} disabled={busy} onChange={(event) => change(event.target.checked)} />
          {t("Block mount")}
        </label>
        {on && mounted && <p className="mount-block-note start-row-note">{t("It is mounted now. Blocking stops the next mount, so unmount it once.")}</p>}
        {error && <p className="warn start-row-note">{fromEngine(error.replace(/^Error: /, ""))}</p>}
      </>
    );
  }
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
