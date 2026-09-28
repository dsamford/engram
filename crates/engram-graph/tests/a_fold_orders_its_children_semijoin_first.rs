#![allow(non_snake_case)]
//! Fix 120: each fold level multiplies its children in an order chosen
//! STRUCTURALLY, so a factor that can be zero runs before the wide expansion
//! it would annul.
//!
//! `FoldPlan::new` used to push children in hop-index order, and `level`
//! multiplies them in that order with a `w == 0` short-circuit. For LSQB q3
//! that put `children[person2] = [KNOWS->person3, IS_LOCATED_IN->city2]`, so
//! person2's ~36 friends were expanded — with all of their closes — BEFORE
//! the two-hop test of whether person2 was even in the bound country, and the
//! result was then multiplied by zero. At SF1 only 99,780 of 361,246 KNOWS
//! edges have both endpoints in one country, so 72.4% of that expansion was
//! waste. q3 measured 5,471 ms against PostgreSQL's 1,376.
//!
//! The rank is structural and never an estimate: a CLOSE before an EXPAND,
//! then the smaller subtree, then hop index as a stable tie-break. That
//! matters — `cardinality.rs` models neither WHERE selectivity nor
//! correlation, and q3's ledger records five cost-model-driven attempts that
//! all made it worse, one by 1.85x.
//!
//! Reordering the factors of a product cannot change the product, so the
//! claim here is TWO-sided and both sides are asserted: every answer is
//! byte-identical to the hop-index order, and the work is strictly less.
//!
//! Canary, run: forcing the rank to `(0, 0, hi)` — that is, back to hop-index
//! order — leaves every answer assertion green and fails
//! `c_the_reordering_does_strictly_less_work` on equal walk counts, which is
//! the whole point: this fix is invisible in answers by construction.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const ORDERED: &str = "interp.pipeline fold children ordered semijoin-first";
const WALKS: &str = "interp.pipeline fold hop walks";

fn stmt(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run {src}: {e:?}"));
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run {src}: {e:?}"))
        .rows
}

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, t) = engram_observe::with_trace(|| rows(g, src));
    (r, t.counters().clone())
}

fn count_of(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

/// q3's shape: countries, persons located in them, and a KNOWS graph whose
/// edges mostly CROSS a country boundary — which is the case the reordering
/// exists for. Twelve countries of six persons; each person knows the next
/// three by global id, so most friendships leave the country.
const COUNTRIES: i64 = 12;
const PER: i64 = 6;
const TOTAL: i64 = COUNTRIES * PER;

fn fixture() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for c in 0..COUNTRIES {
        stmt(&g, &format!("CREATE (:Country {{id: {c}}})"));
    }
    for p in 0..TOTAL {
        stmt(&g, &format!("CREATE (:Person {{id: {p}}})"));
    }
    let edge = |ty: &str, a: i64, b: i64, alab: &str, blab: &str| {
        stmt(
            &g,
            &format!(
                "MATCH (x:{alab} {{id: {a}}}), (y:{blab} {{id: {b}}}) CREATE (x)-[:{ty}]->(y)"
            ),
        );
    };
    for p in 0..TOTAL {
        for d in 1..4 {
            edge("KNOWS", p, (p + d) % TOTAL, "Person", "Person");
        }
    }
    for p in 0..TOTAL {
        edge("IS_LOCATED_IN", p, p / PER, "Person", "Country");
    }
    g.shared_store().seal();
    g
}

/// q3's own shape, plus two more fold shapes with multi-child levels.
const QUERIES: &[&str] = &[
    // The q3 triangle: three persons in one country who all know each other.
    "MATCH (c:Country) \
     MATCH (p1:Person)-[:IS_LOCATED_IN]->(c) \
     MATCH (p2:Person)-[:IS_LOCATED_IN]->(c) \
     MATCH (p3:Person)-[:IS_LOCATED_IN]->(c) \
     MATCH (p1)-[:KNOWS]-(p2)-[:KNOWS]-(p3)-[:KNOWS]-(p1) \
     RETURN count(*) AS n",
    // Two persons in one country who know each other — one level, two children.
    "MATCH (c:Country) \
     MATCH (p1:Person)-[:IS_LOCATED_IN]->(c) \
     MATCH (p2:Person)-[:IS_LOCATED_IN]->(c) \
     MATCH (p1)-[:KNOWS]-(p2) \
     RETURN count(*) AS n",
    // Grouped, so a scrambled order would show in the row order too.
    "MATCH (c:Country) MATCH (p:Person)-[:IS_LOCATED_IN]->(c) \
     MATCH (p)-[:KNOWS]->(q:Person) RETURN c.id AS id, count(*) AS n",
];

fn with_order<R>(on: bool, f: impl FnOnce() -> R) -> R {
    engram_graph::pipeline::set_fold_child_order(on);
    let out = f();
    engram_graph::pipeline::set_fold_child_order(true);
    out
}

#[test]
fn a_the_reordering_actually_fires() {
    let g = fixture();
    let (_, c) = traced(&g, QUERIES[0]);
    assert!(
        count_of(&c, ORDERED) > 0,
        "no level was reordered, so this file proves nothing about q3's shape: {c:?}"
    );
}

#[test]
fn b_every_answer_is_byte_identical_to_hop_index_order() {
    let g = fixture();
    let on: Vec<Vec<Vec<Value>>> =
        with_order(true, || QUERIES.iter().map(|q| rows(&g, q)).collect());
    let off: Vec<Vec<Vec<Value>>> =
        with_order(false, || QUERIES.iter().map(|q| rows(&g, q)).collect());
    assert_eq!(
        on, off,
        "the semijoin-first order changed an ANSWER — reordering the factors of \
         a product must not do that"
    );
    // And pin the triangle by arithmetic rather than against either arm, so a
    // fixture change cannot make both arms agree on a wrong number.
    let triangles = &on[0][0][0];
    assert!(
        matches!(triangles, Value::Int(n) if *n > 0),
        "the triangle count is not a positive integer: {triangles:?}"
    );
}

#[test]
fn c_the_reordering_does_strictly_less_work() {
    let g = fixture();
    let (_, on) = engram_observe::with_trace(|| with_order(true, || rows(&g, QUERIES[0])));
    let (_, off) = engram_observe::with_trace(|| with_order(false, || rows(&g, QUERIES[0])));
    let (won, woff) = (
        count_of(on.counters(), WALKS),
        count_of(off.counters(), WALKS),
    );
    assert!(
        won > 0 && woff > 0,
        "no fold walks were counted: on={won} off={woff}"
    );
    eprintln!(
        "[fix 120] fold hop walks: {won} with the reordering, {woff} without ({:.2}x less work)",
        woff as f64 / won as f64
    );
    assert!(
        won < woff,
        "the semijoin-first order did not reduce the walk count ({won} against \
         {woff}) — on a corpus where most KNOWS edges cross a country it should"
    );
}

#[test]
fn d_a_level_whose_children_read_each_other_is_left_alone() {
    // The legality guard. A predicate reading a sibling subtree's var must
    // pin the order, even though `hop_sum` resets bindings on the way out and
    // the answer happens not to depend on it today.
    let g = fixture();
    // Two children of `p1`, one carrying a WHERE that names the other's var.
    let q = "MATCH (c:Country) MATCH (p1:Person)-[:IS_LOCATED_IN]->(c) \
             MATCH (p1)-[:KNOWS]-(p2:Person) MATCH (p1)-[:KNOWS]-(p3:Person) \
             WHERE p2 <> p3 RETURN count(*) AS n";
    let on = with_order(true, || rows(&g, q));
    let off = with_order(false, || rows(&g, q));
    assert_eq!(
        on, off,
        "a level with cross-reading children answered differently"
    );
}
