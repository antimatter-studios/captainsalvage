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

## Cutting a release

Run `scripts/create-release`. It shows what would go out and stops for a yes, then opens the release
pull request. When that has merged, `scripts/create-release --tag vX.Y.Z` tags `main` and pushes.

- Only the owner decides when to release. Do not start one unless they asked.
- Show them the list it prints, verbatim, and wait for their answer before passing
  `--contents-approved`.
- The tag starts the release pipeline, and the pipeline never creates a tag.

## Testing the window

`tests/window/` drives the real app window against small synthetic disk images. Read its
[README](tests/window/README.md) before running one: it says how to make the images, how the
harness keeps the test away from your settings and real drives, and what to do when a test
stalls. Run one window test at a time, never beside a build.

`scripts/rebase-pr.sh <branch>` rebases a pull request onto `main` in `.work/rebase`, keeps both
sides of conflicts where two branches appended to the same file (`scripts/resolve_appends.py`),
and pushes only if the TypeScript and clippy checks pass. Read the result of a Rust conflict by
hand: keeping both sides can join two functions or two tests together.
