"""Decorators: wrapping with functools.wraps, parameterized, and stacking."""

import functools


def logged(func):
    @functools.wraps(func)
    def wrapper(*args, **kwargs):
        result = func(*args, **kwargs)
        arglist = ", ".join(str(a) for a in args)
        print(f"  {func.__name__}({arglist}) = {result}")
        return result

    return wrapper


def repeat(times):
    def decorator(func):
        @functools.wraps(func)
        def wrapper(*args, **kwargs):
            return [func(*args, **kwargs) for _ in range(times)]

        return wrapper

    return decorator


def bold(func):
    return lambda *a, **k: f"<b>{func(*a, **k)}</b>"


def italic(func):
    return lambda *a, **k: f"<i>{func(*a, **k)}</i>"


@logged
def add(a, b):
    return a + b


@repeat(3)
def roll():
    return 4  # chosen by fair dice roll


@bold
@italic
def render(text):
    return text


add(2, 3)
add(10, 20)
print("name preserved:", add.__name__)
print("repeated:", roll())
print("stacked:", render("hello"))
