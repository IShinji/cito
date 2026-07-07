# Benchmarks

Environment: Apple M4 Max (16 cores), macOS, Python 3.14.6, cito v0.1
(release build). All timings `/usr/bin/time -p`, best of repeated runs with
warm filesystem cache. Collection equivalence is checked with
`scripts/diff_collect.py` in the same session as every timing.

## Collection

### home-assistant 2026.7.1 — 81,251 tests

The largest pytest suite in the wild. Clone at the release tag, isolated
venv pinned to `requirements_test.txt` versions; scope = the core test tree
plus the 249 integration directories whose dependencies install cleanly
(the exact scope that passes differential validation with missing=0
extra=0):

```
python -m pytest --collect-only -q <scope>          16.91 s
cito collect --python <venv> <scope>                 0.11 s     (156x)
```

pytest self-reports 7.30 s of collection; the rest is interpreter startup,
plugin loading, and home-assistant's conftest import graph — all of which
cito's static analysis never pays.

### pandas 3.0.3 — 197,077 tests

Installed wheel (`uv pip install pandas hypothesis "pytest==8.4.2"`), run
from `site-packages` against `pandas/tests`:

```
python -m pytest --collect-only -q pandas/tests     9.48 s
cito collect --python <venv> pandas/tests           0.26 s     (36x)
```

cito's time includes probing `pytest.importorskip` dependencies (pyarrow,
matplotlib, numba, ...) against the venv. Equivalence: 197,077 pytest IDs;
26 missing (0.013%) and 2 wrong extras — the residue of runtime-computed
parametrization; ~8,500 IDs matched via cito's declared bare-name fallback.

### pytest 9.1.1's own test suite — 4,231 tests

Clone of `pytest-dev/pytest` at tag `9.1.1`, deps installed for collection
(`xmlschema pygments attrs mock setuptools hypothesis`):

```
python -m pytest --collect-only -q                  0.62 s
cito collect --python <venv>                        <0.01 s    (>100x)
```

Equivalence: 1 missing out of 4,231 (a `.txt` doctest — doctest collection
is a documented gap), 0 extras. This suite exercises pytest's own config
(`[tool.pytest]` in pyproject, path-pattern `python_files`, prefix
`python_classes`), decorator aliases, parametrized/autouse fixtures, and
`pytest_generate_tests` — the most hostile static-analysis target available.

### Validation matrix (release gate)

`scripts/validate_repos.py` diff-checks a matrix of real repositories at the
tags matching the installed wheels. As of v0.2, 19 of 21 matrix repos
are exact — including pillow 5,218, aiohttp 4,364, typer 1,379, trio 895 —
with sympy (16 extras) and hypothesis (3) documented-partial.
Fifteen earlier repos are exact
(zero missing, zero extras): click 1,686 IDs, jinja2 909, attrs 1,386,
httpx 1,418, starlette 981, urllib3 2,273, werkzeug 969, requests 633,
more-itertools 722, packaging 61,513, pluggy 124, tornado 1,322, black 446,
pydantic 12,775, fastapi 3,317. sympy (13,657) is documented-partial: 0
missing, 39 extras (0.3%) from its custom @SKIP import-time machinery and
environment-conditional test definitions. sqlalchemy and django are out of
scope (their suites need project-specific collection-bootstrap plugins).

### flask 3.1.3 and rich 15.0.0

Source checkouts at the tags matching the installed wheels, pytest 8.4.2
(flask 3.1.3's test suite uses a private pytest API removed in pytest 9):

```
flask: 482 pytest IDs — cito missing 0, extra 0 (14 declared fallbacks)
rich:  981 pytest IDs — cito missing 0, extra 0 (31 declared fallbacks)
```

### Synthetic corpus — 500 files / 11,000 tests

`bench/gen_corpus.py --files 500 --tests 20`:

```
python -m pytest --collect-only -q    3.17 s cold / 0.70 s warm
cito collect                          0.28 s cold / 0.01 s warm   (70x warm)
```

Equivalence: exact (11,000 = 11,000).

## Execution (preview)

Same corpus, trivial test bodies (worst case for parallelism overhead):

```
python -m pytest -q                   2.48 s
cito run -n 8                         1.24 s
cito run -n 8 --warm                  1.20 s
```

`--warm` keeps pytest workers alive across chunks (one `import pytest` per
worker instead of one per chunk). Its advantage grows with conftest import
cost; on this corpus imports are trivial. The v0.2 daemon design targets the
remaining per-chunk overhead (rootdir/config re-computation, module imports).

## Reproduce

```console
$ bench/bench_collect.sh                                  # corpus
$ git clone --depth 1 --branch 9.1.1 https://github.com/pytest-dev/pytest /tmp/pytest-repo
$ uv venv /tmp/ptv && uv pip install --python /tmp/ptv/bin/python \
    "pytest==9.1.1" xmlschema pygments attrs mock setuptools hypothesis
$ python3 scripts/diff_collect.py /tmp/pytest-repo --python /tmp/ptv/bin/python \
    --ignore-missing test_doctest.txt
$ uv venv /tmp/pdv && uv pip install --python /tmp/pdv/bin/python \
    "pytest==8.4.2" pandas hypothesis
$ SP=$(/tmp/pdv/bin/python -c "import pandas, pathlib; print(pathlib.Path(pandas.__file__).parent.parent)")
$ python3 scripts/diff_collect.py "$SP" pandas/tests --python /tmp/pdv/bin/python
```
