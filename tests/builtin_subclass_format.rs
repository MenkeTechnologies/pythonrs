//! A `str`/`int`/`float` subclass as a `%` argument or `format()` target, and a
//! `deque` holding user objects. Frozen expectations from CPython 3.14.

use pythonrs::{eval_str, host};

/// Run `src`, then return the `repr` of global `name`.
fn g(src: &str, name: &str) -> String {
    eval_str(src).expect("program should run without error");
    host::with_host(|h| {
        let v = h
            .read_global(name)
            .unwrap_or_else(|| panic!("global {name} unbound"));
        h.repr_of(&v)
    })
}

const CLASSES: &str = "
class L(str):
    def __str__(s): return 'L!'
    def __repr__(s): return 'R!'
class I(int):
    def __str__(s): return 'I!'
    def __repr__(s): return 'IR'
class FL(float): pass
class P:
    def __repr__(s): return 'PP'
";

#[test]
fn percent_with_a_single_subclass_argument_uses_its_dunders() {
    let src = format!(
        "{CLASSES}r = ('%s' % L('x'), '%r' % L('x'), '%s' % I(5), '%r' % I(5), '%d' % I(5))"
    );
    assert_eq!(g(&src, "r"), "('L!', 'R!', 'I!', 'IR', '5')");
}

#[test]
fn percent_with_a_mapping_as_the_one_argument_reprs_its_contents() {
    let src = format!("{CLASSES}r = '%s' % {{P(): P(), 1: L('x')}}");
    assert_eq!(g(&src, "r"), "'{PP: PP, 1: R!}'");
}

#[test]
fn percent_with_keyed_conversions_still_reads_the_mapping() {
    let src = format!("{CLASSES}r = '%(a)s|%(b)r' % {{'a': L('x'), 'b': I(2)}}");
    assert_eq!(g(&src, "r"), "'L!|IR'");
}

#[test]
fn percent_tuple_subclass_is_still_the_argument_list() {
    let src = "
class T(tuple): pass
r = '%s-%s' % T((1, 2))
";
    assert_eq!(g(src, "r"), "'1-2'");
}

#[test]
fn format_spec_applies_to_the_base_value_not_dunder_str() {
    let src = format!(
        "{CLASSES}r = (format(L('x'), '>4'), f'{{I(5):>4}}', '{{:04d}}'.format(I(7)), format(FL(1.5), '>6.2f'), format(I(255), 'x'), format(L('x'), ''), format(I(5), ''))"
    );
    assert_eq!(
        g(&src, "r"),
        "('   x', '   5', '0007', '  1.50', 'ff', 'L!', 'I!')"
    );
}

#[test]
fn deque_of_user_objects_reprs_through_their_dunders() {
    let src = format!(
        "{CLASSES}import collections
d = collections.deque([P(), L('x')], maxlen=5)
r = (repr(d), str(d), '%s' % d, f'{{d}}', repr([d]))
"
    );
    assert_eq!(
        g(&src, "r"),
        "('deque([PP, R!], maxlen=5)', 'deque([PP, R!], maxlen=5)', 'deque([PP, R!], maxlen=5)', 'deque([PP, R!], maxlen=5)', '[deque([PP, R!], maxlen=5)]')"
    );
}
