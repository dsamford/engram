#![allow(non_snake_case)]
//! Differential tests for RELATIONSHIP-VARIABLE binding + rel-property use in the
//! composable columnar pipeline (`pipeline::plan_and_run_columnar`). The contract
//! is the same as every other `pipeline_*` suite: for every shape the pipeline
//! ACCEPTS, running with `set_columnar_scans(true)` (the pipeline) must equal
//! `set_columnar_scans(false)` (the per-tuple `run_streaming` path) — the full row
//! SET and its ORDER, byte-for-byte — and for every shape it DECLINES the general
//! path answers and the two still agree. The oracle is the same query columnar
//! OFF.
//!
//! What is new here: a bound relationship variable `(a)-[r:T]->(b)` becomes an
//! extra Rel-kind column, whose properties (`r.since`, `r.w`) are read from the
//! RELATIONSHIP column family and whose identity materialises through `rel_of` to
//! a `Value::Rel`. Covered: `WHERE r.prop <cmp> const` over a hop (top-k);
//! `RETURN r.prop` and `RETURN r` (full rel materialisation); `ORDER BY r.prop`
//! (incl. NULL placement); a rel prop as a group-by key and in aggregates
//! (`min`/`max`/`collect`(r.prop), `count(r)` presence); a rel var on an incoming
//! and an UNDIRECTED hop; two rel vars in a multi-hop chain; a rel var on a
//! semijoin's closing hop and in an OPTIONAL left join (nullable, three-valued);
//! plus DECLINE shapes (var-length rel, inline rel-property map, a rel-var use
//! that spans two vars) that must fall back identically.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// A: At{ak}; B: Bt{bx}. T (a->b) carries {since (int; a tie, a NULL), w (int)},
/// including a DUPLICATE a0->b0 edge with distinct props (multiplicity + distinct
/// rel identity). U (b->a) carries {since} and closes two triangles (b0->a0,
/// b1->a2), so a rel var can ride a semijoin, a multi-hop chain and an incoming /
/// undirected hop. a3 has NO outgoing T, so an OPTIONAL over it null-fills.
fn gr() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk_a = |ak: i64| {
        let mut p = BTreeMap::new();
        p.insert("ak".to_string(), Value::Int(ak));
        g.create_node(&["At".into()], &p).expect("a")
    };
    let a = [mk_a(1), mk_a(2), mk_a(3), mk_a(4)];
    let mk_b = |bx: i64| {
        let mut p = BTreeMap::new();
        p.insert("bx".to_string(), Value::Int(bx));
        g.create_node(&["Bt".into()], &p).expect("b")
    };
    let b = [mk_b(10), mk_b(20), mk_b(30), mk_b(40)];

    let t = |s: usize, d: usize, since: Option<i64>, w: i64| {
        let mut p = BTreeMap::new();
        if let Some(v) = since {
            p.insert("since".to_string(), Value::Int(v));
        }
        p.insert("w".to_string(), Value::Int(w));
        g.create_rel(a[s], "T", b[d], &p).expect("T");
    };
    t(0, 0, Some(5), 100);
    t(0, 1, Some(5), 200); // tie on `since` with a0->b0
    t(0, 2, Some(3), 50);
    t(1, 0, Some(8), 300);
    t(1, 1, None, 400); // since NULL — three-valued
    t(2, 3, Some(1), 10);
    t(0, 0, Some(9), 500); // a SECOND a0->b0 edge, distinct rel + props

    let u = |s: usize, d: usize, since: i64| {
        let mut p = BTreeMap::new();
        p.insert("since".to_string(), Value::Int(since));
        g.create_rel(b[s], "U", a[d], &p).expect("U");
    };
    u(0, 0, 2); // b0 -> a0 (triangle with a0-[T]->b0)
    u(1, 2, 7); // b1 -> a2
    g
}

fn rows(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params)
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn cols(g: &Graph, src: &str) -> Vec<String> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .columns
}

/// Run `src` with the pipeline ON and the general path OFF.
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

/// Whether ANY columnar-pipeline operator (core hop, aggregate, or optional)
/// produced the answer for `src` with columnar ON — proves a differential is not
/// vacuous (the pipeline actually fired, rather than silently declining).
fn pipeline_fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    let c = trace.counters();
    let hit = |k: &str| c.get(k).copied() == Some(1);
    hit("interp.pipeline hop runs")
        || hit("interp.pipeline aggregate runs")
        || hit("interp.pipeline optional runs")
}

/// Assert ON == OFF (rows AND order) and that the pipeline FIRED — the accepted
/// shapes.
fn accept(g: &Graph, src: &str) {
    let (on, off) = both(g, src, BTreeMap::new());
    assert_eq!(on, off, "ON != OFF for accepted shape: `{src}`");
    assert!(pipeline_fired(g, src), "pipeline did NOT fire: `{src}`");
}

#[test]
fn relvar_where_orderby_topk() {
    let g = gr();
    // WHERE over a rel prop + ORDER BY a rel prop + LIMIT (the IS3 / IC5 shape).
    // NULL `since` fails `> const` (three-valued) and sorts last ascending.
    for src in [
        "MATCH (a:At)-[r:T]->(b:Bt) WHERE r.since > 3 RETURN a.ak AS ak, r.since AS s ORDER BY r.since DESC, a.ak LIMIT 5",
        "MATCH (a:At)-[r:T]->(b:Bt) WHERE r.since > 3 RETURN a.ak AS ak, r.since AS s ORDER BY r.since DESC, a.ak LIMIT 2",
        "MATCH (a:At)-[r:T]->(b:Bt) RETURN a.ak AS ak, r.w AS w ORDER BY r.since, r.w LIMIT 100",
        "MATCH (a:At)-[r:T]->(b:Bt) RETURN a.ak AS ak, r.w AS w ORDER BY r.since DESC, r.w LIMIT 100",
        // A rel-prop WHERE combined with a NODE-side ORDER BY.
        "MATCH (a:At)-[r:T]->(b:Bt) WHERE r.w >= 200 RETURN a.ak AS ak, b.bx AS bx, r.w AS w ORDER BY b.bx, r.w LIMIT 100",
    ] {
        accept(&g, src);
    }
}

#[test]
fn relvar_return_property_and_production_order() {
    let g = gr();
    for src in [
        // Plain projection of a rel prop — production order, no sort.
        "MATCH (a:At)-[r:T]->(b:Bt) RETURN a.ak AS ak, r.since AS s, r.w AS w",
        // A rel prop mixed with node props.
        "MATCH (a:At)-[r:T]->(b:Bt) RETURN a.ak AS ak, b.bx AS bx, r.since AS s",
    ] {
        accept(&g, src);
    }
}

#[test]
fn relvar_return_whole_relationship() {
    let g = gr();
    // `RETURN r` materialises the WHOLE relationship (id, src, dst, type, props).
    // ON == OFF asserts the pipeline's `rel_of` value equals the general path's
    // `rel.to_value()` — the DUPLICATE a0->b0 edge proves distinct props/identity.
    let src = "MATCH (a:At)-[r:T]->(b:Bt) RETURN r ORDER BY r.since, r.w LIMIT 100";
    accept(&g, src);
    // And explicitly: every projected value is a Value::Rel carrying its type +
    // props (the general path is the oracle; here we pin the shape too).
    assert_eq!(cols(&g, src), vec!["r".to_string()]);
    let (on, _) = both(&g, src, BTreeMap::new());
    assert!(!on.is_empty());
    for row in &on {
        match &row[0] {
            Value::Rel {
                rel_type, props, ..
            } => {
                assert_eq!(rel_type, "T");
                assert!(props.contains_key("w"));
            }
            other => panic!("expected Value::Rel, got {other:?}"),
        }
    }
    // Plain `RETURN r` in production order too.
    accept(&g, "MATCH (a:At)-[r:T]->(b:Bt) RETURN r");
}

#[test]
fn relvar_groupby_and_aggregates() {
    let g = gr();
    for src in [
        // Group BY a rel prop; aggregate over rel props.
        "MATCH (a:At)-[r:T]->(b:Bt) RETURN r.since AS s, count(r) AS c, min(r.w) AS lo, max(r.w) AS hi ORDER BY s LIMIT 100",
        // Global aggregate over rel props (min/max/collect of a rel prop).
        "MATCH (a:At)-[r:T]->(b:Bt) RETURN count(r) AS c, min(r.since) AS lo, max(r.since) AS hi, collect(r.w) AS ws",
        // count(r) as PRESENCE, grouped by the NODE (like count(node)).
        "MATCH (a:At)-[r:T]->(b:Bt) RETURN a.ak AS ak, count(r) AS c ORDER BY ak",
        // count(*) vs count(r) vs count(DISTINCT r) beside a node count.
        "MATCH (a:At)-[r:T]->(b:Bt) RETURN count(*) AS star, count(r) AS c, count(DISTINCT r) AS d, count(a) AS na",
        // collect(r) — the WHOLE relationship folded.
        "MATCH (a:At)-[r:T]->(b:Bt) RETURN a.ak AS ak, collect(r) AS rs ORDER BY ak",
        // A rel-prop WHERE ahead of the aggregate.
        "MATCH (a:At)-[r:T]->(b:Bt) WHERE r.since IS NOT NULL RETURN r.since AS s, sum(r.w) AS tot ORDER BY s",
        // Form A: WITH aggregate over a rel prop, then RETURN.
        "MATCH (a:At)-[r:T]->(b:Bt) WITH a.ak AS ak, count(r) AS c RETURN ak, c ORDER BY c DESC, ak",
    ] {
        accept(&g, src);
    }
}

#[test]
fn relvar_incoming_and_undirected() {
    let g = gr();
    for src in [
        // Incoming hop with a rel var.
        "MATCH (a:At)<-[r:U]-(b:Bt) RETURN a.ak AS ak, r.since AS s ORDER BY r.since, a.ak LIMIT 100",
        // Undirected hop with a rel var — b0 has multiple incoming T edges.
        "MATCH (b:Bt)-[r:T]-(a:At) RETURN b.bx AS bx, r.since AS s, r.w AS w ORDER BY r.w, b.bx LIMIT 100",
        // Undirected, aggregating over the rel prop.
        "MATCH (b:Bt)-[r:T]-(a:At) RETURN b.bx AS bx, count(r) AS c, sum(r.w) AS w ORDER BY bx",
    ] {
        accept(&g, src);
    }
}

#[test]
fn relvar_multihop_two_rel_vars() {
    let g = gr();
    for src in [
        // Two rel vars in one chain: a -[r1:T]-> b -[r2:U]-> a2.
        "MATCH (a:At)-[r1:T]->(b:Bt)-[r2:U]->(a2:At) RETURN a.ak AS ak, r1.since AS s1, r2.since AS s2, a2.ak AS ak2 ORDER BY r1.since, r2.since, a2.ak LIMIT 100",
        // A rel prop from the FIRST hop only, with a WHERE on the SECOND.
        "MATCH (a:At)-[r1:T]->(b:Bt)-[r2:U]->(a2:At) WHERE r2.since > 1 RETURN a.ak AS ak, r1.w AS w ORDER BY r1.w, a.ak LIMIT 100",
        // Aggregate mixing both rel vars' props.
        "MATCH (a:At)-[r1:T]->(b:Bt)-[r2:U]->(a2:At) RETURN a2.ak AS ak2, count(r1) AS c, sum(r1.w) AS w, min(r2.since) AS lo ORDER BY ak2",
    ] {
        accept(&g, src);
    }
}

#[test]
fn relvar_on_semijoin_close() {
    let g = gr();
    for src in [
        // The CLOSING hop of a triangle binds the rel var: a-[:T]->b-[r:U]->a.
        "MATCH (a:At)-[:T]->(b:Bt)-[r:U]->(a) RETURN a.ak AS ak, r.since AS s ORDER BY r.since, a.ak LIMIT 100",
        // Aggregate over the closing hop's rel var.
        "MATCH (a:At)-[:T]->(b:Bt)-[r:U]->(a) RETURN a.ak AS ak, count(r) AS c, sum(r.since) AS s ORDER BY ak",
    ] {
        accept(&g, src);
    }
}

#[test]
fn relvar_optional_nullable_three_valued() {
    let g = gr();
    for src in [
        // OPTIONAL rel var — a3 has no T edge, so r (and r.since) is NULL there.
        "MATCH (a:At) OPTIONAL MATCH (a)-[r:T]->(b:Bt) RETURN a.ak AS ak, r.since AS s ORDER BY a.ak LIMIT 100",
        // Plain production order over the optional rel prop.
        "MATCH (a:At) OPTIONAL MATCH (a)-[r:T]->(b:Bt) RETURN a.ak AS ak, r.w AS w",
        // count(r) / collect(r) over the optional: the null-fill row contributes 0
        // / is omitted (NOT count(*)), three-valued exactly as the general path.
        "MATCH (a:At) OPTIONAL MATCH (a)-[r:T]->(b:Bt) RETURN a.ak AS ak, count(r) AS c ORDER BY ak",
        "MATCH (a:At) OPTIONAL MATCH (a)-[r:T]->(b:Bt) RETURN a.ak AS ak, count(*) AS star, count(r) AS c ORDER BY ak",
        "MATCH (a:At) OPTIONAL MATCH (a)-[r:T]->(b:Bt) RETURN a.ak AS ak, collect(r.w) AS ws ORDER BY ak",
    ] {
        accept(&g, src);
    }
}

/// DECLINE shapes: the pipeline must NOT fire (it cannot reproduce these), and
/// the general path answers identically (ON == OFF).
#[test]
fn relvar_declines_and_falls_back_identically() {
    let g = gr();
    let declines: &[&str] = &[
        // A VARIABLE-LENGTH rel — declined even with a rel var (the bound list
        // semantics differ from a fixed hop's single rel).
        "MATCH (a:At)-[r:T*1..2]->(b:Bt) RETURN b.bx AS x ORDER BY b.bx LIMIT 5",
        // An INLINE rel property MAP (`rel_satisfies` equality) — still declined.
        "MATCH (a:At)-[r:T {w: 100}]->(b:Bt) RETURN a.ak AS ak, r.since AS s ORDER BY ak LIMIT 5",
        // A WHERE spanning BOTH a rel var and a node var — not single-column.
        "MATCH (a:At)-[r:T]->(b:Bt) WHERE r.since > a.ak RETURN b.bx AS x ORDER BY b.bx LIMIT 5",
        // An ORDER BY key spanning the rel var AND a node var.
        "MATCH (a:At)-[r:T]->(b:Bt) RETURN b.bx AS x ORDER BY r.since + b.bx LIMIT 5",
    ];
    for src in declines {
        let (on, off) = both(&g, src, BTreeMap::new());
        assert_eq!(on, off, "decline+fallback disagreement: `{src}`");
        assert!(
            !pipeline_fired(&g, src),
            "pipeline should have DECLINED, not fired: `{src}`"
        );
    }
}

// ─── SPARSE rel-property grouping key: the point-gather fallback (Rels family) ──
//
// The Nodes twin of this lives in `pipeline_join.rs`. Here the grouping-key column
// is a RELATIONSHIP property (`r.since`) loaded from the Rels family. The two
// matched T-rels are created FAR APART in REL-ID space with 10 filler rels between
// them, all carrying `since`, so the `[min_rel, max_rel]` span holds 12 `since`
// entries — over the 2-rel budget (4×2 = 8) — forcing `load_rel_columns`' range
// scan to DECLINE and the point-gather fallback (Rels family) to fire.

/// A named counter's value after running `src` once with the pipeline ON.
fn counter(g: &Graph, src: &str, key: &str) -> u64 {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    trace.counters().get(key).copied().unwrap_or(0)
}

/// A SPARSE rel-grouping-key fixture: two matched `T` edges whose REL ids bracket
/// 10 filler `U` edges that also carry `since`. Creation order sets rel ids, so
/// the two T-rel ids are far apart and the `since` rel-column span blows the
/// 2-group budget — the `r.since` group-by must fall back to the Rels point-gather.
fn gr_sparse() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut pa = BTreeMap::new();
    pa.insert("ak".to_string(), Value::Int(1));
    let a0 = g.create_node(&["At".into()], &pa).expect("a");
    let mut pb = BTreeMap::new();
    pb.insert("bx".to_string(), Value::Int(10));
    let b0 = g.create_node(&["Bt".into()], &pb).expect("b");
    let t = |since: i64| {
        let mut p = BTreeMap::new();
        p.insert("since".to_string(), Value::Int(since));
        g.create_rel(a0, "T", b0, &p).expect("T");
    };
    let u = |since: i64| {
        let mut p = BTreeMap::new();
        p.insert("since".to_string(), Value::Int(since));
        g.create_rel(b0, "U", a0, &p).expect("U");
    };
    t(5); // T-rel #1 (rel id r0)
    for k in 0..10 {
        u(100 + k); // 10 filler rels between the two T-rel ids, each with `since`
    }
    t(9); // T-rel #2 (rel id r11)
    g
}

/// SPARSE rel-property group-by: the `r.since` grouping-key column's range scan
/// over the Rels family exceeds its budget and DECLINES; the point-gather loads
/// exactly the 2 matched rel ids. The aggregate FIRES and the result is
/// byte-identical to the general path.
#[test]
fn rel_sparse_grouping_key_gathers_and_fires() {
    let g = gr_sparse();
    g.set_columnar_column_budget_factor(1); // force the sparse range-scan decline
    let src = "MATCH (a:At)-[r:T]->(b:Bt) RETURN r.since AS s, count(*) AS c ORDER BY s";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "sparse rel group-by vs general disagree");
    assert_eq!(
        on,
        vec![
            vec![Value::Int(5), Value::Int(1)],
            vec![Value::Int(9), Value::Int(1)]
        ],
        "sparse rel group-by exact rows + order"
    );
    assert!(
        pipeline_fired(&g, src),
        "the sparse rel group-by must FIRE via the Rels point-gather"
    );
    assert!(
        counter(&g, src, "graph.column point-gather") > 0,
        "the sparse rel grouping-key column must fall back to the point-gather"
    );
}
