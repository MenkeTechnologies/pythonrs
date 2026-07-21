//! Language Server Protocol over stdio (`python --lsp`).
//!
//! Self-contained and read-only: diagnostics come from the same `parser::parse`
//! the runtime uses (a syntax error maps to the reported line); hover and
//! completion draw on the builtin/keyword/method corpus below. No output ever
//! reaches the terminal — JSON-RPC on stdio only. Structure follows the sibling
//! `-rs` interpreters' `lsp.rs` (see `rubylang/src/lsp.rs`).

use std::collections::HashMap;

use lsp_server::{Connection, ErrorCode, ExtractError, Message, Request, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _,
    PublishDiagnostics,
};
use lsp_types::request::{Completion, HoverRequest, Request as _};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionOptions, CompletionParams, CompletionResponse,
    Diagnostic, DiagnosticSeverity, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, Hover, HoverContents, HoverParams, HoverProviderCapability,
    MarkupContent, MarkupKind, Position, PublishDiagnosticsParams, Range, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions, Uri,
};

/// The builtin / keyword / method corpus: (name, chapter, one-line doc, example).
/// Single source of truth for LSP completion and hover. Every non-keyword entry
/// mirrors a real dispatch arm in `builtins.rs`:
///   * "Builtin"  → `call_builtin_function` (src/builtins.rs match at name arms)
///   * "math"     → `call_math` (the `math.*` module functions)
///   * "str"/"list"/"dict"/"set"/"tuple"/"int"/"float" → the per-type `*_method`
///     dispatch tables (`str_method`, `list_method`, `dict_method`, `set_method`,
///     `tuple_method`, `num_method`).
///
/// Keywords come from the lexer/parser recognized-keyword set.
const CORPUS: &[(&str, &str, &str, &str)] = &[
    // ── Keyword ──
    (
        "def",
        "Keyword",
        "define a function; the body runs on call and returns via `return` (else None)",
        "def greet(): return \"hi\"\ngreet()   # => 'hi'",
    ),
    (
        "class",
        "Keyword",
        "define a class; the body populates its namespace and base list",
        "class A: pass\nA().__class__.__name__   # => 'A'",
    ),
    (
        "return",
        "Keyword",
        "return a value from the current function (None if omitted)",
        "def f(): return 9\nf()   # => 9",
    ),
    (
        "lambda",
        "Keyword",
        "anonymous single-expression function: `lambda args: expr`",
        "(lambda x: x * 2)(4)   # => 8",
    ),
    (
        "yield",
        "Keyword",
        "suspend a generator, yielding a value to the caller",
        "def g():\n    yield 1\nlist(g())   # => [1]",
    ),
    (
        "import",
        "Keyword",
        "import a module by name into the current namespace",
        "import math\nmath.sqrt(16)   # => 4.0",
    ),
    (
        "from",
        "Keyword",
        "import specific names from a module: `from m import a, b`",
        "from math import sqrt\nsqrt(9)   # => 3.0",
    ),
    (
        "as",
        "Keyword",
        "bind an import / with / except target to a name",
        "import math as m\nm.floor(3.7)   # => 3",
    ),
    (
        "if",
        "Keyword",
        "conditional branch; also the `x if cond else y` expression",
        "x = 5 if True else 0\nx   # => 5",
    ),
    (
        "elif",
        "Keyword",
        "additional condition branch inside an if",
        "if False: pass\nelif True: x = 2   # x => 2",
    ),
    (
        "else",
        "Keyword",
        "fallback branch of an if / for / while / try",
        "if False: x = 1\nelse: x = 2   # x => 2",
    ),
    (
        "while",
        "Keyword",
        "loop while the condition is truthy",
        "i = 0\nwhile i < 3: i += 1\ni   # => 3",
    ),
    (
        "for",
        "Keyword",
        "iterate over an iterable: `for x in iterable:`",
        "s = 0\nfor n in [1, 2, 3]: s += n\ns   # => 6",
    ),
    (
        "in",
        "Keyword",
        "membership test, and the `for x in …` separator",
        "3 in [1, 2, 3]   # => True",
    ),
    (
        "is",
        "Keyword",
        "identity test (same object), not value equality",
        "a = None\na is None   # => True",
    ),
    (
        "not",
        "Keyword",
        "logical negation (also `not in` / `is not`)",
        "not False   # => True",
    ),
    (
        "and",
        "Keyword",
        "short-circuiting logical AND; returns an operand",
        "1 and 2   # => 2",
    ),
    (
        "or",
        "Keyword",
        "short-circuiting logical OR; returns an operand",
        "None or 5   # => 5",
    ),
    (
        "try",
        "Keyword",
        "open an exception-handling block (except/else/finally)",
        "try:\n    1 / 0\nexcept ZeroDivisionError:\n    x = -1   # x => -1",
    ),
    (
        "except",
        "Keyword",
        "handle a raised exception, optionally by class and `as` name",
        "try: raise ValueError(\"e\")\nexcept ValueError as e: str(e)   # => 'e'",
    ),
    (
        "finally",
        "Keyword",
        "block that always runs whether or not an exception was raised",
        "try: x = 1\nfinally: y = 2   # y => 2",
    ),
    (
        "raise",
        "Keyword",
        "raise an exception (bare re-raises the active one)",
        "raise ValueError(\"boom\")   # raises ValueError: boom",
    ),
    (
        "with",
        "Keyword",
        "context-manager block: `with ctx as name:` (enter/exit)",
        "with open(\"f\") as fh: data = fh.read()",
    ),
    (
        "pass",
        "Keyword",
        "no-op statement placeholder",
        "def stub(): pass   # defines a no-op function",
    ),
    (
        "break",
        "Keyword",
        "exit the nearest enclosing loop immediately",
        "for n in [1, 2, 3]:\n    if n == 2: break   # stops at 2",
    ),
    (
        "continue",
        "Keyword",
        "skip to the next iteration of the nearest loop",
        "for n in [1, 2, 3]:\n    if n == 2: continue   # skips 2",
    ),
    (
        "del",
        "Keyword",
        "delete a name, item, or attribute binding",
        "d = {'a': 1}\ndel d['a']\nd   # => {}",
    ),
    (
        "assert",
        "Keyword",
        "raise AssertionError if the condition is falsey",
        "assert 1 + 1 == 2   # passes silently",
    ),
    (
        "global",
        "Keyword",
        "declare that a name refers to the module-global binding",
        "x = 0\ndef f():\n    global x\n    x = 5",
    ),
    (
        "nonlocal",
        "Keyword",
        "bind a name to the nearest enclosing function scope",
        "def outer():\n    x = 0\n    def inner():\n        nonlocal x\n        x = 5",
    ),
    (
        "async",
        "Keyword",
        "define a coroutine (`async def`) or async loop/with",
        "async def fetch(): return 1",
    ),
    (
        "await",
        "Keyword",
        "suspend a coroutine until the awaitable resolves",
        "async def f(): return await g()",
    ),
    (
        "match",
        "Keyword",
        "structural pattern-match statement (`match subject:`)",
        "match [1, 2]:\n    case [a, b]: a + b   # => 3",
    ),
    (
        "case",
        "Keyword",
        "a match branch; binds/tests against a pattern",
        "match 2:\n    case 1: x = 'a'\n    case _: x = 'b'   # x => 'b'",
    ),
    (
        "None",
        "Keyword",
        "the sole NoneType instance; the null / absent value",
        "(None is None)   # => True",
    ),
    (
        "True",
        "Keyword",
        "the boolean true value (an int subtype equal to 1)",
        "True + 1   # => 2",
    ),
    (
        "False",
        "Keyword",
        "the boolean false value (an int subtype equal to 0)",
        "False or 7   # => 7",
    ),
    // ── Builtin (call_builtin_function) ──
    (
        "print",
        "Builtin",
        "write args to stdout, space-separated, ending in newline",
        "print(\"hi\")   # prints hi, returns None",
    ),
    (
        "len",
        "Builtin",
        "number of items in a container (str/list/tuple/dict/set)",
        "len([1, 2, 3])   # => 3",
    ),
    (
        "range",
        "Builtin",
        "arithmetic progression: range(stop) or range(start, stop[, step])",
        "list(range(3))   # => [0, 1, 2]",
    ),
    (
        "int",
        "Builtin",
        "convert to an arbitrary-precision integer (optional base)",
        "int(\"ff\", 16)   # => 255",
    ),
    (
        "float",
        "Builtin",
        "convert a number or string to a floating-point value",
        "float(\"3.5\")   # => 3.5",
    ),
    (
        "str",
        "Builtin",
        "string form of an object (like calling __str__)",
        "str(123)   # => '123'",
    ),
    (
        "repr",
        "Builtin",
        "canonical string representation of an object",
        "repr(\"hi\")   # => \"'hi'\"",
    ),
    (
        "bool",
        "Builtin",
        "truth value of x as True or False",
        "bool([])   # => False",
    ),
    (
        "list",
        "Builtin",
        "build a list, optionally from an iterable",
        "list(\"ab\")   # => ['a', 'b']",
    ),
    (
        "tuple",
        "Builtin",
        "build an immutable tuple, optionally from an iterable",
        "tuple([1, 2])   # => (1, 2)",
    ),
    (
        "dict",
        "Builtin",
        "build a dict from kwargs / pairs / mapping",
        "dict(a=1)   # => {'a': 1}",
    ),
    (
        "set",
        "Builtin",
        "build a set (unique members) from an iterable",
        "set([1, 1, 2])   # => {1, 2}",
    ),
    (
        "frozenset",
        "Builtin",
        "build an immutable, hashable set from an iterable",
        "frozenset([1, 2])   # => frozenset({1, 2})",
    ),
    (
        "bytes",
        "Builtin",
        "immutable bytes object from a string/iterable/size",
        "bytes(\"AB\", \"utf-8\")   # => b'AB'",
    ),
    (
        "sum",
        "Builtin",
        "sum of an iterable, plus an optional start value",
        "sum([1, 2, 3])   # => 6",
    ),
    (
        "min",
        "Builtin",
        "smallest item of an iterable or of the args",
        "min([3, 1, 2])   # => 1",
    ),
    (
        "max",
        "Builtin",
        "largest item of an iterable or of the args",
        "max(3, 1, 2)   # => 3",
    ),
    (
        "sorted",
        "Builtin",
        "a new sorted list (optional key= and reverse=)",
        "sorted([3, 1, 2])   # => [1, 2, 3]",
    ),
    (
        "reversed",
        "Builtin",
        "an iterator over a sequence in reverse order",
        "list(reversed([1, 2, 3]))   # => [3, 2, 1]",
    ),
    (
        "enumerate",
        "Builtin",
        "iterator of (index, value) pairs (optional start=)",
        "list(enumerate(\"ab\"))   # => [(0, 'a'), (1, 'b')]",
    ),
    (
        "zip",
        "Builtin",
        "iterator of tuples pairing items from each iterable",
        "list(zip([1, 2], [3, 4]))   # => [(1, 3), (2, 4)]",
    ),
    (
        "map",
        "Builtin",
        "apply a function across one or more iterables (lazy)",
        "list(map(str, [1, 2]))   # => ['1', '2']",
    ),
    (
        "filter",
        "Builtin",
        "items of an iterable where func is truthy (lazy)",
        "list(filter(None, [0, 1, 2]))   # => [1, 2]",
    ),
    (
        "any",
        "Builtin",
        "True if any item of the iterable is truthy",
        "any([0, 0, 1])   # => True",
    ),
    (
        "all",
        "Builtin",
        "True if every item of the iterable is truthy",
        "all([1, 2, 3])   # => True",
    ),
    (
        "abs",
        "Builtin",
        "absolute value / magnitude of a number",
        "abs(-5)   # => 5",
    ),
    (
        "round",
        "Builtin",
        "round a number to optional ndigits (banker's rounding)",
        "round(3.14159, 2)   # => 3.14",
    ),
    (
        "divmod",
        "Builtin",
        "return the pair (a // b, a % b)",
        "divmod(7, 2)   # => (3, 1)",
    ),
    (
        "pow",
        "Builtin",
        "x ** y, or pow(x, y, mod) for modular exponentiation",
        "pow(2, 10)   # => 1024",
    ),
    (
        "type",
        "Builtin",
        "the type/class of an object",
        "type(3).__name__   # => 'int'",
    ),
    (
        "isinstance",
        "Builtin",
        "True if the object is an instance of the class(es)",
        "isinstance(3, int)   # => True",
    ),
    (
        "issubclass",
        "Builtin",
        "True if the first class is a subclass of the second",
        "issubclass(bool, int)   # => True",
    ),
    (
        "callable",
        "Builtin",
        "True if the object can be called like a function",
        "callable(len)   # => True",
    ),
    (
        "hasattr",
        "Builtin",
        "True if the object has the named attribute",
        "hasattr(\"hi\", \"upper\")   # => True",
    ),
    (
        "getattr",
        "Builtin",
        "read an attribute by name (optional default)",
        "getattr(\"hi\", \"upper\")()   # => 'HI'",
    ),
    (
        "setattr",
        "Builtin",
        "set an attribute by name on an object",
        "setattr(obj, \"x\", 7)   # obj.x => 7",
    ),
    (
        "vars",
        "Builtin",
        "the object's __dict__ (namespace mapping)",
        "vars(obj)   # => {...}",
    ),
    (
        "dir",
        "Builtin",
        "sorted list of an object's attribute names",
        "dir(obj)   # => [...]",
    ),
    (
        "id",
        "Builtin",
        "the identity (address) integer of an object",
        "id(obj)   # => a stable int",
    ),
    (
        "hash",
        "Builtin",
        "the hash value of a hashable object",
        "hash((1, 2))   # => an int",
    ),
    (
        "iter",
        "Builtin",
        "return an iterator over the argument",
        "next(iter([9, 8]))   # => 9",
    ),
    (
        "next",
        "Builtin",
        "advance an iterator (optional default on exhaustion)",
        "next(iter([9]))   # => 9",
    ),
    (
        "input",
        "Builtin",
        "read a line from stdin (optional prompt), no newline",
        "input(\"name: \")   # reads one line",
    ),
    (
        "ord",
        "Builtin",
        "Unicode code point of a one-character string",
        "ord(\"A\")   # => 65",
    ),
    (
        "chr",
        "Builtin",
        "one-character string for a Unicode code point",
        "chr(65)   # => 'A'",
    ),
    (
        "hex",
        "Builtin",
        "'0x'-prefixed hexadecimal string of an integer",
        "hex(255)   # => '0xff'",
    ),
    (
        "oct",
        "Builtin",
        "'0o'-prefixed octal string of an integer",
        "oct(8)   # => '0o10'",
    ),
    (
        "bin",
        "Builtin",
        "'0b'-prefixed binary string of an integer",
        "bin(5)   # => '0b101'",
    ),
    (
        "ascii",
        "Builtin",
        "repr of an object with non-ASCII chars escaped",
        "ascii(\"é\")   # => \"'\\\\xe9'\"",
    ),
    (
        "complex",
        "Builtin",
        "build a complex number from real and imaginary parts",
        "complex(1, 2)   # => (1+2j)",
    ),
    (
        "format",
        "Builtin",
        "format a value using a format spec string",
        "format(255, \"x\")   # => 'ff'",
    ),
    (
        "object",
        "Builtin",
        "the base object; object() is a featureless instance",
        "object().__class__.__name__   # => 'object'",
    ),
    (
        "nan",
        "Builtin",
        "the IEEE 754 not-a-number float value",
        "nan != nan   # => True",
    ),
    // ── math (call_math) ──
    (
        "sqrt",
        "math",
        "square root as a float",
        "math.sqrt(16)   # => 4.0",
    ),
    (
        "pow",
        "math",
        "x raised to the power y, as a float",
        "math.pow(2, 3)   # => 8.0",
    ),
    (
        "floor",
        "math",
        "largest integer <= x",
        "math.floor(3.7)   # => 3",
    ),
    (
        "ceil",
        "math",
        "smallest integer >= x",
        "math.ceil(3.2)   # => 4",
    ),
    (
        "fabs",
        "math",
        "absolute value as a float",
        "math.fabs(-5)   # => 5.0",
    ),
    (
        "factorial",
        "math",
        "n! for a non-negative integer n",
        "math.factorial(5)   # => 120",
    ),
    (
        "gcd",
        "math",
        "greatest common divisor of the integer args",
        "math.gcd(12, 8)   # => 4",
    ),
    (
        "log",
        "math",
        "natural log, or log base b when a second arg is given",
        "math.log(8, 2)   # => 3.0",
    ),
    (
        "sin",
        "math",
        "sine of x (radians)",
        "math.sin(0)   # => 0.0",
    ),
    (
        "cos",
        "math",
        "cosine of x (radians)",
        "math.cos(0)   # => 1.0",
    ),
    // ── str methods ──
    (
        "upper",
        "str",
        "copy with all cased characters uppercased",
        "\"abc\".upper()   # => 'ABC'",
    ),
    (
        "lower",
        "str",
        "copy with all cased characters lowercased",
        "\"ABC\".lower()   # => 'abc'",
    ),
    (
        "casefold",
        "str",
        "aggressive lowercase for caseless matching",
        "\"ABC\".casefold()   # => 'abc'",
    ),
    (
        "strip",
        "str",
        "copy with leading and trailing chars (default whitespace) removed",
        "\"  hi  \".strip()   # => 'hi'",
    ),
    (
        "lstrip",
        "str",
        "copy with leading chars (default whitespace) removed",
        "\"  hi\".lstrip()   # => 'hi'",
    ),
    (
        "rstrip",
        "str",
        "copy with trailing chars (default whitespace) removed",
        "\"hi  \".rstrip()   # => 'hi'",
    ),
    (
        "swapcase",
        "str",
        "copy with the case of every letter inverted",
        "\"Abc\".swapcase()   # => 'aBC'",
    ),
    (
        "capitalize",
        "str",
        "copy with the first char upper, the rest lower",
        "\"hELLO\".capitalize()   # => 'Hello'",
    ),
    (
        "title",
        "str",
        "copy with the first letter of each word uppercased",
        "\"a b\".title()   # => 'A B'",
    ),
    (
        "split",
        "str",
        "list of substrings split on sep (default: runs of whitespace)",
        "\"a,b,c\".split(\",\")   # => ['a', 'b', 'c']",
    ),
    (
        "rsplit",
        "str",
        "like split, but scanning from the right (honours maxsplit)",
        "\"a,b,c\".rsplit(\",\", 1)   # => ['a,b', 'c']",
    ),
    (
        "splitlines",
        "str",
        "list of lines, splitting at line boundaries",
        "\"a\\nb\".splitlines()   # => ['a', 'b']",
    ),
    (
        "join",
        "str",
        "concatenate an iterable of strings using self as separator",
        "\",\".join([\"a\", \"b\"])   # => 'a,b'",
    ),
    (
        "replace",
        "str",
        "copy with occurrences of old replaced by new (optional count)",
        "\"aaa\".replace(\"a\", \"b\", 2)   # => 'bba'",
    ),
    (
        "startswith",
        "str",
        "True if the string starts with the given prefix",
        "\"hello\".startswith(\"he\")   # => True",
    ),
    (
        "endswith",
        "str",
        "True if the string ends with the given suffix",
        "\"hello\".endswith(\"lo\")   # => True",
    ),
    (
        "find",
        "str",
        "lowest index of substring, or -1 if not found",
        "\"hello\".find(\"l\")   # => 2",
    ),
    (
        "rfind",
        "str",
        "highest index of substring, or -1 if not found",
        "\"hello\".rfind(\"l\")   # => 3",
    ),
    (
        "index",
        "str",
        "like find, but raises ValueError if the substring is absent",
        "\"hello\".index(\"e\")   # => 1",
    ),
    (
        "count",
        "str",
        "number of non-overlapping occurrences of the substring",
        "\"hello\".count(\"l\")   # => 2",
    ),
    (
        "isdigit",
        "str",
        "True if non-empty and every char is a digit",
        "\"123\".isdigit()   # => True",
    ),
    (
        "isalpha",
        "str",
        "True if non-empty and every char is alphabetic",
        "\"abc\".isalpha()   # => True",
    ),
    (
        "isalnum",
        "str",
        "True if non-empty and every char is alphanumeric",
        "\"a1\".isalnum()   # => True",
    ),
    (
        "isspace",
        "str",
        "True if non-empty and every char is whitespace",
        "\"  \".isspace()   # => True",
    ),
    (
        "isupper",
        "str",
        "True if it has cased chars and they are all uppercase",
        "\"ABC\".isupper()   # => True",
    ),
    (
        "islower",
        "str",
        "True if it has cased chars and they are all lowercase",
        "\"abc\".islower()   # => True",
    ),
    (
        "zfill",
        "str",
        "right-justify to width, padding with '0' (respects sign)",
        "\"42\".zfill(5)   # => '00042'",
    ),
    (
        "center",
        "str",
        "center in a field of width, padding with fillchar (default space)",
        "\"hi\".center(6)   # => '  hi  '",
    ),
    (
        "ljust",
        "str",
        "left-justify in a field of width, padding on the right",
        "\"hi\".ljust(5)   # => 'hi   '",
    ),
    (
        "rjust",
        "str",
        "right-justify in a field of width, padding on the left",
        "\"hi\".rjust(5)   # => '   hi'",
    ),
    (
        "removeprefix",
        "str",
        "copy with the prefix removed if present, else unchanged",
        "\"unhappy\".removeprefix(\"un\")   # => 'happy'",
    ),
    (
        "removesuffix",
        "str",
        "copy with the suffix removed if present, else unchanged",
        "\"file.py\".removesuffix(\".py\")   # => 'file'",
    ),
    (
        "encode",
        "str",
        "encode the string to a bytes object (default UTF-8)",
        "\"ab\".encode()   # => b'ab'",
    ),
    (
        "format",
        "str",
        "substitute {} / {name} fields with the given args",
        "\"{}-{}\".format(1, 2)   # => '1-2'",
    ),
    // ── list methods ──
    (
        "append",
        "list",
        "add a single item to the end of the list (in place)",
        "x = [1]; x.append(2); x   # => [1, 2]",
    ),
    (
        "extend",
        "list",
        "append every item from an iterable (in place)",
        "x = [1]; x.extend([2, 3]); x   # => [1, 2, 3]",
    ),
    (
        "insert",
        "list",
        "insert an item before the given index (in place)",
        "x = [1, 3]; x.insert(1, 2); x   # => [1, 2, 3]",
    ),
    (
        "pop",
        "list",
        "remove and return item at index (default last)",
        "x = [1, 2, 3]; x.pop()   # => 3",
    ),
    (
        "remove",
        "list",
        "remove the first item equal to the value (in place)",
        "x = [1, 2, 2]; x.remove(2); x   # => [1, 2]",
    ),
    (
        "clear",
        "list",
        "remove all items from the list (in place)",
        "x = [1, 2]; x.clear(); x   # => []",
    ),
    (
        "index",
        "list",
        "index of the first item equal to the value (ValueError if none)",
        "[1, 2, 3].index(2)   # => 1",
    ),
    (
        "count",
        "list",
        "number of items equal to the value",
        "[1, 1, 2].count(1)   # => 2",
    ),
    (
        "reverse",
        "list",
        "reverse the list in place",
        "x = [1, 2, 3]; x.reverse(); x   # => [3, 2, 1]",
    ),
    (
        "copy",
        "list",
        "a shallow copy of the list",
        "[1, 2].copy()   # => [1, 2]",
    ),
    (
        "sort",
        "list",
        "sort the list in place (optional key= and reverse=)",
        "x = [3, 1, 2]; x.sort(); x   # => [1, 2, 3]",
    ),
    // ── dict methods ──
    (
        "keys",
        "dict",
        "a view of the dictionary's keys",
        "list({'a': 1}.keys())   # => ['a']",
    ),
    (
        "values",
        "dict",
        "a view of the dictionary's values",
        "list({'a': 1}.values())   # => [1]",
    ),
    (
        "items",
        "dict",
        "a view of the dictionary's (key, value) pairs",
        "list({'a': 1}.items())   # => [('a', 1)]",
    ),
    (
        "get",
        "dict",
        "value for key, or a default (None) if the key is absent",
        "{'a': 1}.get(\"b\", 0)   # => 0",
    ),
    (
        "pop",
        "dict",
        "remove key and return its value (or a supplied default)",
        "{'a': 1}.pop(\"a\")   # => 1",
    ),
    (
        "setdefault",
        "dict",
        "value for key; insert it with a default if absent",
        "d = {}; d.setdefault(\"a\", 1)   # => 1",
    ),
    (
        "update",
        "dict",
        "merge another mapping / pairs into this dict (in place)",
        "d = {'a': 1}; d.update({'b': 2}); d   # => {'a': 1, 'b': 2}",
    ),
    (
        "popitem",
        "dict",
        "remove and return the last-inserted (key, value) pair",
        "{'a': 1}.popitem()   # => ('a', 1)",
    ),
    // ── set methods ──
    (
        "add",
        "set",
        "add an element to the set (in place; a no-op if present)",
        "s = {1}; s.add(2); s   # => {1, 2}",
    ),
    (
        "discard",
        "set",
        "remove an element if present; no error if it is absent",
        "s = {1, 2}; s.discard(3); s   # => {1, 2}",
    ),
    (
        "union",
        "set",
        "a new set with elements from this set and the others",
        "{1}.union({2})   # => {1, 2}",
    ),
    (
        "intersection",
        "set",
        "a new set with elements common to all the sets",
        "{1, 2}.intersection({2, 3})   # => {2}",
    ),
    (
        "difference",
        "set",
        "a new set with elements in this set but not the others",
        "{1, 2}.difference({2})   # => {1}",
    ),
    (
        "symmetric_difference",
        "set",
        "a new set with elements in exactly one of the two sets",
        "{1, 2}.symmetric_difference({2, 3})   # => {1, 3}",
    ),
    (
        "issubset",
        "set",
        "True if every element of this set is in the other",
        "{1}.issubset({1, 2})   # => True",
    ),
    (
        "issuperset",
        "set",
        "True if this set contains every element of the other",
        "{1, 2}.issuperset({1})   # => True",
    ),
    // ── tuple methods ──
    (
        "count",
        "tuple",
        "number of items equal to the value",
        "(1, 1, 2).count(1)   # => 2",
    ),
    (
        "index",
        "tuple",
        "index of the first item equal to the value",
        "(1, 2, 3).index(2)   # => 1",
    ),
    // ── int / float methods (num_method) ──
    (
        "bit_length",
        "int",
        "number of bits needed to represent abs(self) in binary",
        "255 .bit_length()   # => 8",
    ),
    (
        "bit_count",
        "int",
        "number of ones in the binary representation of abs(self)",
        "255 .bit_count()   # => 8",
    ),
    (
        "to_bytes",
        "int",
        "to_bytes(length=1, byteorder='big', *, signed=False) — the int as bytes",
        "(258).to_bytes(2, 'big')   # => b'\\x01\\x02'",
    ),
    (
        "from_bytes",
        "int",
        "from_bytes(bytes, byteorder='big', *, signed=False) — an int from bytes",
        "int.from_bytes(b'\\x01\\x02', 'big')   # => 258",
    ),
    (
        "as_integer_ratio",
        "int",
        "a pair (numerator, denominator) whose exact ratio equals self",
        "(0.5).as_integer_ratio()   # => (1, 2)",
    ),
    (
        "is_integer",
        "float",
        "True if the float has no fractional part",
        "(3.0).is_integer()   # => True",
    ),
    (
        "hex",
        "float",
        "the exact value as a hexadecimal string",
        "(3.14).hex()   # => '0x1.91eb851eb851fp+1'",
    ),
    (
        "fromhex",
        "float",
        "fromhex(s) — parse a hexadecimal float string (inverse of float.hex)",
        "float.fromhex('0x1.8p+1')   # => 3.0",
    ),
    (
        "conjugate",
        "int",
        "the complex conjugate; for a real number, self",
        "(5).conjugate()   # => 5",
    ),
];

/// The builtin corpus, exposed for offline doc generation.
pub fn corpus() -> &'static [(&'static str, &'static str, &'static str, &'static str)] {
    CORPUS
}

/// Open document text keyed by URI, kept current from the sync notifications so
/// hover can look up the identifier under the cursor.
type Docs = HashMap<String, String>;

/// Entry point for `python --lsp`.
pub fn run() -> Result<(), String> {
    spawn_orphan_guard();
    let (conn, io_threads) = Connection::stdio();
    let (init_id, _params) = conn
        .initialize_start()
        .map_err(|e| format!("lsp initialize: {e}"))?;
    let init_result = serde_json::json!({
        "capabilities": server_capabilities(),
        "serverInfo": { "name": "pythonrs", "version": env!("CARGO_PKG_VERSION") },
    });
    conn.sender
        .send(Response::new_ok(init_id, init_result).into())
        .map_err(|e| format!("lsp send: {e}"))?;

    let mut docs: Docs = HashMap::new();
    for msg in &conn.receiver {
        match msg {
            Message::Request(req) => {
                if conn
                    .handle_shutdown(&req)
                    .map_err(|e| format!("lsp shutdown: {e}"))?
                {
                    break;
                }
                dispatch_request(&conn, &docs, req);
            }
            Message::Notification(not) => dispatch_notification(&conn, &mut docs, not),
            Message::Response(_) => {}
        }
    }
    drop(conn);
    io_threads.join().map_err(|_| "lsp io join".to_string())?;
    Ok(())
}

fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::FULL),
                ..Default::default()
            },
        )),
        completion_provider: Some(CompletionOptions {
            resolve_provider: Some(false),
            ..Default::default()
        }),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        ..Default::default()
    }
}

fn handle<P, R>(conn: &Connection, req: Request, f: impl FnOnce(P) -> R)
where
    P: serde::de::DeserializeOwned,
    R: serde::Serialize,
{
    let method = req.method.clone();
    let id = req.id.clone();
    match req.extract::<P>(&method) {
        Ok((id, params)) => {
            let value = serde_json::to_value(f(params)).unwrap_or(serde_json::Value::Null);
            let _ = conn.sender.send(Response::new_ok(id, value).into());
        }
        Err(ExtractError::JsonError { error, .. }) => {
            let _ = conn.sender.send(
                Response::new_err(id, ErrorCode::InvalidParams as i32, error.to_string()).into(),
            );
        }
        Err(ExtractError::MethodMismatch(_)) => unreachable!("method matched before extract"),
    }
}

fn dispatch_request(conn: &Connection, docs: &Docs, req: Request) {
    match req.method.as_str() {
        Completion::METHOD => handle(conn, req, |_p: CompletionParams| completions()),
        HoverRequest::METHOD => handle(conn, req, |p: HoverParams| hover(docs, &p)),
        _ => {
            let _ = conn.sender.send(
                Response::new_err(req.id, ErrorCode::MethodNotFound as i32, "unhandled".into())
                    .into(),
            );
        }
    }
}

fn dispatch_notification(conn: &Connection, docs: &mut Docs, not: lsp_server::Notification) {
    match not.method.as_str() {
        DidOpenTextDocument::METHOD => {
            if let Ok(p) = serde_json::from_value::<DidOpenTextDocumentParams>(not.params) {
                let uri = p.text_document.uri;
                docs.insert(uri.as_str().to_string(), p.text_document.text.clone());
                publish_diagnostics(conn, &uri, &p.text_document.text);
            }
        }
        DidChangeTextDocument::METHOD => {
            if let Ok(p) = serde_json::from_value::<DidChangeTextDocumentParams>(not.params) {
                if let Some(change) = p.content_changes.into_iter().last() {
                    let uri = p.text_document.uri;
                    docs.insert(uri.as_str().to_string(), change.text.clone());
                    publish_diagnostics(conn, &uri, &change.text);
                }
            }
        }
        DidCloseTextDocument::METHOD => {
            if let Ok(p) = serde_json::from_value::<DidCloseTextDocumentParams>(not.params) {
                let uri = p.text_document.uri;
                docs.remove(uri.as_str());
                publish_diagnostics(conn, &uri, "");
            }
        }
        _ => {}
    }
}

fn completions() -> CompletionResponse {
    let items = CORPUS
        .iter()
        .map(|(name, chapter, doc, _example)| CompletionItem {
            label: name.to_string(),
            kind: Some(if *chapter == "Keyword" {
                CompletionItemKind::KEYWORD
            } else if *chapter == "Builtin" || *chapter == "math" {
                CompletionItemKind::FUNCTION
            } else {
                CompletionItemKind::METHOD
            }),
            detail: Some((*doc).to_string()),
            ..Default::default()
        })
        .collect();
    CompletionResponse::Array(items)
}

/// Hover: look up the identifier under the cursor in the corpus and render its
/// chapter, doc, and example. Falls back to a short banner when the cursor is
/// not on a known name.
fn hover(docs: &Docs, params: &HoverParams) -> Hover {
    let pos = params.text_document_position_params.position;
    let uri = params
        .text_document_position_params
        .text_document
        .uri
        .as_str();
    let word = docs
        .get(uri)
        .and_then(|text| word_at(text, pos))
        .unwrap_or_default();

    let matches: Vec<&(&str, &str, &str, &str)> =
        CORPUS.iter().filter(|(name, ..)| *name == word).collect();

    let body = if matches.is_empty() {
        "**pythonrs** — Python on the fusevm bytecode VM + Cranelift JIT.".to_string()
    } else {
        let mut out = String::new();
        for (name, chapter, doc, example) in matches {
            out.push_str(&format!(
                "**`{name}`** — _{chapter}_\n\n{doc}\n\n```python\n{example}\n```\n\n"
            ));
        }
        out.trim_end().to_string()
    };

    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: body,
        }),
        range: None,
    }
}

/// Extract the identifier (`[A-Za-z0-9_]+`) spanning the given position, if any.
fn word_at(text: &str, pos: Position) -> Option<String> {
    let line = text.lines().nth(pos.line as usize)?;
    let chars: Vec<char> = line.chars().collect();
    let col = (pos.character as usize).min(chars.len());
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';

    let mut start = col;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end < chars.len() && is_word(chars[end]) {
        end += 1;
    }
    if start == end {
        return None;
    }
    Some(chars[start..end].iter().collect())
}

fn publish_diagnostics(conn: &Connection, uri: &Uri, text: &str) {
    let params = PublishDiagnosticsParams {
        uri: uri.clone(),
        diagnostics: compute_diagnostics(text),
        version: None,
    };
    let not = lsp_server::Notification::new(PublishDiagnostics::METHOD.to_string(), params);
    let _ = conn.sender.send(not.into());
}

/// Parse the whole document with the runtime's own parser; a syntax error maps
/// to a single diagnostic on the line named in its `(line N)` suffix.
fn compute_diagnostics(text: &str) -> Vec<Diagnostic> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    match crate::parser::parse(text) {
        Ok(_) => Vec::new(),
        Err(e) => {
            let line = parse_error_line(&e).saturating_sub(1);
            vec![Diagnostic {
                range: Range {
                    start: Position { line, character: 0 },
                    end: Position {
                        line,
                        character: 200,
                    },
                },
                severity: Some(DiagnosticSeverity::ERROR),
                message: e,
                ..Default::default()
            }]
        }
    }
}

/// Extract the (1-based) line number from a pythonrs parser error, which embeds
/// it as `… (line N)`. Defaults to line 1 when no such marker is present.
fn parse_error_line(e: &str) -> u32 {
    e.rsplit_once("(line ")
        .and_then(|(_, rest)| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(1)
}

/// Exit if reparented to pid 1 (the editor died) so we never leak.
fn spawn_orphan_guard() {
    std::thread::spawn(|| {
        #[cfg(target_os = "linux")]
        // SAFETY: prctl(PR_SET_PDEATHSIG, ...) only registers a signal disposition.
        unsafe {
            libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGKILL as libc::c_ulong,
                0,
                0,
                0,
            );
        }
        loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            // SAFETY: getppid takes no arguments and never fails.
            if unsafe { libc::getppid() } == 1 {
                std::process::exit(0);
            }
        }
    });
}
