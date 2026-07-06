def test_shadowed():
    assert False  # replaced below


def test_shadowed():
    assert True


class TestShadow:
    def test_dup(self):
        assert False  # replaced below

    def test_dup(self):
        assert True
