# Examples

A corpus of self-contained Python 3 programs that run identically on `pythonrs`
and CPython 3.14. Every script is deterministic (no clocks, addresses, unseeded
randomness, or set-repr ordering), so its stdout is byte-for-byte stable — the
[`parity`](../src/bin/parity.rs) harness runs each through both interpreters and
diffs the output.

Run one:

```sh
python examples/fizzbuzz.py        # the pythonrs binary
```

Run the whole corpus against CPython:

```sh
cargo build --bin python --bin parity
./target/debug/parity
```

## Language

| Script | Shows |
| --- | --- |
| `fizzbuzz.py` | control flow, three ways |
| `comprehensions.py` | list/dict/set/generator comprehensions, nesting, walrus |
| `closures.py` | lexical scope, `nonlocal`, factories, late vs early binding |
| `generators.py` | lazy sequences, `yield from`, `send`/`return` |
| `decorators.py` | `functools.wraps`, parameterized and stacked decorators |
| `exceptions.py` | custom hierarchies, chaining, `finally`, `try/except/else` |
| `context_managers.py` | the `with` protocol, `contextlib`, `redirect_stdout` |
| `pattern_matching.py` | PEP 634 structural pattern matching |
| `type_hints.py` | runtime annotations, `typing`, `NamedTuple`, `cached_property` |

## Objects

| Script | Shows |
| --- | --- |
| `classes.py` | classes, inheritance, `__repr__` |
| `inheritance.py` | `super()`, MRO, overriding, mixins |
| `operators.py` | operator overloading (an immutable `Vector`) |
| `dataclasses_demo.py` | `@dataclass`: defaults, ordering, frozen |
| `enums_demo.py` | `Enum`, `IntEnum`, `Flag`, `auto()` |

## Algorithms

| Script | Shows |
| --- | --- |
| `sorting.py` | quicksort/mergesort + the built-in sort with keys |
| `recursion.py` | Hanoi, permutations, Ackermann, tree walks |
| `dynamic_programming.py` | coin change, LCS, edit distance |
| `matrix.py` | transpose, multiply, identity |
| `calculator.py` | a shunting-yard expression evaluator |
| `fibonacci.py` | four implementations, including memoized |

## Data & text

| Script | Shows |
| --- | --- |
| `strings.py` | string methods, slicing, the format mini-language |
| `wordfreq.py` / `wordcount.py` | text analysis and frequency counts |
| `functional.py` | `map`/`filter`/`reduce`/`partial`/composition |
| `math_demo.py` | the `math` module, big integers, number theory |

## Standard library

| Script | Shows |
| --- | --- |
| `collections_demo.py` | `Counter`, `defaultdict`, `deque`, `namedtuple`, `OrderedDict` |
| `itertools_demo.py` | combinatorics, infinite iterators, grouping |
| `json_demo.py` | serialize/deserialize/pretty-print/round-trip |
| `regex_demo.py` | `re`: match, findall, groups, substitution |
| `hashing.py` | `hashlib`, `base64`, `binascii` |
| `datetime_demo.py` | dates, times, and `timedelta` arithmetic |
| `bank_account.py` | a stateful simulation with a transaction ledger |
