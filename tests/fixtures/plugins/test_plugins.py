"""Collection parity in the presence of common plugins/decorators:
pytest-asyncio (auto mode), hypothesis @given, unittest.mock.patch."""

from unittest import mock

import pytest
from hypothesis import given, strategies as st


async def test_asyncio_auto():
    assert True


@given(st.integers())
def test_hypothesis_given(x):
    assert isinstance(x, int)


@mock.patch("os.getcwd")
def test_mock_patch(fake_getcwd):
    import os

    fake_getcwd.return_value = "/x"
    assert os.getcwd() == "/x"


@pytest.mark.parametrize("v", [1, 2])
@mock.patch("os.getcwd")
def test_mock_plus_params(fake_getcwd, v):
    assert v in (1, 2)
