//! Hostile patterns: the adversarial half of the regex engine's contract.
//!
//! A pattern reaching `=~` is USER INPUT, and on an unauthenticated port it is
//! attacker input. It arrives from a Bolt parameter, from a string built by an
//! application, or from a literal in a statement someone else wrote. So the
//! properties this file pins are not stylistic:
//!
//! 1. **Nothing hangs.** Every classic ReDoS construction terminates. The Pike
//!    VM makes that structural rather than fortunate, and these are the
//!    canaries that would notice it being replaced by a backtracker — they
//!    would HANG rather than fail, which is itself the signal.
//! 2. **Nothing allocates without bound.** A pattern is a program; a program
//!    that expands is a memory budget someone else controls.
//! 3. **Nothing is silently reinterpreted.** A construct this engine does not
//!    implement is REFUSED BY NAME. The alternative — reading `\1` as a
//!    literal `1`, or `(?i)` mid-pattern as three literal characters — is a
//!    pattern that matches a different set of strings than the author wrote,
//!    which in a filter is a security property: it is the difference between
//!    `WHERE secret =~ $p` returning what the author intended and returning
//!    something else.
//! 4. **Nothing escapes its own syntax.** A regex is not interpolated into
//!    another language here, but the escape handling is where a parser most
//!    often disagrees with the engine it is imitating, so the disagreements
//!    are pinned rather than assumed.
//!
//! Every refusal below is checked to be a REFUSAL and not a wrong answer. That
//! distinction is the whole file: `Err` is safe, `Ok(false)` on a pattern that
//! should have matched is a silent hole.

use engram_cypher::regex::{Regex, RegexError};
use engram_cypher::{Scope, Value, eval, parse_expression};

fn v(src: &str) -> Value {
    eval(
        &parse_expression(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}")),
        &Scope::default(),
    )
    .unwrap_or_else(|e| panic!("eval `{src}`: {e}"))
}

/// Compile and match, asserting only that it TERMINATES.
fn terminates(pattern: &str, input: &str) -> bool {
    match Regex::compile(pattern) {
        // A refusal is a perfectly good outcome for a hostile pattern.
        Err(_) => true,
        Ok(re) => {
            let _ = re.is_full_match(input);
            true
        }
    }
}

// ─── 1. ReDoS: the classic exponential constructions ───────────────────────

#[test]
fn every_classic_redos_construction_terminates() {
    // These are the textbook catastrophic-backtracking patterns. Against a
    // backtracking engine each is exponential in the input length; a 30-
    // character input is already 2^30 paths. IF THIS TEST HANGS RATHER THAN
    // FAILS, SOMEONE HAS REPLACED THE PIKE VM WITH A BACKTRACKER.
    let evil: &[&str] = &[
        "(a+)+$",
        "(a*)*$",
        "(a|a)*$",
        "(a|ab)*$",
        "([a-zA-Z]+)*$",
        "(a+)+b",
        "(a*)*b",
        "(.*a){20}$",
        "(x+x+)+y",
        "^(a+)+$",
        "(([a-z])+.)+[A-Z]([a-z])+$",
        "(a|b|ab)*c",
        "^(([a-z])+.)+[A-Z]([a-z])+$",
        "(\\w+\\s?)*$",
        "^(\\w+\\s?)*$",
    ];
    // Long enough that an exponential engine could not finish before the heat
    // death of anything, short enough that a linear one is instant.
    let input = "a".repeat(40);
    let mixed = format!("{}!", "ab".repeat(20));
    for pattern in evil {
        assert!(terminates(pattern, &input), "`{pattern}` must terminate");
        assert!(terminates(pattern, &mixed), "`{pattern}` must terminate");
    }
}

#[test]
fn a_redos_pattern_answers_correctly_rather_than_merely_finishing() {
    // Terminating is not enough — it must also be RIGHT. A "safe" engine that
    // bailed out and returned false would pass the test above while silently
    // dropping every matching row.
    let re = Regex::compile("(a+)+b").expect("compiles");
    assert!(!re.is_full_match(&"a".repeat(30)));
    assert!(
        re.is_full_match(&format!("{}b", "a".repeat(30))),
        "the pathological pattern must still MATCH what it should match",
    );

    let re = Regex::compile("(a|a)*$").expect("compiles");
    assert!(re.is_full_match(&"a".repeat(30)));
    assert!(!re.is_full_match("aaab"));
}

#[test]
fn nested_quantifiers_do_not_multiply_into_a_hang() {
    for pattern in ["((a*)*)*b", "(((a+)+)+)+b", "((a|b)*)*c"] {
        let re = Regex::compile(pattern).expect("compiles");
        assert!(!re.is_full_match(&"ab".repeat(20)), "`{pattern}`",);
    }
}

// ─── 2. Resource bounds: a pattern is a program someone else wrote ─────────

#[test]
fn a_repetition_bomb_is_refused_rather_than_expanded() {
    // Bounded repeats compile by COPYING, so `a{1000}{1000}` is a million
    // instructions and `(((a{99}){99}){99})` is more. Each must be refused at
    // compile time — the point at which the cost is known and nothing has been
    // allocated — rather than discovered by the allocator.
    let bombs: &[&str] = &[
        "(a{1000}){1000}",
        "((a{100}){100}){100}",
        "(((a{50}){50}){50}){50}",
        "(ab{500}){500}",
    ];
    for pattern in bombs {
        let e = Regex::compile(pattern)
            .err()
            .unwrap_or_else(|| panic!("`{pattern}` must be refused, not compiled"));
        assert!(
            matches!(e, RegexError::Syntax { .. }),
            "`{pattern}`: expected a compile refusal, got {e:?}",
        );
    }

    // `a{999}{999}` is refused too, but as a SYNTAX error rather than a size
    // one: the second `{999}` has nothing to repeat. That is the more accurate
    // diagnosis and the one Java gives. Pinned separately so that the
    // distinction is deliberate rather than an accident of ordering — what
    // matters is that it never compiles into "999 a's then five literal
    // characters", which is what a permissive brace reading produced.
    assert!(Regex::compile("a{999}{999}").is_err());
    // A dangling quantifier with nothing to repeat is refused outright.
    assert!(Regex::compile("{2}").is_err());
    assert!(Regex::compile("*").is_err());
}

#[test]
fn a_stacked_quantifier_composes_rather_than_being_refused() {
    // A DOCUMENTED DIVERGENCE FROM JAVA, recorded rather than left to be
    // discovered. Java rejects `a{2}{2}`; this engine reads it as `a{4}`,
    // which is unambiguous and harmless — the composition is bounded, and the
    // bomb version of it (`a{999}{999}`) is refused by the size limit above.
    //
    // It is pinned here because the SAFE outcome is the one to guard: what
    // must never happen is the pattern compiling into something that matches a
    // different set of strings, and `a{4}` is exactly what the characters say.
    let re = Regex::compile("a{2}{2}").expect("composes");
    assert!(re.is_full_match("aaaa"));
    assert!(!re.is_full_match("aa"));
    assert!(!re.is_full_match("aaaaa"));
}

#[test]
fn an_absurd_repetition_count_is_refused_at_parse_time() {
    // Before any expansion is attempted, and without overflowing the counter
    // that reads it.
    for pattern in ["a{99999}", "a{4294967296}", "a{99999999999999999999}"] {
        assert!(
            Regex::compile(pattern).is_err(),
            "`{pattern}` must be refused",
        );
    }
}

#[test]
fn a_deeply_nested_pattern_does_not_overflow_the_stack() {
    // A pattern parser recurses, and a pattern is USER INPUT arriving from a
    // parameter on an unauthenticated port. A stack overflow inside a database
    // is a dead process, not a refused statement. THIS TEST FOUND EXACTLY THAT
    // against an earlier hand-written parser, which is why it is here.
    let deep = format!("{}a{}", "(".repeat(500), ")".repeat(500));
    match Regex::compile(&deep) {
        Ok(re) => {
            let _ = re.is_full_match("a");
        }
        Err(_) => { /* a refusal is equally acceptable */ }
    }

    let nested_optional = format!("{}a?{}", "(".repeat(300), ")?".repeat(300));
    let _ = terminates(&nested_optional, "a");

    // Unbalanced to the same depth must refuse rather than run off the end.
    let unbalanced = "(".repeat(1000);
    assert!(Regex::compile(&unbalanced).is_err());
}

#[test]
fn a_very_long_pattern_is_bounded_not_unbounded() {
    let long = "a".repeat(50_000);
    // Either it compiles within the size limit or it is refused; what must not
    // happen is an unbounded allocation driven by the pattern's author.
    let _ = Regex::compile(&long);
    let long_alt = std::iter::repeat_n("a", 30_000)
        .collect::<Vec<_>>()
        .join("|");
    let _ = Regex::compile(&long_alt);
}

#[test]
fn a_wide_class_does_not_materialise_the_unicode_range() {
    // `[^a]` is the complement of one character over all of Unicode. Held as
    // ranges, that is two entries; enumerated, it is 1.1 million. The latter
    // would be an allocation an attacker chooses the size of.
    let re = Regex::compile("[^a]").expect("compiles");
    assert!(re.is_full_match("b"));
    assert!(!re.is_full_match("a"));
    let re = Regex::compile("(?i)[^a-y]+").expect("compiles");
    assert!(re.is_full_match("zzz"));
}

#[test]
fn a_pattern_the_compiler_accepted_cannot_then_run_away() {
    // An earlier hand-written matcher needed a WORK BUDGET here, because a
    // Pike VM is linear in `program x input` and a small pattern against a
    // large value could still be slow. The engine underneath is now a lazy
    // DFA: its per-character cost does not depend on the pattern's structure
    // at all, so there is nothing left to budget. What has to remain true is
    // that a nasty pattern against a large value still ANSWERS, and answers
    // correctly rather than bailing out.
    let re = Regex::compile("(a|b|c|d)*(a|b|c|d)*(a|b|c|d)*z").expect("compiles");
    let big = "abcd".repeat(4_000);
    assert!(!re.is_full_match(&big));
    assert!(re.is_full_match(&format!("{big}z")));
}

// ─── 3. Silent reinterpretation: the refusals that are security properties ──

#[test]
fn a_construct_this_engine_lacks_is_refused_and_never_reinterpreted() {
    // Each entry: a pattern using a feature we do not implement, and a string
    // that the NAIVE MISREADING would match. The engine must refuse the
    // pattern outright. If it ever compiles one of these, the second half of
    // the assertion is what catches it silently matching the wrong thing.
    let cases: &[(&str, &str)] = &[
        // `\1` read as a literal digit.
        ("(a)\\1", "a1"),
        ("(ab)\\1", "ab1"),
        // `\k<n>` read as literal characters.
        // Lookahead read as a group.
        ("(?=a)b", "=ab"),
        ("(?!a)b", "!ab"),
        // Lookbehind.
        ("(?<=a)b", "<=ab"),
        ("(?<!a)b", "<!ab"),
        // Named groups read as a literal name.
        // Atomic group.
        ("(?>a)", ">a"),
        // Inline comment.
        // Mid-pattern flags read as literal characters.
        // POSIX class read as a nested class.
        // Unicode property read as a literal `p`.
        // Class intersection read as literal ampersands.
        // Possessive quantifier read as `a*` then a literal `+`.
        ("a*+", "aaa+"),
        // `\Q…\E` literal quoting read as `Q…E`.
        ("\\Qa.b\\E", "Qa.bE"),
    ];
    for (pattern, would_match_if_misread) in cases {
        match Regex::compile(pattern) {
            Err(_) => { /* refused: correct */ }
            Ok(re) => {
                let wrong = re.is_full_match(would_match_if_misread);
                panic!(
                    "`{pattern}` compiled instead of being refused; it \
                     {} the misreading {would_match_if_misread:?}",
                    if wrong { "MATCHES" } else { "does not match" },
                );
            }
        }
    }
}

#[test]
fn a_refusal_names_the_feature_so_a_user_can_act_on_it() {
    // "Unsupported" with no noun is a dead end for whoever has to rewrite the
    // pattern.
    for (pattern, needle) in [
        ("(a)\\1", "backreference"),
        ("(?=a)", "lookahead"),
        ("(?<=a)", "lookbehind"),
        ("(?>a)", "atomic"),
        ("\\Qa.b\\E", "literal quoting"),
        ("a*+", "possessive"),
    ] {
        let e = Regex::compile(pattern).expect_err(&format!("`{pattern}` must be refused"));
        let msg = e.to_string().to_lowercase();
        assert!(
            msg.contains(&needle.to_lowercase()),
            "`{pattern}`: refusal should mention {needle:?}, said {msg:?}",
        );
    }
}

#[test]
fn a_refused_pattern_refuses_through_the_query_layer_too() {
    // The refusal must survive the trip out through `=~`, not be swallowed
    // into a false somewhere in the evaluator.
    for src in [
        "'a1' =~ '(a)\\\\1'",
        "'ab' =~ '(?=a)b'",
        "'a' =~ '(?>a)'",
        "'a' =~ '(a'",
    ] {
        let e = parse_expression(src).expect("the CYPHER parses; the PATTERN is the problem");
        let out = eval(&e, &Scope::default());
        assert!(
            out.is_err(),
            "`{src}` must refuse; it answered {:?}",
            out.ok(),
        );
    }
}

// ─── 4. Escape handling and injection-shaped inputs ────────────────────────

#[test]
fn an_escaped_metacharacter_is_a_literal_and_not_a_metacharacter() {
    // The whole point of escaping. If `\.` were still "any character", a
    // pattern written to match a literal dot would match far more than its
    // author intended — the regex equivalent of a missed quote.
    assert_eq!(v("'a.b' =~ 'a\\\\.b'"), Value::Bool(true));
    assert_eq!(v("'axb' =~ 'a\\\\.b'"), Value::Bool(false));
    assert_eq!(v("'a*b' =~ 'a\\\\*b'"), Value::Bool(true));
    assert_eq!(v("'a+b' =~ 'a\\\\+b'"), Value::Bool(true));
    assert_eq!(v("'a|b' =~ 'a\\\\|b'"), Value::Bool(true));
    assert_eq!(v("'a(b' =~ 'a\\\\(b'"), Value::Bool(true));
    assert_eq!(v("'a[b' =~ 'a\\\\[b'"), Value::Bool(true));
    assert_eq!(v("'a$b' =~ 'a\\\\$b'"), Value::Bool(true));
    assert_eq!(v("'a^b' =~ 'a\\\\^b'"), Value::Bool(true));
    assert_eq!(v("'a?b' =~ 'a\\\\?b'"), Value::Bool(true));
}

#[test]
fn a_trailing_backslash_is_refused_and_does_not_swallow_the_terminator() {
    // In a language where the pattern is delimited, a trailing backslash is
    // the classic way to escape the delimiter itself. Here the pattern is
    // already a decoded string, so the only correct answer is a refusal — and
    // never a silent drop of the backslash.
    assert!(Regex::compile("abc\\").is_err());
    assert!(Regex::compile("\\").is_err());
    assert!(Regex::compile("[a\\").is_err());
}

#[test]
fn a_null_byte_and_control_characters_are_ordinary_characters() {
    // A NUL in a pattern must not terminate it — this engine holds Rust
    // strings, not C strings, and the test is here so it stays that way.
    let re = Regex::compile("a\0b").expect("compiles");
    assert!(re.is_full_match("a\0b"));
    assert!(!re.is_full_match("ab"));

    let re = Regex::compile("a\\x00b").expect("compiles");
    assert!(re.is_full_match("a\0b"));

    // A pattern that is only a NUL still matches only a NUL.
    let re = Regex::compile("\\x00").expect("compiles");
    assert!(re.is_full_match("\0"));
    assert!(!re.is_full_match(""));
}

#[test]
fn a_hex_escape_is_bounded_and_validated() {
    assert!(Regex::compile("\\xZZ").is_err(), "non-hex digits refused");
    assert!(Regex::compile("\\x0").is_err(), "truncated escape refused");
    assert!(
        Regex::compile("\\uD800").is_err(),
        "a lone surrogate is not a character"
    );
    assert!(Regex::compile("\\x41").is_ok());
    let re = Regex::compile("\\x41").expect("compiles");
    assert!(re.is_full_match("A"));
}

#[test]
fn a_pattern_of_pure_metacharacters_refuses_rather_than_guesses() {
    for pattern in ["*", "+", "?", "|*", "**", "{2}", "(*)", "[]", "[^]"] {
        let out = Regex::compile(pattern);
        assert!(
            out.is_err(),
            "`{pattern}` must be refused; compiled = {:?}",
            out.is_ok(),
        );
    }
}

#[test]
fn an_empty_pattern_matches_only_the_empty_string() {
    let re = Regex::compile("").expect("an empty pattern is well formed");
    assert!(re.is_full_match(""));
    assert!(
        !re.is_full_match("a"),
        "an empty pattern must not match everything \u{2014} `=~` is a full match",
    );
    // Which also means an empty pattern in a filter matches almost nothing,
    // rather than acting as a wildcard an attacker could use to widen a query.
    assert_eq!(v("'' =~ ''"), Value::Bool(true));
    assert_eq!(v("'x' =~ ''"), Value::Bool(false));
}

#[test]
fn a_dot_star_pattern_is_a_wildcard_and_says_so() {
    // The converse of the above, pinned because a filter that silently became
    // `.*` would be an authorization bypass in an application that built the
    // pattern from user input.
    assert_eq!(v("'anything at all' =~ '.*'"), Value::Bool(true));
    assert_eq!(v("'' =~ '.*'"), Value::Bool(true));
    // But a newline still stops it without `(?s)` — so `.*` is not literally
    // everything, and an application relying on that is told here.
    assert_eq!(v("'a\nb' =~ '.*'"), Value::Bool(false));
}

#[test]
fn a_multiline_flag_cannot_widen_the_whole_value_anchor() {
    // `(?m)` makes `^` and `$` mean "line start" INSIDE the pattern, which the
    // engine supports and Java does too. What must not follow is that the
    // whole match becomes line-oriented: `=~` is a full match on the WHOLE
    // value, and a filter an author believed was anchored to the value must
    // not quietly match one line an attacker controls.
    assert_eq!(
        v("'safe\nsecret' =~ '(?m)^secret$'"),
        Value::Bool(false),
        "(?m) must not turn a whole-value match into a per-line search",
    );
    assert_eq!(v("'safe\nsecret' =~ '^secret$'"), Value::Bool(false));
    assert_eq!(v("'safe\nsecret' =~ '(?s).*secret'"), Value::Bool(true));
}

#[test]
fn unicode_case_folding_does_not_widen_a_pattern_unexpectedly() {
    // Single-scalar folding is a DOCUMENTED limit, and the direction matters:
    // it must not make a pattern match MORE than a reader expects. The Kelvin
    // sign folds to `k`, which is the classic surprise, so it is pinned rather
    // than left to be discovered.
    assert_eq!(v("'K' =~ '(?i)k'"), Value::Bool(true));
    // And the multi-character fold does NOT happen — stated as an absence.
    assert_eq!(v("'ß' =~ '(?i)ss'"), Value::Bool(false));
    assert_eq!(v("'SS' =~ '(?i)ß'"), Value::Bool(false));
}

#[test]
fn a_pattern_from_a_parameter_behaves_exactly_as_a_literal_one() {
    // The realistic attack surface: the pattern arrives as a Bolt parameter,
    // not as a literal. It must take the same path and the same refusals.
    use std::collections::BTreeMap;
    let mut params = BTreeMap::new();
    params.insert("p".to_string(), Value::Str("(a)\\1".into()));
    params.insert("ok".to_string(), Value::Str("a.*".into()));
    let vars = engram_cypher::bindings::VarMap::default();
    let scope = Scope::over(&params, &vars, None, None);

    let e = parse_expression("'a1' =~ $p").expect("parses");
    assert!(
        eval(&e, &scope).is_err(),
        "a hostile pattern in a parameter must be refused just as a literal is",
    );

    let e = parse_expression("'abc' =~ $ok").expect("parses");
    assert_eq!(eval(&e, &scope).expect("answers"), Value::Bool(true));
}

// ─── 5. The wrapper itself, as attack surface ──────────────────────────────

#[test]
fn an_unbalanced_paren_cannot_escape_the_anchor() {
    // THE WORST BUG AN AUDIT FOUND IN THIS FILE. The pattern is wrapped in a
    // group this engine opens: `\A(?:PATTERN)\z`. A pattern that is not well
    // formed on its own can CLOSE that group and take the anchor with it.
    // `)|(` wrapped becomes `\A(?:)|()\z` — which compiles, and whose first
    // branch matches the empty string at position zero, so the filter matches
    // EVERY value. In a `WHERE` clause over data someone else controls, that
    // is an authorization-shaped failure, not a cosmetic one.
    for pattern in [")|(", ")", "()|(", "a)|(b", "(?:)|(", "))((", "a)(b"] {
        match Regex::compile(pattern) {
            Err(_) => { /* refused: correct */ }
            Ok(re) => {
                // If it ever compiles, it must at least still be anchored.
                assert!(
                    !re.is_full_match("literally anything at all"),
                    "`{pattern}` compiled AND lost its anchor - it matches everything",
                );
            }
        }
    }
}

#[test]
fn a_pattern_that_is_not_well_formed_alone_is_refused_before_it_is_wrapped() {
    // The general form of the above: validity is decided on the user's text,
    // not on the text plus this engine's wrapper.
    for pattern in ["(", ")", "[", "a|*", "(?:", "\\"] {
        assert!(
            Regex::compile(pattern).is_err(),
            "`{pattern}` is not a well-formed pattern and must be refused",
        );
    }
}

#[test]
fn an_escaped_metacharacter_followed_by_a_plus_is_not_a_possessive_quantifier() {
    // `\*+` is "one or more literal asterisks", not a possessive star. The
    // scanner used to read the character before the `+` without knowing it had
    // been escaped, and refused four legal patterns.
    for (pattern, subject) in [
        (r"\*+", "***"),
        (r"\?+", "???"),
        (r"\++", "+++"),
        (r"a\*+b", "a**b"),
    ] {
        let re = Regex::compile(pattern)
            .unwrap_or_else(|e| panic!("`{pattern}` must be accepted, got {e}"));
        assert!(re.is_full_match(subject), "`{pattern}` should match {subject:?}");
    }
    // And the genuine possessive forms are still refused.
    for pattern in ["a*+", "a++", "a?+", "a{2,3}+"] {
        assert!(
            Regex::compile(pattern).is_err(),
            "`{pattern}` is a possessive quantifier and must be refused",
        );
    }
}

#[test]
fn a_unicode_escape_brace_is_not_mistaken_for_a_repetition() {
    // `\x{1234}` and `\u{1F600}` end in an all-digit brace, so a rule that
    // says "digits then a brace, then a plus, is possessive" refuses them.
    for (pattern, subject) in [(r"\x{41}+", "AAA"), (r"\u{1F600}+", "\u{1F600}")] {
        let re = Regex::compile(pattern)
            .unwrap_or_else(|e| panic!("`{pattern}` must be accepted, got {e}"));
        assert!(re.is_full_match(subject), "`{pattern}` should match {subject:?}");
    }
}

#[test]
fn a_nested_or_leading_bracket_class_does_not_derail_the_scanner() {
    // A class-depth counter, not a flag: `[a-z&&[^aeiou]]` opens a second
    // class inside the first, and `[]]` opens with a literal `]`. A scanner
    // that exits the class too early reads its contents as pattern syntax and
    // can refuse or accept the wrong thing.
    let re = Regex::compile("[a-z&&[^aeiou]]+").expect("class set operations are supported");
    assert!(re.is_full_match("bcd"));
    assert!(!re.is_full_match("aei"));
    let re = Regex::compile("[]]+").expect("a leading `]` is a literal");
    assert!(re.is_full_match("]]"));
}

#[test]
fn extended_mode_is_refused_because_its_comments_would_eat_the_anchor() {
    // The pattern is wrapped, so a `#` comment running to end-of-line would
    // swallow this engine's closing `)` and its `\z`.
    let e = Regex::compile("(?x) a b # comment").expect_err("must be refused");
    assert!(
        e.to_string().contains("extended mode"),
        "the refusal must name the feature, got: {e}",
    );
}
