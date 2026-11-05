#!/usr/bin/env python3
"""Keeps the changelog, the version numbers and the release notes in step.

Release notes list the pull requests merged into a release, and nothing else. They
come from git history: this repository squashes every pull request into one commit
whose subject is the pull request's title, ending in (#number).

    scripts/release.py notes [--from REF] [--to REF]   print the notes for a range
    scripts/release.py sync                            refresh [Unreleased] and the README
    scripts/release.py prepare [VERSION]               start a release: bump and move notes
    scripts/release.py check v0.2.0                    fail unless a tag matches the files
    scripts/release.py version                         print this build's version
    scripts/release.py build                           build the app with that version

Version numbers: each GitHub release raises the minor number and resets the patch to
0. The patch counts the commits merged since that release, worked out when building, so
nothing is committed for it and pull requests never conflict over it. A rebuild of the
same code keeps the same number. Going to 1.0.0 is a deliberate choice: name it to prepare.
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


def build_version(manifest, latest_tag, commits_since_tag):
    """This build's version. The manifests hold X.Y.0, and the patch counts from X.Y's tag."""
    major, minor, _ = parse_version(manifest)
    if latest_tag and parse_version(latest_tag)[:2] == (major, minor):
        return f"{major}.{minor}.{commits_since_tag}"
    # Before the first release, or between preparing a release and tagging it.
    return f"{major}.{minor}.0"


def next_release_version(manifest, latest_tag):
    """The first release uses the manifests' number. Each later one raises the minor."""
    if not latest_tag:
        major, minor, _ = parse_version(manifest)
        return f"{major}.{minor}.0"
    major, minor, _ = parse_version(latest_tag)
    return f"{major}.{minor + 1}.0"


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


def current_build_version():
    tag = latest_tag()
    commits = int(git("rev-list", "--count", "--first-parent", f"{tag}..HEAD")) if tag else 0
    return build_version(manifest_version(), tag, commits)


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


def command_version(_):
    print(current_build_version())


def command_build(_):
    version = current_build_version()
    print(f"Building version {version}")
    config = json.dumps({"version": version})
    subprocess.run(["npx", "tauri", "build", "--bundles", "app", "--config", config], cwd=ROOT, check=True)


def command_prepare(args):
    version = args.version.removeprefix("v") if args.version else next_release_version(manifest_version(), latest_tag())
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
    if f"### v{version}" not in README.read_text():
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
    prepare.add_argument("version", nargs="?")
    prepare.set_defaults(run=command_prepare)
    check = commands.add_parser("check")
    check.add_argument("tag")
    check.set_defaults(run=command_check)
    commands.add_parser("version").set_defaults(run=command_version)
    commands.add_parser("build").set_defaults(run=command_build)
    args = parser.parse_args()
    args.run(args)


if __name__ == "__main__":
    main()
