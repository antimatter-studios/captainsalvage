import type { Layer } from "./api";
import { decimal } from "./format";
import { t } from "./i18n";

export type Rgb = [number, number, number];

function hex(value: string): Rgb {
  const number = parseInt(value.slice(1), 16);
  return [(number >> 16) & 255, (number >> 8) & 255, number & 255];
}

const DARK_QUERY = "(prefers-color-scheme: dark)";

export function isDark(): boolean {
  return window.matchMedia(DARK_QUERY).matches;
}

/** Calls back when the window's colours change, whether from macOS or the switch. */
export function onThemeChange(listener: () => void): () => void {
  const query = window.matchMedia(DARK_QUERY);
  query.addEventListener("change", listener);
  return () => query.removeEventListener("change", listener);
}

// Slot order and steps come from the validated reference palette. The validator
// passes them in this order against each surface below.
const SERIES_LIGHT = ["#2a78d6", "#eb6834", "#1baf7a", "#eda100", "#e87ba4", "#008300", "#6250d6", "#e34948"];
const SERIES_DARK = ["#3987e5", "#d95926", "#199e70", "#c98500", "#d55181", "#008300", "#9085e9", "#e66767"];
const RAMP_LIGHT = ["#e6eff9", "#9ec5f4", "#5598e7", "#256abf", "#0d366b"];
const RAMP_DARK = ["#16324d", "#184f95", "#2a78d6", "#6da7ec", "#cde2fb"];

interface Theme {
  series: string[];
  ramp: string[];
  surface: string;
  neutral: string;
  faint: string;
  strong: string;
  softBlue: string;
}

function theme(): Theme {
  return isDark()
    ? { series: SERIES_DARK, ramp: RAMP_DARK, surface: "#17191e", neutral: "#464a53", faint: "#25282f", strong: "#b9bdc6", softBlue: "#1d4f8f" }
    : { series: SERIES_LIGHT, ramp: RAMP_LIGHT, surface: "#ffffff", neutral: "#c3c7cf", faint: "#e9ebef", strong: "#2d3340", softBlue: "#9ec5f4" };
}

export interface LegendItem {
  value: number;
  label: string;
  color: string;
}

/** The colors and labels for a layer whose values are categories. */
export function legendFor(layer: Layer): LegendItem[] {
  const colors = theme();
  switch (layer) {
    case "allocation":
      return [
        { value: 1, label: t("In use by current files"), color: colors.strong },
        { value: 2, label: t("Marked free, FAT chain remains"), color: colors.series[0] },
        { value: 3, label: t("Marked free, no FAT chain"), color: colors.softBlue },
      ];
    case "content":
      return [
        { value: 2, label: t("Compressed or media data"), color: colors.series[0] },
        { value: 3, label: t("Text"), color: colors.series[1] },
        { value: 4, label: t("Audio start"), color: colors.series[2] },
        { value: 5, label: t("Image start"), color: colors.series[3] },
        { value: 6, label: t("Video start"), color: colors.series[4] },
        { value: 7, label: t("Document or archive start"), color: colors.series[5] },
        { value: 8, label: t("Binary"), color: colors.series[6] },
        { value: 9, label: t("exFAT directory"), color: colors.series[7] },
        { value: 1, label: t("Empty"), color: colors.neutral },
        { value: 0, label: t("Not scanned yet"), color: colors.faint },
        { value: 10, label: t("Read error"), color: isDark() ? "#ffffff" : "#000000" },
      ];
    case "ownership":
      return [
        { value: 2, label: t("Claimed by one entry"), color: colors.series[0] },
        { value: 3, label: t("Claimed by several entries"), color: colors.series[1] },
        { value: 4, label: t("Directory"), color: colors.series[2] },
        { value: 5, label: t("FAT chain with no name"), color: colors.series[3] },
        { value: 6, label: t("Explained by a hypothesis"), color: colors.series[4] },
        { value: 1, label: t("Nothing explains it"), color: colors.neutral },
      ];
    default:
      return [];
  }
}

/** Builds a lookup from byte value to color for any layer. */
export function colorTable(layer: Layer): Rgb[] {
  const colors = theme();
  const items = legendFor(layer);
  const table: Rgb[] = new Array(256);
  if (items.length > 0) {
    const fallback = hex(colors.faint);
    for (let value = 0; value < 256; value++) {
      table[value] = fallback;
    }
    for (const item of items) {
      table[item.value] = hex(item.color);
    }
    return table;
  }
  const stops = colors.ramp.map(hex);
  table[0] = hex(colors.faint);
  for (let value = 1; value < 256; value++) {
    const position = ((value - 1) / 254) * (stops.length - 1);
    const low = Math.min(Math.floor(position), stops.length - 2);
    const mix = position - low;
    table[value] = [0, 1, 2].map((channel) => Math.round(stops[low][channel] + (stops[low + 1][channel] - stops[low][channel]) * mix)) as Rgb;
  }
  return table;
}

/** What a sequential layer's value means, for tooltips and the legend caption. */
export function describeValue(layer: Layer, value: number): string {
  switch (layer) {
    case "entropy":
      return t("{bits} bits per byte", { bits: decimal((value * 8) / 255, 2) });
    case "confidence":
      return value === 0 ? t("Below the focus floor, or unclaimed") : t("Best confidence {percent}%", { percent: Math.round((value / 255) * 100) });
    case "folders":
      return value === 0 ? t("No directories here") : t("Holds directories");
    case "lens":
      return value === 0 ? t("Out of focus") : t("In focus");
    default:
      return legendFor(layer).find((item) => item.value === value)?.label ?? t("Unknown");
  }
}

/** The words at each end of a sequential layer's legend. */
export function sequentialCaption(layer: Layer): [string, string] | undefined {
  switch (layer) {
    case "entropy":
      return [t("Ordered, repetitive"), t("Random, compressed")];
    case "confidence":
      return [t("Low confidence"), t("High confidence")];
    case "folders":
      return [t("Few directories"), t("Many directories")];
    case "lens":
      return [t("Few clusters match"), t("All clusters match")];
    default:
      return undefined;
  }
}

export function rampGradient(): string {
  return `linear-gradient(90deg, ${theme().ramp.join(", ")})`;
}
