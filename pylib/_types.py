"""Native type-object accelerator for pythonrs's self-contained (no-libpython)
build.

CPython's ``types`` module derives its type objects from deep introspection
primitives -- ``func.__code__``, ``func.__closure__``, ``exc.__traceback__``,
``list[int]``, ``int | str`` -- that pythonrs does not yet expose; running that
derivation here would raise.  ``types.py`` therefore tries ``from _types import
*`` first and, on success, skips the fragile fallback entirely.

This module hands the same names back using ONLY the primitives pythonrs
supports today, plus a pure-Python ``GenericAlias``.  A name that cannot yet be
produced faithfully (``CodeType``, ``CellType``, ``TracebackType``,
``UnionType``, …) is deliberately OMITTED -- accessing ``types.CodeType`` then
raises ``AttributeError`` rather than returning a fake object.  The set grows as
pythonrs's introspection model does; nothing here is a stand-in pretending to be
the real type.
"""

import sys

# ── real type objects pythonrs can introspect directly ──────────────────────


def _f():
    pass


FunctionType = type(_f)
LambdaType = FunctionType  # a lambda is the same type as a def


def _g():
    yield 1


GeneratorType = type(_g())


class _C:
    def _m(self):
        pass


MethodType = type(_C()._m)

BuiltinFunctionType = type(len)
BuiltinMethodType = type([].append)  # same underlying type as a builtin function

ModuleType = type(sys)

NoneType = type(None)
EllipsisType = type(Ellipsis)
NotImplementedType = type(NotImplemented)


# ── SimpleNamespace: a mutable attribute bag (pure-Python, no C type) ────────


class SimpleNamespace:
    """An `object`-like mutable attribute holder, per `types.SimpleNamespace`."""

    def __init__(self, /, **kwargs):
        self.__dict__.update(kwargs)

    def __repr__(self):
        items = ", ".join("%s=%r" % (k, v) for k, v in sorted(self.__dict__.items()))
        return "namespace(%s)" % items

    def __eq__(self, other):
        if isinstance(other, SimpleNamespace):
            return self.__dict__ == other.__dict__
        return NotImplemented


# ── GenericAlias: `list[int]` / `WeakSet[T]` parameterized generics ──────────


class GenericAlias:
    """A parameterized generic alias.

    pythonrs does not make types subscriptable, so stdlib classes reach these
    via ``__class_getitem__ = classmethod(GenericAlias)``.  The surface the
    stdlib relies on: callable (delegates construction to the origin), carries
    ``__origin__``/``__args__``, forwards attribute access to the origin, and
    substitutes the origin when used as a base (``__mro_entries__``).
    """

    def __init__(self, origin, args):
        self.__origin__ = origin
        self.__args__ = args if isinstance(args, tuple) else (args,)

    def __call__(self, *args, **kwargs):
        return self.__origin__(*args, **kwargs)

    def __mro_entries__(self, bases):
        return (self.__origin__,)

    def __getattr__(self, name):
        return getattr(self.__origin__, name)

    def __eq__(self, other):
        if not isinstance(other, GenericAlias):
            return NotImplemented
        return self.__origin__ == other.__origin__ and self.__args__ == other.__args__

    def __repr__(self):
        def _name(a):
            return getattr(a, "__name__", None) or repr(a)

        return "%s[%s]" % (_name(self.__origin__), ", ".join(_name(a) for a in self.__args__))
