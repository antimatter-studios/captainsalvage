#!/usr/bin/env bash
# Tests for create-release, in throwaway repositories with a local origin and a stand-in gh. It
# needs no network, so CI can run it. GUARD may name github-guard's git-changelog.sh, to check
# that what this tags also passes the real guard.
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
BASE="$(mktemp -d "${TMPDIR:-/tmp}/release-test.XXXXXX")"
trap 'rm -rf "$BASE"' EXIT
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

BIN="$BASE/bin"
mkdir -p "$BIN"
cat >"$BIN/gh" <<'EOF'
#!/bin/sh
echo "$*" >>"$GH_LOG"
EOF
chmod +x "$BIN/gh"

# A repository with one release, v0.1.0, and an origin to push to.
start() {
    local where
    where="$(mktemp -d "$BASE/case.XXXXXX")"
    git init -q --bare -b main "$where/origin.git"
    git clone -q "$where/origin.git" "$where/repo" 2>/dev/null
    cd "$where/repo" || exit 1
    git switch -q -c main 2>/dev/null || git switch -q main
    git config user.email test@example.com
    git config user.name test
    git config core.hooksPath .git/hooks
    export GH_LOG="$where/gh.log"
    mkdir -p scripts changelog.d crates/salvage-core src-tauri
    cp "$HERE/create-release" "$HERE/release.py" "$HERE/changelog-check.sh" "$HERE/changelog-gather.sh" scripts/
    echo notes >changelog.d/README.md
    printf '# Changelog\n\n## [Unreleased]\n\n## [0.1.0] — 2026-01-01\n\n### Added\n\n- **First.** One.\n' >CHANGELOG.md
    printf 'Top\n\n<!-- changelog:start -->\n\n### v0.1.0 — 2026-01-01\n\n<!-- changelog:end -->\n\nBottom\n' >README.md
    printf '[package]\nversion = "0.1.0"\n' >crates/salvage-core/Cargo.toml
    printf '[package]\nversion = "0.1.0"\n' >src-tauri/Cargo.toml
    printf '{\n  "version": "0.1.0"\n}\n' >src-tauri/tauri.conf.json
    printf '{\n  "version": "0.1.0"\n}\n' >package.json
    printf 'name = "salvage-core"\nversion = "0.1.0"\n\nname = "captain-salvage"\nversion = "0.1.0"\n' >Cargo.lock
    printf '0.1.0\n' >VERSION
    printf '{\n  "version": "0.1.0",\n  "packages": { "": { "version": "0.1.0" } }\n}\n' >package-lock.json
    git add -A
    git commit -q -m "chore(release): v0.1.0 (#1)"
    git tag -a v0.1.0 -m first
    git push -q origin main v0.1.0
}

# A merged pull request, with its fragment if it was given an entry.
merged() {
    local subject="$1" name="${2:-}" heading="${3:-Added}"
    echo "$subject" >>work.txt
    git add work.txt
    if [ -n "$name" ]; then
        printf '### %s\n\n- **%s.** It helps.\n' "$heading" "$name" >"changelog.d/$name.md"
        git add "changelog.d/$name.md"
    fi
    git commit -q -m "$subject"
    git push -q origin main
}

run() {
    PATH="$BIN:$PATH" scripts/create-release "$@" </dev/null >"$BASE/out" 2>"$BASE/err"
    echo $?
}
said() { cat "$BASE/out" "$BASE/err" | grep -c -- "$1"; }

# A feature raises the minor. A dry run shows it all and changes nothing.
start
merged "feat: a new thing (#2)" new-thing
check "dry run works" "$(run --dry-run)" 0
check "dry run names the version" "$(said 'Release v0.2.0')" 1
check "dry run lists the pull request" "$(said 'a new thing (#2)')" 1
check "dry run shows the entry" "$(said 'new-thing.')" 1
check "dry run stays on main" "$(git symbolic-ref --short HEAD)" main
check "dry run opens nothing" "$(cat "$GH_LOG" 2>/dev/null | grep -c 'pr create')" 0
check "dry run keeps the fragment" "$([ -e changelog.d/new-thing.md ] && echo kept || echo gone)" kept

# With no terminal it stops for a yes, and has changed nothing.
check "no yes stops it" "$(run)" 3
check "it says who to ask" "$(said 'Show the list above')" 1
check "stopped on main" "$(git symbolic-ref --short HEAD)" main

# Tagging before the pull request has merged is refused.
check "early tag refused" "$(run --tag v0.2.0 --contents-approved)" 1
check "early tag makes no tag" "$(git tag -l v0.2.0 | wc -l | tr -d ' ')" 0

# The cut itself.
check "cut works" "$(run --contents-approved)" 0
check "cut is on its branch" "$(git symbolic-ref --short HEAD)" release/v0.2.0
check "cut commit" "$(git log -1 --format=%s)" "chore(release): v0.2.0"
check "changelog has the section" "$(grep -c '^## \[0.2.0\]' CHANGELOG.md)" 1
check "changelog has the entry" "$(grep -c 'new-thing' CHANGELOG.md)" 1
check "unreleased is left empty" "$(bash scripts/changelog-check.sh >/dev/null 2>&1; echo $?)" 0
check "fragment is gone" "$([ -e changelog.d/new-thing.md ] && echo kept || echo gone)" gone
check "notes file stays" "$([ -e changelog.d/README.md ] && echo kept || echo gone)" kept
check "readme has the version" "$(grep -c '^### v0.2.0' README.md)" 1
check "readme keeps its surroundings" "$(grep -c '^Bottom$' README.md)" 1
for file in VERSION crates/salvage-core/Cargo.toml src-tauri/Cargo.toml src-tauri/tauri.conf.json package.json; do
    check "$file is bumped" "$(grep -c '0\.2\.0' "$file")" 1
done
check "nothing left over" "$(git status --porcelain | wc -l | tr -d ' ')" 0
check "pull request opened" "$(grep -c 'pr create --base main --head release/v0.2.0' "$GH_LOG")" 1
check "pull request carries the entries" "$(grep -c 'new-thing' "$GH_LOG")" 1
check "branch pushed" "$(git ls-remote origin release/v0.2.0 | wc -l | tr -d ' ')" 1
check "nothing tagged yet" "$(git ls-remote origin 'refs/tags/v0.2.0' | wc -l | tr -d ' ')" 0

# A second cut while that branch exists is refused.
git switch -q main
check "second cut refused" "$(run --contents-approved)" 1

# Once merged, the tag stage stops for its own yes, then tags and pushes.
git push -q origin release/v0.2.0:main
git pull -q --ff-only origin main
check "tag needs a yes" "$(run --tag v0.2.0)" 3
check "tag shows the entries" "$(said 'new-thing')" 1
check "no yes makes no tag" "$(git tag -l v0.2.0 | wc -l | tr -d ' ')" 0

# A guard refusing the push leaves nothing behind, so the next try can work.
printf '#!/bin/sh\necho refused by a guard >&2\nexit 1\n' >.git/hooks/pre-push
chmod +x .git/hooks/pre-push
check "a refused push fails" "$(run --tag v0.2.0 --contents-approved)" 1
check "it says what happened" "$(said 'the push was refused')" 1
check "a refused push leaves no tag" "$(git tag -l v0.2.0 | wc -l | tr -d ' ')" 0
check "nor on origin" "$(git ls-remote origin 'refs/tags/v0.2.0' | wc -l | tr -d ' ')" 0
chmod -x .git/hooks/pre-push

check "tag works" "$(run --tag v0.2.0 --contents-approved)" 0
check "tag is on origin" "$(git ls-remote origin 'refs/tags/v0.2.0' | wc -l | tr -d ' ')" 1
check "tag is annotated" "$(git cat-file -t v0.2.0)" tag
check "tag is on the release commit" "$(git log -1 --format=%s v0.2.0)" "chore(release): v0.2.0"
if [ -n "${GUARD:-}" ] && [ -x "$GUARD" ]; then
    printf 'refs/tags/v0.2.0 %s refs/tags/v0.2.0 0000000000000000000000000000000000000000\n' "$(git rev-parse 'v0.2.0^{commit}')" |
        "$GUARD" origin origin >/dev/null 2>"$BASE/guard"
    check "the real guard accepts the tag" "$(grep -c 'git-changelog:' "$BASE/guard")" 0
fi
check "a second tag is refused" "$(run --tag v0.2.0 --contents-approved)" 1

# A fix raises the patch.
start
merged "fix: a repair (#2)" a-repair Fixed
run --contents-approved >/dev/null
check "a fix is a patch" "$(git symbolic-ref --short HEAD)" release/v0.1.1

# A feature beats a fix.
start
merged "fix: a repair (#2)" a-repair Fixed
merged "feat: a new thing (#3)" new-thing
run --contents-approved >/dev/null
check "a feature beats a fix" "$(git symbolic-ref --short HEAD)" release/v0.2.0

# A version can be named, and that is the way to 1.0.0.
start
merged "feat: a new thing (#2)" new-thing
run 1.0.0 --contents-approved >/dev/null
check "a named version" "$(git symbolic-ref --short HEAD)" release/v1.0.0

# Nothing that needs releasing.
start
merged "docs: some words (#2)"
check "nothing to release is refused" "$(run --contents-approved)" 1
check "it says why" "$(said 'Nothing to release')" 1

# A release with no entries has nothing to say.
start
merged "feat: forgot the entry (#2)"
check "no fragments is refused" "$(run --contents-approved)" 1
check "it says so" "$(said 'no fragments')" 1

# Uncommitted work would not be in the release.
start
merged "feat: a new thing (#2)" new-thing
echo more >>work.txt
check "a dirty tree is refused" "$(run --contents-approved)" 1
check "it says so too" "$(said 'uncommitted')" 1
git checkout -q -- work.txt

# Everything wrong is reported at once.
start
merged "feat: a new thing (#2)" new-thing
printf 'not a fragment\n' >changelog.d/broken.md
git add changelog.d/broken.md
git commit -q -m "chore: broke it"
git push -q origin main
check "a malformed fragment is refused" "$(run --contents-approved)" 1
check "it names the check" "$(said 'malformed')" 1

echo "create-release: $passed passed, $failed failed"
[ "$failed" -eq 0 ]
