def test_addition():
    assert 1 + 1 == 2


async def test_async_thing():
    assert True


def helper():
    return 42


def testnounderscore():
    assert True


class TestWidget:
    def test_render(self):
        assert True

    def helper_method(self):
        return None

    class TestNested:
        def test_inner(self):
            assert True


class TestWithInit:
    def __init__(self):
        self.x = 1

    def test_skipped(self):
        assert True


class NotCollected:
    def test_ignored(self):
        assert True
