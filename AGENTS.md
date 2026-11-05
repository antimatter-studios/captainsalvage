# Working on Captain Salvage

## Each pull request's title sets the next version

The release pipeline reads the type at the start of the title, and nothing else. So before
opening a pull request, assess what it changes for someone using the app, and title it to match.

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
