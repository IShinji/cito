# Changelog

## 0.1.0 (unreleased)

Initial release.

- `cito collect`: pytest-convention test discovery via ruff's parser,
  parallelized with rayon. pytest config discovery (`pytest.ini`,
  `pyproject.toml` `[tool.pytest]`/`[tool.pytest.ini_options]`, `tox.ini`,
  `setup.cfg`), rootdir inference, `testpaths`, prefix/glob/path patterns,
  `norecursedirs`, virtualenv detection.
- Cross-module base-class resolution (imports, relative imports, star
  imports, package-root sys.path semantics); `unittest.TestCase` subclasses.
- Parametrize expansion with a fallback contract: literals expand to exact
  pytest IDs; anything runtime-dependent (floats, computed values,
  `indirect=`, parametrized/autouse fixtures, `pytest_generate_tests`,
  unknown decorators) emits the bare test name, which remains a valid pytest
  selector.
- `--python` probe for module-level `pytest.importorskip` requirements.
- `cito run`: parallel execution across pytest subprocesses; `--warm` keeps
  workers alive across chunks (`pytest.main()` in-process).
- `--` passthrough of arbitrary pytest args to every chunk; parallel-safe
  coverage: each chunk gets its own COVERAGE_FILE and fragments are
  auto-combined into `.coverage` after the run (verified: 2-chunk run of a
  2-function module reports 100%). Windows CI promoted to blocking.
- `cito run --json` (machine-readable summary), pytest-compatible exit
  code 5 when nothing is collected, `-k` on collect, conftest
  `collect_ignore`/`collect_ignore_glob` (literal lists), `.cito/` cache
  self-gitignores, CI matrix adds macOS + experimental Windows.
- `-k` keyword expressions (and/or/not), `-x`/`--maxfail` fail-fast with
  chunk skipping, `pytest.param(...)` ID rendering (including `id=`),
  re-export chasing for base classes (`base/__init__.py` patterns), warm
  worker pool persists across `--watch` iterations with stale-module
  purging for every changed `.py` file.
- Scheduling: failed-first + recently-modified-first ordering by default,
  `.cito/lastfailed` cache with automatic clearing, `--lf` (only last
  failures), `--watch` (rerun changed test files on save), node-ID selector
  arguments (`path::Class::test`), one-shot batched importorskip probing.
- Differential parity, checked in CI: pytest 9.1.1's own suite 4,231 IDs →
  1 known gap (doctest), 0 extras; pandas 3.0.3 197,077 IDs → 26 missing / 2 extra (0.014%);
  flask 3.1.3 (482 IDs) and rich 15.0.0 (981 IDs) → exact.
- Collection speed: pandas 9.48 s → 0.26 s (36x); pytest's repo 0.62 s →
  <0.01 s; 11k-test corpus 0.70 s → 0.01 s.
