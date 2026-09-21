#!/usr/bin/env python3
"""Inventory and run prebuilt Cargo libtest executables without invoking Cargo.

Every selection is checked against libtest's inventory before any test runs.
The JSON artifact stream is produced by `cargo test --no-run --message-format=json`.
"""

import argparse
import json
from pathlib import Path
import subprocess
import sys


def arguments() -> argparse.Namespace:
    """Parse build evidence, inventory destination, and checked test selection."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("artifacts", type=Path, help="Cargo JSON artifact stream")
    parser.add_argument("--inventory", type=Path, required=True, help="Write selected and ignored test names")
    parser.add_argument("--target", help="Select one Cargo target name")
    parser.add_argument("--filter", default="", help="Required libtest substring filter")
    parser.add_argument("--exact", action="store_true", help="Match the filter exactly")
    parser.add_argument("--nocapture", action="store_true", help="Keep replay diagnostics visible on successful tests")
    parser.add_argument("--ignored", action="store_true", help="Run only ignored tests")
    parser.add_argument("--sudo", action="store_true", help="Elevate only the selected test executable")
    parser.add_argument("--skip", action="append", default=[], help="Required substring exclusion")
    parser.add_argument("--skip-file", type=Path, help="Exact test names owned by another job")
    parser.add_argument("--require-file", type=Path, help="Exact test names this job must execute")
    parser.add_argument("--list-only", action="store_true", help="Validate and save inventory without execution")
    return parser.parse_args()


def names_file(path: Path | None) -> list[str]:
    """Read documented ownership lists, ignoring comments and blank lines."""
    if path is None:
        return []
    return [line.strip() for line in path.read_text().splitlines() if line.strip() and not line.startswith("#")]


def executables(path: Path, target: str | None) -> list[dict]:
    """Collect unique libtest artifacts and their Cargo package working directories."""
    found = {}
    for line in path.read_text().splitlines():
        event = json.loads(line)
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
    return list(found.values())


def test_names(artifact: dict, ignored: bool = False) -> list[str]:
    """Ask libtest for runnable test names, rejecting non-libtest harnesses."""
    command = [artifact["executable"], "--list", "--format", "terse"]
    if ignored:
        command.append("--ignored")
    output = subprocess.check_output(command, cwd=artifact["cwd"], text=True)
    names = []
    for line in output.splitlines():
        if line.endswith(": test"):
            names.append(line.removesuffix(": test"))
        elif line.strip():
            raise ValueError(f"unexpected libtest inventory line: {line!r}")
    return names


def run(args: argparse.Namespace) -> None:
    """Validate all ownership claims, save the inventory, then execute the selection."""
    if args.sudo and not (args.target and args.filter and args.exact and args.ignored):
        raise ValueError("sudo requires one target and one exact ignored test")
    artifacts = executables(args.artifacts, args.target)
    exact_skips = names_file(args.skip_file)
    required = names_file(args.require_file)
    all_names = set()
    selected_names = set()
    for artifact in artifacts:
        artifact["tests"] = test_names(artifact)
        artifact["ignored"] = test_names(artifact, ignored=True)
        all_names.update(artifact["tests"])
        eligible = set(artifact["ignored"]) if args.ignored else set(artifact["tests"]) - set(artifact["ignored"])
        artifact["selected"] = sorted(
            name for name in eligible
            if (name == args.filter if args.exact else args.filter in name)
            and name not in exact_skips
            and not any(skip in name for skip in args.skip)
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
        command = (["sudo", "--"] if args.sudo else []) + [artifact["executable"]]
        if args.filter:
            command.append(args.filter)
        if args.exact:
            command.append("--exact")
        if args.ignored:
            command.append("--ignored")
        if args.nocapture:
            command.append("--nocapture")
        for skip in args.skip + exact_skips:
            command.extend(["--skip", skip])
        # Cargo normally starts each test from its own package directory.
        print(f"Running {artifact['target']} ({len(artifact['selected'])} tests)", flush=True)
        subprocess.run(command, cwd=artifact["cwd"], check=True)


if __name__ == "__main__":
    try:
        run(arguments())
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"CI test execution failed: {error}", file=sys.stderr)
        sys.exit(1)
