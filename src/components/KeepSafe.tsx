import { useCallback, useEffect, useState } from "react";
import { call, type StartupReport } from "../api";
import { t } from "../i18n";
import { Command } from "./Command";

const DEFAULT_DISK_SLEEP_MINUTES = 10;

/**
 * What stops the drops and remounts doing harm, and how to undo all of it afterwards, for the
 * drive the open volume is on. It asks macOS once when shown, and again only when asked to.
 */
export function KeepSafe({ device }: { device: string }) {
  const [report, setReport] = useState<StartupReport | null>(null);
  const [looking, setLooking] = useState(false);
  const look = useCallback(() => {
    setLooking(true);
    call<StartupReport>("startup_disks", { includeInternal: false })
      .then(setReport)
      .catch(() => undefined)
      .finally(() => setLooking(false));
  }, []);
  useEffect(() => look(), [look, device]);

  const volumes = report?.volumes ?? [];
  const drive = volumes.find((volume) => volume.device === device)?.wholeDisk;
  const onDrive = volumes.filter((volume) => drive && volume.wholeDisk === drive && volume.fstabLine && volume.uuid);
  if (onDrive.length === 0) return null;
  const waiting = onDrive.filter((volume) => !volume.protected);
  const names = onDrive.map((volume) => volume.name || t("unnamed volume")).join(t(" and "));
  const diskSleep = report?.diskSleepMinutes ?? null;
  const sleepsNever = diskSleep === 0;
  const restoreSleep = diskSleep && diskSleep > 0 ? diskSleep : DEFAULT_DISK_SLEEP_MINUTES;
  return (
    <section className="keep-safe">
      <div className="keep-safe-head">
        <h3>{t("Keep this disk safe")}</h3>
        <button className="link" onClick={look} disabled={looking}>{looking ? t("Checking") : t("Check again")}</button>
      </div>
      <p>{t("A damaged disk can drop off and come back. Each time, macOS may mount it and write to it, so set this up before you go on.")}</p>

      <div className={waiting.length === 0 ? "keep-step keep-step-done" : "keep-step"}>
        <span className="keep-step-mark" aria-hidden="true">{waiting.length === 0 ? "✓" : "○"}</span>
        <div className="keep-step-body">
          {waiting.length === 0 ? (
            <p><strong>{t("macOS will not mount {names}.", { names })}</strong> {t("After the next reconnect, check that it stayed unmounted.")}</p>
          ) : (
            <>
              <p><strong>{t("Stop macOS mounting {names}.", { names })}</strong> {t("Run this in Terminal. It has not been proven on a damaged disk yet, so after the next reconnect, check that it stayed unmounted.")}</p>
              {waiting.map((volume) => <Command key={volume.device} text={`printf '\\n%s\\n' '${volume.fstabLine}' | sudo tee -a /etc/fstab`} />)}
            </>
          )}
        </div>
      </div>

      <div className={sleepsNever ? "keep-step keep-step-done" : "keep-step"}>
        <span className="keep-step-mark" aria-hidden="true">{sleepsNever ? "✓" : "○"}</span>
        <div className="keep-step-body">
          {sleepsNever ? (
            <p><strong>{t("macOS will not put disks to sleep.")}</strong> {t("A drive's own firmware still may.")}</p>
          ) : (
            <>
              <p><strong>{t("Keep disks awake when this app is closed too.")}</strong> {t("While it is open, it already stops macOS putting disks to sleep. A drive's own firmware still may.")}</p>
              <Command text="sudo pmset -a disksleep 0" />
            </>
          )}
        </div>
      </div>

      <h4>{t("To go back to normal afterwards")}</h4>
      {onDrive.map((volume) => <Command key={volume.device} text={`sudo sed -i '' '/^UUID=${volume.uuid} /d' /etc/fstab`} />)}
      <Command text={`sudo pmset -a disksleep ${restoreSleep}`} />
      <p className="keep-safe-after">{t("Then reconnect the disk, and macOS mounts it as usual.")}</p>
    </section>
  );
}
