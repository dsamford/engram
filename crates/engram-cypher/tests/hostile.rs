//! Hostile-input tests for the Cypher front end.
//!
//! The query string arrives from the Bolt wire with no length limit and goes
//! straight into `parse_any`. Everything here is therefore reachable by an
//! UNAUTHENTICATED client, and a failure is a dead process rather than a wrong
//! answer.
//!
//! The tokenizer was already careful (unterminated block comments and strings
//! are guarded before the lookahead read; integer and float overflow are
//! refused rather than silently promoted, and there is no regex engine so
//! there is no ReDoS). What the parser had no defence against was DEPTH: it is
//! recursive descent, and a nested expression costs one frame per level.

use engram_cypher::{MIN_PARSER_STACK_BYTES, parse_any, parse_expression};

/// Run `f` on a thread carrying exactly the stack the parser documents as its
/// requirement.
///
/// This is not test scaffolding, it is the assertion. The depth limit only
/// bounds anything if the stack is large enough for the limit to be REACHED
/// first — on a 1 MiB stack an unoptimised build overflows around 35 levels,
/// below the 64 the limit allows, so the guard never fires and the process
/// dies anyway. Pinning the stack here means these tests prove the stated
/// contract ("this limit is safe given this much stack") rather than proving
/// whatever the platform happened to default to, which is a number that
/// changes between OSes and between debug and release.
///
/// The workspace bans `std::thread::spawn` to keep the ENGINE deterministic;
/// this is a test harness sizing its own stack, which touches neither the
/// engine nor its trace, so the ban is locally waived with cause.
#[allow(clippy::disallowed_methods)]
fn on_a_sized_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(MIN_PARSER_STACK_BYTES)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("the parser must REFUSE a bomb, not overflow the stack")
}

/// The openCypher TCK nests list literals 40 deep in `Literals7`/`Literals8`.
/// The limit must clear that, or the fix costs conformance — which would be
/// trading one defect for another.
#[test]
fn the_tck_conformance_floor_still_parses() {
    on_a_sized_stack(|| {
        let src = format!("RETURN {}1{}", "[".repeat(40), "]".repeat(40));
        assert!(
            parse_any(&src).is_ok(),
            "40 levels is the TCK's deepest literal; the limit must sit above it"
        );
    });
}

/// Realistic nesting still parses. A limit that real queries hit is a bug.
#[test]
fn deeply_but_reasonably_nested_expressions_still_parse() {
    on_a_sized_stack(|| {
        // The deepest expression in a 3,547-statement real application corpus
        // is 5. Ten is already generous.
        let src = format!("RETURN {}1{}", "(".repeat(10), ")".repeat(10));
        assert!(
            parse_any(&src).is_ok(),
            "ordinary nesting must parse; the limit is for bombs, not for queries"
        );
    });
}

/// The attack: balanced parens deep enough to exhaust the stack.
///
/// THIS IS THE CANARY FOR THE FIX. Before the depth limit this recursed ~200k
/// frames and aborted the process, so it could not have been written as a
/// `Result` assertion at all — there was no `Err` to observe. That it now
/// returns an error rather than killing the test runner IS the assertion.
#[test]
fn a_deep_paren_bomb_returns_an_error_instead_of_aborting() {
    on_a_sized_stack(|| {
        let src = format!("RETURN {}1{}", "(".repeat(200_000), ")".repeat(200_000));
        assert!(
            parse_any(&src).is_err(),
            "a paren bomb must be refused, not survived by luck"
        );
    });
}

/// Unbalanced is the cheaper attack — half the bytes, same recursion, and it
/// never reaches a token that could be rejected on its own merits.
#[test]
fn an_unbalanced_paren_bomb_is_refused() {
    on_a_sized_stack(|| {
        let src = format!("RETURN {}", "(".repeat(200_000));
        assert!(
            parse_any(&src).is_err(),
            "unbalanced nesting must be refused"
        );
    });
}

/// List literals recurse through a different arm than parentheses.
#[test]
fn a_deep_list_literal_bomb_is_refused() {
    on_a_sized_stack(|| {
        let src = format!("RETURN {}1{}", "[".repeat(100_000), "]".repeat(100_000));
        assert!(parse_any(&src).is_err(), "list nesting must be bounded");
    });
}

/// So do function-argument lists.
#[test]
fn a_deep_function_call_bomb_is_refused() {
    on_a_sized_stack(|| {
        let src = format!("RETURN {}1{}", "abs(".repeat(100_000), ")".repeat(100_000));
        assert!(parse_any(&src).is_err(), "call nesting must be bounded");
    });
}

/// And unary operators, which recurse without any bracket at all.
#[test]
fn a_deep_unary_bomb_is_refused() {
    on_a_sized_stack(|| {
        let src = format!("RETURN {}1", "-".repeat(200_000));
        assert!(
            parse_any(&src).is_err(),
            "unary nesting must be bounded — it costs one BYTE per level"
        );
        let src = format!("RETURN {}true", "NOT ".repeat(100_000));
        assert!(parse_any(&src).is_err(), "NOT nesting must be bounded");
    });
}

/// Depth must not leak across siblings: a long flat list is depth 2, not
/// depth N. This is the regression a naive counter with an early return
/// introduces, and it would reject perfectly ordinary queries.
#[test]
fn depth_does_not_leak_across_sibling_expressions() {
    on_a_sized_stack(|| {
        let items = (0..2_000)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let src = format!("RETURN [{items}]");
        assert!(
            parse_any(&src).is_ok(),
            "2,000 SIBLING elements are depth 2, not depth 2000"
        );

        // Same point for a long binary chain: `1 + 1 + 1 + …` is left-associative
        // and iterative in the ladder, so it must not consume depth per term.
        let chain = vec!["1"; 5_000].join(" + ");
        assert!(
            parse_expression(&chain).is_ok(),
            "a long flat operator chain must not be treated as deep nesting"
        );
    });
}

/// A refused deep expression must not poison a later parse.
#[test]
fn a_refusal_does_not_poison_subsequent_parses() {
    on_a_sized_stack(|| {
        let bomb = format!("RETURN {}1", "(".repeat(200_000));
        assert!(parse_any(&bomb).is_err());
        // A fresh, ordinary statement must be entirely unaffected.
        assert!(
            parse_any("MATCH (n:Person) WHERE n.age > 30 RETURN n.name").is_ok(),
            "an ordinary query must still parse after a bomb was refused"
        );
    });
}

/// Ordinary syntax errors must still report as syntax errors, not as depth.
/// The guard sits in front of the recursion, which is the kind of change that
/// can start swallowing unrelated errors.
#[test]
fn ordinary_syntax_errors_are_unaffected() {
    on_a_sized_stack(|| {
        let e = parse_any("MATCH (n) RETURN").unwrap_err().to_string();
        assert!(
            !e.contains("nested"),
            "a truncated RETURN is not a depth problem: {e}"
        );
    });
}
