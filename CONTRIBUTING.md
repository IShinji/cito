# Contributing

## Setup

Rust stable and a Python ≥3.9 with pytest are all you need:

```bash
cargo build --release
uv venv .venv && uv pip install --python .venv/bin/python "pytest==8.4.2"
```

## The test pyramid

```bash
cargo test                                # unit + fixture-tree integration tests
python3 scripts/diff_collect.py tests/fixtures/basic --python .venv/bin/python
python3 scripts/diff_collect.py tests/fixtures/configured --python .venv/bin/python
python3 scripts/diff_collect.py tests/fixtures/plugins --python .venv/bin/python
```

`scripts/diff_collect.py` is the oracle: it runs real pytest and cito on the
same tree and enforces the node-ID contract from the README. Any collection
change must keep all fixture trees at `OK: node IDs match`.

For fuzzing and the release gate:

```bash
python3 bench/fuzz_gen.py --seed 7 --out /tmp/fuzz && \
    python3 scripts/diff_collect.py /tmp/fuzz --python .venv/bin/python
python3 scripts/validate_repos.py --python <venv>/bin/python --cache /tmp/repos
```

`validate_repos.py` clones ~35 real repositories at wheel-matching tags and
diff-checks each one; it is run before every release.

## Before sending a PR

```bash
cargo fmt && cargo clippy --release --all-targets && cargo test
```

CI blocks on all three platforms (Linux, macOS, Windows) plus the pytest
parity job. If you change collection behavior, say which fixture tree or
validated repository covers it — and if none does, add a fixture.

## Reporting a collection mismatch

The most valuable bug report for a compatibility tool is a mismatch. The
issue template carries the recipe; the short version:

```bash
python -m pytest --collect-only -q | sort > pytest.txt
cito collect --python $(which python) | sort > cito.txt
diff pytest.txt cito.txt | head
```
