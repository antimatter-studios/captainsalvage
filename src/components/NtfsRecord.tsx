import { useEffect, useState } from "react";
import { call, type NtfsRecord } from "../api";
import { count } from "../format";
import { fromEngine, t } from "../i18n";

/** What the entry's FILE record says: every name, every stream, and what was wrong with it. */
export function NtfsRecordPanel({ id }: { id: number }) {
  const [record, setRecord] = useState<NtfsRecord | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let current = true;
    setRecord(null);
    setError(null);
    call<NtfsRecord>("ntfs_entry", { id })
      .then((value) => current && setRecord(value))
      .catch((reason) => current && setError(String(reason)));
    return () => {
      current = false;
    };
  }, [id]);
  if (error) return <p className="warn">{fromEngine(error)}</p>;
  if (!record) return null;
  return (
    <>
      <h4>{t("NTFS record")}</h4>
      <dl className="facts">
        {record.recordNumber !== null && <><dt>{t("Record number")}</dt><dd className="mono">{count(record.recordNumber)}</dd></>}
        <dt>{t("Record sequence")}</dt><dd className="mono">{record.sequence}</dd>
        {record.hasAttributeList && <><dt>{t("Attribute list")}</dt><dd>{t("Named data streams in other records are followed; other attributes may not be.")}</dd></>}
      </dl>
      {record.issues.length > 0 && (
        <>
          <h4>{t("Damage found in the record")}</h4>
          {record.issues.map((issue, index) => <p key={index} className="warn">{fromEngine(issue)}</p>)}
        </>
      )}
      <h4>{t("Names")}</h4>
      {record.names.map((name, index) => (
        <div key={index} className="ntfs-name">
          <span className="mono">{name.name}</span>{" "}
          <span className="muted">{t("{namespace}, linked in folder record {record}", { namespace: name.namespace, record: count(name.parentRecord) })}</span>
        </div>
      ))}
      <h4>{t("Data streams")}</h4>
      {record.streams.map((stream, index) => (
        <div key={index} className="ntfs-stream">
          <span className="mono">{stream.name === "" ? t("(the file's own data)") : stream.name}</span>{" "}
          <span className="muted">
            {stream.resident ? t("inside the record") : t("{count} runs", { count: count(stream.runs.length) })}
            {stream.compressed ? ", " + t("compressed") : ""}
            {stream.encrypted ? ", " + t("encrypted") : ""}
            {stream.sparse ? ", " + t("sparse") : ""}
            {", " + t("{count} bytes", { count: count(stream.length) })}
          </span>
        </div>
      ))}
    </>
  );
}
