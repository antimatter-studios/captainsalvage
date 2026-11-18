import { useEffect, useRef } from "react";

export interface MenuItem {
  label: string;
  onChoose: () => void;
}

/** What a right-click on a file or a folder is about. */
export interface ExportItem {
  kind: "file" | "folder";
  /** A file's entry id, or a folder's head cluster. */
  target: number;
  name: string;
}

/** Called when the person right-clicks a file or a folder. */
export type ExportHandler = (event: { preventDefault: () => void; clientX: number; clientY: number }, item: ExportItem) => void;

/** A small menu where the person right-clicked. It closes on any click elsewhere, a scroll or Escape. */
export function ContextMenu({ x, y, items, onClose }: { x: number; y: number; items: MenuItem[]; onClose: () => void }) {
  const menu = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const close = () => onClose();
    const key = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("mousedown", close);
    window.addEventListener("scroll", close, true);
    window.addEventListener("blur", close);
    window.addEventListener("resize", close);
    window.addEventListener("keydown", key);
    menu.current?.querySelector<HTMLButtonElement>("button")?.focus();
    return () => {
      window.removeEventListener("mousedown", close);
      window.removeEventListener("scroll", close, true);
      window.removeEventListener("blur", close);
      window.removeEventListener("resize", close);
      window.removeEventListener("keydown", key);
    };
  }, [onClose]);

  // Stay inside the window.
  const left = Math.max(4, Math.min(x, window.innerWidth - 200));
  const top = Math.max(4, Math.min(y, window.innerHeight - 8 - items.length * 30));
  return (
    <div
      ref={menu}
      className="context-menu"
      role="menu"
      style={{ left, top }}
      onMouseDown={(event) => event.stopPropagation()}
      onContextMenu={(event) => event.preventDefault()}
    >
      {items.map((item) => (
        <button
          key={item.label}
          role="menuitem"
          className="context-menu-item"
          onClick={() => {
            onClose();
            item.onChoose();
          }}
        >
          {item.label}
        </button>
      ))}
    </div>
  );
}
