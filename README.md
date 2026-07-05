# cito

**A fast, pytest-compatible test collector and runner, written in Rust.**

*cito* (Latin: "quickly" — the word doctors still write on urgent orders) is an
experiment in making the pytest inner loop fast, the way
[Ruff](https://github.com/astral-sh/ruff) and [uv](https://github.com/astral-sh/uv)
did for linting and packaging. It parses your test files with ruff's parser and
discovers tests in milliseconds, not seconds.

> **Status: v0.0.x.** `cito collect` works and is fast. `cito run` is an
> experimental scaffold. Nothing here is production-ready yet — the
> compatibility contract below is the plan for getting there.

## Why

- pytest is the default test runner of the Python world (~a billion downloads a
  month), and on large suites *collection alone* can take tens of seconds to
  minutes ([pytest#5516](https://github.com/pytest-dev/pytest/issues/5516)).
  Every `pytest -k one_test` pays that tax before a single test runs.
- The tax is no longer only human: coding agents run the test suite dozens of
  times per task. Suite latency is agent-loop latency.
- Ruff and uv proved the recipe: reimplement the hot path in Rust, treat the
  existing ecosystem's behavior as a compatibility contract, and win by 10–100x.

## Benchmarks

Synthetic corpus: 500 files, 11,000 tests. Apple M4 Max, Python 3.14.6,
pytest 9.1.1. Reproduce with `bench/bench_collect.sh`.

| collection | cold cache | warm cache |
|---|---:|---:|
| `pytest --collect-only -q` | 3.17 s | 0.70 s |
| `cito collect` | 0.28 s | **0.01 s** |

Same corpus, both tools collect exactly the same 11,000 node IDs
(`scripts/diff_collect.py` verifies this in CI).

Honest caveats: a synthetic corpus has no heavy `conftest.py` imports — real
repositories are usually *worse* for pytest (and no better for cito, which
doesn't import anything). The numbers that matter are Django/pandas/
home-assistant scale, and that's the next milestone. The experimental
`cito run -n 8` currently gives ~1.6x on this corpus (2.57 s → 1.60 s) because
each chunk still pays pytest startup — eliminating that tax is the v0.2 warm
worker design below.

## What works today

```console
$ cito collect                # pytest-convention discovery, in parallel
tests/test_api.py::test_get
tests/test_api.py::TestAuth::test_login
$ cito collect --count        # just the number
$ cito collect --json         # grouped by file, for tools and agents
$ cito run -n 8               # experimental: fan node IDs out across pytest processes
```

- `cito collect [PATHS]` walks the tree with pytest's default conventions
  (`test_*.py` / `*_test.py` files; `Test*` classes without `__init__`,
  recursively; `test*` functions and methods), parses with
  `ruff_python_parser`, in parallel with rayon, and prints pytest-style node
  IDs in definition order.
- `cito run [PATHS] [-n N] [--chunk K] [--python PY]` partitions collected
  node IDs (whole files stay together) across N pytest worker processes.
  Experimental: fixture scoping across chunks behaves like
  `pytest-xdist --dist loadfile`, and reporting is crude.

## The compatibility contract

pytest's node IDs are the interface. `scripts/diff_collect.py` collects the
same tree with both tools and fails on any difference (parametrized IDs are
normalized away for now). CI runs it on every commit; the goal is to grow the
corpus it runs against until it includes real-world projects.

Known gaps, in roadmap order:

- [ ] parametrize expansion (`test_x[3-true]`)
- [ ] `python_files` / `python_classes` / `python_functions` ini overrides
- [ ] rootdir/ini discovery (`pyproject.toml`, `pytest.ini`, `setup.cfg`, `tox.ini`)
- [ ] `testpaths`, `norecursedirs` overrides, `__init__.py` package semantics
- [ ] dynamically generated tests (`pytest_generate_tests`) — requires the
      worker protocol below

## Architecture (where this is going)

1. **v0.1 — collection parity + speed** (you are here): Rust discovery + AST
   collection, differential-tested against pytest.
2. **v0.2 — warm workers**: a daemon holding pre-imported CPython workers.
   *Execution stays inside real CPython*, so fixtures, conftest, and plugins
   keep working — Rust owns discovery, scheduling, caching, and reporting.
   (The cargo-nextest model, adapted to Python's import cost.)
3. **v0.3 — scheduling wins**: failed-first, changed-first (AST diff),
   `--watch`, machine-readable output for agents and CI.
4. **Plugin compatibility matrix**: explicit, tested support for the top-20
   pytest plugins (xdist, cov, asyncio, django, hypothesis, mock, ...),
   tracked in a public table.

## Non-goals

- Replacing pytest's test-writing API. Your tests, fixtures, and plugins are
  the point; cito's job is to run them faster.
- A new assertion or fixture DSL.

## Development

```console
$ cargo test                                          # unit + integration tests
$ cargo build --release
$ uv run --with pytest scripts/diff_collect.py tests/fixtures/basic
$ bench/bench_collect.sh                              # reproduce the numbers above
```

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT License](LICENSE-MIT) at your option.
