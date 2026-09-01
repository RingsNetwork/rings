#!/usr/bin/env python3
"""Print the unique test executable for a Cargo JSON target name."""

import json
import sys


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: find-cargo-test-binary.py TARGET_NAME", file=sys.stderr)
        return 2

    target_name = sys.argv[1]
    executables: list[str] = []
    for line in sys.stdin:
        event = json.loads(line)
        target = event.get("target", {})
        executable = event.get("executable")
        if (
            event.get("reason") == "compiler-artifact"
            and target.get("name") == target_name
            and event.get("profile", {}).get("test")
            and isinstance(executable, str)
            and executable not in executables
        ):
            executables.append(executable)

    if len(executables) != 1:
        print(
            f"expected one Cargo test executable for {target_name!r}, found {len(executables)}",
            file=sys.stderr,
        )
        return 1
    print(executables[0])
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
