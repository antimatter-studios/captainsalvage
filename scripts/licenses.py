#!/usr/bin/env python3
"""Keeps what the app is built from to licenses that suit a paid, closed app.

    scripts/licenses.py check     fail if anything shipped has a license outside the rule
    scripts/licenses.py write     rewrite THIRD-PARTY-LICENSES.md and the README's list
    scripts/licenses.py current   fail if either of those is out of date

The rule: every library that ends up in the app must be usable under a permissive license.
MIT is preferred; Apache-2.0, BSD, ISC, Zlib, Unicode and public-domain style licenses are
allowed too. MPL-2.0 is allowed for libraries used unmodified. Nothing under the GPL family.
A license given as "A OR B" passes when either side does; "A AND B" needs both.

Only what ships is checked: Rust crates the app links, not those used only to build or
test, and npm packages the window bundles, not the build tools.
"""

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
NOTICES = ROOT / "THIRD-PARTY-LICENSES.md"
README = ROOT / "README.md"
README_START = "<!-- dependencies:start -->"
README_END = "<!-- dependencies:end -->"

ALLOWED = {
    "MIT", "MIT-0", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "ISC", "Zlib", "0BSD",
    "Unicode-3.0", "Unicode-DFS-2016", "CC0-1.0", "Unlicense", "BSL-1.0", "MPL-2.0",
    # The bzip2 license is a BSD-style permissive license.
    "bzip2-1.0.6",
}
# Exceptions that only ever widen what a license allows.
EXCEPTIONS = {"LLVM-exception"}
LICENSE_FILES = re.compile(r"^(LICEN[CS]E|COPYING|NOTICE)", re.IGNORECASE)


def allowed(expression):
    """Whether an SPDX expression can be met with allowed licenses."""
    if not expression or not expression.strip():
        return False
    text = expression.replace("/", " OR ").replace("(", " ").replace(")", " ")
    text = re.sub(r"\+", "", text)
    for choice in re.split(r"\s+OR\s+", text.strip()):
        parts = [part.strip() for part in re.split(r"\s+AND\s+", choice)]
        if all(license_allowed(part) for part in parts if part):
            return True
    return False


def license_allowed(part):
    name, _, exception = part.partition(" WITH ")
    name = name.strip().removesuffix("-or-later").removesuffix("-only")
    return name in ALLOWED and (not exception or exception.strip() in EXCEPTIONS)


def rust_packages():
    """The crates linked into the app: reached through normal dependencies, not macros."""
    metadata = json.loads(subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=ROOT, capture_output=True, text=True, check=True,
    ).stdout)
    workspace = set(metadata["workspace_members"])
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    seen, waiting = set(), list(workspace)
    while waiting:
        current = waiting.pop()
        if current in seen:
            continue
        seen.add(current)
        if is_macro(packages[current]):
            continue
        for dependency in nodes[current]["deps"]:
            if any(kind["kind"] is None for kind in dependency["dep_kinds"]):
                waiting.append(dependency["pkg"])
    return [
        packages[found] for found in seen
        if found not in workspace and not is_macro(packages[found])
    ], [packages[found] for found in workspace]


def is_macro(package):
    return any("proc-macro" in target["kind"] for target in package["targets"])


def npm_packages():
    """The npm packages the window bundles: everything not marked as a development tool."""
    lock = json.loads((ROOT / "package-lock.json").read_text())
    found = []
    for path, package in lock.get("packages", {}).items():
        if not path or package.get("dev") or package.get("devOptional"):
            continue
        name = path.split("node_modules/")[-1]
        folder = ROOT / path
        license_name = package.get("license") or read_license_field(folder)
        found.append({"name": name, "version": package.get("version", ""), "license": license_name, "folder": folder})
    return found


def read_license_field(folder):
    try:
        return json.loads((folder / "package.json").read_text()).get("license") or "NONE"
    except (OSError, ValueError):
        return "NONE"


def check():
    rust, _ = rust_packages()
    problems = [
        f"{package['name']} {package['version']} (Rust): {package.get('license') or 'no license given'}"
        for package in rust if not allowed(package.get("license") or "")
    ] + [
        f"{package['name']} {package['version']} (npm): {package['license']}"
        for package in npm_packages() if not allowed(package["license"])
    ]
    for problem in sorted(problems):
        print(problem)
    print(f"licenses: {len(rust)} crates and {len(npm_packages())} npm packages, {len(problems)} outside the rule")
    return 1 if problems else 0


def license_texts(folder):
    texts = []
    try:
        names = sorted(path for path in Path(folder).iterdir() if LICENSE_FILES.match(path.name) and path.is_file())
    except OSError:
        return texts
    for path in names:
        try:
            # Trailing spaces carry no meaning in a license, and the repository refuses them.
            text = "\n".join(line.rstrip() for line in path.read_text(errors="replace").splitlines())
            texts.append((path.name, text.strip()))
        except OSError:
            continue
    return texts


def notices():
    rust, _ = rust_packages()
    lines = [
        "# Third-party licenses",
        "",
        "Captain Salvage is built with these libraries. Each is used under the license shown.",
        "Their license texts follow the list, each text once, numbered. `scripts/licenses.py",
        "write` keeps this file current.",
        "",
    ]
    texts = {}
    entries = [
        (package["name"], package["version"], package.get("license") or "", Path(package["manifest_path"]).parent)
        for package in rust
    ] + [
        (package["name"], package["version"], package["license"], package["folder"])
        for package in npm_packages()
    ]
    for name, version, license_name, folder in sorted(entries, key=lambda entry: (entry[0].lower(), entry[1])):
        numbers = []
        for _, text in license_texts(folder):
            numbers.append(texts.setdefault(text, len(texts) + 1))
        see = ", ".join(f"[text {number}](#text-{number})" for number in numbers) or "no license file shipped"
        lines.append(f"- **{name}** {version}, {license_name}: {see}")
    lines += ["", "## License texts", ""]
    for text, number in texts.items():
        lines += [f'<a id="text-{number}"></a>', f"### Text {number}", "", "```text", text, "```", ""]
    return "\n".join(lines).rstrip() + "\n"


def readme_block():
    """The libraries the app itself names, which are the ones chosen for it."""
    _, workspace = rust_packages()
    metadata_packages = {}
    rust, _ = rust_packages()
    for package in rust:
        metadata_packages[package["name"]] = package
    rows = []
    for member in workspace:
        for dependency in member["dependencies"]:
            if dependency.get("kind") is not None or dependency["name"] not in metadata_packages:
                continue
            package = metadata_packages[dependency["name"]]
            rows.append((package["name"], package["version"], package.get("license") or "", member["name"]))
    lock = json.loads((ROOT / "package.json").read_text())
    for name in sorted(lock.get("dependencies", {})):
        found = next((package for package in npm_packages() if package["name"] == name), None)
        if found:
            rows.append((name, found["version"], found["license"], "window"))
    users = {}
    for name, version, license_name, user in rows:
        users.setdefault((name, version, license_name), set()).add(user)
    table = ["| Library | Version | License | Used by |", "|---|---|---|---|"]
    table += [
        f"| `{name}` | {version} | {license_name} | {', '.join(sorted(used))} |"
        for (name, version, license_name), used in sorted(users.items())
    ]
    return "\n".join([README_START, *table, README_END])


def readme_text():
    text = README.read_text()
    start, end = text.find(README_START), text.find(README_END)
    if start < 0 or end < 0:
        raise SystemExit(f"README.md has no {README_START} block")
    return text[:start] + readme_block() + text[end + len(README_END):]


def main():
    command = sys.argv[1] if len(sys.argv) > 1 else "check"
    if command == "check":
        return check()
    if command == "write":
        NOTICES.write_text(notices())
        README.write_text(readme_text())
        print(f"wrote {NOTICES.name} and the README's list")
        return 0
    if command == "current":
        stale = [name for name, now, wanted in [
            (NOTICES.name, NOTICES.read_text() if NOTICES.exists() else "", notices()),
            (README.name, README.read_text(), readme_text()),
        ] if now != wanted]
        if stale:
            print("out of date, run scripts/licenses.py write: " + ", ".join(stale))
            return 1
        print("license notices are current")
        return 0
    raise SystemExit(__doc__)


if __name__ == "__main__":
    sys.exit(main())
