import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useRef, useState } from "react";
import { call, type Selection } from "../api";
import { count } from "../format";
import { fromEngine, questionFor, t, useLanguage, type Language } from "../i18n";
import { Command } from "./Command";
import { Icon } from "./icons";

interface ToolCall {
  name: string;
  input: string;
}

interface Message {
  role: "you" | "claude" | "error";
  text: string;
  tools: ToolCall[];
  about?: string;
}

interface AgentInfo {
  id: string;
  name: string;
  installed: boolean;
  install: string;
}

interface Model {
  id: string;
  name: string;
}

/** What any agent said or did, already read from its own output by the app. */
type AgentEvent =
  | { type: "session"; id: string }
  | { type: "text"; text: string }
  | { type: "tool"; name: string; input: string }
  | { type: "error"; text: string }
  | { type: "end"; code: number | null; stderr: string };

const AGENT_SETTING = "agent";

export function describeSelection(selection: Selection): string {
  switch (selection.kind) {
    case "cluster":
      if (selection.byteStart !== undefined && selection.byteEnd !== undefined) {
        return `Cluster ${count(selection.cluster)}, bytes ${count(selection.byteStart)} to ${count(selection.byteEnd - 1)}`;
      }
      return `Cluster ${count(selection.cluster)}`;
    case "entry":
      return selection.name;
    case "folder":
      return selection.name || t("Folder at cluster {cluster}", { cluster: count(selection.head) });
    case "chain":
      return t("Nameless chain at cluster {cluster}", { cluster: count(selection.head) });
    case "region":
      return t("Unexplained region, clusters {start} to {end}", { start: count(selection.startCluster), end: count(selection.endCluster) });
    default:
      return t("Nothing selected");
  }
}

/** Questions worth asking about what is selected, offered before the person types. */
function suggestionsFor(selection: Selection): string[] {
  switch (selection.kind) {
    case "cluster":
      return [t("What is in this cluster?"), t("Which file does this cluster belong to?")];
    case "entry":
      return [t("Is this file whole?"), t("Where are the rest of its clusters?"), t("Can it be put back together?")];
    case "folder":
      return [t("What did this folder hold?"), t("Where did this folder belong?")];
    case "chain":
      return [t("Which file did this chain belong to?"), t("Where does this chain continue?")];
    case "region":
      return [t("What is in this region?"), t("Could this be part of a lost file?")];
    default:
      return [t("What can be recovered from this disk?"), t("Where are the biggest lost pieces?")];
  }
}

/** "cluster 1,234" or "clusters 52 to 60", in English or with the window's own separators. */
const CLUSTER_MENTION = /(\bclusters?\s+)(\d{1,3}(?:[,.\u00a0\u202f ]\d{3})+(?!\d)|\d+)/gi;

/** Plain text, with every cluster number it mentions turned into a link to that cluster. */
function WithClusters({ text }: { text: string }) {
  const parts: (string | { label: string; cluster: number })[] = [];
  let last = 0;
  for (const match of text.matchAll(CLUSTER_MENTION)) {
    const at = (match.index ?? 0) + match[1].length;
    parts.push(text.slice(last, at));
    const cluster = Number(match[2].replace(/\D/g, ""));
    parts.push(Number.isSafeInteger(cluster) ? { label: match[2], cluster } : match[2]);
    last = at + match[2].length;
  }
  parts.push(text.slice(last));
  return (
    <>
      {parts.map((part, index) =>
        typeof part === "string" ? (
          part
        ) : (
          <button
            key={index}
            className="link chat-cluster"
            title={t("Show cluster {cluster}", { cluster: count(part.cluster) })}
            onClick={() => void call("show_in_app", { kind: "cluster", value: part.cluster })}
          >
            {part.label}
          </button>
        ),
      )}
    </>
  );
}

/** Renders the small part of Markdown Claude's answers rely on. */
function Formatted({ text }: { text: string }) {
  const blocks = text.split(/```/);
  return (
    <>
      {blocks.map((block, index) =>
        index % 2 === 1 ? (
          <pre key={index} className="chat-code">{block.replace(/^[a-z]*\n/, "")}</pre>
        ) : (
          <span key={index}>
            {block.split(/(`[^`]+`|\*\*[^*]+\*\*)/).map((part, inner) => {
              if (part.startsWith("`") && part.endsWith("`") && part.length > 1) return <code key={inner}>{part.slice(1, -1)}</code>;
              if (part.startsWith("**") && part.endsWith("**") && part.length > 3) return <strong key={inner}><WithClusters text={part.slice(2, -2)} /></strong>;
              return <WithClusters key={inner} text={part} />;
            })}
          </span>
        ),
      )}
    </>
  );
}

/** Says whether the agent is answering, and how many replies there are, for the tab's dot. */
export interface ChatActivity {
  busy: boolean;
  replies: number;
}

export function Chat({ selection, onActivity }: { selection: Selection; onActivity?: (activity: ChatActivity) => void }) {
  const { language } = useLanguage();
  // The language Claude was last told in this conversation, so it is only told again on a change.
  const toldLanguage = useRef<Language | null>(null);
  const [messages, setMessages] = useState<Message[]>([]);
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const scroller = useRef<HTMLDivElement>(null);
  const [agents, setAgents] = useState<AgentInfo[]>([]);
  const [agent, setAgent] = useState("claude");
  const [models, setModels] = useState<Model[]>([]);
  const [model, setModel] = useState("");
  const loaded = useRef(false);

  // Which agents are on this Mac, and the choice remembered from last time.
  useEffect(() => {
    void Promise.all([invoke<AgentInfo[]>("chat_agents"), invoke<Record<string, any>>("ui_settings")]).then(([found, settings]) => {
      setAgents(found);
      const saved = settings[AGENT_SETTING] as { agent?: string; model?: string } | undefined;
      const known = found.find((item) => item.id === saved?.agent);
      setAgent(known?.id ?? found.find((item) => item.installed)?.id ?? "claude");
      setModel(known ? saved?.model ?? "" : "");
      loaded.current = true;
    });
  }, []);
  // Codex and opencode are asked for their models, which takes a moment.
  useEffect(() => {
    setModels([]);
    if (!agents.find((item) => item.id === agent)?.installed) return;
    let current = true;
    invoke<Model[]>("chat_models", { agent })
      .then((found) => current && setModels(found))
      .catch(() => current && setModels([{ id: "", name: t("Default") }]));
    return () => {
      current = false;
    };
  }, [agent, agents]);
  useEffect(() => {
    if (loaded.current) void invoke("set_ui_setting", { key: AGENT_SETTING, value: { agent, model } });
  }, [agent, model]);
  const chosen = agents.find((item) => item.id === agent);

  const replies = messages.filter((message) => message.role !== "you").length;
  useEffect(() => onActivity?.({ busy, replies }), [busy, replies, onActivity]);

  useEffect(() => {
    const appendToLast = (change: (message: Message) => Message) =>
      setMessages((current) => {
        const last = current[current.length - 1];
        if (!last || last.role !== "claude") return [...current, change({ role: "claude", text: "", tools: [] })];
        return [...current.slice(0, -1), change(last)];
      });

    const stop = listen<AgentEvent>("chat-event", ({ payload }) => {
      if (payload.type === "text") {
        appendToLast((message) => ({ ...message, text: message.text + payload.text }));
      } else if (payload.type === "tool") {
        appendToLast((message) => ({ ...message, tools: [...message.tools, { name: payload.name, input: payload.input }] }));
      } else if (payload.type === "error") {
        setMessages((current) => [...current, { role: "error", text: payload.text || t("The agent reported an error."), tools: [] }]);
      } else if (payload.type === "end") {
        setBusy(false);
        if (payload.code !== 0 && payload.code !== null && payload.stderr) {
          setMessages((current) => [...current, { role: "error", text: String(payload.stderr).trim(), tools: [] }]);
        }
      }
    });
    return () => {
      stop.then((unlisten) => unlisten());
    };
  }, []);

  useEffect(() => {
    scroller.current?.scrollTo({ top: scroller.current.scrollHeight });
  }, [messages]);

  async function send() {
    const text = draft.trim();
    if (!text || busy) return;
    const about = describeSelection(selection);
    setDraft("");
    setBusy(true);
    setMessages((current) => [...current, { role: "you", text, tools: [], about }]);
    try {
      // The person sees their own words. Claude also hears which language to answer in.
      await invoke("chat_send", { agent, model: model || null, message: questionFor(text, language, toldLanguage.current), selection });
      toldLanguage.current = language;
    } catch (reason) {
      setBusy(false);
      setMessages((current) => [...current, { role: "error", text: String(reason), tools: [] }]);
    }
  }

  async function startOver() {
    await invoke("chat_reset");
    toldLanguage.current = null;
    setMessages([]);
    setBusy(false);
  }

  return (
    <section className="chat" aria-label={t("Ask agent")}>
      <div className="chat-head">
        <select
          value={agent}
          aria-label={t("Agent")}
          disabled={busy}
          onChange={(event) => {
            // A conversation belongs to one agent, so another agent starts a new one.
            setAgent(event.target.value);
            setModel("");
            void startOver();
          }}
        >
          {agents.map((item) => (
            <option key={item.id} value={item.id}>{item.installed ? item.name : t("{name} (not installed)", { name: item.name })}</option>
          ))}
        </select>
        {models.length > 0 && (
          <select value={model} aria-label={t("Model")} disabled={busy} onChange={(event) => setModel(event.target.value)}>
            {!models.some((item) => item.id === model) && <option value={model}>{model}</option>}
            {models.map((item) => <option key={item.id} value={item.id}>{item.id ? item.name : t("Default model")}</option>)}
          </select>
        )}
        <span className="toolbar-spacer" />
        <button className="link" onClick={startOver} disabled={messages.length === 0}>{t("New conversation")}</button>
      </div>
      {chosen && !chosen.installed && (
        <div className="chat-install">
          <p>{t("{name} is not installed on this Mac. Install it in Terminal, then choose it again:", { name: chosen.name })}</p>
          <Command text={chosen.install} />
        </div>
      )}
      <div ref={scroller} className="chat-log" aria-live="polite">
        {messages.length === 0 && (
          <p className="empty">{t("Select something on the map, in the list or in the bytes, then ask about it. The agent can read the whole disk and test ideas, and has no way to write to it.")}</p>
        )}
        {messages.map((message, index) => (
          <div key={index} className={`chat-message chat-${message.role}`}>
            {message.about && <div className="chat-about">{t("About: {subject}", { subject: message.about })}</div>}
            {message.tools.length > 0 && (
              // What the agent looked at, folded away so the answer comes first.
              <details className="chat-tools-fold">
                <summary>{message.tools.length === 1 ? t("1 tool used") : t("{count} tools used", { count: count(message.tools.length) })}</summary>
                <ul className="chat-tools">
                  {message.tools.map((tool, inner) => (
                    <li key={inner} title={tool.input} className="mono">{tool.name}</li>
                  ))}
                </ul>
              </details>
            )}
            {message.text && <div className="chat-text"><Formatted text={message.role === "error" ? fromEngine(message.text) : message.text} /></div>}
          </div>
        ))}
        {busy && <div className="chat-busy">{t("Investigating")}</div>}
      </div>
      <div className="chat-input">
        {!busy && !draft && (
          <div className="chat-suggestions" aria-label={t("Suggested questions")}>
            {suggestionsFor(selection).map((question) => (
              <button key={question} className="chat-suggestion" onClick={() => setDraft(question)}>{question}</button>
            ))}
          </div>
        )}
        <div className="chat-context">About: {describeSelection(selection)}</div>
        <div className="composer">
          <textarea
            value={draft}
            rows={2}
            placeholder={t("What is this? Where does it lead?")}
            aria-label={t("Question for the agent")}
            onChange={(event) => setDraft(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && !event.shiftKey) {
                event.preventDefault();
                void send();
              }
            }}
          />
          {busy ? (
            <button onClick={() => invoke("chat_cancel")} title={t("Stop")} aria-label={t("Stop")}><Icon name="stop" size={13} /></button>
          ) : (
            <button className="primary" onClick={send} disabled={!draft.trim()} title={t("Ask")} aria-label={t("Ask")}><Icon name="send" size={15} /></button>
          )}
        </div>
      </div>
    </section>
  );
}
