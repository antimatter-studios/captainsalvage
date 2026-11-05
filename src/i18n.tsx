import { invoke } from "@tauri-apps/api/core";
import { createContext, useCallback, useContext, useState, type ReactNode } from "react";
import de from "./locales/de";
import es from "./locales/es";
import fr from "./locales/fr";

/**
 * The window's languages. English is written in the code, and each other language maps that
 * English, word for word, to its own. A string with no translation shows in English.
 */
export type Language = "en" | "fr" | "de" | "es";

export const LANGUAGES: { id: Language; label: string; name: string }[] = [
  { id: "en", label: "EN", name: "English" },
  { id: "fr", label: "FR", name: "Français" },
  { id: "de", label: "DE", name: "Deutsch" },
  { id: "es", label: "ES", name: "Español" },
];

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

const LanguageContext = createContext<{ language: Language; choose: (language: Language) => void }>({
  language: "en",
  choose: () => undefined,
});

export function useLanguage() {
  return useContext(LanguageContext);
}

/**
 * Holds the chosen language above the whole window. Changing it renders everything again, so
 * every `t` call, wherever it is, picks up the new language.
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
