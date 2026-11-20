import { useMemo } from "react";
import { marks, type BadKind, type BadRange } from "../badSectors";
import { count, bytes as formatBytes } from "../format";
import { locale, t } from "../i18n";

/** What a bad range is called everywhere, in the legend and in tooltips. */
export function badSectorsLabel(): string {
  return t("Bad sectors: reading here drops the drive");
}

/** How it was found, in a sentence. */
export function badKindText(kind: BadKind): string {
  switch (kind) {
    case "reset": return t("The drive reset during a read here.");
    case "failed": return t("A read here failed.");
    case "suspect": return t("Inside a band the probe jumped over, so it was not read.");
  }
}

/** When it was found, as the person's own settings write a date and a time. */
export function badFoundText(range: BadRange): string | null {
  if (!range.foundMillis) return null;
  return t("Found {when}", { when: new Date(range.foundMillis).toLocaleString(locale(), { dateStyle: "medium", timeStyle: "short" }) });
}

/** The flashing red marks over a strip: one for each bad stretch of what the strip shows. */
export function BadMarks({ bad, start, end }: { bad: BadRange[]; start: number; end: number }) {
  const shown = useMemo(() => marks(bad, start, end), [bad, start, end]);
  if (shown.length === 0) return null;
  return (
    <div className="strip-bad" aria-hidden="true">
      {shown.map((mark, index) => <span key={index} className="bad-range" style={{ left: `${mark.left}%`, width: `${mark.width}%` }} />)}
    </div>
  );
}

/** The legend entry: its swatch flashes like the marks do, and is hatched when motion is not wanted. */
export function BadLegendItem() {
  return <span className="legend-item" title={t("Ranges the drive dropped out on, or that failed to read, or that a probe jumped over. The app does not read them, to spare the drive.")}>
    <span className="legend-swatch bad-swatch" />{badSectorsLabel()}
  </span>;
}

/** What a tooltip adds when the pointer is on bad sectors: what they are, where, how found and when. */
export function BadTip({ ranges, unit }: { ranges: BadRange[]; unit: "bytes" | "clusters" }) {
  return <>
    {ranges.slice(0, 3).map((range) => {
      const found = badFoundText(range);
      return <div key={`${range.start}:${range.end}`} className="strip-tip-bad">
        <div className="strip-tip-row"><span className="legend-swatch bad-swatch" /><strong>{badSectorsLabel()}</strong></div>
        <div className="strip-tip-sub">{unit === "bytes"
          ? t("{from} to {to}", { from: formatBytes(range.start), to: formatBytes(range.end) })
          : t("Clusters {from} to {to}", { from: count(range.start), to: count(range.end - 1) })}</div>
        <div>{badKindText(range.kind)}{found ? ` ${found}.` : ""}</div>
      </div>;
    })}
    {ranges.length > 3 && <div className="strip-tip-sub">{t("and {count} more", { count: count(ranges.length - 3) })}</div>}
  </>;
}
