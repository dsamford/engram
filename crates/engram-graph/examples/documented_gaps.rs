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

    // ── `=~` parses, then refuses at evaluation ──────────────────────────
    //
    // The distinction the page draws: the GRAMMAR accepts it, the evaluator
    // does not. If it ever starts answering, cypher-support.md and
    // known-limits.md both need the entry removed.
    let regex = try_run(&g, "RETURN 'abc' =~ 'a.*' AS m");
    let msg = regex.expect_err("`=~` must still refuse — see cypher-support.md");
    assert!(
        msg.contains("=~"),
        "the refusal should name the operator so a user can find the page: {msg}"
    );

    // ── UNION inside CALL {} is refused ──────────────────────────────────
    let union_in_call = try_run(
        &g,
        "CALL { RETURN 1 AS x UNION RETURN 2 AS x } RETURN x",
    );
    assert!(
        union_in_call.is_err(),
        "UNION inside CALL {{}} must still refuse — see cypher-support.md"
    );

    // ── ...while a top-level UNION works ─────────────────────────────────
    //
    // The page shows both, because "UNION is unsupported" would be wrong.
    let union_ok = try_run(&g, "RETURN 1 AS x UNION RETURN 2 AS x")
        .expect("top-level UNION is supported and the page says so");
    assert_eq!(union_ok.len(), 2, "UNION ALL semantics: two rows");

    // ── A standalone CALL yields no rows ─────────────────────────────────
    //
    // The quiet one, and the reason it is called out on three pages: it does
    // not error, it returns nothing. A Neo4j user reads that as an empty
    // database rather than as a missing YIELD.
    let bare = try_run(&g, "CALL dbms.components()")
        .expect("a bare CALL is accepted, it simply answers nothing");
    assert!(
        bare.is_empty(),
        "a standalone CALL yields no rows here; if it starts yielding, \
         getting-started.md, cypher-support.md, procedures.md and \
         known-limits.md all describe behaviour that no longer exists: {bare:?}"
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
