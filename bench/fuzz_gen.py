#!/usr/bin/env python3
"""Seeded random pytest-project generator for differential fuzzing.

Generates a project that exercises the collector's hard corners — nested
classes, cross-module inheritance, re-exports, parametrize variants,
fixtures (parametrized/autouse), marks, pytestmark, aliases, shadowing,
conftest collect_ignore — while staying valid for real pytest to collect.
Pair with scripts/diff_collect.py to compare answers.
"""

import argparse
import pathlib
import random

SAFE_STRS = ["red", "blue", "a-b", "x.y", "v_1"]
MARKS = ["slow", "network", "smoke"]


class Gen:
    def __init__(self, seed: int):
        self.rng = random.Random(seed)
        self.fixture_pool = ["tmp_path"]  # always requestable

    def scalar(self) -> str:
        r = self.rng
        return r.choice(
            [
                str(r.randint(-5, 99)),
                repr(r.choice(SAFE_STRS)),
                r.choice(["True", "False", "None"]),
                str(r.uniform(0, 9)),  # float: cito must fall back
            ]
        )

    def parametrize(self, argnames: list) -> str:
        r = self.rng
        n = len(argnames)
        values = []
        for _ in range(r.randint(1, 4)):
            if n == 1:
                inner = self.scalar()
            else:
                inner = "(" + ", ".join(self.scalar() for _ in range(n)) + ")"
            if n == 1 and r.random() < 0.25:
                kw = ""
                if r.random() < 0.5:
                    kw = f", id={self.rng.choice(SAFE_STRS)!r}"
                elif r.random() < 0.5:
                    kw = f", marks=pytest.mark.{r.choice(MARKS)}"
                inner = f"pytest.param({inner}{kw})"
            values.append(inner)
        if r.random() < 0.1:
            if n == 1:
                values.append("make_value()")  # computed: fallback
            else:
                values.append("(" + ", ".join(["make_value()"] * n) + ")")
        names = ",".join(argnames)
        return f"@pytest.mark.parametrize({names!r}, [{', '.join(values)}])"

    def decorators(self, argnames_out: list, in_unittest: bool) -> list:
        r = self.rng
        lines = []
        if not in_unittest:
            stacks = r.choices([0, 1, 2], weights=[5, 4, 1])[0]
            for i in range(stacks):
                names = [f"p{i}_{j}" for j in range(r.randint(1, 2))]
                argnames_out.extend(names)
                lines.append(self.parametrize(names))
        if r.random() < 0.3:
            lines.append(f"@pytest.mark.{r.choice(MARKS)}")
        if r.random() < 0.1 and self.fixture_pool:
            fx = r.choice(self.fixture_pool)
            lines.append(f"@pytest.mark.usefixtures({fx!r})")
        if r.random() < 0.1:
            lines.append('@mock.patch("os.getcwd")')
            argnames_out.insert(0, "_fake")
        return lines

    def function(self, indent: str, in_unittest: bool = False, extra_args=()) -> list:
        r = self.rng
        name = r.choice(
            [
                f"test_fn_{r.randint(0, 6)}",  # duplicates trigger shadowing
                f"testplain{r.randint(0, 9)}",
                f"helper_{r.randint(0, 9)}",
            ]
        )
        args = list(extra_args)
        lines = self.decorators(args, in_unittest)
        if r.random() < 0.3 and self.fixture_pool and not in_unittest:
            args.append(r.choice(self.fixture_pool))
        self_arg = ["self"] if indent else []
        is_async = "" if in_unittest or r.random() > 0.15 else "async "
        signature = ", ".join(self_arg + args)
        out = [indent + line for line in lines]
        out.append(f"{indent}{is_async}def {name}({signature}):")
        out.append(f"{indent}    assert True")
        out.append("")
        return out

    def fixture(self, indent: str = "") -> list:
        r = self.rng
        name = f"fx_{r.randint(0, 5)}"
        kwargs = []
        if r.random() < 0.4:
            kwargs.append(f"params=[{self.scalar()}, {self.scalar()}]")
        if r.random() < 0.2:
            kwargs.append("autouse=True")
        deco = f"@pytest.fixture({', '.join(kwargs)})" if kwargs else "@pytest.fixture"
        self.fixture_pool.append(name)
        self_arg = "self, " if indent else ""
        return [
            f"{indent}{deco}",
            f"{indent}def {name}({self_arg}request):",
            f"{indent}    return getattr(request, 'param', None)",
            "",
        ]

    def klass(self, bases: list) -> list:
        r = self.rng
        name = r.choice([f"TestBox{r.randint(0, 9)}", f"Plain{r.randint(0, 9)}"])
        base = ""
        in_unittest = False
        if bases and r.random() < 0.5:
            base = f"({r.choice(bases)})"
        elif r.random() < 0.15:
            base = "(unittest.TestCase)"
            in_unittest = True
        args: list = []
        lines = [] if in_unittest else [
            line for line in self.decorators(args, False) if "usefixtures" in line
        ]
        # Class-level parametrize forces every method (incl. inherited) to
        # accept the argname, so only use it on base-less classes.
        class_args = ()
        if not in_unittest and not base and r.random() < 0.25:
            lines.append(self.parametrize(["cp"]))
            class_args = ("cp",)
        lines.append(f"class {name}{base}:")
        body: list = []
        if r.random() < 0.1 and not in_unittest:
            body += ["    def __init__(self):", "        self.x = 1", ""]
        if r.random() < 0.2 and not in_unittest:
            body += self.fixture("    ")
        for _ in range(r.randint(1, 3)):
            body += self.function("    ", in_unittest, class_args)
        if r.random() < 0.2 and not in_unittest and not class_args:
            inner = [f"    class TestInner{r.randint(0, 3)}:"]
            inner += ["    " + l for l in self.function("    ")]
            body += inner + [""]
        lines += body or ["    pass", ""]
        lines.append("")
        return lines

    def test_file(self, bases: list) -> str:
        r = self.rng
        lines = ["import unittest", "from unittest import mock", "", "import pytest"]
        if bases:
            lines.append("from support import SupportBase")
            lines.append("from helpers import ExportedBase")
        lines.append("")
        lines.append("def make_value():")
        lines.append("    return 42")
        lines.append("")
        if r.random() < 0.2:
            lines.append(f"pytestmark = pytest.mark.{r.choice(MARKS)}")
            lines.append("")
        if r.random() < 0.2:
            lines.append(
                f"shared_params = pytest.mark.parametrize('sp', [{self.scalar()}, {self.scalar()}])"
            )
            lines.append("")
        for _ in range(r.randint(0, 2)):
            lines += self.fixture()
        has_alias = any("shared_params =" in l for l in lines)
        for _ in range(r.randint(1, 4)):
            if has_alias and r.random() < 0.3:
                lines.append("@shared_params")
                lines.append(f"def test_alias_{r.randint(0, 9)}(sp):")
                lines.append("    assert sp is not None or sp is None")
                lines.append("")
            else:
                lines += self.function("")
        for _ in range(r.randint(0, 2)):
            lines += self.klass(bases)
        return "\n".join(lines) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seed", type=int, required=True)
    parser.add_argument("--files", type=int, default=6)
    parser.add_argument("--out", type=pathlib.Path, required=True)
    args = parser.parse_args()

    gen = Gen(args.seed)
    out = args.out
    out.mkdir(parents=True, exist_ok=True)

    (out / "pytest.ini").write_text(
        "[pytest]\nmarkers =\n    slow: s\n    network: n\n    smoke: k\n"
    )

    (out / "support.py").write_text(
        "import pytest\n\n\nclass SupportBase:\n"
        "    def test_from_support(self):\n        assert True\n\n"
        "    def helper(self):\n        return 1\n"
    )
    pkg = out / "helpers"
    pkg.mkdir(exist_ok=True)
    (pkg / "impl.py").write_text(
        "class ExportedBase:\n    def test_from_export(self):\n        assert True\n"
    )
    (pkg / "__init__.py").write_text("from .impl import ExportedBase\n")

    conftest = ["import pytest", ""]
    conftest += gen.fixture()
    if gen.rng.random() < 0.3:
        conftest += gen.fixture()
    (out / "conftest.py").write_text("\n".join(conftest) + "\n")

    bases = ["SupportBase", "ExportedBase"]
    for i in range(args.files):
        (out / f"test_fuzz_{i:02d}.py").write_text(gen.test_file(bases))
    # One file the conftest ignores, sometimes.
    if gen.rng.random() < 0.5:
        (out / "test_ignored_fuzz.py").write_text("def test_hidden():\n    assert True\n")
        with (out / "conftest.py").open("a") as f:
            f.write('\ncollect_ignore = ["test_ignored_fuzz.py"]\n')
    print(f"seed={args.seed} files={args.files} at {out}")


if __name__ == "__main__":
    main()
