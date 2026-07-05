#!/usr/bin/env python3
"""Differential oracle: cito's collected node IDs must match pytest's.

Runs `pytest --collect-only -q` and `cito collect` against the same directory
(with that directory as cwd and the same extra args, so both emit
rootdir-relative node IDs) and compares:

- a cito ID with a `[...]` suffix must match a pytest ID exactly;
- a bare cito ID may stand for pytest's parametrized IDs of the same base —
  that is cito's declared fallback for parametrization it cannot resolve
  statically (floats, computed values, pytest_generate_tests, ...).

Anything else is a failure in either direction.
"""

import argparse
import pathlib
import re
import subprocess
import sys


def pytest_ids(target: pathlib.Path, python: str, args: list[str]) -> set[str]:
    proc = subprocess.run(
        [python, "-m", "pytest", "--collect-only", "-q", *args],
        capture_output=True,
        text=True,
        cwd=target,
    )
    if proc.returncode not in (0, 5):
        sys.exit(
            f"pytest --collect-only failed ({proc.returncode}):\n"
            f"{proc.stdout}{proc.stderr}"
        )
    ids = set()
    for line in proc.stdout.splitlines():
        line = line.strip()
        # Node IDs may contain spaces only inside parametrize brackets;
        # anything with a space before the first `[` is prose (warnings),
        # not a node ID.
        head = line.split("[", 1)[0]
        if "::" in head and " " not in head and not line.startswith("="):
            ids.add(line)
    return ids


def cito_ids(
    target: pathlib.Path, binary: str, args: list[str], python: str
) -> set[str]:
    proc = subprocess.run(
        [binary, "collect", "--python", python, *args],
        capture_output=True,
        text=True,
        cwd=target,
    )
    if proc.returncode != 0:
        sys.exit(f"cito collect failed ({proc.returncode}):\n{proc.stderr}")
    return {line.strip() for line in proc.stdout.splitlines() if line.strip()}


def base(node_id: str) -> str:
    return re.sub(r"\[.*\]$", "", node_id)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target", type=pathlib.Path)
    parser.add_argument(
        "args", nargs="*", help="extra paths/args passed to both tools"
    )
    parser.add_argument("--cito", default=None, help="path to the cito binary")
    parser.add_argument("--python", default=sys.executable)
    parser.add_argument(
        "--ignore-missing",
        action="append",
        default=[],
        metavar="SUBSTR",
        help="treat MISSING ids containing SUBSTR as known gaps",
    )
    args = parser.parse_args()

    repo = pathlib.Path(__file__).resolve().parent.parent
    binary = args.cito or next(
        (
            str(p)
            for p in (repo / "target/release/cito", repo / "target/debug/cito")
            if p.exists()
        ),
        None,
    )
    if binary is None:
        sys.exit("no cito binary found; run `cargo build` first or pass --cito")

    target = args.target.resolve()
    expected = pytest_ids(target, args.python, args.args)
    actual = cito_ids(target, str(binary), args.args, args.python)

    bare = {a for a in actual if "[" not in a}
    exact = actual - bare

    missing = sorted(
        pid
        for pid in expected
        if pid not in actual
        and base(pid) not in bare
        and not any(sub in pid for sub in args.ignore_missing)
    )
    used_as_base = {base(pid) for pid in expected}
    extra = sorted(
        (exact - expected)
        | {b for b in bare if b not in expected and b not in used_as_base}
    )
    fallbacks = sum(1 for b in bare if b not in expected and b in used_as_base)

    for node_id in missing:
        print(f"MISSING (pytest collects it, cito doesn't): {node_id}")
    for node_id in extra:
        print(f"EXTRA   (cito collects it, pytest doesn't): {node_id}")
    print(
        f"pytest={len(expected)} cito={len(actual)} "
        f"missing={len(missing)} extra={len(extra)} "
        f"(cito used {fallbacks} declared parametrize fallbacks)"
    )
    if missing or extra:
        return 1
    print("OK: node IDs match")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
