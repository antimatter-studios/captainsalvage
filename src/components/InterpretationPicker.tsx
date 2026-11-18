import { t } from "../i18n";
import type { ChoiceId, InterpretationChoice, OpenedSource } from "../openedSource";

/** What one choice reads as in the picker, such as `NTFS (detected, 86%)`. */
function choiceText(choice: InterpretationChoice): string {
  if (choice.id === "none") return t("No filesystem (raw bytes)");
  if (choice.detected && choice.confidence !== null) return t("{name} (detected, {percent}%)", { name: choice.id, percent: Math.round(choice.confidence * 100) });
  if (choice.available && choice.origin === "saved") return t("{name} (saved)", { name: choice.id });
  return choice.id;
}

/**
 * The opened source's interpretation, in the header of its workspace: what the bytes are read as,
 * which the person can change. A partition shows the choice, with the detected filesystem marked
 * and its confidence. A whole disk has no filesystem of its own: it is raw bytes, with its extents
 * to open. Changing the choice makes a new scan and keeps the earlier one, which says so.
 */
export function InterpretationPicker({ opened, busy, note, onSwitch, onScans }: {
  opened: OpenedSource;
  busy: boolean;
  note: { text: string; warn: boolean } | null;
  onSwitch: (id: ChoiceId) => void;
  onScans: () => void;
}) {
  const current = opened.choices.find((choice) => choice.current)?.id ?? "none";
  return (
    <span className="interp">
      {opened.scope === "partition" ? (
        <label className="interp-pick">
          {t("Filesystem:")}
          <select value={current} disabled={busy} aria-label={t("Filesystem")} onChange={(event) => onSwitch(event.target.value as ChoiceId)}>
            {opened.choices.map((choice) => (
              <option key={choice.id} value={choice.id} title={choice.available ? undefined : t("No boot evidence found for this. Choosing it explains how to look for some.")}>{choiceText(choice)}</option>
            ))}
          </select>
        </label>
      ) : (
        <span className="interp-whole">{t("Whole disk: raw bytes. Open one of its extents to read it as a filesystem.")}</span>
      )}
      <button className="link interp-scans" onClick={onScans}>{t("Saved scans ({count})", { count: opened.scans.length })}</button>
      {note && <span className={note.warn ? "interp-note warn" : "interp-note"} role="status">{note.text}</span>}
    </span>
  );
}
