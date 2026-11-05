# changelog.d

Changes that have not been released yet, one file per pull request. A release gathers them
into a dated section of [`CHANGELOG.md`](../CHANGELOG.md) and deletes them, so this directory
holds only what the next release will say.

A fragment is named for its branch -- `fix/run-end-bound` writes `fix-run-end-bound.md` -- and
holds one or more level-3 headings, each over `- ` entries:

```markdown
### Fixed

- **The run-end bound is checked.** A run that ends past the device is refused rather than read.
```

On a `feat/` or `fix/` branch, the pre-commit hook writes the first one from what is staged,
using an agent for the wording. Install it with `scripts/install-hooks.sh`. If you write the file
yourself, the hook leaves it alone. Check the wording before opening a pull request, because
the hook only saw the branch's first commit.

A change no one using the app would notice -- tests, CI, documentation -- needs no fragment.

- `scripts/changelog-check.sh` refuses a malformed fragment, and an entry written into
  `CHANGELOG.md` directly.
- `scripts/changelog-gather.sh VERSION` is the release step, run by `scripts/create-release`.
