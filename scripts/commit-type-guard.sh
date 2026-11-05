#!/usr/bin/env bash
# Commit-msg: a commit's type must agree with its branch's prefix.
#
# The fragment's heading comes from the branch, and the version comes from the pull request's
# type, so a `feat:` on a `fix/` branch would say one thing in the changelog and another in the
# version. Types with no effect on a release are not checked, so a `docs:` or `test:` commit can
# sit on any branch. Install with scripts/install-hooks.sh.
set -u

branch="$(git symbolic-ref --quiet --short HEAD 2>/dev/null)" || exit 0
subject="$(sed -n 1p "$1")"
type="$(printf '%s' "$subject" | sed -nE 's/^([a-z]+)(\([^)]*\))?!?:.*/\1/p')"

case "$branch:$type" in
    feat/*:fix | fix/*:feat)
        cat >&2 <<EOF
commit-type-guard: this commit says "$type:", but the branch is $branch.

A fix repairs something that was wrong, and a feature adds something new. Pick the one that is
true. Either change the commit's type, or move the work to a branch named for it. If this change
really does both, split it, or use feat.
EOF
        exit 1
        ;;
esac
exit 0
