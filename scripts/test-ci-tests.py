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
from unittest.mock import ANY
from unittest.mock import patch


def load(name: str, file: str):
    """Load an executable helper without requiring a Python package in scripts/."""
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(file))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


ci_tests = load("ci_tests", "run-ci-tests.py")
divergence = load("divergence", "check-transport-independent-modules.py")


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
            artifacts=self.artifacts, inventory=self.inventory, target=None, target_libdir=self.root / "rustlib",
            filter="", exact=False, ignored=False, sudo=False, nocapture=False, skip=[],
            skip_file=None, require_file=None, list_only=False, skip_modules=None, only_modules=None,
        )
        self.tests = ["unit::fast", "model::large", "gateway::privileged"]
        self.ignored = ["gateway::privileged"]
        self.listing = patch.object(ci_tests, "test_names", side_effect=lambda artifact, environment, ignored=False: self.ignored if ignored else self.tests)
        self.listing.start()
        self.addCleanup(self.listing.stop)
        self.execution = patch.object(ci_tests.subprocess, "run")
        self.run_mock = self.execution.start()
        self.addCleanup(self.execution.stop)

    def test_artifact_is_reused_once_with_package_cwd(self):
        """Duplicate Cargo events must not duplicate execution or change package cwd."""
        ci_tests.run(self.args)
        self.run_mock.assert_called_once_with([str(self.root / "tests")], cwd=str(self.root), env=ANY, check=True)
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

    def test_listed_module_runs_in_exactly_one_run(self):
        """A module list moves a module's tests to its owner: the skipping run leaves them out and
        the owning run executes them alone, so neither runs them twice or drops them."""
        modules = self.root / "modules.txt"
        modules.write_text("# owned by another job\nsample unit\n")
        self.args.skip_modules = modules
        ci_tests.run(self.args)
        self.assertEqual(json.loads(self.inventory.read_text())[0]["selected"], ["model::large"])
        self.run_mock.assert_called_once_with(
            [str(self.root / "tests"), "--skip", "unit::"], cwd=str(self.root), env=ANY, check=True,
        )
        self.run_mock.reset_mock()
        self.args.skip_modules, self.args.only_modules = None, modules
        ci_tests.run(self.args)
        self.assertEqual(json.loads(self.inventory.read_text())[0]["selected"], ["unit::fast"])
        self.run_mock.assert_called_once_with(
            [str(self.root / "tests"), "unit::"], cwd=str(self.root), env=ANY, check=True,
        )

    def test_listed_module_matching_nothing_fails(self):
        """A renamed module must not silently stop moving its tests."""
        self.args.skip_modules = self.root / "modules.txt"
        self.args.skip_modules.write_text("sample renamed\n")
        with self.assertRaisesRegex(ValueError, "matches zero tests"):
            ci_tests.run(self.args)
        self.run_mock.assert_not_called()

    def test_listed_module_matching_inside_another_name_fails(self):
        """libtest matches a module's prefix anywhere in a name; it must not reach another module."""
        self.tests.append("model::unit::nested")
        self.args.skip_modules = self.root / "modules.txt"
        self.args.skip_modules.write_text("sample unit\n")
        with self.assertRaisesRegex(ValueError, "inside another test name"):
            ci_tests.run(self.args)
        self.run_mock.assert_not_called()

    def test_listed_module_cannot_hide_a_required_test(self):
        """A required model search cannot be moved away from the job that must run it."""
        self.args.skip_modules = self.root / "modules.txt"
        self.args.skip_modules.write_text("sample model\n")
        self.args.require_file = self.root / "required.txt"
        self.args.require_file.write_text("model::large\n")
        with self.assertRaisesRegex(ValueError, "required tests"):
            ci_tests.run(self.args)
        self.run_mock.assert_not_called()

    def test_per_crate_runs_ignore_other_targets(self):
        """A per-crate run acts on its own target's modules and ignores the others', in both modes."""
        modules = self.root / "modules.txt"
        modules.write_text("other unit\nsample unit\n")
        self.args.only_modules = modules
        ci_tests.run(self.args)
        self.assertEqual(json.loads(self.inventory.read_text())[0]["selected"], ["unit::fast"])
        self.args.only_modules, self.args.skip_modules = None, modules
        ci_tests.run(self.args)
        self.assertEqual(json.loads(self.inventory.read_text())[0]["selected"], ["model::large"])

    def test_module_lists_combine_with_nothing_that_changes_their_meaning(self):
        """Lists are exclusive, and `--exact` or a filter would change what a prefix selects."""
        modules = self.root / "modules.txt"
        modules.write_text("sample unit\n")
        self.args.skip_modules = self.args.only_modules = modules
        with self.assertRaisesRegex(ValueError, "exclusive"):
            ci_tests.run(self.args)
        self.args.skip_modules, self.args.exact = None, True
        with self.assertRaisesRegex(ValueError, "--exact"):
            ci_tests.run(self.args)
        self.args.exact, self.args.filter = False, "unit"
        with self.assertRaisesRegex(ValueError, "no --filter"):
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
            ["sudo", "--", "env", ANY, str(self.root / "tests"), "gateway::privileged", "--exact", "--ignored"],
            cwd=str(self.root), env=ANY, check=True,
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
            subprocess.run(["rustc", "--test", "-C", "prefer-dynamic", str(source), "-o", str(binary)], check=True)
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


class DivergenceInventory(unittest.TestCase):
    """Derive divergence points from synthetic crates, as both builds of a listed target see them."""

    DUMMY = 'feature = "dummy"'
    BOTH = 'feature = "native-webrtc"'

    def points(self, files: dict[str, str], crate_under_test: bool = True):
        """The inventory of a crate with `files` under src/, in a native and a dummy build."""
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        for name, text in files.items():
            (root / "src" / name).parent.mkdir(parents=True, exist_ok=True)
            (root / "src" / name).write_text(text)
        modules = divergence.library_modules(root / "src" / "lib.rs")
        builds = divergence.Builds(
            {"test": crate_under_test, self.DUMMY: False, self.BOTH: True},
            {"test": crate_under_test, self.DUMMY: True, self.BOTH: True},
        )
        return divergence.inventory(root, [(modules, builds, root / "src")])

    def shapes(self, found) -> dict[str, tuple[str, bool]]:
        """Each point's code, with its shape and whether it needs a ledger entry."""
        return {point.head: (point.shape, point.needs_entry) for point in found.points}

    def test_differences_in_shared_code_need_entries(self):
        """A statement, a field and an impl on a shared type differ in code both builds contain."""
        found = self.points({"lib.rs": '''
pub struct Shared {
    #[cfg(feature = "dummy")]
    extra: u8,
}
#[cfg(feature = "dummy")]
impl Shared {
    fn probe(&self) {}
}
fn shared() {
    #[cfg(feature = "dummy")]
    dummy_only();
    let _flag = cfg!(feature = "dummy");
}
'''})
        self.assertEqual(self.shapes(found), {
            "extra: u8,": ("field or variant", True),
            "impl Shared {": ("impl on a shared type", True),
            "dummy_only();": ("statement", True),
            'cfg!(feature = "dummy");': ("statement", True),
        })

    def test_code_one_build_lacks_entirely_needs_no_entry(self):
        """An item only one build defines, and code under a predicate both builds share, are inert."""
        found = self.points({"lib.rs": '''
#[cfg(feature = "dummy")]
fn dummy_only() {}
#[cfg(feature = "dummy")]
struct Probe;
#[cfg(feature = "dummy")]
impl Probe {}
#[cfg(any(feature = "dummy", feature = "native-webrtc"))]
fn both() {}
'''})
        self.assertEqual({needs for _, needs in self.shapes(found).values()}, {False})
        self.assertNotIn("fn both() {}", self.shapes(found))

    def test_test_functions_and_dependency_tests_need_no_entry(self):
        """A test function reaches only itself; a dependency's `cfg(test)` code is in neither build."""
        source = {"lib.rs": '''
#[cfg(test)]
mod tests {
    #[test]
    fn case() {
        #[cfg(feature = "dummy")]
        let _probe = 1;
    }
}
#[cfg(all(test, feature = "dummy"))]
fn hook() {}
'''}
        self.assertEqual(self.shapes(self.points(source)), {
            "let _probe = 1;": ("statement", False), "fn hook() {}": ("item in one build only", False),
        })
        self.assertEqual(self.points(source, crate_under_test=False).points, [])

    def test_values_reach_readers_through_constants(self):
        """A substituted constant, and a constant derived from it, make their readers points."""
        found = self.points({"lib.rs": '''
#[cfg(feature = "dummy")]
const BOUND: u64 = 1;
#[cfg(not(feature = "dummy"))]
const BOUND: u64 = 2;
const DERIVED: u64 = BOUND * 2;
const fn scaled() -> u64 { DERIVED * 3 }
mod inner {
    pub(super) fn reads() -> u64 { super::scaled() }
    fn unrelated() -> u64 { 3 }
}
'''})
        readers = [point for point in found.points if point.shape == "reader"]
        self.assertEqual([(point.scope, point.head) for point in readers], [("mod inner / fn reads", "reads scaled")])
        self.assertEqual(readers[0].namer[0::2], ("reads", []))
        self.assertEqual({p.shape for p in found.points if not p.needs_entry}, {"substituted value"})

    def test_module_declared_in_one_build_is_found(self):
        """A file whose `mod` declaration one build lacks cannot hold a listed module."""
        found = self.points({"lib.rs": '#[cfg(feature = "dummy")]\nmod gated;\n', "gated.rs": "fn f() {}\n"})
        self.assertEqual({path.name for path in found.one_build_files}, {"gated.rs"})

    def test_predicates_differ_only_where_the_dummy_feature_decides(self):
        """Atoms other than `test` and features take every value, the same in both builds."""
        builds = divergence.Builds({"test": True, self.DUMMY: False}, {"test": True, self.DUMMY: True})
        gated = divergence.parse_predicate('all(test, feature = "dummy", not(target_family = "wasm"))')
        wasm = divergence.parse_predicate('target_family = "wasm"')
        self.assertTrue(builds.differ([], [gated]))
        self.assertFalse(builds.differ([wasm], [gated]))
        self.assertFalse(builds.differ([], [wasm]))

    def test_build_script_cfgs_follow_the_features_they_derive_from(self):
        """`rings_transport_backend` agrees across the builds only while both enable a backend."""
        derived = divergence.parse_predicate(divergence.BUILD_SCRIPT_CFGS["rings-transport"]["rings_transport_backend"])
        backend = divergence.parse_predicate("rings_transport_backend")

        def builds(native_webrtc: bool):
            atoms = {"test": False, self.BOTH: native_webrtc, 'feature = "web-sys-webrtc"': False}
            return divergence.Builds(
                {**atoms, self.DUMMY: False, "rings_transport_backend": derived},
                {**atoms, self.DUMMY: True, "rings_transport_backend": derived},
            )

        self.assertFalse(builds(native_webrtc=True).differ([], [backend]))
        self.assertTrue(builds(native_webrtc=False).differ([], [backend]))

    def test_unreached_readers_are_named_by_their_callers_handle(self):
        """A method is named by its impl's self type; a free function by its name."""
        self.assertEqual(divergence.reached_name("impl<'a> DelegationTable<'a, Lease> / fn insert"), "DelegationTable")
        self.assertEqual(divergence.reached_name("impl Trait for Carrier<T> / fn send"), "Carrier")
        self.assertEqual(divergence.reached_name("mod tests / fn walk"), "walk")


if __name__ == "__main__":
    unittest.main()
