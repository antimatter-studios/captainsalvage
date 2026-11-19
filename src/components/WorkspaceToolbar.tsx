import type { ReactNode } from "react";
import { bytes } from "../format";
import { fromEngine, t } from "../i18n";

export function WorkspaceToolbar({ path, length, label, description, progress, onChangeDisk, children }: {
  path: string;
  length: number;
  label?: string;
  description?: ReactNode;
  progress: { text: string; fraction: number; title?: string; error?: string | null };
  onChangeDisk: () => void;
  children: ReactNode;
}) {
  const fraction = Math.max(0, Math.min(1, progress.fraction));
  return (
    <header className="toolbar" data-tauri-drag-region>
      <div className="brand title-text">
        <strong className="brand-device title-name" title={path}><bdi>{label || path}</bdi></strong>
        <div className="brand-meta title-sub">
          <span className="brand-size">{bytes(length)}</span>
          <button className="link brand-change" onClick={onChangeDisk}>{t("Change disk")}</button>
          {description}
        </div>
      </div>
      <div className="scan">
        <div className="scan-text" title={progress.title ?? progress.text}>{progress.text}</div>
        <div className="scan-bar" role="progressbar" aria-label={progress.text} aria-valuenow={Math.round(fraction * 100)} aria-valuemin={0} aria-valuemax={100}>
          <div className="scan-fill" style={{ width: `${fraction * 100}%` }} />
        </div>
      </div>
      <div className="toolbar-spacer" data-tauri-drag-region />
      {children}
      {/* An error is a line of its own under the title row, the whole width of the window: in the narrow
          column of the progress bar it broke into a word to a line and ran off the top. */}
      {progress.error && <div className="toolbar-error" role="alert">{fromEngine(progress.error)}</div>}
    </header>
  );
}
