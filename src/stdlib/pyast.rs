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
//! the semantics (`__init__` binding positional args to `_fields`, `__repr__`
//! naming only the fields that were set) written in Python where they belong.

/// Every `_ast` node type: `(name, base, fields, attributes)`, transcribed
/// from CPython's ASDL-generated module. `_fields` drives construction and
/// `ast.iter_fields`; `_attributes` are the source-position slots every
/// statement and expression carries.
pub const AST_NODES: &[(&str, &str, &[&str], &[&str])] = &[
    ("AST", "object", &[], &[]),
    ("Add", "operator", &[], &[]),
    ("And", "boolop", &[], &[]),
    ("AnnAssign", "stmt", &["target", "annotation", "value", "simple"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Assert", "stmt", &["test", "msg"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Assign", "stmt", &["targets", "value", "type_comment"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("AsyncFor", "stmt", &["target", "iter", "body", "orelse", "type_comment"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("AsyncFunctionDef", "stmt", &["name", "args", "body", "decorator_list", "returns", "type_comment", "type_params"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("AsyncWith", "stmt", &["items", "body", "type_comment"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Attribute", "expr", &["value", "attr", "ctx"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("AugAssign", "stmt", &["target", "op", "value"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Await", "expr", &["value"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("BinOp", "expr", &["left", "op", "right"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("BitAnd", "operator", &[], &[]),
    ("BitOr", "operator", &[], &[]),
    ("BitXor", "operator", &[], &[]),
    ("BoolOp", "expr", &["op", "values"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Break", "stmt", &[], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Call", "expr", &["func", "args", "keywords"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("ClassDef", "stmt", &["name", "bases", "keywords", "body", "decorator_list", "type_params"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Compare", "expr", &["left", "ops", "comparators"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Constant", "expr", &["value", "kind"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Continue", "stmt", &[], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Del", "expr_context", &[], &[]),
    ("Delete", "stmt", &["targets"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Dict", "expr", &["keys", "values"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("DictComp", "expr", &["key", "value", "generators"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Div", "operator", &[], &[]),
    ("Eq", "cmpop", &[], &[]),
    ("ExceptHandler", "excepthandler", &["type", "name", "body"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Expr", "stmt", &["value"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Expression", "mod", &["body"], &[]),
    ("FloorDiv", "operator", &[], &[]),
    ("For", "stmt", &["target", "iter", "body", "orelse", "type_comment"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("FormattedValue", "expr", &["value", "conversion", "format_spec"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("FunctionDef", "stmt", &["name", "args", "body", "decorator_list", "returns", "type_comment", "type_params"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("FunctionType", "mod", &["argtypes", "returns"], &[]),
    ("GeneratorExp", "expr", &["elt", "generators"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Global", "stmt", &["names"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Gt", "cmpop", &[], &[]),
    ("GtE", "cmpop", &[], &[]),
    ("If", "stmt", &["test", "body", "orelse"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("IfExp", "expr", &["test", "body", "orelse"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Import", "stmt", &["names"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("ImportFrom", "stmt", &["module", "names", "level"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("In", "cmpop", &[], &[]),
    ("Interactive", "mod", &["body"], &[]),
    ("Interpolation", "expr", &["value", "str", "conversion", "format_spec"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Invert", "unaryop", &[], &[]),
    ("Is", "cmpop", &[], &[]),
    ("IsNot", "cmpop", &[], &[]),
    ("JoinedStr", "expr", &["values"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("LShift", "operator", &[], &[]),
    ("Lambda", "expr", &["args", "body"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("List", "expr", &["elts", "ctx"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("ListComp", "expr", &["elt", "generators"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Load", "expr_context", &[], &[]),
    ("Lt", "cmpop", &[], &[]),
    ("LtE", "cmpop", &[], &[]),
    ("MatMult", "operator", &[], &[]),
    ("Match", "stmt", &["subject", "cases"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("MatchAs", "pattern", &["pattern", "name"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("MatchClass", "pattern", &["cls", "patterns", "kwd_attrs", "kwd_patterns"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("MatchMapping", "pattern", &["keys", "patterns", "rest"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("MatchOr", "pattern", &["patterns"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("MatchSequence", "pattern", &["patterns"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("MatchSingleton", "pattern", &["value"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("MatchStar", "pattern", &["name"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("MatchValue", "pattern", &["value"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Mod", "operator", &[], &[]),
    ("Module", "mod", &["body", "type_ignores"], &[]),
    ("Mult", "operator", &[], &[]),
    ("Name", "expr", &["id", "ctx"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("NamedExpr", "expr", &["target", "value"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Nonlocal", "stmt", &["names"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Not", "unaryop", &[], &[]),
    ("NotEq", "cmpop", &[], &[]),
    ("NotIn", "cmpop", &[], &[]),
    ("Or", "boolop", &[], &[]),
    ("ParamSpec", "type_param", &["name", "default_value"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Pass", "stmt", &[], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Pow", "operator", &[], &[]),
    ("RShift", "operator", &[], &[]),
    ("Raise", "stmt", &["exc", "cause"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Return", "stmt", &["value"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Set", "expr", &["elts"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("SetComp", "expr", &["elt", "generators"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Slice", "expr", &["lower", "upper", "step"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Starred", "expr", &["value", "ctx"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Store", "expr_context", &[], &[]),
    ("Sub", "operator", &[], &[]),
    ("Subscript", "expr", &["value", "slice", "ctx"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("TemplateStr", "expr", &["values"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Try", "stmt", &["body", "handlers", "orelse", "finalbody"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("TryStar", "stmt", &["body", "handlers", "orelse", "finalbody"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Tuple", "expr", &["elts", "ctx"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("TypeAlias", "stmt", &["name", "type_params", "value"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("TypeIgnore", "type_ignore", &["lineno", "tag"], &[]),
    ("TypeVar", "type_param", &["name", "bound", "default_value"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("TypeVarTuple", "type_param", &["name", "default_value"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("UAdd", "unaryop", &[], &[]),
    ("USub", "unaryop", &[], &[]),
    ("UnaryOp", "expr", &["op", "operand"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("While", "stmt", &["test", "body", "orelse"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("With", "stmt", &["items", "body", "type_comment"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("Yield", "expr", &["value"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("YieldFrom", "expr", &["value"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("alias", "AST", &["name", "asname"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("arg", "AST", &["arg", "annotation", "type_comment"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("arguments", "AST", &["posonlyargs", "args", "vararg", "kwonlyargs", "kw_defaults", "kwarg", "defaults"], &[]),
    ("boolop", "AST", &[], &[]),
    ("cmpop", "AST", &[], &[]),
    ("comprehension", "AST", &["target", "iter", "ifs", "is_async"], &[]),
    ("excepthandler", "AST", &[], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("expr", "AST", &[], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("expr_context", "AST", &[], &[]),
    ("keyword", "AST", &["arg", "value"], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("match_case", "AST", &["pattern", "guard", "body"], &[]),
    ("mod", "AST", &[], &[]),
    ("operator", "AST", &[], &[]),
    ("pattern", "AST", &[], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("stmt", "AST", &[], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("type_ignore", "AST", &[], &[]),
    ("type_param", "AST", &[], &["lineno", "col_offset", "end_lineno", "end_col_offset"]),
    ("unaryop", "AST", &[], &[]),
    ("withitem", "AST", &["context_expr", "optional_vars"], &[]),
];

/// The Python source defining the node hierarchy. Generated from [`AST_NODES`]
/// so the two can never disagree about a field name.
pub fn module_source() -> String {
    let mut s = String::with_capacity(24 * 1024);
    s.push_str(
        r#""""Abstract syntax tree node types (generated from the ASDL grammar)."""

class AST:
    """Base of every AST node."""

    _fields = ()
    _attributes = ()

    def __init__(self, *args, **kwargs):
        # Positional arguments bind to `_fields` in order, exactly as CPython's
        # generated constructors do; anything else is a keyword. A field left
        # unset stays unset — `ast.Name(id='x')` has no `ctx`, and code checks
        # for that with `hasattr`.
        if len(args) > len(self._fields):
            raise TypeError(
                f'{type(self).__name__} constructor takes at most '
                f'{len(self._fields)} positional arguments'
            )
        for name, value in zip(self._fields, args):
            setattr(self, name, value)
        for key, value in kwargs.items():
            setattr(self, key, value)

    def __repr__(self):
        parts = []
        for name in self._fields:
            if hasattr(self, name):
                parts.append(f'{name}={getattr(self, name)!r}')
        return f'{type(self).__name__}({", ".join(parts)})'

    def __eq__(self, other):
        if type(self) is not type(other):
            return NotImplemented
        for name in self._fields:
            if getattr(self, name, None) != getattr(other, name, None):
                return False
        return True

    def __hash__(self):
        return hash(type(self).__name__)


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
    let mut pending: Vec<&(&str, &str, &[&str], &[&str])> =
        AST_NODES.iter().filter(|(n, ..)| *n != "AST").collect();
    let mut ordered: Vec<&(&str, &str, &[&str], &[&str])> = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        let (ready, rest): (
            Vec<&(&str, &str, &[&str], &[&str])>,
            Vec<&(&str, &str, &[&str], &[&str])>,
        ) = pending
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
    for (name, base, fields, attrs) in ordered {
        s.push_str(&format!("\nclass {name}({base}):\n"));
        let f: Vec<String> = fields.iter().map(|x| format!("'{x}'")).collect();
        let a: Vec<String> = attrs.iter().map(|x| format!("'{x}'")).collect();
        s.push_str(&format!("    _fields = ({}{})\n", f.join(", "), trailing(fields)));
        s.push_str(&format!(
            "    _attributes = ({}{})\n",
            a.join(", "),
            trailing(attrs)
        ));
    }
    s
}

/// A one-element Python tuple needs its trailing comma, or it is not a tuple.
fn trailing(items: &[&str]) -> &'static str {
    if items.len() == 1 { "," } else { "" }
}
