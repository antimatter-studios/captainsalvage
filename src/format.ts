import { currentLanguage, locale, t } from "./i18n";

/** A number with a fixed count of decimals, in the window's language. */
export function decimal(value: number, digits: number): string {
  return value.toLocaleString(locale(), { minimumFractionDigits: digits, maximumFractionDigits: digits });
}

export function bytes(value: number): string {
  // French writes bytes as octets: o, Ko, Mo, Go, To.
  const french = currentLanguage() === "fr";
  if (value < 1000) return `${value} ${french ? "o" : "B"}`;
  const units = french ? ["Ko", "Mo", "Go", "To"] : ["KB", "MB", "GB", "TB"];
  let scaled = value;
  let unit = -1;
  while (scaled >= 1000 && unit < units.length - 1) {
    scaled /= 1000;
    unit++;
  }
  return `${decimal(scaled, scaled >= 100 ? 0 : 1)} ${units[unit]}`;
}

export function count(value: number): string {
  return value.toLocaleString(locale());
}

export function percent(fraction: number): string {
  if (fraction > 0 && fraction < 0.005) return t("under 1%");
  return `${Math.round(fraction * 100)}%`;
}

/** The name the window gives a folder that lost its own, by where it starts. */
export function lostFolderName(cluster: number): string {
  return t("Lost folder {number}", { number: count(cluster) });
}

/** Shows a map path in the same words as the sidebar. */
export function displayPath(path: string | null): string {
  if (!path) return "/";
  return path.replace(/\[orphan @(\d+)\]/g, (_, cluster: string) => lostFolderName(Number(cluster)));
}

export function hexOffset(value: number): string {
  return `0x${value.toString(16)}`;
}

export function duration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return t("unknown");
  const totalMinutes = Math.round(seconds / 60);
  const hours = Math.floor(totalMinutes / 60);
  const minutes = totalMinutes % 60;
  return hours > 0 ? `${hours} h ${minutes} min` : `${minutes} min`;
}

export function clock(millis: number): string {
  return new Date(millis).toLocaleTimeString(locale(), { hour: "2-digit", minute: "2-digit" });
}

/** Shows an exFAT date as a date and time, without the ISO separator. */
export function stamp(value: string | null): string {
  return value ? value.replace("T", " ") : t("unknown");
}

const VERDICT_LABELS: Record<string, () => string> = {
  live: () => t("Visible now"),
  "tested-good": () => t("Tested good"),
  "tested-bad": () => t("Failed test"),
  "likely-intact": () => t("Likely intact"),
  plausible: () => t("Plausible"),
  partial: () => t("Partial"),
  overwritten: () => t("Overwritten"),
  empty: () => t("Empty"),
  "rename-residue": () => t("Rename leftover"),
  "damaged-entry": () => t("Damaged entry"),
  folder: () => t("Folder"),
  "folder-empty": () => t("Empty folder"),
  "folder-missing": () => t("Folder contents gone"),
};

/** What a verdict is called in the window, or the verdict itself if it has no name yet. */
export function verdictLabel(verdict: string): string {
  return VERDICT_LABELS[verdict]?.() ?? verdict;
}

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
