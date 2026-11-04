#!/usr/bin/env python3
"""Keeps the changelog, the version numbers and the release notes in step.

Release notes list the pull requests merged into a release, and nothing else. They
come from git history: this repository squashes every pull request into one commit
whose subject is the pull request's title, ending in (#number).

    scripts/release.py notes [--from REF] [--to REF]   print the notes for a range
    scripts/release.py sync                            refresh [Unreleased] and the README
    scripts/release.py prepare 0.2.0                   start a release: bump and move notes
    scripts/release.py check v0.2.0                    fail unless a tag matches the files
"""

import argparse
import datetime
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
SUBJECT = re.compile(r"^(?:(?P<type>[a-z]+)(?P<scope>\([^)]*\))?!?:\s*)?(?P<title>.+?)\s*\(#(?P<number>\d+)\)$")
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
        lines.append(f"### {heading.replace('[', '').replace(']', '')}")
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


def read_sections():
    return split_sections(CHANGELOG.read_text()) if CHANGELOG.exists() else []


def write_all(sections):
    CHANGELOG.write_text(join_changelog(sections))
    README.write_text(replace_readme_block(README.read_text(), readme_block(sections)))


def unreleased_body():
    return render_notes(pulls_between(previous_tag("HEAD"), "HEAD")).strip()


def command_notes(args):
    end = args.to
    start = args.start if args.start else previous_tag(end)
    sys.stdout.write(render_notes(pulls_between(start, end)))


def command_sync(_):
    released = [item for item in read_sections() if item[0] != UNRELEASED]
    write_all([(UNRELEASED, unreleased_body()), *released])


def command_prepare(args):
    version = args.version.removeprefix("v")
    released = [item for item in read_sections() if item[0] != UNRELEASED]
    if any(heading.startswith(f"[{version}]") for heading, _ in released):
        raise SystemExit(f"CHANGELOG.md already has {version}")
    today = datetime.date.today().isoformat()
    section = (f"[{version}] — {today}", unreleased_body())
    write_all([(UNRELEASED, NO_CHANGES), section, *released])
    names = [*CARGO_MANIFESTS, *JSON_MANIFESTS, "Cargo.lock", "package-lock.json"]
    new = set_versions({name: (ROOT / name).read_text() for name in names}, version)
    for name, text in new.items():
        (ROOT / name).write_text(text)
    print(f"Prepared {version}. Open a pull request titled: chore(release): v{version}")
    print(f"After it merges: git tag v{version} on main, then push the tag.")


def command_check(args):
    version = args.tag.removeprefix("v")
    problems = []
    for name in CARGO_MANIFESTS:
        found = re.search(r'(?m)^version = "([^"]+)"', (ROOT / name).read_text())
        if not found or found[1] != version:
            problems.append(f"{name} says {found[1] if found else 'nothing'}")
    for name in JSON_MANIFESTS:
        found = json.loads((ROOT / name).read_text()).get("version")
        if found != version:
            problems.append(f"{name} says {found}")
    if not any(heading.startswith(f"[{version}]") for heading, _ in read_sections()):
        problems.append(f"CHANGELOG.md has no section for {version}")
    if f"### {version}" not in README.read_text():
        problems.append(f"README.md has no section for {version}")
    if problems:
        raise SystemExit(f"{args.tag} does not match the repository: " + "; ".join(problems))
    print(f"{args.tag} matches the versions and the changelog.")


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
    check = commands.add_parser("check")
    check.add_argument("tag")
    check.set_defaults(run=command_check)
    args = parser.parse_args()
    args.run(args)


if __name__ == "__main__":
    main()
