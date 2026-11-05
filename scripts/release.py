#!/usr/bin/env python3
"""Keeps the changelog, the version numbers and the release notes in step.

Release notes list the pull requests merged into a release, and nothing else. They
come from git history: this repository squashes every pull request into one commit
whose subject is the pull request's title, ending in (#number).

    scripts/release.py notes [--from REF] [--to REF]   print the notes for a range
    scripts/release.py sync                            rewrite the changelog from the tags
    scripts/release.py prepare VERSION                 name a version, such as 1.0.0
    scripts/release.py next                            print the next version, for the pipeline
    scripts/release.py version                         print this build's version
    scripts/release.py build                           build the app with that version

Version numbers follow what was merged, so nobody has to decide when to release. A new
feature raises the minor number and resets the patch. A fix raises the patch. Anything
else, such as documentation, releases nothing. Several merges at once take the largest
of these, once. A version is never written down for this: it is worked out from the tags
and the commits since the last one, so a build of the same code always has the same number.

Going to 1.0.0 is a deliberate choice. Name it with prepare, and merge that as a pull
request: a version in the manifests that is above the last tag is released as it is.
"""

import argparse
import json
import re
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
UNRELEASED = "[Unreleased]"
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
CARGO_MANIFESTS = ["crates/lens-core/Cargo.toml", "src-tauri/Cargo.toml"]
JSON_MANIFESTS = ["src-tauri/tauri.conf.json", "package.json"]
LOCKED_CRATES = ["exfat-lens", "lens-core"]


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


def readme_block(sections, limit=README_VERSIONS):
    """The newest versions for the README, with its headings one level down."""
    lines = [
        README_START,
        "",
        "The latest versions. [CHANGELOG.md](CHANGELOG.md) has the full history.",
        "",
    ]
    for heading, body in sections[:limit]:
        lines.append("### " + re.sub(r"^\[(\d[^\]]*)\]", r"v\1", heading).replace("[", "").replace("]", ""))
        lines.append("")
        lines.append(re.sub(r"^### (.+)$", r"**\1**", body, flags=re.MULTILINE))
        lines.append("")
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


def parse_version(text):
    major, minor, patch = text.removeprefix("v").split(".")
    return int(major), int(minor), int(patch)


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


def manifest_version():
    return json.loads((ROOT / "src-tauri/tauri.conf.json").read_text())["version"]


def latest_tag():
    done = subprocess.run(
        ["git", "describe", "--tags", "--abbrev=0", "--match", "v[0-9]*", "HEAD"],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    return done.stdout.strip() if done.returncode == 0 else None


def subjects_since(tag):
    span = f"{tag}..HEAD" if tag else "HEAD"
    return git("log", "--first-parent", "--format=%s", span).splitlines()


def current_version():
    tag = latest_tag()
    return next_version(manifest_version(), tag, subjects_since(tag))


def git(*args):
    return subprocess.run(["git", *args], cwd=ROOT, check=True, capture_output=True, text=True).stdout.strip()


def previous_tag(ref):
    """The last release before a ref, or None for the first one."""
    done = subprocess.run(
        ["git", "describe", "--tags", "--abbrev=0", "--match", "v[0-9]*", f"{ref}^"],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    return done.stdout.strip() if done.returncode == 0 else None


def pulls_between(start, end):
    span = f"{start}..{end}" if start else end
    pulls = [parse_subject(line) for line in git("log", "--first-parent", "--format=%s", span).splitlines()]
    return [pull for pull in pulls if pull]


def write_all(sections):
    CHANGELOG.write_text(join_changelog(sections))
    README.write_text(replace_readme_block(README.read_text(), readme_block(sections)))


def unreleased_body():
    return render_notes(pulls_between(latest_tag(), "HEAD")).strip()


def tags_newest_first():
    return git("tag", "--list", "v[0-9]*", "--merged", "HEAD", "--sort=-v:refname").split()


def released_sections():
    """Every release, from the tags, so the file can be rebuilt at any time."""
    tags = tags_newest_first()
    sections = []
    for index, tag in enumerate(tags):
        before = tags[index + 1] if index + 1 < len(tags) else None
        date = git("log", "-1", "--format=%as", tag)
        sections.append((f"[{tag.removeprefix('v')}] — {date}", render_notes(pulls_between(before, tag)).strip()))
    return sections


def command_notes(args):
    end = args.to
    start = args.start if args.start else previous_tag(end)
    sys.stdout.write(render_notes(pulls_between(start, end)))


def command_sync(_):
    write_all([(UNRELEASED, unreleased_body()), *released_sections()])


def command_next(_):
    version, release = current_version()
    print(f"version={version}")
    print(f"release={'true' if release else 'false'}")


def command_version(_):
    print(current_version()[0])


def command_build(_):
    version = current_version()[0]
    print(f"Building version {version}")
    config = json.dumps({"version": version})
    subprocess.run(["npx", "tauri", "build", "--bundles", "app", "--config", config], cwd=ROOT, check=True)


def command_prepare(args):
    version = args.version.removeprefix("v")
    names = [*CARGO_MANIFESTS, *JSON_MANIFESTS, "Cargo.lock", "package-lock.json"]
    new = set_versions({name: (ROOT / name).read_text() for name in names}, version)
    for name, text in new.items():
        (ROOT / name).write_text(text)
    print(f"Prepared {version}. Open a pull request titled: chore(release): v{version}")
    print("When it merges, the pipeline releases that version.")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(required=True)
    notes = commands.add_parser("notes")
    notes.add_argument("--from", dest="start")
    notes.add_argument("--to", default="HEAD")
    notes.set_defaults(run=command_notes)
    commands.add_parser("sync").set_defaults(run=command_sync)
    prepare = commands.add_parser("prepare")
    prepare.add_argument("version")
    prepare.set_defaults(run=command_prepare)
    commands.add_parser("next").set_defaults(run=command_next)
    commands.add_parser("version").set_defaults(run=command_version)
    commands.add_parser("build").set_defaults(run=command_build)
    args = parser.parse_args()
    args.run(args)


if __name__ == "__main__":
    main()
