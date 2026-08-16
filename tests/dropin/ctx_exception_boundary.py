"""Exceptions crossing between user code and the standard library.

`@contextlib.contextmanager` drives this module's generators through the CPython
generator protocol (`next`/`throw`/`close`), and a stdlib mapping's `KeyError`
comes back the other way. Both directions must keep the exception's class, its
`args`, and any attributes outside `args` — printing `e.args` rather than just
the message is the point: the message is the part that agrees even when the
exception has been rebuilt wrongly.
"""
import contextlib
import json
import os


@contextlib.contextmanager
def cleanup(tag):
    print('enter', tag)
    try:
        yield tag
    finally:
        print('exit', tag)


@contextlib.contextmanager
def handles():
    try:
        yield
    except KeyError as e:
        print('handled', e.args, repr(str(e)))


@contextlib.contextmanager
def translates():
    try:
        yield
    except ValueError:
        raise KeyError('replaced')


with cleanup('plain') as tag:
    print('body', tag)

with handles():
    raise KeyError("a'b")
print('after handled')

try:
    with cleanup('raising'):
        raise ValueError('through')
except ValueError as e:
    print('propagated', e.args)

try:
    with translates():
        raise ValueError('original')
except KeyError as e:
    print('translated', e.args, repr(str(e)))

with contextlib.ExitStack() as stack:
    first = stack.enter_context(cleanup('a'))
    second = stack.enter_context(cleanup('b'))
    print('stacked', first, second)

try:
    os.environ['PYTHONRS_DEFINITELY_NOT_SET_XYZ']
except KeyError as e:
    print('env', e.args, repr(str(e)))

try:
    json.loads('{"a": }')
except ValueError as e:
    print('json', type(e).__name__, e.lineno, e.colno, e.pos, e.msg)
