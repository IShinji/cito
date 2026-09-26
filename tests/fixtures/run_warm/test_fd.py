import os
import sys


def test_writes_to_real_stdout():
    # Bypasses sys.stdout: lands on file descriptor 1 directly.
    os.system("echo hello-from-subprocess")
    os.write(1, b"raw-fd-write\n")


def test_reads_stdin():
    # Must not swallow the worker's request stream.
    assert sys.stdin.read() == ""


def test_after():
    pass
