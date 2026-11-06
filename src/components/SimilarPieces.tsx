import { useEffect, useState } from "react";
import { call, type Row } from "../api";
import { count, lostFolderName, percent } from "../format";
import { fromEngine, t } from "../i18n";
import { Icon } from "./icons";

interface SimilarPiece {
  piece: Row;
  shared: number;
  covers: number;
  coveredBy: number;
  quality: number;
  ownQuality: number;
}

interface Similar {
  piece: Row;
  files: number;
  similar: SimilarPiece[];
}

function pieceName(row: Row): string {
  return row.name || (row.nodeKind === "root" ? t("Volume root") : lostFolderName(row.head ?? row.firstCluster));
}

/**
 * Other pieces holding many of the same files as this one, found on request because it reads
 * every file entry. Each can be opened, or swapped in where a canvas offers that.
 */
export function SimilarPieces({ head, onOpen, onSwap, used }: {
  head: number;
  onOpen?: (row: Row) => void;
  onSwap?: (row: Row) => void;
  used?: Set<number>;
}) {
  const [result, setResult] = useState<Similar | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let current = true;
    setResult(null);
    setError(null);
    call<Similar>("similar_pieces", { head })
      .then((found) => current && setResult(found))
      .catch((reason) => current && setError(String(reason)));
    return () => {
      current = false;
    };
  }, [head]);

  if (error) return <p className="warn">{fromEngine(error.replace(/^Error: /, ""))}</p>;
  if (!result) return <p className="muted similar-wait">{t("Comparing this piece with every other one.")}</p>;
  if (result.similar.length === 0) return <p className="muted">{t("No other piece holds many of the same files.")}</p>;
  return (
    <div className="similar-list">
      {result.similar.map((item) => {
        const head = item.piece.head ?? item.piece.firstCluster;
        const better = item.quality > item.ownQuality + 0.01;
        return (
          <div key={head} className="similar-row">
            <Icon name="folder" />
            <span className="similar-main">
              <button className="link similar-name" onClick={() => onOpen?.(item.piece)} disabled={!onOpen}>{pieceName(item.piece)}</button>
              <span className="muted">
                {item.shared === 1
                  ? t("One file in common, {covers} of this one and {coveredBy} of that one", { covers: percent(item.covers), coveredBy: percent(item.coveredBy) })
                  : t("{shared} files in common, {covers} of this one and {coveredBy} of that one", {
                      shared: count(item.shared),
                      covers: percent(item.covers),
                      coveredBy: percent(item.coveredBy),
                    })}
              </span>
              <span className={better ? "tone-good" : "muted"}>
                {better
                  ? t("Better data: {quality} likely intact, against {own} here", { quality: percent(item.quality), own: percent(item.ownQuality) })
                  : t("{quality} likely intact, against {own} here", { quality: percent(item.quality), own: percent(item.ownQuality) })}
              </span>
            </span>
            {onSwap && (
              used?.has(head)
                ? <span className="muted">{t("On the canvas")}</span>
                : <button onClick={() => onSwap(item.piece)}>{t("Swap in")}</button>
            )}
          </div>
        );
      })}
    </div>
  );
}
