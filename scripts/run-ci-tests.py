#!/usr/bin/env python3
"""Inventory and run prebuilt Cargo libtest executables without invoking Cargo.

Every selection is checked against libtest's inventory before any test runs.
The JSON artifact stream is produced by `cargo test --no-run --message-format=json`.
"""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys


def arguments() -> argparse.Namespace:
    """Parse build evidence, inventory destination, and checked test selection."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("artifacts", type=Path, help="Cargo JSON artifact stream")
    parser.add_argument("--inventory", type=Path, required=True, help="Write selected and ignored test names")
    parser.add_argument("--target-libdir", type=Path, help="Rust target library directory (default: active rustc)")
    parser.add_argument("--target", help="Select one Cargo target name")
    parser.add_argument("--filter", default="", help="Required libtest substring filter")
    parser.add_argument("--exact", action="store_true", help="Match the filter exactly")
    parser.add_argument("--nocapture", action="store_true", help="Keep replay diagnostics visible on successful tests")
    parser.add_argument("--ignored", action="store_true", help="Run only ignored tests")
    parser.add_argument("--sudo", action="store_true", help="Elevate only the selected test executable")
    parser.add_argument("--skip", action="append", default=[], help="Required substring exclusion")
    parser.add_argument("--skip-file", type=Path, help="Exact test names owned by another job")
    parser.add_argument("--require-file", type=Path, help="Exact test names this job must execute")
    parser.add_argument("--skip-modules", type=Path, help="`<target> <module>` lines whose tests another job owns")
    parser.add_argument("--only-modules", type=Path, help="`<target> <module>` lines whose tests this run owns")
    parser.add_argument("--list-only", action="store_true", help="Validate and save inventory without execution")
    return parser.parse_args()


def names_file(path: Path | None) -> list[str]:
    """Read documented ownership lists, ignoring comments and blank lines."""
    if path is None:
        return []
    return [line.strip() for line in path.read_text().splitlines() if line.strip() and not line.startswith("#")]


def module_entries(path: Path | None) -> list[tuple[str, str]]:
    """Read `<target> <module>` ownership lines: a module owns every test whose name starts with
    `module::` in the test executable of that Cargo target."""
    entries = []
    for line in names_file(path):
        target, _, module = line.partition(" ")
        module = module.strip()
        if not target or not module or " " in module or module.endswith(":"):
            raise ValueError(f"malformed module line: {line!r}")
        entries.append((target, module))
    return entries


def executables(path: Path, target: str | None) -> list[dict]:
    """Collect unique libtest artifacts and their Cargo package working directories."""
    found = {}
    linked_paths = []
    for line in path.read_text().splitlines():
        event = json.loads(line)
        if event.get("reason") == "build-script-executed":
            linked_paths.extend(event.get("linked_paths", []))
        executable = event.get("executable")
        if event.get("reason") != "compiler-artifact" or not event.get("profile", {}).get("test") or not executable:
            continue
        if target and event["target"]["name"] != target:
            continue
        # A workspace build can report an artifact repeatedly; never execute it twice.
        found[executable] = {
            "executable": executable,
            "target": event["target"]["name"],
            "package_id": event["package_id"],
            "cwd": str(Path(event["manifest_path"]).parent),
            "profile": event["profile"],
            "features": event.get("features", []),
        }
    if not found or (target and len(found) != 1):
        raise ValueError(f"expected {'one' if target else 'at least one'} test executable, found {len(found)}")
    for artifact in found.values():
        # Cargo only forwards build-script library paths within its target directory.
        target_root = Path(artifact["executable"]).parent.parent.parent
        artifact["linked_paths"] = [
            str(path) for entry in linked_paths
            if not entry.startswith("framework=")
            and (path := Path(entry.removeprefix("native=").removeprefix("dependency="))).is_relative_to(target_root)
        ]
    return list(found.values())


def runtime_environment(artifact: dict, target_libdir: Path) -> tuple[dict, str]:
    """Restore Cargo's native dynamic-library search paths, including proc-macro libstd."""
    environment = os.environ.copy()
    variable = "DYLD_FALLBACK_LIBRARY_PATH" if sys.platform == "darwin" else "LD_LIBRARY_PATH"
    deps = Path(artifact["executable"]).parent
    paths = artifact["linked_paths"] + [str(deps), str(deps.parent), str(target_libdir)]
    if inherited := environment.get(variable):
        paths.append(inherited)
    elif sys.platform == "darwin":
        # Setting DYLD_FALLBACK_LIBRARY_PATH replaces dyld's implicit fallbacks.
        paths.extend([str(Path.home() / "lib"), "/usr/local/lib", "/usr/lib"])
    environment[variable] = os.pathsep.join(paths)
    return environment, variable


def test_names(artifact: dict, environment: dict, ignored: bool = False) -> list[str]:
    """Ask libtest for runnable test names, rejecting non-libtest harnesses."""
    command = [artifact["executable"], "--list", "--format", "terse"]
    if ignored:
        command.append("--ignored")
    output = subprocess.check_output(command, cwd=artifact["cwd"], env=environment, text=True)
    names = []
    for line in output.splitlines():
        if line.endswith(": test"):
            names.append(line.removesuffix(": test"))
        elif line.strip():
            raise ValueError(f"unexpected libtest inventory line: {line!r}")
    return names


def owned_by_this_run(name: str, modules: list[str], args: argparse.Namespace) -> bool:
    """Whether a run with these module lists executes `name`: an owning run executes only its
    listed modules, a skipping run everything but them, and a run with no list everything."""
    in_listed_module = any(name.startswith(prefix) for prefix in modules)
    if args.only_modules:
        return in_listed_module
    return not (args.skip_modules and in_listed_module)


def run(args: argparse.Namespace) -> None:
    """Validate all ownership claims, save the inventory, then execute the selection."""
    if args.sudo and not (args.target and args.filter and args.exact and args.ignored):
        raise ValueError("sudo requires one target and one exact ignored test")
    if args.skip_modules and args.only_modules:
        raise ValueError("--skip-modules and --only-modules are exclusive")
    # libtest's filters and skips are substrings, and `--exact` would make them whole names.
    if (args.skip_modules or args.only_modules) and args.exact:
        raise ValueError("a module list cannot be combined with --exact")
    if args.only_modules and args.filter:
        raise ValueError("--only-modules selects by module and takes no --filter")
    artifacts = executables(args.artifacts, args.target)
    modules = module_entries(args.skip_modules or args.only_modules)
    target_libdir = args.target_libdir or Path(subprocess.check_output(
        ["rustc", "--print", "target-libdir"], text=True,
    ).strip())
    exact_skips = names_file(args.skip_file)
    required = names_file(args.require_file)
    all_names = set()
    selected_names = set()
    for artifact in artifacts:
        environment, _ = runtime_environment(artifact, target_libdir)
        artifact["target_libdir"] = str(target_libdir)
        artifact["tests"] = test_names(artifact, environment)
        artifact["ignored"] = test_names(artifact, environment, ignored=True)
        all_names.update(artifact["tests"])
        eligible = set(artifact["ignored"]) if args.ignored else set(artifact["tests"]) - set(artifact["ignored"])
        artifact["modules"] = [module + "::" for target, module in modules if target == artifact["target"]]
        artifact["selected"] = sorted(
            name for name in eligible
            if (name == args.filter if args.exact else args.filter in name)
            and name not in exact_skips
            and not any(skip in name for skip in args.skip)
            and owned_by_this_run(name, artifact["modules"], args)
        )
        selected_names.update(artifact["selected"])
    for name in exact_skips:
        if name not in all_names:
            raise ValueError(f"excluded test no longer exists: {name}")
        # libtest's --skip is a substring match; do not accidentally exclude a sibling.
        if any(name in other and name != other for other in all_names):
            raise ValueError(f"exact exclusion also matches another test: {name}")
    for skip in args.skip:
        if not any(skip in name for name in all_names):
            raise ValueError(f"exclusion matches zero tests: {skip}")
    targets = {artifact["target"] for artifact in artifacts}
    for target, module in modules:
        # A per-crate run sees only its own target. Every run that builds a target validates its
        # modules, and scripts/check-transport-independent-modules.py rejects unknown targets.
        if target not in targets:
            continue
        prefix = module + "::"
        names = [name for artifact in artifacts if artifact["target"] == target for name in artifact["tests"]]
        if not any(name.startswith(prefix) for name in names):
            raise ValueError(f"listed module matches zero tests: {target} {module}")
        # libtest matches the prefix anywhere in a name; it must not reach a test outside the module.
        if any(prefix in name and not name.startswith(prefix) for name in names):
            raise ValueError(f"listed module also matches inside another test name: {target} {module}")
    if not selected_names:
        raise ValueError("selection matches zero runnable tests")
    if missing := set(required) - selected_names:
        raise ValueError(f"required tests are absent, ignored, or excluded: {sorted(missing)}")
    args.inventory.parent.mkdir(parents=True, exist_ok=True)
    args.inventory.write_text(json.dumps(artifacts, indent=2) + "\n")
    print(f"Validated {sum(len(item['selected']) for item in artifacts)} selected tests; inventory: {args.inventory}", flush=True)
    if args.list_only:
        return
    for artifact in artifacts:
        if not artifact["selected"]:
            continue
        environment, variable = runtime_environment(artifact, target_libdir)
        # sudo strips loader variables; restore only this explicit path after elevation.
        command = (["sudo", "--", "env", f"{variable}={environment[variable]}"] if args.sudo else []) + [artifact["executable"]]
        if args.filter:
            command.append(args.filter)
        if args.only_modules:
            command.extend(artifact["modules"])
        if args.exact:
            command.append("--exact")
        if args.ignored:
            command.append("--ignored")
        if args.nocapture:
            command.append("--nocapture")
        for skip in args.skip + exact_skips + (artifact["modules"] if args.skip_modules else []):
            command.extend(["--skip", skip])
        # Cargo normally starts each test from its own package directory.
        print(f"Running {artifact['target']} ({len(artifact['selected'])} tests)", flush=True)
        subprocess.run(command, cwd=artifact["cwd"], env=environment, check=True)


if __name__ == "__main__":
    try:
        run(arguments())
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"CI test execution failed: {error}", file=sys.stderr)
        sys.exit(1)
