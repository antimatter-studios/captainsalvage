#!/usr/bin/env bash
# Builds the copy of the app that the window tests (tests/window/) drive: the same code as the
# shipped app, plus the `test-hooks` feature. It goes in target/test-hooks, never in target/release,
# so it cannot replace the app you installed or the one `npm run app` made.
#
# It is not signed, has no Finder extension and is never installed. Never ship it.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
export CARGO_TARGET_DIR="$root/target/test-hooks"
npx tauri build --bundles app --features test-hooks
app="$CARGO_TARGET_DIR/release/bundle/macos/Captain Salvage.app"
"$root/scripts/check-test-hooks.sh" present "$app"
echo "Test build: $app/Contents/MacOS/captain-salvage"
