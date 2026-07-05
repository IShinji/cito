def test_hidden():
    # This file matches neither test_*.py nor *_test.py, so pytest must not
    # collect it — and neither must cito.
    assert True
