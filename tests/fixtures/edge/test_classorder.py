import unittest


class TestOuter:
    def test_1(self):
        pass

    class TestInner:
        def test_in(self):
            pass

    def test_2(self):
        pass


class Base:
    def test_base(self):
        pass

    def test_overridden(self):
        pass


class TestChild(Base):
    def test_child(self):
        pass

    def test_overridden(self):
        pass


class A:
    def test_a1(self):
        pass

    def test_m(self):
        pass


class B(A):
    pass


class C(A):
    def test_c(self):
        pass

    def test_m(self):
        pass


class TestDiamond(B, C):
    pass


class WithNested:
    class TestInherited:
        def test_i(self):
            pass


class TestHasInherited(WithNested):
    def test_h(self):
        pass


class TestAttrs(Base):
    test_base = None
    test_data = dict(a=1)
    test_list = [1, 2]
    test_lambda = lambda self: None

    def test_d(self):
        pass


class UT(unittest.TestCase):
    def test_z(self):
        pass

    def test_a(self):
        pass

    test_none = None

    class TestNested:
        def test_n(self):
            pass

    def test_M(self):
        pass
