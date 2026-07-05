import unittest

from support import MixinBase, SecondLevel


class TestInherited(SecondLevel):
    def test_own(self):
        assert True


class TestOverride(MixinBase):
    def test_from_mixin(self):  # overrides the mixin; must appear once
        assert True


class LegacySuite(unittest.TestCase):
    """Collected despite not matching Test*, because it's a TestCase."""

    def test_unittest_style(self):
        assert True

    def not_a_test(self):
        return None
