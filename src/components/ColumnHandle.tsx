import type { KeyboardEvent, PointerEvent } from "react";

const KEY_STEP = 16;

/**
 * A draggable edge between two columns. `edge` says which side of its panel it sits on:
 * the sidebar's width follows the pointer, and the right panel grows as it moves left.
 */
export function ColumnHandle({ label, edge, onWidth, onReset }: {
  label: string;
  edge: "left-panel" | "right-panel";
  onWidth: (width: number) => void;
  onReset: () => void;
}) {
  const widthAt = (x: number) => (edge === "left-panel" ? x : window.innerWidth - x);

  const start = (event: PointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) return;
    event.preventDefault();
    try {
      event.currentTarget.setPointerCapture(event.pointerId);
    } catch {
      // Dragging still works while the pointer stays over the handle.
    }
    document.body.classList.add("resizing-columns");
    event.currentTarget.dataset.dragging = "true";
  };
  const move = (event: PointerEvent<HTMLDivElement>) => {
    if (event.currentTarget.dataset.dragging === "true") onWidth(widthAt(event.clientX));
  };
  const stop = (event: PointerEvent<HTMLDivElement>) => {
    delete event.currentTarget.dataset.dragging;
    document.body.classList.remove("resizing-columns");
  };
  const onKey = (event: KeyboardEvent<HTMLDivElement>) => {
    if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
    event.preventDefault();
    const panel = event.currentTarget.parentElement?.getBoundingClientRect().width ?? 0;
    const grows = edge === "left-panel" ? event.key === "ArrowRight" : event.key === "ArrowLeft";
    onWidth(panel + (grows ? KEY_STEP : -KEY_STEP));
  };

  return (
    <div
      className={edge === "left-panel" ? "column-handle column-handle-left" : "column-handle column-handle-right"}
      role="separator"
      aria-orientation="vertical"
      aria-label={label}
      tabIndex={0}
      title="Drag to resize. Double-click to reset."
      onPointerDown={start}
      onPointerMove={move}
      onPointerUp={stop}
      onPointerCancel={stop}
      onDoubleClick={onReset}
      onKeyDown={onKey}
    />
  );
}
