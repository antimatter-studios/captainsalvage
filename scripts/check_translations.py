#!/usr/bin/env python3
"""Checks that every English string the window translates has a translation in each language.

The window's text is English in the code, passed to t("..."), and the English the engine sends
is listed in src/engineText.ts. Each file in src/locales maps that English to one language. This fails when a string has no translation, when a translation is left
over from text that is gone, when a translation drops or invents a {placeholder}, when t() is
given anything but a plain string, which could not be checked, or when an engine sentence is no
longer in the Rust source.

    python3 scripts/check_translations.py            check, for CI
    python3 scripts/check_translations.py --missing  print what each language lacks, as JSON
"""
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "src"
LOCALES = SOURCE / "locales"
ENGINE_TEXT = SOURCE / "engineText.ts"
RUST = [ROOT / "crates" / "lens-core" / "src", ROOT / "src-tauri" / "src"]
LANGUAGES = ["fr", "de", "es"]
# t( preceded by something that cannot end a name, so other functions ending in t are skipped.
CALL = re.compile(r'(?<![\w.$])t\(\s*')
STRING = re.compile(r'"((?:[^"\\]|\\.)*)"')
PLACEHOLDER = re.compile(r"\{(\w+)\}")


def used_strings() -> tuple[set[str], list[str]]:
    found: set[str] = set()
    problems: list[str] = []
    for path in sorted(SOURCE.rglob("*.ts*")):
        if LOCALES in path.parents or path.name in ("i18n.tsx", "engineText.ts"):
            continue
        text = path.read_text()
        for call in CALL.finditer(text):
            rest = text[call.end():]
            literal = STRING.match(rest)
            line = text.count("\n", 0, call.start()) + 1
            where = f"{path.relative_to(ROOT)}:{line}"
            if not literal:
                problems.append(f"{where}: t() needs a plain string, so it can be checked")
                continue
            after = rest[literal.end():].lstrip()
            if after.startswith("+"):
                problems.append(f"{where}: t() needs one string, not a joined one")
                continue
            found.add(json.loads(f'"{literal.group(1)}"'))
    return found, problems


def engine_lists() -> dict[str, list[str]]:
    text = ENGINE_TEXT.read_text()
    lists = {}
    for name in ("ENGINE_TEXT", "SYSTEM_TEXT"):
        start = text.index(f"export const {name}")
        body = text[text.index("[", start): text.index("];", start)]
        body = "\n".join(line for line in body.splitlines() if not line.strip().startswith("//"))
        lists[name] = [json.loads(f'"{item}"') for item in STRING.findall(body)]
    return lists


def engine_drift(templates: list[str]) -> list[str]:
    """Each piece of an engine sentence must still be in the Rust source, or it is stale."""
    source = "".join(path.read_text() for folder in RUST for path in folder.rglob("*.rs"))
    source = re.sub(r"\\\n\s*", "", source)
    problems = []
    for template in templates:
        pieces = re.split(r"\{\w+\}|, |: ", template)
        for piece in (piece.strip(" .,:") for piece in pieces):
            if len(piece) >= 4 and piece not in source:
                problems.append(f"engine: {template!r} is no longer in the Rust source ({piece!r})")
                break
    return problems


def translations(language: str) -> dict[str, str]:
    text = (LOCALES / f"{language}.ts").read_text()
    body = text[text.index("{"): text.rindex("}") + 1]
    body = re.sub(r",\s*}$", "}", body)
    return json.loads(body)


def main() -> int:
    used, problems = used_strings()
    engine = engine_lists()
    used |= set(engine["ENGINE_TEXT"]) | set(engine["SYSTEM_TEXT"])
    problems += engine_drift(engine["ENGINE_TEXT"])
    missing: dict[str, list[str]] = {}
    for language in LANGUAGES:
        table = translations(language)
        lacking = sorted(used - table.keys())
        missing[language] = lacking
        for text in lacking:
            problems.append(f"{language}: no translation for {text!r}")
        for text in sorted(table.keys() - used):
            problems.append(f"{language}: translation left over for {text!r}")
        for text, translated in table.items():
            if set(PLACEHOLDER.findall(text)) != set(PLACEHOLDER.findall(translated)):
                problems.append(f"{language}: {text!r} and its translation have different placeholders")
            if not translated.strip():
                problems.append(f"{language}: empty translation for {text!r}")
    if "--missing" in sys.argv:
        print(json.dumps(missing, ensure_ascii=False, indent=1))
        return 0
    for problem in problems:
        print(problem)
    print(f"translations: {len(used)} strings, {len(LANGUAGES)} languages, {len(problems)} problems")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
