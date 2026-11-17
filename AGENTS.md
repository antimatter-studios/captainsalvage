# Working on Captain Salvage

## Each pull request's title sets the next version

When a release is prepared, the version comes from the type at the start of each title since
the last one, and nothing else. So before opening a pull request, assess what it changes for
someone using the app, and title it to match.

| Type | Use it when | Version |
|---|---|---|
| `fix:` | Something that should already work was wrong, and now it is right | Patch goes up |
| `feat:` | The app can do something it could not do before | Minor goes up |
| `docs:`, `test:`, `chore:`, `ci:`, `refactor:` | Someone using the app would notice nothing | None, and no release |

- Judge by the effect on the user, not by the size of the change. A large repair is still a
  `fix:`, and a small new option is still a `feat:`.
- Improving something that already worked is a `feat:`. Only broken behaviour earns a `fix:`.
- If a change does both, split it into two pull requests. If it cannot be split, use `feat:`.
- Say why in the description, so the assessment can be checked. Correct the title before
  merging if the work changed, because the squash commit takes its subject from the title.
- Never set a version by hand. 1.0.0 and other named versions are the owner's decision.

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
stalls. Run one window test at a time, never beside a build.

`scripts/rebase-pr.sh <branch>` rebases a pull request onto `main` in `.work/rebase`, keeps both
sides of conflicts where two branches appended to the same file (`scripts/resolve_appends.py`),
and pushes only if the TypeScript and clippy checks pass. Read the result of a Rust conflict by
hand: keeping both sides can join two functions or two tests together.

## Independent oracles

A reader that shares the code it checks proves only that it agrees with itself. NTFS is also
checked against ntfs-3g's tools and the Linux kernel's ntfs3 driver, in the Linux test VM:
`chore test:oracle` (see the README's tests section). The tests behind the `oracle` feature fail,
never skip, when the VM or a tool is missing, and the failure names that task. When an oracle
disagrees with the app, write the failing test first, then fix the app, or the driver in a pull
request stacked on the driver's stack. Do not edit the harness repository.
