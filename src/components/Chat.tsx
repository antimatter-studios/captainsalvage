import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useRef, useState } from "react";
import type { Selection } from "../api";
import { count } from "../format";
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

const TOOL_PREFIX = "mcp__exfat-lens__";

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
      return selection.name || `Folder at cluster ${count(selection.head)}`;
    case "chain":
      return `Nameless chain at cluster ${count(selection.head)}`;
    case "region":
      return `Unexplained region, clusters ${count(selection.startCluster)} to ${count(selection.endCluster)}`;
    default:
      return "Nothing selected";
  }
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
              if (part.startsWith("**") && part.endsWith("**") && part.length > 3) return <strong key={inner}>{part.slice(2, -2)}</strong>;
              return part;
            })}
          </span>
        ),
      )}
    </>
  );
}

export function Chat({ selection }: { selection: Selection }) {
  const [messages, setMessages] = useState<Message[]>([]);
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const scroller = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const appendToLast = (change: (message: Message) => Message) =>
      setMessages((current) => {
        const last = current[current.length - 1];
        if (!last || last.role !== "claude") return [...current, change({ role: "claude", text: "", tools: [] })];
        return [...current.slice(0, -1), change(last)];
      });

    const stop = listen<Record<string, any>>("chat-event", ({ payload }) => {
      if (payload.type === "stream_event") {
        const event = payload.event;
        if (event?.type === "content_block_delta" && event.delta?.type === "text_delta") {
          appendToLast((message) => ({ ...message, text: message.text + event.delta.text }));
        }
        if (event?.type === "content_block_start" && event.content_block?.type === "tool_use") {
          const name = String(event.content_block.name).replace(TOOL_PREFIX, "");
          appendToLast((message) => ({ ...message, tools: [...message.tools, { name, input: "" }] }));
        }
      } else if (payload.type === "assistant") {
        const used = (payload.message?.content ?? []).filter((block: any) => block.type === "tool_use");
        if (used.length > 0) {
          appendToLast((message) => {
            const tools = [...message.tools];
            for (const block of used) {
              const name = String(block.name).replace(TOOL_PREFIX, "");
              const index = tools.findIndex((tool) => tool.name === name && tool.input === "");
              const input = JSON.stringify(block.input);
              if (index >= 0) tools[index] = { name, input };
              else tools.push({ name, input });
            }
            return { ...message, tools };
          });
        }
      } else if (payload.type === "result" && payload.is_error) {
        setMessages((current) => [...current, { role: "error", text: String(payload.result ?? "Claude reported an error."), tools: [] }]);
      } else if (payload.type === "chat-end") {
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
      await invoke("chat_send", { message: text, selection });
    } catch (reason) {
      setBusy(false);
      setMessages((current) => [...current, { role: "error", text: String(reason), tools: [] }]);
    }
  }

  async function startOver() {
    await invoke("chat_reset");
    setMessages([]);
    setBusy(false);
  }

  return (
    <section className="chat" aria-label="Ask Claude">
      <div className="chat-head">
        <strong><Icon name="sparkle" size={15} /> Ask Claude</strong>
        <button className="link" onClick={startOver} disabled={messages.length === 0}>New conversation</button>
      </div>
      <div ref={scroller} className="chat-log" aria-live="polite">
        {messages.length === 0 && (
          <p className="empty">Select something on the map, in the list or in the bytes, then ask about it. Claude can read the whole disk and test ideas, and has no way to write to it.</p>
        )}
        {messages.map((message, index) => (
          <div key={index} className={`chat-message chat-${message.role}`}>
            {message.about && <div className="chat-about">About: {message.about}</div>}
            {message.tools.length > 0 && (
              <ul className="chat-tools">
                {message.tools.map((tool, inner) => (
                  <li key={inner} title={tool.input} className="mono">{tool.name}</li>
                ))}
              </ul>
            )}
            {message.text && <div className="chat-text"><Formatted text={message.text} /></div>}
          </div>
        ))}
        {busy && <div className="chat-busy">Investigating</div>}
      </div>
      <div className="chat-input">
        <div className="chat-context">About: {describeSelection(selection)}</div>
        <div className="composer">
          <textarea
            value={draft}
            rows={2}
            placeholder="What is this? Where does it lead?"
            aria-label="Question for Claude"
            onChange={(event) => setDraft(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && !event.shiftKey) {
                event.preventDefault();
                void send();
              }
            }}
          />
          {busy ? (
            <button onClick={() => invoke("chat_cancel")} title="Stop" aria-label="Stop"><Icon name="stop" size={13} /></button>
          ) : (
            <button className="primary" onClick={send} disabled={!draft.trim()} title="Ask" aria-label="Ask"><Icon name="send" size={15} /></button>
          )}
        </div>
      </div>
    </section>
  );
}
