#!/usr/bin/env python3
"""Generate a synthetic pytest corpus for collection benchmarks."""

import argparse
import pathlib

FUNC = '''

def test_{name}():
    assert sum(range(10)) == 45
'''

CLASS = '''

class TestBox{i:04d}:
    def test_method_a(self):
        assert True

    def test_method_b(self):
        assert "cito".startswith("c")
'''


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--files", type=int, default=500)
    parser.add_argument(
        "--tests", type=int, default=20, help="module-level tests per file"
    )
    parser.add_argument("--out", type=pathlib.Path, required=True)
    args = parser.parse_args()

    args.out.mkdir(parents=True, exist_ok=True)
    (args.out / "conftest.py").write_text("# generated corpus\n")
    for i in range(args.files):
        parts = [f'"""Generated module {i}."""']
        for j in range(args.tests):
            parts.append(FUNC.format(name=f"case_{i:04d}_{j:03d}"))
        parts.append(CLASS.format(i=i))
        (args.out / f"test_mod_{i:04d}.py").write_text("".join(parts))
    total = args.files * (args.tests + 2)
    print(f"wrote {args.files} files, {total} tests, at {args.out}")


if __name__ == "__main__":
    main()
