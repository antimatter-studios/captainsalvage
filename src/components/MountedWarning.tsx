import { useCallback, useEffect, useState } from "react";
import { call } from "../api";
import { t } from "../i18n";

interface MountedAnyway {
  uuid: string;
  name: string;
  device: string;
  mountPoint: string;
}

const CHECK_EVERY_MILLIS = 15_000;

/**
 * Says plainly, on every screen, when a volume the person asked to keep unmounted is mounted
 * anyway. That happens when it was mounted before the app started: at boot, at login, or while
 * the app was closed. Unmounting is the person's to do, so the button only asks macOS to.
 */
export function MountedWarning() {
  const [mounted, setMounted] = useState<MountedAnyway[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [problem, setProblem] = useState<string | null>(null);

  const refresh = useCallback(() => {
    call<{ mounted: MountedAnyway[] }>("mount_blocks_mounted")
      .then((value) => setMounted(value.mounted))
      .catch(() => undefined);
  }, []);

  useEffect(() => {
    refresh();
    const timer = window.setInterval(refresh, CHECK_EVERY_MILLIS);
    window.addEventListener("focus", refresh);
    return () => {
      window.clearInterval(timer);
      window.removeEventListener("focus", refresh);
    };
  }, [refresh]);

  if (mounted.length === 0) return null;

  const unmount = async (volume: MountedAnyway) => {
    setBusy(volume.uuid);
    setProblem(null);
    try {
      await call("mount_block_unmount", { uuid: volume.uuid });
    } catch (reason) {
      setProblem(String(reason));
    } finally {
      setBusy(null);
      refresh();
    }
  };

  return (
    <div className="mounted-warning" role="alert">
      {mounted.map((volume) => (
        <div key={volume.uuid} className="mounted-warning-row">
          <div className="mounted-warning-text">
            <strong>
              {t("{name} is mounted, though you asked to keep it unmounted.", {
                name: volume.name,
              })}
            </strong>
            <span>
              {t(
                "It was mounted before Captain Salvage could stop it, at startup or while the app was closed. While it stays mounted, macOS may write to it. Unmounting writes a little too, but much less.",
              )}
            </span>
          </div>
          <button
            className="primary"
            disabled={busy !== null}
            onClick={() => void unmount(volume)}
          >
            {busy === volume.uuid
              ? t("Unmounting")
              : t("Unmount {name}", { name: volume.name })}
          </button>
        </div>
      ))}
      {problem && <p className="mounted-warning-problem">{problem}</p>}
    </div>
  );
}
