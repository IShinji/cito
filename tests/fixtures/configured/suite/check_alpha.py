def check_one():
    assert True


def spec_two():
    assert True


def test_regular():  # python_functions is overridden; NOT collected
    assert True


class SuiteAlpha:
    def check_method(self):
        assert True


class TestBeta:  # python_classes is overridden; NOT collected
    def check_ignored(self):
        assert True
