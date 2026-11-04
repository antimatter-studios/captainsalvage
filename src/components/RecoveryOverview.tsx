import { useEffect, useState } from "react";
import { call, type Overview, type Status } from "../api";
import { bytes, count, percent } from "../format";
import { legendFor } from "../palette";
import { Icon } from "./icons";

interface RegionStats {
  ownership: {
    unexplained: number;
    claimedByOneEntry: number;
    claimedBySeveralEntries: number;
    directory: number;
    namelessChain: number;
    explainedByHypothesis: number;
  };
}

/** The first thing on screen: what has been found so far, before any selection. */
export function RecoveryOverview({ overview, status, revision }: { overview: Overview; status: Status | null; revision: number }) {
  const [stats, setStats] = useState<RegionStats | null>(null);
  useEffect(() => {
    let current = true;
    call<RegionStats>("region_stats").then((value) => current && setStats(value)).catch(() => undefined);
    return () => {
      current = false;
    };
  }, [revision]);

  const clusterSize = overview.geometry.clusterSize;
  const verdicts = status?.summary?.verdicts ?? [];
  const named = verdicts.filter((item) => item.verdict === "likely-intact" || item.verdict === "plausible");
  const namedBytes = named.reduce((total, item) => total + item.bytes, 0);
  const namedFiles = named.reduce((total, item) => total + item.count, 0);
  const scanned = status ? status.scan.clustersScanned / status.scan.totalClusters : 0;
  const complete = status?.scan.status === "complete";

  const colors = new Map(legendFor("ownership").map((item) => [item.value, item.color]));
  const segments = stats
    ? [
        { label: "Files and folders found by name", clusters: stats.ownership.claimedByOneEntry + stats.ownership.directory, color: colors.get(2) },
        { label: "Claimed by several entries", clusters: stats.ownership.claimedBySeveralEntries, color: colors.get(3) },
        { label: "FAT chains with no name yet", clusters: stats.ownership.namelessChain, color: colors.get(5) },
        { label: "Explained by a hypothesis", clusters: stats.ownership.explainedByHypothesis, color: colors.get(6) },
        { label: "Nothing explains it yet", clusters: stats.ownership.unexplained, color: colors.get(1) },
      ].filter((segment) => segment.clusters > 0)
    : [];
  const total = segments.reduce((sum, segment) => sum + segment.clusters, 0) || 1;

  return (
    <section className="center-panel recovery" aria-label="Recovery overview">
      <div className="recovery-inner">
        <div className="recovery-headline">{status?.summary ? bytes(namedBytes) : "Building the map"}</div>
        <p className="recovery-sub">
          {status?.summary
            ? `found by name so far, in ${count(namedFiles)} files${complete ? "." : `, with ${percent(scanned)} of the disk scanned.`}`
            : "Files appear here once the scan has found their folders."}
        </p>
        <p className="recovery-note">These are estimates until a file is tested or opened. Use Copy out to get what matters to safety.</p>

        {segments.length > 0 && (
          <>
            <div className="recovery-bar" role="img" aria-label="What explains the space on this volume">
              {segments.map((segment) => (
                <span
                  key={segment.label}
                  title={`${segment.label}: ${bytes(segment.clusters * clusterSize)}`}
                  style={{ flexGrow: segment.clusters, background: segment.color }}
                />
              ))}
            </div>
            <table className="recovery-table">
              <tbody>
                {segments.map((segment) => (
                  <tr key={segment.label}>
                    <td><span className="legend-swatch" style={{ background: segment.color }} /></td>
                    <td>{segment.label}</td>
                    <td className="numeric">{bytes(segment.clusters * clusterSize)}</td>
                    <td className="numeric muted">{percent(segment.clusters / total)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </>
        )}

        <ul className="recovery-steps">
          <li><Icon name="folder" /> Open a lost folder in the sidebar to see what survived.</li>
          <li><Icon name="aperture" /> Click the map to read the bytes at any point on the disk.</li>
          <li><Icon name="sparkle" /> Select anything, then ask Claude what it is and where it leads.</li>
        </ul>
      </div>
    </section>
  );
}
