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
        /// The field's source text, verbatim. f-strings ignore it; a t-string
        /// exposes it as `Interpolation.expression` (PEP 750), which is the whole
        /// point of a template — the consumer sees what was written, not just the
        /// value it evaluated to.
        src: String,
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
    /// `name=value` (or `**value`) as a `SyntaxError` reports it — `(lineno,
    /// offset, end_lineno, end_offset)`, 1-based, exclusive end, in characters.
    /// `None` for a synthetic keyword.
    pub span: Option<(u32, u32, u32, u32)>,
}

/// A source span for traceback carets: character columns within a 1-based
/// `line`. `line == 0` marks "no span" (synthetic/desugared nodes). When
/// `anchor_end > anchor_start`, the `[anchor_start, anchor_end)` sub-range
/// renders the secondary caret `^` and the rest of `[start, end)` renders the
/// primary caret `~` — CPython's `~^~` binary-op and `~~~^^^` subscript/call
/// anchoring. With no anchor the whole `[start, end)` renders `^`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Span {
    pub line: u32,
    /// The line the node ends on: `line` for every caret span. Only the
    /// spans that locate a compile-time error (a misplaced `yield`/`await`,
    /// a comprehension, an assignment expression) may run past `line`.
    pub end_line: u32,
    pub start: u32,
    pub end: u32,
    pub anchor_start: u32,
    pub anchor_end: u32,
    /// This op is the direct call value of an `x = f(...)` / `return f(...)`
    /// statement, whose caret CPython suppresses when the call raises.
    pub suppress: bool,
}

impl Span {
    pub const NONE: Span = Span {
        line: 0,
        end_line: 0,
        start: 0,
        end: 0,
        anchor_start: 0,
        anchor_end: 0,
        suppress: false,
    };
    pub fn is_some(&self) -> bool {
        self.line != 0
    }
    /// A span within one line, which is what a traceback caret can draw.
    pub fn is_one_line(&self) -> bool {
        self.end_line == self.line
    }
    pub fn has_anchor(&self) -> bool {
        self.anchor_end > self.anchor_start
    }
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
    /// PEP 750 `t"..."` — evaluates to a `string.templatelib.Template`, not a str.
    TString(Vec<FStrPart>),

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

    /// `lambda params: body`. The parameter list is boxed: inline it is the
    /// largest payload by far and would set every `Expr`'s size, and with it the
    /// stack frame of every function that holds one.
    Lambda {
        params: Box<Params>,
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

    /// An expression carrying its source span, attached by the parser to the
    /// caret-bearing forms (name load, binary op, subscript, call, attribute,
    /// unary op). The compiler peels it and records the span for the raising op;
    /// every other consumer treats `Spanned(e, _)` as `e` via `Expr::unspanned`.
    Spanned(Box<Expr>, Span),
}

impl Expr {
    /// Peel any `Spanned` wrapper(s), returning the underlying expression. Used
    /// wherever code matches on expression structure (assignment targets, walrus
    /// targets, constant folding) so a wrapped node is treated as its inner form.
    pub fn unspanned(&self) -> &Expr {
        let mut e = self;
        while let Expr::Spanned(inner, _) = e {
            e = inner;
        }
        e
    }
    /// The span wrapping this expression, or `Span::NONE`.
    pub fn span(&self) -> Span {
        match self {
            Expr::Spanned(_, s) => *s,
            _ => Span::NONE,
        }
    }
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
    /// Parameter/return annotations in source order, as `(name, expr)` pairs
    /// (the return annotation uses the name `"return"`). Evaluated at def time to
    /// build the function's `__annotations__` dict. Empty for a `lambda` or an
    /// unannotated `def`.
    pub annotations: Vec<(String, Expr)>,
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
    /// `type Name[params] = value` (PEP 695). `value` is evaluated lazily, on
    /// the first read of the alias's `__value__`.
    TypeAlias {
        name: String,
        params: Vec<TypeParam>,
        value: Expr,
    },
}

/// One PEP 695 type parameter: `T`, `*Ts` or `**P`.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeParam {
    pub name: String,
    pub kind: TypeParamKind,
}

/// Which `typing` object a [`TypeParam`] creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeParamKind {
    /// `T` — a `TypeVar`.
    TypeVar,
    /// `*Ts` — a `TypeVarTuple`.
    TypeVarTuple,
    /// `**P` — a `ParamSpec`.
    ParamSpec,
}

/// One `case pattern [if guard]: body` of a `match`.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchCase {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Vec<Stmt>,
}

/// A `match` pattern (PEP 634): its shape and where it sits in the source.
#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    pub kind: PatternKind,
    pub loc: Loc,
}

/// A node's extent as CPython's AST records it: 1-based lines and 0-based
/// UTF-8 BYTE columns, end exclusive (`lineno`, `col_offset`, `end_lineno`,
/// `end_col_offset`). The compiler raises its pattern `SyntaxError`s at this
/// extent, and `compiler_error` reports `col_offset + 1` — a byte column — as
/// the exception's `offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Loc {
    pub lineno: u32,
    pub col_offset: u32,
    pub end_lineno: u32,
    pub end_col_offset: u32,
}

/// The shape of a [`Pattern`].
#[derive(Debug, Clone, PartialEq)]
pub enum PatternKind {
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
    /// The statement's extent as a `SyntaxError` reports it — `(lineno,
    /// offset, end_lineno, end_offset)`, 1-based with an exclusive end, in
    /// characters. Recorded only for the statements CPython's symbol table or
    /// compiler points an error at: `global` / `nonlocal` (see `symtable.rs`)
    /// and `break` / `continue` / `return` (an escape from an `except*` block).
    pub span: Option<(u32, u32, u32, u32)>,
}

impl Stmt {
    pub fn new(kind: StmtKind, line: u32) -> Stmt {
        Stmt {
            kind,
            line,
            span: None,
        }
    }
}

impl From<StmtKind> for Stmt {
    /// Wrap a `StmtKind` as a synthetic statement (line 0). Used for desugared
    /// bodies with no source line; the debug marker skips line-0 statements so
    /// they never become spurious breakpoint targets.
    fn from(kind: StmtKind) -> Stmt {
        Stmt {
            kind,
            line: 0,
            span: None,
        }
    }
}

/// Dropping an expression is a loop, not a recursion.
///
/// The parser builds a left-recursive chain — `a.b.c…`, `1+1+1…`, `f()()…`,
/// `a[0][0]…` — in a loop, exactly as pegen's left-recursion does, so nothing
/// bounds its depth at parse time: CPython bounds it at compile time instead,
/// where the first walk that runs out of stack raises `RecursionError` (see
/// [`crate::stack`]). The derived drop glue would then recurse once per link
/// and abort on the very tree whose compile just failed cleanly. Each boxed
/// child is moved onto a worklist and replaced by a leaf, so the depth of the
/// drop is one frame whatever the depth of the tree.
///
/// Only boxed children are taken. A child held in a `Vec` (a call's arguments,
/// a display's elements, a comprehension's clauses) is dropped by the vector,
/// which runs this same impl on it; the nesting that reaches through vectors is
/// bounded by the tokenizer's 200-bracket limit.
impl Drop for Expr {
    fn drop(&mut self) {
        let mut pending = Vec::new();
        self.take_boxed_children(&mut pending);
        while let Some(mut child) = pending.pop() {
            child.take_boxed_children(&mut pending);
        }
    }
}

impl Expr {
    /// Move every boxed child into `out`, leaving `Expr::None` in its place.
    fn take_boxed_children(&mut self, out: &mut Vec<Expr>) {
        let mut take = |b: &mut Box<Expr>| {
            if !matches!(**b, Expr::None) {
                out.push(std::mem::replace(&mut **b, Expr::None));
            }
        };
        match self {
            Expr::Starred(x)
            | Expr::UnaryOp(_, x)
            | Expr::Attribute(x, _)
            | Expr::YieldFrom(x)
            | Expr::Await(x)
            | Expr::Spanned(x, _)
            | Expr::Compare(x, _)
            | Expr::Call { func: x, .. }
            | Expr::Lambda { body: x, .. }
            | Expr::ListComp(x, _)
            | Expr::SetComp(x, _)
            | Expr::GenExp(x, _)
            | Expr::Yield(Some(x)) => take(x),
            Expr::BinOp(_, a, b)
            | Expr::Subscript(a, b)
            | Expr::NamedExpr(a, b)
            | Expr::DictComp(a, b, _) => {
                take(a);
                take(b);
            }
            Expr::IfExp { test, body, orelse } => {
                take(test);
                take(body);
                take(orelse);
            }
            Expr::Slice { lo, hi, step } => {
                for b in [lo, hi, step].into_iter().flatten() {
                    take(b);
                }
            }
            _ => {}
        }
    }
}
