# Working on Captain Salvage

## Each pull request's title sets the next version

Every merged pull request is released. When a release is prepared, the version comes from the type
at the start of each title since the last one, and nothing else. So before opening a pull request,
assess what it changes for someone using the app, and title it to match.

| Type | Use it when | Version |
|---|---|---|
| `feat:` | The app can do something it could not do before | Minor goes up, patch resets to 0 |
| `fix:` | Something that should already work was wrong, and now it is right | Patch goes up |
| `docs:`, `test:`, `chore:`, `ci:`, `refactor:` | Someone using the app would notice nothing | Patch goes up |

A release is a GitHub tag with its notes, and has no binaries. The `chore(release)` commits that
make a release move nothing. A `feat` or `fix` writes a changelog fragment (below); a pull request of
any other type writes none, and the release lists it itself, under Documentation (`docs:`) or
Maintenance (the rest), so a release of only tests is not empty.

- Judge by the effect on the user, not by the size of the change. A large repair is still a
  `fix:`, and a small new option is still a `feat:`.
- Improving something that already worked is a `feat:`. Only broken behaviour earns a `fix:`.
- If a change does both, split it into two pull requests. If it cannot be split, use `feat:`.
- Say why in the description, so the assessment can be checked. Correct the title before
  merging if the work changed, because the squash commit takes its subject from the title.
- Never set a version by hand. Only the owner can name 1.0.0 or any other version; the number
  otherwise follows the rule above, and `scripts/create-release` is how it is cut.
- **No AI attribution and no co-author trailers**, in commits or in pull request descriptions.

## Changelog entries are fragments

A pull request never edits `CHANGELOG.md`. A `feat` or `fix` branch adds one file,
`changelog.d/<branch>.md`. The pre-commit hook writes the first one from what is staged, using an
agent for the wording, and leaves alone a file you wrote. Read it before opening the pull request,
because it only saw the branch's first commit. Run `scripts/install-hooks.sh` once per clone.

A refused commit says what to fix. Do what it says, and do not use `--no-verify` to get past it.

## The version lives in one file

`VERSION` holds the project's version, and nothing else states it by hand. `scripts/release.py sync`
copies it into every manifest and lock file, and `scripts/release.py check` fails when any of them
differ. CI runs the check on every pull request. Change the version only through the release
tooling, never by editing a manifest.

## Cutting a release

Run `scripts/create-release`. It shows what would go out and stops for a yes, then opens the release
pull request. When that has merged, `scripts/create-release --tag vX.Y.Z` tags `main` and pushes.

- Only the owner decides when to release. Do not start one unless they asked.
- Show them the list it prints, verbatim, and wait for their answer before passing
  `--contents-approved`.
- The tag starts the release pipeline, and the pipeline never creates a tag.
- A bug that was introduced and fixed within the same unreleased version gets no changelog entry. No
  release ever had the bug, so the notes would announce a fix for something nobody ran. Drop the
  fix's fragment, or never write it. The entry for the feature stays, and says how it works now.
- A release is not cut until its window suite has passed on a release candidate (see the sequence
  below). The unit and CI checks do not drive the window, so they cannot stand in for it.

### The sequence, start to finish

1. **`npm ci`.** The release writes the license list, and `scripts/licenses.py` reads each package's own
   license files from `node_modules`. `scripts/create-release` and `release.py prepare` stop, before they
   change anything, when `node_modules` is missing, and say so. It also needs `cargo`.
2. **Cut or prepare.** There are two ways to a release branch, and a candidate is cut from either.
   - *From `main`:* `scripts/create-release` shows the pull requests and entries, stops for the owner's
     yes, then makes `release/vX.Y.Z` with the fragments gathered into `CHANGELOG.md`, the README's list,
     every manifest, and the license list (`THIRD-PARTY-LICENSES.md` and the README's table), and opens
     the pull request. A failure after the branch exists puts the checkout back and deletes the branch.
   - *From an integration branch:* `integration/rc-vX.Y.Z` collects several open pull requests so they are
     tried together before any merges. On it, `python3 scripts/release.py prepare X.Y.Z` does what the cut
     does, in the working tree, and you commit and push it. Its `VERSION` is plain `X.Y.Z`.
3. **More pull requests join the branch.** `prepare` refuses a version whose section exists. When
   fragments remain in `changelog.d/` because a pull request joined after `prepare`, run
   `python3 scripts/release.py prepare --append`. It adds them to the existing section, under the headings
   they use, and keeps the section's date and the compare links. An entry the section already has, word
   for word, is not written twice, and the fragment is removed all the same. Use it only on a branch where
   the version is not released. On `main`, after a release, a fragment is the next release's.
4. **Read the notes.** `python3 scripts/release.py notes-check` flags words the product has retired
   (`canvas`, `basket`, `Inspect raw`, `Rebuild Tree`) and the sentence "not part of this change", which
   promises the opposite of another entry. They may appear only in a `### Removed` entry, or in an entry
   that says it is a rename: a list item containing "is now called", "are now called", "was renamed" or
   "renamed to" (any case), which may name the old word anywhere in that item. The cut refuses while
   a fragment on `main` has one, and `--allow-vocabulary` is for when it is meant. CI only warns. Fix the
   wording in the pull request that wrote it, not by hand in the release commit.
5. **License list.** `prepare` already runs `python3 scripts/licenses.py write`. Run it yourself, then
   `python3 scripts/licenses.py current`, after anything else that changes a version or a dependency. The
   README names this app's own crates with their version, so a stale list fails CI at the license check.
6. **Release candidate.** On the pushed, clean branch, `scripts/create-release --rc` tags `vX.Y.Z-rc<N>`
   (add `--install` to build and install it). The candidate commit is the version stamp: `VERSION`, the
   manifests and the lock files say `X.Y.Z-rc<N>`, and nothing else differs.
7. **Window suite.** Run the suite in `tests/window/` against the installed candidate, one test at a time
   and never beside a build. Read its README first. A candidate that has not passed it is not released.
   More commits on the branch make the next candidate, `rc2`, and the suite runs on that one.
8. **Merge, then tag.** Once the release pull request has merged:
   `git switch main && git pull --ff-only && scripts/create-release --tag vX.Y.Z`. The tag is made on
   `main`, starts the pipeline, and then the release's candidate tags are deleted from origin and from here,
   each with the command that puts it back. Only the owner decides to do any of this.

The shipped app has no test hooks: `ui_eval` and the `SALVAGE_TEST_*` variables exist only under the
Cargo feature `test-hooks`, which `scripts/release.py build` and the release workflow leave off. So a
release candidate's window suite runs against `scripts/build-test-app.sh` built from the same commit,
and `scripts/check-test-hooks.sh absent` on the shipped bundle proves it holds none of them.

### Release candidates, before the release pull request merges

With the release branch `release/vX.Y.Z` checked out, pushed and clean, `scripts/create-release --rc`
tags `vX.Y.Z-rc1`, then `-rc2` after more commits, and so on. The branch may also be
`integration/rc-vX.Y.Z`, which collects several open pull requests so they can be tried together
before any of them merges: its `VERSION` is plain `X.Y.Z` and its head is pushed, and the candidate is
cut from it the same way, in one command. A candidate lets the version be built
and installed here under a real number and a real tag, before anything merges. Add `--build`, or
`--install` to install it for this user too, to do that in a scratch worktree at the tag.

- A candidate is tagged, not cut: no notes, no squash, no pipeline, no GitHub Release, and a build
  never goes on GitHub. The release pull request keeps `VERSION` at X.Y.Z, so what merges is final.
- The tag points at a commit beside the branch head that says `X.Y.Z-rc<N>` in `VERSION` and every
  manifest and lock file, so the build and the app show the candidate's number. It is reachable only
  through the tag. `scripts/tag-kind` says what a tag may look like, once: `vX.Y.Z` is final, must be
  on `main` and is the only one that publishes; `vX.Y.Z-rc<N>` (lowercase, no leading zero) is a
  candidate and may be on any commit. Any other name is refused by name, never taken for a release.
- A release branch cut before this tooling merged needs it first: `scripts/rebase-pr.sh release/vX.Y.Z`.
- Only the owner decides when to release, and a candidate is part of that. Do not tag one unless they
  asked. After the pull request has merged, `scripts/create-release --tag vX.Y.Z` makes the real
  release, then deletes that release's candidate tags from origin and from here. Each deleted tag is
  printed with the command that puts it back.

## Testing the window

`tests/window/` drives the real app window against small synthetic disk images. Read its
[README](tests/window/README.md) before running one: it says how to make the images, how the
harness keeps the test away from your settings and real drives, and what to do when a test
stalls. Run one window test at a time, never beside a build. CI does not run them, so a pull request
that changes how a source opens, the start screen or the harness lists which it ran and each pass
count, and every one of them is run on a release candidate's build (the README says which).

`scripts/rebase-pr.sh <branch>` rebases a pull request onto `main` in `.work/rebase`, keeps both
sides of conflicts where two branches appended to the same file (`scripts/resolve_appends.py`),
and pushes only if the TypeScript and clippy checks pass. Read the result of a Rust conflict by
hand: keeping both sides can join two functions or two tests together.

## Independent oracles

A reader that shares the code it checks proves only that it agrees with itself. NTFS is also
checked against ntfs-3g's tools and the Linux kernel's ntfs3 driver, and FAT and exFAT against
mkfs.fat, mkfs.exfat, mtools and the kernel's vfat and exfat drivers, in the Linux test VM:
`chore test:oracle` (see the README's tests section). The tests behind the `oracle` feature fail,
never skip, when the VM or a tool is missing, and the failure names that task. When an oracle
disagrees with the app, write the failing test first, then fix the app, or the driver in a pull
request stacked on the driver's stack. Do not edit the harness repository.
