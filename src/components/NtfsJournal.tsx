import { useState } from "react";
import { call, type JournalEvidence } from "../api";
import { count, hexOffset } from "../format";
import { fromEngine, t } from "../i18n";

type Usn = JournalEvidence["usnJournal"];
type Log = JournalEvidence["logFile"];

const USN_STATE: Record<Usn["state"], () => string> = {
  absent: () => t("The volume has no change journal."),
  deleting: () => t("Windows was deleting the change journal, so no record in it is to be relied on."),
  active: () => t("The change journal is active."),
  unreadable: () => t("The change journal could not be read."),
};

const LOG_STATE: Record<Log["state"], () => string> = {
  empty: () => t("$LogFile is empty."),
  clean: () => t("$LogFile holds nothing to replay."),
  pending: () => t("$LogFile may hold transactions that Windows would redo."),
  unreadable: () => t("$LogFile could not be read."),
};

/**
 * What the volume's change journal and `$LogFile` say, as a separate interpretation next to the volume's facts. It is read
 * from the source when asked for, never written, never replayed, and never part of the files the scan finds.
 */
export function NtfsJournalPanel() {
  const [evidence, setEvidence] = useState<JournalEvidence | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function read() {
    setBusy(true);
    setError(null);
    try {
      setEvidence(await call<JournalEvidence>("ntfs_journal_evidence"));
    } catch (reason) {
      setError(String(reason));
    } finally {
      setBusy(false);
    }
  }

  async function moreUsn() {
    const cursor = evidence?.usnJournal.nextUsn;
    if (cursor === null || cursor === undefined) return;
    setBusy(true);
    setError(null);
    try {
      const next = await call<JournalEvidence>("ntfs_journal_evidence", { fromUsn: cursor, maxLogRecords: 1 });
      setEvidence((now) => now && { ...now, usnJournal: { ...next.usnJournal, records: [...now.usnJournal.records, ...next.usnJournal.records] } });
    } catch (reason) {
      setError(String(reason));
    } finally {
      setBusy(false);
    }
  }

  async function moreLog() {
    const cursor = evidence?.logFile.nextLsn;
    if (cursor === null || cursor === undefined) return;
    setBusy(true);
    setError(null);
    try {
      const next = await call<JournalEvidence>("ntfs_journal_evidence", { fromLsn: cursor, maxUsnBytes: 1 });
      setEvidence((now) => now && { ...now, logFile: { ...next.logFile, records: [...now.logFile.records, ...next.logFile.records] } });
    } catch (reason) {
      setError(String(reason));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="ntfs-journal">
      <h4>{t("Change journal and $LogFile")}</h4>
      {!evidence && (
        <>
          <p className="hint">{t("What the volume recorded about recent changes. It is read from the disk when you ask, never written or replayed, and it is not part of the files the scan finds.")}</p>
          <button onClick={read} disabled={busy}>{busy ? t("Reading") : t("Read the journal")}</button>
        </>
      )}
      {error && <p className="warn">{fromEngine(error)}</p>}
      {evidence && (
        <>
          <p className="hint">{fromEngine(evidence.interpretation)}. {fromEngine(evidence.caution)}</p>
          <UsnSection usn={evidence.usnJournal} more={moreUsn} busy={busy} />
          <LogSection log={evidence.logFile} more={moreLog} busy={busy} />
        </>
      )}
    </div>
  );
}

function UsnSection({ usn, more, busy }: { usn: Usn; more: () => void; busy: boolean }) {
  return (
    <>
      <h5>{t("Change journal")}</h5>
      <p>{USN_STATE[usn.state]()}</p>
      {usn.detail && <p className="muted">{usn.detail}</p>}
      {usn.journal && (
        <dl className="facts">
          <dt>{t("Journal id")}</dt><dd className="mono">{usn.journal.journalId}</dd>
          <dt>{t("First USN")}</dt><dd className="mono">{count(usn.journal.firstUsn)}</dd>
          <dt>{t("Next USN")}</dt><dd className="mono">{count(usn.journal.nextUsn)}</dd>
        </dl>
      )}
      {usn.records.length > 0 && (
        <table className="boot-table journal-usn">
          <thead>
            <tr><th>{t("USN")}</th><th>{t("Time")}</th><th>{t("Name")}</th><th>{t("File record")}</th><th>{t("What happened")}</th></tr>
          </thead>
          <tbody>
            {usn.records.map((record) => (
              <tr key={record.usn}>
                <td className="mono">{count(record.usn)}</td>
                <td className="mono">{record.isoTime ?? ""}</td>
                <td className="mono">{record.name}</td>
                <td className="mono">{count(record.fileSegment)} / {record.fileSequence}</td>
                <td className="mono">{record.reasons.join(", ")}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {usn.stopped && <p className="warn">{usn.stopped}</p>}
      {usn.nextUsn !== null && !usn.finished && <button onClick={more} disabled={busy}>{t("Show more records")}</button>}
      <p className="hint">{t("Read from {place}.", { place: usn.provenance })}</p>
    </>
  );
}

function LogSection({ log, more, busy }: { log: Log; more: () => void; busy: boolean }) {
  return (
    <>
      <h5>{t("$LogFile")}</h5>
      <p className={log.needsReplay ? "warn" : undefined}>{LOG_STATE[log.state]()}</p>
      <p className="muted">{log.detail}</p>
      {log.volumeOffset !== null && (
        <dl className="facts">
          <dt>{t("Starts on the volume at")}</dt><dd className="mono">{hexOffset(log.volumeOffset)}</dd>
          {log.length !== null && <><dt>{t("Length")}</dt><dd className="mono">{t("{count} bytes", { count: count(log.length) })}</dd></>}
        </dl>
      )}
      {log.records.length > 0 && (
        <table className="boot-table journal-log">
          <thead>
            <tr><th>{t("LSN")}</th><th>{t("At on the volume")}</th><th>{t("Transaction")}</th><th>{t("Redo")}</th><th>{t("Undo")}</th></tr>
          </thead>
          <tbody>
            {log.records.map((record) => (
              <tr key={record.lsn}>
                <td className="mono">{count(record.lsn)}</td>
                <td className="mono">{hexOffset(record.volumeOffset)}</td>
                <td className="mono">{record.transactionId}</td>
                <td className="mono">{hexOffset(record.redoOperation)}</td>
                <td className="mono">{hexOffset(record.undoOperation)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {log.stopped && <p className="warn">{log.stopped}</p>}
      {log.nextLsn !== null && !log.finished && <button onClick={more} disabled={busy}>{t("Show more records")}</button>}
      <p className="hint">{t("Read from {place}.", { place: log.provenance })}</p>
    </>
  );
}
