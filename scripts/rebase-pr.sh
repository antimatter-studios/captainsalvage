#!/bin/sh
# Rebases one PR branch onto main in a scratch worktree, resolving appended-file conflicts,
# checks it, and pushes it with a lease. Usage: scripts/rebase-pr.sh <branch> [<base>]
# Stops without pushing if the TypeScript or clippy check fails.
set -e
BRANCH=$1; BASE=${2:-main}
REPO=$(cd "$(git rev-parse --path-format=absolute --git-common-dir)/.." && pwd); WORK=$REPO/.work
URL=https://github.com/antimatter-studios/captainsalvage.git
cd $REPO
git -c credential.helper='!gh auth git-credential' fetch -q $URL "+$BASE:refs/remotes/origin/$BASE" "+$BRANCH:refs/remotes/origin/$BRANCH"
rm -rf $WORK/rebase; git worktree prune
git worktree add -q -B rebase/work $WORK/rebase "refs/remotes/origin/$BRANCH"
cd $WORK/rebase
ln -sf $REPO/node_modules node_modules
export CARGO_TARGET_DIR=$WORK/target GIT_EDITOR=true
if ! git rebase "refs/remotes/origin/$BASE" >/dev/null 2>&1; then
  while git diff --name-only --diff-filter=U | grep -q .; do
    python3 $REPO/scripts/resolve_appends.py
    git add -A src crates
    git rebase --continue >/dev/null 2>&1 || true
  done
fi
python3 scripts/check_translations.py | tail -1
npx tsc --noEmit || { echo "tsc failed, not pushing"; exit 1; }
cargo clippy -q -p salvage-core --all-targets -- -D warnings || { echo "clippy failed, not pushing"; exit 1; }
echo checks-ok
OLD=$(git rev-parse "refs/remotes/origin/$BRANCH")
git -c credential.helper='!gh auth git-credential' push -q --force-with-lease="refs/heads/$BRANCH:$OLD" $URL "HEAD:refs/heads/$BRANCH"
echo "pushed $BRANCH at $(git rev-parse --short HEAD)"
