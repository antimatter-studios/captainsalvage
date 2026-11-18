#!/usr/bin/env bash
# Says whether a built app holds the hooks that drive its window in tests.
#
#   scripts/check-test-hooks.sh absent  <app bundle or binary>   fails if any hook is in it
#   scripts/check-test-hooks.sh present <app bundle or binary>   fails if any hook is missing
#
# A shipped or installed app must pass `absent`: the hooks are not compiled into it at all. The
# build the window tests drive (scripts/build-test-app.sh) must pass `present`. The names are
# searched for as bytes anywhere under the path, so a bundle is checked whole.
set -euo pipefail

mode="${1:-}"
path="${2:-}"
if [ "$#" -ne 2 ] || { [ "$mode" != absent ] && [ "$mode" != present ]; } || [ ! -e "$path" ]; then
    echo "Usage: scripts/check-test-hooks.sh absent|present <app bundle or binary>" >&2
    exit 2
fi

# Every name a test hook reads or answers to. Keep in step with HOOK_NAMES in
# crates/captainsalvage/tests/test_hooks.rs.
names=(
    SALVAGE_TEST_UI
    SALVAGE_TEST_HIDDEN
    SALVAGE_TEST_DARK
    SALVAGE_TEST_COPY_ROOT
    SALVAGE_TEST_STARTUP_DISKS
    SALVAGE_AGENT_
    test-eval
    ui_eval
    test_disconnect
)
# The last two are tool names the compiler matches by comparing bytes in place, so an optimised
# build need not hold them as text. A build without the hooks must hold neither as text, but a
# build with them need not either, and is not failed for it: its window tests use both.
optional=(ui_eval test_disconnect)

bad=0
for name in "${names[@]}"; do
    if grep -r -a -q -F -- "$name" "$path"; then
        found=yes
    else
        found=no
    fi
    printf '%-28s %s\n' "$name" "$found"
    if [ "$mode" = absent ] && [ "$found" = yes ]; then bad=1; fi
    if [ "$mode" = present ] && [ "$found" = no ]; then
        case " ${optional[*]} " in *" $name "*) ;; *) bad=1 ;; esac
    fi
done

if [ "$bad" -ne 0 ]; then
    if [ "$mode" = absent ]; then
        echo "FAIL: $path holds test hooks. A shipped app must not." >&2
    else
        echo "FAIL: $path lacks test hooks. Build it with scripts/build-test-app.sh." >&2
    fi
    exit 1
fi
echo "PASS: test hooks are $mode in $path"
