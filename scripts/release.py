#!/usr/bin/env python3
"""Keeps the changelog, the version numbers and the release notes in step.

Release notes list the pull requests merged into a release, and nothing else. They
come from git history: this repository squashes every pull request into one commit
whose subject is the pull request's title, ending in (#number).

    scripts/release.py notes [--from REF] [--to REF]   print pull requests for a range
    scripts/release.py sync                            rebuild the README's list from CHANGELOG.md
    scripts/release.py prepare [VERSION]               gather the fragments, set the version, write the license list
    scripts/release.py prepare --append [VERSION]      add the fragments that remain to a section that exists
    scripts/release.py notes-check [--strict]          flag retired words in the notes a release will say
    scripts/release.py contents [VERSION]              print the next version and what is in it
    scripts/release.py section VERSION                 print a version's CHANGELOG.md section
    scripts/release.py check v0.2.0                    fail unless a tag matches the files
    scripts/release.py next-rc                         print the next release candidate, such as 0.4.0-rc1
    scripts/release.py stamp-rc 0.4.0-rc1              write a release candidate's version into every file
    scripts/release.py version                         print this build's version
    scripts/release.py build                           build the app with that version
    scripts/release.py install user|system             copy the build to ~/Applications or /Applications

scripts/create-release is what you run to cut a release: it calls these.

Version numbers follow what was merged since the last tag. A new feature raises the minor
number and resets the patch. A fix raises the patch. Anything else moves nothing. Several
merges take the largest of these, once. So the number is worked out, and a build shows the
number that code would be released as. Going to 1.0.0 is a deliberate choice: name it.

A release candidate, 0.4.0-rc1, is a release's content under a version that has not been
released. The release pull request keeps VERSION at 0.4.0, and the candidate's tag points at a
commit beside it that says 0.4.0-rc1 everywhere, so a build shows the candidate's number. It
never counts as a release: the next version is worked out from the last plain tag. What a tag or
a version may look like is decided once, by scripts/tag-kind, and this file asks it.

Unreleased changes are fragments in changelog.d/. A release gathers them into CHANGELOG.md, and
the README's list is a copy of the newest versions there, so the two cannot disagree.

`prepare` also writes the license list (THIRD-PARTY-LICENSES.md and the README's table), because
the README names this app's own crates with their version, and a release raises it. That list is read
from the installed npm packages, so `prepare` refuses, before it changes anything, until `npm ci` has run.

`prepare` refuses a version whose section CHANGELOG.md already has. For a release that is not yet
released, such as an integration branch that has taken more pull requests since its candidate was
cut, `prepare --append` adds the fragments that remain to that section and leaves the rest of it.

`notes-check` looks for the words the product has retired in the notes a release will say, the way a
person reading them would. They may appear under "Removed", and in an entry that says it is a rename
("is now called", "are now called", "was renamed", "renamed to"), and nowhere else. It only reports, and
`--strict` makes a finding fail, which is what the cut stage of scripts/create-release uses.
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REPO = "antimatter-studios/captainsalvage"
CHANGELOG = ROOT / "CHANGELOG.md"
README = ROOT / "README.md"
README_START = "<!-- changelog:start -->"
README_END = "<!-- changelog:end -->"
README_VERSIONS = 10
# Each version's screenshots, in media/changelog/v<version>/, with the table that shows them.
SCREENSHOTS = ROOT / "media/changelog"
SCREENSHOT_TABLE = "screenshots.md"
UNRELEASED = "[Unreleased]"
FRAGMENTS = "changelog.d"
LICENSES = "scripts/licenses.py"
# Words the product no longer uses, and what a note should say instead. A note may use one only
# under "Removed", where saying what is gone is the point. The last one is not a word: it is an
# entry that tells the reader the release does not do what another entry says it does.
RETIRED_VOCABULARY = [
    (re.compile(r"\bcanvas\w*", re.IGNORECASE), "canvas", 'say "Disk Rebuild"'),
    (re.compile(r"\bbasket\w*", re.IGNORECASE), "basket", 'say "treasure list"'),
    (re.compile(r"\binspect raw\b", re.IGNORECASE), "Inspect raw", 'say "Open"'),
    (re.compile(r"\brebuild tree\b", re.IGNORECASE), "Rebuild Tree", 'say "Disk Rebuild"'),
    (re.compile(r"\bnot part of this change\b", re.IGNORECASE), "not part of this change",
     "another entry says the release has it, so drop the sentence or the entry that it contradicts"),
]
VOCABULARY_EXEMPT = "Removed"
# An entry that says it is a rename may name the old word, because that is what it reports.
RENAME_MARKERS = re.compile(r"\b(?:is now called|are now called|was renamed|renamed to)\b", re.IGNORECASE)
NO_CHANGES = "No pull requests merged yet."
SECTION_ORDER = ["Added", "Fixed", "Documentation", "Other"]
SECTION_FOR_TYPE = {"feat": "Added", "fix": "Fixed", "docs": "Documentation"}
SUBJECT = re.compile(r"^(?:(?P<type>[a-z]+)(?P<scope>\([^)]*\))?(?P<breaking>!)?:\s*)?(?P<title>.+?)\s*\(#(?P<number>\d+)\)$")
LEVEL_MAJOR = 3
LEVEL_MINOR = 2
LEVEL_PATCH = 1
LEVEL_NONE = 0
CHANGELOG_HEADER = f"""# Changelog

Each release lists the pull requests merged into it, and nothing else. The same notes
appear on the [releases page](https://github.com/{REPO}/releases). `scripts/release.py`
writes this file.
"""
# What a tag, or a version, may look like is scripts/tag-kind's to say. Beside this file, not under
# ROOT, so a test that points ROOT at a scratch project still asks the real one.
TAG_KIND = Path(__file__).resolve().parent / "tag-kind"
# A release's tag is plain, and tag-kind accepts no other suffix than a candidate's, so a tag with
# a dash is never one. `git describe` would otherwise take a candidate for the last release.
NOT_A_RELEASE = "v*-*"
# The one file that says the version. Everything else is copied from it by `prepare`.
VERSION_FILE = "VERSION"
CARGO_MANIFESTS = ["crates/captainsalvage/Cargo.toml", "src-tauri/Cargo.toml"]
JSON_MANIFESTS = ["src-tauri/tauri.conf.json", "package.json"]
LOCKED_CRATES = ["captain-salvage", "captainsalvage"]
APP_NAME = "Captain Salvage.app"
INSTALL_FOLDERS = {"user": Path.home() / "Applications", "system": Path("/Applications")}
OLD_FINDER_HELPER = "Captain Salvage Finder.app"
LSREGISTER = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"


def parse_subject(subject):
    """A squash commit's subject as a pull request, or None if it did not come from one."""
    match = SUBJECT.match(subject.strip())
    if not match:
        return None
    kind = match["type"] or ""
    if kind == "chore" and match["scope"] == "(release)":
        return None
    title = match["title"]
    return {
        "number": int(match["number"]),
        "section": SECTION_FOR_TYPE.get(kind, "Other"),
        "title": title[0].upper() + title[1:],
    }


def render_notes(pulls, repo=REPO):
    """Pull requests grouped by what they did, newest first within each group."""
    if not pulls:
        return NO_CHANGES + "\n"
    blocks = []
    for section in SECTION_ORDER:
        lines = [
            f"- {pull['title']} ([#{pull['number']}](https://github.com/{repo}/pull/{pull['number']}))"
            for pull in pulls
            if pull["section"] == section
        ]
        if lines:
            blocks.append(f"### {section}\n\n" + "\n".join(lines) + "\n")
    return "\n".join(blocks)


def split_sections(text):
    """The changelog's version sections, as (heading, body), in file order."""
    found = re.split(r"^## (.+)$", text, flags=re.MULTILINE)
    return [(found[i].strip(), found[i + 1].strip()) for i in range(1, len(found) - 1, 2)]


def join_changelog(sections):
    parts = [CHANGELOG_HEADER]
    for heading, body in sections:
        parts.append(f"## {heading}\n\n{body}\n")
    return "\n".join(parts)


def version_screenshots(folder, heading):
    """The screenshot table kept beside a version's images, or None if it has none."""
    match = re.match(r"^\[(\d[^\]]*)\]", heading)
    if not match:
        return None
    table = folder / f"v{match[1]}" / SCREENSHOT_TABLE
    return table.read_text().strip() if table.exists() else None


def readme_block(sections, limit=README_VERSIONS, screenshots=None):
    """The newest versions for the README, with its headings one level down. With a
    screenshots folder, a version's table from there follows its notes."""
    lines = [
        README_START,
        "",
        "The latest versions. [CHANGELOG.md](CHANGELOG.md) has the full history.",
        "",
    ]
    # The limit is on versions, so [Unreleased] does not push one out.
    unreleased = [item for item in sections if item[0] == UNRELEASED]
    versions = [item for item in sections if item[0] != UNRELEASED]
    for heading, body in [*unreleased, *versions[:limit]]:
        lines.append("### " + re.sub(r"^\[(\d[^\]]*)\]", r"v\1", heading).replace("[", "").replace("]", ""))
        lines.append("")
        lines.append(re.sub(r"^### (.+)$", r"**\1**", body, flags=re.MULTILINE))
        lines.append("")
        table = screenshots and version_screenshots(screenshots, heading)
        if table:
            lines += ["**Screenshots**", "", table, ""]
    lines.append(README_END)
    return "\n".join(lines)


def replace_readme_block(readme, block):
    pattern = re.compile(re.escape(README_START) + r".*?" + re.escape(README_END), re.DOTALL)
    if not pattern.search(readme):
        raise SystemExit("README.md has no changelog markers")
    return pattern.sub(lambda _: block, readme)


def set_versions(files, version):
    """Each manifest's own version. Files maps a path's text; returns the new text."""
    out = {}
    for path, text in files.items():
        if path.endswith("Cargo.toml"):
            out[path] = re.sub(r'(?m)^version = "[^"]+"', f'version = "{version}"', text, count=1)
        elif path == "Cargo.lock":
            for crate in LOCKED_CRATES:
                text = re.sub(rf'(name = "{crate}"\nversion = )"[^"]+"', rf'\1"{version}"', text)
            out[path] = text
        elif path == "package-lock.json":
            out[path] = re.sub(r'"version": "[^"]+"', f'"version": "{version}"', text, count=2)
        else:
            out[path] = re.sub(r'"version": "[^"]+"', f'"version": "{version}"', text, count=1)
    return out


def tag_kind(text):
    """"final" or "candidate" for a tag or version, as scripts/tag-kind says. Raises ValueError,
    carrying its message, for a name it does not understand."""
    done = subprocess.run(["bash", str(TAG_KIND), "v" + text.removeprefix("v")], capture_output=True, text=True)
    if done.returncode != 0:
        raise ValueError(done.stderr.strip())
    return done.stdout.strip()


def base_version(text):
    """The release a version belongs to: 0.4.0 for 0.4.0-rc2, and 0.4.0 for itself."""
    return text.removeprefix("v").split("-", 1)[0]


def is_candidate(text):
    return tag_kind(text) == "candidate"


def parse_version(text):
    """A version's three numbers. A candidate counts as the release it is a candidate for."""
    major, minor, patch = base_version(text).split(".")
    return int(major), int(minor), int(patch)


def next_candidate(version, tags):
    """The version of the next candidate of a release, one past the highest already tagged."""
    taken = []
    for tag in tags:
        # Only a candidate of this release counts, and tag-kind says what one is.
        try:
            if tag.startswith(f"v{version}-rc") and tag_kind(tag) == "candidate":
                taken.append(int(tag.removeprefix(f"v{version}-rc")))
        except ValueError:
            continue
    return f"{version}-rc{max(taken, default=0) + 1}"


def release_level(subjects):
    """How far these squash commits move the version. A commit from no pull request counts for nothing."""
    level = LEVEL_NONE
    for subject in subjects:
        match = SUBJECT.match(subject.strip())
        if not match:
            continue
        if match["breaking"]:
            level = max(level, LEVEL_MAJOR)
        elif match["type"] == "feat":
            level = max(level, LEVEL_MINOR)
        elif match["type"] == "fix":
            level = max(level, LEVEL_PATCH)
    return level


def next_version(manifest, latest_tag, subjects):
    """The version these commits make, and whether that means a new release."""
    if not latest_tag:
        return manifest, True
    major, minor, patch = parse_version(latest_tag)
    if parse_version(manifest) > (major, minor, patch):
        return manifest, True
    level = release_level(subjects)
    # Before 1.0.0, a breaking change is only a new minor version.
    if level == LEVEL_MAJOR and major == 0:
        level = LEVEL_MINOR
    if level == LEVEL_MAJOR:
        return f"{major + 1}.0.0", True
    if level == LEVEL_MINOR:
        return f"{major}.{minor + 1}.0", True
    if level == LEVEL_PATCH:
        return f"{major}.{minor}.{patch + 1}", True
    return f"{major}.{minor}.{patch}", False


def source_version():
    """The one version the project carries, from VERSION. Every manifest copies it."""
    text = (ROOT / VERSION_FILE).read_text().strip()
    try:
        tag_kind(text)
    except ValueError as problem:
        raise SystemExit(f"{VERSION_FILE} must hold one version such as 0.2.0 or 0.2.0-rc1, not {text!r}.\n{problem}")
    if text.startswith("v"):
        raise SystemExit(f"{VERSION_FILE} holds a version, not a tag: {text!r} has no v.")
    return text


def latest_tag():
    done = subprocess.run(
        ["git", "describe", "--tags", "--abbrev=0", "--match", "v[0-9]*", "--exclude", NOT_A_RELEASE, "HEAD"],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    return done.stdout.strip() if done.returncode == 0 else None


def subjects_since(tag, ref="HEAD"):
    span = f"{tag}..{ref}" if tag else ref
    return git("log", "--first-parent", "--format=%s", span).splitlines()


def current_version():
    tag = latest_tag()
    return next_version(source_version(), tag, subjects_since(tag))


def version_at(ref):
    """The next version going by what is on a branch, whatever is checked out."""
    done = subprocess.run(
        ["git", "describe", "--tags", "--abbrev=0", "--match", "v[0-9]*", "--exclude", NOT_A_RELEASE, ref],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    tag = done.stdout.strip() if done.returncode == 0 else None
    manifest = json.loads(git("show", f"{ref}:src-tauri/tauri.conf.json"))["version"]
    subjects = subjects_since(tag, ref)
    return (*next_version(manifest, tag, subjects), subjects)


def git(*args):
    return subprocess.run(["git", *args], cwd=ROOT, check=True, capture_output=True, text=True).stdout.strip()


def previous_tag(ref):
    """The last release before a ref, or None for the first one."""
    done = subprocess.run(
        ["git", "describe", "--tags", "--abbrev=0", "--match", "v[0-9]*", "--exclude", NOT_A_RELEASE, f"{ref}^"],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    return done.stdout.strip() if done.returncode == 0 else None


def pulls_between(start, end):
    span = f"{start}..{end}" if start else end
    pulls = [parse_subject(line) for line in git("log", "--first-parent", "--format=%s", span).splitlines()]
    return [pull for pull in pulls if pull]


def read_sections():
    return split_sections(CHANGELOG.read_text()) if CHANGELOG.exists() else []


def write_readme():
    """The README's list is a copy of the newest versions in CHANGELOG.md."""
    versions = [item for item in read_sections() if item[0] != UNRELEASED]
    README.write_text(replace_readme_block(README.read_text(), readme_block(versions, screenshots=SCREENSHOTS)))


def command_notes(args):
    end = args.to
    start = args.start if args.start else previous_tag(end)
    sys.stdout.write(render_notes(pulls_between(start, end)))


def write_versions(version):
    """Copies one version into every manifest and lock file, and VERSION itself."""
    names = [*CARGO_MANIFESTS, *JSON_MANIFESTS, "Cargo.lock", "package-lock.json"]
    names = [name for name in names if (ROOT / name).exists()]
    new = set_versions({name: (ROOT / name).read_text() for name in names}, version)
    for name, text in new.items():
        (ROOT / name).write_text(text)
    (ROOT / VERSION_FILE).write_text(version + "\n")


def command_sync(_):
    """Makes every file carry VERSION, and the README match the changelog."""
    write_versions(source_version())
    write_readme()


def command_contents(args):
    version, release, subjects = version_at("origin/main")
    if args.version:
        version = args.version.removeprefix("v")
    elif not release:
        raise SystemExit("Nothing to release: no feat or fix has merged since the last tag.")
    print(version)
    for subject in subjects:
        print(subject)


def command_section(args):
    version = args.version.removeprefix("v")
    for heading, body in read_sections():
        if heading.startswith(f"[{version}]"):
            print(body)
            return
    raise SystemExit(f"CHANGELOG.md has no section for {version}")


def command_version(_):
    print(current_version()[0])


def command_build(_):
    # The build carries the version the files carry, never a number worked out from pull
    # requests that have not been prepared into a release yet.
    version = source_version()
    print(f"Building version {version}")
    config = json.dumps({"version": version})
    subprocess.run(["npx", "tauri", "build", "--bundles", "app", "--config", config], cwd=ROOT, check=True)
    # The Finder extension goes inside, and the app is signed, when signing material is given.
    subprocess.run([str(ROOT / "macos/finder/package.sh"), str(built_app())], cwd=ROOT, check=True)


def command_next_rc(_):
    """The version of the next candidate of the release VERSION names. VERSION must be plain: a
    candidate is cut from the release's own pull request, which says the release's number."""
    version = source_version()
    if is_candidate(version):
        raise SystemExit(f"{VERSION_FILE} says {version}, which is already a candidate. Cut one from the release's own branch.")
    print(next_candidate(version, git("tag", "--list", f"v{version}-rc*").splitlines()))


def command_stamp_rc(args):
    """Writes a candidate's version into VERSION and every manifest and lock file, so a build of
    that commit carries it. Only a candidate of the release VERSION names is accepted: this is
    not a way to set any other number."""
    candidate = args.version.removeprefix("v")
    released = source_version()
    try:
        wanted_a_candidate = is_candidate(candidate)
    except ValueError as problem:
        raise SystemExit(f"{args.version} is not a release candidate such as {released}-rc1.\n{problem}")
    if is_candidate(released) or not wanted_a_candidate:
        raise SystemExit(f"{args.version} is not a release candidate such as {released}-rc1")
    if base_version(candidate) != released:
        raise SystemExit(f"{candidate} is a candidate of {base_version(candidate)}, but {VERSION_FILE} says {released}")
    write_versions(candidate)
    print(f"Every file carries {candidate}.")


def require_npm_packages():
    """The license list is read from the installed npm packages, so it cannot be written without them."""
    if not (ROOT / "node_modules").is_dir():
        raise SystemExit("node_modules is not here, and the license list is read from the npm packages in it. "
                         "Run `npm ci`, then run this again. Nothing was changed.")


def write_licenses():
    """Writes THIRD-PARTY-LICENSES.md and the README's list. The README names this app's own crates with
    their version, so a release that raises the version leaves the list out of date until this runs."""
    done = subprocess.run([sys.executable, str(ROOT / LICENSES), "write"], cwd=ROOT)
    if done.returncode != 0:
        raise SystemExit(f"{LICENSES} write failed, so the license list is out of date. Fix what it said, then run it again.")


def command_prepare(args):
    if args.version:
        version = args.version.removeprefix("v")
    elif args.append:
        # The section being added to is the release this branch is making, and VERSION says which.
        version = base_version(source_version())
    else:
        version, release = current_version()
        if not release:
            raise SystemExit("Nothing to release: no feat or fix has merged since the last tag.")
    # Everything that can be known to fail is checked before the first file changes.
    require_npm_packages()
    gather = ["bash", str(ROOT / "scripts/changelog-gather.sh"), *(["--append"] if args.append else []), version]
    done = subprocess.run(gather, cwd=ROOT)
    if done.returncode != 0:
        raise SystemExit(done.returncode)
    write_readme()
    write_versions(version)
    write_licenses()


def vocabulary_findings(text, source, first_line=1):
    """The retired words in some notes, as (source, line, word, advice, text). Notes are `### Heading`
    sections of entries. A retired word may be named in a "Removed" entry, and in an entry that says it is
    a rename, which is any list item containing one of RENAME_MARKERS, in the lines of that one item."""
    section = None
    entry = None  # the id of the list item a line belongs to
    lines = []  # (number, line, section, entry)
    entry_text = {}
    for number, line in enumerate(text.splitlines(), first_line):
        heading = re.match(r"^###\s+(.+?)\s*$", line)
        if heading or re.match(r"^##\s", line):
            section = heading[1] if heading else None
            entry = None
            continue
        if line.startswith("- "):
            entry = number
        elif line.strip() and not line[0].isspace():
            entry = None
        if entry is not None:
            entry_text[entry] = entry_text.get(entry, "") + line + "\n"
        lines.append((number, line, section, entry))
    found = []
    for number, line, section, entry in lines:
        if section == VOCABULARY_EXEMPT or (entry is not None and RENAME_MARKERS.search(entry_text[entry])):
            continue
        for pattern, word, advice in RETIRED_VOCABULARY:
            if pattern.search(line):
                found.append((source, number, word, advice, line.strip()))
    return found


def changelog_section_lines(version):
    """A version's section of CHANGELOG.md as (the number of its first line, its text), or None."""
    if not CHANGELOG.exists():
        return None
    lines = CHANGELOG.read_text().splitlines()
    heading = re.compile(rf"^##\s+\[?v?{re.escape(version)}[\]\s]")
    for start, line in enumerate(lines):
        if heading.match(line):
            end = next((n for n in range(start + 1, len(lines)) if re.match(r"^##\s", lines[n])), len(lines))
            return start + 1, "\n".join(lines[start:end])
    return None


def fragments_in(ref):
    """The fragments, as (path, text): those committed at a ref, or those in the working tree."""
    if ref:
        names = git("ls-tree", "-r", "--name-only", ref, "--", FRAGMENTS).splitlines()
        return [(name, git("show", f"{ref}:{name}")) for name in names
                if name.endswith(".md") and not name.endswith("/README.md")]
    folder = ROOT / FRAGMENTS
    return [(f"{FRAGMENTS}/{path.name}", path.read_text()) for path in sorted(folder.glob("*.md"))
            if path.name != "README.md"] if folder.is_dir() else []


def command_notes_check(args):
    """Reports retired words in the notes a release will say: the fragments waiting to be gathered, and,
    unless --fragments names a ref, the changelog section of the release VERSION names or --version
    does. A release that is already tagged has said what it said, so its section is left alone unless
    --version names it. Exits 1 on --strict."""
    found = []
    for path, text in fragments_in(args.fragments):
        found += vocabulary_findings(text, path)
    if not args.fragments:
        version = args.version.removeprefix("v") if args.version else base_version(source_version())
        released = bool(git("tag", "--list", f"v{version}"))
        section = None if released and not args.version else changelog_section_lines(version)
        if section:
            found += vocabulary_findings(section[1], "CHANGELOG.md", section[0])
    actions = os.environ.get("GITHUB_ACTIONS") == "true"
    for source, number, word, advice, line in found:
        message = f'"{word}" outside Removed or a rename: {advice}.'
        print(f"{source}:{number}: {message}\n    {line[:160]}")
        if actions:
            print(f"::warning file={source},line={number}::{message}")
    if found:
        print(f"notes-check: {len(found)} place(s) use retired words. A note may name one only under Removed, or in an entry that says it is a rename.", file=sys.stderr)
        if args.strict:
            raise SystemExit(1)
    else:
        print("notes-check: the notes use none of the retired words.")


def command_check(args):
    """Every manifest and lock file must carry VERSION. Given a tag, the tag must be it too,
    and the changelog and README must describe it. A candidate's tag, v0.4.0-rc1, needs the
    files to say 0.4.0-rc1, and the changelog and README to describe 0.4.0, the release it is
    a candidate for. A release's tag must be plain: no file may still say a candidate. Runs on
    every pull request, so drift fails before it merges."""
    version = source_version()
    problems = []
    for name, found in manifest_versions():
        if found != version:
            problems.append(f"{name} says {found}")
    if args.tag:
        tag = args.tag.removeprefix("v")
        try:
            tag_kind(tag)
        except ValueError as problem:
            # A name tag-kind does not understand is refused, whatever else is true of it.
            problems.append(str(problem).replace("\n", " "))
        else:
            if tag != version:
                problems.append(f"the tag is {tag}, but {VERSION_FILE} says {version}")
            described = base_version(tag)
            if not any(heading.startswith(f"[{described}]") for heading, _ in read_sections()):
                problems.append(f"CHANGELOG.md has no section for {described}")
            if f"### v{described}" not in README.read_text():
                problems.append(f"README.md has no section for {described}")
    if problems:
        raise SystemExit("the version is not one number everywhere: " + "; ".join(problems))
    print(f"Every file carries {version}.")


def manifest_versions():
    """The version each manifest and lock file states, as (name, version) pairs."""
    found = []
    for name in CARGO_MANIFESTS:
        match = re.search(r'(?m)^version = "([^"]+)"', (ROOT / name).read_text())
        found.append((name, match[1] if match else None))
    for name in JSON_MANIFESTS:
        found.append((name, json.loads((ROOT / name).read_text()).get("version")))
    for crate in LOCKED_CRATES:
        match = re.search(rf'name = "{crate}"\nversion = "([^"]+)"', (ROOT / "Cargo.lock").read_text())
        found.append((f"Cargo.lock ({crate})", match[1] if match else None))
    if (ROOT / "package-lock.json").exists():
        lock = json.loads((ROOT / "package-lock.json").read_text())
        found.append(("package-lock.json", lock.get("version")))
        found.append(("package-lock.json (root package)", lock.get("packages", {}).get("", {}).get("version")))
    return found


def running_programs():
    """The path of every program running now, for this user or any other."""
    listed = subprocess.run(["ps", "-axo", "comm="], capture_output=True, text=True, check=True)
    return [line.strip() for line in listed.stdout.splitlines()]


def install_app(source, folder, running):
    """Copies the built app into folder, replacing the copy there. The new copy is written
    next to the old one and swapped in, so a failed copy leaves the old one as it was.
    Returns the installed path, or raises SystemExit with what to do."""
    source, folder = Path(source), Path(folder)
    if not (source / "Contents/Info.plist").is_file():
        raise SystemExit(f"There is no build at {source}. Run `npm run app` first.")
    target = folder / source.name
    inside = str(target) + "/"
    if any(program.startswith(inside) for program in running):
        raise SystemExit(f"{target} is running. Quit it, then install again.")
    try:
        folder.mkdir(parents=True, exist_ok=True)
        staging = folder / f".{source.name}.installing"
        old = folder / f".{source.name}.old"
        for leftover in (staging, old):
            if leftover.exists():
                shutil.rmtree(leftover)
        # ditto keeps the bundle's symlinks, permissions and extended attributes.
        subprocess.run(["ditto", str(source), str(staging)], check=True)
        if target.exists():
            os.rename(target, old)
        os.rename(staging, target)
        if old.exists():
            shutil.rmtree(old)
    except PermissionError:
        raise SystemExit(f"You cannot write to {folder}. Use an administrator account, or install for this user only.")
    return target


def retire_finder_helper(folder):
    """The Finder helper used to be an app of its own, with the identifier the app now has.
    Two apps with one identifier confuse macOS, so the old helper goes."""
    old = Path(folder) / OLD_FINDER_HELPER
    if not old.exists():
        return None
    if Path(LSREGISTER).exists():
        subprocess.run([LSREGISTER, "-u", str(old)], check=False, capture_output=True)
    shutil.rmtree(old)
    return old


def built_app():
    """Where `build` leaves the app: in CARGO_TARGET_DIR when that is set, as `build` itself reads it."""
    target = Path(os.environ.get("CARGO_TARGET_DIR") or ROOT / "target")
    return target / "release/bundle/macos" / APP_NAME


def command_install(args):
    folder = INSTALL_FOLDERS[args.where]
    target = install_app(built_app(), folder, running_programs())
    if Path(LSREGISTER).exists():
        subprocess.run([LSREGISTER, "-f", "-R", "-trusted", str(target)], check=False)
    extension = target / "Contents/PlugIns/Captain Salvage.appex"
    if extension.exists():
        subprocess.run(["pluginkit", "-a", str(extension)], check=False)
    retired = retire_finder_helper(folder)
    if retired:
        print(f"Removed {retired}: the Finder extension is inside the app now")
    print(f"Installed {target}")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(required=True)
    notes = commands.add_parser("notes")
    notes.add_argument("--from", dest="start")
    notes.add_argument("--to", default="HEAD")
    notes.set_defaults(run=command_notes)
    commands.add_parser("next-rc").set_defaults(run=command_next_rc)
    stamp = commands.add_parser("stamp-rc")
    stamp.add_argument("version")
    stamp.set_defaults(run=command_stamp_rc)
    commands.add_parser("sync").set_defaults(run=command_sync)
    prepare = commands.add_parser("prepare")
    prepare.add_argument("version", nargs="?")
    prepare.add_argument("--append", action="store_true",
                         help="Add the remaining fragments to the version's existing section.")
    prepare.set_defaults(run=command_prepare)
    notes_check = commands.add_parser("notes-check")
    notes_check.add_argument("--version", help="The release whose changelog section is read. Default: VERSION's.")
    notes_check.add_argument("--fragments", metavar="REF", help="Read the fragments committed at REF, and nothing else.")
    notes_check.add_argument("--strict", action="store_true", help="Exit 1 when a retired word is found.")
    notes_check.set_defaults(run=command_notes_check)
    check = commands.add_parser("check")
    check.add_argument("tag", nargs="?", help="A release tag, when checking a release.")
    check.set_defaults(run=command_check)
    contents = commands.add_parser("contents")
    contents.add_argument("version", nargs="?")
    contents.set_defaults(run=command_contents)
    section = commands.add_parser("section")
    section.add_argument("version")
    section.set_defaults(run=command_section)
    commands.add_parser("version").set_defaults(run=command_version)
    commands.add_parser("build").set_defaults(run=command_build)
    install = commands.add_parser("install")
    install.add_argument("where", choices=sorted(INSTALL_FOLDERS))
    install.set_defaults(run=command_install)
    args = parser.parse_args()
    args.run(args)


if __name__ == "__main__":
    main()
