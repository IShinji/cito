import pytest


@pytest.mark.slow
def test_marked_slow():
    assert True


@pytest.mark.network
class TestMarkedClass:
    def test_net_call(self):
        assert True
