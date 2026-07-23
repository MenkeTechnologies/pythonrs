"""Self-contained pythonrs replacement for CPython's C ``_weakref`` module.

pythonrs has no garbage collector -- the object heap only grows for the life
of the process -- so a referent can never be reclaimed.  A "weak" reference is
therefore a *strong* reference that always resolves: ``ref(obj)()`` returns
``obj`` and never ``None``, and the death callback is never invoked.

This is functionally correct for every stdlib consumer that uses weakrefs as a
registry rather than to observe collection -- ``abc.ABCMeta`` (whose
``_abc_registry`` / ``_abc_cache`` / ``_abc_negative_cache`` are ``WeakSet``s),
``_weakrefset.WeakSet``, ``weakref.WeakValueDictionary`` used as a cache, etc.
The only observable difference from CPython is that referents outlive their last
weak referrer -- which, on a heap that never collects, is already true of every
object regardless of weakrefs.

Written in pure Python and executed on pythonrs's own interpreter: no libpython,
no C extension.  This is the endgame (``--no-default-features``) path.
"""


class ReferenceType:
    """A weak reference (strong, non-expiring, under pythonrs's no-GC heap).

    Two references compare and hash by their referent while it is alive (always,
    here), so ``ref(x) == ref(x)`` and ``ref(x) in {ref(x)}`` -- the invariant
    ``_weakrefset.WeakSet`` relies on to store and look members up by ``ref``.
    """

    __slots__ = ("_referent", "_callback", "_hash")

    def __init__(self, obj, callback=None):
        self._referent = obj
        self._callback = callback
        # CPython computes and caches the referent's hash lazily on first
        # ``hash(wr)`` and requires the referent be hashable then; mirror that
        # by deferring until ``__hash__`` so an unhashable referent only errors
        # if the reference is actually hashed.
        self._hash = None

    def __call__(self):
        return self._referent

    def __hash__(self):
        if self._hash is None:
            self._hash = hash(self._referent)
        return self._hash

    def __eq__(self, other):
        if self is other:
            return True
        if not isinstance(other, ReferenceType):
            return NotImplemented
        # Both referents are alive (nothing is ever collected), so compare by
        # referent value, matching CPython's live-vs-live weakref equality.
        mine = self._referent
        theirs = other._referent
        return mine is theirs or mine == theirs

    def __ne__(self, other):
        result = self.__eq__(other)
        if result is NotImplemented:
            return result
        return not result

    def __repr__(self):
        return "<weakref at 0x0; to %r>" % (type(self._referent).__name__,)


# CPython exposes the constructor as ``ref``; ``ReferenceType`` is the same type.
ref = ReferenceType


class ProxyType:
    """A non-callable weak proxy -- a strong passthrough here."""

    __slots__ = ("_referent",)

    def __init__(self, obj):
        object.__setattr__(self, "_referent", obj)

    def __getattr__(self, name):
        return getattr(object.__getattribute__(self, "_referent"), name)

    def __setattr__(self, name, value):
        setattr(object.__getattribute__(self, "_referent"), name, value)

    def __delattr__(self, name):
        delattr(object.__getattribute__(self, "_referent"), name)


class CallableProxyType(ProxyType):
    """A callable weak proxy -- forwards ``__call__`` to the referent."""

    __slots__ = ()

    def __call__(self, *args, **kwargs):
        return object.__getattribute__(self, "_referent")(*args, **kwargs)


def proxy(obj, callback=None):
    if callable(obj):
        return CallableProxyType(obj)
    return ProxyType(obj)


def getweakrefcount(obj):
    # No live-weakref bookkeeping under a non-collecting heap.
    return 0


def getweakrefs(obj):
    return []


def _remove_dead_weakref(dct, key):
    # Nothing ever dies; used by the C ``WeakValueDictionary`` fast path to drop
    # a key whose value was collected. Mirror the mapping-pop signature as a
    # best-effort delete so any caller that reaches it stays correct.
    try:
        del dct[key]
    except KeyError:
        pass
