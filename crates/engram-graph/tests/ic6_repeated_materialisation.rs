#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! `ic6-friend-tags` is 55% of read time at a 90 ms median — is it re-decoding
//! the same records?
//!
//! # The measurement that prompts this
//!
//! The stress harness's per-shape table, `read-only` at 8 clients on official
//! LDBC SF1, sorted by share of read time:
//!
//! | shape | ops | p50 | % of read time |
//! |---|---|---|---|
//! | **ic6-friend-tags** | 1,384 (5%) | **90.29 ms** | **55.1%** |
//! | is3-friends | 6,868 | 0.77 ms | 9.7% |
//! | is1-profile | 9,635 | 0.20 ms | 9.2% |
//!
//! Five percent of read operations take fifty-five percent of read time.
//! Everything else in the mix is under 7 ms. The read path is not broadly slow;
//! this one shape is.
//!
//! # The hypothesis under test
//!
//! ```cypher
//! MATCH (p:Person {id: K})-[:KNOWS]-(f:Person)<-[:HAS_CREATOR]-(m:Message)
//! MATCH (m)-[:HAS_TAG]->(t:Tag)
//! RETURN t.name, count(*) AS c ORDER BY c DESC LIMIT 10
//! ```
//!
//! The rows fan out — friends x their messages x each message's tags — but the
//! TAGS DO NOT. SF1 has on the order of 16,000 tags against tens of thousands
//! of rows per query, so the same `t` is projected many times. `mat_node`
//! (interp.rs) calls `graph.node_projected(id, set)` per row with no memo, so
//! each occurrence decodes that record again.
//!
//! # What this file establishes, and what it does not
//!
//! It measures the RATIO of node materialisations to distinct nodes on an
//! ic6-shaped fixture. A ratio near 1 refutes the hypothesis and says the 90 ms
//! is elsewhere — which is a useful answer, and the reason this is a
//! measurement rather than an optimisation.
//!
//! It does NOT establish that a memo is worth building: that needs the ratio
//! AND the share of the query's time that materialisation accounts for, and
//! only the pod can supply the second.

use std::collections::BTreeMap;

use engram_cypher::parse_statement;
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn stmt(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    let txn = g.open_txn();
    let (txn, r) = g.with_txn(txn, || run_query(g, &q, BTreeMap::new()));
    match r {
        Ok(_) => g
            .commit_owned(txn)
            .unwrap_or_else(|e| panic!("commit {src}: {e:?}")),
        Err(e) => {
            g.rollback_owned(txn);
            panic!("run {src}: {e:?}");
        }
    }
}

/// An ic6-shaped corpus, scaled down but with SF1's ratio between the fan-out
/// and the tag vocabulary: many rows, few distinct tags.
///
/// 1 person, 8 friends, 12 messages each (96 messages), 3 tags per message
/// drawn from a vocabulary of 10. So ~288 tag rows over 10 distinct tags — a
/// repeat factor of ~29, against SF1's tens of thousands of rows over ~16,000
/// tags. The absolute numbers differ; the SHAPE is what is under test.
fn build(g: &Graph) -> u64 {
    g.set_degree_table_after(0);
    stmt(g, "CREATE (:Person {id: 0, name: 'hub'})");
    for f in 1..=8i64 {
        stmt(g, &format!("CREATE (:Person {{id: {f}, name: 'f{f}'}})"));
        stmt(
            g,
            &format!("MATCH (a:Person {{id: 0}}), (b:Person {{id: {f}}}) CREATE (a)-[:KNOWS]->(b)"),
        );
    }
    for t in 0..10i64 {
        stmt(g, &format!("CREATE (:Tag {{id: {t}, name: 'tag{t}'}})"));
    }
    let mut messages = 0u64;
    for f in 1..=8i64 {
        for j in 0..12i64 {
            let mid = f * 100 + j;
            stmt(g, &format!("CREATE (:Message {{id: {mid}}})"));
            stmt(
                g,
                &format!(
                    "MATCH (m:Message {{id: {mid}}}), (p:Person {{id: {f}}}) \
                     CREATE (m)-[:HAS_CREATOR]->(p)"
                ),
            );
            for k in 0..3i64 {
                let tag = (mid + k) % 10;
                stmt(
                    g,
                    &format!(
                        "MATCH (m:Message {{id: {mid}}}), (t:Tag {{id: {tag}}}) \
                         CREATE (m)-[:HAS_TAG]->(t)"
                    ),
                );
            }
            messages += 1;
        }
    }
    messages
}

/// THE MEASUREMENT: how many times is a node record materialised, against how
/// many distinct nodes the query actually touches?
#[test]
fn report_the_repeat_factor_on_an_ic6_shaped_query() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let messages = build(&g);

    let src = "MATCH (p:Person {id: 0})-[:KNOWS]-(f:Person)<-[:HAS_CREATOR]-(m:Message) \
               MATCH (m)-[:HAS_TAG]->(t:Tag) \
               RETURN t.name, count(*) AS c ORDER BY c DESC LIMIT 10";
    let q = parse_statement(src).expect("parse");
    let (res, trace) = engram_observe::with_trace(|| run_query(&g, &q, BTreeMap::new()));
    let res = res.expect("run");

    let c = |k: &str| trace.counters().get(k).copied().unwrap_or(0);
    let full = c("graph.nodes materialised in full");
    let projected = c("graph.nodes materialised with a projection");
    let total = full + projected;

    eprintln!(
        "[ic6] {messages} messages, 10 distinct tags -> {} result row(s); \
         node materialisations: {full} full + {projected} projected = {total}",
        res.rows.len()
    );
    assert!(
        !res.rows.is_empty(),
        "the fixture must answer rows, or the counters describe nothing"
    );
    assert_eq!(
        total, 0,
        "REFUTED, and pinned: the shape materialises no node records at all, so          there is no repeated decode for a per-statement memo to remove"
    );
    assert_eq!(
        c("interp.pipeline aggregate native-key group-by"),
        1,
        "it is served by the pipelined columnar aggregate — the PATH the          refutation rests on, so it is asserted rather than assumed"
    );
    assert!(
        c("store.column scans") > 0 && c("graph.column visits already in id order") > 0,
        "and `t.name` is gathered from a COLUMN in id order rather than decoded          per row: {} column scans, {} in-order visits",
        c("store.column scans"),
        c("graph.column visits already in id order")
    );
}
