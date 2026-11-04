export function bytes(value: number): string {
  if (value < 1000) return `${value} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let scaled = value;
  let unit = -1;
  while (scaled >= 1000 && unit < units.length - 1) {
    scaled /= 1000;
    unit++;
  }
  return `${scaled >= 100 ? scaled.toFixed(0) : scaled.toFixed(1)} ${units[unit]}`;
}

export function count(value: number): string {
  return value.toLocaleString("en-US");
}

export function percent(fraction: number): string {
  if (fraction > 0 && fraction < 0.005) return "under 1%";
  return `${Math.round(fraction * 100)}%`;
}

/** Shows a map path in the same words as the sidebar. */
export function displayPath(path: string | null): string {
  if (!path) return "/";
  return path.replace(/\[orphan @(\d+)\]/g, (_, cluster: string) => `Lost folder, cluster ${Number(cluster).toLocaleString("en-US")}`);
}

export function hexOffset(value: number): string {
  return `0x${value.toString(16)}`;
}

export function duration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "unknown";
  const totalMinutes = Math.round(seconds / 60);
  const hours = Math.floor(totalMinutes / 60);
  const minutes = totalMinutes % 60;
  return hours > 0 ? `${hours} h ${minutes} min` : `${minutes} min`;
}

export function clock(millis: number): string {
  return new Date(millis).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

/** Shows an exFAT date as a date and time, without the ISO separator. */
export function stamp(value: string | null): string {
  return value ? value.replace("T", " ") : "unknown";
}

export const VERDICT_LABEL: Record<string, string> = {
  live: "Visible now",
  "likely-intact": "Likely intact",
  plausible: "Plausible",
  partial: "Partial",
  overwritten: "Overwritten",
  empty: "Empty",
  "rename-residue": "Rename leftover",
  "damaged-entry": "Damaged entry",
  folder: "Folder",
  "folder-empty": "Empty folder",
  "folder-missing": "Folder contents gone",
};

/** Status is never color alone, so each verdict also has its own glyph. */
export const VERDICT_GLYPH: Record<string, string> = {
  live: "●",
  "likely-intact": "✓",
  plausible: "~",
  partial: "◐",
  overwritten: "✕",
  empty: "○",
  "rename-residue": "↺",
  "damaged-entry": "!",
  folder: "▸",
  "folder-empty": "○",
  "folder-missing": "✕",
};
