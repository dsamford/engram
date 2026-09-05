#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! A multi-key pattern map may SEEK a declared index instead of scanning the
//! label — and must answer identically when it does.
//!
//! The gate this replaces was `entries.len() == 1` in
//! `anchored_start_candidate_ids`: an ARITY test, not a cardinality one. So
//! `MATCH (n:Churn {id: X, nonce: M}) DETACH DELETE n` could never seek at any
//! label size, even though the workload had declared
//! `CREATE INDEX ... FOR (n:Churn) ON (n.id)` and Neo4j used it.
//!
//! The cost was not merely a slower scan. Every candidate the scan materialises
//! enters the transaction's OCC read set, and validation is O(read set) point
//! lookups **under the global commit latch** — which is why that profile got
//! SLOWER with more workers instead of faster.
//!
//! The bar here is not "faster". It is that the seek and the scan return **the
//! same rows in the same order**: `index_probe_eq` returns a CANDIDATE set and
//! never an oracle, and `node_satisfies` re-checks every label and every map
//! entry afterwards, so the probe may only narrow the stream.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn run(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"));
}

/// Rows as the client would see them, ORDER PRESERVED.
fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn count(t: &engram_observe::Trace, k: &str) -> u64 {
    t.counters().get(k).copied().unwrap_or(0)
}

/// A corpus big enough that a scan and a seek are genuinely different plans,
/// with a declared index on `id` and NONE on `nonce` — the churn shape.
fn seeded(g: &Graph) {
    ddl(g, "CREATE INDEX churn_id IF NOT EXISTS FOR (n:Churn) ON (n.id)");
    for i in 0..600i64 {
        let nonce = i % 7;
        let tag = i % 3;
        run(
            g,
            &format!("CREATE (:Churn {{id: {i}, nonce: {nonce}, tag: 'x{tag}'}})"),
        );
    }
    // Same property names on a DIFFERENT label: a probe on `id` must not be
    // allowed to answer a pattern that requires `:Other`.
    for i in 0..50i64 {
        run(g, &format!("CREATE (:Other {{id: {i}, nonce: 0}})"));
    }
}

/// The corpus of shapes both arms must agree on.
const CASES: &[&str] = &[
    // two keys, both selective
    "MATCH (n:Churn {id: 17, nonce: 3}) RETURN n.id, n.nonce, n.tag",
    // two keys where the SECOND is the non-indexed one and excludes everything
    "MATCH (n:Churn {id: 17, nonce: 999}) RETURN n.id",
    // three keys
    "MATCH (n:Churn {id: 21, nonce: 0, tag: 'x0'}) RETURN n.id, n.tag",
    // the indexed key matches nothing
    "MATCH (n:Churn {id: 99999, nonce: 0}) RETURN n.id",
    // a string value on the non-indexed key
    "MATCH (n:Churn {id: 8, tag: 'x2'}) RETURN n.id, n.tag",
    // cross-type: stored Int, queried as Float
    "MATCH (n:Churn {id: 12.0, nonce: 5}) RETURN n.id",
    // a value the index cannot order at all -> must fall back
    "MATCH (n:Churn {id: 3, nonce: null}) RETURN n.id",
    // the label the index does NOT cover
    "MATCH (n:Other {id: 7, nonce: 0}) RETURN n.id",
    // no label at all: no declared index can apply
    "MATCH (n {id: 5, nonce: 5}) RETURN n.id",
    // multi-row: many nodes share a nonce, so the result is a real set
    "MATCH (n:Churn {nonce: 2, tag: 'x1'}) RETURN n.id",
];

#[test]
fn every_multikey_shape_answers_identically_on_both_arms() {
    let mut arms = Vec::new();
    for on in [false, true] {
        let g = graph();
        g.set_pattern_map_seek(on);
        seeded(&g);
        let answers: Vec<Vec<Vec<Value>>> = CASES.iter().map(|q| rows(&g, q)).collect();
        arms.push(answers);
    }
    for (i, q) in CASES.iter().enumerate() {
        assert_eq!(
            arms[0][i], arms[1][i],
            "case {i} `{q}`: the seek must answer exactly what the scan does, in \
             the same ORDER — both candidate sources are ascending by id, so \
             equality here is row-for-row and not merely set equality"
        );
    }
}

/// Non-vacuity: the ON arm really seeks and the OFF arm really scans. Without
/// this the differential above could be comparing a scan against a scan.
///
/// **The probe statement must be a WRITE.** A plain `MATCH ... RETURN` of this
/// shape is answered by the COLUMNAR projection planner, which has its own
/// property seek and never reaches `anchored_start_candidate_ids` — the first
/// version of this test asserted against a `RETURN` and failed with
/// `interp.columnar projection scans: 1` in the trace, which is the honest
/// answer to "am I measuring the path I changed?".
///
/// That split is the same "two seed planners disagree" shape the write path
/// suffered from: the columnar reader could already seek a two-key map while
/// the write path could not.
#[test]
fn the_lever_actually_switches_the_plan() {
    let write = "MATCH (n:Churn {id: 17, nonce: 3}) SET n.touched = 1";

    let g = graph();
    g.set_pattern_map_seek(true);
    seeded(&g);
    let (_, on) = engram_observe::with_trace(|| run(&g, write));
    assert!(
        count(&on, "interp.pattern map seeks") >= 1,
        "ON arm must seek a declared index on the WRITE path, counters: {:?}",
        on.counters()
    );

    let g2 = graph();
    g2.set_pattern_map_seek(false);
    seeded(&g2);
    let (_, off) = engram_observe::with_trace(|| run(&g2, write));
    assert_eq!(
        count(&off, "interp.pattern map seeks"),
        0,
        "OFF arm must take the label scan — it is the differential's control"
    );
    // THE POINT. `graph.nodes materialised in full` counts a full
    // `Record::decode` + `decode_props` per candidate, and each of those reads
    // enters the transaction's OCC read set. Validation then walks that set
    // under the global commit latch — so this count IS the per-statement cost
    // that made more workers slower rather than faster.
    let on_decodes = count(&on, "graph.nodes materialised in full");
    let off_decodes = count(&off, "graph.nodes materialised in full");
    eprintln!(
        "[multikey seek] nodes materialised: seek {on_decodes}, scan {off_decodes} (600 :Churn)"
    );
    assert!(
        on_decodes <= 8,
        "the seek arm must materialise a handful of candidates, got {on_decodes}"
    );
    assert!(
        off_decodes >= 100,
        "the scan arm must materialise the label, got {off_decodes} — if this ever \
         drops, the control stopped being a scan and the comparison no longer \
         measures what it says"
    );
}

/// An UNDECLARED key is never probed. `index_probe_eq` builds an index on first
/// use, so probing an undeclared property would let a plan incur a
/// whole-partition build the operator never asked for.
#[test]
fn an_undeclared_key_is_never_probed() {
    let g = graph();
    g.set_pattern_map_seek(true);
    // No CREATE INDEX at all.
    for i in 0..600i64 {
        let nonce = i % 7;
        run(&g, &format!("CREATE (:Churn {{id: {i}, nonce: {nonce}}})"));
    }
    // A WRITE, so this exercises `anchored_start_candidate_ids` rather than
    // the columnar projection planner (see `the_lever_actually_switches_the_plan`).
    let (_, t) = engram_observe::with_trace(|| {
        run(&g, "MATCH (n:Churn {id: 17, nonce: 3}) SET n.touched = 1");
    });
    assert_eq!(
        count(&t, "interp.pattern map seeks"),
        0,
        "with nothing declared there is no index to probe, counters: {:?}",
        t.counters()
    );
    assert!(
        count(&t, "interp.pattern map seeks declined") >= 1,
        "and the decline must be RECORDED, not silent"
    );
}

/// A declared index on a DIFFERENT label must not answer this pattern.
#[test]
fn an_index_on_another_label_does_not_apply() {
    let g = graph();
    g.set_pattern_map_seek(true);
    ddl(&g, "CREATE INDEX other_id IF NOT EXISTS FOR (n:Other) ON (n.id)");
    for i in 0..600i64 {
        let nonce = i % 7;
        run(&g, &format!("CREATE (:Churn {{id: {i}, nonce: {nonce}}})"));
    }
    let (_, t) = engram_observe::with_trace(|| {
        run(&g, "MATCH (n:Churn {id: 17, nonce: 3}) SET n.touched = 1");
    });
    assert_eq!(
        count(&t, "interp.pattern map seeks"),
        0,
        "an index declared FOR (n:Other) says nothing about :Churn rows"
    );
}

/// Writes driven by a multi-key match must behave identically too — this is
/// the shape the delete-churn workload actually runs.
#[test]
fn a_multikey_detach_delete_behaves_identically_on_both_arms() {
    let mut finals = Vec::new();
    for on in [false, true] {
        let g = graph();
        g.set_pattern_map_seek(on);
        seeded(&g);
        // Wire each Churn to an anchor, so DETACH DELETE has edges to clear.
        run(&g, "CREATE (:Anchor {a: 1})");
        run(
            &g,
            "MATCH (a:Anchor {a: 1}), (n:Churn) WHERE n.id < 20 CREATE (a)-[:R]->(n)",
        );
        for i in 0..20i64 {
            let nonce = i % 7;
            run(
                &g,
                &format!("MATCH (n:Churn {{id: {i}, nonce: {nonce}}}) DETACH DELETE n"),
            );
        }
        let survivors = rows(&g, "MATCH (n:Churn) RETURN n.id");
        let edges = rows(&g, "MATCH ()-[r:R]->() RETURN r");
        assert!(
            g.verify_rel_endpoints().expect("fsck").is_empty(),
            "arm on={on}: FSCK found a dangling edge"
        );
        finals.push((survivors.len(), edges.len()));
    }
    assert_eq!(
        finals[0], finals[1],
        "the seek may change what the engine PAYS, never what it DELETES"
    );
}

/// An index declared AFTER the catalogue was first consulted must still be
/// seen.
///
/// The catalogue is cached against the SCHEMA EPOCH, and index DDL does not
/// bump that epoch — only constraint DDL does. So a plan that read the
/// catalogue before any `CREATE INDEX` cached an empty one and kept it for the
/// life of the process: every subsequent seek silently declined, and the only
/// symptom would have been a benchmark that stayed slow.
///
/// `create_index` and `drop_schema` now clear the cache directly. This pins it.
#[test]
fn an_index_declared_after_the_first_read_is_still_seen() {
    let g = graph();
    g.set_pattern_map_seek(true);
    for i in 0..600i64 {
        let nonce = i % 7;
        run(&g, &format!("CREATE (:Churn {{id: {i}, nonce: {nonce}}})"));
    }
    // Consult the catalogue BEFORE the index exists — this is what poisoned it.
    let (_, before) = engram_observe::with_trace(|| {
        run(&g, "MATCH (n:Churn {id: 1, nonce: 1}) SET n.touched = 1");
    });
    assert_eq!(
        count(&before, "interp.pattern map seeks"),
        0,
        "nothing is declared yet, so there is nothing to seek"
    );

    ddl(&g, "CREATE INDEX churn_id IF NOT EXISTS FOR (n:Churn) ON (n.id)");

    let (_, after) = engram_observe::with_trace(|| {
        run(&g, "MATCH (n:Churn {id: 2, nonce: 2}) SET n.touched = 1");
    });
    assert!(
        count(&after, "interp.pattern map seeks") >= 1,
        "the newly declared index must be visible to the very next statement, \
         counters: {:?}",
        after.counters()
    );

    // And DROPPING it must stop the seek, for the same reason.
    ddl(&g, "DROP INDEX churn_id");
    let (_, dropped) = engram_observe::with_trace(|| {
        run(&g, "MATCH (n:Churn {id: 3, nonce: 3}) SET n.touched = 1");
    });
    assert_eq!(
        count(&dropped, "interp.pattern map seeks"),
        0,
        "a dropped index must stop being consulted"
    );
}
