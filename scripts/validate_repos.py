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
import os
import pathlib
import subprocess
import sys

REPO = pathlib.Path(__file__).resolve().parent.parent

# (package, git url, tag templates, diff args, known-gap substrings, max_extra)
# max_extra > 0 marks a documented-partial repo: zero missing required, but
# up to that many extras tolerated (e.g. sympy's custom @SKIP machinery and
# environment-conditional test definitions).
MATRIX = [
    ("click", "https://github.com/pallets/click", ["{v}", "v{v}"], [], [], 0),
    ("jinja2", "https://github.com/pallets/jinja", ["{v}", "v{v}"], [], [], 0),
    ("attrs", "https://github.com/python-attrs/attrs", ["{v}", "v{v}"], [], [], 0),
    ("httpx", "https://github.com/encode/httpx", ["{v}", "v{v}"], [], [], 0),
    ("starlette", "https://github.com/encode/starlette", ["{v}", "v{v}"], [], [], 0),
    ("urllib3", "https://github.com/urllib3/urllib3", ["{v}", "v{v}"], [], [], 0),
    ("werkzeug", "https://github.com/pallets/werkzeug", ["{v}", "v{v}"], [], [], 0),
    ("requests", "https://github.com/psf/requests", ["v{v}", "{v}"], [], [], 0),
    ("more_itertools", "https://github.com/more-itertools/more-itertools", ["v{v}", "{v}"], [], [], 0),
    ("packaging", "https://github.com/pypa/packaging", ["{v}", "v{v}"], [], [], 0),
    ("pluggy", "https://github.com/pytest-dev/pluggy", ["{v}", "v{v}"], [], [], 0),
    ("tornado", "https://github.com/tornadoweb/tornado", ["v{v}", "{v}"], ["tornado/test"], [], 0),
    ("black", "https://github.com/psf/black", ["{v}", "v{v}"], [], [], 0),
    ("pydantic", "https://github.com/pydantic/pydantic", ["v{v}", "{v}"], [], [], 0),
    ("fastapi", "https://github.com/fastapi/fastapi", ["{v}", "v{v}"], [], [], 0),
    ("sympy", "https://github.com/sympy/sympy", ["sympy-{v}", "{v}"], [], [], 20),
    ("pillow", "https://github.com/python-pillow/Pillow", ["{v}", "v{v}"], [], [], 0),
    # hypothesis: 3 extras = unittest generator-method dynamics in its own
    # asyncio wrappers.
    ("hypothesis", "https://github.com/HypothesisWorks/hypothesis", ["v{v}", "hypothesis-python-{v}"], ["hypothesis/tests/cover"], [], 3),

    ("typer", "https://github.com/fastapi/typer", ["{v}", "v{v}"], [], [], 0),
    ("trio", "https://github.com/python-trio/trio", ["v{v}", "{v}"], [], [], 0),
    ("djangorestframework", "https://github.com/encode/django-rest-framework", ["{v}", "v{v}"], [], [], 0),
    ("networkx", "https://github.com/networkx/networkx", ["networkx-{v}", "{v}", "v{v}"], [], [], 0),
    ("sqlglot", "https://github.com/tobymao/sqlglot", ["v{v}", "{v}"], [], [], 0),
    ("cryptography", "https://github.com/pyca/cryptography", ["{v}", "v{v}"], [], [], 0),
    # litestar: needs a pinned full dev environment (time_machine API drift
    # etc.) — polars family.
    ("pytest_asyncio", "https://github.com/pytest-dev/pytest-asyncio", ["v{v}", "{v}"], [], [], 0),
    # aiohttp: ~19 extras = marks applied dynamically by conftest hooks
    # (pytest_collection_modifyitems tagging whole directories) interacting
    # with addopts -m deselection.
    ("aiohttp", "https://github.com/aio-libs/aiohttp", ["v{v}", "{v}"], [], [], 20),
    # Documented out of the matrix (not collection-semantics issues):
    # - pygments: custom pytest_collect_file collectors turn example files
    #   (.pmod, ...) into test items
    # - polars: suite imports ~40 optional integrations (deltalake,
    #   pyiceberg, moto[server], ...) — needs the full dev environment
    # - psutil: repo source tree shadows the compiled wheel; suite must run
    #   against an installed build
    # - hypothesis: monorepo-subproject rootdir nuance under investigation
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
        # Neutralize git-lfs (may be configured globally but not installed);
        # LFS payloads are never needed for collection.
        env = dict(os.environ, GIT_LFS_SKIP_SMUDGE="1")
        result = sh(
            [
                "git",
                "-c", "filter.lfs.smudge=",
                "-c", "filter.lfs.process=",
                "-c", "filter.lfs.required=false",
                "clone", "-q", "--depth", "1", "--branch", tag, url, str(dest),
            ],
            env=env,
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
    for package, url, templates, extra, ignore, max_extra in MATRIX:
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
        if result.returncode != 0 and max_extra:
            import re as _re

            summary = next(
                (l for l in result.stdout.splitlines() if l.startswith("pytest=")), ""
            )
            m = _re.search(r"missing=(\d+) extra=(\d+)", summary)
            if m and int(m.group(1)) == 0 and int(m.group(2)) <= max_extra:
                status = "PART"
        if status == "FAIL":
            failures += 1
        print(f"{package:12s} {status} {version:10s} {' | '.join(tail)}")
        if status == "FAIL":
            for line in result.stdout.strip().splitlines()[:6]:
                print(f"    {line}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
