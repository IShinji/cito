# cito

[![CI](https://github.com/IShinji/cito/actions/workflows/ci.yml/badge.svg)](https://github.com/IShinji/cito/actions/workflows/ci.yml)

**A fast, pytest-compatible test collector and runner, written in Rust.**

*cito* (Latin: "quickly" — the word doctors still write on urgent orders) makes
the pytest inner loop fast, the way [Ruff](https://github.com/astral-sh/ruff)
and [uv](https://github.com/astral-sh/uv) did for linting and packaging. It
discovers your tests by parsing them with ruff's parser — in milliseconds, not
seconds — and verifies against real pytest that it finds the same node IDs.

> **Status: v0.2.** Collection is differential-tested against pytest's own
> suite, pandas, flask, rich, and 100 fuzz seeds. The runner does parallel
> subprocesses, warm in-process workers, and a per-project daemon that makes
> one-shot runs ~0.02 s. Not yet 1.0; the compatibility contract below is
> the map.

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
| click 8.4.2 | 1,686 | 0 | 0 |
| jinja2 3.1.6 | 909 | 0 | 0 |
| attrs 26.1.0 | 1,386 | 0 | 0 |
| httpx 0.28.1 | 1,418 | 0 | 0 |
| starlette 1.3.1 | 981 | 0 | 0 |
| urllib3 2.7.0 | 2,273 | 0 | 0 |
| werkzeug 3.1.8 | 969 | 0 | 0 |
| requests 2.34.2 | 633 | 0 | 0 |
| more-itertools 11.1.0 | 722 | 0 | 0 |
| packaging 26.2 | 61,513 | 0 | 0 |
| pluggy 1.6.0 | 124 | 0 | 0 |
| tornado 6.5.7 | 1,322 | 0 | 0 |
| black 26.5.1 | 446 | 0 | 0 |
| pydantic 2.13.4 | 12,775 | 0 | 0 |
| fastapi 0.139.0 | 3,317 | 0 | 0 |
| sympy 1.14.0 | 13,657 | 0 | 16 (0.1%: custom @SKIP import-time machinery) |
| typer 0.26.8 | 1,379 | 0 | 0 |
| networkx 3.6.1 | 7,100 | 0 | 0 |
| cryptography 49.0.0 | 4,472 | 0 | 0 |
| django-rest-framework 3.17.1 | 1,552 | 0 | 0 |
| sqlglot 30.12.0 | 1,127 | 0 | 0 |
| pytest-asyncio 1.4.0 | 299 | 0 | 0 |
| textual 8.2.8 | 3,467 | 0 | 0 |
| pytest-xdist 3.8.0 | 212 | 0 | 0 |
| httpcore 1.0.9 | 220 | 0 | 0 |
| scikit-learn wheel (site-packages) | 47,349 | 0 | 2 |
| botocore 1.43.40 | 78,668 | 0 | 0 |
| tox 4.56.1 | 7,929 | 0 | 0 |
| openai-python 2.44.0 | 6,731 | 0 | 0 |
| coverage.py 7.15.0 | 1,586 | 0 | 0 |
| virtualenv 21.6.0 | 328 | 0 | 0 |
| scipy wheel (site-packages) | 96,387 | 3,467 (3.6%: `type()` class factories) | 5 |
| numpy 3.x wheel (site-packages) | 49,443 | 760 (1.5%: `type()` loop-generated SIMD classes) | 0 |
| trio 0.33.0 | 895 | 0 | 0 |
| pillow 12.3.0 | 5,218 | 0 | 0 |
| aiohttp 3.14.1 | 4,364 | 0 | 0 |
| hypothesis 6.156.1 | 3,647 | 0 | 3 (asyncio wrapper dynamics) |

(`scripts/validate_repos.py` reruns the whole matrix against fresh clones —
the release gate. sqlalchemy and django are documented out: their suites
require project-specific collection-bootstrap plugins that no static tool
can see.)

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
$ cito run -- --cov=mypkg         # pass anything through to pytest; parallel
                                  # coverage fragments are combined for you
$ cito run -m "not slow"          # mark expressions, filtered at collection time
$ cito run --changed              # only files whose content changed since last run
$ cito run --daemon               # hit the per-project warm daemon: one-shot
                                  # runs in ~0.02s (auto-starts; unix)
$ cito daemon status              # start | stop | status
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
  every run schedules previously-failed files first, then files whose
  content hash changed since the last run, then most-recently modified —
  the fastest possible time-to-first-signal. `--changed` runs *only* the
  changed files. `--lf` runs
  only the recorded failures (the cache clears as they pass). `--watch`
  keeps running: save a test file and only that file reruns.
- **Node-ID selectors**: `cito run tests/a.py::TestX` and
  `cito collect tests/a.py::test_y` restrict to matching tests, including
  their parametrizations.
- **Mark expressions**: `-m "not slow"` filters on statically-harvested
  marks (function, class chain, and module `pytestmark`) *at collection
  time* — deselected tests are never scheduled at all. `-m`/`-k` inside
  config `addopts` are honored (CLI wins). Per-parametrize
  `pytest.param(marks=...)` marks are not filtered (approximation).
- **Namespace collection**: test classes/functions *imported* into a test
  module are collected there too (the urllib3-contrib rerun pattern);
  `@pytest.fixture(name=...)` renames are tracked; anyio's plugin-injected
  backend parametrization is detected and falls back safely.

## The compatibility contract

pytest's node IDs are the interface:

1. A cito ID with a `[...]` suffix must match a pytest ID **exactly**.
2. A bare cito ID stands for pytest's parametrized IDs of the same base —
   cito's declared fallback wherever static analysis cannot be sure.

`scripts/diff_collect.py` enforces both directions on every commit against
the fixture trees, a generated corpus, and **randomized differential fuzzing**
(`bench/fuzz_gen.py` builds seeded projects mixing nested classes,
cross-module inheritance, re-exports, parametrize variants, fixtures, marks,
and shadowing; 100 seeds pass locally, three run in CI). Real repositories
(pytest, pandas, flask, rich) are checked before releases.

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

## Architecture

1. **v0.1 — collection parity + speed**: shipped.
2. **v0.2 — warm workers**: shipped — `--warm` pools within a run, `--watch`
   keeps them across saves, and `cito run --daemon` keeps them across CLI
   invocations (workers self-purge modules whose files changed).
3. **v0.3 — scheduling**: mostly shipped (failed-first, content-changed-first,
   `--lf`, `--changed`, `--json`); remaining: AST-level impact analysis.
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
