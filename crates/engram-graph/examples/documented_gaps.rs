//! The refusals the book documents, asserted as refusals.
//!
//! `docs/book/src/using/cypher-support.md` and `known-limits.md` state what
//! Engram will not do. A gap page is a promise like any other, and it rots in
//! the more embarrassing direction: a limitation that is quietly fixed leaves
//! the documentation telling people not to use something that works.
//!
//! So every documented refusal is asserted here. When one of these starts
//! passing, this example fails and names the page to update.
//!
//! ```text
//! cargo run -p engram-graph --example documented_gaps
//! ```

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, RunError, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// Run a statement, returning either its rows or the engine's refusal.
fn try_run(g: &Graph, src: &str) -> Result<Vec<Vec<Value>>, String> {
    let stmt = match parse_statement(src) {
        Ok(s) => s,
        Err(e) => return Err(format!("parse: {e}")),
    };
    match run_query(g, &stmt, BTreeMap::new()) {
        Ok(r) => Ok(r.rows),
        Err(RunError::Unsupported(m)) => Err(format!("unsupported: {m}")),
        Err(e) => Err(format!("{e}")),
    }
}

fn demo() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));

    // ── `=~` EVALUATES, over a finite automaton ──────────────────────────
    //
    // This assertion was inverted on 2026-09-09. It used to require a
    // refusal, and it was right to: the grammar accepted `=~` while the
    // evaluator did not. `crates/engram-cypher/src/regex/` closed that gap,
    // and this canary is how the documentation pass found out — it is the
    // guard doing its job, not a test that rotted.
    //
    // What it pins now is the other direction. If `=~` ever stops answering,
    // cypher-support.md, known-limits.md and reference/regex.md all describe
    // an operator that no longer works.
    let regex =
        try_run(&g, "RETURN 'abc' =~ 'a.*' AS m").expect("`=~` evaluates — see reference/regex.md");
    assert_eq!(regex.len(), 1, "a full-match boolean is one row: {regex:?}");

    // ── UNION inside CALL {} now ANSWERS ─────────────────────────────────
    //
    // It refused until SNB BI bi4 needed it: that query's subquery counts
    // messages per person in one arm and adds back the top-forum members who
    // have none in the other, and `UNION ALL` between them is the only way to
    // say it. Each arm runs against the same seed row and the arms concatenate.
    //
    // Pinned as BEHAVIOUR rather than as a refusal, for the same reason `=~`
    // is: if this stops answering, cypher-support.md describes a feature the
    // engine no longer has.
    let union_in_call = try_run(&g, "CALL { RETURN 1 AS x UNION RETURN 2 AS x } RETURN x")
        .expect("UNION inside CALL {} is supported — see cypher-support.md");
    assert_eq!(
        union_in_call.len(),
        2,
        "two arms, two distinct values, two rows: {union_in_call:?}"
    );

    // ── ...while a top-level UNION works ─────────────────────────────────
    //
    // The page shows both, because "UNION is unsupported" would be wrong.
    let union_ok = try_run(&g, "RETURN 1 AS x UNION RETURN 2 AS x")
        .expect("top-level UNION is supported and the page says so");
    assert_eq!(union_ok.len(), 2, "UNION ALL semantics: two rows");

    // ── A standalone CALL returns its declared output columns ────────────
    //
    // Inverted on 2026-09-09, alongside the `=~` assertion above and for the
    // same reason. This used to require an EMPTY result, and it was the
    // quiet gap called out on four pages: a bare `CALL` did not error, it
    // simply answered nothing, which a Neo4j user reads as an empty database
    // rather than as a missing `YIELD`. A `CALL` that ends a query is now
    // itself the result, per openCypher, taking its columns from the
    // procedure catalogue.
    //
    // `YIELD` is still required when the `CALL` is not the last clause, and
    // the assertion below this one pins that half.
    let bare = try_run(&g, "CALL dbms.components()")
        .expect("a standalone CALL is its own result — see reference/procedures.md");
    assert!(
        !bare.is_empty(),
        "a standalone CALL returns the procedure's declared output columns; \
         if it goes back to answering nothing, getting-started.md, \
         cypher-support.md, procedures.md and known-limits.md all describe \
         behaviour that no longer exists: {bare:?}"
    );

    // ── ...and YIELD + RETURN is the form that works ─────────────────────
    let yielded = try_run(
        &g,
        "CALL dbms.components() YIELD name, versions, edition
         RETURN name, versions, edition",
    )
    .expect("YIELD + RETURN is the documented form");
    assert_eq!(yielded.len(), 1);
    assert!(
        matches!(&yielded[0][0], Value::Str(s) if s == "Engram"),
        "the page prints Engram as the component name: {:?}",
        yielded[0]
    );

    // ── Setting a property to null removes it ────────────────────────────
    //
    // core-concepts.md claimed these were distinguishable before this was
    // checked against a running engine. They are not.
    try_run(&g, "CREATE (:T {name: 'explicit-null', v: null})").expect("create");
    try_run(&g, "CREATE (:T {name: 'absent'})").expect("create");
    let keys = try_run(
        &g,
        "MATCH (n:T) RETURN n.name AS name, 'v' IN keys(n) AS has_v ORDER BY name",
    )
    .expect("read back");
    for row in &keys {
        assert!(
            matches!(row[1], Value::Bool(false)),
            "an explicit null and an absent property are indistinguishable to a \
             query — core-concepts.md says so: {row:?}"
        );
    }

    println!("documented_gaps: every documented refusal still refuses");
}

fn main() {
    demo();
}

/// `cargo test --examples` COMPILES an example but does not run its `main`, so
/// an example alone proves the snippet type-checks and nothing about whether it
/// still answers. This is what makes the assertions above run in CI.
#[cfg(test)]
mod tests {
    #[test]
    fn the_documented_behaviour_still_holds() {
        super::demo();
    }
}
