#!/usr/bin/env python3
"""Regression checks for CI coverage loss, artifact reuse, and privilege boundaries."""

import argparse
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

# Load the executable helper without requiring a Python package in scripts/.
spec = importlib.util.spec_from_file_location("ci_tests", Path(__file__).with_name("run-ci-tests.py"))
ci_tests = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ci_tests)


class CoverageGuards(unittest.TestCase):
    """Exercise selection against a fake libtest inventory and Cargo artifact stream."""

    def setUp(self):
        """Build an isolated artifact manifest with an intentional duplicate event."""
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.artifacts = self.root / "artifacts.jsonl"
        self.inventory = self.root / "inventory.json"
        event = {
            "reason": "compiler-artifact", "executable": str(self.root / "tests"),
            "profile": {"test": True}, "target": {"name": "sample"},
            "package_id": "sample", "manifest_path": str(self.root / "Cargo.toml"),
        }
        self.artifacts.write_text(json.dumps(event) + "\n" + json.dumps(event) + "\n")
        self.args = argparse.Namespace(
            artifacts=self.artifacts, inventory=self.inventory, target=None,
            filter="", exact=False, ignored=False, sudo=False, nocapture=False, skip=[],
            skip_file=None, require_file=None, list_only=False,
        )
        self.tests = ["unit::fast", "model::large", "gateway::privileged"]
        self.ignored = ["gateway::privileged"]
        self.listing = patch.object(ci_tests, "test_names", side_effect=lambda artifact, ignored=False: self.ignored if ignored else self.tests)
        self.listing.start()
        self.addCleanup(self.listing.stop)
        self.execution = patch.object(ci_tests.subprocess, "run")
        self.run_mock = self.execution.start()
        self.addCleanup(self.execution.stop)

    def test_artifact_is_reused_once_with_package_cwd(self):
        """Duplicate Cargo events must not duplicate execution or change package cwd."""
        ci_tests.run(self.args)
        self.run_mock.assert_called_once_with([str(self.root / "tests")], cwd=str(self.root), check=True)
        saved = json.loads(self.inventory.read_text())
        self.assertEqual(saved[0]["selected"], ["model::large", "unit::fast"])
        self.assertEqual(saved[0]["ignored"], self.ignored)

    def test_required_model_cannot_be_ignored(self):
        """A profile change that ignores a required model must fail before execution."""
        self.args.require_file = self.root / "required.txt"
        self.args.require_file.write_text("model::large\n")
        self.ignored.append("model::large")
        with self.assertRaisesRegex(ValueError, "required tests"):
            ci_tests.run(self.args)
        self.run_mock.assert_not_called()

    def test_renamed_exclusion_fails(self):
        """An ownership list must not silently stop matching after a rename."""
        self.args.skip_file = self.root / "skips.txt"
        self.args.skip_file.write_text("model::renamed\n")
        with self.assertRaisesRegex(ValueError, "no longer exists"):
            ci_tests.run(self.args)
        self.run_mock.assert_not_called()

    def test_exact_exclusion_cannot_hide_a_sibling(self):
        """Reject libtest substring expansion of a supposedly exact model exclusion."""
        self.tests.append("model::large_regression")
        self.args.skip_file = self.root / "skips.txt"
        self.args.skip_file.write_text("model::large\n")
        with self.assertRaisesRegex(ValueError, "another test"):
            ci_tests.run(self.args)
        self.run_mock.assert_not_called()

    def test_empty_filter_fails(self):
        """A stale scale-matrix or privileged filter cannot produce a green zero-test run."""
        self.args.filter = "missing"
        with self.assertRaisesRegex(ValueError, "zero runnable"):
            ci_tests.run(self.args)
        self.run_mock.assert_not_called()

    def test_empty_skip_fails(self):
        """Removing the storm module must invalidate its exclusion too."""
        self.args.skip = ["missing::"]
        with self.assertRaisesRegex(ValueError, "zero tests"):
            ci_tests.run(self.args)

    def test_privilege_is_restricted_to_exact_ignored_test(self):
        """Only the selected prebuilt harness is elevated, with an exact test filter."""
        self.args.target = "sample"
        self.args.filter = "gateway::privileged"
        self.args.exact = self.args.ignored = self.args.sudo = True
        ci_tests.run(self.args)
        self.run_mock.assert_called_once_with(
            ["sudo", "--", str(self.root / "tests"), "gateway::privileged", "--exact", "--ignored"],
            cwd=str(self.root), check=True,
        )

    def test_broad_privileged_execution_fails(self):
        """Never elevate an entire test suite by a missing selector."""
        self.args.sudo = True
        with self.assertRaisesRegex(ValueError, "sudo requires"):
            ci_tests.run(self.args)
        self.run_mock.assert_not_called()

    def test_test_failure_propagates(self):
        """A failed executable cannot be hidden by inventory generation."""
        self.run_mock.side_effect = subprocess.CalledProcessError(101, ["tests"])
        with self.assertRaises(subprocess.CalledProcessError):
            ci_tests.run(self.args)

    def test_ambiguous_target_fails(self):
        """A target name shared by two artifacts must not choose an arbitrary binary."""
        event = json.loads(self.artifacts.read_text().splitlines()[0])
        event["executable"] += "-second"
        with self.artifacts.open("a") as output:
            output.write(json.dumps(event) + "\n")
        self.args.target = "sample"
        with self.assertRaisesRegex(ValueError, "found 2"):
            ci_tests.run(self.args)

    def test_missing_artifacts_fail(self):
        """A successful build with no test artifacts is not test coverage."""
        self.artifacts.write_text('{"reason": "build-finished", "success": true}\n')
        with self.assertRaisesRegex(ValueError, "found 0"):
            ci_tests.run(self.args)


class RealLibtest(unittest.TestCase):
    """Verify inventory parsing and failure propagation against an actual Rust harness."""

    def test_real_harness_selection_and_working_directory(self):
        """Run passing/ignored/failing selections using the same executable on disk."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "fixture.rs"
            source.write_text('''
#[test]
fn working_directory() { assert!(std::path::Path::new("fixture.rs").exists()); }
#[test]
#[ignore]
fn explicitly_selected() {}
#[test]
#[ignore]
fn deliberate_failure() { panic!("propagate test failure"); }
''')
            binary = root / "fixture"
            subprocess.run(["rustc", "--test", str(source), "-o", str(binary)], check=True)
            artifacts = root / "artifacts.jsonl"
            artifacts.write_text(json.dumps({
                "reason": "compiler-artifact", "executable": str(binary),
                "profile": {"test": True}, "target": {"name": "fixture"},
                "package_id": "fixture", "manifest_path": str(root / "Cargo.toml"),
            }) + "\n")
            inventory = root / "inventory.json"
            command = [sys.executable, str(Path(__file__).with_name("run-ci-tests.py")), str(artifacts), "--inventory", str(inventory)]
            subprocess.run(command, check=True, capture_output=True)
            self.assertEqual(json.loads(inventory.read_text())[0]["selected"], ["working_directory"])
            subprocess.run(command + ["--ignored", "--exact", "--filter", "explicitly_selected"], check=True, capture_output=True)
            failed = subprocess.run(command + ["--ignored", "--exact", "--filter", "deliberate_failure"], capture_output=True)
            self.assertNotEqual(failed.returncode, 0)


if __name__ == "__main__":
    unittest.main()
