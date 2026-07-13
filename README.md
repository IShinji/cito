# cito

[![CI](https://github.com/IShinji/cito/actions/workflows/ci.yml/badge.svg)](https://github.com/IShinji/cito/actions/workflows/ci.yml)
[![PyPI](https://img.shields.io/pypi/v/cito.svg)](https://pypi.org/project/cito/)
[![Python 3.9+](https://img.shields.io/badge/python-3.9%2B-blue.svg)](https://pypi.org/project/cito/)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**A fast, pytest-compatible test collector and runner, written in Rust.**

*cito* (Latin: "quickly" — the word doctors still write on urgent orders) does for
the pytest inner loop what [Ruff](https://github.com/astral-sh/ruff) and
[uv](https://github.com/astral-sh/uv) did for linting and packaging: reimplement the
hot path in Rust, treat the existing tool's behavior as a compatibility contract, and
win by 10–100×. cito discovers your tests by parsing them with Ruff's parser — in
milliseconds — and verifies against real pytest that it finds the same node IDs.

> Collecting home-assistant's 81,251 tests: `pytest --collect-only -q` **16.91 s** →
> `cito collect` **0.11 s** (156×) — producing the identical node IDs.

*Pre-1.0 and under active development: collection is differentially tested against
44 real suites (~716k node IDs checked); the runner is a preview. See the
[FAQ](#faq).*

## Highlights

- ⚡ **Up to 156× faster collection.** `pytest --collect-only` takes seconds to
  minutes on large suites
  ([pytest#5516](https://github.com/pytest-dev/pytest/issues/5516)) before a single
  test runs; cito pays that tax in milliseconds — see [benchmarks](#benchmarks).
- 🧾 **The same node IDs as pytest, as a contract.** ~716k IDs differentially
  checked across 44 real suites plus randomized fuzzing, enforced on every commit —
  see [compatibility](#compatibility).
- 🔁 **Run only what a change can reach.** `--changed` follows the AST import graph,
  `--lf` reruns last failures, `--watch` reruns on save.
- 🔥 **Warm workers and a per-project daemon** put one-shot runs at ~0.02 s.
- 🧩 **Your plugins keep working** — pytest executes your tests unchanged; a tested
  [plugin matrix](#plugins) tracks what affects collection.
- 🤖 **Built for the agent loop.** Coding agents run the suite dozens of times per
  task, so suite latency is loop latency.
- 🐍 **Static and configuration-aware.** Discovers `pytest.ini`, `pyproject.toml`,
  `tox.ini`, and `setup.cfg`; env-aware collection with `--python`.
- 📦 **A single Rust binary.** `pip install cito` or `uv tool install cito`, no
  runtime dependencies.

## Installation

cito is published to PyPI as a prebuilt wheel — install it with pip or uv:

```console
$ pip install cito
$ uv tool install cito       # or, in a project: uv add --dev cito
```

To build the latest from a checkout you need a Rust toolchain; it builds with
[maturin](https://maturin.rs) or plain cargo:

```console
$ uv tool install .          # or: pip install .
$ cargo install --path .     # Rust-toolchain route
```

> **PyPI has v0.2.0** — the first public release: fast collection, the parallel
> runner, `--warm` workers, the per-project daemon, and `--lf` / `--json` /
> `--watch` / `--changed`. **`main` is v0.3**, which upgrades `--changed` to
> AST-level impact analysis (the transitive import graph described below) and adds
> the plugin compatibility matrix — build from source to use it.

## Usage

### Collecting tests

```console
$ cito collect                    # pytest-convention discovery, in parallel
tests/test_api.py::test_get
tests/test_api.py::TestAuth::test_login[admin]

$ cito collect --count            # just the number
$ cito collect --json             # grouped by file, for tools and agents
$ cito collect -k "http and not slow"        # keyword filter at collection time
$ cito collect --python .venv/bin/python     # env-aware: honors module-level importorskip
```

### Running tests

```console
$ cito run -n 8                   # parallel runner (subprocess workers)
$ cito run -n 8 --warm            # workers stay warm across chunks
$ cito run tests/test_api.py::TestAuth       # node-ID selectors, like pytest
$ cito run -k "http and not slow" # keyword expressions
$ cito run -m "not slow"          # mark expressions, filtered at collection time
$ cito run -x                     # stop at first failure (--maxfail N)
$ cito run --json                 # machine-readable summary for agents/CI
$ cito run -- --cov=mypkg         # pass anything through to pytest; parallel
                                  # coverage fragments are combined for you
```

### Running only what changed

```console
$ cito run --lf                   # only the tests that failed last time
$ cito run --changed              # only tests a change can reach (AST import graph)
$ cito run --watch --warm         # live loop: warm workers survive across saves
```

`--changed` runs only the tests a change can actually reach: a test file counts as
impacted when it, a conftest above it, the config file, or **any project file it
transitively imports** changed since the last run. Change one core module and exactly
the tests that import it (directly or through other project modules) run; touch
nothing and `cito run --changed` runs nothing. Resolution is AST-level and stays
inside the project — third-party packages are treated as stable.

### The warm daemon

```console
$ cito run --daemon               # hit the per-project warm daemon: one-shot
                                  # runs in ~0.02s (auto-starts; unix)
$ cito daemon status              # start | stop | status
```

`--warm` keeps pytest workers alive and runs chunks via `pytest.main()` in-process,
so conftest, fixtures, and plugins keep working; the daemon extends that across CLI
invocations. Execution always stays inside real CPython — cito partitions node IDs
(whole files together, like `xdist --dist loadfile`) and hands them to pytest.

## Compatibility

pytest's node IDs are the interface, and cito treats them as a contract:

1. A cito ID with a `[...]` suffix matches a pytest ID **exactly**.
2. A bare cito ID stands for pytest's parametrized IDs of the same base — cito's
   declared fallback wherever static analysis cannot be sure. A bare name is always a
   valid pytest selector for all of its parametrizations.

`scripts/diff_collect.py` enforces both directions on every commit — against fixture
trees, a generated corpus, and **randomized differential fuzzing** (`bench/fuzz_gen.py`
builds seeded projects mixing nested classes, cross-module inheritance, re-exports,
parametrize variants, fixtures, marks, and shadowing; 100 seeds pass locally, three
run in CI). Before releases, `scripts/validate_repos.py` reruns the whole matrix
against fresh clones.

**Differential results** — pytest IDs vs. cito, on a few of the largest and most
hostile suites:

| suite | pytest IDs | missing | wrong extras |
|---|---:|---:|---:|
| home-assistant 2026.7.1 (core + 249 integrations) | 81,251 | 0 | 0 |
| botocore 1.43.40 | 78,668 | 0 | 0 |
| packaging 26.2 | 61,513 | 0 | 0 |
| pandas 3.0.3 | 197,077 | 26 (0.013%) | 2 |
| pytest 9.1.1 (its own suite) | 4,231 | 1 (a `.txt` doctest) | 0 |

Across 44 suites, ~716k node IDs are checked; the vast majority are exact.

<details>
<summary>Full differential matrix — 44 suites, including the honestly-partial cases</summary>

| suite | pytest IDs | missing | wrong extras |
|---|---:|---:|---:|
| pytest's own suite | 4,231 | 1 (a `.txt` doctest; doctest support is a known gap) | 0 |
| home-assistant 2026.7.1 (core + 249 integrations) | 81,251 | 0 | 0 |
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
| fastapi 0.139.0 | 3,323 | 0 | 0 |
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
| sphinx 9.1.0 | 2,424 | 0 | 0 |
| pip 26.1.2 | 2,997 | 0 | 0 |
| scipy wheel (site-packages) | 96,387 | 3,467 (3.6%: `type()` class factories) | 5 |
| numpy 3.x wheel (site-packages) | 49,443 | 760 (1.5%: `type()` loop-generated SIMD classes) | 0 |
| trio 0.33.0 | 895 | 0 | 0 |
| pillow 12.3.0 | 5,219 | 0 | 0 |
| aiohttp 3.14.1 | 4,364 | 0 | 0 |
| hypothesis 6.156.1 | 3,647 | 0 | 3 (asyncio wrapper dynamics) |

(`scripts/validate_repos.py` reruns the whole matrix against fresh clones — the
release gate. sqlalchemy and django are documented out: their suites require
project-specific collection-bootstrap plugins that no static tool can see.)

</details>

<details>
<summary>What cito understands about collection, in detail</summary>

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
  to the bare test name rather than risking a wrong ID.
- **Environment awareness (opt-in)**: `--python PY` probes module-level
  `pytest.importorskip("...")` requirements and drops modules pytest would
  skip in that environment. Without it, collection is fully static.
- **Scheduling**: failures are recorded in `.cito/lastfailed` (rootdir); every
  run schedules previously-failed files first, then changed files, then
  most-recently modified — the fastest possible time-to-first-signal. `--lf`
  runs only the recorded failures (the cache clears as they pass).
- **Mark expressions**: `-m "not slow"` filters on statically-harvested marks
  (function, class chain, and module `pytestmark`) *at collection time* —
  deselected tests are never scheduled. `-m`/`-k` inside config `addopts` are
  honored (CLI wins). Per-parametrize `pytest.param(marks=...)` marks are not
  filtered (approximation).
- **Namespace collection**: test classes/functions *imported* into a test
  module are collected there too (the urllib3-contrib rerun pattern);
  `@pytest.fixture(name=...)` renames are tracked; anyio's plugin-injected
  backend parametrization is detected and falls back safely.

</details>

Known gaps, tracked honestly:

- doctest collection (`--doctest-modules`, `.txt` doctests)
- exact expansion where parametrization is computed at runtime (falls back to
  bare names by design; ~4% of IDs in heavily-fixtured repos like pandas),
  including pytest's duplicate-ID suffixes (True0/True1), which always fall back
- `pytest_generate_tests`-generated *extra* tests that add new names
- import-time class factories that synthesize tests from data files
  (jsonschema builds ~7k tests from the JSON-Schema-Test-Suite this way)
- plugin-driven collection hooks and custom collectors (literal
  `collect_ignore` / `collect_ignore_glob` lists in conftest.py ARE supported;
  computed appends are not), and plugins that redefine collection semantics
  outright (pytest-relaxed)

### Plugins

Plugins run untouched at execution time — cito hands pytest real node IDs and pytest
loads your plugins as always. What matters for cito is whether a plugin changes
*collection*; this matrix is the current, tested state:

| plugin | collection-time behavior | status | evidence |
|---|---|---|---|
| pytest-asyncio | async tests collected normally | supported | its own suite (299 IDs exact), aiohttp, home-assistant |
| anyio | backend fixture parametrizes tests | supported (declared fallback) | httpx 1,418 / starlette 981 exact |
| pytest-django | standard collection | supported | django-rest-framework 1,552 exact |
| hypothesis | `@given` wraps, IDs unchanged | supported | hypothesis suite, pandas |
| pytest-xdist | none (runtime distribution) | compatible — cito schedules its own workers; pass `-n` through `--` if you must | pytest-xdist suite 212 exact |
| pytest-cov | none (runtime) | supported — per-chunk `COVERAGE_FILE` isolation, auto-combine | coverage.py suite 1,586 exact |
| pytest-mock | none (fixture) | compatible | used across validated repos |
| pytest-timeout | none (runtime) | compatible | home-assistant venv |
| syrupy / snapshot plugins | none (fixture/report) | compatible | home-assistant, textual 3,467 exact |
| pytest-socket | none (runtime guard) | compatible | pip 2,997 exact (its addopts require it) |
| pytest-rerunfailures / flaky | none (runtime reruns) | compatible | pytest's own suite |
| pytest-randomly | runtime ordering only | compatible (cito orders files; in-chunk order is pytest's) | — |
| pytest-relaxed | **rewrites collection semantics** | not supported (documented out) | paramiko |
| ipython ipdoctest, doctest plugins | **collect non-test sources** | not supported (doctest is a known gap) | ipython, dateutil |
| custom `pytest_collect_file` collectors | **turn arbitrary files into tests** | not supported by design | pygments, sqlalchemy/django bootstrap plugins |

## Benchmarks

Collection, wall time (Apple M4 Max, Python 3.14, warm cache; see
[BENCHMARKS.md](BENCHMARKS.md) to reproduce, including the execution/runner numbers):

| suite | tests | `pytest --collect-only -q` | `cito collect` | speedup |
|---|---:|---:|---:|---:|
| home-assistant 2026.7.1 (validated scope) | 81,251 | 16.91 s | **0.11 s** | 156× |
| pandas 3.0.3 (installed) | 197,077 | 9.48 s | **0.26 s** | 36× |
| pytest 9.1.1 (own suite) | 4,231 | 0.62 s | **&lt;0.01 s** | &gt;100× |
| synthetic corpus | 11,000 | 0.70 s | **0.01 s** | 70× |

`scripts/diff_collect.py` computes the collection equivalence on every CI run, so the
speed above always comes with the same answers.

## Roadmap

cito ships in phases, each gated on differential parity:

1. **v0.1 — collection parity + speed** — shipped.
2. **v0.2 — warm workers** — shipped: `--warm` pools within a run, `--watch` keeps
   them across saves, and `cito run --daemon` keeps them across CLI invocations
   (workers self-purge modules whose files changed).
3. **v0.3 — scheduling & impact analysis** — shipped on `main`: failed-first
   ordering, `--lf`, `--json`, and AST-level `--changed` (follows the project import
   graph, conftest chain, and config).
4. **Plugin compatibility matrix** — shipped — see [Compatibility](#compatibility);
   each row is backed by a differential-validated repository.
5. **Toward 1.0** — freeze the CLI surface and the node-ID contract.

## Contributing

Contributions welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) for setup, the test
pyramid, and how to report a collection mismatch (the most valuable bug report for a
compatibility tool). The short version:

```console
$ cargo test                                   # unit + fixture-tree integration tests
$ cargo fmt && cargo clippy --release --all-targets
$ uv run --with pytest scripts/diff_collect.py tests/fixtures/basic
```

CI blocks on Linux, macOS, and Windows plus a pytest node-ID parity job. If you change
collection behavior, name the fixture tree or validated repository that covers it —
and if none does, add a fixture.

## FAQ

**How do you pronounce cito?** "KEE-toh" — Latin for "quickly," the word doctors still
write on prescriptions and orders to mean *urgently*. Say it however you like.

**Is cito a replacement for pytest?** No — and that's a non-goal. cito doesn't execute
your tests itself, and it adds no new assertion or fixture DSL; pytest does the
running. cito makes the inner loop fast — collection, selection, scheduling, warm
workers — and hands pytest the exact node IDs to run. Your tests, fixtures, and
plugins are the point.

**Which Python and pytest versions does it support?** Any Python ≥3.9 with your
existing pytest; node-ID parity is validated against pytest 8.x and 9.x (a few repos
are pinned per their own constraints). Static collection never imports your code;
opt-in `--python` probing uses whatever interpreter you point it at.

**Why is it so much faster?** Collection never starts a Python interpreter, imports
your modules, or loads plugins and conftest — the three costs that dominate
`pytest --collect-only`. cito parses the source with Ruff's parser and resolves
pytest's collection semantics statically, in parallel, in Rust.

**Is it ready for production?** Collection is the mature part: differentially tested
against 44 real suites (~716k node IDs) plus randomized fuzzing, with node-ID parity
enforced in CI on every commit. The runner is a preview — it drives real pytest
subprocesses, so your fixtures, conftest, and plugins run exactly as before, but its
CLI is younger. cito is pre-1.0: `0.x` minor versions may change behavior where pytest
compatibility requires it (patches are fixes only); **1.0 will freeze the CLI and the
node-ID contract.**

**Does it work on Windows, macOS, and Linux?** Yes — CI blocks on all three. The warm
daemon is unix-only for now; everything else is cross-platform.

## Acknowledgements

- [Ruff](https://github.com/astral-sh/ruff) — cito parses every test file with its
  `ruff_python_parser` and `ruff_python_ast` crates, and Ruff (with
  [uv](https://github.com/astral-sh/uv)) is the proof that reimplementing a hot path
  in Rust while keeping the ecosystem's contract is a recipe worth following.
- [pytest](https://github.com/pytest-dev/pytest) — the behavior cito treats as a
  contract and differentially tests against on every commit. cito is a faster front
  door to pytest, not a fork of it.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT License](LICENSE-MIT) at your option.
