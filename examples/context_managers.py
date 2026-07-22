"""Context managers: the `with` protocol, contextlib helpers, and StringIO."""

import io
from contextlib import contextmanager, redirect_stdout, suppress


class Timer:
    """A context manager written the classic way, with __enter__/__exit__."""

    def __init__(self, label):
        self.label = label

    def __enter__(self):
        print(f"[{self.label}] start")
        return self

    def __exit__(self, exc_type, exc_val, exc_tb):
        print(f"[{self.label}] end (error: {exc_type is not None})")
        return False


@contextmanager
def tag(name):
    print(f"<{name}>")
    yield name
    print(f"</{name}>")


with Timer("work"):
    print("  doing work")

with tag("div") as t:
    print(f"  content in {t}")

# suppress swallows a chosen exception.
with suppress(ZeroDivisionError):
    x = 1 / 0
    print("unreachable")
print("survived the division")

# redirect_stdout captures printed output.
buffer = io.StringIO()
with redirect_stdout(buffer):
    print("captured line 1")
    print("captured line 2")
print("captured:", repr(buffer.getvalue()))

# __exit__ still runs when the body raises.
try:
    with Timer("failing"):
        raise RuntimeError("expected")
except RuntimeError as e:
    print("propagated:", e)
