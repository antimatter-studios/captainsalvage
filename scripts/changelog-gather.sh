#!/usr/bin/env bash
# Gather changelog.d/ into a dated CHANGELOG.md section, for a release.
#
# Between releases a repository's unreleased changes are fragments, one file
# per pull request under changelog.d/ (agent-core v6; changelog-check.sh). A
# release commit runs this: it writes `## [VERSION] — DATE` into CHANGELOG.md
# with every fragment's entries under their headings, and deletes the
# fragments, so the entries move rather than copy and the directory is empty
# again after every release (its README.md stays).
#
# Headings: Keep a Changelog's own, in its order (Added, Changed, Deprecated,
# Removed, Fixed, Security), then any other a fragment used, in the order the
# fragments first use them. Fragments are read in file-name order, so the
# release's entries come out the same on every machine.
#
# The version heading copies the newest one CHANGELOG.md already has --
# `## [1.2.3] — DATE`, `## v1.2.3 — DATE` or `## 1.2.3 — DATE`, with the
# separator it uses -- so a repository keeps one style; a changelog with no
# release yet gets `## [VERSION] — DATE`.
#
# Where the section goes: under an `## [Unreleased]` heading, which stays,
# empty (changelog-check.sh keeps it so); with none, above the newest
# release; with no release either, at the end.
#
# Links: a CHANGELOG.md that keeps Keep a Changelog's link definitions at the
# end gets them kept up: `[Unreleased]: URL/compare/vPREV...HEAD` moves to
# compare from the new version, and `[VERSION]: URL/compare/vPREV...vVERSION`
# is added beneath it. Without an Unreleased link, the new version's link
# goes above the newest one. A file with no compare links gets none.
#
# It refuses, changing nothing, when a fragment is malformed, when there are
# no fragments (a release describes itself), or when CHANGELOG.md already has
# a section for VERSION.
#
# --append is for a release whose section exists and is not released yet, such as a
# release candidate's integration branch that has taken more pull requests since the
# section was written. It gathers the fragments that remain into that section, under
# the headings they use (a new heading goes in Keep a Changelog's order), and leaves
# its heading, its date and the compare links as they were. An entry the section
# already has, word for word, is not written twice: a fragment that came back from a
# merge is deleted all the same, and the run says how many entries it skipped. With
# no section for VERSION, --append refuses, because there is nothing to add to.
#
# Usage: changelog-gather.sh [--append] VERSION [DATE]
#   VERSION  e.g. 0.3.10 (a leading v is dropped)
#   DATE     YYYY-MM-DD, default today (UTC); --append keeps the section's own date
#   FS_CORE_CALLER names the repository when run in place from rust-fs-core.
set -euo pipefail

die() { echo "changelog-gather: $*" >&2; exit 1; }

append=false
if [ "${1:-}" = "--append" ]; then append=true; shift; fi
version="${1:-}"
[ -n "$version" ] || die "usage: changelog-gather.sh [--append] VERSION [DATE]"
version="${version#v}"
date="${2:-$(date -u +%F)}"
case "$date" in
    [0-9][0-9][0-9][0-9]-[0-1][0-9]-[0-3][0-9]) ;;
    *) die "the date '$date' is not YYYY-MM-DD" ;;
esac

ROOT="${FS_CORE_CALLER:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
DIR="$ROOT/changelog.d"
LOG="$ROOT/CHANGELOG.md"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

[ -f "$LOG" ] || die "there is no CHANGELOG.md in $ROOT"
FS_CORE_CALLER="$ROOT" bash "$HERE/changelog-check.sh" >/dev/null ||
    die "changelog-check.sh refused the fragments; nothing was changed"

fragments=()
while IFS= read -r f; do
    fragments+=("$f")
done < <(find "$DIR" -maxdepth 1 -type f -name '*.md' ! -name README.md | LC_ALL=C sort)
[ "${#fragments[@]}" -gt 0 ] || die "changelog.d/ has no fragments: a release with nothing to say is not a release"

has_section=false
if awk -v v="$version" '
    /^##[[:space:]]/ {
        s = $0; sub(/^##[[:space:]]+/, "", s); sub(/^\[/, "", s); sub(/^v/, "", s)
        split(s, a, /[][[:space:]]/)
        if (a[1] == v) found = 1
    }
    END { exit !found }
' "$LOG"; then
    has_section=true
fi
if [ "$append" = "false" ] && [ "$has_section" = "true" ]; then
    die "CHANGELOG.md already has a section for $version. To add the fragments that remain to it, run with --append"
fi
if [ "$append" = "true" ] && [ "$has_section" = "false" ]; then
    die "CHANGELOG.md has no section for $version to append to. Run without --append to write one"
fi

# The heading's style: the newest version heading's.
style="$(awk '
    /^##[[:space:]]/ {
        s = $0; sub(/^##[[:space:]]+/, "", s)
        if (s ~ /^\[?[Uu]nreleased/) next
        if (s ~ /^\[v?[0-9]/) kind = "bracket"
        else if (s ~ /^v[0-9]/) kind = "v"
        else if (s ~ /^[0-9]+\.[0-9]+/) kind = "bare"
        else next
        sep = (s ~ / - [0-9]/) ? "hyphen" : "emdash"
        print kind, sep; exit
    }
' "$LOG")"
case "${style%% *}" in
    v) open_v="v"; close_v="" ;;
    bare) open_v=""; close_v="" ;;
    *) open_v="["; close_v="]" ;;
esac
case "${style##* }" in
    hyphen) sep="-" ;;
    *) sep="—" ;;
esac
heading="## ${open_v}${version}${close_v} ${sep} ${date}"

# The section: every fragment's body under each heading, headings in order.
section="$(awk -v vhead="$heading" '
    BEGIN {
        split("Added Changed Deprecated Removed Fixed Security", canon, " ")
        for (i = 1; i <= 6; i++) rank[canon[i]] = i
    }
    FNR == 1 { heading = "" }
    /^###[[:space:]]/ {
        heading = $0; sub(/^###[[:space:]]+/, "", heading); sub(/[[:space:]]+$/, "", heading)
        if (!(heading in body)) {
            order[++n] = heading
            body[heading] = ""
        }
        pending[heading] = 1
        next
    }
    heading != "" {
        # Blank lines between one fragment and the next are written once.
        if ($0 ~ /^[[:space:]]*$/) { if (body[heading] != "") gap[heading] = 1; next }
        if (gap[heading] || (pending[heading] && body[heading] != "")) body[heading] = body[heading] "\n"
        gap[heading] = 0; pending[heading] = 0
        body[heading] = body[heading] $0 "\n"
    }
    END {
        printf "%s\n", vhead
        for (i = 1; i <= 6; i++) if (canon[i] in body) emit(canon[i])
        for (i = 1; i <= n; i++) if (!(order[i] in rank)) emit(order[i])
    }
    function emit(h) { printf "\n### %s\n\n%s", h, body[h] }
' "${fragments[@]}")"

# Write it in. A new release goes under the (empty) Unreleased heading, above the newest release, or
# at the end, and the compare links are kept up. With --append the section is there already, and the
# fragments are merged into it.
tmp="$(mktemp "${TMPDIR:-/tmp}/changelog-gather.XXXXXX")"
trap 'rm -f "$tmp" "$tmp.skipped"' EXIT

if [ "$append" = "true" ]; then
    # The section as it is, to tell which entries it has already.
    existing="$(awk -v v="$version" '
        function is_target(line,   s, a) {
            s = line; sub(/^##[[:space:]]+/, "", s); sub(/^\[/, "", s); sub(/^v/, "", s)
            split(s, a, /[][[:space:]]/)
            return a[1] == v
        }
        /^##[[:space:]]/ { inside = is_target($0); next }
        /^\[[^]]+\]:[[:space:]]/ { inside = 0 }
        inside { print }
    ' "$LOG")"
    # Each heading the fragments use goes onto the end of the same heading in the section, or into a
    # new one in Keep a Changelog order. An entry is told by its first line.
    SECTION="$section" EXISTING="$existing" awk -v ver="$version" '
        function is_target(line,   s, a) {
            s = line; sub(/^##[[:space:]]+/, "", s); sub(/^\[/, "", s); sub(/^v/, "", s)
            split(s, a, /[][[:space:]]/)
            return a[1] == ver
        }
        function rank_of(h) { return (h in rank) ? rank[h] : 99 }
        function trim(s) { sub(/^\n+/, "", s); sub(/\n+$/, "", s); return s }
        function emit_before(limit,   i, h) {
            for (i = 1; i <= nn; i++) {
                h = norder[i]
                if (nb[h] != "" && !(h in present) && !(h in written) && rank_of(h) < limit) {
                    print "### " h; print ""; print nb[h]; print ""
                    written[h] = 1
                }
            }
        }
        function finish(   i, h, c) {
            for (i = 1; i <= nblk; i++) present[bh[i]] = 1
            pre = trim(pre)
            if (pre != "") { print pre; print "" }
            for (i = 1; i <= nblk; i++) {
                h = bh[i]
                emit_before(rank_of(h))
                c = trim(bc[i])
                print "### " h; print ""
                if (c != "") print c
                if (nb[h] != "") { if (c != "") print ""; print nb[h] }
                print ""
            }
            emit_before(100)
            inside = 0
        }
        BEGIN {
            split("Added Changed Deprecated Removed Fixed Security", canon, " ")
            for (i = 1; i <= 6; i++) rank[canon[i]] = i
            m = split(ENVIRON["EXISTING"], E, "\n")
            for (i = 1; i <= m; i++) have[E[i]] = 1
            m = split(ENVIRON["SECTION"], L, "\n")
            for (i = 1; i <= m; i++) {
                if (L[i] ~ /^###[[:space:]]/) {
                    h = L[i]; sub(/^###[[:space:]]+/, "", h); sub(/[[:space:]]+$/, "", h)
                    norder[++nn] = h; nb[h] = ""; cur = h; skip = 0; blank = 0
                    continue
                }
                if (cur == "") continue
                if (L[i] ~ /^[[:space:]]*$/) { blank = 1; skip = 0; continue }
                if (L[i] ~ /^- /) {
                    skip = (L[i] in have)
                    if (skip) dropped++
                }
                if (skip) continue
                if (blank && nb[cur] != "") nb[cur] = nb[cur] "\n"
                blank = 0
                nb[cur] = nb[cur] (nb[cur] == "" ? "" : "\n") L[i]
            }
        }
        /^##[[:space:]]/ || /^\[[^]]+\]:[[:space:]]/ {
            if (inside) finish()
            if (/^##[[:space:]]/ && is_target($0)) { inside = 1; print; print ""; next }
        }
        inside && /^###[[:space:]]/ {
            h = $0; sub(/^###[[:space:]]+/, "", h); sub(/[[:space:]]+$/, "", h)
            bh[++nblk] = h; bc[nblk] = ""; next
        }
        inside { if (nblk == 0) pre = pre $0 "\n"; else bc[nblk] = bc[nblk] $0 "\n"; next }
        { print }
        END {
            if (inside) finish()
            print dropped + 0 > "/dev/stderr"
        }
    ' "$LOG" >"$tmp" 2>"$tmp.skipped"
    skipped="$(cat "$tmp.skipped")"
    cat "$tmp" >"$LOG"
else
    SECTION="$section" awk '
        BEGIN { section = ENVIRON["SECTION"] }
        !done && /^##[[:space:]]+\[?[Uu]nreleased\]?/ {
            print; print ""; print section; print ""; done = 1; skipping = 1; next
        }
        skipping && (/^##[[:space:]]/ || /^\[[^]]+\]:[[:space:]]/) { skipping = 0 }
        skipping { next }
        !done && (/^##[[:space:]]/ || /^\[[^]]+\]:[[:space:]]/) { print section; print ""; done = 1 }
        { print }
        END { if (!done) { print ""; print section } }
    ' "$LOG" | awk -v ver="$version" '
        # [Unreleased]: URL/compare/PREV...HEAD
        !done && /^\[[Uu]nreleased\]:[[:space:]]+[^[:space:]]+\/compare\/[^[:space:]]+\.\.\.HEAD[[:space:]]*$/ {
            label = $0; sub(/\].*$/, "]", label)
            url = $0; sub(/^[^]]*\]:[[:space:]]+/, "", url); sub(/[[:space:]]+$/, "", url)
            base = url; sub(/\/compare\/.*$/, "", base)
            prev = url; sub(/^.*\/compare\//, "", prev); sub(/\.\.\.HEAD$/, "", prev)
            pre = (prev ~ /^v/) ? "v" : ""
            print label ": " base "/compare/" pre ver "...HEAD"
            print "[" ver "]: " base "/compare/" prev "..." pre ver
            done = 1; next
        }
        # No Unreleased link: above the newest version link, comparing from it.
        !done && !unrel && /^\[[0-9][^]]*\]:[[:space:]]+[^[:space:]]+\/compare\/[^[:space:]]+\.\.\.[^[:space:]]+[[:space:]]*$/ {
            url = $0; sub(/^[^]]*\]:[[:space:]]+/, "", url); sub(/[[:space:]]+$/, "", url)
            base = url; sub(/\/compare\/.*$/, "", base)
            prev = url; sub(/^.*\.\.\./, "", prev)
            pre = (prev ~ /^v/) ? "v" : ""
            print "[" ver "]: " base "/compare/" prev "..." pre ver
            done = 1
        }
        /^\[[Uu]nreleased\]:/ { unrel = 1 }
        { print }
    ' >"$tmp"
    cat "$tmp" >"$LOG"
fi

# The fragments move: tracked ones leave the index, so the release commit
# deletes them; untracked ones are simply removed.
for f in "${fragments[@]}"; do
    if git -C "$ROOT" ls-files --error-unmatch -- "$f" >/dev/null 2>&1; then
        git -C "$ROOT" rm --quiet -- "$f"
    else
        rm -f -- "$f"
    fi
done

if [ "$append" = "true" ]; then
    echo "changelog-gather: ${#fragments[@]} fragment(s) added to the $version section of CHANGELOG.md ($skipped entries it already had were skipped)"
else
    echo "changelog-gather: ${#fragments[@]} fragment(s) gathered into CHANGELOG.md as $version ($date)"
fi
