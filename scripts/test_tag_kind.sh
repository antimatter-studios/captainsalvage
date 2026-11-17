#!/usr/bin/env bash
# Tests for tag-kind: what a tag may look like is decided there, once. Needs no git and no network.
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
KIND="$HERE/tag-kind"
passed=0
failed=0

check() {
    if [ "$2" = "$3" ]; then
        passed=$((passed + 1))
    else
        failed=$((failed + 1))
        echo "FAILED: $1: expected [$3], got [$2]" >&2
    fi
}
# kind TAG: what it prints, then its exit status.
kind() { out="$(bash "$KIND" "$1" 2>"$HERE/../.tag-kind-err.$$")"; code=$?; err="$(cat "$HERE/../.tag-kind-err.$$")"; rm -f "$HERE/../.tag-kind-err.$$"; }

for tag in v0.4.0 v1.0.0 v10.20.30; do
    kind "$tag"; check "$tag is final" "$out/$code" "final/0"
    bash "$KIND" "$tag" --final; check "$tag passes --final" "$?" 0
done
for tag in v0.4.0-rc1 v1.0.0-rc2 v1.0.0-rc10 v1.0.0-rc123; do
    kind "$tag"; check "$tag is a candidate" "$out/$code" "candidate/0"
    bash "$KIND" "$tag" --final; check "$tag fails --final" "$?" 1
done

# Every other shape is an error with a reason, never a default. The reason is the point.
refused() {  # refused TAG PHRASE
    kind "$1"
    check "$1 is refused" "$code" 2
    check "$1 prints nothing on stdout" "$out" ""
    check "$1 says why" "$(printf '%s' "$err" | grep -c -- "$2")" 1
    bash "$KIND" "$1" --final 2>/dev/null; check "$1 does not pass --final" "$?" 2
}
refused v1.0.0-RC1 'must be lowercase'
refused v1.0.0-Rc1 'must be lowercase'
refused v1.0.0-rc 'needs a number'
refused v1.0.0-rc0 'no leading zeros'
refused v1.0.0-rc01 'no leading zeros'
refused v1.0.0-hotfix 'unrecognised suffix'
refused v1.0.0-beta1 'unrecognised suffix'
refused v1.0.0-rc.1 'straight after'
refused v1.0.0-rc-1 'straight after'
refused v1.0.0-rc1x 'straight after'
refused v1.0.0-rc1-rc2 'straight after'
refused 'v1.0.0~rc1' 'a ~ cannot'
refused 1.0.0 'starts with v'
refused v1.0 'exactly three numbers'
refused v1 'exactly three numbers'
refused v1.0.0.0 'exactly three numbers'
refused 'v1.0.0 ' 'exactly three numbers'
refused nightly 'starts with v'

# A final tag is exactly one without a dash, which is what release.py's `git describe --exclude`
# relies on to leave candidates out.
for tag in v0.4.0 v1.0.0-rc1 v2.3.4 v2.3.4-rc9; do
    case "$tag" in *-*) expected=1 ;; *) expected=0 ;; esac
    bash "$KIND" "$tag" --final; check "$tag: final means no dash" "$?" "$expected"
done

# A tag with no argument is a usage error, not an answer.
bash "$KIND" >/dev/null 2>&1; check "no tag is refused" "$?" 1

# github-guard's hooks carry a copy of the candidate shape, because they cannot run a file out of the
# working tree. GUARDS may name its lib/common.sh, to hold the copy to this definition.
if [ -n "${GUARDS:-}" ] && [ -f "$GUARDS" ]; then
    for tag in v0.4.0 v0.4.0-rc1 v0.4.0-rc12 v1.0.0-RC1 v1.0.0-rc v1.0.0-rc0 v1.0.0-rc01 v1.0.0-hotfix v1.0.0-rc.1 v1.0.0-beta1 \
        'v1.0.0~rc1' v1.0 1.0.0-rc1 v1.0.0-rc1x v1.0.0-rc1-rc2; do
        [ "$(bash "$KIND" "$tag" 2>/dev/null)" = candidate ] && expected=0 || expected=1
        bash -c '. "$1"; gg_is_release_candidate "$2"' bash "$GUARDS" "$tag"
        check "the guard and tag-kind agree on $tag" "$?" "$expected"
    done
fi

echo "tag-kind: $passed passed, $failed failed"
[ "$failed" -eq 0 ]
