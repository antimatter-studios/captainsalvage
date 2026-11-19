#!/usr/bin/env python3
"""Measures how much of our own code the tests run, and fails when a part falls below the baseline.

    scripts/coverage.py check               measure, compare with docs/coverage-baseline.json, exit 1 on a drop
    scripts/coverage.py baseline            measure and write docs/coverage-baseline.json (a deliberate act)
    scripts/coverage.py measure             measure and print the numbers
    ... --rust-json F --ts-lcov F           use measurements already made, so a comparison needs no test run

What is measured (issue #215), and only our own code, never a library or a test:

  rust_engine   crates/captainsalvage/src      `cargo llvm-cov --workspace`: lines, regions, functions
  rust_tauri    src-tauri/src                  the same run
  typescript    src/ (.ts and .tsx)            `node --test tests/*.mjs` with Node's own coverage:
                                               lines, branches, functions

The TypeScript number is of the files the tests load, which are few: the window's components are
driven by the window tests (tests/window), which CI cannot run. So the baseline also records how many
of the files in src/ the tests load and how many lines the rest hold, and the check holds the number
of files loaded from falling too. Two timing tests (the speed budgets) are left out of the
TypeScript run: they measure the machine and fail under instrumentation, and they load no file that
the others do not.

A part fails when it is more than TOLERANCE percentage points under the baseline. Going up never
fails. Raise the baseline on purpose with `baseline`, in the pull request that earned it.
"""

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BASELINE = ROOT / "docs/coverage-baseline.json"
# Percentage points a part may fall below the baseline before the check fails. Instrumented runs of
# the same code differ by a few hundredths of a point between machines (threads, timing, platform
# code); half a point is far above that and far below a real loss of tests.
TOLERANCE = 0.5
RUST_PARTS = {"rust_engine": "crates/captainsalvage/src/", "rust_tauri": "src-tauri/src/"}
RUST_METRICS = ("lines", "regions", "functions")
TS_METRICS = ("lines", "branches", "functions")
# Files the coverage never reads: tests, and the code of libraries and the standard library.
RUST_IGNORE = r"(/tests/|/tests\.rs$|/\.cargo/|/rustc/)"
# The speed budgets time the machine, so they say nothing under instrumentation and fail on it.
TS_SKIP = ("raw_disk_crowded_budget_test.mjs", "raw_disk_perf_test.mjs")


def percent(covered, count):
    return round(100.0 * covered / count, 2) if count else 100.0


def rust_numbers(summary):
    """Per part, per metric: covered, count and percent, from `cargo llvm-cov --json --summary-only`."""
    totals = {part: {m: [0, 0] for m in RUST_METRICS} for part in RUST_PARTS}
    for file in summary["data"][0]["files"]:
        name = file["filename"]
        relative = name[len(str(ROOT)) + 1:] if name.startswith(str(ROOT)) else name
        for part, prefix in RUST_PARTS.items():
            if relative.startswith(prefix):
                for metric in RUST_METRICS:
                    totals[part][metric][0] += file["summary"][metric]["covered"]
                    totals[part][metric][1] += file["summary"][metric]["count"]
    return {
        part: {m: {"covered": c, "count": n, "percent": percent(c, n)} for m, (c, n) in metrics.items()}
        for part, metrics in totals.items()
    }


def source_files():
    return sorted(p for pattern in ("*.ts", "*.tsx") for p in (ROOT / "src").rglob(pattern) if not p.name.endswith(".d.ts"))


def lcov_numbers(text):
    """The TypeScript numbers from an lcov report of the files the tests loaded, plus the size of the
    files they did not load."""
    sums = {m: [0, 0] for m in TS_METRICS}
    keys = {"lines": ("LH", "LF"), "branches": ("BRH", "BRF"), "functions": ("FNH", "FNF")}
    loaded = set()
    for line in text.splitlines():
        if line.startswith("SF:"):
            loaded.add(Path(line[3:]).as_posix().removeprefix(str(ROOT) + "/"))
            continue
        for metric, (hit, found) in keys.items():
            if line.startswith(hit + ":"):
                sums[metric][0] += int(line.split(":")[1])
            elif line.startswith(found + ":"):
                sums[metric][1] += int(line.split(":")[1])
    files = [p.relative_to(ROOT).as_posix() for p in source_files()]
    unloaded = [f for f in files if f not in loaded]
    unloaded_lines = sum(1 for f in unloaded for line in (ROOT / f).read_text().splitlines() if line.strip())
    numbers = {m: {"covered": c, "count": n, "percent": percent(c, n)} for m, (c, n) in sums.items()}
    numbers["files_loaded"] = {"covered": len(loaded), "count": len(files), "percent": percent(len(loaded), len(files))}
    numbers["unloaded_source_lines"] = unloaded_lines
    return numbers


def run(command, **options):
    print("+ " + " ".join(command), file=sys.stderr, flush=True)
    return subprocess.run(command, cwd=ROOT, check=True, **options)


def measure(rust_json=None, ts_lcov=None, scratch=None):
    scratch = Path(scratch or ROOT / "target/coverage")
    scratch.mkdir(parents=True, exist_ok=True)
    if rust_json is None:
        rust_json = scratch / "rust.json"
        run(["cargo", "llvm-cov", "--workspace", "--locked", "--json", "--summary-only",
             "--ignore-filename-regex", RUST_IGNORE, "--output-path", str(rust_json)])
    if ts_lcov is None:
        ts_lcov = scratch / "typescript.lcov"
        tests = sorted(str(p.relative_to(ROOT)) for p in (ROOT / "tests").glob("*.mjs") if p.name not in TS_SKIP)
        run(["node", "--test", "--experimental-test-coverage", "--test-coverage-include=src/**",
             "--test-reporter=spec", "--test-reporter-destination=stderr",
             "--test-reporter=lcov", f"--test-reporter-destination={ts_lcov}", *tests])
    rust = rust_numbers(json.loads(Path(rust_json).read_text()))
    return {**rust, "typescript": lcov_numbers(Path(ts_lcov).read_text())}


def comparisons(baseline, current):
    """Every number the gate holds, as (name, baseline, current, allowed drop, unit). Percentages may
    fall by TOLERANCE; the number of source files the tests load may not fall at all, and does not
    count against the tests when new files are added."""
    for part in (*RUST_PARTS, "typescript"):
        for metric in RUST_METRICS if part in RUST_PARTS else TS_METRICS:
            yield (f"{part} {metric}", baseline[part][metric]["percent"], current[part][metric]["percent"], TOLERANCE, "%")
    yield ("typescript files loaded by the tests", baseline["typescript"]["files_loaded"]["covered"],
           current["typescript"]["files_loaded"]["covered"], 0, " files")


def failures(baseline, current):
    found = []
    for name, was, now, allowed, unit in comparisons(baseline, current):
        if now < was - allowed:
            found.append(f"{name}: {now:g}{unit} is {was - now:.2f} under the baseline {was:g}{unit} (allowed: {allowed:g})")
    return found


def table(numbers):
    rows = []
    for part in (*RUST_PARTS, "typescript"):
        metrics = RUST_METRICS if part in RUST_PARTS else (*TS_METRICS, "files_loaded")
        for metric in metrics:
            item = numbers[part][metric]
            rows.append(f"{part:12} {metric:13} {item['percent']:6.2f}%  ({item['covered']} of {item['count']})")
    rows.append(f"typescript   lines in src/ files no test loads: {numbers['typescript']['unloaded_source_lines']}")
    return "\n".join(rows)


def document(numbers):
    return {
        "about": "What the tests run of our own code. Written by `scripts/coverage.py baseline`; checked by "
                 "`scripts/coverage.py check`, which fails when a part is more than the tolerance under it.",
        "tolerance_points": TOLERANCE,
        "tools": {"rust": "cargo llvm-cov (1.95.0, workspace)", "typescript": "node --test --experimental-test-coverage"},
        **numbers,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("command", choices=("check", "baseline", "measure"))
    parser.add_argument("--rust-json", help="A `cargo llvm-cov --json --summary-only` report to use instead of running the tests.")
    parser.add_argument("--ts-lcov", help="An lcov report of the TypeScript tests to use instead of running them.")
    args = parser.parse_args()
    current = measure(args.rust_json, args.ts_lcov)
    print(table(current))
    if args.command == "baseline":
        BASELINE.write_text(json.dumps(document(current), indent=2) + "\n")
        print(f"wrote {BASELINE.relative_to(ROOT)}")
    elif args.command == "check":
        baseline = json.loads(BASELINE.read_text())
        found = failures(baseline, current)
        if found:
            print("\nCoverage is under the baseline:\n  " + "\n  ".join(found), file=sys.stderr)
            print("Add the tests that were lost. If the drop is meant (code removed with its tests), run "
                  "`scripts/coverage.py baseline` in the same pull request and say why.", file=sys.stderr)
            sys.exit(1)
        print(f"\nCoverage holds: no part is more than {TOLERANCE} points under docs/coverage-baseline.json.")


if __name__ == "__main__":
    main()
