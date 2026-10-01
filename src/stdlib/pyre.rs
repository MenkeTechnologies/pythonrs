//! `re.Scanner`, as Python source run on pythonrs.
//!
//! CPython defines `Scanner` in pure Python in `Lib/re/__init__.py`, on top of
//! two `_sre` pieces pythonrs's native `re` also provides: `Pattern.scanner`
//! (the `SRE_Scanner` cursor) and `Match.lastindex`. Writing it in Python keeps
//! it a class like CPython's — instances carry `lexicon`/`scanner` attributes,
//! an action receives the scanner itself and can read `self.match`, and a
//! subclass can override `scan` — where a native object would have to imitate
//! each of those.
//!
//! The one departure is how the combined pattern is built. CPython assembles it
//! from `re._parser` parse trees: one capture group per lexicon phrase, the
//! groups numbered 1..n in lexicon order, each phrase parsed in a state of its
//! own, so the phrase a match came from is `lexicon[m.lastindex - 1]`. pythonrs
//! has no parse trees, so the same alternation is built as pattern TEXT,
//! `(p1)|(p2)|…`. A phrase's own capture groups are then numbered in between
//! the wrappers, so the phrase is found by its wrapper's group number instead —
//! `_wrapper_groups` counts each phrase's groups to place the wrappers.

/// The Python source of the `re._scanner` helper module, whose `Scanner`
/// class the native `re` module exports.
pub fn module_source() -> &'static str {
    r#""""`re.Scanner` for pythonrs's native `re` (see `src/stdlib/pyre.rs`)."""


def _wrapper_groups(lexicon, flags):
    """The group number of each phrase's wrapper group in `Scanner`'s combined
    pattern, by lexicon index: every wrapper is followed by its phrase's own
    groups, so each one sits that many numbers past the last."""
    import re
    numbers = {}
    gid = 1
    for index, (phrase, action) in enumerate(lexicon):
        numbers[gid] = index
        gid += 1 + re.compile(phrase, flags).groups
    return numbers


def _text(s, like):
    """`s` as the same kind of string as the phrase `like` (a bytes lexicon
    builds a bytes pattern)."""
    return s.encode('ascii') if isinstance(like, bytes) else s


class Scanner:
    __module__ = 're'

    def __init__(self, lexicon, flags=0):
        import re
        self.lexicon = lexicon
        if not lexicon:
            # CPython's compiled code for an empty BRANCH fails `_sre`'s
            # validator, which reports it this way.
            raise RuntimeError('invalid SRE code')
        # combine phrases into a compound pattern; under VERBOSE a phrase may
        # end in a `#` comment, which must not swallow the wrapper's `)`
        close = '\n)' if flags & re.VERBOSE else ')'
        p = [_text('(', phrase) + phrase + _text(close, phrase) for phrase, action in lexicon]
        self.scanner = re.compile(_text('|', p[0]).join(p), flags)

    def scan(self, string):
        import re
        result = []
        append = result.append
        match = self.scanner.scanner(string).match
        phrase_of = _wrapper_groups(self.lexicon, self.scanner.flags & re.VERBOSE)
        i = 0
        while True:
            m = match()
            if not m:
                break
            j = m.end()
            if i == j:
                break
            action = self.lexicon[phrase_of[m.lastindex]][1]
            if callable(action):
                self.match = m
                action = action(self, m.group())
            if action is not None:
                append(action)
            i = j
        return result, string[i:]
"#
}
