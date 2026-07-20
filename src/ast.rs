//! Python abstract syntax tree.
//!
//! Faithful to CPython's surface grammar as far as pythonrs lowers it today:
//! every node here has a direct lowering in `compiler.rs`. Unlike Ruby, Python
//! is statement-oriented — most control flow is a `Stmt` that yields no value —
//! so the tree separates `Stmt` (blocks of these form suites) from `Expr`.

/// A binary arithmetic/bit operator (Python `a <op> b`). Comparison and boolean
/// operators are separate (`Compare`, `BoolOp`) because Python chains them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,      // `/` — true division (always float in Python 3)
    FloorDiv, // `//`
    Mod,      // `%`
    Pow,      // `**`
    MatMul,   // `@`
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

/// A boolean short-circuit operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoolOp {
    And,
    Or,
}

/// A comparison operator (one link of a `Compare` chain).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Is,
    IsNot,
    In,
    NotIn,
}

/// A unary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,    // -x
    Pos,    // +x
    Not,    // not x
    Invert, // ~x
}

/// A formatted-string segment (`f"..."`).
#[derive(Debug, Clone, PartialEq)]
pub enum FStrPart {
    Lit(String),
    /// `{expr!conv:spec}` — `conv` is 's'/'r'/'a' or none. `spec` is the format
    /// spec parsed as its own mini joined-string: an empty vec means no spec,
    /// literal text is a `Lit`, and a nested replacement field (`{w}` in
    /// `{x:{w}.2f}`) is an `Expr` evaluated at runtime and spliced into the spec.
    Expr {
        expr: Box<Expr>,
        conv: Option<char>,
        spec: Vec<FStrPart>,
    },
}

/// One `(target, iter, ifs)` clause of a comprehension.
#[derive(Debug, Clone, PartialEq)]
pub struct Comprehension {
    pub target: Box<Expr>,
    pub iter: Box<Expr>,
    pub ifs: Vec<Expr>,
    /// `async for` clause (an asynchronous comprehension), driven via `__anext__`.
    pub is_async: bool,
}

/// A keyword argument at a call site: `name=value`, or `**mapping` when `name`
/// is `None`.
#[derive(Debug, Clone, PartialEq)]
pub struct Keyword {
    pub name: Option<String>,
    pub value: Expr,
}

/// A Python expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    None,
    True,
    False,
    Ellipsis,
    Int(i64),
    /// An integer literal too wide for `i64` (kept as text; host promotes it).
    BigInt(String),
    Float(f64),
    Complex(f64),
    Str(String),
    Bytes(Vec<u8>),
    FString(Vec<FStrPart>),

    /// A bare name (`x`); the compiler resolves scope (LEGB) at runtime.
    Name(String),

    List(Vec<Expr>),
    Tuple(Vec<Expr>),
    Set(Vec<Expr>),
    /// key/value pairs; a `None` key is a `**mapping` spread.
    Dict(Vec<(Option<Expr>, Expr)>),

    /// `*expr` — a starred element (call arg / assignment target / iterable
    /// unpack).
    Starred(Box<Expr>),

    BoolOp(BoolOp, Vec<Expr>),
    UnaryOp(UnOp, Box<Expr>),
    BinOp(BinOp, Box<Expr>, Box<Expr>),
    /// `a < b <= c` — a chained comparison: left plus (op, rhs) links.
    Compare(Box<Expr>, Vec<(CmpOp, Expr)>),

    /// `body if test else orelse`.
    IfExp {
        test: Box<Expr>,
        body: Box<Expr>,
        orelse: Box<Expr>,
    },

    /// A call `func(args, keywords)`.
    Call {
        func: Box<Expr>,
        args: Vec<Expr>,
        keywords: Vec<Keyword>,
    },
    /// `value.attr`.
    Attribute(Box<Expr>, String),
    /// `value[slice]`.
    Subscript(Box<Expr>, Box<Expr>),
    /// `lo:hi:step` inside a subscript. Any bound may be absent.
    Slice {
        lo: Option<Box<Expr>>,
        hi: Option<Box<Expr>>,
        step: Option<Box<Expr>>,
    },

    /// `lambda params: body`.
    Lambda {
        params: Params,
        body: Box<Expr>,
    },

    ListComp(Box<Expr>, Vec<Comprehension>),
    SetComp(Box<Expr>, Vec<Comprehension>),
    /// `{k: v for ...}`.
    DictComp(Box<Expr>, Box<Expr>, Vec<Comprehension>),
    GenExp(Box<Expr>, Vec<Comprehension>),

    /// `yield expr` / `yield` (None) as an expression.
    Yield(Option<Box<Expr>>),
    YieldFrom(Box<Expr>),
    /// `await expr`.
    Await(Box<Expr>),

    /// `:=` walrus in an expression context.
    NamedExpr(Box<Expr>, Box<Expr>),
}

/// A formal-parameter list for a `def`/`lambda`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Params {
    /// Positional-or-keyword parameter names, in order.
    pub names: Vec<String>,
    /// Default expressions for the trailing `defaults.len()` positional params.
    pub defaults: Vec<Expr>,
    /// Count of leading positional-only params (before `/`).
    pub posonly: usize,
    /// `*args` collector name, if any (bare `*` records `Some("")` to open the
    /// keyword-only section without collecting).
    pub star: Option<String>,
    /// Keyword-only parameter names (after `*`).
    pub kwonly: Vec<String>,
    /// Defaults for keyword-only params (`None` = required).
    pub kwonly_defaults: Vec<Option<Expr>>,
    /// `**kwargs` collector name, if any.
    pub kwargs: Option<String>,
}

/// One `except` clause of a `try`.
#[derive(Debug, Clone, PartialEq)]
pub struct ExceptHandler {
    /// The exception type expression(s); `None` is a bare `except:`.
    pub typ: Option<Expr>,
    /// `as name` binding.
    pub name: Option<String>,
    pub body: Vec<Stmt>,
    /// `except*` (exception groups).
    pub star: bool,
}

/// One `with` item: `context_expr [as optional_vars]`.
#[derive(Debug, Clone, PartialEq)]
pub struct WithItem {
    pub context: Expr,
    pub vars: Option<Expr>,
}

/// A Python statement.
#[derive(Debug, Clone, PartialEq)]
pub enum StmtKind {
    /// An expression evaluated for effect (its value is discarded, except at the
    /// REPL top level).
    Expr(Expr),
    /// `targets... = value` (chained assignment: `a = b = expr`).
    Assign {
        targets: Vec<Expr>,
        value: Expr,
    },
    /// `target op= value`.
    AugAssign {
        target: Expr,
        op: BinOp,
        value: Expr,
    },
    /// `target: annotation [= value]`.
    AnnAssign {
        target: Expr,
        annotation: Expr,
        value: Option<Expr>,
    },

    If {
        test: Expr,
        body: Vec<Stmt>,
        orelse: Vec<Stmt>,
    },
    While {
        test: Expr,
        body: Vec<Stmt>,
        orelse: Vec<Stmt>,
    },
    For {
        target: Expr,
        iter: Expr,
        body: Vec<Stmt>,
        orelse: Vec<Stmt>,
        is_async: bool,
    },
    With {
        items: Vec<WithItem>,
        body: Vec<Stmt>,
        is_async: bool,
    },

    FuncDef {
        name: String,
        params: Params,
        body: Vec<Stmt>,
        decorators: Vec<Expr>,
        is_async: bool,
    },
    ClassDef {
        name: String,
        bases: Vec<Expr>,
        keywords: Vec<Keyword>,
        body: Vec<Stmt>,
        decorators: Vec<Expr>,
    },

    Return(Option<Expr>),
    Delete(Vec<Expr>),
    Pass,
    Break,
    Continue,

    Import(Vec<Alias>),
    ImportFrom {
        module: Option<String>,
        names: Vec<Alias>,
        level: usize,
    },

    Global(Vec<String>),
    Nonlocal(Vec<String>),

    Raise {
        exc: Option<Expr>,
        cause: Option<Expr>,
    },
    Try {
        body: Vec<Stmt>,
        handlers: Vec<ExceptHandler>,
        orelse: Vec<Stmt>,
        finalbody: Vec<Stmt>,
    },
    Assert {
        test: Expr,
        msg: Option<Expr>,
    },
    /// `match subject: case ...` — structural pattern matching (Python 3.10).
    Match {
        subject: Expr,
        cases: Vec<MatchCase>,
    },
}

/// One `case pattern [if guard]: body` of a `match`.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchCase {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Vec<Stmt>,
}

/// A `match` pattern (PEP 634).
#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    /// `_` — matches anything, binds nothing.
    Wildcard,
    /// A capture name — matches anything, binds it.
    Capture(String),
    /// A literal or dotted-value pattern (`1`, `"x"`, `None`, `Color.RED`),
    /// matched by `==`.
    Value(Expr),
    /// `p | q | ...` — matches if any alternative matches.
    Or(Vec<Pattern>),
    /// `pattern as name` — matches the sub-pattern and binds `name` to the whole.
    As(Box<Pattern>, String),
    /// `[p, *rest, q]` — a sequence pattern. `star` is the index of the `*` slot.
    Sequence {
        elems: Vec<Pattern>,
        star: Option<usize>,
    },
    /// `*name` / `*_` inside a sequence pattern.
    Star(Option<String>),
    /// `{key: p, ..., **rest}` — a mapping pattern.
    Mapping {
        keys: Vec<(Expr, Pattern)>,
        rest: Option<String>,
    },
    /// `ClassName(pos..., kw=pat...)` — a class pattern.
    Class {
        cls: Expr,
        pos: Vec<Pattern>,
        kw: Vec<(String, Pattern)>,
    },
}

/// An `import` alias: `name [as asname]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Alias {
    pub name: String,
    pub asname: Option<String>,
}

/// A statement plus its 1-based source line (for tracebacks and DAP markers).
#[derive(Debug, Clone, PartialEq)]
pub struct Stmt {
    pub kind: StmtKind,
    pub line: u32,
}

impl Stmt {
    pub fn new(kind: StmtKind, line: u32) -> Stmt {
        Stmt { kind, line }
    }
}

impl From<StmtKind> for Stmt {
    /// Wrap a `StmtKind` as a synthetic statement (line 0). Used for desugared
    /// bodies with no source line; the debug marker skips line-0 statements so
    /// they never become spurious breakpoint targets.
    fn from(kind: StmtKind) -> Stmt {
        Stmt { kind, line: 0 }
    }
}
