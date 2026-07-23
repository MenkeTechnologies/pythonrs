//! Interactive REPL (`python --repl`, or `python` on a TTY).
//!
//! Keeps one persistent host across lines (module globals, defs, classes and
//! imports survive between prompts). Compound statements (those whose header
//! ends in `:`) accumulate lines until a blank line closes the block.
//!
//! Tab pops a `ColumnarMenu` of completions sourced from the LSP corpus
//! (`lsp::corpus` — keywords, builtins, `math.*`, and per-type method names)
//! plus the live module globals / class names from the persistent host. This
//! is the same word corpus the LSP serves; the REPL just adds the interpreter's
//! current bindings on top so freshly-defined names complete immediately.

use crate::banner;
use nu_ansi_term::Color;
use reedline::{
    default_emacs_keybindings, ColumnarMenu, Completer, Emacs, FileBackedHistory, KeyCode,
    KeyModifiers, MenuBuilder, Reedline, ReedlineEvent, ReedlineMenu, Signal, Span, Suggestion,
};
use reedline::{DefaultPrompt, DefaultPromptSegment};

/// History file: `~/.pythonrs/history` (falls back to CWD-relative if `$HOME`
/// is unset). Mirrors stryke's `~/.stryke/history`.
fn history_path() -> std::path::PathBuf {
    let dir = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".pythonrs"))
        .unwrap_or_else(|| std::path::PathBuf::from(".pythonrs"));
    let _ = std::fs::create_dir_all(&dir);
    dir.join("history")
}

/// The static word corpus: every LSP-corpus name (keywords, builtins, `math.*`,
/// str/list/dict/set/tuple/int/float methods), sorted and deduped. Built once at
/// REPL startup — the corpus is `'static`, so this never changes across lines.
fn build_static_words() -> Vec<String> {
    let mut v: Vec<String> = crate::lsp::corpus()
        .iter()
        .map(|(name, ..)| (*name).to_string())
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Byte index of the identifier under the cursor and the incomplete prefix. Word
/// boundaries are any non-identifier char (`.` included, so `math.sq` completes
/// the `sq` segment against bare corpus names like `sqrt`).
fn completion_word_start(line: &str, pos: usize) -> (usize, &str) {
    let pos = pos.min(line.len());
    let before = line.get(..pos).unwrap_or("");
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| !(c.is_alphanumeric() || *c == '_'))
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    (start, line.get(start..pos).unwrap_or(""))
}

/// Reedline completer: static LSP corpus + live host globals/class names,
/// prefix-matched against the identifier at the cursor.
struct PyCompleter {
    static_words: Vec<String>,
}

impl Completer for PyCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let (start, prefix) = completion_word_start(line, pos);
        let span = Span::new(start, pos);

        // Live bindings from the persistent host — same thread as read_line, so
        // a direct `with_host` read is always current (no snapshot needed).
        let dynamic = crate::host::with_host(|h| h.global_names());

        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut out: Vec<Suggestion> = Vec::new();
        for w in self.static_words.iter().chain(dynamic.iter()) {
            if !w.starts_with(prefix) {
                continue;
            }
            if !seen.insert(w.as_str()) {
                continue;
            }
            out.push(Suggestion {
                value: w.clone(),
                description: None,
                style: None,
                extra: None,
                span,
                append_whitespace: false,
                display_override: None,
                match_indices: None,
            });
        }
        out.sort_by(|a, b| a.value.cmp(&b.value));
        out
    }
}

/// Build a reedline editor wired with completion menu, Tab/Shift+Tab bindings,
/// and file-backed history.
fn build_editor() -> Reedline {
    let completer = PyCompleter {
        static_words: build_static_words(),
    };

    let menu = ColumnarMenu::default()
        .with_name("completion_menu")
        .with_columns(4)
        .with_column_padding(2);

    let mut keybindings = default_emacs_keybindings();
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );
    keybindings.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::BackTab,
        ReedlineEvent::MenuPrevious,
    );
    keybindings.add_binding(KeyModifiers::NONE, KeyCode::BackTab, ReedlineEvent::MenuPrevious);

    let editor = Reedline::create()
        .with_completer(Box::new(completer))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
        .with_edit_mode(Box::new(Emacs::new(keybindings)));

    match FileBackedHistory::with_file(5_000, history_path()) {
        Ok(h) => editor.with_history(Box::new(h)),
        // History unavailable (read-only home, CI sandbox): run without it.
        Err(_) => editor,
    }
}

/// Run the REPL loop.
pub fn run() {
    banner::print_banner();
    crate::host::reset_host();
    let mut line_editor = build_editor();
    let prompt = DefaultPrompt::new(
        DefaultPromptSegment::Basic(">>> ".to_string()),
        DefaultPromptSegment::Empty,
    );

    loop {
        match line_editor.read_line(&prompt) {
            Ok(Signal::Success(mut buffer)) => {
                if buffer.trim().is_empty() {
                    continue;
                }
                // Accumulate an indented block if the header opens a suite.
                if opens_block(&buffer) {
                    let cont_prompt = DefaultPrompt::new(
                        DefaultPromptSegment::Basic("... ".to_string()),
                        DefaultPromptSegment::Empty,
                    );
                    while let Ok(Signal::Success(more)) = line_editor.read_line(&cont_prompt) {
                        if more.trim().is_empty() {
                            break;
                        }
                        buffer.push('\n');
                        buffer.push_str(&more);
                    }
                }
                run_line(&buffer);
            }
            Ok(Signal::CtrlC) => continue,
            Ok(Signal::CtrlD) => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
}

/// Non-TTY REPL: read source lines from stdin and evaluate them with the same
/// interactive semantics as the TTY loop (persistent host, block accumulation on
/// an open header, `sys.displayhook` echo of non-`None` top-level expressions).
/// This is the pipe-driven analogue of CPython's `python3 -i < file` — reached
/// only when `--repl` is passed with a non-interactive stdin. No banner and no
/// prompts are emitted (nothing to prompt to), keeping stdout to program output
/// and displayhook echoes only.
pub fn run_piped() {
    use std::io::BufRead;
    crate::host::reset_host();
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines().map_while(Result::ok);
    while let Some(mut buffer) = lines.next() {
        if buffer.trim().is_empty() {
            continue;
        }
        if opens_block(&buffer) {
            for more in lines.by_ref() {
                if more.trim().is_empty() {
                    break;
                }
                buffer.push('\n');
                buffer.push_str(&more);
            }
        }
        run_line(&buffer);
    }
}

fn opens_block(s: &str) -> bool {
    let t = s.trim_end();
    t.ends_with(':')
        || t.ends_with('\\')
        || t.ends_with('(')
        || t.ends_with('[')
        || t.ends_with('{')
}

fn run_line(src: &str) {
    match crate::compile_interactive(src) {
        Ok(prog) => match crate::run_compiled(prog) {
            Ok(_) => {}
            Err(e) => eprintln!("{}", Color::Red.paint(e)),
        },
        Err(e) => eprintln!("{}", Color::Red.paint(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_words_include_keywords_builtins_methods() {
        let v = build_static_words();
        assert!(v.iter().any(|w| w == "def"), "keyword `def` missing");
        assert!(v.iter().any(|w| w == "lambda"), "keyword `lambda` missing");
        assert!(v.iter().any(|w| w == "sqrt"), "math `sqrt` missing");
        assert!(v.iter().any(|w| w == "append"), "list method `append` missing");
    }

    #[test]
    fn word_start_at_bare_identifier() {
        let s = "print(le";
        let (st, pre) = completion_word_start(s, s.len());
        assert_eq!(st, 6);
        assert_eq!(pre, "le");
    }

    #[test]
    fn word_start_snaps_after_dot() {
        let s = "math.sq";
        let (st, pre) = completion_word_start(s, s.len());
        assert_eq!(st, 5);
        assert_eq!(pre, "sq");
    }

    #[test]
    fn word_start_empty_prefix_after_space() {
        let s = "x = ";
        let (st, pre) = completion_word_start(s, s.len());
        assert_eq!(st, 4);
        assert_eq!(pre, "");
    }

    #[test]
    fn completer_prefix_matches_and_dedups() {
        let mut c = PyCompleter {
            static_words: build_static_words(),
        };
        let line = "le";
        let out = c.complete(line, line.len());
        assert!(out.iter().any(|s| s.value == "len"), "`len` not suggested for `le`");
        // Every suggestion must actually start with the prefix.
        assert!(out.iter().all(|s| s.value.starts_with("le")));
        // No duplicate values.
        let mut vals: Vec<&str> = out.iter().map(|s| s.value.as_str()).collect();
        vals.sort();
        let n = vals.len();
        vals.dedup();
        assert_eq!(n, vals.len(), "duplicate suggestions leaked");
    }
}
