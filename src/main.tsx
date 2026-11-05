import { invoke } from "@tauri-apps/api/core";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import { isLanguage, LanguageRoot, systemLanguage } from "./i18n";
import "./styles.css";

// The saved language is read before the first paint, so the window never flashes in English.
invoke<string | null>("get_language")
  .catch(() => null)
  .then((saved) => {
    createRoot(document.getElementById("root") as HTMLElement).render(
      <StrictMode>
        <LanguageRoot initial={isLanguage(saved) ? saved : systemLanguage()}>
          <App />
        </LanguageRoot>
      </StrictMode>,
    );
  });
