#!/usr/bin/env python3
"""Tests for release.py. Needs no git history and no network."""

import argparse
import contextlib
import fnmatch
import io
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
            "Cargo.lock": 'name = "captainsalvage"\nversion = "0.1.0"\n\nname = "serde"\nversion = "1.0.0"\n',
            "package-lock.json": '{"version": "0.1.0", "packages": {"": {"version": "0.1.0"}, "a": {"version": "9"}}}',
        }
        new = release.set_versions(files, "0.2.0")
        self.assertIn('[package]\nversion = "0.2.0"', new["x/Cargo.toml"])
        self.assertIn('serde = { version = "1.0.0" }', new["x/Cargo.toml"])
        self.assertIn('"dep": { "version": "9" }', new["package.json"])
        self.assertIn('name = "captainsalvage"\nversion = "0.2.0"', new["Cargo.lock"])
        self.assertIn('name = "serde"\nversion = "1.0.0"', new["Cargo.lock"])
        self.assertEqual(new["package-lock.json"].count('"0.2.0"'), 2)
        self.assertIn('"a": {"version": "9"}', new["package-lock.json"])


class Candidates(unittest.TestCase):
    """A release candidate, 0.4.0-rc1, is a release's content under a version that is not released."""

    def test_a_candidate_parses_as_the_release_it_belongs_to(self):
        self.assertEqual(release.parse_version("v0.4.0-rc2"), (0, 4, 0))
        self.assertEqual(release.parse_version("0.4.0-rc12"), (0, 4, 0))
        self.assertEqual(release.parse_version("v0.4.0"), (0, 4, 0))

    def test_the_base_version_drops_the_candidate(self):
        self.assertEqual(release.base_version("v0.4.0-rc1"), "0.4.0")
        self.assertEqual(release.base_version("0.4.0"), "0.4.0")
        self.assertTrue(release.is_candidate("v0.4.0-rc1"))
        self.assertFalse(release.is_candidate("v0.4.0"))

    def test_the_first_candidate_is_one(self):
        self.assertEqual(release.next_candidate("0.4.0", ["v0.3.0"]), "0.4.0-rc1")

    def test_each_candidate_is_one_past_the_last(self):
        self.assertEqual(release.next_candidate("0.4.0", ["v0.4.0-rc1"]), "0.4.0-rc2")
        self.assertEqual(release.next_candidate("0.4.0", ["v0.4.0-rc1", "v0.4.0-rc2"]), "0.4.0-rc3")

    def test_a_gap_is_not_filled(self):
        self.assertEqual(release.next_candidate("0.4.0", ["v0.4.0-rc1", "v0.4.0-rc4"]), "0.4.0-rc5")

    def test_ten_comes_after_nine(self):
        tags = [f"v0.4.0-rc{n}" for n in range(1, 10)] + ["v0.4.0-rc10"]
        self.assertEqual(release.next_candidate("0.4.0", tags), "0.4.0-rc11")

    def test_only_this_releases_candidates_count(self):
        tags = ["v0.4.0-rc1", "v0.4.1-rc7", "v0.40.0-rc9", "v0.4.0", "v0.4.0-beta3", "v0.4.0-rcx", "v0.4.0-rc01", "v0.4.0-RC9"]
        self.assertEqual(release.next_candidate("0.4.0", tags), "0.4.0-rc2")
        self.assertEqual(release.next_candidate("0.4.1", tags), "0.4.1-rc8")

    def test_a_candidate_manifest_does_not_confuse_the_next_version(self):
        self.assertEqual(release.next_version("0.4.0-rc1", "v0.3.0", ["docs: a (#1)"]), ("0.4.0-rc1", True))
        self.assertEqual(release.next_version("0.3.0-rc1", "v0.3.0", ["feat: a (#1)"]), ("0.4.0", True))

    def test_every_manifest_takes_a_candidate_version(self):
        files = {
            "x/Cargo.toml": '[package]\nversion = "0.4.0"\n\n[dependencies]\nserde = { version = "1.0.0" }\n',
            "package.json": '{\n  "name": "a",\n  "version": "0.4.0",\n  "dep": { "version": "9" }\n}\n',
            "src-tauri/tauri.conf.json": '{\n  "version": "0.4.0",\n  "identifier": "a.b"\n}\n',
            "Cargo.lock": 'name = "captainsalvage"\nversion = "0.4.0"\n\nname = "captain-salvage"\nversion = "0.4.0"\n\nname = "serde"\nversion = "1.0.0"\n',
            "package-lock.json": '{"version": "0.4.0", "packages": {"": {"version": "0.4.0"}, "a": {"version": "9"}}}',
        }
        new = release.set_versions(files, "0.4.0-rc2")
        self.assertIn('[package]\nversion = "0.4.0-rc2"', new["x/Cargo.toml"])
        self.assertIn('serde = { version = "1.0.0" }', new["x/Cargo.toml"])
        self.assertIn('"version": "0.4.0-rc2"', new["package.json"])
        self.assertIn('"dep": { "version": "9" }', new["package.json"])
        self.assertIn('"version": "0.4.0-rc2"', new["src-tauri/tauri.conf.json"])
        self.assertEqual(new["Cargo.lock"].count('version = "0.4.0-rc2"'), 2)
        self.assertIn('name = "serde"\nversion = "1.0.0"', new["Cargo.lock"])
        self.assertEqual(new["package-lock.json"].count('"0.4.0-rc2"'), 2)
        self.assertIn('"a": {"version": "9"}', new["package-lock.json"])
        # And back to the plain number, as a later release will.
        again = release.set_versions(new, "0.4.0")
        self.assertEqual(again["Cargo.lock"].count('version = "0.4.0"'), 2)
        self.assertNotIn("rc", again["package-lock.json"])


class Project(unittest.TestCase):
    """The tool run against a small project in a throwaway git repository."""

    CHANGELOG = "# Changelog\n\n## [0.4.0] — 2026-02-01\n\n- New.\n\n## [0.3.0] — 2026-01-01\n\n- Old.\n"
    README = "Top\n\n" + release.README_START + "\n\n### v0.4.0 — 2026-02-01\n\n" + release.README_END + "\n"

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.original = release.ROOT
        self.root = Path(self.temp.name)
        release.ROOT = self.root
        release.CHANGELOG = self.root / "CHANGELOG.md"
        release.README = self.root / "README.md"
        (self.root / "crates/captainsalvage").mkdir(parents=True)
        (self.root / "src-tauri").mkdir()
        self.write_all("0.4.0")
        release.CHANGELOG.write_text(self.CHANGELOG)
        release.README.write_text(self.README)
        for command in (["init", "-q", "-b", "main"], ["config", "user.email", "t@example.com"], ["config", "user.name", "t"]):
            self.git(*command)

    def tearDown(self):
        release.ROOT = self.original
        release.CHANGELOG = self.original / "CHANGELOG.md"
        release.README = self.original / "README.md"
        self.temp.cleanup()

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.root, check=True, capture_output=True, text=True).stdout.strip()

    def write_all(self, version):
        (self.root / "VERSION").write_text(version + "\n")
        (self.root / "crates/captainsalvage/Cargo.toml").write_text(f'[package]\nversion = "{version}"\n')
        (self.root / "src-tauri/Cargo.toml").write_text(f'[package]\nversion = "{version}"\n')
        (self.root / "src-tauri/tauri.conf.json").write_text(f'{{\n  "version": "{version}"\n}}\n')
        (self.root / "package.json").write_text(f'{{\n  "version": "{version}"\n}}\n')
        (self.root / "Cargo.lock").write_text(
            f'name = "captainsalvage"\nversion = "{version}"\n\nname = "captain-salvage"\nversion = "{version}"\n'
        )
        (self.root / "package-lock.json").write_text(
            f'{{\n  "version": "{version}",\n  "packages": {{ "": {{ "version": "{version}" }} }}\n}}\n'
        )

    def commit(self, subject):
        self.git("add", "-A")
        self.git("commit", "-q", "--allow-empty", "-m", subject)

    def check(self, tag=None):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            release.command_check(argparse.Namespace(tag=tag))
        return out.getvalue()

    def refused(self, call, *args):
        with self.assertRaises(SystemExit) as raised:
            call(*args)
        return str(raised.exception)

    def test_the_version_file_may_hold_a_candidate(self):
        (self.root / "VERSION").write_text("0.4.0-rc1\n")
        self.assertEqual(release.source_version(), "0.4.0-rc1")

    def test_the_version_file_refuses_anything_else_that_is_not_a_candidate(self):
        for text in ("0.4.0-beta1", "0.4.0-rc", "0.4.0-rc0", "0.4.0-rc01", "0.4.0-RC1", "0.4.0-rc.1", "0.4.0-hotfix", "0.4.0+rc1", "0.4", "v0.4.0"):
            (self.root / "VERSION").write_text(text + "\n")
            with self.assertRaises(SystemExit, msg=text):
                release.source_version()

    def test_check_passes_for_a_release(self):
        self.assertIn("0.4.0", self.check("v0.4.0"))

    def test_check_passes_for_a_candidate_whose_files_carry_it(self):
        self.write_all("0.4.0-rc1")
        self.assertIn("0.4.0-rc1", self.check("v0.4.0-rc1"))

    def test_a_candidate_needs_the_base_versions_changelog_and_readme(self):
        self.write_all("0.5.0-rc1")
        message = self.refused(self.check, "v0.5.0-rc1")
        self.assertIn("CHANGELOG.md has no section for 0.5.0", message)
        self.assertIn("README.md has no section for 0.5.0", message)

    def test_a_candidate_is_not_documented_by_a_section_of_its_own(self):
        self.write_all("0.4.0-rc1")
        release.CHANGELOG.write_text(self.CHANGELOG.replace("[0.4.0]", "[0.4.0-rc1]"))
        self.assertIn("CHANGELOG.md has no section for 0.4.0", self.refused(self.check, "v0.4.0-rc1"))

    def test_a_candidate_tag_with_plain_files_is_refused(self):
        message = self.refused(self.check, "v0.4.0-rc1")
        self.assertIn("the tag is 0.4.0-rc1, but VERSION says 0.4.0", message)

    def test_a_release_tag_with_candidate_files_is_refused(self):
        self.write_all("0.4.0-rc1")
        message = self.refused(self.check, "v0.4.0")
        self.assertIn("the tag is 0.4.0, but VERSION says 0.4.0-rc1", message)

    def test_a_different_candidate_is_refused(self):
        self.write_all("0.4.0-rc1")
        self.assertIn("the tag is 0.4.0-rc2", self.refused(self.check, "v0.4.0-rc2"))

    def test_a_file_left_on_the_plain_version_is_refused_when_the_rest_are_a_candidate(self):
        self.write_all("0.4.0-rc1")
        (self.root / "package.json").write_text('{\n  "version": "0.4.0"\n}\n')
        self.assertIn("package.json says 0.4.0", self.refused(self.check, "v0.4.0-rc1"))

    def test_a_tag_that_is_not_a_version_is_refused(self):
        for tag, why in (("v0.4.0-beta1", "unrecognised suffix"), ("v0.4.0-RC1", "must be lowercase"),
                         ("v0.4.0-rc", "needs a number"), ("v0.4.0-rc0", "no leading zeros"),
                         ("v0.4.0-hotfix", "unrecognised suffix"), ("v0.4.0-rc.1", "number straight after")):
            message = self.refused(self.check, tag)
            self.assertIn("is not a release tag", message, tag)
            self.assertIn(why, message, tag)

    def test_the_latest_tag_ignores_candidates(self):
        self.commit("first")
        self.git("tag", "-a", "v0.3.0", "-m", "x")
        self.commit("second")
        self.git("tag", "-a", "v0.4.0-rc1", "-m", "x")
        self.assertEqual(release.latest_tag(), "v0.3.0")

    def test_a_candidate_on_the_commit_does_not_make_that_commit_a_release(self):
        self.commit("first")
        self.git("tag", "-a", "v0.3.0", "-m", "x")
        self.commit("second")
        self.git("tag", "-a", "v0.4.0-rc1", "-m", "x")
        self.assertEqual(release.previous_tag("HEAD"), "v0.3.0")
        self.assertEqual(release.version_at("HEAD")[0:2], ("0.4.0", True))

    def test_the_next_candidate_counts_the_tags_there_are(self):
        self.commit("first")
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            release.command_next_rc(None)
        self.assertEqual(out.getvalue().strip(), "0.4.0-rc1")
        self.git("tag", "-a", "v0.4.0-rc1", "-m", "x")
        self.git("tag", "-a", "v0.4.0-rc2", "-m", "x")
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            release.command_next_rc(None)
        self.assertEqual(out.getvalue().strip(), "0.4.0-rc3")

    def test_a_candidate_is_not_cut_from_a_candidate(self):
        self.write_all("0.4.0-rc1")
        self.assertIn("already a candidate", self.refused(release.command_next_rc, None))

    def test_stamping_writes_the_candidate_everywhere_and_touches_no_notes(self):
        self.commit("first")
        with contextlib.redirect_stdout(io.StringIO()):
            release.command_stamp_rc(argparse.Namespace(version="0.4.0-rc2"))
        self.assertEqual(release.source_version(), "0.4.0-rc2")
        self.assertTrue(all(found == "0.4.0-rc2" for _, found in release.manifest_versions()))
        self.assertEqual(sorted(self.git("diff", "--name-only").splitlines()), sorted([
            "Cargo.lock", "VERSION", "crates/captainsalvage/Cargo.toml", "package-lock.json",
            "package.json", "src-tauri/Cargo.toml", "src-tauri/tauri.conf.json",
        ]))
        self.assertIn("0.4.0-rc2", self.check("v0.4.0-rc2"))

    def test_stamping_is_not_a_way_to_set_another_number(self):
        for wanted in ("0.5.0", "0.4.0", "0.4.0-beta1", "0.4.0-rc0", "0.4.0-RC1"):
            message = self.refused(release.command_stamp_rc, argparse.Namespace(version=wanted))
            self.assertIn("not a release candidate", message, wanted)
        message = self.refused(release.command_stamp_rc, argparse.Namespace(version="0.5.0-rc1"))
        self.assertIn("is a candidate of 0.5.0, but VERSION says 0.4.0", message)
        self.assertEqual(release.source_version(), "0.4.0")


class Workflow(unittest.TestCase):
    """release.yml starts for a release and for nothing else, as scripts/tag-kind says."""

    WORKFLOW = release.ROOT / ".github/workflows/release.yml"

    @classmethod
    def patterns(cls):
        text = cls.WORKFLOW.read_text()
        block = text.split("    tags:\n", 1)[1].split("  workflow_dispatch", 1)[0]
        return [line.strip()[2:].strip('"') for line in block.splitlines() if line.strip().startswith("- ")]

    def starts(self, tag):
        """GitHub's rule: some pattern matches, and no later "!" pattern that matches takes it back."""
        runs = False
        for pattern in self.patterns():
            if pattern.startswith("!"):
                runs = runs and not fnmatch.fnmatchcase(tag, pattern[1:])
            elif fnmatch.fnmatchcase(tag, pattern):
                runs = True
        return runs

    def test_a_release_starts_it(self):
        for tag in ("v0.4.0", "v1.0.0", "v10.20.30"):
            self.assertEqual(release.tag_kind(tag), "final")
            self.assertTrue(self.starts(tag), tag)

    def test_a_candidate_does_not(self):
        for tag in ("v0.4.0-rc1", "v0.4.0-rc2", "v0.4.0-rc10", "v1.0.0-rc123"):
            self.assertEqual(release.tag_kind(tag), "candidate")
            self.assertFalse(self.starts(tag), tag)

    def test_a_name_only_like_a_candidate_starts_it_to_be_refused_by_name(self):
        for tag in ("v0.4.0-RC1", "v0.4.0-rc", "v0.4.0-rc0", "v0.4.0-hotfix", "v0.4.0-beta1", "v0.4.0-rc.1"):
            with self.assertRaises(ValueError, msg=tag):
                release.tag_kind(tag)
            self.assertTrue(self.starts(tag), tag)

    def test_the_first_step_asks_tag_kind(self):
        text = self.WORKFLOW.read_text()
        step = text.split("- name: check the tag", 1)[1].split("- uses:", 1)[0]
        self.assertIn('scripts/tag-kind "$GITHUB_REF_NAME"', step)
        self.assertIn("is not on main", step)

    def test_a_tag_with_a_dash_is_never_the_last_release(self):
        # `git describe --exclude` is how the last release is found, and it relies on this.
        for tag in ("v0.4.0", "v0.4.0-rc1", "v10.0.0", "v10.0.0-rc9"):
            self.assertEqual(release.tag_kind(tag) == "final", not fnmatch.fnmatchcase(tag, release.NOT_A_RELEASE), tag)


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
