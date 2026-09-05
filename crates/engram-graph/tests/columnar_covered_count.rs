#![allow(non_snake_case)]
//! A COVERED COUNT: `MATCH (n:L {a: 'x', b: 'y'}) RETURN count(n)` where every
//! equality is on a key with a DECLARED index for `L` is answered from the
//! indexes' intersection — no record read, no label scan.
//!
//! The production shape (2026-09-04): `MATCH (n:UserDataNode {nodeType: 'email',
//! userId: $u}) RETURN count(n)` — 18k of 38k members match, so neither key
//! alone is selective enough to seek and the engine read 18k records (1.2 s on
//! the paged mirror). Neo4j answered from its composite index in 4 ms. This is
//! the same answer from the same information: two scoped probes, intersected,
//! then intersected with the label's membership so a node whose label was
//! removed since the index was built is not counted.
//!
//! The contract: the covered path FIRES only when it is exact — string values
//! (an Int/Float probe unions the cross-type bucket and needs the verifier),
//! every conjunct a declared-scoped equality, a pure `count`, no pending
//! transaction writes — and is byte-identical to the general path when it does.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn both(g: &Graph, src: &str) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_columnar_scans(true);
    let on = rows(g, src);
    g.set_columnar_scans(false);
    let off = rows(g, src);
    g.set_columnar_scans(true);
    (on, off)
}

fn counter(g: &Graph, src: &str, key: &str) -> u64 {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace.counters().get(key).copied().unwrap_or(0)
}

const COVERED: &str = "interp.columnar covered count";
const GATHER: &str = "graph.column point-gather";

/// 700 `:Doc` nodes: `kind` 'email' on 600 / 'note' on 100, `owner` u0..u349
/// (two docs each), `n` the ordinal. Both keys declared for `Doc` when
/// `declare_both`; only `owner` otherwise.
fn corpus(declare_both: bool) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_label_scoped_indexes(true);
    ddl(&g, "CREATE INDEX doc_owner IF NOT EXISTS FOR (n:Doc) ON (n.owner)");
    if declare_both {
        ddl(&g, "CREATE INDEX doc_kind IF NOT EXISTS FOR (n:Doc) ON (n.kind)");
    }
    for i in 0..700i64 {
        let mut m = BTreeMap::new();
        m.insert(
            "kind".to_string(),
            Value::Str(if i % 7 == 0 { "note" } else { "email" }.to_string()),
        );
        m.insert("owner".to_string(), Value::Str(format!("u{}", i % 350)));
        m.insert("n".to_string(), Value::Int(i));
        g.create_node(&["Doc".into()], &m).expect("node");
    }
    g
}

const COUNT_N: &str = "MATCH (d:Doc {kind: 'email', owner: 'u8'}) RETURN count(d) AS n";
const COUNT_STAR: &str = "MATCH (d:Doc) WHERE d.owner = 'u8' AND d.kind = 'email' RETURN count(*) AS n";
const COUNT_UNSELECTIVE: &str = "MATCH (d:Doc {kind: 'email'}) RETURN count(d) AS n";

#[test]
fn a_covered_count_is_answered_from_the_index_intersection_without_a_record_read() {
    let g = corpus(true);
    for src in [COUNT_N, COUNT_STAR] {
        let (on, off) = both(&g, src);
        assert_eq!(on, off, "covered vs general disagree on `{src}`");
        assert_eq!(on, vec![vec![Value::Int(2)]], "u8 owns docs 8 and 358, both email");
        assert!(counter(&g, src, COVERED) > 0, "`{src}` must be answered by the intersection");
        assert_eq!(counter(&g, src, GATHER), 0, "`{src}` must read no record");
    }
    // The UNSELECTIVE single key: 600 of 700 — a seek would lose to the scan,
    // and the covered count does not care: it counts index entries.
    let (on, off) = both(&g, COUNT_UNSELECTIVE);
    assert_eq!(on, off);
    assert_eq!(on, vec![vec![Value::Int(600)]]);
    assert!(counter(&g, COUNT_UNSELECTIVE, COVERED) > 0);
    assert_eq!(counter(&g, COUNT_UNSELECTIVE, GATHER), 0);
}

/// A key WITHOUT a declared index, a NUMERIC value, an EXTRA predicate, a
/// DISTINCT, a second aggregate, or a grouping key — each keeps the general
/// path, and the answers agree.
#[test]
fn the_covered_path_declines_everything_it_cannot_answer_exactly() {
    let g = corpus(false); // `kind` undeclared
    let cases = [
        ("undeclared key", COUNT_N),
        ("numeric value", "MATCH (d:Doc {owner: 'u8', n: 8}) RETURN count(d) AS n"),
        ("extra predicate", "MATCH (d:Doc {owner: 'u8'}) WHERE d.n > 100 RETURN count(d) AS n"),
        ("distinct", "MATCH (d:Doc {owner: 'u8'}) RETURN count(DISTINCT d.kind) AS n"),
        ("another aggregate", "MATCH (d:Doc {owner: 'u8'}) RETURN count(d) AS n, max(d.n) AS m"),
        ("grouping key", "MATCH (d:Doc {owner: 'u8'}) RETURN d.kind AS k, count(d) AS n ORDER BY k"),
    ];
    for (why, src) in cases {
        let (on, off) = both(&g, src);
        assert_eq!(on, off, "{why}: columnar vs general disagree on `{src}`");
        assert_eq!(counter(&g, src, COVERED), 0, "{why}: must not be covered: `{src}`");
    }
    // And the one it CAN answer here — the declared key alone — still fires.
    let src = "MATCH (d:Doc {owner: 'u8'}) RETURN count(d) AS n";
    let (on, off) = both(&g, src);
    assert_eq!(on, off);
    assert_eq!(on, vec![vec![Value::Int(2)]]);
    assert!(counter(&g, src, COVERED) > 0);
}

/// A node whose LABEL was removed keeps its index entries until the index
/// catches up; the membership snapshot is exact, so the count must drop.
#[test]
fn a_removed_label_is_not_counted() {
    let g = corpus(true);
    assert_eq!(rows(&g, COUNT_N), vec![vec![Value::Int(2)]]);
    rows(&g, "MATCH (d:Doc {n: 8}) REMOVE d:Doc");
    let (on, off) = both(&g, COUNT_N);
    assert_eq!(on, off, "after the label removal the paths must still agree");
    assert_eq!(on, vec![vec![Value::Int(1)]], "doc 8 left the label");
    assert!(counter(&g, COUNT_N, COVERED) > 0, "and the covered path still answers");
}
