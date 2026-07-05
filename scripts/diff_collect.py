#!/usr/bin/env python3
"""Differential oracle: cito's collected node IDs must match pytest's.

Runs `pytest --collect-only -q` and `cito collect` against the same directory
(with that directory as cwd, so both emit rootdir-relative node IDs) and
diffs the two sets. Parametrized IDs are normalized by stripping the `[...]`
suffix, because cito does not expand parametrization yet.
"""

import argparse
import pathlib
import re
import subprocess
import sys


def pytest_ids(target: pathlib.Path, python: str) -> set[str]:
    proc = subprocess.run(
        [python, "-m", "pytest", "--collect-only", "-q", "."],
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
        if "::" in line and " " not in line:
            ids.add(re.sub(r"\[.*\]$", "", line))
    return ids


def cito_ids(target: pathlib.Path, binary: str) -> set[str]:
    proc = subprocess.run(
        [binary, "collect", "."], capture_output=True, text=True, cwd=target
    )
    if proc.returncode != 0:
        sys.exit(f"cito collect failed ({proc.returncode}):\n{proc.stderr}")
    return {line.strip() for line in proc.stdout.splitlines() if line.strip()}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target", type=pathlib.Path)
    parser.add_argument("--cito", default=None, help="path to the cito binary")
    parser.add_argument("--python", default=sys.executable)
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
    expected = pytest_ids(target, args.python)
    actual = cito_ids(target, str(binary))

    missing = sorted(expected - actual)
    extra = sorted(actual - expected)
    for node_id in missing:
        print(f"MISSING (pytest collects it, cito doesn't): {node_id}")
    for node_id in extra:
        print(f"EXTRA   (cito collects it, pytest doesn't): {node_id}")
    print(
        f"pytest={len(expected)} cito={len(actual)} "
        f"missing={len(missing)} extra={len(extra)}"
    )
    if missing or extra:
        return 1
    print("OK: node IDs identical (after parametrize normalization)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
