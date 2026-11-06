const PATHS = {
  folder: "M3 7.5A2.5 2.5 0 0 1 5.5 5h3.6l2 2.2h7.4A2.5 2.5 0 0 1 21 9.7v7.8a2.5 2.5 0 0 1-2.5 2.5h-13A2.5 2.5 0 0 1 3 17.5z",
  file: "M7 3h7l5 5v11a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2zM14 3v5h5",
  document: "M7 3h7l5 5v11a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2zM14 3v5h5M8.5 13h7M8.5 17h7",
  film: "M5 4h14a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2zM8 4v16M16 4v16M3 9h5M3 15h5M16 9h5M16 15h5",
  music: "M9 18V6l11-2v12M9 18a3 3 0 1 1-6 0 3 3 0 0 1 6 0zM20 16a3 3 0 1 1-6 0 3 3 0 0 1 6 0z",
  image: "M5 4h14a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2zM3 16l5-5 4 4 3-3 6 6M15.5 8.5h.01",
  archive: "M4 8h16v11a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2zM3 4h18v4H3zM10 12h4",
  code: "M8 8l-4 4 4 4M16 8l4 4-4 4M13.5 5l-3 14",
  link: "M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1",
  broken: "M9 7l1-1a4 4 0 0 1 5.7 5.7M15 17l-1 1a4 4 0 0 1-5.7-5.7M4 4l16 16",
  question: "M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18zM9.5 9.5a2.5 2.5 0 1 1 3.6 2.2c-.7.4-1.1 1-1.1 1.8M12 17h.01",
  shield: "M12 3l7.5 3v5.5c0 4.6-3.1 8-7.5 9.5-4.4-1.5-7.5-4.9-7.5-9.5zM9 12l2.2 2.2L15.5 10",
  alert: "M12 4l9 16H3zM12 10v4.5M12 17.5h.01",
  search: "M11 18a7 7 0 1 0 0-14 7 7 0 0 0 0 14zM21 21l-5-5",
  chevronRight: "M9 6l6 6-6 6",
  chevronLeft: "M15 6l-6 6 6 6",
  chevronUp: "M6 15l6-6 6 6",
  chevronDown: "M6 9l6 6 6-6",
  basket: "M5 9h14l-1.4 9.2a2 2 0 0 1-2 1.8H8.4a2 2 0 0 1-2-1.8zM9 9l2.5-5M15 9l-2.5-5M3.5 9h17",
  grip: "M9 6h.01M15 6h.01M9 12h.01M15 12h.01M9 18h.01M15 18h.01",
  info: "M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18zM12 11v5M12 8h.01",
  download: "M12 4v11M7.5 11L12 15.5 16.5 11M4 20h16",
  disk: "M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18zM12 14.5a2.5 2.5 0 1 0 0-5 2.5 2.5 0 0 0 0 5z",
  bulb: "M9 18h6M10 21h4M12 3a6 6 0 0 0-3.7 10.7c.5.4.7 1 .7 1.6V16h6v-.7c0-.6.2-1.2.7-1.6A6 6 0 0 0 12 3z",
  aperture: "M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18zM12 3l4 7M21 12h-8M16.5 20l-4-7M7.5 20l4-7M3 12h8M7.5 4l4 7",
  clock: "M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18zM12 7v5l3.5 2",
  plug: "M9 3v5M15 3v5M7 8h10v3a5 5 0 0 1-10 0zM12 16v5",
  send: "M12 19V5M6 11l6-6 6 6",
  stop: "M7 7h10v10H7z",
  close: "M6 6l12 12M18 6L6 18",
  grid: "M4 4h7v7H4zM13 4h7v7h-7zM4 13h7v7H4zM13 13h7v7h-7z",
  sun: "M12 17a5 5 0 1 0 0-10 5 5 0 0 0 0 10zM12 2.5v2M12 19.5v2M4.2 4.2l1.4 1.4M18.4 18.4l1.4 1.4M2.5 12h2M19.5 12h2M4.2 19.8l1.4-1.4M18.4 5.6l1.4-1.4",
  moon: "M20.5 14.5A8.5 8.5 0 0 1 9.5 3.5a8.5 8.5 0 1 0 11 11z",
  auto: "M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18zM12 3v18M12 7.5h5.5M12 12h6.5M12 16.5h5.5",
  plus: "M12 5v14M5 12h14",
  pencil: "M4 20h4L19 9l-4-4L4 16zM13.5 6.5l4 4",
  puzzle: "M10 4h4v3a2 2 0 1 0 4 0V4h2v6h-3a2 2 0 1 0 0 4h3v6h-6v-3a2 2 0 1 0-4 0v3H4v-6h3a2 2 0 1 0 0-4H4V4z",
  home: "M4 10.5L12 4l8 6.5M6.5 9v11h4v-5.5h3V20h4V9",
  // A dump truck, for building: the Rebuild tab.
  truck: "M2 7.5h10.5l1 6.5H4zM13.5 9H17l3.5 4V16h-7zM15 10.5h1.6l1.8 2h-3.4zM3 16h2M9 16h7M9 17.5a2 2 0 1 1-4 0 2 2 0 0 1 4 0zM20 17.5a2 2 0 1 1-4 0 2 2 0 0 1 4 0z",
  // A generic robot head, standing for an agent.
  robot: "M7 8h10a3 3 0 0 1 3 3v6a3 3 0 0 1-3 3H7a3 3 0 0 1-3-3v-6a3 3 0 0 1 3-3zM12 8V4.5M12 3.5h.01M9.5 13h.01M14.5 13h.01M10 16.5h4M2 13v2.5M22 13v2.5",
  sparkle: "M12 3l1.8 5.2L19 10l-5.2 1.8L12 17l-1.8-5.2L5 10l5.2-1.8zM19 16l.8 2.2L22 19l-2.2.8L19 22l-.8-2.2L16 19l2.2-.8z",
} as const;

export type IconName = keyof typeof PATHS;

export function Icon({ name, size = 16, className }: { name: IconName; size?: number; className?: string }) {
  return (
    <svg
      className={className}
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.7}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <path d={PATHS[name]} />
    </svg>
  );
}

const EXTENSION_ICONS: Record<string, IconName> = {
  mp4: "film", m4v: "film", mov: "film", avi: "film", mkv: "film", webm: "film", wmv: "film", mpg: "film", mpeg: "film", "3gp": "film",
  mp3: "music", wav: "music", flac: "music", ogg: "music", m4a: "music", aac: "music", wma: "music", mid: "music", midi: "music", aif: "music", aiff: "music",
  jpg: "image", jpeg: "image", png: "image", gif: "image", bmp: "image", webp: "image", tif: "image", tiff: "image", heic: "image", svg: "image",
  zip: "archive", rar: "archive", gz: "archive", tgz: "archive", "7z": "archive", tar: "archive", iso: "archive", dmg: "archive", jar: "archive",
  js: "code", ts: "code", jsx: "code", tsx: "code", php: "code", py: "code", rb: "code", c: "code", h: "code", cpp: "code", java: "code", go: "code", rs: "code", sh: "code", css: "code", html: "code", htm: "code", json: "code", xml: "code", yml: "code", yaml: "code", sql: "code",
  pdf: "document", doc: "document", docx: "document", xls: "document", xlsx: "document", ppt: "document", pptx: "document", txt: "document", md: "document", rtf: "document",
};

export function fileIcon(name: string): IconName {
  const extension = name.includes(".") ? name.split(".").pop()?.toLowerCase() ?? "" : "";
  return EXTENSION_ICONS[extension] ?? "file";
}

const VERDICT_TONES: Record<string, string> = {
  live: "tone-neutral",
  "tested-good": "tone-good",
  "tested-bad": "tone-critical",
  "likely-intact": "tone-good",
  plausible: "tone-info",
  partial: "tone-warning",
  "damaged-entry": "tone-serious",
  overwritten: "tone-critical",
  "folder-missing": "tone-critical",
};

export function verdictTone(verdict: string): string {
  return VERDICT_TONES[verdict] ?? "tone-neutral";
}

const RING_RADIUS = 6;
const RING_LENGTH = 2 * Math.PI * RING_RADIUS;

/** A small ring whose filled share is a confidence score. */
export function ConfidenceRing({ value, tone, label }: { value: number; tone: string; label: string }) {
  return (
    <svg className={`ring ${tone}`} width={16} height={16} viewBox="0 0 16 16" role="img" aria-label={label}>
      <title>{label}</title>
      <circle className="ring-track" cx={8} cy={8} r={RING_RADIUS} />
      <circle
        className="ring-value"
        cx={8}
        cy={8}
        r={RING_RADIUS}
        strokeDasharray={`${Math.max(value, 0.02) * RING_LENGTH} ${RING_LENGTH}`}
        transform="rotate(-90 8 8)"
      />
    </svg>
  );
}
