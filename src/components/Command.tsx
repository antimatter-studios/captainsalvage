import { useState } from "react";

const COPIED_MESSAGE_MS = 1500;

/** A command for the person's own terminal, set as code, with a button that copies it. */
export function Command({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  const copy = () => {
    void navigator.clipboard.writeText(text).then(() => {
      setCopied(true);
      window.setTimeout(() => setCopied(false), COPIED_MESSAGE_MS);
    });
  };
  return (
    <div className="command">
      <code>{text}</code>
      <button onClick={copy}>{copied ? "Copied" : "Copy"}</button>
    </div>
  );
}
