#!/usr/bin/env bash
# Builds scripts/test_translations.tsx for Node with the bundler Vite already brings, and runs it.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$(mktemp -d "${TMPDIR:-/tmp}/translations-test.XXXXXX")"
trap 'rm -rf "$OUT"' EXIT
cd "$ROOT"
npx rolldown scripts/test_translations.tsx --format esm --platform node --file "$OUT/test.mjs" >/dev/null
node "$OUT/test.mjs"
