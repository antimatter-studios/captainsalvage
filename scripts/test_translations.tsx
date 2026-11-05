// Tests that engine text is translated sentence by sentence, with the values in it kept and,
// where they are names the engine uses, translated too. Run with scripts/test_translations.sh.
import { renderToString } from "react-dom/server";
import { fromEngine, LanguageRoot, type Language } from "../src/i18n";

(globalThis as { document?: unknown }).document = { documentElement: {} };

let passed = 0;
let failed = 0;
function check(name: string, got: string, want: string) {
  if (got === want) {
    passed += 1;
  } else {
    failed += 1;
    console.error(`FAILED: ${name}\n  expected: ${want}\n  got:      ${got}`);
  }
}
function use(language: Language) {
  renderToString(<LanguageRoot initial={language}>{null}</LanguageRoot>);
}

use("en");
check("English is left alone", fromEngine("Folder contents found."), "Folder contents found.");

use("fr");
check("a known sentence", fromEngine("Folder contents found."), "Contenu du dossier trouvé.");
check(
  "joined sentences and their numbers",
  fromEngine("Entry checksum failed, so its fields may be damaged. FAT chain ends early: 3 of 9 clusters reachable."),
  "La somme de contrôle de l'entrée est invalide : ses champs sont peut-être endommagés. La chaîne FAT s'arrête tôt : 3 clusters accessibles sur 9.",
);
check(
  "a name inside a sentence is translated",
  fromEngine("First cluster is JPEG start, matching the name."),
  "Le premier cluster est du type début de JPEG, ce qui correspond au nom.",
);
check(
  "an error's parts are translated one by one",
  fromEngine("main boot sector: no exFAT signature at offset 3: this is not an exFAT volume"),
  "secteur d'amorçage principal : aucune signature exFAT au décalage 3 : ce n'est pas un volume exFAT".replace(/ : /g, ": "),
);
check(
  "a system error at the end of a sentence",
  fromEngine("cannot open /dev/rdisk9s2. Use the disk_access tool to see what needs granting.: Permission denied (os error 13)"),
  "impossible d'ouvrir /dev/rdisk9s2. L'écran d'accueil indique quelle autorisation donner.: Autorisation refusée (erreur système 13)",
);
check("unknown text is kept", fromEngine("something the engine never says"), "something the engine never says");
check("nothing stays nothing", fromEngine(null), "");

use("de");
check(
  "German, with a value that is not a name",
  fromEngine("It uses compression method 99, which this app cannot unpack yet."),
  "Sie nutzt die Kompressionsmethode 99, die diese App noch nicht entpacken kann.",
);

console.log(`translations: ${passed} passed, ${failed} failed`);
process.exit(failed === 0 ? 0 : 1);
