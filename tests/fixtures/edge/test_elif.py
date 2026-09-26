import sys

if sys.platform != "no-such-platform":
    def test_first_arm():
        pass
elif sys.platform != "another-platform":
    def test_dead_elif():
        pass
else:
    def test_dead_else():
        pass

if sys.platform == "no-such-platform":
    def test_dead_if():
        pass
elif sys.platform != "no-such-platform":
    def test_live_elif():
        pass
else:
    def test_dead_else_after_true_elif():
        pass
