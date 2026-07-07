# Changelog

## 0.2.0 (unreleased)

Initial release.

- Sixth validation wave: botocore 78,668 IDs exact (module re-exports in
  dotted base classes now chase through package __init__ import bindings —
  `from tests import unittest`), tox 7,929, openai-python 6,731,
  coverage.py 1,586 (validated against a pytest-9 oracle: its native
  [tool.pytest] table with minversion=9 is invisible to pytest 8 — cito
  implements pytest-9 semantics), virtualenv 328 — all zero-diff. Oracle
  hardened against repos whose addopts enable xdist (-n0 with fallback);
  validate_repos supports a per-entry pytest-9 interpreter. Documented
  out: dateutil (doctest crash under plugin mix), babel (CLDR build step),
  celery (cloud-SDK imports at collection).
- Fifth validation wave: textual 3,467 / pytest-xdist 212 / httpcore 220
  zero-diff; scikit-learn wheel 47,349 IDs with ZERO missing (2 extras);
  scipy wheel 96,387 IDs with 5 extras (missing 3.6% = type() class
  factories, the genuinely-dynamic family). New systematic rule:
  try-import boolean availability flags (`has_x = True/False` around an
  import) bind to the probe, so `if has_umfpack:` test classes resolve like
  pytest (cleared 154 scipy extras); module-level `del NAME` removes
  bindings. Documented out: anyio (plugin self-test isolation), ipython
  (ipdoctest), mkdocs (unittest-style suite), yt-dlp (setattr-loop
  extractor tests), matplotlib wheel (no baseline images).
- Fourth validation wave: networkx 7,100, cryptography 4,472 (class-body
  `test_x = factory(...)` assignments now emit as fallback methods),
  django-rest-framework 1,552 (pytest-django), sqlglot 1,127 (exact-exact),
  pytest-asyncio 299 — all zero-diff; numpy 49,443 via site-packages with
  760 missing (import-time type() factories) and zero extras. New
  pytest-compatible `--ignore` flag on collect/run; validate_repos clones
  neutralize git-lfs filters.
- Rootdir determination now mirrors pytest's `determine_setup` exactly
  (read from source): section-less pyproject.toml is only a last-resort
  anchor, `pytest.toml`/`.pytest.toml`/`.pytest.ini` are recognized, and
  the setup.py / per-arg / invocation-dir fallback chain is implemented —
  fixes monorepo subprojects (hypothesis). Deferred branch guards: imported
  predicate functions with constant returns (`if is_win32():`) and
  `X = import_module(...)` availability bindings (probed) now decide
  conditional test definitions; `X = Machine.TestCase` bindings emit
  synthetic unittest classes (hypothesis stateful). Matrix: 19/21 exact
  incl. pillow and aiohttp; sympy 16 extras, hypothesis 3.
- Third validation wave (typer, trio exact; pillow 1 extra; aiohttp 19):
  platform/argv guard conditions (`sys.platform == ...`,
  `"--flag" in sys.argv`) are constant-evaluated, so platform-gated
  module skips and branch definitions resolve like pytest on this machine;
  `try: import x / except: pytest.skip()` maps onto the importorskip probe;
  the probe now really imports (a module can exist yet fail to import);
  conftest `pytest_plugins` declarations are resolved and their modules
  join fixture visibility (aiohttp.pytest_plugin's loop parametrization);
  diff oracle hardened against repos whose addopts add `-v`.
  Documented out: pygments (custom file collectors), polars (needs full
  dev env), psutil (source shadows wheel), hypothesis (monorepo rootdir
  nuance under investigation).
- Second validation wave (werkzeug, requests, more-itertools, packaging,
  pluggy, tornado, black, pydantic, fastapi, sympy — 15/16 exact): symlinked
  test directories are followed like pytest does (pydantic vendors
  pydantic-core's tests via a symlink); absolute imports also resolve
  through the probe python's sys.path, so external TestCase bases work
  (aiohttp's AioHTTPTestCase, IsolatedAsyncioTestCase, stdlib chase);
  top-level if/else and try/except test definitions are collected from all
  branches with keep-last shadowing; mark aliases (`slow =
  pytest.mark.slow`) resolve through imports for `-m`; module-level
  conditional `pytest.skip(...)` and imported skip-helper functions drop
  modules under probing; unresolvable `ids=` now falls back instead of
  being ignored, and `=` is allowed in rendered IDs.
- Validation-matrix findings, all fixed and locked in by
  `scripts/validate_repos.py` (click, jinja2, attrs, httpx, starlette,
  urllib3 — all exact): quote-aware `addopts` splitting with `-m`/`-k`
  honored as defaults; `@pytest.fixture(name=...)` registration names;
  anyio plugin backend parametrization detection (mark + well-known fixture
  names); collection of test classes/functions imported into test modules.
- **Per-project warm daemon** (`cito daemon start|stop|status`,
  `cito run --daemon`, unix): workers keep pytest and conftest imported
  across CLI invocations — a one-shot `cito run` on a warm daemon completes
  in ~0.02 s wall. Module freshness is enforced inside the workers (any
  imported file whose mtime changed is purged before each run), verified by
  editing a test red/green across daemon runs. Version-skewed daemons are
  retired automatically; unreachable daemons fall back to local workers.

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
- Randomized differential fuzzing (bench/fuzz_gen.py): seeded generator
  covering the hard-corner matrix, verified 100/100 seeds against real
  pytest; found and fixed a real shadowing bug (duplicate `def` names now
  keep the last definition, matching Python semantics).
- `-m` mark expressions evaluated at collection time (marks harvested from
  decorators, class chains, and module pytestmark; CI checks deselection
  parity against pytest); `--changed` runs only content-changed files;
  default scheduling is now failed-first, changed-first, recent-first
  (content hashes in `.cito/hashes`); addopts containing -n/--dist emit an
  xdist nesting warning.
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
