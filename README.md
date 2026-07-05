# cito

[![CI](https://github.com/IShinji/cito/actions/workflows/ci.yml/badge.svg)](https://github.com/IShinji/cito/actions/workflows/ci.yml)

**A fast, pytest-compatible test collector and runner, written in Rust.**

*cito* (Latin: "quickly" — the word doctors still write on urgent orders) makes
the pytest inner loop fast, the way [Ruff](https://github.com/astral-sh/ruff)
and [uv](https://github.com/astral-sh/uv) did for linting and packaging. It
discovers your tests by parsing them with ruff's parser — in milliseconds, not
seconds — and verifies against real pytest that it finds the same node IDs.

> **Status: v0.1.** Collection is real, fast, and differential-tested against
> pytest's own test suite and pandas'. The parallel runner (`cito run`,
> `--warm`) is a working preview. Not yet production-ready; the compatibility
> contract below is the plan for getting there.

## Benchmarks

Collection, wall time (Apple M4 Max, Python 3.14, warm cache; see
[BENCHMARKS.md](BENCHMARKS.md) to reproduce):

| suite | tests | `pytest --collect-only -q` | `cito collect` | speedup |
|---|---:|---:|---:|---:|
| pandas 3.0.3 (installed) | 197,077 | 9.48 s | **0.26 s** | 36x |
| pytest 9.1.1 (own suite) | 4,231 | 0.62 s | **&lt;0.01 s** | &gt;100x |
| synthetic corpus | 11,000 | 0.70 s | **0.01 s** | 70x |

And the part that matters more than speed — **the same answers**:

| suite | pytest IDs | missing | wrong extras |
|---|---:|---:|---:|
| pytest's own suite | 4,231 | 1 (a `.txt` doctest; doctest support is a known gap) | 0 |
| pandas 3.0.3 | 197,077 | 26 (0.013%) | 2 |
| flask 3.1.3 | 482 | 0 | 0 |
| rich 15.0.0 | 981 | 0 | 0 |

`scripts/diff_collect.py` computes this equivalence on every CI run.

## Why

- pytest is the default test runner of the Python world (~a billion downloads
  a month), and on large suites *collection alone* takes seconds to minutes
  ([pytest#5516](https://github.com/pytest-dev/pytest/issues/5516)). Every
  `pytest -k one_test` pays that tax before a single test runs.
- The tax is no longer only human: coding agents run the suite dozens of times
  per task. Suite latency is agent-loop latency.
- Ruff and uv proved the recipe: reimplement the hot path in Rust, treat the
  existing ecosystem's behavior as a compatibility contract, win by 10–100x.

## Install

Not on PyPI yet. From a checkout (builds with [maturin](https://maturin.rs)
or plain cargo):

```console
$ uv tool install .          # or: pip install .
$ cargo install --path .     # Rust toolchain route
```

## What works today

```console
$ cito collect                    # pytest-convention discovery, in parallel
tests/test_api.py::test_get
tests/test_api.py::TestAuth::test_login[admin]
$ cito collect --count            # just the number
$ cito collect --json             # grouped by file, for tools and agents
$ cito collect --python .venv/bin/python   # env-aware: honors module-level importorskip
$ cito run -n 8                   # parallel runner (subprocess workers)
$ cito run -n 8 --warm            # v0.2 preview: pytest workers stay warm across chunks
$ cito run tests/test_api.py::TestAuth     # node-ID selectors, like pytest
$ cito run --lf                   # only the tests that failed last time
$ cito run --watch --warm         # live loop: warm workers survive across saves
$ cito run -k "http and not slow" # keyword expressions
$ cito run -x                     # stop at first failure (--maxfail N)
$ cito run --json                 # machine-readable summary for agents/CI
```

- **Configuration discovery**: `pytest.ini`, `pyproject.toml` (`[tool.pytest]`
  and `[tool.pytest.ini_options]`), `tox.ini`, `setup.cfg`; rootdir inference;
  `testpaths`, `python_files` / `python_classes` / `python_functions` (prefix
  and glob forms, including path patterns like `testing/python/*.py`),
  `norecursedirs`, virtualenv detection.
- **Collection semantics**: definition-order node IDs; nested classes;
  `__init__`/`__new__` exclusion; **cross-module base-class resolution** (the
  pandas `TestMaskedArrays(base.ExtensionTests)` pattern — resolved through
  imports, relative imports, star-imports, and Python's package-root sys.path
  semantics); `unittest.TestCase` subclasses collected regardless of naming.
- **Parametrize expansion with an honesty contract**: literal scalars, tuples,
  stacked decorators (cartesian, pytest's piece order), `ids=`, class-level
  parametrize, module-level parametrize aliases, and duplicate-ID
  disambiguation are expanded *exactly*. Anything static analysis cannot
  prove — floats, computed values, `indirect=`, parametrized or autouse
  fixtures, `pytest_generate_tests` in scope, unknown decorators — falls back
  to the bare test name rather than risking a wrong ID. A bare name is always
  a valid pytest selector for all of its parametrizations.
- **Environment awareness (opt-in)**: `--python PY` probes module-level
  `pytest.importorskip("...")` requirements and drops modules pytest would
  skip in that environment. Without it, collection is fully static.
- **Runner preview**: `cito run` partitions node IDs (whole files together,
  like `xdist --dist loadfile`) across N pytest subprocesses; `--warm` keeps
  workers alive and runs chunks via `pytest.main()` in-process — execution
  stays inside real CPython, so conftest, fixtures, and plugins keep working.
  Corpus numbers: serial pytest 2.48 s → `cito run -n 8` 1.24 s → `--warm`
  1.20 s.
- **Scheduling**: failures are recorded in `.cito/lastfailed` (rootdir);
  every run schedules previously-failed files first, then most-recently
  modified files — the fastest possible time-to-first-signal. `--lf` runs
  only the recorded failures (the cache clears as they pass). `--watch`
  keeps running: save a test file and only that file reruns.
- **Node-ID selectors**: `cito run tests/a.py::TestX` and
  `cito collect tests/a.py::test_y` restrict to matching tests, including
  their parametrizations.

## The compatibility contract

pytest's node IDs are the interface:

1. A cito ID with a `[...]` suffix must match a pytest ID **exactly**.
2. A bare cito ID stands for pytest's parametrized IDs of the same base —
   cito's declared fallback wherever static analysis cannot be sure.

`scripts/diff_collect.py` enforces both directions on every commit against
the fixture trees and a generated corpus, and is run manually against real
repositories (pytest, pandas) before releases.

Known gaps, tracked honestly:

- doctest collection (`--doctest-modules`, `.txt` doctests)
- exact expansion where parametrization is computed at runtime (falls back to
  bare names by design; ~4% of IDs in heavily-fixtured repos like pandas),
  including pytest's duplicate-ID suffixes (True0/True1), which always
  fall back
- `pytest_generate_tests`-generated *extra* tests that add new names
- plugin-driven collection hooks and custom collectors (literal
  `collect_ignore` / `collect_ignore_glob` lists in conftest.py ARE
  supported; computed appends are not)

## Architecture (where this is going)

1. **v0.1 — collection parity + speed** (you are here).
2. **v0.2 — warm workers, properly**: a daemon holding pre-imported CPython
   workers; Rust owns discovery, scheduling, caching, and reporting. First
   cut ships behind `cito run --warm`.
3. **v0.3 — scheduling wins**: failed-first, changed-first (AST diff),
   `--watch`, machine-readable output for agents and CI.
4. **Plugin compatibility matrix**: explicit, tested support for the top-20
   pytest plugins (xdist, cov, asyncio, django, hypothesis, mock, ...).

## Non-goals

- Replacing pytest's test-writing API. Your tests, fixtures, and plugins are
  the point; cito's job is to run them faster.
- A new assertion or fixture DSL.

## Development

```console
$ cargo test                                   # unit + integration tests
$ cargo build --release
$ uv run --with pytest scripts/diff_collect.py tests/fixtures/basic
$ bench/bench_collect.sh                       # reproduce the corpus numbers
```

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT License](LICENSE-MIT) at your option.
