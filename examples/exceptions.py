"""Exceptions: custom hierarchies, chaining, finally, and try/except/else."""


class ValidationError(Exception):
    def __init__(self, field, message):
        super().__init__(f"{field}: {message}")
        self.field = field


def validate_age(age):
    if not isinstance(age, int):
        raise ValidationError("age", "must be an integer")
    if age < 0:
        raise ValidationError("age", "must be non-negative")
    if age > 150:
        raise ValidationError("age", "implausibly large")
    return age


for value in [25, -3, 200, "old"]:
    try:
        result = validate_age(value)
    except ValidationError as e:
        print(f"invalid {value!r}: {e.field}")
    else:
        print(f"valid: {result}")

# Exception chaining with `raise ... from`.
def parse_config(raw):
    try:
        return int(raw)
    except ValueError as e:
        raise RuntimeError("config must be numeric") from e


try:
    parse_config("not-a-number")
except RuntimeError as e:
    print("chained cause:", type(e.__cause__).__name__)

# finally always runs.
def with_cleanup(fail):
    try:
        if fail:
            raise ValueError("boom")
        return "ok"
    except ValueError:
        return "recovered"
    finally:
        print("  cleanup ran")


print("result:", with_cleanup(False))
print("result:", with_cleanup(True))

# Catching a group of exception types.
for op in [lambda: 1 / 0, lambda: [][5], lambda: {}["k"]]:
    try:
        op()
    except (ZeroDivisionError, IndexError, KeyError) as e:
        print("caught:", type(e).__name__)
