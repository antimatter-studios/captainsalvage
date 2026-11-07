import { useEffect, useState } from "react";
import { call, type Extent, type FileTestStatus, type MountHistory, type Overview, type Status } from "../api";
import { bytes, count, percent } from "../format";
import { useSimple } from "../mode";
import { fromEngine, locale, t } from "../i18n";
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
function MacosWrites({ revision, onShowClusters }: { revision: number; onShowClusters: (extents: Extent[]) => void }) {
  const [history, setHistory] = useState<MountHistory | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  // The map is hidden in Simple mode, so there is nothing to show the writes on.
  const simple = useSimple();
  const show = (date: string) => {
    setProblem(null);
    call<{ dates: { date: string; extents: Extent[] }[] }>("macos_writes")
      .then((value) => onShowClusters(value.dates.find((day) => day.date === date)?.extents ?? []))
      .catch((reason) => setProblem(String(reason)));
  };
  useEffect(() => {
    let current = true;
    call<MountHistory>("mount_history").then((value) => current && setHistory(value)).catch(() => undefined);
    return () => {
      current = false;
    };
  }, [revision]);
  if (!history || history.dates.length === 0) return null;
  const baseline = new Date(history.baselineRecordedAtMillis).toLocaleDateString(locale());
  return (
    <div className="recovery-tests">
      <h3>{t("When macOS wrote to this volume")}</h3>
      <p className="recovery-note recovery-note-tight">
        {t("macOS keeps these folders up to date while a volume is mounted. A date after the damage means those writes may have landed on lost files.")}
      </p>
      <table className="recovery-table">
        <tbody>
          {history.dates.map((day) => (
            <tr key={day.date}>
              <td>{day.date}</td>
              <td className="recovery-number">{day.entries === 1 ? t("1 entry") : t("{count} entries", { count: count(day.entries) })}</td>
              <td className="recovery-number">{bytes(day.bytes)}</td>
              <td className="recovery-number">{day.clusters === 1 ? t("1 cluster") : t("{count} clusters", { count: count(day.clusters) })}</td>
              {!simple && (
                <td className="recovery-number">
                  <button className="link" onClick={() => show(day.date)}>{t("Show on the map")}</button>
                </td>
              )}
            </tr>
          ))}
        </tbody>
      </table>
      {problem && <p className="recovery-note bad">{problem}</p>}
      <p className="recovery-note recovery-note-after">
        {t("The safety baseline cannot see any of these from before {date}, when it was recorded.", { date: baseline })}{" "}
        {history.dirtyFlag
          ? t("The dirty flag is set, so the last mount did not end cleanly.")
          : t("The dirty flag is clear, which says only that the last mount ended cleanly, and not that the volume is undamaged.")}
      </p>
    </div>
  );
}

interface TypeTestTotals {
  extension: string;
  group: string;
  files: number;
  bytes: number;
  passed: number;
  damaged: number;
  noTest: number;
  notTestedYet: number;
}

/** The most types listed at first, largest by bytes. */
const TYPES_SHOWN = 8;

/** How the tests went for each file type, largest first. */
function TestsByType({ done }: { done: number }) {
  const [types, setTypes] = useState<TypeTestTotals[]>([]);
  const [all, setAll] = useState(false);
  useEffect(() => {
    let current = true;
    call<{ types: TypeTestTotals[] }>("file_tests_by_type")
      .then((value) => current && setTypes(value.types))
      .catch(() => undefined);
    return () => {
      current = false;
    };
  }, [done]);
  if (types.length === 0) return null;
  const shown = all ? types : types.slice(0, TYPES_SHOWN);
  return (
    <>
      <table className="recovery-table recovery-types">
        <thead>
          <tr>
            <th>{t("Type")}</th>
            <th className="numeric">{t("Files")}</th>
            <th className="numeric tone-good">{t("Passed")}</th>
            <th className="numeric tone-critical">{t("Damaged")}</th>
            <th className="numeric muted">{t("Untested")}</th>
          </tr>
        </thead>
        <tbody>
          {shown.map((type) => (
            <tr key={type.extension}>
              <td>{type.extension ? `.${type.extension}` : t("No extension")}</td>
              <td className="numeric">{count(type.files)}</td>
              <td className="numeric">{count(type.passed)}</td>
              <td className="numeric">{count(type.damaged)}</td>
              <td className="numeric muted">{count(type.noTest + type.notTestedYet)}</td>
            </tr>
          ))}
        </tbody>
      </table>
      {types.length > TYPES_SHOWN && (
        <button className="link" onClick={() => setAll(!all)}>
          {all ? t("Show fewer types") : t("Show all {count} types", { count: count(types.length) })}
        </button>
      )}
    </>
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
      <h3>{t("File tests")}</h3>
      {tested > 0 ? (
        <table className="recovery-table">
          <tbody>
            <tr>
              <td className="tone-good">{t("Passed")}</td>
              <td className="numeric">{count(tests.passed)}</td>
            </tr>
            <tr>
              <td className="tone-critical">{t("Damaged")}</td>
              <td className="numeric">{count(tests.damaged)}</td>
            </tr>
            {tests.untested > 0 && (
              <tr>
                <td className="muted">{t("No test for this type yet")}</td>
                <td className="numeric muted">{count(tests.untested)}</td>
              </tr>
            )}
          </tbody>
        </table>
      ) : (
        <p className="muted">{t("Scores are estimates until a file's real bytes are tested. This reads each file once, at low priority, and writes nothing to the disk. You can pause at any time.")}</p>
      )}
      {tested > 0 && <TestsByType done={tests.done} />}
      {running && (
        <div className="copy-progress">
          <div className="scan-bar" role="progressbar" aria-valuenow={tests.total ? Math.round((tests.done / tests.total) * 100) : 0} aria-valuemin={0} aria-valuemax={100}>
            <div className="scan-fill" style={{ width: `${tests.total ? (tests.done / tests.total) * 100 : 0}%` }} />
          </div>
          <div className="muted">{t("Testing {done} of {total}, {bytes} read", { done: count(tests.done), total: count(tests.total), bytes: bytes(tests.bytesDone) })}</div>
        </div>
      )}
      <div className="detail-actions">
        {running ? (
          <button onClick={() => call("file_tests_stop").then(onChanged)}>{t("Pause")}</button>
        ) : (
          <>
            <button className="primary" onClick={() => start(["photos", "documents"])}>{t("Test photos and documents")}</button>
            <button onClick={() => start(["music", "archives"])}>{t("Test music and archives")}</button>
          </>
        )}
      </div>
      {tests.error && <p className="warn">{fromEngine(tests.error)}</p>}
      {error && <p className="warn">{fromEngine(error)}</p>}
    </div>
  );
}

/** The first thing on screen: what has been found so far, before any selection. */
const FOUND_VERDICTS = ["tested-good", "likely-intact", "plausible", "live"];

export function RecoveryOverview({ overview, status, revision, onChanged, onShowClusters }: { overview: Overview; status: Status | null; revision: number; onChanged: () => void; onShowClusters: (extents: Extent[]) => void }) {
  // Simple mode hides the map, so its hint is left out.
  const simple = useSimple();
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
  // The same verdicts the start screen counts: a file still visible on the volume is found too.
  const named = verdicts.filter((item) => FOUND_VERDICTS.includes(item.verdict));
  const namedBytes = named.reduce((total, item) => total + item.bytes, 0);
  const namedFiles = named.reduce((total, item) => total + item.count, 0);
  const scanned = status ? status.scan.clustersScanned / status.scan.totalClusters : 0;
  const complete = status?.scan.status === "complete";

  const colors = new Map(legendFor("ownership").map((item) => [item.value, item.color]));
  const segments = stats
    ? [
        { label: t("Files and folders found by name"), clusters: stats.ownership.claimedByOneEntry + stats.ownership.directory, color: colors.get(2) },
        { label: t("Claimed by several entries"), clusters: stats.ownership.claimedBySeveralEntries, color: colors.get(3) },
        { label: t("FAT chains with no name yet"), clusters: stats.ownership.namelessChain, color: colors.get(5) },
        { label: t("Explained by a hypothesis"), clusters: stats.ownership.explainedByHypothesis, color: colors.get(6) },
        { label: t("Nothing explains it yet"), clusters: stats.ownership.unexplained, color: colors.get(1) },
      ].filter((segment) => segment.clusters > 0)
    : [];
  const total = segments.reduce((sum, segment) => sum + segment.clusters, 0) || 1;

  return (
    <section className="center-panel recovery" aria-label={t("Recovery overview")}>
      <div className="recovery-inner">
        <div className="recovery-headline">{status?.summary ? bytes(namedBytes) : t("Building the map")}</div>
        <p className="recovery-sub">
          {status?.summary
            ? complete
              ? t("found by name so far, in {count} files.", { count: count(namedFiles) })
              : t("found by name so far, in {count} files, with {percent} of the disk scanned.", { count: count(namedFiles), percent: percent(scanned) })
            : t("Files appear here once the scan has found their folders.")}
        </p>
        <p className="recovery-note">{t("These are estimates until a file is tested or opened. Add what matters to your treasure list to get it to safety.")}</p>

        {segments.length > 0 && (
          <>
            <div className="recovery-bar" role="img" aria-label={t("What explains the space on this volume")}>
              {segments.map((segment) => (
                <span
                  key={segment.label}
                  title={t("{label}: {bytes}", { label: segment.label, bytes: bytes(segment.clusters * clusterSize) })}
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

        <MacosWrites revision={revision} onShowClusters={onShowClusters} />

        <KeepSafe device={status?.diskBack?.device ?? overview.device} />

        <ul className="recovery-steps">
          <li><Icon name="folder" /> {t("Open a lost folder in the sidebar to see what survived.")}</li>
          {!simple && <li><Icon name="aperture" /> {t("Click the map to read the bytes at any point on the disk.")}</li>}
          <li><Icon name="sparkle" /> {t("Select anything, then chat with an agent about what it is and where it leads.")}</li>
        </ul>
      </div>
    </section>
  );
}
