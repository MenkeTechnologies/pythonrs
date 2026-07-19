//! Method-call intercepts (aspect-oriented advice) — a glob-matched registry of
//! before/after/around hooks keyed by method-name pattern.
//!
//! This is the substrate for pythonrs's AOP layer (the same design as zshrs's
//! function intercepts and rubylang's method intercepts): register a pattern
//! like `"user_*"` or `"__*__"` with an advice kind, and the dispatcher can
//! consult `matches()` to weave advice around a call. The registry and glob
//! matching are live and tested here; the dispatch-loop weave is a later wave so
//! the fast path stays fast until the feature is turned on explicitly.

use glob::Pattern;
use std::cell::RefCell;

/// When advice runs relative to the intercepted call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advice {
    Before,
    After,
    Around,
}

struct Intercept {
    pattern: Pattern,
    advice: Advice,
    handler: String,
}

thread_local! {
    static INTERCEPTS: RefCell<Vec<Intercept>> = const { RefCell::new(Vec::new()) };
}

/// Register `handler` to run as `advice` whenever a called method name matches
/// the glob `pattern`. Returns an error if the pattern is malformed.
pub fn register(pattern: &str, advice: Advice, handler: &str) -> Result<(), String> {
    let pat =
        Pattern::new(pattern).map_err(|e| format!("bad intercept pattern '{pattern}': {e}"))?;
    INTERCEPTS.with(|i| {
        i.borrow_mut().push(Intercept {
            pattern: pat,
            advice,
            handler: handler.to_string(),
        })
    });
    Ok(())
}

/// Clear all registered intercepts.
pub fn clear() {
    INTERCEPTS.with(|i| i.borrow_mut().clear());
}

/// The advice handlers whose pattern matches `method`, in registration order.
pub fn matches(method: &str) -> Vec<(Advice, String)> {
    INTERCEPTS.with(|i| {
        i.borrow()
            .iter()
            .filter(|iv| iv.pattern.matches(method))
            .map(|iv| (iv.advice, iv.handler.clone()))
            .collect()
    })
}

/// Whether any intercept is registered (dispatch fast-path guard).
pub fn any() -> bool {
    INTERCEPTS.with(|i| !i.borrow().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_and_ordering() {
        clear();
        register("user_*", Advice::Before, "log_entry").unwrap();
        register("__*__", Advice::Around, "guard_dunder").unwrap();
        assert!(any());

        let m = matches("user_create");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].0, Advice::Before);

        let m = matches("__init__");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].1, "guard_dunder");

        assert!(matches("plain").is_empty());
        clear();
    }

    #[test]
    fn bad_pattern_is_reported() {
        assert!(register("[", Advice::Before, "x").is_err());
    }
}
