#!/usr/bin/env python3
"""Keeps the changelog, the version numbers and the release notes in step.

Release notes list the pull requests merged into a release, and nothing else. They
come from git history: this repository squashes every pull request into one commit
whose subject is the pull request's title, ending in (#number).

    scripts/release.py notes [--from REF] [--to REF]   print pull requests for a range
    scripts/release.py sync                            rebuild the README's list from CHANGELOG.md
    scripts/release.py prepare [VERSION]               gather the fragments and set the version
    scripts/release.py contents [VERSION]              print the next version and what is in it
    scripts/release.py section VERSION                 print a version's CHANGELOG.md section
    scripts/release.py check v0.2.0                    fail unless a tag matches the files
    scripts/release.py version                         print this build's version
    scripts/release.py build                           build the app with that version
    scripts/release.py install user|system             copy the build to ~/Applications or /Applications

scripts/create-release is what you run to cut a release: it calls these.

Version numbers follow what was merged since the last tag. A new feature raises the minor
number and resets the patch. A fix raises the patch. Anything else moves nothing. Several
merges take the largest of these, once. So the number is worked out, and a build shows the
number that code would be released as. Going to 1.0.0 is a deliberate choice: name it.

Unreleased changes are fragments in changelog.d/. A release gathers them into CHANGELOG.md, and
the README's list is a copy of the newest versions there, so the two cannot disagree.
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
# The one file that says the version. Everything else is copied from it by `prepare`.
VERSION_FILE = "VERSION"
CARGO_MANIFESTS = ["crates/salvage-core/Cargo.toml", "src-tauri/Cargo.toml"]
JSON_MANIFESTS = ["src-tauri/tauri.conf.json", "package.json"]
LOCKED_CRATES = ["captain-salvage", "salvage-core"]
APP_NAME = "Captain Salvage.app"
BUILT_APP = ROOT / "target/release/bundle/macos" / APP_NAME
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


def source_version():
    """The one version the project carries, from VERSION. Every manifest copies it."""
    text = (ROOT / VERSION_FILE).read_text().strip()
    if not re.fullmatch(r"\d+\.\d+\.\d+", text):
        raise SystemExit(f"{VERSION_FILE} must hold one version such as 0.2.0, not {text!r}")
    return text


def latest_tag():
    done = subprocess.run(
        ["git", "describe", "--tags", "--abbrev=0", "--match", "v[0-9]*", "HEAD"],
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
        ["git", "describe", "--tags", "--abbrev=0", "--match", "v[0-9]*", ref],
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
    target = Path(os.environ.get("CARGO_TARGET_DIR") or ROOT / "target")
    subprocess.run([str(ROOT / "macos/finder/package.sh"), str(target / "release/bundle/macos" / APP_NAME)], cwd=ROOT, check=True)


def command_prepare(args):
    if args.version:
        version = args.version.removeprefix("v")
    else:
        version, release = current_version()
        if not release:
            raise SystemExit("Nothing to release: no feat or fix has merged since the last tag.")
    subprocess.run(["bash", str(ROOT / "scripts/changelog-gather.sh"), version], cwd=ROOT, check=True)
    write_readme()
    write_versions(version)


def command_check(args):
    """Every manifest and lock file must carry VERSION. Given a tag, the tag must be it too,
    and the changelog and README must describe it. Runs on every pull request, so drift
    fails before it merges."""
    version = source_version()
    problems = []
    for name, found in manifest_versions():
        if found != version:
            problems.append(f"{name} says {found}")
    if args.tag:
        tag = args.tag.removeprefix("v")
        if tag != version:
            problems.append(f"the tag is {tag}, but {VERSION_FILE} says {version}")
        if not any(heading.startswith(f"[{version}]") for heading, _ in read_sections()):
            problems.append(f"CHANGELOG.md has no section for {version}")
        if f"### v{version}" not in README.read_text():
            problems.append(f"README.md has no section for {version}")
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


def command_install(args):
    folder = INSTALL_FOLDERS[args.where]
    target = install_app(BUILT_APP, folder, running_programs())
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
    commands.add_parser("sync").set_defaults(run=command_sync)
    prepare = commands.add_parser("prepare")
    prepare.add_argument("version", nargs="?")
    prepare.set_defaults(run=command_prepare)
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
