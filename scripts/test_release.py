#!/usr/bin/env python3
"""Tests for release.py. Needs no git history and no network."""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import release  # noqa: E402


class Subjects(unittest.TestCase):
    def test_a_squashed_pull_request_is_read(self):
        pull = release.parse_subject("feat: play more formats through ffmpeg (#31)")
        self.assertEqual(pull, {"number": 31, "section": "Added", "title": "Play more formats through ffmpeg"})

    def test_each_type_lands_in_its_section(self):
        self.assertEqual(release.parse_subject("fix: a thing (#2)")["section"], "Fixed")
        self.assertEqual(release.parse_subject("docs: a thing (#3)")["section"], "Documentation")
        self.assertEqual(release.parse_subject("ci: a thing (#4)")["section"], "Other")
        self.assertEqual(release.parse_subject("no prefix at all (#5)")["section"], "Other")

    def test_a_commit_without_a_pull_request_is_left_out(self):
        self.assertIsNone(release.parse_subject("docs: add the README"))

    def test_the_release_commit_itself_is_left_out(self):
        self.assertIsNone(release.parse_subject("chore(release): v0.2.0 (#40)"))


class Notes(unittest.TestCase):
    def test_only_the_pull_requests_given_appear(self):
        pulls = [release.parse_subject("feat: one (#1)"), release.parse_subject("fix: two (#2)")]
        text = release.render_notes(pulls, repo="o/r")
        self.assertIn("### Added\n\n- One ([#1](https://github.com/o/r/pull/1))", text)
        self.assertIn("### Fixed\n\n- Two ([#2](https://github.com/o/r/pull/2))", text)
        self.assertNotIn("### Other", text)
        self.assertNotIn("Full Changelog", text)

    def test_an_empty_range_says_so(self):
        self.assertEqual(release.render_notes([]), release.NO_CHANGES + "\n")


class Versions(unittest.TestCase):
    def test_a_feature_raises_the_minor_and_resets_the_patch(self):
        self.assertEqual(release.next_version("0.1.0", "v0.2.3", ["feat: a thing (#1)"]), ("0.3.0", True))

    def test_a_fix_raises_the_patch(self):
        self.assertEqual(release.next_version("0.1.0", "v0.2.3", ["fix: a thing (#1)"]), ("0.2.4", True))

    def test_several_merges_take_the_largest_change_once(self):
        subjects = ["fix: one (#1)", "feat: two (#2)", "feat: three (#3)", "fix: four (#4)"]
        self.assertEqual(release.next_version("0.1.0", "v0.2.3", subjects), ("0.3.0", True))

    def test_anything_else_releases_nothing(self):
        subjects = ["docs: a (#1)", "chore: b (#2)", "test: c (#3)", "ci: d (#4)", "no prefix (#5)"]
        self.assertEqual(release.next_version("0.1.0", "v0.2.3", subjects), ("0.2.3", False))

    def test_the_same_code_keeps_the_same_number(self):
        self.assertEqual(release.next_version("0.1.0", "v0.2.3", []), ("0.2.3", False))

    def test_a_commit_from_no_pull_request_counts_for_nothing(self):
        self.assertEqual(release.next_version("0.1.0", "v0.2.3", ["feat: pushed directly"]), ("0.2.3", False))

    def test_the_first_release_uses_the_number_it_has(self):
        self.assertEqual(release.next_version("0.1.0", None, []), ("0.1.0", True))

    def test_a_named_version_above_the_last_tag_is_released_as_it_is(self):
        self.assertEqual(release.next_version("1.0.0", "v0.9.2", ["chore(release): v1.0.0 (#9)"]), ("1.0.0", True))

    def test_a_version_named_earlier_does_not_hold_later_ones_back(self):
        self.assertEqual(release.next_version("1.0.0", "v1.0.0", ["feat: a thing (#1)"]), ("1.1.0", True))

    def test_a_breaking_change_is_a_minor_before_one_and_a_major_after(self):
        self.assertEqual(release.next_version("0.1.0", "v0.2.3", ["feat!: a thing (#1)"]), ("0.3.0", True))
        self.assertEqual(release.next_version("1.0.0", "v1.2.3", ["fix!: a thing (#1)"]), ("2.0.0", True))


class Files(unittest.TestCase):
    SECTIONS = [(f"[0.{n}.0] — 2026-01-01", "### Added\n\n- A thing") for n in range(12, 0, -1)]

    def test_the_changelog_survives_a_round_trip(self):
        text = release.join_changelog(self.SECTIONS)
        self.assertEqual(release.split_sections(text), self.SECTIONS)

    def test_the_readme_keeps_ten_versions_and_links_to_the_rest(self):
        block = release.readme_block(self.SECTIONS)
        self.assertEqual(block.count("\n### "), 10)
        self.assertIn("### v0.12.0 — 2026-01-01", block)
        self.assertNotIn("### v0.2.0", block)
        self.assertIn("CHANGELOG.md", block)
        self.assertIn("**Added**", block)

    def test_unreleased_does_not_push_out_a_version(self):
        block = release.readme_block([(release.UNRELEASED, "Nothing yet."), *self.SECTIONS])
        self.assertIn("### Unreleased", block)
        self.assertEqual(block.count("\n### v"), 10)
        self.assertIn("### v0.3.0", block)
        self.assertNotIn("### v0.2.0", block)

    def test_the_changelog_keeps_every_version(self):
        sections = [(release.UNRELEASED, "Nothing yet."), *self.SECTIONS]
        self.assertEqual(release.split_sections(release.join_changelog(sections)), sections)

    def test_a_versions_screenshots_follow_its_notes(self):
        with tempfile.TemporaryDirectory() as folder:
            shots = Path(folder) / "v0.12.0"
            shots.mkdir()
            (shots / release.SCREENSHOT_TABLE).write_text("| a | b |\n")
            block = release.readme_block(self.SECTIONS, screenshots=Path(folder))
        self.assertEqual(block.count("**Screenshots**"), 1)
        self.assertLess(block.index("### v0.12.0"), block.index("| a | b |"))
        self.assertLess(block.index("| a | b |"), block.index("### v0.11.0"))

    def test_unreleased_has_no_version_mark(self):
        block = release.readme_block([("[Unreleased]", "Nothing yet.")])
        self.assertIn("### Unreleased", block)

    def test_the_readme_changes_only_between_its_markers(self):
        old = f"before\n{release.README_START}\nold\n{release.README_END}\nafter\n"
        new = release.replace_readme_block(old, f"{release.README_START}\nnew\n{release.README_END}")
        self.assertEqual(new, f"before\n{release.README_START}\nnew\n{release.README_END}\nafter\n")

    def test_the_version_file_must_hold_one_plain_version(self):
        with tempfile.TemporaryDirectory() as folder:
            original = release.ROOT
            release.ROOT = Path(folder)
            try:
                (Path(folder) / release.VERSION_FILE).write_text("0.2\n")
                with self.assertRaises(SystemExit):
                    release.source_version()
                (Path(folder) / release.VERSION_FILE).write_text("0.2.0\n")
                self.assertEqual(release.source_version(), "0.2.0")
            finally:
                release.ROOT = original

    def test_every_version_number_moves_and_nothing_else(self):
        files = {
            "x/Cargo.toml": '[package]\nversion = "0.1.0"\n\n[dependencies]\nserde = { version = "1.0.0" }\nother = "1"\n',
            "package.json": '{\n  "name": "a",\n  "version": "0.1.0",\n  "dep": { "version": "9" }\n}\n',
            "Cargo.lock": 'name = "salvage-core"\nversion = "0.1.0"\n\nname = "serde"\nversion = "1.0.0"\n',
            "package-lock.json": '{"version": "0.1.0", "packages": {"": {"version": "0.1.0"}, "a": {"version": "9"}}}',
        }
        new = release.set_versions(files, "0.2.0")
        self.assertIn('[package]\nversion = "0.2.0"', new["x/Cargo.toml"])
        self.assertIn('serde = { version = "1.0.0" }', new["x/Cargo.toml"])
        self.assertIn('"dep": { "version": "9" }', new["package.json"])
        self.assertIn('name = "salvage-core"\nversion = "0.2.0"', new["Cargo.lock"])
        self.assertIn('name = "serde"\nversion = "1.0.0"', new["Cargo.lock"])
        self.assertEqual(new["package-lock.json"].count('"0.2.0"'), 2)
        self.assertIn('"a": {"version": "9"}', new["package-lock.json"])


class Install(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.source = self.root / "build" / release.APP_NAME
        self.write(self.source, "new")
        self.folder = self.root / "Applications"

    def tearDown(self):
        self.temp.cleanup()

    def write(self, app, text):
        (app / "Contents/MacOS").mkdir(parents=True, exist_ok=True)
        (app / "Contents/Info.plist").write_text("<plist/>")
        (app / "Contents/MacOS/captain-salvage").write_text(text)

    def binary(self, app):
        return (app / "Contents/MacOS/captain-salvage").read_text()

    def test_a_first_install_makes_the_folder_and_copies_the_app(self):
        target = release.install_app(self.source, self.folder, [])
        self.assertEqual(target, self.folder / release.APP_NAME)
        self.assertEqual(self.binary(target), "new")

    def test_an_install_replaces_the_old_copy_whole(self):
        old = self.folder / release.APP_NAME
        self.write(old, "old")
        (old / "Contents/stale").write_text("left over")
        release.install_app(self.source, self.folder, [])
        self.assertEqual(self.binary(old), "new")
        self.assertFalse((old / "Contents/stale").exists())
        self.assertEqual(sorted(path.name for path in self.folder.iterdir()), [release.APP_NAME])

    def test_a_running_copy_is_left_alone(self):
        old = self.folder / release.APP_NAME
        self.write(old, "old")
        running = [str(old / "Contents/MacOS/captain-salvage")]
        with self.assertRaises(SystemExit):
            release.install_app(self.source, self.folder, running)
        self.assertEqual(self.binary(old), "old")

    def test_a_copy_running_from_the_build_does_not_stop_an_install(self):
        running = [str(self.source / "Contents/MacOS/captain-salvage")]
        self.assertEqual(self.binary(release.install_app(self.source, self.folder, running)), "new")

    def test_there_must_be_a_build(self):
        with self.assertRaises(SystemExit) as raised:
            release.install_app(self.root / "missing" / release.APP_NAME, self.folder, [])
        self.assertIn("npm run app", str(raised.exception))

    def test_a_failed_copy_keeps_the_old_one(self):
        old = self.folder / release.APP_NAME
        self.write(old, "old")
        failing = subprocess.CalledProcessError(1, "ditto")
        real = subprocess.run
        subprocess.run = lambda *a, **k: (_ for _ in ()).throw(failing)
        try:
            with self.assertRaises(subprocess.CalledProcessError):
                release.install_app(self.source, self.folder, [])
        finally:
            subprocess.run = real
        self.assertEqual(self.binary(old), "old")


if __name__ == "__main__":
    unittest.main()
