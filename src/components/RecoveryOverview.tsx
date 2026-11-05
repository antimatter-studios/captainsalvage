import { useEffect, useState } from "react";
import { call, type FileTestStatus, type MountHistory, type Overview, type Status } from "../api";
import { bytes, count, percent } from "../format";
import { legendFor } from "../palette";
import { Icon } from "./icons";
import { KeepSafe } from "./KeepSafe";

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

/** Starts, pauses and reports the background file tests. */
/** The dates the volume's own system folders changed, which only a finished scan can show. */
function MacosWrites({ revision }: { revision: number }) {
  const [history, setHistory] = useState<MountHistory | null>(null);
  useEffect(() => {
    let current = true;
    call<MountHistory>("mount_history").then((value) => current && setHistory(value)).catch(() => undefined);
    return () => {
      current = false;
    };
  }, [revision]);
  if (!history || history.dates.length === 0) return null;
  const baseline = new Date(history.baselineRecordedAtMillis).toLocaleDateString();
  return (
    <div className="recovery-tests">
      <h3>When macOS wrote to this volume</h3>
      <p className="recovery-note recovery-note-tight">
        macOS keeps these folders up to date while a volume is mounted. A date after the damage means those writes may have landed on lost files.
      </p>
      <table className="recovery-table">
        <tbody>
          {history.dates.map((day) => (
            <tr key={day.date}>
              <td>{day.date}</td>
              <td className="recovery-number">{count(day.entries)} {day.entries === 1 ? "entry" : "entries"}</td>
              <td className="recovery-number">{bytes(day.bytes)}</td>
              <td className="recovery-number">{count(day.clusters)} {day.clusters === 1 ? "cluster" : "clusters"}</td>
            </tr>
          ))}
        </tbody>
      </table>
      <p className="recovery-note recovery-note-after">
        The safety baseline cannot see any of these from before {baseline}, when it was recorded. The dirty flag is {history.dirtyFlag ? "set, so the last mount did not end cleanly" : "clear, which says only that the last mount ended cleanly, and not that the volume is undamaged"}.
      </p>
    </div>
  );
}

function FileTests({ tests, onChanged }: { tests: FileTestStatus; onChanged: () => void }) {
  const [error, setError] = useState<string | null>(null);
  const running = tests.state === "running";
  const start = (groups: string[]) => {
    setError(null);
    call("file_tests_start", { groups }).then(onChanged).catch((reason) => setError(String(reason)));
  };
  const tested = tests.passed + tests.damaged;
  return (
    <div className="recovery-tests">
      <h3>File tests</h3>
      {tested > 0 ? (
        <table className="recovery-table">
          <tbody>
            <tr>
              <td className="tone-good">Passed</td>
              <td className="numeric">{count(tests.passed)}</td>
            </tr>
            <tr>
              <td className="tone-critical">Damaged</td>
              <td className="numeric">{count(tests.damaged)}</td>
            </tr>
            {tests.untested > 0 && (
              <tr>
                <td className="muted">No test for this type yet</td>
                <td className="numeric muted">{count(tests.untested)}</td>
              </tr>
            )}
          </tbody>
        </table>
      ) : (
        <p className="muted">Scores are estimates until a file's real bytes are tested. This reads each file once, at low priority, and writes nothing to the disk. You can pause at any time.</p>
      )}
      {running && (
        <div className="copy-progress">
          <div className="scan-bar" role="progressbar" aria-valuenow={tests.total ? Math.round((tests.done / tests.total) * 100) : 0} aria-valuemin={0} aria-valuemax={100}>
            <div className="scan-fill" style={{ width: `${tests.total ? (tests.done / tests.total) * 100 : 0}%` }} />
          </div>
          <div className="muted">Testing {count(tests.done)} of {count(tests.total)}, {bytes(tests.bytesDone)} read</div>
        </div>
      )}
      <div className="detail-actions">
        {running ? (
          <button onClick={() => call("file_tests_stop").then(onChanged)}>Pause</button>
        ) : (
          <>
            <button className="primary" onClick={() => start(["photos", "documents"])}>Test photos and documents</button>
            <button onClick={() => start(["music", "archives"])}>Test music and archives</button>
          </>
        )}
      </div>
      {tests.error && <p className="warn">{tests.error}</p>}
      {error && <p className="warn">{error}</p>}
    </div>
  );
}

/** The first thing on screen: what has been found so far, before any selection. */
export function RecoveryOverview({ overview, status, revision, onChanged }: { overview: Overview; status: Status | null; revision: number; onChanged: () => void }) {
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
  const named = verdicts.filter((item) => item.verdict === "tested-good" || item.verdict === "likely-intact" || item.verdict === "plausible");
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
        <p className="recovery-note">These are estimates until a file is tested or opened. Add what matters to your basket to get it to safety.</p>

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

        {status && <FileTests tests={status.fileTests} onChanged={onChanged} />}

        <MacosWrites revision={revision} />

        <KeepSafe device={status?.diskBack?.device ?? overview.device} />

        <ul className="recovery-steps">
          <li><Icon name="folder" /> Open a lost folder in the sidebar to see what survived.</li>
          <li><Icon name="aperture" /> Click the map to read the bytes at any point on the disk.</li>
          <li><Icon name="sparkle" /> Select anything, then ask Claude what it is and where it leads.</li>
        </ul>
      </div>
    </section>
  );
}
