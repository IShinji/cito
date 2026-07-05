import pytest


@pytest.mark.parametrize("x", [1, 2, 3])
def test_ints(x):
    assert x > 0


@pytest.mark.parametrize("s", ["red", "blue"])
def test_strs(s):
    assert s


@pytest.mark.parametrize("v", [None, True, False])
def test_specials(v):
    assert v is None or isinstance(v, bool)


@pytest.mark.parametrize("x,y", [(1, "a"), (2, "b")])
def test_pairs(x, y):
    assert x and y


@pytest.mark.parametrize("a", [1, 2])
@pytest.mark.parametrize("b", ["x", "y"])
def test_stacked(a, b):
    assert a and b


@pytest.mark.parametrize("f", [1.5, 2.5])
def test_floats(f):  # floats fall back to the bare name in cito
    assert f


@pytest.mark.parametrize("x", [1, 2], ids=["one", "two"])
def test_ids(x):
    assert x


@pytest.mark.parametrize("d", [{"k": 1}, {"k": 2}])
def test_complex(d):  # non-literals fall back to the bare name in cito
    assert d


class TestClsParams:
    @pytest.mark.parametrize("n", [1, 2])
    def test_m(self, n):
        assert n


@pytest.mark.parametrize("c", ["p", "q"])
class TestClassLevel:
    def test_via_class(self, c):
        assert c

    @pytest.mark.parametrize("n", [1, 2])
    def test_combined(self, n, c):
        assert n and c


@pytest.mark.parametrize(
    "p",
    [
        pytest.param(1),
        pytest.param(2, id="two"),
        pytest.param(3, marks=pytest.mark.skip),
    ],
)
def test_param_objects(p):
    assert p


@pytest.mark.parametrize("ext", [pytest.param(".xlsx"), pytest.param(".ods")])
class TestClassParamObjects:
    def test_with_ext(self, ext):
        assert ext
