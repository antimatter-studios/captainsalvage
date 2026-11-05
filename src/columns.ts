import { useCallback, useEffect, useState } from "react";

const STORAGE_KEY = "lens-column-widths";
// The sidebar's top row holds the window controls, the app's name and the home button, so the
// sidebar cannot be narrower than that row.
const DEFAULT_LEFT = 320;
const MIN_LEFT = 320;
const MAX_LEFT = 620;
const MIN_CENTER = 380;
const MIN_RIGHT = 300;
/** Until the right edge has been dragged, the two columns share the space by these rules. */
const DEFAULT_CENTER_AND_RIGHT = "minmax(640px, 1.1fr) minmax(420px, 1fr)";
const DEFAULT_CENTER_AND_RIGHT_MINIMUM = 640 + 420;

export interface ColumnWidths {
  left: number;
  right: number | null;
}

const clamp = (value: number, low: number, high: number) => Math.max(low, Math.min(high, value));

function fitLeft(left: number, right: number | null, windowWidth: number): number {
  const others = right === null ? DEFAULT_CENTER_AND_RIGHT_MINIMUM : right + MIN_CENTER;
  return clamp(left, MIN_LEFT, Math.max(MIN_LEFT, Math.min(MAX_LEFT, windowWidth - others)));
}

function fitRight(right: number, left: number, windowWidth: number): number {
  return clamp(right, MIN_RIGHT, Math.max(MIN_RIGHT, windowWidth - left - MIN_CENTER));
}

/** Keeps every column usable at this window width, giving up the right panel's width first. */
export function fitWidths(widths: ColumnWidths, windowWidth: number): ColumnWidths {
  const right = widths.right === null ? null : fitRight(widths.right, widths.left, windowWidth);
  return { left: fitLeft(widths.left, right, windowWidth), right };
}

function remembered(): ColumnWidths {
  try {
    const saved = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "null");
    if (typeof saved?.left === "number" && (saved.right === null || typeof saved.right === "number")) return saved;
  } catch {
    // Remembered widths are only a convenience.
  }
  return { left: DEFAULT_LEFT, right: null };
}

export function useColumnWidths() {
  const [wanted, setWanted] = useState<ColumnWidths>(remembered);
  const [windowWidth, setWindowWidth] = useState(window.innerWidth);

  useEffect(() => {
    const onResize = () => setWindowWidth(window.innerWidth);
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);

  useEffect(() => {
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(wanted));
    } catch {
      // Remembered widths are only a convenience.
    }
  }, [wanted]);

  const widths = fitWidths(wanted, windowWidth);
  // Dragging one edge stops at the middle column's minimum, and never moves the other edge.
  const setLeft = useCallback((left: number) => setWanted((current) => ({ ...current, left: fitLeft(left, current.right, window.innerWidth) })), []);
  const setRight = useCallback((right: number) => setWanted((current) => ({ ...current, right: fitRight(right, current.left, window.innerWidth) })), []);
  const resetLeft = useCallback(() => setWanted((current) => ({ ...current, left: DEFAULT_LEFT })), []);
  const resetRight = useCallback(() => setWanted((current) => ({ ...current, right: null })), []);

  const columns = `${widths.left}px ${widths.right === null ? DEFAULT_CENTER_AND_RIGHT : `minmax(0, 1fr) ${widths.right}px`}`;
  return { widths, columns, setLeft, setRight, resetLeft, resetRight };
}
