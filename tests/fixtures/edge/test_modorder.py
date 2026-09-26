import contextlib


def test_a():
    pass


from edge_helpers import test_imported, test_disabled_helper


def test_b():
    pass


def test_a():  # redefinition keeps test_a's first slot
    pass


def test_c():
    pass


del test_c


with contextlib.suppress(Exception):
    def test_in_with():
        pass


def test_c():  # rebinding after del: collected, at the end
    pass
