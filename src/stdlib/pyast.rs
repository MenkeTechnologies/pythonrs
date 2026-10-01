//! `_ast` — the AST node types `ast.py` is built on.
//!
//! CPython generates this module from `Parser/Python.asdl`: ~130 classes in a
//! shallow hierarchy (`Add` is an `operator` is an `AST`), each carrying a
//! `_fields` tuple naming its children and, for statements and expressions, an
//! `_attributes` tuple naming the four source-position slots. There is no
//! behavior beyond construction and field access — the traversal helpers
//! (`walk`, `iter_fields`, `NodeVisitor`, `unparse`) all live in `ast.py`.
//!
//! Because the node types are pure data, they are DECLARED here and defined by
//! running generated Python, rather than hand-registered one at a time from Rust.
//! The generated source is the table below expanded into `class` statements —
//! the same relationship CPython's C file has to the ASDL grammar, and it keeps
//! the semantics (`__init__` binding positional args to `_fields` and filling
//! the defaults, `__repr__`) written in Python where they belong.

/// One AST node's table entry: `(name, base, fields, attributes)`. Each field
/// is `(name, type)`, the type written as CPython 3.14 renders the field's
/// `_field_types` entry: `T | None` for an optional (`?`) ASDL field, `list[T]`
/// for a sequence (`*`), a bare `T` for a required one. `_attributes` are the
/// source-position slots every statement and expression carries.
pub type NodeSpec = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
    &'static [&'static str],
);

/// Every `_ast` node type, transcribed from CPython 3.14's ASDL-generated
/// module (each class's `_fields`, `_field_types` and `_attributes`).
pub const AST_NODES: &[NodeSpec] = &[
    ("AST", "object", &[], &[]),
    ("Add", "operator", &[], &[]),
    ("And", "boolop", &[], &[]),
    (
        "AnnAssign",
        "stmt",
        &[
            ("target", "expr"),
            ("annotation", "expr"),
            ("value", "expr | None"),
            ("simple", "int"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Assert",
        "stmt",
        &[("test", "expr"), ("msg", "expr | None")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Assign",
        "stmt",
        &[
            ("targets", "list[expr]"),
            ("value", "expr"),
            ("type_comment", "str | None"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "AsyncFor",
        "stmt",
        &[
            ("target", "expr"),
            ("iter", "expr"),
            ("body", "list[stmt]"),
            ("orelse", "list[stmt]"),
            ("type_comment", "str | None"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "AsyncFunctionDef",
        "stmt",
        &[
            ("name", "str"),
            ("args", "arguments"),
            ("body", "list[stmt]"),
            ("decorator_list", "list[expr]"),
            ("returns", "expr | None"),
            ("type_comment", "str | None"),
            ("type_params", "list[type_param]"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "AsyncWith",
        "stmt",
        &[
            ("items", "list[withitem]"),
            ("body", "list[stmt]"),
            ("type_comment", "str | None"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Attribute",
        "expr",
        &[("value", "expr"), ("attr", "str"), ("ctx", "expr_context")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "AugAssign",
        "stmt",
        &[("target", "expr"), ("op", "operator"), ("value", "expr")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Await",
        "expr",
        &[("value", "expr")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "BinOp",
        "expr",
        &[("left", "expr"), ("op", "operator"), ("right", "expr")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    ("BitAnd", "operator", &[], &[]),
    ("BitOr", "operator", &[], &[]),
    ("BitXor", "operator", &[], &[]),
    (
        "BoolOp",
        "expr",
        &[("op", "boolop"), ("values", "list[expr]")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Break",
        "stmt",
        &[],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Call",
        "expr",
        &[
            ("func", "expr"),
            ("args", "list[expr]"),
            ("keywords", "list[keyword]"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "ClassDef",
        "stmt",
        &[
            ("name", "str"),
            ("bases", "list[expr]"),
            ("keywords", "list[keyword]"),
            ("body", "list[stmt]"),
            ("decorator_list", "list[expr]"),
            ("type_params", "list[type_param]"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Compare",
        "expr",
        &[
            ("left", "expr"),
            ("ops", "list[cmpop]"),
            ("comparators", "list[expr]"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Constant",
        "expr",
        &[("value", "object"), ("kind", "str | None")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Continue",
        "stmt",
        &[],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    ("Del", "expr_context", &[], &[]),
    (
        "Delete",
        "stmt",
        &[("targets", "list[expr]")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Dict",
        "expr",
        &[("keys", "list[expr]"), ("values", "list[expr]")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "DictComp",
        "expr",
        &[
            ("key", "expr"),
            ("value", "expr"),
            ("generators", "list[comprehension]"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    ("Div", "operator", &[], &[]),
    ("Eq", "cmpop", &[], &[]),
    (
        "ExceptHandler",
        "excepthandler",
        &[
            ("type", "expr | None"),
            ("name", "str | None"),
            ("body", "list[stmt]"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Expr",
        "stmt",
        &[("value", "expr")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    ("Expression", "mod", &[("body", "expr")], &[]),
    ("FloorDiv", "operator", &[], &[]),
    (
        "For",
        "stmt",
        &[
            ("target", "expr"),
            ("iter", "expr"),
            ("body", "list[stmt]"),
            ("orelse", "list[stmt]"),
            ("type_comment", "str | None"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "FormattedValue",
        "expr",
        &[
            ("value", "expr"),
            ("conversion", "int"),
            ("format_spec", "expr | None"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "FunctionDef",
        "stmt",
        &[
            ("name", "str"),
            ("args", "arguments"),
            ("body", "list[stmt]"),
            ("decorator_list", "list[expr]"),
            ("returns", "expr | None"),
            ("type_comment", "str | None"),
            ("type_params", "list[type_param]"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "FunctionType",
        "mod",
        &[("argtypes", "list[expr]"), ("returns", "expr")],
        &[],
    ),
    (
        "GeneratorExp",
        "expr",
        &[("elt", "expr"), ("generators", "list[comprehension]")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Global",
        "stmt",
        &[("names", "list[str]")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    ("Gt", "cmpop", &[], &[]),
    ("GtE", "cmpop", &[], &[]),
    (
        "If",
        "stmt",
        &[
            ("test", "expr"),
            ("body", "list[stmt]"),
            ("orelse", "list[stmt]"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "IfExp",
        "expr",
        &[("test", "expr"), ("body", "expr"), ("orelse", "expr")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Import",
        "stmt",
        &[("names", "list[alias]")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "ImportFrom",
        "stmt",
        &[
            ("module", "str | None"),
            ("names", "list[alias]"),
            ("level", "int | None"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    ("In", "cmpop", &[], &[]),
    ("Interactive", "mod", &[("body", "list[stmt]")], &[]),
    (
        "Interpolation",
        "expr",
        &[
            ("value", "expr"),
            ("str", "object"),
            ("conversion", "int"),
            ("format_spec", "expr | None"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    ("Invert", "unaryop", &[], &[]),
    ("Is", "cmpop", &[], &[]),
    ("IsNot", "cmpop", &[], &[]),
    (
        "JoinedStr",
        "expr",
        &[("values", "list[expr]")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    ("LShift", "operator", &[], &[]),
    (
        "Lambda",
        "expr",
        &[("args", "arguments"), ("body", "expr")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "List",
        "expr",
        &[("elts", "list[expr]"), ("ctx", "expr_context")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "ListComp",
        "expr",
        &[("elt", "expr"), ("generators", "list[comprehension]")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    ("Load", "expr_context", &[], &[]),
    ("Lt", "cmpop", &[], &[]),
    ("LtE", "cmpop", &[], &[]),
    ("MatMult", "operator", &[], &[]),
    (
        "Match",
        "stmt",
        &[("subject", "expr"), ("cases", "list[match_case]")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "MatchAs",
        "pattern",
        &[("pattern", "pattern | None"), ("name", "str | None")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "MatchClass",
        "pattern",
        &[
            ("cls", "expr"),
            ("patterns", "list[pattern]"),
            ("kwd_attrs", "list[str]"),
            ("kwd_patterns", "list[pattern]"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "MatchMapping",
        "pattern",
        &[
            ("keys", "list[expr]"),
            ("patterns", "list[pattern]"),
            ("rest", "str | None"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "MatchOr",
        "pattern",
        &[("patterns", "list[pattern]")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "MatchSequence",
        "pattern",
        &[("patterns", "list[pattern]")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "MatchSingleton",
        "pattern",
        &[("value", "object")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "MatchStar",
        "pattern",
        &[("name", "str | None")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "MatchValue",
        "pattern",
        &[("value", "expr")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    ("Mod", "operator", &[], &[]),
    (
        "Module",
        "mod",
        &[
            ("body", "list[stmt]"),
            ("type_ignores", "list[type_ignore]"),
        ],
        &[],
    ),
    ("Mult", "operator", &[], &[]),
    (
        "Name",
        "expr",
        &[("id", "str"), ("ctx", "expr_context")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "NamedExpr",
        "expr",
        &[("target", "expr"), ("value", "expr")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Nonlocal",
        "stmt",
        &[("names", "list[str]")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    ("Not", "unaryop", &[], &[]),
    ("NotEq", "cmpop", &[], &[]),
    ("NotIn", "cmpop", &[], &[]),
    ("Or", "boolop", &[], &[]),
    (
        "ParamSpec",
        "type_param",
        &[("name", "str"), ("default_value", "expr | None")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Pass",
        "stmt",
        &[],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    ("Pow", "operator", &[], &[]),
    ("RShift", "operator", &[], &[]),
    (
        "Raise",
        "stmt",
        &[("exc", "expr | None"), ("cause", "expr | None")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Return",
        "stmt",
        &[("value", "expr | None")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Set",
        "expr",
        &[("elts", "list[expr]")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "SetComp",
        "expr",
        &[("elt", "expr"), ("generators", "list[comprehension]")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Slice",
        "expr",
        &[
            ("lower", "expr | None"),
            ("upper", "expr | None"),
            ("step", "expr | None"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Starred",
        "expr",
        &[("value", "expr"), ("ctx", "expr_context")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    ("Store", "expr_context", &[], &[]),
    ("Sub", "operator", &[], &[]),
    (
        "Subscript",
        "expr",
        &[
            ("value", "expr"),
            ("slice", "expr"),
            ("ctx", "expr_context"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "TemplateStr",
        "expr",
        &[("values", "list[expr]")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Try",
        "stmt",
        &[
            ("body", "list[stmt]"),
            ("handlers", "list[excepthandler]"),
            ("orelse", "list[stmt]"),
            ("finalbody", "list[stmt]"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "TryStar",
        "stmt",
        &[
            ("body", "list[stmt]"),
            ("handlers", "list[excepthandler]"),
            ("orelse", "list[stmt]"),
            ("finalbody", "list[stmt]"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Tuple",
        "expr",
        &[("elts", "list[expr]"), ("ctx", "expr_context")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "TypeAlias",
        "stmt",
        &[
            ("name", "expr"),
            ("type_params", "list[type_param]"),
            ("value", "expr"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "TypeIgnore",
        "type_ignore",
        &[("lineno", "int"), ("tag", "str")],
        &[],
    ),
    (
        "TypeVar",
        "type_param",
        &[
            ("name", "str"),
            ("bound", "expr | None"),
            ("default_value", "expr | None"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "TypeVarTuple",
        "type_param",
        &[("name", "str"), ("default_value", "expr | None")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    ("UAdd", "unaryop", &[], &[]),
    ("USub", "unaryop", &[], &[]),
    (
        "UnaryOp",
        "expr",
        &[("op", "unaryop"), ("operand", "expr")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "While",
        "stmt",
        &[
            ("test", "expr"),
            ("body", "list[stmt]"),
            ("orelse", "list[stmt]"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "With",
        "stmt",
        &[
            ("items", "list[withitem]"),
            ("body", "list[stmt]"),
            ("type_comment", "str | None"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "Yield",
        "expr",
        &[("value", "expr | None")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "YieldFrom",
        "expr",
        &[("value", "expr")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "alias",
        "AST",
        &[("name", "str"), ("asname", "str | None")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "arg",
        "AST",
        &[
            ("arg", "str"),
            ("annotation", "expr | None"),
            ("type_comment", "str | None"),
        ],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "arguments",
        "AST",
        &[
            ("posonlyargs", "list[arg]"),
            ("args", "list[arg]"),
            ("vararg", "arg | None"),
            ("kwonlyargs", "list[arg]"),
            ("kw_defaults", "list[expr]"),
            ("kwarg", "arg | None"),
            ("defaults", "list[expr]"),
        ],
        &[],
    ),
    ("boolop", "AST", &[], &[]),
    ("cmpop", "AST", &[], &[]),
    (
        "comprehension",
        "AST",
        &[
            ("target", "expr"),
            ("iter", "expr"),
            ("ifs", "list[expr]"),
            ("is_async", "int"),
        ],
        &[],
    ),
    (
        "excepthandler",
        "AST",
        &[],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "expr",
        "AST",
        &[],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    ("expr_context", "AST", &[], &[]),
    (
        "keyword",
        "AST",
        &[("arg", "str | None"), ("value", "expr")],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "match_case",
        "AST",
        &[
            ("pattern", "pattern"),
            ("guard", "expr | None"),
            ("body", "list[stmt]"),
        ],
        &[],
    ),
    ("mod", "AST", &[], &[]),
    ("operator", "AST", &[], &[]),
    (
        "pattern",
        "AST",
        &[],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    (
        "stmt",
        "AST",
        &[],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    ("type_ignore", "AST", &[], &[]),
    (
        "type_param",
        "AST",
        &[],
        &["lineno", "col_offset", "end_lineno", "end_col_offset"],
    ),
    ("unaryop", "AST", &[], &[]),
    (
        "withitem",
        "AST",
        &[("context_expr", "expr"), ("optional_vars", "expr | None")],
        &[],
    ),
];

/// The Python source defining the node hierarchy. Generated from [`AST_NODES`]
/// so the two can never disagree about a field name.
///
/// `AST.__init__` is `ast_type_init` and `AST.__repr__` is `ast_repr` from
/// CPython 3.14's `Python/Python-ast.c`, line for line: positional arguments
/// bind to `_fields`, a field given twice is a `TypeError`, a keyword that is
/// neither a field nor an attribute warns, and every field left unset gets the
/// default its `_field_types` entry implies — nothing for an optional field
/// (the class carries a `None` default), `[]` for a sequence, `Load()` for an
/// `expr_context`, and a `DeprecationWarning` for a missing required one. The
/// repr reads every field with `getattr`, so an optional field's class-level
/// `None` shows (`Constant(value=1, kind=None)`), nests at most three nodes
/// deep, and elides the middle of a sequence longer than two.
pub fn module_source() -> String {
    let mut s = String::with_capacity(32 * 1024);
    s.push_str(
        r#""""Abstract syntax tree node types (generated from the ASDL grammar)."""

class AST:
    """Base of every AST node."""

    __module__ = 'ast'
    _fields = ()
    _attributes = ()
    __match_args__ = ()

    def __init__(self, *args, **kwargs):
        cls = type(self)
        fields = cls._fields
        numfields = len(fields)
        if numfields < len(args):
            raise TypeError(
                f'{cls.__name__} constructor takes at most '
                f'{numfields} positional argument{"" if numfields == 1 else "s"}'
            )
        remaining = list(fields)
        for name, value in zip(fields, args):
            setattr(self, name, value)
            remaining.remove(name)
        for key, value in kwargs.items():
            if key in fields:
                if key not in remaining:
                    raise TypeError(
                        f'{cls.__name__} got multiple values for argument {key!r}'
                    )
                remaining.remove(key)
            elif key not in cls._attributes:
                import warnings
                warnings.warn(
                    f'{cls.__name__}.__init__ got an unexpected keyword argument '
                    f'{key!r}. Support for arbitrary keyword arguments is '
                    f'deprecated and will be removed in Python 3.15.',
                    DeprecationWarning,
                    stacklevel=2,
                )
            setattr(self, key, value)
        if not remaining:
            return
        # A user subclass of `AST` that declares no `_field_types` keeps the
        # pre-3.13 behavior: an unpassed field simply does not exist.
        field_types = getattr(cls, '_field_types', None)
        if field_types is None:
            return
        for name in remaining:
            tp = field_types[name]
            if isinstance(tp, _UnionType):
                # Optional: the class already carries the `None` default.
                pass
            elif isinstance(tp, _GenericAlias):
                setattr(self, name, [])
            elif tp is expr_context:
                setattr(self, name, _Load_singleton)
            else:
                import warnings
                warnings.warn(
                    f'{cls.__name__}.__init__ missing 1 required positional '
                    f'argument: {name!r}. This will become an error in Python 3.15.',
                    DeprecationWarning,
                    stacklevel=2,
                )

    def __repr__(self):
        return _repr_max_depth(self, 3)

    def __eq__(self, other):
        if type(self) is not type(other):
            return NotImplemented
        for name in self._fields:
            if getattr(self, name, None) != getattr(other, name, None):
                return False
        return True

    def __hash__(self):
        return hash(type(self).__name__)


_UnionType = type(int | None)
_GenericAlias = type(list[int])

# The nodes whose repr is in progress — `Py_ReprEnter`'s bookkeeping, so a
# node that contains itself renders as `Name(...)` instead of recursing.
_repr_running = set()


def _repr_max_depth(node, depth):
    name = type(node).__name__
    if depth <= 0 or id(node) in _repr_running:
        return f'{name}(...)'
    fields = getattr(type(node), '_fields', ())
    if not fields:
        return f'{name}()'
    _repr_running.add(id(node))
    try:
        parts = []
        for field in fields:
            value = getattr(node, field)
            if isinstance(value, (list, tuple)):
                value_repr = _repr_list(value, depth)
            elif isinstance(value, AST):
                value_repr = _repr_max_depth(value, depth - 1)
            else:
                value_repr = repr(value)
            parts.append(f'{field}={value_repr}')
        return f'{name}({", ".join(parts)})'
    finally:
        _repr_running.discard(id(node))


def _repr_list(seq, depth):
    # `ast_repr_list`: the first and last items, with `...` between them when
    # anything was skipped. A one-item tuple has no trailing comma.
    length = len(seq)
    if length == 0:
        return repr(seq)
    is_list = isinstance(seq, list)
    out = '[' if is_list else '('
    for i in range(min(length, 2)):
        if i > 0:
            out += ', '
        item = seq[0] if i == 0 else seq[length - 1]
        if isinstance(item, AST):
            out += _repr_max_depth(item, depth - 1)
        else:
            out += repr(item)
        if i == 0 and length > 2:
            out += ', ...'
    return out + (']' if is_list else ')')


# Compiler flags `ast.parse` passes to `compile()`.
PyCF_ONLY_AST = 1024
PyCF_TYPE_COMMENTS = 4096
PyCF_ALLOW_TOP_LEVEL_AWAIT = 8192
PyCF_OPTIMIZED_AST = 33792

"#,
    );
    // Emit in dependency order: a class cannot name a base that has not been
    // defined yet, and the table is alphabetical (`Add(operator)` sorts long
    // before `operator(AST)`).
    let mut defined: Vec<&str> = vec!["AST", "object"];
    let mut pending: Vec<&NodeSpec> = AST_NODES.iter().filter(|(n, ..)| *n != "AST").collect();
    let mut ordered: Vec<&NodeSpec> = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        let (ready, rest): (Vec<&NodeSpec>, Vec<&NodeSpec>) = pending
            .into_iter()
            .partition(|(_, base, ..)| defined.contains(base));
        if ready.is_empty() {
            break;
        }
        for node in &ready {
            defined.push(node.0);
        }
        ordered.extend(ready);
        pending = rest;
    }
    // A sum type (`expr`, `stmt`, …) is the base of its constructors and gets
    // no `_field_types`; every constructor does, even a field-less one.
    let is_sum = |name: &str| AST_NODES.iter().any(|(_, base, ..)| *base == name);
    let inherited_attrs = |base: &str| {
        AST_NODES
            .iter()
            .find(|(n, ..)| *n == base)
            .map_or(&[][..], |(.., attrs)| *attrs)
    };
    for &(name, base, fields, attrs) in &ordered {
        s.push_str(&format!("\nclass {name}({base}):\n"));
        // CPython's node types report the public module: `ast.Name.__module__`
        // is `'ast'`, which is also how a `_field_types` entry renders
        // (`list[ast.stmt]`).
        s.push_str("    __module__ = 'ast'\n");
        let names: Vec<&str> = fields.iter().map(|(f, _)| *f).collect();
        s.push_str(&format!("    _fields = {}\n", py_tuple(&names)));
        s.push_str(&format!("    __match_args__ = {}\n", py_tuple(&names)));
        s.push_str(&format!("    _attributes = {}\n", py_tuple(attrs)));
        // An optional (`?`) field or attribute defaults to `None` on the class
        // that declares it, which is what lets `repr` read it unset.
        for (field, tp) in fields.iter() {
            if tp.ends_with("| None") {
                s.push_str(&format!("    {field} = None\n"));
            }
        }
        let inherited = inherited_attrs(base);
        for attr in attrs.iter().filter(|a| !inherited.contains(a)) {
            if matches!(*attr, "end_lineno" | "end_col_offset") {
                s.push_str(&format!("    {attr} = None\n"));
            }
        }
    }
    // `_field_types` names other node classes, so it is bound once all of
    // them exist.
    s.push_str("\n_Load_singleton = Load()\n");
    for &(name, _, fields, _) in ordered.iter().filter(|(n, ..)| !is_sum(n)) {
        let entries: Vec<String> = fields
            .iter()
            .map(|(f, tp)| format!("'{f}': {tp}"))
            .collect();
        s.push_str(&format!(
            "{name}._field_types = {{{}}}\n",
            entries.join(", ")
        ));
    }
    s
}

/// `names` as a Python tuple literal — a one-element tuple needs its trailing
/// comma, or it is not a tuple.
fn py_tuple(names: &[&str]) -> String {
    let quoted: Vec<String> = names.iter().map(|x| format!("'{x}'")).collect();
    let trailing = if names.len() == 1 { "," } else { "" };
    format!("({}{trailing})", quoted.join(", "))
}
