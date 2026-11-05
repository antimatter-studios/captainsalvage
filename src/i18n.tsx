import { invoke } from "@tauri-apps/api/core";
import { createContext, useCallback, useContext, useState, type ReactNode } from "react";
import { ENGINE_TEXT, SYSTEM_TEXT } from "./engineText";
import de from "./locales/de";
import es from "./locales/es";
import fr from "./locales/fr";

/**
 * The window's languages. English is written in the code, and each other language maps that
 * English, word for word, to its own. A string with no translation shows in English.
 */
export type Language = "en" | "fr" | "de" | "es";

/** Each language with its short label, its own name, and its name in English for Claude. */
export const LANGUAGES: { id: Language; label: string; name: string; english: string }[] = [
  { id: "en", label: "EN", name: "English", english: "English" },
  { id: "fr", label: "FR", name: "Français", english: "French" },
  { id: "de", label: "DE", name: "Deutsch", english: "German" },
  { id: "es", label: "ES", name: "Español", english: "Spanish" },
];

/**
 * A question for Claude, told the person's language when Claude has not been told it yet in
 * this conversation. Claude keeps it for the rest of the session, so it is said once, and again
 * only after the person switches. English needs no telling at the start.
 */
export function questionFor(text: string, language: Language, told: Language | null): string {
  if (told === language || (told === null && language === "en")) return text;
  const name = LANGUAGES.find((item) => item.id === language)?.english ?? "English";
  return `I speak ${name}, so please answer in ${name}.\n\n${text}`;
}

const LOCALES: Record<Language, string> = { en: "en-US", fr: "fr-FR", de: "de-DE", es: "es-ES" };
const DICTIONARIES: Record<Language, Record<string, string>> = { en: {}, fr, de, es };

let current: Language = "en";

export function isLanguage(value: unknown): value is Language {
  return typeof value === "string" && value in LOCALES;
}

/** The first of macOS's preferred languages that the window has, or English. */
export function systemLanguage(): Language {
  for (const tag of navigator.languages ?? [navigator.language]) {
    const base = tag.slice(0, 2).toLowerCase();
    if (isLanguage(base)) return base;
  }
  return "en";
}

/** The locale numbers and dates are written in. */
export function locale(): string {
  return LOCALES[current];
}

export function currentLanguage(): Language {
  return current;
}

/**
 * Translates English written in the code. `{name}` in the text is replaced from `values`, after
 * translating, so a translation can move it. The text must be a literal, so it can be checked.
 */
export function t(text: string, values?: Record<string, string | number>): string {
  const chosen = DICTIONARIES[current][text] ?? text;
  if (!values) return chosen;
  return chosen.replace(/\{(\w+)\}/g, (whole, name: string) => (name in values ? String(values[name]) : whole));
}

interface EnginePattern {
  template: string;
  pattern: RegExp;
  names: string[];
}

let enginePatterns: EnginePattern[] | null = null;

/** The engine's templates as patterns, the most specific first, built once. */
function patterns(): EnginePattern[] {
  if (enginePatterns) return enginePatterns;
  const literal = (template: string) => template.replace(/\{\w+\}/g, "").length;
  enginePatterns = [...ENGINE_TEXT, ...SYSTEM_TEXT]
    .filter((template) => /\{\w+\}/.test(template))
    .map((template) => {
      const names: string[] = [];
      const source = template
        .split(/(\{\w+\})/)
        .map((part) => {
          const placeholder = /^\{(\w+)\}$/.exec(part);
          if (placeholder) {
            names.push(placeholder[1]);
            return "(.+?)";
          }
          return part.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
        })
        .join("");
      return { template, pattern: new RegExp(`^${source}$`, "s"), names };
    })
    .sort((a, b) => literal(b.template) - literal(a.template));
  return enginePatterns;
}

/** One sentence, name or part of an error, translated, or null when it is not known. */
function translatePart(part: string): string | null {
  const table = DICTIONARIES[current];
  if (table[part] !== undefined) return table[part];
  for (const { template, pattern, names } of patterns()) {
    const match = pattern.exec(part);
    if (!match) continue;
    const translated = table[template];
    if (translated === undefined) return null;
    // A value such as a content name is translated too, when it is one the engine uses.
    const values = Object.fromEntries(names.map((name, index) => [name, table[match[index + 1]] ?? match[index + 1]]));
    return translated.replace(/\{(\w+)\}/g, (whole, name: string) => values[name] ?? whole);
  }
  return null;
}

/**
 * Shows text the engine wrote in the window's language. The engine writes English sentences,
 * often several joined, and errors whose parts are joined by ": ". Each sentence is matched
 * against src/engineText.ts, then each part of it, and anything unknown stays as it is.
 */
export function fromEngine(text: string | null | undefined): string {
  if (!text) return text ?? "";
  if (current === "en") return text;
  return text
    .split(/(?<=[.!?])\s+(?=[A-Z])/)
    .map((sentence) => translatePart(sentence) ?? sentence.split(": ").map((part) => translatePart(part) ?? part).join(": "))
    .join(" ");
}

const LanguageContext = createContext<{ language: Language; choose: (language: Language) => void }>({
  language: "en",
  choose: () => undefined,
});

export function useLanguage() {
  return useContext(LanguageContext);
}

/**
 * Holds the chosen language above the whole window. App reads it with useLanguage, so changing
 * it draws App and everything in it again, and every `t` call picks up the new language. Words
 * kept in a useMemo need the language among its dependencies.
 */
export function LanguageRoot({ initial, children }: { initial: Language; children: ReactNode }) {
  const [language, setLanguage] = useState(initial);
  current = language;
  document.documentElement.lang = language;
  const choose = useCallback((next: Language) => {
    setLanguage(next);
    void invoke("set_language", { language: next }).catch(() => undefined);
  }, []);
  return <LanguageContext.Provider value={{ language, choose }}>{children}</LanguageContext.Provider>;
}
