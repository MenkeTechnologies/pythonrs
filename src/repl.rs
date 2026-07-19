//! Interactive REPL (`python --repl`, or `python` on a TTY).
//!
//! Keeps one persistent host across lines (module globals, defs, classes and
//! imports survive between prompts). Compound statements (those whose header
//! ends in `:`) accumulate lines until a blank line closes the block.

use crate::banner;
use nu_ansi_term::Color;
use reedline::{DefaultPrompt, DefaultPromptSegment, Reedline, Signal};

/// Run the REPL loop.
pub fn run() {
    banner::print_banner();
    crate::host::reset_host();
    let mut line_editor = Reedline::create();
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

fn opens_block(s: &str) -> bool {
    let t = s.trim_end();
    t.ends_with(':')
        || t.ends_with('\\')
        || t.ends_with('(')
        || t.ends_with('[')
        || t.ends_with('{')
}

fn run_line(src: &str) {
    match crate::compile(src) {
        Ok(prog) => match crate::run_compiled(prog) {
            Ok(_) => {}
            Err(e) => eprintln!("{}", Color::Red.paint(e)),
        },
        Err(e) => eprintln!("{}", Color::Red.paint(e)),
    }
}
