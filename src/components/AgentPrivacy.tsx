import { useEffect, useState } from "react";
import { call } from "../api";
import { t } from "../i18n";

/**
 * The person's choice of what agents see. Off, an agent sees names and previews like the window
 * does. On, it works from counts, sizes, types and verdicts only.
 */
export function AgentPrivacy() {
  const [hidden, setHidden] = useState<boolean | null>(null);
  useEffect(() => {
    call<{ namesHidden: boolean }>("agent_privacy").then((value) => setHidden(value.namesHidden)).catch(() => undefined);
  }, []);
  if (hidden === null) return null;
  const change = (next: boolean) => {
    call<{ namesHidden: boolean }>("set_agent_privacy", { namesHidden: next })
      .then((value) => setHidden(value.namesHidden))
      .catch(() => undefined);
  };
  return (
    <label className="check agent-privacy" title={t("Agents still see counts, sizes, types and verdicts. A script can still read raw bytes, so this prevents accidents, not a determined agent.")}>
      <input type="checkbox" checked={hidden} onChange={(event) => change(event.target.checked)} />
      {t("Keep file names and previews from agents")}
    </label>
  );
}
