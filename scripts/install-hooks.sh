#!/usr/bin/env bash
# Puts this repository's own guards beside github-guard's.
#
# They are copied, not linked or called from the working tree, because a hook that runs a file from
# the checkout would let any branch replace what runs next. The pre-commit guard's name sorts last,
# so the guards that block private files, and the formatters that re-stage, have already run. Re-run
# this after either script changes.
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
hooks="$(git rev-parse --git-path hooks)"
[ -x "$hooks/pre-commit" ] && [ -x "$hooks/commit-msg" ] || {
    echo "install-hooks: github-guard's hook dispatchers are not installed in this clone." >&2
    exit 1
}
install_guard() {
    mkdir -p "$hooks/$2.d"
    cp "$root/scripts/$1" "$hooks/$2.d/$3"
    chmod +x "$hooks/$2.d/$3"
    echo "install-hooks: installed $hooks/$2.d/$3"
}
install_guard changelog-fragment.sh pre-commit zz-changelog-fragment.sh
install_guard commit-type-guard.sh commit-msg commit-type-guard.sh
