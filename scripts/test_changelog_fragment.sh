#!/usr/bin/env bash
# Tests for changelog-fragment.sh, in throwaway repositories with stand-in agents. It needs no
# network and no real agent, so CI can run it.
set -u

SCRIPT="$(cd "$(dirname "$0")" && pwd)/changelog-fragment.sh"
BASE="$(mktemp -d "${TMPDIR:-/tmp}/fragment-test.XXXXXX")"
trap 'rm -rf "$BASE"' EXIT
GOOD='- **The thing now works.** You no longer have to work around it.'
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

# A stand-in agent that logs how it was called and what it was sent, then gives its answer.
agent() {
    local name="$1" answer="$2" pause="${3:-0}"
    cat >"$BIN/$name" <<EOF
#!/bin/sh
echo "\$*" >>"$LOG/$name.args"
cat >"$LOG/$name.prompt"
[ "$pause" = "0" ] || sleep "$pause"
printf '%s\n' '$answer'
EOF
    chmod +x "$BIN/$name"
}

# A new repository with the hook installed, on the given branch, and no agents yet.
start() {
    local branch="$1" with_dir="${2:-yes}"
    REPO="$(mktemp -d "$BASE/repo.XXXXXX")"
    BIN="$(mktemp -d "$BASE/bin.XXXXXX")"
    LOG="$(mktemp -d "$BASE/log.XXXXXX")"
    cd "$REPO" || exit 1
    git init -q
    git config user.email test@example.com
    git config user.name test
    git config core.hooksPath .git/hooks
    echo one >app.txt
    [ "$with_dir" = "yes" ] && mkdir changelog.d && echo notes >changelog.d/README.md
    git add -A
    git commit -q -m first --no-verify
    git switch -q -c "$branch"
    cp "$SCRIPT" .git/hooks/pre-commit
    chmod +x .git/hooks/pre-commit
    echo two >>app.txt
    git add app.txt
}

# Commits with only the stand-ins and the system's tools available. Prints the exit code.
commit() {
    PATH="$BIN:/usr/bin:/bin" CHANGELOG_CODEX_PLACES="/nonexistent/codex" \
        CHANGELOG_AGENT_SECONDS="${SECONDS_ALLOWED:-5}" \
        git commit -q -m "a change" >"$LOG/out" 2>"$LOG/err"
    echo $?
}

in_commit() { git show --name-only --format= HEAD | grep -c "$1"; }

# A feature: Codex answers, the heading comes from the branch, and it is in this commit.
start feat/new-thing
agent codex "$GOOD"
check "feature commits" "$(commit)" 0
check "feature fragment is in the commit" "$(in_commit changelog.d/feat-new-thing.md)" 1
check "feature heading" "$(sed -n 1p changelog.d/feat-new-thing.md)" "### Added"
check "feature entry" "$(sed -n 3p changelog.d/feat-new-thing.md)" "$GOOD"
check "nothing left staged" "$(git diff --cached --name-only | wc -l | tr -d ' ')" 0

# Codex gets a read-only sandbox and the right model.
args="$(cat "$LOG/codex.args")"
check "codex is read-only" "$(echo "$args" | grep -c -- '--sandbox read-only')" 1
check "codex model" "$(echo "$args" | grep -c -- '-m gpt-6-luna')" 1
check "codex effort" "$(echo "$args" | grep -c 'model_reasoning_effort=medium')" 1
check "the prompt holds the diff" "$(grep -c '^+two' "$LOG/codex.prompt")" 1
check "the prompt calls the diff data" "$(grep -c 'not instructions' "$LOG/codex.prompt")" 1

# A fix gets the other heading.
start fix/broken-thing
agent codex "$GOOD"
check "fix commits" "$(commit)" 0
check "fix heading" "$(sed -n 1p changelog.d/fix-broken-thing.md)" "### Fixed"

# Other branches and main need no fragment, and no agent is called.
for branch in chore/tidy docs/words test/more ci/step; do
    start "$branch"
    agent codex "$GOOD"
    check "$branch commits" "$(commit)" 0
    check "$branch writes nothing" "$(ls changelog.d | wc -l | tr -d ' ')" 1
    check "$branch calls no agent" "$([ -e "$LOG/codex.args" ] && echo called || echo no)" no
done

# A repository without changelog.d is left alone, so the hook can later go everywhere.
start feat/elsewhere no
agent codex "$GOOD"
check "no changelog.d commits" "$(commit)" 0
check "no changelog.d calls no agent" "$([ -e "$LOG/codex.args" ] && echo called || echo no)" no

# A file written by hand always wins.
start feat/by-hand
agent codex "$GOOD"
printf '### Added\n\n- **By hand.** Mine.\n' >changelog.d/feat-by-hand.md
git add changelog.d/feat-by-hand.md
check "hand-written commits" "$(commit)" 0
check "hand-written kept" "$(sed -n 3p changelog.d/feat-by-hand.md)" "- **By hand.** Mine."
check "hand-written calls no agent" "$([ -e "$LOG/codex.args" ] && echo called || echo no)" no

# A later commit on the branch keeps what is there.
start feat/two-commits
agent codex "$GOOD"
commit >/dev/null
rm -f "$LOG/codex.args"
echo three >>app.txt
git add app.txt
check "second commit works" "$(commit)" 0
check "second commit calls no agent" "$([ -e "$LOG/codex.args" ] && echo called || echo no)" no

# Only files a user would not read about, so there is nothing to describe.
start feat/lock-only
git restore --staged app.txt
git checkout -q -- app.txt
echo lock >Cargo.lock
git add Cargo.lock
agent codex "$GOOD"
check "lockfile commits" "$(commit)" 0
check "lockfile calls no agent" "$([ -e "$LOG/codex.args" ] && echo called || echo no)" no

# Replayed work was described where it was first committed.
start feat/replay
agent codex "$GOOD"
mkdir .git/rebase-merge
PATH="$BIN:/usr/bin:/bin" bash "$SCRIPT" >/dev/null 2>&1
check "a rebase writes nothing" "$([ -e changelog.d/feat-replay.md ] && echo wrote || echo no)" no
rmdir .git/rebase-merge

# Fallbacks. Each bad answer from Codex falls through to Claude.
for bad in "" "Here is the entry you asked for" "- plain with no bold" "### Added"; do
    start feat/fallback
    agent codex "$bad"
    agent claude "$GOOD"
    check "fallback from [$bad] commits" "$(commit)" 0
    check "fallback from [$bad] entry" "$(sed -n 3p changelog.d/feat-fallback.md)" "$GOOD"
done

# Two lines are refused, because the agent is meant to return one.
start feat/two-lines
agent codex "$GOOD
$GOOD"
agent claude "$GOOD"
check "two lines falls through" "$(commit)" 0
check "two lines entry count" "$(grep -c '^- ' changelog.d/feat-two-lines.md)" 1
check "two lines used claude" "$(grep -c 'claude wrote' "$LOG/err")" 1

# Claude runs with no tools, on the right model.
args="$(cat "$LOG/claude.args")"
check "claude model" "$(echo "$args" | grep -c -- '--model claude-haiku-5-5')" 1
check "claude effort" "$(echo "$args" | grep -c -- '--effort medium')" 1
check "claude has no tools" "$(echo "$args" | grep -c -- '--tools')" 1

# Codex not installed at all.
start feat/no-codex
agent claude "$GOOD"
check "no codex commits" "$(commit)" 0
check "no codex entry" "$(sed -n 3p changelog.d/feat-no-codex.md)" "$GOOD"

# A hung agent cannot freeze a commit.
start feat/hung
agent codex "$GOOD" 30
agent claude "$GOOD"
began="$(date +%s)"
# The limit also applies to the agent that answers, so it must allow for a busy machine.
SECONDS_ALLOWED=3
check "hung agent commits" "$(commit)" 0
SECONDS_ALLOWED=5
check "hung agent was stopped" "$([ $(($(date +%s) - began)) -lt 15 ] && echo quick || echo slow)" quick
check "hung agent fell through" "$(grep -c 'claude wrote' "$LOG/err")" 1

# Every agent failing stops the commit and says what to write.
start feat/all-fail
agent codex ""
agent claude ""
before="$(git rev-parse HEAD)"
check "all failing stops the commit" "$(commit)" 1
check "all failing commits nothing" "$(git rev-parse HEAD)" "$before"
check "all failing writes nothing" "$([ -e changelog.d/feat-all-fail.md ] && echo wrote || echo no)" no
check "all failing names the file" "$(grep -c 'changelog.d/feat-all-fail.md' "$LOG/err")" 1

# No agents installed behaves the same, and the hand-written way out works.
start feat/none
check "no agents stops the commit" "$(commit)" 1
printf '### Added\n\n- **By hand.** Mine.\n' >changelog.d/feat-none.md
git add changelog.d/feat-none.md
check "the way out works" "$(commit)" 0

# CHANGELOG.md only changes in a release.
start chore/tidy
echo direct >CHANGELOG.md
git add CHANGELOG.md
check "a direct edit is refused" "$(commit)" 1
check "it says where the entry goes" "$(grep -c 'changelog.d/chore-tidy.md' "$LOG/err")" 1
check "it names the release script" "$(grep -c 'scripts/create-release' "$LOG/err")" 1
start feat/tidy
agent codex "$GOOD"
echo direct >CHANGELOG.md
git add CHANGELOG.md
check "a direct edit is refused on a feature" "$(commit)" 1
start release/v1.0.0
echo gathered >CHANGELOG.md
git add CHANGELOG.md
check "a release may edit it" "$(commit)" 0

# A very large change is cut down before it leaves the machine.
start feat/huge
agent codex "$GOOD"
head -c 300000 /dev/zero | tr '\0' 'x' >big.txt
git add big.txt
CHANGELOG_MAX_DIFF_BYTES=5000 commit >/dev/null
size="$(wc -c <"$LOG/codex.prompt" | tr -d ' ')"
check "large diff is capped" "$([ "$size" -lt 8000 ] && echo capped || echo "$size")" capped

# A commit's type must agree with its branch.
GUARD="$(dirname "$SCRIPT")/commit-type-guard.sh"
start fix/thing
message() { printf '%s\n' "$1" >"$LOG/msg"; bash "$GUARD" "$LOG/msg" 2>"$LOG/guard"; echo $?; }
check "feat on a fix branch is refused" "$(message 'feat: not a fix')" 1
check "it says what to do" "$(grep -c 'Pick the one that is' "$LOG/guard")" 1
check "fix on a fix branch" "$(message 'fix: a repair')" 0
check "a scope is understood" "$(message 'feat(ui): not a fix')" 1
check "docs on a fix branch" "$(message 'docs: words')" 0
check "no type at all" "$(message 'a plain message')" 0
start feat/thing
check "fix on a feature branch is refused" "$(message 'fix: not a feature')" 1
check "feat on a feature branch" "$(message 'feat: a thing')" 0
start chore/thing
check "any type on another branch" "$(message 'feat: a thing')" 0

echo "changelog-fragment: $passed passed, $failed failed"
[ "$failed" -eq 0 ]
