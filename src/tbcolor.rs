//! The colours of a traceback printed for an uncaught exception.
//!
//! CPython 3.13+ prints that traceback through `sys.excepthook` with
//! `colorize=_colorize.can_colorize(file=sys.stderr)`: on a terminal (or under
//! `FORCE_COLOR`/`PYTHON_COLORS=1`) the `File` line's parts, the exception's
//! type and message, and the source range the carets mark are wrapped in the
//! `_colorize.Traceback` theme's ANSI codes. `traceback.format_exc()` and the
//! other `traceback` functions never colour (their `colorize` defaults to
//! False), so colouring is switched on only around the top-level report
//! ([`with`]); every formatter here returns the plain text otherwise.

use std::cell::Cell;

/// `_colorize.ANSIColors`, as the `Traceback` theme uses them.
const RESET: &str = "\x1b[0m";
const MAGENTA: &str = "\x1b[35m";
const BOLD_MAGENTA: &str = "\x1b[1;35m";
const RED: &str = "\x1b[31m";
const BOLD_RED: &str = "\x1b[1;31m";

thread_local! {
    static ON: Cell<bool> = const { Cell::new(false) };
}

/// Run `f` with colouring `on`, restoring the previous setting after.
pub fn with<T>(on: bool, f: impl FnOnce() -> T) -> T {
    let prev = ON.with(|c| c.replace(on));
    let r = f();
    ON.with(|c| c.set(prev));
    r
}

fn on() -> bool {
    ON.with(Cell::get)
}

/// `_colorize.can_colorize(file=sys.stderr)`: `PYTHON_COLORS` decides when it
/// is `0` or `1`, then a non-empty `NO_COLOR` refuses and a non-empty
/// `FORCE_COLOR` forces, `TERM=dumb` refuses, and otherwise stderr must be a
/// terminal.
pub fn can_colorize_stderr() -> bool {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    match std::env::var_os("PYTHON_COLORS").as_deref() {
        Some(v) if v == "0" => return false,
        Some(v) if v == "1" => return true,
        _ => {}
    }
    if var("NO_COLOR").is_some() {
        return false;
    }
    if var("FORCE_COLOR").is_some() {
        return true;
    }
    if std::env::var_os("TERM").is_some_and(|t| t == "dumb") {
        return false;
    }
    // SAFETY: `isatty` only inspects the descriptor.
    unsafe { libc::isatty(2) == 1 }
}

/// `  File "f", line N, in name` (`format_frame_summary`), or the
/// `  File "f", line N` of a syntax error when `name` is `None`.
pub fn file_line(filename: &str, lineno: impl std::fmt::Display, name: Option<&str>) -> String {
    let (m, r) = if on() { (MAGENTA, RESET) } else { ("", "") };
    match name {
        Some(name) => {
            format!("  File {m}\"{filename}\"{r}, line {m}{lineno}{r}, in {m}{name}{r}\n")
        }
        None => format!("  File {m}\"{filename}\"{r}, line {m}{lineno}{r}\n"),
    }
}

/// `_format_final_exc_line`: `Type: message`, or a bare `Type`, with the type
/// in bold magenta and the message in magenta.
pub fn final_line(line: &str) -> String {
    if !on() {
        return line.to_string();
    }
    match line.split_once(": ") {
        Some((ty, msg)) => format!("{BOLD_MAGENTA}{ty}{RESET}: {MAGENTA}{msg}{RESET}"),
        None => format!("{BOLD_MAGENTA}{line}{RESET}"),
    }
}

/// A source line and the caret line under it, both already indented: each
/// run of `^` (and the text above it) in bold red, each run of `~` in red, as
/// `format_frame_summary` groups the zipped characters by caret.
pub fn source_and_carets(line: &str, carets: &str) -> (String, String) {
    if !on() {
        return (line.to_string(), carets.to_string());
    }
    let text: Vec<char> = line.chars().collect();
    let marks: Vec<char> = carets.chars().collect();
    let (mut out_line, mut out_carets) = (String::new(), String::new());
    let mut i = 0;
    while i < text.len().max(marks.len()) {
        let mark = marks.get(i).copied();
        let mut j = i;
        while j < text.len().max(marks.len()) && marks.get(j).copied() == mark {
            j += 1;
        }
        let chars: String = text
            .get(i..j.min(text.len()))
            .unwrap_or(&[])
            .iter()
            .collect();
        let run: String = marks
            .get(i..j.min(marks.len()))
            .unwrap_or(&[])
            .iter()
            .collect();
        match mark.and_then(|c| match c {
            '^' => Some(BOLD_RED),
            '~' => Some(RED),
            _ => None,
        }) {
            Some(color) => {
                out_line.push_str(&format!("{color}{chars}{RESET}"));
                out_carets.push_str(&format!("{color}{run}{RESET}"));
            }
            None => {
                out_line.push_str(&chars);
                out_carets.push_str(&run);
            }
        }
        i = j;
    }
    (out_line, out_carets)
}

/// The source line of a syntax error with `text[col..end]` in bold red, and
/// the carets under it (`_format_syntax_error`).
pub fn syntax_source(ltext: &str, col: usize, end: usize, lead: &str, carets: &str) -> String {
    if !on() {
        return format!("    {ltext}\n    {lead}{carets}\n");
    }
    let chars: Vec<char> = ltext.chars().collect();
    let take = |a: usize, b: usize| -> String {
        chars
            .get(a.min(chars.len())..b.min(chars.len()))
            .unwrap_or(&[])
            .iter()
            .collect()
    };
    format!(
        "    {}{BOLD_RED}{}{RESET}{}\n    {lead}{BOLD_RED}{carets}{RESET}\n",
        take(0, col),
        take(col, end),
        take(end, chars.len())
    )
}

/// Whether colouring is on, for a CPython-side formatter that takes
/// `colorize=`.
pub fn enabled() -> bool {
    on()
}
