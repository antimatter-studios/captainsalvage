#!/usr/bin/env python3
"""Tests for scripts/coverage.py: the grouping of a report into our parts, and the gate."""
import copy
import json
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


if __name__ == "__main__":
    unittest.main()
