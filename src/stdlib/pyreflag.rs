//! `re.RegexFlag`, as Python source run on pythonrs.
//!
//! CPython builds `RegexFlag` in `Lib/re/__init__.py` with the `enum` machinery:
//!
//! ```python
//! @enum.global_enum
//! @enum._simple_enum(enum.IntFlag, boundary=enum.KEEP)
//! class RegexFlag:
//!     NOFLAG = 0
//!     ASCII = A = ...
//!     ...
//!     __str__ = object.__str__
//!     _numeric_repr_ = hex
//! ```
//!
//! so `re.I` IS `re.RegexFlag.IGNORECASE`, an `int` whose repr is
//! `re.IGNORECASE`, and `re.I | re.M` is the composite `re.IGNORECASE|re.MULTILINE`.
//! pythonrs's `enum` is CPython's class across the FFI bridge; building
//! `RegexFlag` on it would start libpython on every `import re` and hand the
//! native engine a bridged object for every `flags` argument. So the parts of
//! `enum.Flag`/`enum.IntFlag` that `RegexFlag` uses are ported here, natively:
//! the member table, `Flag._missing_` under the `KEEP` boundary (composites and
//! unknown bits), the bitwise operators, `__contains__`/`__iter__`/`__len__`,
//! `enum.global_flag_repr`, and the class-level protocol of `EnumType`
//! (`RegexFlag(2)`, `RegexFlag['I']`, `list(RegexFlag)`, `__members__`).
//!
//! What is NOT reproduced: `RegexFlag` derives from `int` alone, so its MRO has
//! no `enum.IntFlag`/`Flag`/`Enum` and `isinstance(re.I, enum.IntFlag)` is
//! False, and its metaclass is this module's `_FlagType` rather than
//! `enum.EnumType`.

/// The Python source of the `re._regexflag` helper module, whose `RegexFlag`
/// class the native `re` module exports along with its members.
pub fn module_source() -> &'static str {
    r#""""`re.RegexFlag` for pythonrs's native `re` (see `src/stdlib/pyreflag.rs`)."""


def _iter_bits_lsb(num):
    # `enum._iter_bits_lsb`: each set bit of `num`, lowest first.
    while num:
        b = num & (~num + 1)
        yield b
        num ^= b


def _is_single_bit(num):
    # `enum._is_single_bit`
    if num == 0:
        return False
    num &= num - 1
    return num == 0


class _FlagType(type):
    """The class-level protocol `enum.EnumType` gives a flag class."""

    def __call__(cls, value):
        # `EnumType.__call__` -> `Enum.__new__`: a member by value, else
        # `Flag._missing_` builds (and caches) the composite.
        if type(value) is cls:
            return value
        try:
            return cls._value2member_map_[value]
        except (KeyError, TypeError):
            pass
        return cls._missing_(value)

    def __getitem__(cls, name):
        return cls._member_map_[name]

    def __iter__(cls):
        return (cls._member_map_[name] for name in cls._member_names_)

    def __reversed__(cls):
        return (cls._member_map_[name] for name in reversed(cls._member_names_))

    def __len__(cls):
        return len(cls._member_names_)

    def __contains__(cls, value):
        if isinstance(value, cls):
            return True
        return value in cls._value2member_map_

    def __repr__(cls):
        return "<flag %r>" % cls.__name__

    def __dir__(cls):
        # `EnumType.__dir__` for a class whose members are `int`s made by
        # `int.__new__`: the mixed-in type's names plus the enum protocol.
        interesting = set([
                '__class__', '__contains__', '__doc__', '__getitem__',
                '__iter__', '__len__', '__members__', '__module__',
                '__name__', '__qualname__', '__new__',
                ]
                + cls._member_names_
                )
        return sorted(set(dir(int)) | interesting)

    @property
    def __members__(cls):
        return dict(cls._member_map_)


class RegexFlag(int, metaclass=_FlagType):
    __module__ = 're'
    __qualname__ = 'RegexFlag'
    # `_simple_enum` gives a class without a docstring this one.
    __doc__ = 'An enumeration.'

    # `_simple_enum(..., boundary=KEEP)` and `_numeric_repr_ = hex`.
    _numeric_repr_ = hex

    @classmethod
    def _missing_(cls, value):
        # `enum.Flag._missing_` under the `KEEP` boundary.
        if not isinstance(value, int):
            raise ValueError("%r is not a valid %s" % (value, cls.__qualname__))
        flag_mask = cls._flag_mask_
        singles_mask = cls._singles_mask_
        all_bits = cls._all_bits_
        neg_value = None
        if (
                not ~all_bits <= value <= all_bits
                or value & (all_bits ^ flag_mask)
            ):
            if value < 0:
                value = max(all_bits + 1, 2 ** (value.bit_length())) + value
        if value < 0:
            neg_value = value
            value = all_bits + 1 + value
        aliases = value & ~singles_mask
        member_value = value & singles_mask
        pseudo_member = int.__new__(cls, value)
        pseudo_member._value_ = value
        if member_value or aliases:
            members = []
            combined_value = 0
            for m in cls._iter_member_(member_value):
                members.append(m)
                combined_value |= m._value_
            if aliases:
                value = member_value | aliases
                for n, pm in cls._member_map_.items():
                    if pm not in members and pm._value_ and pm._value_ & value == pm._value_:
                        members.append(pm)
                        combined_value |= pm._value_
            unknown = value ^ combined_value
            pseudo_member._name_ = '|'.join([m._name_ for m in members])
            if not combined_value:
                pseudo_member._name_ = None
            elif unknown:
                pseudo_member._name_ += '|%s' % cls._numeric_repr_(unknown)
        else:
            pseudo_member._name_ = None
        pseudo_member = cls._value2member_map_.setdefault(value, pseudo_member)
        if neg_value is not None:
            cls._value2member_map_[neg_value] = pseudo_member
        return pseudo_member

    @classmethod
    def _iter_member_(cls, value):
        # `Flag._iter_member_by_def_`: the members in `value`, in definition
        # order (which `RegexFlag`'s is not the same as value order).
        members = [cls._value2member_map_.get(v) for v in _iter_bits_lsb(value & cls._flag_mask_)]
        return iter(sorted(members, key=lambda m: m._sort_order_))

    @property
    def name(self):
        return self._name_

    @property
    def value(self):
        return self._value_

    # `enum.global_flag_repr`: module-qualified member names.
    def __repr__(self):
        module = self.__class__.__module__.split('.')[-1]
        cls_name = self.__class__.__name__
        if self._name_ is None:
            return "%s.%s(%r)" % (module, cls_name, self._value_)
        if _is_single_bit(self._value_):
            return '%s.%s' % (module, self._name_)
        name = []
        for n in self._name_.split('|'):
            if n[0].isdigit():
                name.append(n)
            else:
                name.append('%s.%s' % (module, n))
        return '|'.join(name)

    __str__ = object.__str__

    def __format__(self, format_spec):
        # `IntFlag` formats as its `int`; `int.__format__` with an empty spec
        # is `str(self)`.
        if not format_spec:
            return str(self)
        return format(self._value_, format_spec)

    def __reduce_ex__(self, proto):
        return self.__class__, (self._value_, )

    def __copy__(self):
        return self

    def __deepcopy__(self, memo):
        return self

    def __bool__(self):
        return bool(self._value_)

    def __contains__(self, other):
        if not isinstance(other, self.__class__):
            raise TypeError(
                "unsupported operand type(s) for 'in': %r and %r" % (
                    type(other).__qualname__, self.__class__.__qualname__))
        return other._value_ & self._value_ == other._value_

    def __iter__(self):
        return self._iter_member_(self._value_)

    def __len__(self):
        return self._value_.bit_count()

    def _get_value(self, flag):
        if isinstance(flag, self.__class__):
            return flag._value_
        elif isinstance(flag, int):
            return flag
        return NotImplemented

    def __or__(self, other):
        other_value = self._get_value(other)
        if other_value is NotImplemented:
            return NotImplemented
        return self.__class__(self._value_ | other_value)

    def __and__(self, other):
        other_value = self._get_value(other)
        if other_value is NotImplemented:
            return NotImplemented
        return self.__class__(self._value_ & other_value)

    def __xor__(self, other):
        other_value = self._get_value(other)
        if other_value is NotImplemented:
            return NotImplemented
        return self.__class__(self._value_ ^ other_value)

    def __invert__(self):
        # The `KEEP` boundary inverts every bit, unknown ones included.
        if self._inverted_ is None:
            self._inverted_ = self.__class__(~self._value_)
        return self._inverted_

    __rand__ = __and__
    __ror__ = __or__
    __rxor__ = __xor__


def _define(cls, members):
    """Create `cls`'s members from `(names, value)` pairs, in definition order —
    what `enum._simple_enum` does with the class body. Every name after the
    first in a group is an alias of the same member."""
    cls._member_names_ = []
    cls._member_map_ = {}
    cls._value2member_map_ = {}
    cls._flag_mask_ = 0
    for order, (names, value) in enumerate(members):
        member = int.__new__(cls, value)
        member._name_ = names[0]
        member._value_ = value
        member._sort_order_ = order
        for name in names:
            cls._member_map_[name] = member
            setattr(cls, name, member)
        cls._value2member_map_[value] = member
        if _is_single_bit(value):
            cls._member_names_.append(names[0])
            cls._flag_mask_ |= value
    cls._singles_mask_ = cls._flag_mask_
    cls._all_bits_ = 2 ** cls._flag_mask_.bit_length() - 1
    cls._inverted_ = None


# `_constants.py`'s `SRE_FLAG_*` values.
_define(RegexFlag, [
    (('NOFLAG',), 0),
    (('ASCII', 'A'), 256),
    (('IGNORECASE', 'I'), 2),
    (('LOCALE', 'L'), 4),
    (('UNICODE', 'U'), 32),
    (('MULTILINE', 'M'), 8),
    (('DOTALL', 'S'), 16),
    (('VERBOSE', 'X'), 64),
    (('DEBUG',), 128),
])
"#
}
