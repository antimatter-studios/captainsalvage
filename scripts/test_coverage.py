#!/usr/bin/env python3
"""Tests for scripts/coverage.py: the grouping of a report into our parts, and the gate."""
import copy
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import coverage  # noqa: E402


def rust_report(files):
    return {"data": [{"files": [
        {"filename": f"{coverage.ROOT}/{name}",
         "summary": {m: {"covered": c, "count": n} for m in coverage.RUST_METRICS}}
        for name, c, n in files
    ]}]}


class Rust(unittest.TestCase):
    def test_files_are_grouped_by_the_part_they_belong_to(self):
        numbers = coverage.rust_numbers(rust_report([
            ("crates/captainsalvage/src/a.rs", 8, 10),
            ("crates/captainsalvage/src/b/c.rs", 2, 10),
            ("src-tauri/src/lib.rs", 0, 5),
            ("somewhere/else.rs", 99, 99),
        ]))
        self.assertEqual(numbers["rust_engine"]["lines"], {"covered": 10, "count": 20, "percent": 50.0})
        self.assertEqual(numbers["rust_tauri"]["regions"], {"covered": 0, "count": 5, "percent": 0.0})

    def test_a_part_with_nothing_in_it_is_whole(self):
        self.assertEqual(coverage.rust_numbers(rust_report([]))["rust_tauri"]["lines"]["percent"], 100.0)


class TypeScript(unittest.TestCase):
    def test_the_loaded_files_are_counted_and_the_rest_are_sized(self):
        lcov = "SF:src/stale.ts\nFNF:2\nFNH:1\nBRF:4\nBRH:3\nLF:10\nLH:9\nend_of_record\n"
        numbers = coverage.lcov_numbers(lcov)
        self.assertEqual(numbers["lines"], {"covered": 9, "count": 10, "percent": 90.0})
        self.assertEqual(numbers["branches"]["percent"], 75.0)
        self.assertEqual(numbers["functions"]["percent"], 50.0)
        self.assertEqual(numbers["files_loaded"]["covered"], 1)
        self.assertGreater(numbers["files_loaded"]["count"], 10)
        self.assertGreater(numbers["unloaded_source_lines"], 1000)


class Gate(unittest.TestCase):
    def numbers(self):
        item = lambda p, c=1, n=1: {"covered": c, "count": n, "percent": p}
        return {
            "rust_engine": {m: item(80.0) for m in coverage.RUST_METRICS},
            "rust_tauri": {m: item(1.0) for m in coverage.RUST_METRICS},
            "typescript": {**{m: item(70.0) for m in coverage.TS_METRICS}, "files_loaded": item(12.0, 6, 49)},
        }

    def test_the_same_numbers_pass(self):
        self.assertEqual(coverage.failures(self.numbers(), self.numbers()), [])

    def test_a_small_wobble_and_any_rise_pass(self):
        now = self.numbers()
        now["rust_engine"]["lines"]["percent"] = 79.6
        now["typescript"]["lines"]["percent"] = 90.0
        self.assertEqual(coverage.failures(self.numbers(), now), [])

    def test_a_drop_past_the_tolerance_names_the_part(self):
        now = self.numbers()
        now["rust_engine"]["regions"]["percent"] = 79.0
        found = coverage.failures(self.numbers(), now)
        self.assertEqual(len(found), 1)
        self.assertIn("rust_engine regions", found[0])

    def test_each_part_is_held_on_its_own(self):
        now = self.numbers()
        for part in ("rust_tauri", "typescript"):
            now[part]["lines"]["percent"] = 0.0
        self.assertEqual(len(coverage.failures(self.numbers(), now)), 2)

    def test_the_files_the_tests_load_may_not_fall_but_new_files_do_not_count_against_them(self):
        now = self.numbers()
        now["typescript"]["files_loaded"] = {"covered": 6, "count": 80, "percent": 7.5}
        self.assertEqual(coverage.failures(self.numbers(), now), [])
        now["typescript"]["files_loaded"]["covered"] = 5
        self.assertIn("files loaded", coverage.failures(self.numbers(), now)[0])

    def test_the_committed_baseline_is_in_the_shape_the_gate_reads(self):
        baseline = json.loads(coverage.BASELINE.read_text())
        self.assertEqual(coverage.failures(baseline, copy.deepcopy(baseline)), [])
        self.assertEqual(baseline["tolerance_points"], coverage.TOLERANCE)


class BaselineFromCi(unittest.TestCase):
    def log(self, prefix=""):
        numbers = json.loads(coverage.BASELINE.read_text())
        return "\n".join(prefix + line for line in coverage.table(numbers).splitlines()), numbers

    def test_the_table_the_check_prints_is_read_back_into_the_same_numbers(self):
        text, numbers = self.log()
        read = coverage.numbers_from_log(text)
        for part in ("rust_engine", "rust_tauri", "typescript"):
            self.assertEqual(read[part], {k: v for k, v in numbers[part].items()})

    def test_a_prefix_on_each_line_and_other_output_around_the_table_are_ignored(self):
        text, numbers = self.log("coverage\tRun check\t2026-11-19T10:00:00.123456Z ")
        noisy = "warning: something\n" + text + "\nCoverage holds: no part is more than 0.5 points under it.\n"
        self.assertEqual(coverage.numbers_from_log(noisy)["rust_engine"], numbers["rust_engine"])

    def test_the_last_table_wins(self):
        text, numbers = self.log()
        first = text.replace(f"({numbers['rust_engine']['lines']['covered']} of", "(1 of")
        self.assertEqual(coverage.numbers_from_log(first + "\n" + text)["rust_engine"]["lines"], numbers["rust_engine"]["lines"])

    def test_a_log_without_the_whole_table_is_refused_and_says_what_is_missing(self):
        text, _ = self.log()
        cut = "\n".join(line for line in text.splitlines() if not line.startswith("rust_tauri"))
        with self.assertRaises(ValueError) as caught:
            coverage.numbers_from_log(cut)
        self.assertIn("rust_tauri lines", str(caught.exception))
        with self.assertRaises(ValueError):
            coverage.numbers_from_log("nothing here")

    def run_script(self, *arguments, ci=False, stdin=None):
        env = {k: v for k, v in os.environ.items() if k != "CI"}
        if ci:
            env["CI"] = "true"
        return subprocess.run([sys.executable, str(Path(coverage.__file__)), *arguments], env=env, input=stdin,
                              capture_output=True, text=True)

    def test_baseline_refuses_outside_ci_without_local_and_changes_nothing(self):
        before = coverage.BASELINE.read_text()
        done = self.run_script("baseline")
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("only in CI", done.stderr)
        self.assertIn("--from-log", done.stderr)
        self.assertEqual(coverage.BASELINE.read_text(), before)

    def test_from_log_writes_the_baseline_without_measuring_and_anywhere(self):
        text, numbers = self.log()
        before = coverage.BASELINE.read_text()
        try:
            done = self.run_script("baseline", "--from-log", "-", stdin=text)
            self.assertEqual(done.returncode, 0, done.stderr)
            written = json.loads(coverage.BASELINE.read_text())
            for part in ("rust_engine", "rust_tauri", "typescript"):
                self.assertEqual(written[part], numbers[part])
        finally:
            coverage.BASELINE.write_text(before)

    def test_from_log_with_a_broken_log_changes_nothing(self):
        before = coverage.BASELINE.read_text()
        done = self.run_script("baseline", "--from-log", "-", stdin="no table")
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("missing", done.stderr)
        self.assertEqual(coverage.BASELINE.read_text(), before)

    def test_baseline_measures_in_ci_and_with_local_but_not_otherwise(self):
        before = coverage.BASELINE.read_text()
        with tempfile.TemporaryDirectory() as scratch:
            rust, lcov = Path(scratch) / "rust.json", Path(scratch) / "ts.lcov"
            rust.write_text(json.dumps(rust_report([("crates/captainsalvage/src/a.rs", 3, 4)])))
            lcov.write_text("SF:src/stale.ts\nFNF:1\nFNH:1\nBRF:1\nBRH:1\nLF:1\nLH:1\nend_of_record\n")
            given = ("baseline", "--rust-json", str(rust), "--ts-lcov", str(lcov))
            try:
                self.assertNotEqual(self.run_script(*given).returncode, 0)
                self.assertEqual(coverage.BASELINE.read_text(), before)
                for run in (lambda: self.run_script(*given, ci=True), lambda: self.run_script(*given, "--local")):
                    done = run()
                    self.assertEqual(done.returncode, 0, done.stderr)
                    self.assertEqual(json.loads(coverage.BASELINE.read_text())["rust_engine"]["lines"]["percent"], 75.0)
                    coverage.BASELINE.write_text(before)
            finally:
                coverage.BASELINE.write_text(before)

    def test_the_options_belong_to_baseline_and_do_not_mix(self):
        for arguments in (("check", "--from-log", "-"), ("check", "--local"), ("baseline", "--local", "--from-log", "-")):
            self.assertNotEqual(self.run_script(*arguments, stdin="").returncode, 0, arguments)


if __name__ == "__main__":
    unittest.main()
