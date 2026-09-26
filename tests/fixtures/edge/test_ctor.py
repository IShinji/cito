class InitBase:
    def __init__(self):
        pass


class Middle(InitBase):
    pass


class TestInheritsInit(Middle):
    def test_x(self):
        pass


class NewBase:
    def __new__(cls):
        return super().__new__(cls)


class TestInheritsNew(NewBase):
    def test_y(self):
        pass


class TestNoCtor:
    def test_z(self):
        pass
