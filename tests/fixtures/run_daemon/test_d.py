import os

from helper import f


def test_f():
    assert f() == 1


def test_env():
    assert os.environ.get("CITO_FIXTURE_FLAG") == "on"
