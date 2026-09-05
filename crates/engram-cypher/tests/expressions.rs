#![allow(non_snake_case)]
//! The expression layer — precedence, three-valued logic, semantics.

use std::collections::BTreeMap;

use engram_cypher::{
    EvalError, LexError, ParseError, Scope, TokenKind, Truth, Value, eval, parse_expression,
    tokenize,
};

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

// ─── Precedence and parsing ─────────────────────────────────────────────────

#[test]
fn arithmetic_precedence_is_the_usual_ladder() {
    assert_eq!(v("1 + 2 * 3"), Value::Int(7));
    assert_eq!(v("(1 + 2) * 3"), Value::Int(9));
    assert_eq!(v("2 * 3 ^ 2"), Value::Float(18.0), "^ binds tighter than *");
    assert_eq!(
        v("-2 ^ 2"),
        Value::Float(4.0),
        "unary minus binds tighter than ^ (openCypher)"
    );
    assert_eq!(v("7 - 2 - 1"), Value::Int(4), "left associative");
}

#[test]
fn boolean_ladder_is_or_xor_and_not() {
    assert_eq!(
        v("true OR false AND false"),
        Value::Bool(true),
        "AND binds tighter"
    );
    assert_eq!(v("(true OR false) AND false"), Value::Bool(false));
    assert_eq!(
        v("NOT true OR true"),
        Value::Bool(true),
        "NOT binds tighter than OR"
    );
    assert_eq!(
        v("true XOR true OR true"),
        Value::Bool(true),
        "XOR binds tighter than OR"
    );
}

#[test]
fn string_operators_bind_tighter_than_comparison() {
    // openCypher: STARTS WITH sits between arithmetic and `=`.
    assert_eq!(v("'ab' STARTS WITH 'a' = true"), Value::Bool(true));
}

#[test]
fn chained_comparisons_are_the_conjunction() {
    assert_eq!(v("1 < 2 < 3"), Value::Bool(true));
    assert_eq!(v("1 < 2 < 0"), Value::Bool(false));
    assert_eq!(
        v("5 < 2 < 3"),
        Value::Bool(false),
        "the FIRST leg failing fails the chain"
    );
    assert_eq!(
        v("1 < 2 < 0 < 9"),
        Value::Bool(false),
        "a middle leg failing fails the chain"
    );
    assert_eq!(
        v("1 < null < 3"),
        Value::Null,
        "a null leg poisons the chain"
    );
    assert_eq!(v("3 > 2 >= 2"), Value::Bool(true));
}

#[test]
fn parse_refusals_name_position_and_expectation() {
    let e = parse_expression("1 + ").unwrap_err();
    match e {
        ParseError::Unexpected { expected, at, .. } => {
            assert!(
                expected.contains("expression"),
                "names what it wanted: {expected}"
            );
            assert_eq!(at, 4, "positions at the byte");
        }
        other => panic!("expected Unexpected, got {other:?}"),
    }
    assert!(matches!(
        parse_expression("{a: 1, a: 2}").unwrap_err(),
        ParseError::DuplicateMapKey { .. }
    ));
    // Trailing input is refused, not ignored.
    assert!(matches!(
        parse_expression("1 2").unwrap_err(),
        ParseError::Unexpected { .. }
    ));
}

#[test]
fn the_tokenizer_covers_the_corpus_shapes() {
    let toks = tokenize("n.`weird name` // line\n /* block */ 0x1F 0o17 1.5e-3 $p <> <= ..")
        .expect("tokenizes");
    let kinds: Vec<&TokenKind> = toks.iter().map(|t| &t.kind).collect();
    assert!(matches!(kinds[0], TokenKind::Ident(n) if n == "n"));
    assert!(matches!(kinds[2], TokenKind::Ident(n) if n == "weird name"));
    assert!(matches!(kinds[3], TokenKind::Int(31)));
    assert!(matches!(kinds[4], TokenKind::Int(15)));
    assert!(matches!(kinds[5], TokenKind::Float(f) if (*f - 0.0015).abs() < 1e-12));
    assert!(matches!(kinds[6], TokenKind::Param(p) if p == "p"));
    assert!(matches!(kinds[7], TokenKind::Neq));
    assert!(matches!(kinds[8], TokenKind::Le));
    assert!(matches!(kinds[9], TokenKind::DotDot));

    assert!(matches!(
        tokenize("'unterminated"),
        Err(LexError::Unterminated {
            what: "string",
            at: 0
        })
    ));
    assert!(matches!(
        tokenize("9999999999999999999999"),
        Err(LexError::BadNumber { .. })
    ));
}

#[test]
fn keywords_are_case_insensitive_and_identifiers_are_not() {
    assert_eq!(v("TRUE and true AND True"), Value::Bool(true));
    let toks = tokenize("Match somevar SOMEVAR").expect("tokenizes");
    assert!(matches!(&toks[0].kind, TokenKind::Keyword("MATCH")));
    assert!(matches!(&toks[1].kind, TokenKind::Ident(n) if n == "somevar"));
    assert!(matches!(&toks[2].kind, TokenKind::Ident(n) if n == "SOMEVAR"));
}

// ─── Three-valued logic — the load-bearing table ────────────────────────────

#[test]
fn null_equals_anything_is_null_and_fails_closed() {
    // THE case the plan names: `null = 'x'` must be null (which a filter
    // reads as "does not pass"), never true and never an error.
    assert_eq!(v("null = 'x'"), Value::Null);
    assert_eq!(v("null = null"), Value::Null);
    assert_eq!(v("null <> null"), Value::Null);
    assert_eq!(v("null < 1"), Value::Null);
}

#[test]
fn the_truth_tables() {
    assert_eq!(
        v("null AND false"),
        Value::Bool(false),
        "false dominates AND"
    );
    assert_eq!(v("null AND true"), Value::Null);
    assert_eq!(v("null OR true"), Value::Bool(true), "true dominates OR");
    assert_eq!(v("null OR false"), Value::Null);
    assert_eq!(v("NOT null"), Value::Null);
    assert_eq!(v("null XOR true"), Value::Null);
    assert_eq!(v("null XOR null"), Value::Null);
}

#[test]
fn comparability_vs_equality_across_types() {
    // openCypher: `=` across incomparable types is FALSE; `<` is NULL.
    assert_eq!(v("1 = 'a'"), Value::Bool(false));
    assert_eq!(v("1 < 'a'"), Value::Null);
    assert_eq!(
        v("1 = 1.0"),
        Value::Bool(true),
        "ints and floats compare numerically"
    );
    assert_eq!(v("1 < 1.5"), Value::Bool(true));
}

#[test]
fn is_null_does_not_propagate() {
    assert_eq!(v("null IS NULL"), Value::Bool(true));
    assert_eq!(v("null IS NOT NULL"), Value::Bool(false));
    assert_eq!(v("1 IS NULL"), Value::Bool(false));
    assert_eq!(v("1 IS NOT NULL"), Value::Bool(true));
}

#[test]
fn IN_follows_the_membership_rules() {
    assert_eq!(v("1 IN [1, 2]"), Value::Bool(true));
    assert_eq!(v("3 IN [1, 2]"), Value::Bool(false));
    assert_eq!(
        v("3 IN [1, null]"),
        Value::Null,
        "an unknown element poisons a miss"
    );
    assert_eq!(v("1 IN [1, null]"), Value::Bool(true), "but a hit is a hit");
    assert_eq!(
        v("null IN []"),
        Value::Bool(false),
        "nothing is in the empty list, not even null"
    );
    assert_eq!(v("null IN [1]"), Value::Null);
    assert_eq!(v("1 IN null"), Value::Null);
}

#[test]
fn list_and_map_equality_is_deep_and_three_valued() {
    assert_eq!(v("[1, 2] = [1, 2]"), Value::Bool(true));
    assert_eq!(v("[1, 2] = [1, 3]"), Value::Bool(false));
    assert_eq!(v("[1, null] = [1, 2]"), Value::Null);
    assert_eq!(
        v("[1, null] = [2, null]"),
        Value::Bool(false),
        "a definite mismatch dominates"
    );
    assert_eq!(v("{a: 1} = {a: 1}"), Value::Bool(true));
    assert_eq!(v("{a: 1} = {b: 1}"), Value::Bool(false));
    assert_eq!(v("{a: null} = {a: 1}"), Value::Null);
}

// ─── Arithmetic semantics ───────────────────────────────────────────────────

#[test]
fn integer_division_and_the_zero_refusals() {
    assert_eq!(v("7 / 2"), Value::Int(3), "int / int is integer division");
    assert_eq!(v("7.0 / 2"), Value::Float(3.5));
    assert_eq!(v("7 % 3"), Value::Int(1));
    assert_eq!(err("1 / 0"), EvalError::DivisionByZero);
    assert_eq!(err("1 % 0"), EvalError::DivisionByZero);
    assert_eq!(
        v("1.0 / 0"),
        Value::Float(f64::INFINITY),
        "float division follows IEEE"
    );
}

#[test]
fn integer_overflow_REFUSES_rather_than_wrapping_or_floating() {
    assert_eq!(err("9223372036854775807 + 1"), EvalError::Overflow("+"));
    assert_eq!(err("-9223372036854775807 - 2"), EvalError::Overflow("-"));
    assert_eq!(err("9223372036854775807 * 2"), EvalError::Overflow("*"));
}

#[test]
fn plus_concatenates_strings_and_lists() {
    assert_eq!(v("'a' + 'b'"), Value::Str("ab".into()));
    assert_eq!(v("'a' + 1"), Value::Str("a1".into()));
    assert_eq!(v("1 + 'a'"), Value::Str("1a".into()));
    assert_eq!(
        v("[1] + [2]"),
        Value::List(vec![Value::Int(1), Value::Int(2)])
    );
    assert_eq!(
        v("[1] + 2"),
        Value::List(vec![Value::Int(1), Value::Int(2)]),
        "list appends"
    );
    assert_eq!(v("null + 1"), Value::Null);
    assert!(matches!(err("true + 1"), EvalError::Type { .. }));
}

// ─── Access: property, index, slice ─────────────────────────────────────────

#[test]
fn property_index_and_slice_semantics() {
    assert_eq!(v("{a: 7}.a"), Value::Int(7));
    assert_eq!(
        v("{a: 7}.b"),
        Value::Null,
        "a missing key is null, not an error"
    );
    assert_eq!(v("null.a"), Value::Null);
    assert_eq!(v("[10, 20, 30][0]"), Value::Int(10));
    assert_eq!(
        v("[10, 20, 30][-1]"),
        Value::Int(30),
        "negative indexes from the end"
    );
    assert_eq!(v("[10][9]"), Value::Null, "out of range is null");
    assert_eq!(v("{a: 1}['a']"), Value::Int(1));
    assert_eq!(
        v("[1, 2, 3, 4][1..3]"),
        Value::List(vec![Value::Int(2), Value::Int(3)])
    );
    assert_eq!(
        v("[1, 2, 3][..2]"),
        Value::List(vec![Value::Int(1), Value::Int(2)])
    );
    assert_eq!(
        v("[1, 2, 3][1..]"),
        Value::List(vec![Value::Int(2), Value::Int(3)])
    );
    assert_eq!(
        v("[1, 2, 3][-2..]"),
        Value::List(vec![Value::Int(2), Value::Int(3)])
    );
    assert_eq!(
        v("[1, 2][5..9]"),
        Value::List(vec![]),
        "out-of-range slice clamps"
    );
    assert_eq!(
        v("[1, 2][null..1]"),
        Value::Null,
        "a null bound nulls the slice"
    );
}

#[test]
fn map_keys_and_property_names_keep_the_SOURCE_spelling_of_keywords() {
    // `count` is a keyword to the tokenizer; as a map key or property name it
    // must keep the user's spelling, not the canonical COUNT.
    assert_eq!(v("{count: 7}.count"), Value::Int(7));
    assert_eq!(v("{Match: 1}['Match']"), Value::Int(1));
}

// ─── CASE, comprehensions, reduce ───────────────────────────────────────────

#[test]
fn both_case_forms_work_and_null_subjects_fall_through() {
    assert_eq!(
        v("CASE 2 WHEN 1 THEN 'a' WHEN 2 THEN 'b' ELSE 'c' END"),
        Value::Str("b".into())
    );
    assert_eq!(
        v("CASE WHEN false THEN 1 WHEN true THEN 2 END"),
        Value::Int(2)
    );
    assert_eq!(
        v("CASE WHEN false THEN 1 END"),
        Value::Null,
        "no arm, no else: null"
    );
    // A null subject matches NO arm (null = x is never true), so: else.
    assert_eq!(
        v("CASE null WHEN null THEN 'hit' ELSE 'miss' END"),
        Value::Str("miss".into())
    );
}

#[test]
fn list_comprehensions_filter_and_map() {
    assert_eq!(
        v("[x IN [1, 2, 3] WHERE x > 1 | x * 10]"),
        Value::List(vec![Value::Int(20), Value::Int(30)])
    );
    assert_eq!(
        v("[x IN [1, 2] | x + 1]"),
        Value::List(vec![Value::Int(2), Value::Int(3)])
    );
    assert_eq!(
        v("[x IN [1, 2, 3] WHERE x <> 2]"),
        Value::List(vec![Value::Int(1), Value::Int(3)])
    );
    assert_eq!(v("[x IN null | x]"), Value::Null);
    // A null filter verdict fails closed, per row.
    assert_eq!(
        v("[x IN [1, null, 3] WHERE x > 0]"),
        Value::List(vec![Value::Int(1), Value::Int(3)])
    );
}

#[test]
fn reduce_folds() {
    assert_eq!(
        v("reduce(acc = 0, x IN [1, 2, 3] | acc + x)"),
        Value::Int(6)
    );
    assert_eq!(
        v("reduce(s = '', x IN ['a', 'b'] | s + x)"),
        Value::Str("ab".into())
    );
    assert_eq!(v("reduce(acc = 0, x IN null | acc + x)"), Value::Null);
}

// ─── Functions ──────────────────────────────────────────────────────────────

#[test]
fn the_scalar_registry() {
    assert_eq!(v("coalesce(null, null, 3)"), Value::Int(3));
    assert_eq!(v("coalesce(null)"), Value::Null);
    assert_eq!(
        v("size('héllo')"),
        Value::Int(5),
        "size counts characters, not bytes"
    );
    assert_eq!(v("size([1, 2])"), Value::Int(2));
    assert_eq!(v("head([7, 8])"), Value::Int(7));
    assert_eq!(v("last([7, 8])"), Value::Int(8));
    assert_eq!(v("head([])"), Value::Null);
    assert_eq!(v("toString(1.5)"), Value::Str("1.5".into()));
    assert_eq!(v("toInteger('42')"), Value::Int(42));
    assert_eq!(
        v("toInteger('42x')"),
        Value::Null,
        "an unparseable string is null, not an error"
    );
    assert_eq!(v("toFloat('2.5')"), Value::Float(2.5));
    assert_eq!(v("toUpper('ab')"), Value::Str("AB".into()));
    assert_eq!(v("trim('  x ')"), Value::Str("x".into()));
    assert_eq!(
        v("split('a,b', ',')"),
        Value::List(vec![Value::Str("a".into()), Value::Str("b".into())])
    );
    assert_eq!(
        v("range(0, 3)"),
        Value::List((0..=3).map(Value::Int).collect()),
        "range is INCLUSIVE of the end"
    );
    assert_eq!(
        v("range(3, 0, -2)"),
        Value::List(vec![Value::Int(3), Value::Int(1)])
    );
    assert_eq!(
        v("keys({b: 1, a: 2})"),
        Value::List(vec![Value::Str("a".into()), Value::Str("b".into())])
    );
    assert_eq!(v("exists(null)"), Value::Bool(false));
    assert_eq!(v("exists(1)"), Value::Bool(true));
    assert_eq!(
        v("reverse([1, 2])"),
        Value::List(vec![Value::Int(2), Value::Int(1)])
    );
    assert_eq!(v("abs(-3)"), Value::Int(3));
}

#[test]
fn function_names_are_case_insensitive() {
    assert_eq!(v("COALESCE(null, 1)"), Value::Int(1));
    assert_eq!(v("ToString(5)"), Value::Str("5".into()));
}

#[test]
fn the_five_apoc_functions_live() {
    assert_eq!(
        v("apoc.convert.toJson({b: [1, true], a: 'x'})"),
        Value::Str(r#"{"a":"x","b":[1,true]}"#.into())
    );
    assert_eq!(
        v("apoc.convert.fromJsonList('[1, \"a\", null]')"),
        Value::List(vec![Value::Int(1), Value::Str("a".into()), Value::Null])
    );
    let m = v("apoc.convert.fromJsonMap('{\"k\": {\"n\": 1.5}}')");
    let mut inner = BTreeMap::new();
    inner.insert("n".to_string(), Value::Float(1.5));
    let mut outer = BTreeMap::new();
    outer.insert("k".to_string(), Value::Map(inner));
    assert_eq!(m, Value::Map(outer));
    assert_eq!(
        v("apoc.coll.toSet([3, 1, 3, 2, 1])"),
        Value::List(vec![Value::Int(3), Value::Int(1), Value::Int(2)]),
        "first occurrence wins, order preserved"
    );
    assert_eq!(
        v("apoc.coll.min([3, null, 1, 2])"),
        Value::Int(1),
        "nulls are skipped"
    );
    assert_eq!(v("apoc.coll.min([null])"), Value::Null);
    // A JSON round trip through both directions.
    assert_eq!(
        v("apoc.convert.fromJsonMap(apoc.convert.toJson({a: [1, 'x']}))"),
        v("{a: [1, 'x']}")
    );
}

#[test]
fn refusals_are_NAMED_never_null() {
    assert!(
        matches!(err("definitely_not_a_function(1)"), EvalError::UnknownFunction(n) if n == "definitely_not_a_function")
    );
    assert!(matches!(
        err("count(x)"),
        EvalError::AggregateInScalarContext(_)
    ));
    assert!(
        matches!(err("count(*)"), EvalError::AggregateInScalarContext(_)),
        "count(*) parses and is refused HERE, not at the parser"
    );
    assert!(matches!(err("size(1)"), EvalError::Function { .. }));
    assert_eq!(err("'a' =~ 'a.*'"), EvalError::RegexUnsupported);
    assert_eq!(
        v("null =~ 'a'"),
        Value::Null,
        "null still propagates past the unsupported regex"
    );
}

#[test]
fn params_resolve_and_absence_is_not_null() {
    let params: BTreeMap<String, Value> = [
        ("p".to_string(), Value::Int(9)),
        ("n".to_string(), Value::Null),
    ]
    .into_iter()
    .collect();
    let vars = engram_cypher::bindings::VarMap::new();
    let scope = Scope::over(&params, &vars, None, None);
    let e = parse_expression("$p + 1").expect("parses");
    assert_eq!(eval(&e, &scope).expect("evals"), Value::Int(10));
    let e = parse_expression("$n").expect("parses");
    assert_eq!(
        eval(&e, &scope).expect("evals"),
        Value::Null,
        "a SUPPLIED null is a value"
    );
    let e = parse_expression("$missing").expect("parses");
    assert!(
        matches!(eval(&e, &scope), Err(EvalError::UnknownParam(p)) if p == "missing"),
        "an ABSENT parameter is a refusal — absence is not a value"
    );
}

#[test]
fn dotted_function_names_vs_property_access() {
    // `apoc.coll.min(...)` is a call; `m.a` on a bound map is a property
    // access; the parser must not confuse them.
    let mut scope = Scope::default();
    let mut m = BTreeMap::new();
    m.insert("a".to_string(), Value::Int(5));
    scope.bind("m", Value::Map(m));
    let e = parse_expression("m.a + apoc.coll.min([2, 1])").expect("parses");
    assert_eq!(eval(&e, &scope).expect("evals"), Value::Int(6));
}

#[test]
fn truth_helper_matches_the_tables() {
    assert_eq!(Truth::Unknown.and(Truth::False), Truth::False);
    assert_eq!(Truth::Unknown.or(Truth::True), Truth::True);
    assert_eq!(!Truth::Unknown, Truth::Unknown);
    assert_eq!(
        Truth::Unknown.to_value(),
        Value::Null,
        "Unknown is null, never false"
    );
}
