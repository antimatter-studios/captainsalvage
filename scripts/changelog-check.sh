#!/usr/bin/env bash
# Check that a repository keeps its unreleased changelog as fragments.
#
# Since agent-core v6, a pull request does not edit CHANGELOG.md: it adds
# one file under changelog.d/, so pull requests open at the same time never
# conflict over the same lines at the top of one file. A release gathers the
# fragments into a dated section of CHANGELOG.md and deletes them in the same
# commit (changelog-gather.sh), so an entry is a fragment until it ships and a
# line of CHANGELOG.md after, and never both.
#
# This is what keeps that true, run by `chore lint`:
#
#   - changelog.d/ exists, and holds only fragments (*.md) and its README.md;
#   - every fragment is one or more `### Heading` sections, each with at least
#     one `- ` entry, and nothing before the first heading or above level 3
#     (the release supplies the `## [version]` heading);
#   - CHANGELOG.md has no unreleased entries of its own: an `## [Unreleased]`
#     section, if it keeps one, is empty up to the next heading or to the
#     link definitions at the end.
#
# Usage: changelog-check.sh            (the calling repository)
#   FS_CORE_CALLER names the repository when run in place from rust-fs-core.
set -euo pipefail

ROOT="${FS_CORE_CALLER:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
DIR="$ROOT/changelog.d"

die() { echo "changelog-check: $*" >&2; exit 1; }

[ -d "$DIR" ] || die "there is no changelog.d/: unreleased entries are fragments there (agent-core v6), one file per pull request"

problems=0
problem() { echo "changelog-check: $*" >&2; problems=$((problems + 1)); }

fragments=0
for path in "$DIR"/* "$DIR"/.[!.]*; do
    [ -e "$path" ] || continue
    name="$(basename "$path")"
    [ "$name" = "README.md" ] && continue
    case "$name" in
        .gitkeep) continue ;;
        *.md) ;;
        *) problem "changelog.d/$name is not a fragment: only *.md fragments and README.md belong there"; continue ;;
    esac
    [ -f "$path" ] || { problem "changelog.d/$name is not a file"; continue; }
    fragments=$((fragments + 1))
    # One pass: the first non-blank line must be a level-3 heading, no
    # heading may be above level 3, and every heading needs an entry.
    verdict="$(awk '
        /^[[:space:]]*$/ { next }
        /^#[^#]/ || /^##[^#]/ || /^#$/ || /^##$/ {
            print "a heading above level 3 (line " NR "): the release adds the version heading"; bad = 1; exit
        }
        /^###[[:space:]]+[^[:space:]]/ {
            if (open && !entries) { print "the heading \"" heading "\" has no entries"; bad = 1; exit }
            heading = $0; sub(/^###[[:space:]]+/, "", heading)
            open = 1; entries = 0; seen = 1; next
        }
        /^###/ { print "an empty heading (line " NR ")"; bad = 1; exit }
        !seen { print "text before the first ### heading (line " NR ")"; bad = 1; exit }
        /^- / { entries++ }
        END {
            if (bad) exit
            if (!seen) { print "no ### heading and no entries: the fragment is empty"; exit }
            if (!entries) { print "the heading \"" heading "\" has no entries" }
        }
    ' "$path")"
    [ -z "$verdict" ] || problem "changelog.d/$name: $verdict"
done

if [ -f "$ROOT/CHANGELOG.md" ]; then
    unreleased="$(awk '
        /^##[[:space:]]+\[?[Uu]nreleased\]?/ { inside = 1; next }
        /^##[[:space:]]/ || /^\[[^]]+\]:[[:space:]]/ { inside = 0 }
        inside && /[^[:space:]]/ { n++ }
        END { print n + 0 }
    ' "$ROOT/CHANGELOG.md")"
    if [ "$unreleased" -gt 0 ]; then
        problem "CHANGELOG.md has $unreleased line(s) under Unreleased: an unreleased entry is a fragment in changelog.d/, and CHANGELOG.md takes entries only when a release gathers them"
    fi
fi

[ "$problems" -eq 0 ] || die "$problems problem(s)"
echo "changelog-check: $fragments fragment(s) in changelog.d/, and CHANGELOG.md holds no unreleased entries of its own"
