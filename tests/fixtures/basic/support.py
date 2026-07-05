# Deliberately NOT named test_*.py: these bases are only reachable through
# cito's lazy cross-module resolution.


class MixinBase:
    def test_from_mixin(self):
        assert True

    def helper(self):
        return 1


class SecondLevel(MixinBase):
    def test_second_level(self):
        assert True
