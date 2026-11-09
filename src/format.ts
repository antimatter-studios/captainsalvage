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

const GROUP_NAMES: Record<string, () => string> = {
  photos: () => t("Photos"),
  videos: () => t("Videos"),
  music: () => t("Music"),
  documents: () => t("Documents"),
  archives: () => t("Archives"),
  other: () => t("Other files"),
};

const MOSTLY_NAMES: Record<string, () => string> = {
  photos: () => t("Mostly photos"),
  videos: () => t("Mostly videos"),
  music: () => t("Mostly music"),
  documents: () => t("Mostly documents"),
  archives: () => t("Mostly archives"),
  other: () => t("Mostly other files"),
};

/** All of one type, or more than half of one, by the bytes in the folder. */
const ALL_OF_ONE_SHARE = 0.999;
const MOSTLY_SHARE = 0.5;

/**
 * A suggested label for a folder that lost its name, from what its own files are, such as
 * "Mostly videos, 128 files". It is only a guess, so the window shows it as one. Null when the
 * folder holds no files of its own.
 */
export function guessedFolderLabel(contents: { group: string; share: number; files: number } | undefined): string | null {
  if (!contents || contents.files === 0) return null;
  const kind =
    contents.share >= ALL_OF_ONE_SHARE
      ? (GROUP_NAMES[contents.group] ?? GROUP_NAMES.other)()
      : contents.share >= MOSTLY_SHARE
        ? (MOSTLY_NAMES[contents.group] ?? MOSTLY_NAMES.other)()
        : t("Mixed files");
  return contents.files === 1 ? t("{kind}, 1 file", { kind }) : t("{kind}, {count} files", { kind, count: count(contents.files) });
}

/** Shows a map path in the same words as the sidebar. Simple mode leaves out cluster numbers. */
export function displayPath(path: string | null, simple = false): string {
  if (!path) return "/";
  return path.replace(/\[orphan @(\d+)\]/g, (_, cluster: string) => (simple ? t("Lost folder") : lostFolderName(Number(cluster))));
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

/** Shows an exFAT date as a date and time, without the ISO separator, and its zone when the entry recorded one. */
export function stamp(value: string | null): string {
  return value ? value.replace("T", " ").replace(/([+-]\d\d:\d\d)$/, " UTC$1") : t("unknown");
}

const VERDICT_LABELS: Record<string, () => string> = {
  live: () => t("Visible now"),
  "tested-good": () => t("Tested good"),
  "tested-bad": () => t("Failed test"),
  "tested-gaps": () => t("Plays with gaps"),
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
