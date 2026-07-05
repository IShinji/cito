#!/usr/bin/env bash
# Collection benchmark: pytest --collect-only vs cito collect on a synthetic corpus.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
corpus="$repo/bench/corpus"
files="${FILES:-500}"
tests="${TESTS:-20}"

cargo build --release --manifest-path "$repo/Cargo.toml"
python3 "$repo/bench/gen_corpus.py" --files "$files" --tests "$tests" --out "$corpus"

pytest_cmd=(python3 -m pytest)
if ! python3 -c "import pytest" 2>/dev/null; then
  pytest_cmd=(uv run --with pytest python -m pytest)
fi

echo "--- pytest --collect-only -q (3 runs) ---"
for _ in 1 2 3; do
  /usr/bin/time -p "${pytest_cmd[@]}" --collect-only -q "$corpus" >/dev/null 2>"$corpus/.t" || true
  grep ^real "$corpus/.t"
done

echo "--- cito collect (3 runs) ---"
for _ in 1 2 3; do
  /usr/bin/time -p "$repo/target/release/cito" collect "$corpus" >/dev/null 2>"$corpus/.t"
  grep ^real "$corpus/.t"
done
