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
# Stand-ins for the build tools, which record what they were asked and where.
for tool in npx npm; do
    cat >"$BIN/$tool" <<EOF
#!/bin/sh
echo "$tool \$* | \$(pwd -P) | \${CARGO_TARGET_DIR:-}" >>"\$BUILD_LOG"
EOF
    chmod +x "$BIN/$tool"
done

# cargo is only asked for by the license list, which the stand-in below writes without it.
printf '#!/bin/sh\nexit 0\n' >"$BIN/cargo"
chmod +x "$BIN/cargo"

# A stand-in for scripts/licenses.py, with its two rules that matter here: the README's list names
# the app's own crate with its version, and `current` fails when the list does not say what VERSION
# says. A file named licenses.fail makes `write` fail, as it does when cargo or a package is missing.
cat >"$BASE/licenses.py" <<'EOF'
import sys
from pathlib import Path

root = Path(__file__).resolve().parent.parent
version = (root / "VERSION").read_text().strip()
notices, readme = root / "THIRD-PARTY-LICENSES.md", root / "README.md"
start, end = "<!-- dependencies:start -->", "<!-- dependencies:end -->"
wanted_notices = f"captainsalvage {version}\n"
text = readme.read_text()
wanted_readme = text[: text.index(start)] + f"{start}\n| `captainsalvage` | {version} |\n{end}" + text[text.index(end) + len(end):]
if sys.argv[1] == "write":
    if (root / "licenses.fail").exists():
        sys.exit("licenses: stand-in told to fail")
    notices.write_text(wanted_notices)
    readme.write_text(wanted_readme)
elif sys.argv[1] == "current":
    sys.exit(0 if notices.read_text() == wanted_notices and readme.read_text() == wanted_readme else 1)
EOF

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
    export GH_LOG="$where/gh.log" BUILD_LOG="$where/build.log"
    mkdir -p scripts changelog.d crates/captainsalvage src-tauri
    cp "$HERE/create-release" "$HERE/release.py" "$HERE/tag-kind" "$HERE/changelog-check.sh" "$HERE/changelog-gather.sh" scripts/
    cp "$BASE/licenses.py" scripts/licenses.py
    mkdir -p node_modules
    mkdir -p macos/finder
    printf '#!/bin/sh\nexit 0\n' >macos/finder/package.sh
    chmod +x macos/finder/package.sh
    echo notes >changelog.d/README.md
    printf '# Changelog\n\n## [Unreleased]\n\n## [0.1.0] — 2026-01-01\n\n### Added\n\n- **First.** One.\n' >CHANGELOG.md
    # shellcheck disable=SC2016
    printf 'Top\n\n<!-- changelog:start -->\n\n### v0.1.0 — 2026-01-01\n\n<!-- changelog:end -->\n\n<!-- dependencies:start -->\n| `captainsalvage` | 0.1.0 |\n<!-- dependencies:end -->\n\nBottom\n' >README.md
    printf 'captainsalvage 0.1.0\n' >THIRD-PARTY-LICENSES.md
    printf '[package]\nversion = "0.1.0"\n' >crates/captainsalvage/Cargo.toml
    printf '[package]\nversion = "0.1.0"\n' >src-tauri/Cargo.toml
    printf '{\n  "version": "0.1.0"\n}\n' >src-tauri/tauri.conf.json
    printf '{\n  "version": "0.1.0"\n}\n' >package.json
    printf 'name = "captainsalvage"\nversion = "0.1.0"\n\nname = "captain-salvage"\nversion = "0.1.0"\n' >Cargo.lock
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
for file in VERSION crates/captainsalvage/Cargo.toml src-tauri/Cargo.toml src-tauri/tauri.conf.json package.json; do
    check "$file is bumped" "$(grep -c '0\.2\.0' "$file")" 1
done
check "nothing left over" "$(git status --porcelain | wc -l | tr -d ' ')" 0
# #166: the README's license list names the app's crate with its version, so a release must rewrite it.
check "the license list is current after a release" "$(python3 scripts/licenses.py current; echo $?)" 0
check "the license list is in the release commit" "$(git show HEAD:THIRD-PARTY-LICENSES.md)" "captainsalvage 0.2.0"
# shellcheck disable=SC2016
check "the README's list says the new version" "$(git show HEAD:README.md | grep -c '| `captainsalvage` | 0.2.0 |')" 1
check "pull request opened" "$(grep -c 'pr create --base main --head release/v0.2.0' "$GH_LOG")" 1
check "pull request carries the entries" "$(grep -c 'new-thing' "$GH_LOG")" 1
check "branch pushed" "$(git ls-remote origin release/v0.2.0 | wc -l | tr -d ' ')" 1
check "nothing tagged yet" "$(git ls-remote origin 'refs/tags/v0.2.0' | wc -l | tr -d ' ')" 0

# A release candidate: the release's content under a version that is not released yet.
branch_head="$(git rev-parse HEAD)"
git switch -q main
check "a candidate off the release branch is refused" "$(run --rc)" 1
check "it says to switch" "$(said 'not a release branch')" 1
check "no candidate was tagged off it" "$(git tag -l 'v0.2.0-rc*' | wc -l | tr -d ' ')" 0
git switch -q release/v0.2.0
echo more >>work.txt
check "a candidate with a dirty tree is refused" "$(run --rc)" 1
check "it names the dirty tree" "$(said 'uncommitted')" 1
git checkout -q -- work.txt
echo more >>work.txt
git add work.txt
git commit -q -m "chore: a commit nobody pushed"
check "a candidate of an unpushed branch is refused" "$(run --rc)" 1
check "it says to push" "$(said 'is not what is on GitHub')" 1
git reset -q --hard "$branch_head"
check "a version and --rc together are refused" "$(run --rc 1.0.0)" 2
check "--build without --rc is refused" "$(run --build)" 2
check "a dry run changes nothing" "$(run --rc --dry-run)" 0
check "a dry run names the tag" "$(said 'would tag v0.2.0-rc1')" 1
check "a dry run tags nothing" "$(git tag -l 'v0.2.0-rc*' | wc -l | tr -d ' ')" 0

check "the first candidate works" "$(run --rc)" 0
check "it is rc.1" "$(git ls-remote origin 'refs/tags/v0.2.0-rc1' | wc -l | tr -d ' ')" 1
check "the tag is annotated" "$(git cat-file -t v0.2.0-rc1)" tag
check "the candidate's commit follows the branch head" "$(git rev-parse 'v0.2.0-rc1^')" "$branch_head"
check "the candidate's commit says what it is" "$(git log -1 --format=%s v0.2.0-rc1)" "chore(release): v0.2.0-rc1"
for file in VERSION crates/captainsalvage/Cargo.toml src-tauri/Cargo.toml src-tauri/tauri.conf.json package.json Cargo.lock package-lock.json; do
    check "$file says the candidate" "$(git show "v0.2.0-rc1:$file" | grep -c '0\.2\.0-rc1')" "$(git show "$branch_head:$file" | grep -c '0\.2\.0')"
done
check "only the version files differ" "$(git diff --name-only "$branch_head" v0.2.0-rc1 | wc -l | tr -d ' ')" 7
check "the notes are the release's own" "$(git diff --name-only "$branch_head" v0.2.0-rc1 | grep -c 'CHANGELOG\|README')" 0
check "the release's changelog is at the tag" "$(git show v0.2.0-rc1:CHANGELOG.md | grep -c '^## \[0.2.0\]')" 1
check "the branch still says the release" "$(cat VERSION)" 0.2.0
check "the checkout did not move" "$(git symbolic-ref --short HEAD)" release/v0.2.0
check "the checkout is the branch head" "$(git rev-parse HEAD)" "$branch_head"
check "the checkout is clean" "$(git status --porcelain | wc -l | tr -d ' ')" 0
check "the branch on origin did not move" "$(git rev-parse origin/release/v0.2.0)" "$branch_head"
check "the candidate is not on the branch" "$(git branch -a --contains v0.2.0-rc1 | wc -l | tr -d ' ')" 0
check "it leaves no worktree" "$(git worktree list | wc -l | tr -d ' ')" 1
check "it says how to build" "$(said 'release.py install user')" 1
check "a candidate opens no pull request and no release" "$(grep -c 'release create\|pr create --base main --head release/v0.2.0-rc' "$GH_LOG")" 0
if [ -n "${GUARD:-}" ] && [ -x "$GUARD" ]; then
    printf 'refs/tags/v0.2.0-rc1 %s refs/tags/v0.2.0-rc1 0000000000000000000000000000000000000000\n' "$(git rev-parse 'v0.2.0-rc1^{commit}')" |
        "$GUARD" origin origin >/dev/null 2>"$BASE/guard"
    check "the real guard accepts the candidate" "$(grep -c 'git-changelog:' "$BASE/guard")" 0
fi

# A second cut while that branch exists is refused.
git switch -q main
check "second cut refused" "$(run --contents-approved)" 1

# Once merged, the tag stage stops for its own yes, then tags and pushes.
git push -q origin release/v0.2.0:main
git pull -q --ff-only origin main
# A tag that is not a release is refused by name, before anything is checked.
check "a candidate is not a release tag" "$(run --tag v0.2.0-rc1)" 2
check "it says to use --rc" "$(said 'Run --rc')" 1
check "a capital RC is refused" "$(run --tag v0.2.0-RC1)" 2
check "it names the mistake" "$(said 'must be lowercase')" 1
check "an unknown suffix is refused" "$(run --tag v0.2.0-hotfix)" 2
check "it says what is accepted" "$(said 'Accepted shapes')" 1
check "a candidate is not a version to cut" "$(run v0.2.0-rc1 --contents-approved)" 2
check "none of those made a tag" "$(git tag -l 'v0.2.0' 'v0.2.0-RC1' 'v0.2.0-hotfix' | wc -l | tr -d ' ')" 0

# Other releases' candidates, and names that only look like one, are not this release's to retire.
git tag v0.3.0-rc1 HEAD
git tag v0.2.0-rc01 HEAD
git tag v0.2.0-rc HEAD
git push -q origin v0.3.0-rc1 v0.2.0-rc01 v0.2.0-rc
check "tag needs a yes" "$(run --tag v0.2.0)" 3
check "it lists the candidates it will retire" "$(said 'retired once the tag is pushed: v0.2.0-rc1 $')" 1
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
check "a refused push keeps the candidates" "$(git ls-remote origin 'refs/tags/v0.2.0-rc1' | wc -l | tr -d ' ')" 1

# One candidate that origin will not let go is reported and does not fail the release.
git tag v0.2.0-rc2 HEAD
git push -q origin v0.2.0-rc2
cat >../origin.git/hooks/update <<'HOOK'
#!/bin/sh
[ "$1" = refs/tags/v0.2.0-rc2 ] && [ "$3" = 0000000000000000000000000000000000000000 ] && { echo no >&2; exit 1; }
exit 0
HOOK
chmod +x ../origin.git/hooks/update

check "tag works" "$(run --tag v0.2.0 --contents-approved)" 0
cat "$BASE/out" "$BASE/err" >"$BASE/tagged"
check "tag is on origin" "$(git ls-remote origin 'refs/tags/v0.2.0' | wc -l | tr -d ' ')" 1
check "tag is annotated" "$(git cat-file -t v0.2.0)" tag
check "tag is on the release commit" "$(git log -1 --format=%s v0.2.0)" "chore(release): v0.2.0"
if [ -n "${GUARD:-}" ] && [ -x "$GUARD" ]; then
    printf 'refs/tags/v0.2.0 %s refs/tags/v0.2.0 0000000000000000000000000000000000000000\n' "$(git rev-parse 'v0.2.0^{commit}')" |
        "$GUARD" origin origin >/dev/null 2>"$BASE/guard"
    check "the real guard accepts the tag" "$(grep -c 'git-changelog:' "$BASE/guard")" 0
fi
check "a second tag is refused" "$(run --tag v0.2.0 --contents-approved)" 1
check "the release's candidate is retired on origin" "$(git ls-remote origin 'refs/tags/v0.2.0-rc1' | wc -l | tr -d ' ')" 0
check "and here" "$(git tag -l v0.2.0-rc1 | wc -l | tr -d ' ')" 0
check "it says how to put it back" "$(grep -c 'v0.2.0-rc1 .*gone (put it back: git push origin [0-9a-f]*:refs/tags/v0.2.0-rc1)' "$BASE/tagged")" 1
check "it says which one it could not delete" "$(grep -c 'v0.2.0-rc2 .*COULD NOT BE DELETED' "$BASE/tagged")" 1
check "a candidate origin keeps is reported, not fatal" "$(git ls-remote origin 'refs/tags/v0.2.0-rc2' | wc -l | tr -d ' ')" 1
check "another release's candidate stays" "$(git ls-remote origin 'refs/tags/v0.3.0-rc1' | wc -l | tr -d ' ')" 1
check "look-alikes stay" "$(git ls-remote origin 'refs/tags/v0.2.0-rc01' 'refs/tags/v0.2.0-rc' | wc -l | tr -d ' ')" 2
check "the release itself stays" "$(git ls-remote origin 'refs/tags/v0.2.0' | wc -l | tr -d ' ')" 1

# The license list is read from the installed npm packages, so a cut without them stops before it changes anything.
start
merged "feat: a new thing (#2)" new-thing
rmdir node_modules
check "a cut without node_modules is refused" "$(run --contents-approved)" 1
check "it says to run npm ci" "$(said 'Run: npm ci')" 1
check "it made no branch" "$(git branch --list 'release/*' | wc -l | tr -d ' ')" 0
check "it stayed on main" "$(git symbolic-ref --short HEAD)" main
check "it kept the fragment" "$([ -e changelog.d/new-thing.md ] && echo kept || echo gone)" kept
check "even a dry run says so" "$(run --dry-run)" 1

# When the license list cannot be written after the branch exists, the checkout is put back.
start
merged "feat: a new thing (#2)" new-thing
touch licenses.fail
check "a failing license list fails the cut" "$(run --contents-approved)" 1
check "it says the release is not made" "$(said 'preparing the release failed')" 1
check "it is back on main" "$(git symbolic-ref --short HEAD)" main
check "the release branch is gone" "$(git branch --list 'release/*' | wc -l | tr -d ' ')" 0
check "the tracked files are as they were" "$(git diff HEAD --stat | wc -l | tr -d ' ')" 0
check "the fragment is back" "$([ -e changelog.d/new-thing.md ] && echo kept || echo gone)" kept
check "no pull request was opened" "$(grep -c 'pr create' "$GH_LOG" 2>/dev/null)" 0
rm licenses.fail

# A retired word outside Removed stops the cut, unless it is allowed. Removed may name what is gone.
start
printf '### Changed\n\n- **The canvas is now a Disk Rebuild.** Same thing.\n' >changelog.d/rename.md
git add changelog.d/rename.md
git commit -q -m "feat: rename it (#2)"
git push -q origin main
check "a retired word is refused" "$(run --contents-approved)" 1
check "it names the word and the file" "$(said 'changelog.d/rename.md:3: "canvas" outside Removed')" 1
check "it says how to allow it" "$(said -- '--allow-vocabulary')" 1
check "it made no branch for it" "$(git branch --list 'release/*' | wc -l | tr -d ' ')" 0
check "a dry run is refused too" "$(run --dry-run)" 1
check "allowing the vocabulary lets it through" "$(run --contents-approved --allow-vocabulary)" 0
check "it cut the release" "$(git symbolic-ref --short HEAD)" release/v0.2.0
start
printf '### Removed\n\n- **The canvas and the basket are gone.** Use a Disk Rebuild and the treasure list.\n' >changelog.d/gone.md
git add changelog.d/gone.md
git commit -q -m "feat: remove the old names (#2)"
git push -q origin main
check "Removed may name retired words" "$(run --contents-approved)" 0
check "a Removed entry cuts the release" "$(git symbolic-ref --short HEAD)" release/v0.2.0

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

# A release branch, as a cut leaves it, for the candidates that follow.
start
merged "feat: a new thing (#2)" new-thing
run --contents-approved >/dev/null
branch_head="$(git rev-parse HEAD)"
check "the first candidate works" "$(run --rc)" 0
# More commits on the branch make the next candidate, from the new head.
echo again >>work.txt
git add work.txt
git commit -q -m "chore: another change to the release"
git push -q origin release/v0.2.0
branch_head2="$(git rev-parse HEAD)"
check "the second candidate works" "$(run --rc)" 0
check "it is rc.2" "$(git ls-remote origin 'refs/tags/v0.2.0-rc2' | wc -l | tr -d ' ')" 1
check "it follows the new head" "$(git rev-parse 'v0.2.0-rc2^')" "$branch_head2"
check "rc.1 stayed where it was" "$(git rev-parse 'v0.2.0-rc1^')" "$branch_head"
check "rc.2 says rc.2" "$(git show v0.2.0-rc2:VERSION)" 0.2.0-rc2
check "the numbering counts the tags on origin too" "$(git tag -d v0.2.0-rc1 v0.2.0-rc2 >/dev/null; run --rc --dry-run >/dev/null; said 'would tag v0.2.0-rc3')" 1
git fetch -q --tags origin

# A refused push leaves no tag behind.
printf '#!/bin/sh\necho refused by a guard >&2\nexit 1\n' >.git/hooks/pre-push
chmod +x .git/hooks/pre-push
check "a refused candidate fails" "$(run --rc)" 1
check "it says what happened" "$(said 'the push was refused')" 1
check "a refused candidate leaves no tag" "$(git tag -l v0.2.0-rc3 | wc -l | tr -d ' ')" 0
rm .git/hooks/pre-push

# --build builds the tag in a scratch worktree, and leaves this checkout alone.
check "a candidate with --build works" "$(run --rc --build)" 0
check "it is rc.3" "$(git ls-remote origin 'refs/tags/v0.2.0-rc3' | wc -l | tr -d ' ')" 1
check "the build was asked for the candidate's version" "$(grep -c '0\.2\.0-rc3' "$BUILD_LOG")" 1
check "the build ran away from this checkout" "$(grep -c "tauri build.* | $(pwd -P) | " "$BUILD_LOG")" 0
check "the build shares this checkout's target" "$(grep -c " | $(pwd -P)/target$" "$BUILD_LOG")" 1
check "building leaves no worktree" "$(git worktree list | wc -l | tr -d ' ')" 1
check "building leaves it clean" "$(git status --porcelain | wc -l | tr -d ' ')" 0

# Candidates are not releases: they do not move the next version, on or off main.


# An integration branch, integration/rc-vX.Y.Z, collects several pull requests for a candidate.
# VERSION is plain X.Y.Z on it and its head is pushed, so it is cut like a release branch.
start
merged "feat: a new thing (#2)" new-thing
run --contents-approved >/dev/null
git switch -q -c integration/rc-v0.2.0
integration_head="$(git rev-parse HEAD)"
git push -q origin integration/rc-v0.2.0
check "an integration branch is a branch to cut from" "$(run --rc --dry-run)" 0
check "its dry run names the tag" "$(said 'would tag v0.2.0-rc1')" 1
check "an integration candidate works" "$(run --rc)" 0
check "it is rc.1" "$(git ls-remote origin 'refs/tags/v0.2.0-rc1' | wc -l | tr -d ' ')" 1
check "its commit follows the integration head" "$(git rev-parse 'v0.2.0-rc1^')" "$integration_head"
check "it says the candidate" "$(git show v0.2.0-rc1:VERSION)" 0.2.0-rc1
check "the integration branch still says the release" "$(cat VERSION)" 0.2.0
check "the checkout stays on the integration branch" "$(git symbolic-ref --short HEAD)" integration/rc-v0.2.0
check "the integration branch on origin did not move" "$(git rev-parse origin/integration/rc-v0.2.0)" "$integration_head"
check "it tags the candidate of the version in the branch name" "$(git ls-remote --refs origin 'refs/tags/v0.2.0-rc*' | wc -l | tr -d ' ')" 1
# Only the exact name is a release branch: a look-alike is refused, and nothing is tagged.
git switch -q -c integration/rc-0.2.0
git push -q origin integration/rc-0.2.0
check "a look-alike integration branch is refused" "$(run --rc)" 1
check "it says what a release branch is" "$(said 'integration/rc-vX.Y.Z')" 1
git switch -q -c integration/rc-v0.2
git push -q origin integration/rc-v0.2
check "an integration branch without a full version is refused" "$(run --rc)" 1
check "neither made a candidate" "$(git ls-remote --refs origin 'refs/tags/v0.2.0-rc*' | wc -l | tr -d ' ')" 1

# Candidates are not releases: they do not move the next version, on or off main.
start
merged "feat: a new thing (#2)" new-thing
git tag v0.9.0-rc1 main
git push -q origin v0.9.0-rc1
check "a candidate tag on main is not the last release" "$(run --dry-run >/dev/null; said 'Release v0.2.0')" 1

echo "create-release: $passed passed, $failed failed"
[ "$failed" -eq 0 ]
