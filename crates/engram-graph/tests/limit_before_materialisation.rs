#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! `RETURN m.id LIMIT 25` must not decode every matching node.
//!
//! # The defect this pins
//!
//! `project_rows_tail` materialised EVERY row of the chunk — and materialising
//! a Node var decodes the whole record — and only then did `project_tail`
//! apply SKIP/LIMIT.
//!
//! Measured on the pod against official LDBC SF1, single client, with
//! `ENGRAM_TRACE_COUNTERS=1`:
//!
//! | statement | `nodes materialised in full` | `store.gets` |
//! |---|---|---|
//! | `MATCH (p:Person {id: 933})<-[:HAS_CREATOR]-(m:Message) RETURN m.id LIMIT 25` | **1,212** | **1,212** |
//! | the same walk, `RETURN count(*)` | **0** | **0** |
//!
//! 1,212 is that person's entire message count. The control — the identical
//! traversal with the projection removed — did one adjacency lookup and no
//! decodes at all, which is what says the decode WAS the shape's whole cost
//! (3.6 ms, against Neo4j's 2.40 ms on the same corpus).
//!
//! # Why a prefix is sound
//!
//! `project_tail` truncates the SAME sequence at the SAME point, so taking the
//! prefix early is byte-identical — but only when nothing between may reorder
//! or drop rows. An ORDER BY chooses which rows survive; DISTINCT can dedup a
//! `limit`-row prefix down to fewer than `limit` and under-answer. Both are
//! asserted below, because a pushdown that fires on them is a WRONG-ANSWER bug
//! and not a slow one.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run {src}: {e:?}"))
        .rows
}

/// One person with `MSGS` messages — the is5 shape, scaled to a unit test.
const MSGS: i64 = 400;

fn fixture() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run_stmt(
        &g,
        &parse_any("CREATE INDEX person_id FOR (n:Person) ON (n.id)").expect("parse index"),
        BTreeMap::new(),
    )
    .expect("index");
    rows(&g, "CREATE (:Person {id: 0, firstName: 'hub'})");
    rows(&g, "CREATE (:Person {id: 1, firstName: 'other'})");
    rows(
        &g,
        &format!(
            "UNWIND range(0, {}) AS i MATCH (p:Person {{id: 0}}) \
             CREATE (m:Message {{id: i, body: 'x'}})-[:HAS_CREATOR]->(p)",
            MSGS - 1
        ),
    );
    g.shared_store().seal();
    g
}

fn full_decodes(g: &Graph, src: &str) -> u64 {
    let q = parse_statement(src).expect("parse");
    let (r, t) = engram_observe::with_trace(|| run_query(g, &q, BTreeMap::new()));
    r.expect("run");
    t.counters()
        .get("graph.nodes materialised in full")
        .copied()
        .unwrap_or(0)
}

#[test]
fn a_limited_projection_decodes_only_the_rows_it_returns() {
    let g = fixture();
    const LIMIT: usize = 25;
    let src = format!(
        "MATCH (p:Person {{id: 0}})<-[:HAS_CREATOR]-(m:Message) RETURN m.id LIMIT {LIMIT}"
    );

    let answer = rows(&g, &src);
    assert_eq!(answer.len(), LIMIT, "the fixture must actually hit the limit");

    let decodes = full_decodes(&g, &src);
    eprintln!("[limit] {MSGS} messages, LIMIT {LIMIT} -> {decodes} full node decode(s)");
    assert!(
        decodes <= LIMIT as u64,
        "decoded {decodes} nodes to return {LIMIT} rows — the projection is \
         materialising the whole match before the limit applies"
    );

    // THE CANARY. The bound above is only evidence if the counter can reach
    // {MSGS}: without the pushdown this same fixture decodes every message, and
    // `LIMIT {MSGS}` is that run — same query, same rows, limit lifted.
    let unlimited = full_decodes(
        &g,
        &format!("MATCH (p:Person {{id: 0}})<-[:HAS_CREATOR]-(m:Message) RETURN m.id LIMIT {MSGS}"),
    );
    assert_eq!(
        unlimited, MSGS as u64,
        "the counter must reach {MSGS} when the limit does not bind, or the \
         bound above is the absence of instrumentation rather than of work"
    );

    // ANSWERS UNCHANGED: the prefix must be exactly what truncating late gives.
    let all = rows(
        &g,
        "MATCH (p:Person {id: 0})<-[:HAS_CREATOR]-(m:Message) RETURN m.id",
    );
    assert_eq!(
        answer,
        all[..LIMIT].to_vec(),
        "the limited rows must be the unlimited rows' prefix"
    );
}

/// ORDER BY chooses WHICH rows survive, so no prefix of the unordered input is
/// the answer. DISTINCT can dedup a `limit`-row prefix to fewer than `limit`.
/// A pushdown that fired on either would answer WRONGLY, so both are pinned by
/// their answers and not by a counter.
#[test]
fn the_pushdown_must_not_fire_where_it_would_change_the_answer() {
    let g = fixture();

    let ordered = rows(
        &g,
        "MATCH (p:Person {id: 0})<-[:HAS_CREATOR]-(m:Message) RETURN m.id ORDER BY m.id DESC LIMIT 3",
    );
    let got: Vec<i64> = ordered
        .iter()
        .map(|r| match r[0] {
            Value::Int(i) => i,
            ref v => panic!("id is not an Int: {v:?}"),
        })
        .collect();
    assert_eq!(
        got,
        vec![MSGS - 1, MSGS - 2, MSGS - 3],
        "ORDER BY … DESC LIMIT 3 must return the LAST three ids, which no \
         prefix of the traversal order contains"
    );

    // Every message here has `body: 'x'`, so DISTINCT over it collapses 400
    // rows to ONE. A prefix taken before the dedup would still be one row —
    // so the case that discriminates is a limit ABOVE the distinct count.
    let distinct = rows(
        &g,
        "MATCH (p:Person {id: 0})<-[:HAS_CREATOR]-(m:Message) RETURN DISTINCT m.body LIMIT 5",
    );
    assert_eq!(
        distinct.len(),
        1,
        "DISTINCT over one repeated value is one row, whatever the limit"
    );
}

/// `RETURN *` carries every variable BY NAME and is a flag beside the items,
/// not an expansion into them — so var pruning must keep every var under one.
/// Pinned by the ANSWER: a pruned var would come back missing or null.
#[test]
fn a_star_projection_keeps_every_var() {
    let g = fixture();
    let starred = rows(
        &g,
        "MATCH (p:Person {id: 0})<-[:HAS_CREATOR]-(m:Message) RETURN * LIMIT 3",
    );
    assert_eq!(starred.len(), 3, "the star projection must answer rows");
    for r in &starred {
        assert!(
            r.len() >= 2,
            "RETURN * must carry BOTH bound vars, got {} column(s): {r:?}",
            r.len()
        );
        for v in r {
            assert!(
                !matches!(v, Value::Null),
                "no column of a star projection may be null: {r:?}"
            );
        }
    }
}
