//! The `string` standard-library module — the character-class string constants.
//!
//! This module exposes only constants, so [`call`] never claims a name.
//!
//! Wiring (done by the parent): an `import_module` arm for `"string"` calling
//! [`entries`]. No `call_builtin_function` routing is required.

use crate::host::PyHost;
use fusevm::Value;

const ASCII_LOWERCASE: &str = "abcdefghijklmnopqrstuvwxyz";
const ASCII_UPPERCASE: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const DIGITS: &str = "0123456789";
const HEXDIGITS: &str = "0123456789abcdefABCDEF";
const OCTDIGITS: &str = "01234567";
const PUNCTUATION: &str = "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~";
const WHITESPACE: &str = " \t\n\r\x0b\x0c";

pub fn entries(h: &mut PyHost) -> Vec<(String, Value)> {
    let ascii_letters = format!("{ASCII_LOWERCASE}{ASCII_UPPERCASE}");
    let printable = format!("{DIGITS}{ascii_letters}{PUNCTUATION}{WHITESPACE}");
    vec![
        ("ascii_lowercase".into(), h.new_str(ASCII_LOWERCASE)),
        ("ascii_uppercase".into(), h.new_str(ASCII_UPPERCASE)),
        ("ascii_letters".into(), h.new_str(ascii_letters)),
        ("digits".into(), h.new_str(DIGITS)),
        ("hexdigits".into(), h.new_str(HEXDIGITS)),
        ("octdigits".into(), h.new_str(OCTDIGITS)),
        ("punctuation".into(), h.new_str(PUNCTUATION)),
        ("whitespace".into(), h.new_str(WHITESPACE)),
        ("printable".into(), h.new_str(printable)),
    ]
}

/// `string` exposes no callables. Always `None` so the parent keeps dispatching.
pub fn call(_h: &mut PyHost, _fname: &str, _args: &[Value]) -> Option<Result<Value, String>> {
    None
}
