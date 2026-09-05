#![allow(non_snake_case)]
//! Differential tests for the composable columnar pipeline's group-by-COUNT
//! operator (Phase 3a of `pipeline::plan_and_run_columnar`). The contract is the
//! same as `pipeline_core`/`pipeline_multihop`: for every aggregating shape the
//! pipeline accepts, running with `set_columnar_scans(true)` (the columnar
//! reduction + shared aggregating tail) must equal `set_columnar_scans(false)`
//! (the per-tuple `run_streaming` aggregation) — the full ROW SET *and its
//! order*, byte-for-byte — and for every shape it declines, the general path
//! answers and the two still agree.
//!
//! THE load-bearing fact under test is FIRST-SEEN group order: `run_streaming`
//! emits aggregation groups in the order each distinct key is first encountered
//! in the PRODUCTION row stream (scan-order × nested reverse-adjacency), and an
//! aggregating projection's ORDER BY then sorts those group rows with a STABLE
//! sort so ties fall back on first-seen order. The TIE test's `ORDER BY c DESC
//! LIMIT 1` over two groups tied at the max count keeps the first-seen group;
//! perturbing the reduction's accumulation order (sort/reverse the groups)
//! diverges it (the canary), which also proves the operator fires.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// Ag{ak} -R-> Bg{grp str (X/Y/Z + one null), bk int} -R2-> Cg{ck}.
/// R fan-out (creation order is load-bearing — it drives reverse-adjacency):
///   a0 -> b0,b1,b2,b3,b4   a1 -> b5,b0,b2   a2 -> b4   a3 -> (none)
/// so `count(*)` grouped by `b.grp` ties X=Y=3 (the tie test), the null-grp
/// group b4 has count 2, a2's only edge lands on a null-grp end (a `count(x)`
/// group that reaches 0) and a3 never appears (a group that is absent, not 0).
/// R2 fan-out: b0->c0,c1  b1->c2  b2->c3  (b3,b4,b5 have no R2), so the 2-hop
/// `count(*)` grouped by `c` ties c0=c1=2.
fn ga() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk_a = |ak: i64| {
        let mut p = BTreeMap::new();
        p.insert("ak".to_string(), Value::Int(ak));
        g.create_node(&["Ag".into()], &p).expect("a")
    };
    let a = [mk_a(1), mk_a(2), mk_a(3), mk_a(4)];
    let mk_b = |grp: Option<&str>, bk: i64| {
        let mut p = BTreeMap::new();
        if let Some(s) = grp {
            p.insert("grp".to_string(), Value::Str(s.to_string()));
        }
        p.insert("bk".to_string(), Value::Int(bk));
        g.create_node(&["Bg".into()], &p).expect("b")
    };
    let b = [
        mk_b(Some("X"), 0), // b0
        mk_b(Some("X"), 1), // b1
        mk_b(Some("Y"), 2), // b2
        mk_b(Some("Y"), 3), // b3
        mk_b(None, 4),      // b4 — grp NULL
        mk_b(Some("Z"), 5), // b5
    ];
    let mk_c = |ck: i64| {
        let mut p = BTreeMap::new();
        p.insert("ck".to_string(), Value::Int(ck));
        g.create_node(&["Cg".into()], &p).expect("c")
    };
    let c = [mk_c(0), mk_c(1), mk_c(2), mk_c(3)];
    for (s, d) in [
        (0, 0),
        (0, 1),
        (0, 2),
        (0, 3),
        (0, 4),
        (1, 5),
        (1, 0),
        (1, 2),
        (2, 4),
    ] {
        g.create_rel(a[s], "R", b[d], &BTreeMap::new()).expect("R");
    }
    for (s, d) in [(0, 0), (0, 1), (1, 2), (2, 3)] {
        g.create_rel(b[s], "R2", c[d], &BTreeMap::new())
            .expect("R2");
    }
    g
}

fn rows(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params)
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// Run `src` with the pipeline ON, then the general path OFF; return both.
fn both(
    g: &Graph,
    src: &str,
    params: BTreeMap<String, Value>,
) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_columnar_scans(true);
    let on = rows(g, src, params.clone());
    g.set_columnar_scans(false);
    let off = rows(g, src, params);
    g.set_columnar_scans(true);
    (on, off)
}

/// Whether the GROUP-BY-COUNT operator fired for `src` with columnar ON.
fn agg_fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    trace
        .counters()
        .get("interp.pipeline aggregate runs")
        .copied()
        == Some(1)
}

fn assert_agrees(g: &Graph, src: &str) {
    let (on, off) = both(g, src, BTreeMap::new());
    assert_eq!(on, off, "columnar vs general disagree: `{src}`");
}

/// The accepted shapes: node-grouping and prop-grouping over a 1-hop and a
/// 2-hop chain, both forms (RETURN aggregate and WITH→RETURN), with ORDER
/// BY/SKIP/LIMIT — ON must equal OFF row-for-row AND in order.
#[test]
fn agg_matches_general_across_shapes() {
    let g = ga();
    let cases: &[&str] = &[
        // Form B, group by node, 1-hop, ORDER BY count then a tiebreak.
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.bk AS bk, count(*) AS c ORDER BY c DESC, b.bk ASC LIMIT 4",
        // Form A, group by node, 1-hop.
        "MATCH (a:Ag)-[:R]->(b:Bg) WITH b, count(*) AS c RETURN b.bk AS bk, c ORDER BY c DESC, b.bk ASC LIMIT 4",
        // Group by a property (X/Y/Z + a NULL group), Form B, whole set.
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, count(*) AS c ORDER BY c DESC, g ASC",
        // Group by a property, Form A.
        "MATCH (a:Ag)-[:R]->(b:Bg) WITH b.grp AS g, count(*) AS c RETURN g, c ORDER BY c DESC, g ASC",
        // Group by the FAR node over a 2-hop chain (c0=c1=2 tie), Form B.
        "MATCH (a:Ag)-[:R]->(b:Bg)-[:R2]->(c:Cg) RETURN c.ck AS ck, count(*) AS n ORDER BY n DESC, c.ck ASC LIMIT 3",
        // Group by the MID node over a 2-hop chain, Form A.
        "MATCH (a:Ag)-[:R]->(b:Bg)-[:R2]->(c:Cg) WITH b, count(*) AS n RETURN b.bk AS bk, n ORDER BY n DESC, b.bk ASC",
        // count(*) with count(x) is exercised elsewhere; here a plain ORDER BY
        // over the count alias only + SKIP + LIMIT.
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, count(*) AS c ORDER BY c DESC, g ASC SKIP 1 LIMIT 2",
        // Form A with a post-WITH WHERE (a HAVING-style filter over the count).
        "MATCH (a:Ag)-[:R]->(b:Bg) WITH b.grp AS g, count(*) AS c WHERE c >= 2 RETURN g, c ORDER BY c DESC, g ASC",
        // Group by the START-side property.
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN a.ak AS ak, count(*) AS c ORDER BY ak ASC",
        // A constant grouping key collapses everything to one group.
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN 1 AS one, count(*) AS c ORDER BY c",
    ];
    for src in cases {
        assert_agrees(&g, src);
        assert!(agg_fired(&g, src), "operator did not fire: `{src}`");
    }
}

/// `count(x)` null semantics: a null `x` is excluded, so a group whose every row
/// has a null `x` reaches count 0 (a2 → b4 with a null grp), while a start node
/// with no edge (a3) never produces a row and so has NO group — the count-0 vs
/// absent distinction `run_streaming` draws.
#[test]
fn agg_count_x_excludes_nulls_and_keeps_zero_groups() {
    let g = ga();
    for src in [
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN a.ak AS ak, count(b.grp) AS c ORDER BY ak ASC",
        "MATCH (a:Ag)-[:R]->(b:Bg) WITH a.ak AS ak, count(b.grp) AS c RETURN ak, c ORDER BY ak ASC",
    ] {
        assert_agrees(&g, src);
        assert!(agg_fired(&g, src), "operator did not fire: `{src}`");
    }
    // The explicit value contract: ak=3 (a2, whose only end b4 has a null grp)
    // is present with count 0; ak=4 (a3, no edge) is absent entirely.
    let (on, _) = both(
        &g,
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN a.ak AS ak, count(b.grp) AS c ORDER BY ak ASC",
        BTreeMap::new(),
    );
    assert!(
        on.contains(&vec![Value::Int(3), Value::Int(0)]),
        "a group whose count reaches 0 must still appear: {on:?}"
    );
    assert!(
        !on.iter().any(|r| r.first() == Some(&Value::Int(4))),
        "a start node that never matches must have NO group: {on:?}"
    );
}

/// FIRST-SEEN ORDER: `count(*)` grouped by `b.grp` ties X=Y=3; `ORDER BY c DESC
/// LIMIT 1` keeps whichever tied group was seen FIRST in production order. ON
/// must equal OFF — and it does only because the columnar reduction accumulates
/// groups in first-seen production order. This is the query the CANARY perturbs.
#[test]
fn agg_first_seen_order_decides_the_tie() {
    let g = ga();
    let src = "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, count(*) AS c ORDER BY c DESC LIMIT 1";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "first-seen tie: columnar vs general disagree");
    // The tie IS at the max (two groups share the largest count), so the LIMIT-1
    // cut is decided purely by first-seen order — the canary's target.
    assert_eq!(on.len(), 1, "LIMIT 1 keeps exactly one group");
    assert_eq!(
        on[0][1],
        Value::Int(3),
        "the kept group is a max-count group"
    );
    assert!(agg_fired(&g, src), "operator did not fire");
    // A second slice: LIMIT 2 keeps the max plus the first-seen of the runner-up
    // tie (Y=null=... depends on the fixture), again first-seen-decided.
    assert_agrees(
        &g,
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, count(*) AS c ORDER BY c DESC LIMIT 2",
    );
}

/// A RETURN-level aggregate (Form B) with the count aliased and ordered by the
/// alias — the plain analytical-loser shape.
#[test]
fn agg_return_level_aggregate() {
    let g = ga();
    let src =
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, count(*) AS total ORDER BY total DESC, g ASC";
    assert_agrees(&g, src);
    assert!(agg_fired(&g, src), "operator did not fire");
}

/// DECLINE set: shapes the generalised group-by-aggregate still cannot feed
/// column-natively must DECLINE from the pipeline (so the general path answers)
/// yet still agree ON vs OFF. Phase 3b MOVED sum/avg/min/max/collect, DISTINCT,
/// multiple aggregates, compound expressions and the global aggregate INTO the
/// accept set (see `pipeline_aggregate2.rs`); what remains declined is a
/// projection-level DISTINCT, a key/arg spanning >1 var, and a second MATCH.
#[test]
fn agg_declines_outside_the_class() {
    let g = ga();
    let declines: &[&str] = &[
        // A projection-level DISTINCT (post-aggregation dedup).
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN DISTINCT b.grp AS g, count(*) AS c ORDER BY g",
        // A grouping key spanning TWO vars.
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN a.ak + b.bk AS k, count(*) AS c ORDER BY k",
        // A count argument spanning TWO vars.
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, count(a.ak + b.bk) AS c ORDER BY g",
        // A sum argument spanning TWO vars — an aggregate over a non-column value.
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, sum(a.ak + b.bk) AS s ORDER BY g",
        // An avg argument spanning TWO vars.
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, avg(a.ak + b.bk) AS s ORDER BY g",
        // A second MATCH after the aggregating WITH.
        "MATCH (a:Ag)-[:R]->(b:Bg) WITH b, count(*) AS c MATCH (b)-[:R2]->(d:Cg) RETURN b.bk AS bk, c, d.ck AS ck ORDER BY bk, ck",
    ];
    for src in declines {
        assert_agrees(&g, src);
        assert!(!agg_fired(&g, src), "should have DECLINED: `{src}`");
    }
}

/// The census aggregation shapes batch.rs owns (a single-var scan, a single-var
/// histogram — NO hop) must still DECLINE from the pipeline, so batch.rs keeps
/// answering them and the decoded-corpus digest does not move.
#[test]
fn agg_declines_census_shapes_to_batch() {
    let g = ga();
    for src in [
        "MATCH (n:Bg) RETURN count(*) AS c",
        "MATCH (n:Bg) RETURN n.grp AS g, count(*) AS c ORDER BY g",
        "MATCH (n:Bg) WITH n.grp AS g, count(*) AS c RETURN g, c ORDER BY g",
    ] {
        assert_agrees(&g, src);
        assert!(
            !agg_fired(&g, src),
            "a census shape must decline to batch.rs: `{src}`"
        );
    }
}

/// The count of a named `counted!` counter after running `src` with columnar ON.
fn counter(g: &Graph, src: &str, key: &str) -> u64 {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    trace.counters().get(key).copied().unwrap_or(0)
}

/// The COUNT-FOLD variants of the accepted shapes: `count(*)` keyed by the
/// SEED (the tail folds to a degree sum) or global, Form B and A, ORDER BY the
/// count with a tie cut, and the 2-hop chain — the triple (fold ON / fold OFF /
/// general) agrees and the fold fires. The `count(x)` / property-key / tail-key
/// shapes keep the aggregate operator WITHOUT the fold (declines, still agree).
#[test]
fn agg_count_star_folds_the_unread_tail() {
    let g = ga();
    type Rows = Vec<Vec<Value>>;
    let triple = |src: &str| -> (Rows, Rows, Rows) {
        g.set_columnar_scans(true);
        engram_graph::pipeline::set_count_fold(true);
        let on = rows(&g, src, BTreeMap::new());
        engram_graph::pipeline::set_count_fold(false);
        let fold_off = rows(&g, src, BTreeMap::new());
        engram_graph::pipeline::set_count_fold(true);
        g.set_columnar_scans(false);
        let general = rows(&g, src, BTreeMap::new());
        g.set_columnar_scans(true);
        (on, fold_off, general)
    };
    let folds: &[&str] = &[
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN a.ak AS ak, count(*) AS c ORDER BY ak ASC",
        "MATCH (a:Ag)-[:R]->(b:Bg) WITH a.ak AS ak, count(*) AS c RETURN ak, c ORDER BY ak ASC",
        "MATCH (a:Ag)-[:R]->(b:Bg) WITH a, count(*) AS c RETURN a.ak AS ak, c ORDER BY c DESC, ak ASC",
        // (The GLOBAL single-hop `count(*)` is a census shape another operator
        // owns before the pipeline is tried; the CONST-keyed form reaches the
        // reducer's general key path with the folded weights.)
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN 1 AS one, count(*) AS c ORDER BY c",
        "MATCH (a:Ag)-[:R]->(b:Bg)-[:R2]->(c:Cg) RETURN a.ak AS ak, count(*) AS n ORDER BY n DESC, ak ASC",
        "MATCH (a:Ag)-[:R]->(b:Bg)-[:R2]->(c:Cg) RETURN count(*) AS n",
        // The MID var keyed: only the far hop folds (a degree sum per b).
        "MATCH (a:Ag)-[:R]->(b:Bg)-[:R2]->(c:Cg) WITH b, count(*) AS n RETURN b.bk AS bk, n ORDER BY n DESC, b.bk ASC",
        // Tie cut by LIMIT under a folded tail.
        "MATCH (a:Ag)-[:R]->(b:Bg)-[:R2]->(c:Cg) RETURN b.grp AS g, count(*) AS n ORDER BY n DESC LIMIT 1",
        "MATCH (a:Ag)-[:R]->(b:Bg) WITH a.ak AS ak, count(*) AS c WHERE c >= 2 RETURN ak, c ORDER BY ak",
    ];
    for src in folds {
        let (on, fold_off, general) = triple(src);
        assert_eq!(on, general, "fold ON vs general disagree: `{src}`");
        assert_eq!(fold_off, general, "fold OFF vs general disagree: `{src}`");
        assert_eq!(counter(&g, src, "interp.pipeline count fold"), 1, "fold did not fire: `{src}`");
        assert!(agg_fired(&g, src), "operator did not fire: `{src}`");
    }
    // a0 has 5 R edges, a1 3, a2 1, a3 none (no group).
    let (on, _, _) = triple("MATCH (a:Ag)-[:R]->(b:Bg) RETURN a.ak AS ak, count(*) AS c ORDER BY ak ASC");
    assert_eq!(
        on,
        vec![
            vec![Value::Int(1), Value::Int(5)],
            vec![Value::Int(2), Value::Int(3)],
            vec![Value::Int(3), Value::Int(1)]
        ]
    );
    let declines: &[&str] = &[
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN a.ak AS ak, count(b) AS c ORDER BY ak ASC",
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN a.ak AS ak, count(b.grp) AS c ORDER BY ak ASC",
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, count(*) AS c ORDER BY c DESC, g ASC",
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.bk AS bk, count(*) AS c ORDER BY c DESC, b.bk ASC LIMIT 4",
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN 1 AS one, count(*) AS c, collect(b.bk) AS bks",
    ];
    for src in declines {
        let (on, fold_off, general) = triple(src);
        assert_eq!(on, general, "columnar vs general disagree: `{src}`");
        assert_eq!(fold_off, general, "fold OFF vs general disagree: `{src}`");
        assert_eq!(counter(&g, src, "interp.pipeline count fold"), 0, "fold must decline: `{src}`");
        assert!(agg_fired(&g, src), "operator did not fire: `{src}`");
    }
}

/// The group projection reads its group key ONLY through properties (`b.bk`,
/// `b.grp`), so it GATHERS those columns instead of materialising the whole Bg
/// node per group. Covers the ABSENT-property case: b4 has no `grp`, so its group
/// key is `Value::Null` — byte-identical to `value_of(node).grp` on a missing
/// property. Asserts ON==OFF, that the per-group full-node decode dropped to ZERO
/// (six groups, formerly six decodes), and that the gather fired.
#[test]
fn grouped_node_props_gather_not_the_whole_node() {
    let g = ga();
    let src = "MATCH (a:Ag)-[:R]->(b:Bg) \
               RETURN b.bk AS bk, b.grp AS g, count(*) AS c ORDER BY bk";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "gathered node-prop group-by ON must equal OFF");
    // b4 (bk=4) has grp absent -> the gathered group key is Null, exactly as a
    // full-node decode's missing property yields.
    assert!(
        on.iter()
            .any(|r| r == &vec![Value::Int(4), Value::Null, Value::Int(2)]),
        "the absent-grp group's key gathers as Null: {on:?}"
    );
    assert_eq!(
        counter(&g, src, "graph.nodes materialised in full"),
        0,
        "the group projection gathers b.bk/b.grp, it never decodes the Bg node"
    );
    assert_eq!(
        counter(&g, src, "interp.agg group-key props gathered"),
        1,
        "the group-key property gather fired"
    );
    assert!(agg_fired(&g, src), "the group-by-aggregate operator fired");
}

/// FALLBACK: Form A carries the group node WHOLE (`WITH b, count(*)`) so the
/// downstream RETURN can read `b.bk` off it — the property Map cannot stand in
/// for the node, so the group projection MUST still materialise it. ON==OFF; the
/// gather does NOT fire; the full-node decode is retained.
#[test]
fn whole_node_carry_still_materialises() {
    let g = ga();
    let src = "MATCH (a:Ag)-[:R]->(b:Bg) WITH b, count(*) AS c \
               RETURN b.bk AS bk, c ORDER BY c DESC, b.bk ASC";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "whole-node carry ON must equal OFF");
    // Fix 25: the carry is bare in the WITH, but every clause AFTER it reads
    // `b` by property (`b.bk`), so the gathered Map is what they read — the
    // full per-group materialisation this test used to pin is gone.
    assert!(
        counter(&g, src, "interp.agg group-key props gathered") > 0,
        "a bare carry read only by property after the WITH gathers"
    );
    assert!(
        counter(&g, src, "interp.agg bare group key gathered for its later reads") > 0
    );
    assert_eq!(
        counter(&g, src, "graph.nodes materialised in full"),
        0,
        "'WITH b … RETURN b.bk' no longer materialises the node"
    );
    // CONTROL: a later BARE use keeps the full-node materialisation.
    let bare = "MATCH (a:Ag)-[:R]->(b:Bg) WITH b, count(*) AS c \
                RETURN b, c ORDER BY c DESC, b.bk ASC";
    let (on, off) = both(&g, bare, BTreeMap::new());
    assert_eq!(on, off, "bare later use ON must equal OFF");
    assert_eq!(counter(&g, bare, "interp.agg group-key props gathered"), 0);
    assert!(
        counter(&g, bare, "graph.nodes materialised in full") > 0,
        "'RETURN b' keeps the per-group full-node materialisation"
    );
}

/// The group-key COLUMN the reduction loads to FORM the groups is REUSED by the
/// projection instead of a second point-gather: grouping by `b.bk` and reading
/// only `b.bk` — `reduce_agg_groups` loaded `b.bk` for the key, so the projection
/// reuses it (`interp.agg group-key cols reused` fires) rather than re-loading.
#[test]
fn group_key_projection_reuses_the_reduction_column() {
    let g = ga();
    let src = "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.bk AS bk, count(*) AS c ORDER BY bk";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "reused group-key projection ON must equal OFF");
    assert_eq!(
        counter(&g, src, "interp.agg group-key cols reused"),
        1,
        "the projection reused the reduction's loaded group-key column"
    );
    assert_eq!(
        counter(&g, src, "graph.nodes materialised in full"),
        0,
        "no full Bg node decoded"
    );
    assert!(agg_fired(&g, src), "the group-by-aggregate operator fired");
}
