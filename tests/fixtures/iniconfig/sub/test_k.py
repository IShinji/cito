import pytest


@pytest.mark.slow
def test_a():
    pass


class TestC:
    def test_b(self):
        pass


class CheckA:
    def test_checked(self):
        pass


class CheckC:
    def test_unchecked(self):
        pass


def test_c():
    pass


@pytest.mark.parametrize(
    "x",
    [
        0xFFFFFFFFFFFFFFFFFFFF,
        -0,
        0o7777777777777777777777777,
        0b11111111111111111111111111111111111111111111111111111111111111111,
        -0x1_0000_0000_0000_0000,
    ],
)
def test_big(x):
    pass
