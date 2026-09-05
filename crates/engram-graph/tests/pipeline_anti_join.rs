#![allow(non_snake_case)]
//! Differential tests for the bound-endpoint EDGE PREDICATE / ANTI-JOIN
//! (operator B of `docs/lsqb-completeness-plan.md`): a WHERE conjunct
//! `[NOT] (a)-[:T]->(b)` over two BOUND node vars is recognised as
//! `WherePred.edge` (before `contains_opaque`'s decline) and answered per row
//! by `Graph::edge_count_slim` — in the chunk filter when both vars are
//! materialised, or INLINE at a folded level (`InlinePred::EdgeToBound`) under
//! a `count(*)` fold (the q8 / q9 shapes).
//!
//! The semantics are the ones `interp::exists_probe_fast` already pins for the
//! general path (both endpoints bound ⇒ an adjacency membership test), so the
//! contract is the usual: for every accepted shape `set_columnar_scans(true)`
//! must equal `set_columnar_scans(false)` — the full ROW SET *and its order* —
//! and every declined shape falls back and still agrees. Pinned here:
//!   - NOT and bare forms, both arrows, undirected, untyped, a self-loop, and
//!     PARALLEL edges (existence, never multiplicity);
//!   - a LABELLED far end DECLINES (the general matcher re-verifies the label);
//!   - an OPTIONAL-introduced (nullable) endpoint must NOT fire and must agree
//!     (the probe's null semantics belong to the general path);
//!   - the fold's inline form fires on the q8 / q9 shapes.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// A{ak}-R->B{bk}-S->C{ck}, with T edges A->C to (anti-)join against:
///   R: a0->b0,b1   a1->b1   a2->b2
///   S: b0->c0,c1   b1->c1,c2   b2->c3
///   T: a0->c1 TWICE (parallel), a1->c2, c3->a2 (the REVERSE arrow — hit by
///      `(a)-[:T]-(c)` and `(c)-[:T]->(a)`, missed by `(a)-[:T]->(c)`),
///      c0->c0 (a self-loop)
///   X: a0->b0 (the OPTIONAL tests' second type)
fn gaj() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, key: &str, v: i64| {
        let mut p = BTreeMap::new();
        p.insert(key.to_string(), Value::Int(v));
        g.create_node(&[label.into()], &p).expect("node")
    };
    let a: Vec<u64> = (0..3).map(|i| mk("A", "ak", i)).collect();
    let b: Vec<u64> = (0..3).map(|i| mk("B", "bk", i)).collect();
    let c: Vec<u64> = (0..4).map(|i| mk("C", "ck", i)).collect();
    let e = BTreeMap::new();
    for (s, d) in [(0, 0), (0, 1), (1, 1), (2, 2)] {
        g.create_rel(a[s], "R", b[d], &e).expect("R");
    }
    for (s, d) in [(0, 0), (0, 1), (1, 1), (1, 2), (2, 3)] {
        g.create_rel(b[s], "S", c[d], &e).expect("S");
    }
    g.create_rel(a[0], "T", c[1], &e).expect("T");
    g.create_rel(a[0], "T", c[1], &e).expect("T parallel");
    g.create_rel(a[1], "T", c[2], &e).expect("T");
    g.create_rel(c[3], "T", a[2], &e).expect("T reverse");
    g.create_rel(c[0], "T", c[0], &e).expect("T self");
    g.create_rel(a[0], "X", b[0], &e).expect("X");
    g
}

fn run(g: &Graph, src: &str) -> Result<Vec<Vec<Value>>, String> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .map(|r| r.rows)
        .map_err(|e| e.to_string())
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run(g, src).unwrap_or_else(|e| panic!("run `{src}`: {e}"))
}

fn both(g: &Graph, src: &str) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_columnar_scans(true);
    let on = rows(g, src);
    g.set_columnar_scans(false);
    let off = rows(g, src);
    g.set_columnar_scans(true);
    (on, off)
}

fn counter(g: &Graph, src: &str, key: &str) -> Option<u64> {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace.counters().get(key).copied()
}

const FILTER: &str = "interp.pipeline edge pred filter";
const INLINE: &str = "interp.pipeline edge pred inline";

/// The chunk-filter form must FIRE and agree.
fn filter_agrees_and_fires(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let (on, off) = both(g, src);
    assert_eq!(on, off, "columnar vs general disagree: `{src}`");
    assert!(
        counter(g, src, FILTER).is_some(),
        "edge filter did not fire: `{src}`"
    );
    on
}

/// Neither edge-predicate form may fire; the answers must still agree.
fn declines_but_agrees(g: &Graph, src: &str) {
    let (on, off) = both(g, src);
    assert_eq!(on, off, "columnar vs general disagree: `{src}`");
    assert_eq!(
        counter(g, src, FILTER),
        None,
        "edge filter must not fire: `{src}`"
    );
    assert_eq!(
        counter(g, src, INLINE),
        None,
        "edge inline must not fire: `{src}`"
    );
}

fn i(n: i64) -> Value {
    Value::Int(n)
}

/// Every accepted spelling over materialised endpoints: NOT / bare, both
/// arrows, undirected, untyped, a self-loop, an AND with a property conjunct,
/// applied as early as both endpoints bind.
#[test]
fn edge_pred_filter_matches_general_across_shapes() {
    let g = gaj();
    let cases: &[&str] = &[
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T]->(c) RETURN a.ak AS ak, c.ck AS ck ORDER BY ak, ck LIMIT 100",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE (a)-[:T]->(c) RETURN a.ak AS ak, c.ck AS ck ORDER BY ak, ck LIMIT 100",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (c)<-[:T]-(a) RETURN a.ak AS ak, c.ck AS ck ORDER BY ak, ck LIMIT 100",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE (c)-[:T]->(a) RETURN a.ak AS ak, c.ck AS ck ORDER BY ak, ck LIMIT 100",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE (a)-[:T]-(c) RETURN a.ak AS ak, c.ck AS ck ORDER BY ak, ck LIMIT 100",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T]-(c) RETURN a.ak AS ak, c.ck AS ck ORDER BY ak, ck LIMIT 100",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE (a)-->(c) RETURN a.ak AS ak, c.ck AS ck ORDER BY ak, ck LIMIT 100",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)--(c) RETURN a.ak AS ak, c.ck AS ck ORDER BY ak, ck LIMIT 100",
        "MATCH (b:B)-[:S]->(c:C) WHERE (c)-[:T]->(c) RETURN b.bk AS bk, c.ck AS ck ORDER BY bk, ck LIMIT 100",
        "MATCH (b:B)-[:S]->(c:C) WHERE NOT (c)-[:T]->(c) RETURN b.bk AS bk, c.ck AS ck ORDER BY bk, ck LIMIT 100",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T]->(c) AND a.ak > 0 RETURN a.ak AS ak, c.ck AS ck ORDER BY ak, ck LIMIT 100",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T]->(c) AND a <> c RETURN a.ak AS ak, c.ck AS ck ORDER BY ak, ck LIMIT 100",
        // Production order (no ORDER BY) — the filter keeps the selection order.
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T]->(c) RETURN b.bk AS bk",
        // A never-minted type: no edge, so NOT holds everywhere and bare nowhere.
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:NEVER]->(c) RETURN a.ak AS ak, c.ck AS ck ORDER BY ak, ck LIMIT 100",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE (a)-[:NEVER]->(c) RETURN a.ak AS ak",
    ];
    for src in cases {
        filter_agrees_and_fires(&g, src);
    }
    // The numbers, by hand. Walks (a,c): a0: b0→c0,c1; b1→c1,c2 → (0,0)(0,1)(0,1)(0,2);
    // a1: b1→c1,c2 → (1,1)(1,2); a2: b2→c3 → (2,3). T out of A: a0->c1, a1->c2.
    // NOT (a)-[:T]->(c) keeps (0,0) (0,2) (1,1) (2,3).
    let on = filter_agrees_and_fires(
        &g,
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T]->(c) RETURN a.ak AS ak, c.ck AS ck ORDER BY ak, ck LIMIT 100",
    );
    assert_eq!(
        on,
        vec![
            vec![i(0), i(0)],
            vec![i(0), i(2)],
            vec![i(1), i(1)],
            vec![i(2), i(3)]
        ],
        "anti-join rows"
    );
    // PARALLEL edges are existence, not multiplicity: (0,1) appears exactly as
    // often as its walks (twice), never ×2 again for the two T edges.
    let on = filter_agrees_and_fires(
        &g,
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE (a)-[:T]->(c) RETURN a.ak AS ak, c.ck AS ck ORDER BY ak, ck LIMIT 100",
    );
    assert_eq!(
        on,
        vec![vec![i(0), i(1)], vec![i(0), i(1)], vec![i(1), i(2)]],
        "positive rows"
    );
    // UNDIRECTED sees the reversed edge c3->a2: (2,3) joins.
    let on = filter_agrees_and_fires(
        &g,
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE (a)-[:T]-(c) RETURN a.ak AS ak, c.ck AS ck ORDER BY ak, ck LIMIT 100",
    );
    assert!(
        on.contains(&vec![i(2), i(3)]),
        "undirected must see the reverse edge: {on:?}"
    );
    // The SELF-LOOP: only c0 carries one.
    let on = filter_agrees_and_fires(
        &g,
        "MATCH (b:B)-[:S]->(c:C) WHERE (c)-[:T]->(c) RETURN b.bk AS bk, c.ck AS ck ORDER BY bk, ck LIMIT 100",
    );
    assert_eq!(on, vec![vec![i(0), i(0)]], "self-loop rows");
}

/// Under a `count(*)` fold the predicate is INLINE at the folded level (q8:
/// `NOT (comment)-[:HAS_TAG]->(tag1)` with `tag1` the seed), keyed or global,
/// with the `<>` conjunct beside it; the fold and the inline counters fire and
/// the count is the general path's.
#[test]
fn edge_pred_inline_in_the_count_fold() {
    let g = gaj();
    let cases: &[&str] = &[
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T]->(c) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE (a)-[:T]->(c) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T]-(c) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (c)<-[:T]-(a) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T]->(c) AND a <> c RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T]->(c) RETURN a.ak AS ak, count(*) AS n ORDER BY ak",
        "MATCH (b:B)-[:S]->(c:C) WHERE NOT (c)-[:T]->(c) RETURN count(*) AS n",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:NEVER]->(c) RETURN count(*) AS n",
    ];
    for src in cases {
        let (on, off) = both(&g, src);
        assert_eq!(on, off, "inline edge pred disagree: `{src}`");
        assert_eq!(
            counter(&g, src, "interp.pipeline count fold"),
            Some(1),
            "fold: `{src}`"
        );
        assert_eq!(
            counter(&g, src, INLINE),
            Some(1),
            "inline edge pred: `{src}`"
        );
        assert_eq!(
            counter(&g, src, FILTER),
            None,
            "not the chunk filter: `{src}`"
        );
    }
    let (on, _) = both(
        &g,
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T]->(c) RETURN count(*) AS n",
    );
    assert_eq!(on, vec![vec![i(4)]], "anti-join count");
}

/// DECLINES: a labelled far end, a two-hop pattern, a rel variable, an unbound
/// far end, a property on an endpoint — each leaves the conjunct opaque, so
/// the whole statement declines to the general path and agrees.
#[test]
fn edge_pred_declines_richer_patterns() {
    let g = gaj();
    for src in [
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T]->(c:C) RETURN a.ak AS ak, c.ck AS ck ORDER BY ak, ck LIMIT 100",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a:A)-[:T]->(c) RETURN a.ak AS ak, c.ck AS ck ORDER BY ak, ck LIMIT 100",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T]->()-[:T]->(c) RETURN a.ak AS ak ORDER BY ak LIMIT 100",
        // (A rel VARIABLE inside a pattern predicate is an `UndefinedVariable`
        // error on both paths — an error, not a decline — so it is not listed.)
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T*1..2]->(c) RETURN a.ak AS ak ORDER BY ak LIMIT 100",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T]->(:C) RETURN a.ak AS ak ORDER BY ak LIMIT 100",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T]->() RETURN a.ak AS ak ORDER BY ak LIMIT 100",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T {w: 1}]->(c) RETURN a.ak AS ak ORDER BY ak LIMIT 100",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T]->(c {ck: 1}) RETURN a.ak AS ak ORDER BY ak LIMIT 100",
        "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T]->(c:C) RETURN count(*) AS n",
    ] {
        declines_but_agrees(&g, src);
    }
}

/// OPTIONAL MATCH. The clause's OWN WHERE may probe the clause's var against
/// an outer var (both real at filter time — the filter runs before the
/// null-fill): the OPTIONAL operator and the edge filter fire. A predicate
/// over an EARLIER clause's nullable var, or in a post-WITH WHERE over the
/// nullable var, must NOT fire and must agree with the general path (whose
/// null handling is the semantics).
#[test]
fn edge_pred_over_optional_vars() {
    let g = gaj();
    // In-clause, both endpoints real.
    let src = "MATCH (a:A) OPTIONAL MATCH (a)-[:R]->(b:B) WHERE NOT (a)-[:X]->(b) RETURN a.ak AS ak, count(b) AS n ORDER BY ak";
    let (on, off) = both(&g, src);
    assert_eq!(on, off, "in-clause OPTIONAL edge pred disagree");
    assert_eq!(
        counter(&g, src, "interp.pipeline optional runs"),
        Some(1),
        "OPTIONAL fired"
    );
    assert!(
        counter(&g, src, FILTER).is_some(),
        "edge filter fired inside the left join"
    );
    // a0->b0 is X-linked: a0 keeps only b1 (1); a1: b1 (1); a2: b2 (1).
    assert_eq!(
        on,
        vec![vec![i(0), i(1)], vec![i(1), i(1)], vec![i(2), i(1)]]
    );

    // An EARLIER clause's nullable var as an endpoint: the OPTIONAL recogniser
    // declines the whole statement; the general path answers both.
    for src in [
        "MATCH (a:A) OPTIONAL MATCH (a)-[:R]->(b:B) OPTIONAL MATCH (a)-[:X]->(d:B) WHERE NOT (b)-[:S]-(d) RETURN a.ak AS ak, count(*) AS n ORDER BY ak",
        "MATCH (a:A) OPTIONAL MATCH (a)-[:R]->(b:B) OPTIONAL MATCH (a)-[:X]->(d:B) WHERE (d)-[:S]->(b) RETURN count(*) AS n",
    ] {
        g.set_columnar_scans(true);
        let on = run(&g, src);
        g.set_columnar_scans(false);
        let off = run(&g, src);
        g.set_columnar_scans(true);
        assert_eq!(on, off, "nullable-endpoint OPTIONAL disagree: `{src}`");
        assert_eq!(
            counter(&g, src, "interp.pipeline optional runs"),
            None,
            "must decline: `{src}`"
        );
        assert_eq!(
            counter(&g, src, FILTER),
            None,
            "edge filter must not fire: `{src}`"
        );
        assert_eq!(
            counter(&g, src, INLINE),
            None,
            "edge inline must not fire: `{src}`"
        );
    }
    // A post-WITH WHERE over the nullable var: opaque to the WITH tail, declines.
    let src = "MATCH (a:A) OPTIONAL MATCH (a)-[:R]->(b:B) WITH a, b WHERE NOT (a)-[:X]->(b) RETURN a.ak AS ak, count(*) AS n ORDER BY ak";
    g.set_columnar_scans(true);
    let on = run(&g, src);
    g.set_columnar_scans(false);
    let off = run(&g, src);
    g.set_columnar_scans(true);
    assert_eq!(on, off, "post-WITH nullable edge pred disagree");
    assert_eq!(counter(&g, src, FILTER), None);
    assert_eq!(counter(&g, src, INLINE), None);
}

/// Non-vacuity: the filter form fires only with columnar ON.
#[test]
fn edge_pred_fires_only_when_columnar_on() {
    let g = gaj();
    let src = "MATCH (a:A)-[:R]->(b:B)-[:S]->(c:C) WHERE NOT (a)-[:T]->(c) RETURN a.ak AS ak, c.ck AS ck ORDER BY ak, ck LIMIT 100";
    assert!(counter(&g, src, FILTER).is_some());
    g.set_columnar_scans(false);
    let (_, trace) = engram_observe::with_trace(|| rows(&g, src));
    g.set_columnar_scans(true);
    assert_eq!(
        trace.counters().get(FILTER).copied(),
        None,
        "must NOT fire with columnar OFF"
    );
}
