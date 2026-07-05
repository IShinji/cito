import pytest

# With a probe python (`cito collect --python ...`), this module is dropped
# exactly like pytest drops it; in static mode cito still collects it.
pytest.importorskip("cito_nonexistent_dependency_xyz")


def test_needs_missing_dep():
    assert True
