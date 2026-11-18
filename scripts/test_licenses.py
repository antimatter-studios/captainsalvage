#!/usr/bin/env python3
"""The license list must refuse to be made, or checked, when the npm packages are not installed."""

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "licenses.py"


def load(root):
    spec = importlib.util.spec_from_file_location("licenses_under_test", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.ROOT = root
    return module


def write_lock(root):
    lock = {"packages": {"": {}, "node_modules/left-pad": {"version": "1.0.0", "license": "MIT"}}}
    (root / "package-lock.json").write_text(json.dumps(lock))


class NpmPackages(unittest.TestCase):
    def test_missing_packages_stop_it_and_say_how_to_fix_it(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            write_lock(root)
            with self.assertRaises(SystemExit) as raised:
                load(root).npm_packages()
            message = str(raised.exception)
            self.assertIn("left-pad", message)
            self.assertIn("npm ci", message)

    def test_installed_packages_are_listed(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            write_lock(root)
            package = root / "node_modules" / "left-pad"
            package.mkdir(parents=True)
            (package / "package.json").write_text(json.dumps({"name": "left-pad", "license": "MIT"}))
            found = load(root).npm_packages()
            self.assertEqual([entry["name"] for entry in found], ["left-pad"])


if __name__ == "__main__":
    sys.exit(unittest.main())
