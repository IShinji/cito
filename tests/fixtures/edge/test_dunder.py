import unittest


class TestOff:
    __test__ = False

    def test_off(self):
        pass


class OffBase:
    __test__ = False

    def test_base(self):
        pass


class TestInheritsOff(OffBase):
    def test_inherited_off(self):
        pass


class TestBackOn(OffBase):
    __test__ = True

    def test_on(self):
        pass


class NotNamedLikeATest:
    __test__ = True

    def test_nose_style(self):
        pass


class UTOff(unittest.TestCase):
    __test__ = False

    def test_ut_off(self):
        pass


def test_fn_off():
    pass


test_fn_off.__test__ = False


def test_fn_on():
    pass
