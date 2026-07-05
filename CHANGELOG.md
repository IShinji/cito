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
- Differential parity, checked in CI: pytest 9.1.1's own suite 4,231 IDs →
  1 known gap (doctest), 0 extras; pandas 3.0.3 197,077 IDs → 0.34% missing.
- Collection speed: pandas 9.48 s → 0.26 s (36x); pytest's repo 0.62 s →
  <0.01 s; 11k-test corpus 0.70 s → 0.01 s.
