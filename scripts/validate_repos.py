#!/usr/bin/env python3
"""Release-gate validation: differential-check cito against a matrix of
real-world repositories, cloned at the tag matching the installed wheel.

Usage:
    python3 scripts/validate_repos.py --python VENV/bin/python --cache DIR [--only pkg,pkg]

The venv must have the packages (and their test dependencies) installed.
Prints one summary line per repo and exits nonzero if any repo regresses.
"""

import argparse
import importlib.metadata
import pathlib
import subprocess
import sys

REPO = pathlib.Path(__file__).resolve().parent.parent

# (package, git url, tag templates, diff args, known-gap substrings)
MATRIX = [
    ("click", "https://github.com/pallets/click", ["{v}", "v{v}"], [], []),
    ("jinja2", "https://github.com/pallets/jinja", ["{v}", "v{v}"], [], []),
    ("attrs", "https://github.com/python-attrs/attrs", ["{v}", "v{v}"], [], []),
    ("httpx", "https://github.com/encode/httpx", ["{v}", "v{v}"], [], []),
    ("starlette", "https://github.com/encode/starlette", ["{v}", "v{v}"], [], []),
    ("urllib3", "https://github.com/urllib3/urllib3", ["{v}", "v{v}"], [], []),
    # Skipped by design (documented): sqlalchemy and django test suites
    # require their own collection-bootstrap plugins; pytest itself collects
    # nothing without them.
]


def sh(args, **kwargs):
    return subprocess.run(args, capture_output=True, text=True, **kwargs)


def clone_at(url: str, templates: list, version: str, dest: pathlib.Path) -> bool:
    if dest.exists():
        return True
    for template in templates:
        tag = template.format(v=version, v_und=version.replace(".", "_"))
        result = sh(
            ["git", "clone", "-q", "--depth", "1", "--branch", tag, url, str(dest)]
        )
        if result.returncode == 0:
            return True
    return False


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--python", required=True)
    parser.add_argument("--cache", type=pathlib.Path, required=True)
    parser.add_argument("--only", default="")
    args = parser.parse_args()
    args.cache.mkdir(parents=True, exist_ok=True)

    only = {p for p in args.only.split(",") if p}
    failures = 0
    for package, url, templates, extra, ignore in MATRIX:
        if only and package not in only:
            continue
        probe = sh(
            [
                args.python,
                "-c",
                f"import importlib.metadata as m; print(m.version({package!r}))",
            ]
        )
        if probe.returncode != 0:
            print(f"{package:12s} SKIP (not installed in venv)")
            continue
        version = probe.stdout.strip()
        dest = args.cache / f"{package}-{version}"
        if not clone_at(url, templates, version, dest):
            print(f"{package:12s} SKIP (no tag for {version})")
            continue
        cmd = [
            sys.executable,
            str(REPO / "scripts/diff_collect.py"),
            str(dest),
            "--python",
            args.python,
        ]
        for substr in ignore:
            cmd += ["--ignore-missing", substr]
        if extra:
            cmd += ["--", *extra]
        result = sh(cmd)
        tail = (result.stdout.strip().splitlines() or ["(no output)"])[-2:]
        status = "OK  " if result.returncode == 0 else "FAIL"
        if result.returncode != 0:
            failures += 1
        print(f"{package:12s} {status} {version:10s} {' | '.join(tail)}")
        if result.returncode != 0:
            for line in result.stdout.strip().splitlines()[:6]:
                print(f"    {line}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
