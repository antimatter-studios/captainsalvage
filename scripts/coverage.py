#!/usr/bin/env python3
"""Measures how much of our own code the tests run, and fails when a part falls below the baseline.

    scripts/coverage.py check               measure, compare with docs/coverage-baseline.json, exit 1 on a drop
    scripts/coverage.py baseline --from-log F   write docs/coverage-baseline.json from the table a CI run printed
    scripts/coverage.py baseline --local    measure here and write it (only for a baseline meant for this machine)
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

The baseline is CI's number, not this machine's. CI skips the tests that need local tools (ffmpeg, the
oracle tools), so a Mac measures about 0.8 points more of the Rust engine's functions than CI does, and
a baseline written here would fail the check in CI. So `baseline` measures only inside CI (where the CI
variable is set), unless `--local` says that a local number is meant. To raise the baseline from a pull
request's own CI run, take the table its coverage job printed:

    gh run view <run id> --log --job <coverage job id> | scripts/coverage.py baseline --from-log -

`--from-log` reads the table the check prints (a file, or `-` for standard input; the last table in it
is used, and a prefix on each line such as a timestamp is ignored) and writes the baseline from it. It
measures nothing, so it works anywhere.
"""

import argparse
import json
import os
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


TABLE_ROW = re.compile(r"(rust_engine|rust_tauri|typescript)\s+(lines|regions|functions|branches|files_loaded)\s+"
                       r"[0-9.]+%\s+\((\d+) of (\d+)\)")
TABLE_UNLOADED = re.compile(r"typescript\s+lines in src/ files no test loads:\s+(\d+)")


def numbers_from_log(text):
    """The numbers `table` printed, read back. The percent is worked out again from covered and count, as
    `percent` does when measuring, so a baseline written from a log is the same as one measured there.
    When the log holds several tables (a rerun, or the table printed twice) the last one wins."""
    numbers = {}
    for line in text.splitlines():
        row = TABLE_ROW.search(line)
        if row:
            part, metric, covered, count = row.group(1), row.group(2), int(row.group(3)), int(row.group(4))
            numbers.setdefault(part, {})[metric] = {"covered": covered, "count": count, "percent": percent(covered, count)}
        unloaded = TABLE_UNLOADED.search(line)
        if unloaded:
            numbers.setdefault("typescript", {})["unloaded_source_lines"] = int(unloaded.group(1))
    missing = []
    for part in (*RUST_PARTS, "typescript"):
        wanted = RUST_METRICS if part in RUST_PARTS else (*TS_METRICS, "files_loaded", "unloaded_source_lines")
        missing += [f"{part} {m}" for m in wanted if m not in numbers.get(part, {})]
    if missing:
        raise ValueError("the log does not hold the whole table; missing: " + ", ".join(missing))
    return {part: numbers[part] for part in (*RUST_PARTS, "typescript")}


def running_in_ci():
    return bool(os.environ.get("CI"))


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
    parser.add_argument("--from-log", metavar="FILE", help="With `baseline`: write the baseline from the table a CI run printed "
                        "(a file, or - for standard input) instead of measuring.")
    parser.add_argument("--local", action="store_true", help="With `baseline`: measure here and write the baseline although this is "
                        "not CI. CI measures less (it skips the tests that need local tools), so a local baseline can fail it.")
    args = parser.parse_args()
    if args.from_log and args.command != "baseline":
        parser.error("--from-log is for `baseline`")
    if args.local and args.command != "baseline":
        parser.error("--local is for `baseline`")
    if args.from_log and args.local:
        parser.error("--from-log takes CI's numbers; --local measures here. Use one.")
    if args.command == "baseline" and args.from_log:
        text = sys.stdin.read() if args.from_log == "-" else Path(args.from_log).read_text()
        try:
            current = numbers_from_log(text)
        except ValueError as error:
            sys.exit(f"coverage: {error}")
        print(table(current))
        BASELINE.write_text(json.dumps(document(current), indent=2) + "\n")
        print(f"wrote {BASELINE.relative_to(ROOT)} from the log")
        return
    if args.command == "baseline" and not args.local and not running_in_ci():
        sys.exit("coverage: `baseline` measures only in CI. CI skips the tests that need local tools, so a baseline "
                 "measured here is higher than CI's and the check then fails in CI.\n"
                 "Write it from CI's numbers:  gh run view <run id> --log --job <coverage job id> | "
                 "scripts/coverage.py baseline --from-log -\n"
                 "or pass --local if a local baseline is meant.")
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
                  "`scripts/coverage.py baseline --from-log <this job's log>` in the same pull request and say why.", file=sys.stderr)
            sys.exit(1)
        print(f"\nCoverage holds: no part is more than {TOLERANCE} points under docs/coverage-baseline.json.")


if __name__ == "__main__":
    main()
