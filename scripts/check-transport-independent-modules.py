#!/usr/bin/env python3
"""Check that every module the dummy job owns compiles to the same tests in both feature sets.

`scripts/ci-transport-independent-modules.txt` names modules whose tests only the dummy job runs
(see `.github/CI.md`). Moving them is sound when they behave identically in the native and the
dummy build:

1. Same tests, same code: no file of a listed module, and no declaration of it, mentions
   `feature = "dummy"`. Checked here.
2. Same behaviour: a test that compiles in both builds reaches only items present in both, and the
   shared items whose bodies differ under `dummy` are the transport backend and code reachable
   only through it, plus test hooks (the simulation runtime, the outbound test trace) that are
   inert until a dummy-only API arms them. So a test that constructs no transport behaves the
   same in both. The dummy job enforces that half: it runs the listed modules with
   `RINGS_DUMMY_TRANSPORT_FORBIDDEN` set, and the dummy transport aborts the run if any test
   constructs one, however indirectly.
"""

import argparse
from pathlib import Path
import re
import sys

ROOTS = {"rings_core": Path("crates/core/src"), "rings_node": Path("crates/node/src")}
DUMMY_FEATURE = 'feature = "dummy"'


def entries(path: Path) -> list[tuple[str, str]]:
    """Read `<target> <module>` lines, ignoring comments and blank lines."""
    lines = [line.strip() for line in path.read_text().splitlines()]
    return [tuple(line.split(" ", 1)) for line in lines if line and not line.startswith("#")]


def module_files(root: Path, module: str) -> list[Path]:
    """The files of a file-backed module: `a/b.rs` and everything under `a/b/`."""
    *parents, name = module.split("::")
    directory = root.joinpath(*parents)
    files = [directory / f"{name}.rs"] if (directory / f"{name}.rs").is_file() else []
    if (directory / name).is_dir():
        files.extend(sorted((directory / name).rglob("*.rs")))
    if not files:
        raise ValueError(f"listed module has no file of its own (inline modules cannot be listed): {module}")
    return files


def declaring_file(root: Path, module: str) -> Path:
    """The file that declares `mod <name>` for `module`."""
    *parents, _ = module.split("::")
    if not parents:
        return root / "lib.rs"
    directory = root.joinpath(*parents[:-1])
    for candidate in (directory / f"{parents[-1]}.rs", directory / parents[-1] / "mod.rs"):
        if candidate.is_file():
            return candidate
    raise ValueError(f"no file declares the parent of listed module: {module}")


def declaration_attributes(source: str, name: str) -> str | None:
    """The attributes on the `mod <name>` declaration in `source`, or `None` if there is none.

    Attributes are collected forward until their brackets balance, so a multi-line `#[cfg(...)]`
    is read whole; doc comments keep pending attributes; any other line clears them.
    """
    declaration = re.compile(rf"^\s*(pub(\([^)]*\))?\s+)?mod\s+{re.escape(name)}\s*[;{{]")
    pending, attribute, depth = [], [], 0
    for line in source.splitlines():
        stripped = line.strip()
        if depth > 0 or stripped.startswith("#["):
            attribute.append(stripped)
            depth += stripped.count("[") - stripped.count("]")
            if depth <= 0:
                pending.append(" ".join(attribute))
                attribute, depth = [], 0
            continue
        if declaration.match(line):
            return " ".join(pending)
        if not stripped.startswith("//"):
            pending = []
    return None


def check(path: Path) -> list[str]:
    """Return one message per violation of clause 1 for every listed module."""
    problems = []
    for target, module in entries(path):
        root = ROOTS.get(target)
        if root is None:
            problems.append(f"unknown target {target!r} for module {module}")
            continue
        try:
            for file in module_files(root, module):
                if DUMMY_FEATURE in file.read_text():
                    problems.append(f"{target} {module}: {file} mentions {DUMMY_FEATURE}")
            parent = declaring_file(root, module)
            attributes = declaration_attributes(parent.read_text(), module.rsplit("::", 1)[-1])
            if attributes is None:
                problems.append(f"{target} {module}: {parent} does not declare it")
            elif DUMMY_FEATURE in attributes:
                problems.append(f"{target} {module}: its declaration in {parent} is gated on {DUMMY_FEATURE}")
        except ValueError as error:
            problems.append(f"{target} {module}: {error}")
    return problems


def main() -> None:
    """Report every violation and fail if there is one."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("modules", type=Path, help="The `<target> <module>` list")
    problems = check(parser.parse_args().modules)
    for problem in problems:
        print(problem, file=sys.stderr)
    if problems:
        sys.exit(1)


if __name__ == "__main__":
    main()
