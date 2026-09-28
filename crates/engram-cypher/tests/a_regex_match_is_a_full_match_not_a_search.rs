//! `=~` — the semantics Cypher specifies, over the `regex` crate.
//!
//! Cypher's `=~` is `java.lang.String.matches`: the pattern must match the
//! WHOLE string. That is the first test here because it is the one people get
//! wrong, and because the rest of the operator's behaviour follows from it —
//! captures are pointless, greediness cannot change the answer, and both ends
//! are anchored whether the pattern says so or not.
//!
//! The engine underneath is a finite automaton, so it cannot backtrack. That
//! is a safety property rather than a performance one: a regex in a `WHERE`
//! clause runs over every row of a scan, on an engine with no clock to time
//! itself out with. The catastrophic-backtracking canaries live in
//! `a_hostile_regex_is_refused_or_bounded_never_trusted.rs` alongside the rest
//! of the adversarial surface.

use engram_cypher::regex::{Regex, RegexError};
use engram_cypher::{EvalError, Scope, Value, eval, parse_expression};

fn v(src: &str) -> Value {
    eval(
        &parse_expression(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}")),
        &Scope::default(),
    )
    .unwrap_or_else(|e| panic!("eval `{src}`: {e}"))
}

fn err(src: &str) -> EvalError {
    eval(
        &parse_expression(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}")),
        &Scope::default(),
    )
    .expect_err("expected a refusal")
}

fn t(b: bool) -> Value {
    Value::Bool(b)
}

// ─── Full-match semantics ──────────────────────────────────────────────────

#[test]
fn a_regex_match_is_a_full_match_not_a_search() {
    // THE ONE PEOPLE GET WRONG. A substring is not a match.
    assert_eq!(v("'foo' =~ 'oo'"), t(false));
    assert_eq!(v("'foo' =~ '.*oo'"), t(true));
    assert_eq!(v("'foo' =~ 'foo'"), t(true));
    assert_eq!(v("'foo' =~ 'f.*'"), t(true));
    assert_eq!(v("'foo' =~ 'fo'"), t(false));
    assert_eq!(v("'foo' =~ 'foobar'"), t(false));
}

#[test]
fn an_alternation_is_anchored_as_a_whole_and_not_branch_by_branch() {
    // The anchoring wrapper has to group before it anchors. Without the
    // grouping, `a|b` would read as "starts with a, OR ends with b" and
    // `'b' =~ 'a|b'` would be false — a wrong answer in the most ordinary
    // pattern anyone writes.
    assert_eq!(v("'a' =~ 'a|b'"), t(true));
    assert_eq!(v("'b' =~ 'a|b'"), t(true));
    assert_eq!(v("'ab' =~ 'a|b'"), t(false));
    assert_eq!(v("'xay' =~ 'a|b'"), t(false));
}

#[test]
fn an_explicit_anchor_is_accepted_and_redundant() {
    assert_eq!(v("'foo' =~ '^foo$'"), t(true));
    assert_eq!(v("'foo' =~ '^fo'"), t(false));
}

#[test]
fn alternation_and_grouping_work() {
    assert_eq!(v("'cat' =~ '(cat|dog)'"), t(true));
    assert_eq!(v("'dog' =~ '(cat|dog)'"), t(true));
    assert_eq!(v("'cow' =~ '(cat|dog)'"), t(false));
    assert_eq!(v("'abcabc' =~ '(abc){2}'"), t(true));
    assert_eq!(v("'abcabc' =~ '(abc){3}'"), t(false));
}

#[test]
fn quantifiers_count_correctly() {
    assert_eq!(v("'aaa' =~ 'a*'"), t(true));
    assert_eq!(v("'' =~ 'a*'"), t(true));
    assert_eq!(v("'' =~ 'a+'"), t(false));
    assert_eq!(v("'aa' =~ 'a{2}'"), t(true));
    assert_eq!(v("'aaa' =~ 'a{2}'"), t(false));
    assert_eq!(v("'aaa' =~ 'a{2,}'"), t(true));
    assert_eq!(v("'aa' =~ 'a{1,3}'"), t(true));
    assert_eq!(v("'aaaa' =~ 'a{1,3}'"), t(false));
}

#[test]
fn a_bounded_repeat_cannot_be_satisfied_with_a_gap() {
    assert_eq!(v("'aa' =~ 'a{0,3}'"), t(true));
    assert_eq!(v("'aab' =~ 'a{0,3}'"), t(false));
    assert_eq!(v("'abab' =~ '(ab){0,3}'"), t(true));
}

#[test]
fn character_classes_and_shorthands_work() {
    assert_eq!(v("'a1' =~ '[a-z][0-9]'"), t(true));
    assert_eq!(v("'a1' =~ '\\\\w\\\\d'"), t(true));
    assert_eq!(v("'a-' =~ '[a-z][-]'"), t(true));
    assert_eq!(v("'q' =~ '[^a-p]'"), t(true));
    assert_eq!(v("'b' =~ '[^a-p]'"), t(false));
    assert_eq!(v("'a b' =~ 'a\\\\sb'"), t(true));
}

#[test]
fn a_lazy_quantifier_is_accepted_and_changes_no_answer() {
    // Greediness chooses AMONG matches; `=~` asks only whether one exists.
    assert_eq!(v("'aaa' =~ 'a*?'"), t(true));
    assert_eq!(v("'aaa' =~ '.*?'"), t(true));
    assert_eq!(v("'foo' =~ '.+?o'"), t(true));
}

#[test]
fn a_word_boundary_holds_where_it_should() {
    assert_eq!(v("'ab' =~ '\\\\bab\\\\b'"), t(true));
    assert_eq!(v("'ab' =~ '\\\\ba\\\\bb'"), t(false));
}

// ─── Unicode ───────────────────────────────────────────────────────────────

#[test]
fn a_regex_matches_by_characters_not_by_bytes() {
    // 'é' is two BYTES and one CHARACTER. A byte-oriented matcher would let
    // `..` match it and `.` match half of it — a wrong answer, not a faster
    // one.
    assert_eq!(v("'é' =~ '.'"), t(true));
    assert_eq!(v("'é' =~ '..'"), t(false));
    assert_eq!(v("'日本' =~ '..'"), t(true));
    assert_eq!(v("'日本' =~ '.'"), t(false));
}

#[test]
fn a_dot_does_not_match_a_newline_without_the_s_flag() {
    assert_eq!(v("'a\nb' =~ 'a.b'"), t(false));
    assert_eq!(v("'a\nb' =~ '(?s)a.b'"), t(true));
}

#[test]
fn a_leading_case_insensitive_flag_folds_both_sides() {
    assert_eq!(v("'FOO' =~ '(?i)foo'"), t(true));
    assert_eq!(v("'foo' =~ '(?i)FOO'"), t(true));
    assert_eq!(v("'FOO' =~ 'foo'"), t(false));
    assert_eq!(v("'FOO' =~ '(?i)[a-z]+'"), t(true));
}

#[test]
fn a_mid_pattern_case_flag_applies_only_after_it_appears() {
    // An earlier hand-written matcher REFUSED this, because folding at a
    // position was something its trigram analysis could not express. Sharing
    // one parser with the index removed the problem rather than working around
    // it: the parsed tree arrives with folding already applied as character
    // classes, so the matcher and the index cannot disagree about which
    // characters were folded.
    assert_eq!(v("'aB' =~ 'a(?i)b'"), t(true));
    assert_eq!(v("'ab' =~ 'a(?i)b'"), t(true));
    assert_eq!(v("'Ab' =~ 'a(?i)b'"), t(false));
}

#[test]
fn unicode_classes_are_available() {
    // `\p{…}` and POSIX classes come with the engine. They were absent from
    // the hand-written one and their arrival is a capability, not a change of
    // meaning for anything that already worked.
    assert_eq!(v("'abc' =~ '\\\\p{L}+'"), t(true));
    assert_eq!(v("'123' =~ '\\\\p{L}+'"), t(false));
    assert_eq!(v("'日本' =~ '\\\\p{Han}+'"), t(true));
    assert_eq!(v("'abc' =~ '[[:alpha:]]+'"), t(true));
    assert_eq!(v("'a1' =~ '[[:alpha:]]+'"), t(false));
}

// ─── Null and type semantics ───────────────────────────────────────────────

#[test]
fn a_null_operand_to_a_regex_makes_the_result_null() {
    assert_eq!(v("null =~ 'a'"), Value::Null);
    assert_eq!(v("'a' =~ null"), Value::Null);
    assert_eq!(v("null =~ null"), Value::Null);
    // Null wins over the type check: this is null, not a type error.
    assert_eq!(v("null =~ 3"), Value::Null);
}

#[test]
fn a_non_string_operand_to_a_regex_is_a_type_error() {
    // Not stringified. `3 =~ '3'` answering true would be a silent coercion in
    // an operator whose whole job is exactness.
    assert!(matches!(err("3 =~ '3'"), EvalError::Type { .. }));
    assert!(matches!(err("'3' =~ 3"), EvalError::Type { .. }));
    assert!(matches!(err("[1] =~ 'a'"), EvalError::Type { .. }));
}

// ─── Bounds and refusals ───────────────────────────────────────────────────

#[test]
fn a_pathological_pattern_answers_rather_than_needing_a_timeout() {
    // The engine is a finite automaton, so its per-character cost does not
    // depend on the pattern's structure. There is nothing to budget and
    // nothing that can time out; what replaces a budget is that the input
    // simply gets an answer.
    let re = Regex::compile("(a|a)*b").expect("compiles");
    let input = "a".repeat(4096);
    assert!(!re.is_full_match(&input));
    assert!(re.is_full_match(&format!("{input}b")));
}

#[test]
fn a_pattern_too_large_to_compile_is_refused_at_compile_time() {
    // The bound that does still exist, applied where the cost is known and
    // before anything has been allocated.
    assert!(
        Regex::compile("((a{1000}){1000}){1000}").is_err(),
        "a repetition bomb must be refused",
    );
    assert!(Regex::compile("a{1000}").is_ok());
}

#[test]
fn an_unsupported_regex_feature_is_refused_by_name() {
    // These are the constructs a finite automaton genuinely cannot run. Each
    // is named rather than reported as a parse error about something else,
    // because a user who wrote `\1` needs to be told "backreferences".
    let cases: [(&str, &str); 7] = [
        ("(a)\\1", "backreferences"),
        ("(?=x)a", "lookahead"),
        ("(?!x)a", "negative lookahead"),
        ("(?<=x)a", "lookbehind"),
        ("(?<!x)a", "negative lookbehind"),
        ("(?>a)", "atomic"),
        ("\\Qa.b\\E", "literal quoting"),
    ];
    for (pattern, expect) in cases {
        let e = Regex::compile(pattern).expect_err(&format!("`{pattern}` must be refused"));
        let msg = e.to_string();
        assert!(
            msg.contains(expect),
            "`{pattern}` must be refused by name; expected {expect:?} in {msg:?}",
        );
    }
}

#[test]
fn a_backreference_is_not_silently_read_as_a_literal_digit() {
    // THE NEGATIVE THAT MATTERS MOST. A naive reading takes `\1` for
    // "backslash then 1" and produces a pattern that matches a different set
    // of strings, for ever, with nothing looking wrong.
    assert!(Regex::compile("(a)\\1").is_err());
    assert!(
        Regex::compile("a\\1").is_err(),
        "even with no group to refer to, `\\1` must not become a literal `1`",
    );
    // And the literal digit is still reachable the honest way.
    assert_eq!(v("'a1' =~ 'a1'"), t(true));
}

#[test]
fn a_possessive_quantifier_is_refused_but_a_lazy_one_is_not() {
    // A possessive quantifier can REMOVE matches, so accepting it as something
    // else would change answers. A lazy one cannot, so treating it as greedy
    // is exact.
    assert!(Regex::compile("a*+").is_err());
    assert!(Regex::compile("a++").is_err());
    assert!(Regex::compile("a*?").is_ok());
    assert!(Regex::compile("a+?").is_ok());
}

#[test]
fn a_malformed_pattern_is_a_syntax_error() {
    for pattern in ["(a", "a)", "[a", "*a", "a{2,1}", "[z-a]", "a\\"] {
        let e = Regex::compile(pattern).expect_err(&format!("`{pattern}` must be refused"));
        assert!(
            matches!(e, RegexError::Syntax { .. }),
            "`{pattern}`: expected a syntax error, got {e:?}",
        );
    }
}

#[test]
fn a_refusal_does_not_echo_the_anchoring_this_engine_added() {
    // The pattern is wrapped in `\A(?:…)\z` before compiling. A diagnostic
    // that showed the user that wrapper would be talking about a pattern they
    // did not write.
    let e = Regex::compile("(a").expect_err("must be refused");
    let msg = e.to_string();
    assert!(
        !msg.contains("\\A(?:"),
        "the refusal must not show the internal anchoring: {msg}",
    );
}

#[test]
fn a_brace_that_is_not_a_repetition_is_a_literal_brace() {
    // `a{foo}` is six literal characters in the engines people copy patterns
    // from, and a user writing a pattern over JSON-ish text relies on it.
    assert_eq!(v("'a{foo}' =~ 'a\\\\{foo\\\\}'"), t(true));
}
