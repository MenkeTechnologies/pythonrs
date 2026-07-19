//! Offline generator for `docs/reference.html` — renders the pythonrs builtin
//! reference as a static page for GitHub Pages. Run before pushing Pages.

use std::fmt::Write as _;

const BUILTINS: &[(&str, &str)] = &[
    ("print(*args, sep=' ', end='\\n')", "Write args to stdout."),
    ("len(x)", "Number of items in a container."),
    ("range(stop) / range(start, stop[, step])", "An arithmetic progression."),
    ("int(x=0, base=10)", "Convert to an integer (arbitrary precision)."),
    ("float(x=0.0)", "Convert to a floating-point number."),
    ("str(x='')", "String form of an object."),
    ("repr(x)", "The canonical string representation."),
    ("list / tuple / set / dict / frozenset", "Container constructors."),
    ("bool(x)", "Truth value of x."),
    ("sum(iterable, start=0)", "Sum of a sequence."),
    ("min / max(iterable, *, key, default)", "Smallest / largest item."),
    ("sorted(iterable, *, key, reverse)", "A new sorted list."),
    ("enumerate(iterable, start=0)", "Index/value pairs."),
    ("zip(*iterables)", "Tuples of parallel items."),
    ("map(func, *iterables)", "Apply func across items."),
    ("filter(func, iterable)", "Items where func is truthy."),
    ("any / all(iterable)", "Boolean fold over truthiness."),
    ("abs / round / divmod / pow", "Numeric helpers."),
    ("type(x) / isinstance(x, cls)", "Runtime type checks."),
    ("hasattr / getattr / setattr", "Attribute access by name."),
    ("ord / chr / hex / oct / bin", "Character/number conversions."),
    ("iter(x) / next(it[, default])", "Iterator protocol."),
    ("input([prompt])", "Read a line from stdin."),
];

fn main() {
    let mut body = String::new();
    for (sig, desc) in BUILTINS {
        let _ = write!(
            body,
            "<tr><td><code>{}</code></td><td>{}</td></tr>\n",
            html_escape(sig),
            html_escape(desc)
        );
    }
    let html = format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
<title>pythonrs reference</title>\
<style>body{{font-family:system-ui,sans-serif;max-width:900px;margin:2rem auto;padding:0 1rem;background:#0d1117;color:#c9d1d9}}\
h1{{color:#58a6ff}}table{{border-collapse:collapse;width:100%}}\
td{{border-bottom:1px solid #30363d;padding:.5rem;vertical-align:top}}\
code{{color:#79c0ff}}</style></head><body>\
<h1>pythonrs builtin reference</h1>\
<p>Python on fusevm — a compiled Python runtime (bytecode VM + Cranelift JIT).</p>\
<table>{body}</table></body></html>"
    );
    print!("{html}");
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
