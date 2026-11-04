import type { Layer } from "./api";

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
  const t = theme();
  switch (layer) {
    case "allocation":
      return [
        { value: 1, label: "In use by current files", color: t.strong },
        { value: 2, label: "Marked free, FAT chain remains", color: t.series[0] },
        { value: 3, label: "Marked free, no FAT chain", color: t.softBlue },
      ];
    case "content":
      return [
        { value: 2, label: "Compressed or media data", color: t.series[0] },
        { value: 3, label: "Text", color: t.series[1] },
        { value: 4, label: "Audio start", color: t.series[2] },
        { value: 5, label: "Image start", color: t.series[3] },
        { value: 6, label: "Video start", color: t.series[4] },
        { value: 7, label: "Document or archive start", color: t.series[5] },
        { value: 8, label: "Binary", color: t.series[6] },
        { value: 9, label: "exFAT directory", color: t.series[7] },
        { value: 1, label: "Empty", color: t.neutral },
        { value: 0, label: "Not scanned yet", color: t.faint },
        { value: 10, label: "Read error", color: isDark() ? "#ffffff" : "#000000" },
      ];
    case "ownership":
      return [
        { value: 2, label: "Claimed by one entry", color: t.series[0] },
        { value: 3, label: "Claimed by several entries", color: t.series[1] },
        { value: 4, label: "Directory", color: t.series[2] },
        { value: 5, label: "FAT chain with no name", color: t.series[3] },
        { value: 6, label: "Explained by a hypothesis", color: t.series[4] },
        { value: 1, label: "Nothing explains it", color: t.neutral },
      ];
    default:
      return [];
  }
}

/** Builds a lookup from byte value to color for any layer. */
export function colorTable(layer: Layer): Rgb[] {
  const t = theme();
  const items = legendFor(layer);
  const table: Rgb[] = new Array(256);
  if (items.length > 0) {
    const fallback = hex(t.faint);
    for (let value = 0; value < 256; value++) {
      table[value] = fallback;
    }
    for (const item of items) {
      table[item.value] = hex(item.color);
    }
    return table;
  }
  const stops = t.ramp.map(hex);
  table[0] = hex(t.faint);
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
      return `${((value * 8) / 255).toFixed(2)} bits per byte`;
    case "confidence":
      return value === 0 ? "Below the focus floor, or unclaimed" : `Best confidence ${Math.round((value / 255) * 100)}%`;
    case "folders":
      return value === 0 ? "No directories here" : "Holds directories";
    case "lens":
      return value === 0 ? "Out of focus" : "In focus";
    default:
      return legendFor(layer).find((item) => item.value === value)?.label ?? "Unknown";
  }
}

export const SEQUENTIAL_CAPTION: Partial<Record<Layer, [string, string]>> = {
  entropy: ["Ordered, repetitive", "Random, compressed"],
  confidence: ["Low confidence", "High confidence"],
  folders: ["Few directories", "Many directories"],
  lens: ["Few clusters match", "All clusters match"],
};

export function rampGradient(): string {
  return `linear-gradient(90deg, ${theme().ramp.join(", ")})`;
}
