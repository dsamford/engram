#![allow(non_snake_case)]
//! Differential tests for Phase 3b of the composable columnar pipeline's
//! group-by-aggregate operator (`pipeline::plan_and_run_columnar`): the
//! generalisation from count-only to the FULL aggregate set — `sum`, `avg`,
//! `min`, `max`, `collect`, `count` — with DISTINCT, MULTIPLE aggregates per
//! projection, COMPOUND aggregate expressions, MULTI-VAR grouping keys, and a
//! GLOBAL (no grouping key) aggregate. The contract is the same as
//! `pipeline_aggregate.rs`: for every accepted shape, `set_columnar_scans(true)`
//! (the columnar reduction reusing `run_streaming`'s own `SiteAcc` fold, folded
//! in PRODUCTION order) must equal `set_columnar_scans(false)` (the per-tuple
//! `run_streaming` aggregation) — the full ROW SET *and its order*, byte-for-byte
//! — and the AGGREGATE path must FIRE.
//!
//! Two load-bearing orders are under test, each with a canary in the operator:
//! (1) FIRST-SEEN group order (a stable ORDER BY breaks ties on it) — perturbing
//! the reduction's accumulation order diverges `agg_first_seen_order_decides_the_tie`
//! in `pipeline_aggregate.rs`; (2) COLLECT encounter order — because the pipeline
//! pushes each site's argument in production row order, a `collect`'s list order
//! is byte-identical, and reversing the per-row push loop flips it (the
//! `agg2_collect_order_is_production_order` list-order assertions).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// A richer fixture than `ga()`: A{aid} -R-> B{grp (X/Y/Z + null), bk (int +
/// null), gender} -R2-> C{ck}. Duplicate edges reach the same B (b0 from a0 and
/// a1; b2 twice) so DISTINCT differs from non-DISTINCT and a `collect` group
/// carries several elements in a deterministic production order; b3/b5 have a
/// NULL bk (sum/avg/min/max/collect null semantics), and the whole Z group (b5)
/// is all-null.
fn ga2() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk_a = |aid: i64| {
        let mut p = BTreeMap::new();
        p.insert("aid".to_string(), Value::Int(aid));
        g.create_node(&["Ag".into()], &p).expect("a")
    };
    let a = [mk_a(1), mk_a(2), mk_a(3), mk_a(4)];
    let mk_b = |grp: Option<&str>, bk: Option<i64>, gender: &str| {
        let mut p = BTreeMap::new();
        if let Some(s) = grp {
            p.insert("grp".to_string(), Value::Str(s.to_string()));
        }
        if let Some(k) = bk {
            p.insert("bk".to_string(), Value::Int(k));
        }
        p.insert("gender".to_string(), Value::Str(gender.to_string()));
        g.create_node(&["Bg".into()], &p).expect("b")
    };
    let b = [
        mk_b(Some("X"), Some(10), "M"), // b0
        mk_b(Some("X"), Some(20), "F"), // b1
        mk_b(Some("Y"), Some(30), "M"), // b2
        mk_b(Some("Y"), None, "F"),     // b3 — bk NULL
        mk_b(None, Some(40), "M"),      // b4 — grp NULL
        mk_b(Some("Z"), None, "F"),     // b5 — grp Z, bk NULL (all-null group)
    ];
    let mk_c = |ck: i64| {
        let mut p = BTreeMap::new();
        p.insert("ck".to_string(), Value::Int(ck));
        g.create_node(&["Cg".into()], &p).expect("c")
    };
    let c = [mk_c(100), mk_c(200), mk_c(300)];
    // Creation order is load-bearing (drives reverse-adjacency).
    for (s, d) in [
        (0, 0),
        (0, 1),
        (0, 2),
        (0, 3),
        (0, 4),
        (1, 0), // b0 again — duplicate group-X member
        (1, 5),
        (1, 2), // b2 again — duplicate group-Y member
        (2, 4), // b4 again — null-grp member
    ] {
        g.create_rel(a[s], "R", b[d], &BTreeMap::new()).expect("R");
    }
    for (s, d) in [(0, 0), (0, 1), (1, 2), (2, 0)] {
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

/// Whether the GROUP-BY-AGGREGATE operator fired for `src` with columnar ON.
fn agg_fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, BTreeMap::new()));
    trace
        .counters()
        .get("interp.pipeline aggregate runs")
        .copied()
        == Some(1)
}

/// The pipeline must ANSWER (fire) and its output must equal the general path's,
/// row-for-row and in order.
fn agrees_and_fires(g: &Graph, src: &str) {
    let (on, off) = both(g, src, BTreeMap::new());
    assert_eq!(on, off, "columnar vs general disagree: `{src}`");
    assert!(agg_fired(g, src), "operator did not fire: `{src}`");
}

/// sum / avg / min / max grouped by a var.prop AND by a node, over 1-hop and
/// 2-hop chains, Form B and Form A — each must fire and match the general path.
#[test]
fn agg2_scalar_aggregates_fire_and_match() {
    let g = ga2();
    let cases: &[&str] = &[
        // Grouped by a property (X/Y/Z + a NULL group), 1-hop, Form B.
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, sum(b.bk) AS s ORDER BY g",
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, avg(b.bk) AS a ORDER BY g",
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, min(b.bk) AS mn ORDER BY g",
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, max(b.bk) AS mx ORDER BY g",
        // Grouped by the mid NODE, Form A.
        "MATCH (a:Ag)-[:R]->(b:Bg) WITH b, sum(b.bk) AS s RETURN b.bk AS bk, s ORDER BY bk",
        // Grouped by the START-side property.
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN a.aid AS aid, sum(b.bk) AS s, max(b.bk) AS mx ORDER BY aid",
        // 2-hop chain: sum over the far side grouped by the mid node.
        "MATCH (a:Ag)-[:R]->(b:Bg)-[:R2]->(c:Cg) RETURN b.bk AS bk, sum(c.ck) AS s, min(c.ck) AS mn ORDER BY bk",
        // 2-hop, grouped by the far node's property, Form A.
        "MATCH (a:Ag)-[:R]->(b:Bg)-[:R2]->(c:Cg) WITH c.ck AS ck, avg(b.bk) AS a RETURN ck, a ORDER BY ck",
    ];
    for src in cases {
        agrees_and_fires(&g, src);
    }
}

/// `collect` grouped by a var.prop AND by a node, 1-hop and 2-hop — the list's
/// ELEMENT ORDER is production order, so ON must equal OFF exactly (the columnar
/// path pushes each element in production row order, reproducing the fold). This
/// also SHADOWS the previously-divergent batch.rs hop-chain `collect` fast path.
#[test]
fn agg2_collect_order_is_production_order() {
    let g = ga2();
    let cases: &[&str] = &[
        // collect grouped by a property (group X carries [10,10,20] etc.).
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, collect(b.bk) AS l ORDER BY g",
        // collect the START-side id grouped by the mid node (multiple a per b).
        "MATCH (a:Ag)-[:R]->(b:Bg) WITH b, collect(a.aid) AS l RETURN b.bk AS bk, l ORDER BY bk",
        // 2-hop collect grouped by a property.
        "MATCH (a:Ag)-[:R]->(b:Bg)-[:R2]->(c:Cg) RETURN b.grp AS g, collect(c.ck) AS l ORDER BY g",
    ];
    for src in cases {
        agrees_and_fires(&g, src);
    }
    // The explicit list-order contract the collect-order canary perturbs: group X
    // collects b.bk in production order. Both paths must produce the SAME list.
    let (on, off) = both(
        &g,
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, collect(b.bk) AS l ORDER BY g",
        BTreeMap::new(),
    );
    assert_eq!(on, off, "collect list order: columnar vs general disagree");
    let x_row = on
        .iter()
        .find(|r| r[0] == Value::Str("X".into()))
        .expect("an X group");
    // X is reached a0->b0, a0->b1, a1->b0 — the columnar path emits b0's two
    // arrivals and b1's in production (scan × reverse-adjacency) order; whatever
    // that order is, it is a NON-trivial multi-element list the canary flips.
    let Value::List(items) = &x_row[1] else {
        panic!("collect yields a list, got {:?}", x_row[1]);
    };
    assert_eq!(items.len(), 3, "group X collects three non-null bk values");
}

/// DISTINCT inside `count` / `collect` / `sum` — the DISTINCT set folds through
/// the SAME canonical-key dedup `run_streaming` uses, applied per site.
#[test]
fn agg2_distinct_aggregates() {
    let g = ga2();
    let cases: &[&str] = &[
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, count(DISTINCT b.bk) AS c ORDER BY g",
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, collect(DISTINCT b.bk) AS l ORDER BY g",
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, sum(DISTINCT b.bk) AS s ORDER BY g",
        // DISTINCT and non-DISTINCT side by side in one projection.
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, sum(b.bk) AS s, sum(DISTINCT b.bk) AS sd ORDER BY g",
    ];
    for src in cases {
        agrees_and_fires(&g, src);
    }
    // DISTINCT actually bites: group X reaches b0(10) twice, so sum != sum DISTINCT.
    let (on, _) = both(
        &g,
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, sum(b.bk) AS s, sum(DISTINCT b.bk) AS sd ORDER BY g",
        BTreeMap::new(),
    );
    let x = on
        .iter()
        .find(|r| r[0] == Value::Str("X".into()))
        .expect("an X group");
    assert_eq!(x[1], Value::Int(40), "sum(bk) over X = 10+10+20");
    assert_eq!(x[2], Value::Int(30), "sum(DISTINCT bk) over X = 10+20");
}

/// MULTIPLE aggregates in one projection (the `WITH b, count(*), collect(a.id),
/// max(b.creationDate)` shape), each an independent site folded in lockstep.
#[test]
fn agg2_multiple_aggregates_per_projection() {
    let g = ga2();
    let cases: &[&str] = &[
        "MATCH (a:Ag)-[:R]->(b:Bg) WITH b, count(*) AS c, collect(a.aid) AS l, max(b.bk) AS mx RETURN b.bk AS bk, c, l, mx ORDER BY bk",
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, count(*) AS c, sum(b.bk) AS s, avg(b.bk) AS a, min(b.bk) AS mn ORDER BY g",
    ];
    for src in cases {
        agrees_and_fires(&g, src);
    }
}

/// COMPOUND aggregate expressions — an aggregate inside an arithmetic expression,
/// and two aggregates combined — rewritten to `$__aggN` and evaluated per group.
#[test]
fn agg2_compound_aggregate_expressions() {
    let g = ga2();
    let cases: &[&str] = &[
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, sum(b.bk) + 1 AS s1 ORDER BY g",
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, 1.0 * sum(b.bk) / count(*) AS mean ORDER BY g",
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, count(*) + count(b.bk) AS both ORDER BY g",
    ];
    for src in cases {
        agrees_and_fires(&g, src);
    }
}

/// GLOBAL aggregate — no grouping key, one group over all live rows. Includes the
/// zero-rows case (a global aggregate over an empty match still yields ONE row).
/// (A bare `RETURN count(*)` is owned by the faster `try_count_fast` path, so the
/// global cases here carry a second aggregate or a non-count aggregate to reach
/// the pipeline.)
#[test]
fn agg2_global_aggregate() {
    let g = ga2();
    let cases: &[&str] = &[
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN sum(b.bk) AS s",
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN count(*) AS c, sum(b.bk) AS s, avg(b.bk) AS a, collect(b.bk) AS l",
        // 2-hop global.
        "MATCH (a:Ag)-[:R]->(b:Bg)-[:R2]->(c:Cg) RETURN count(*) AS c, max(c.ck) AS mx",
        // Global over ZERO rows (unminted type) — one row, count 0 / sum 0 / avg null.
        "MATCH (a:Ag)-[:NOPE]->(b:Bg) RETURN count(*) AS c, sum(b.bk) AS s, avg(b.bk) AS a",
    ];
    for src in cases {
        agrees_and_fires(&g, src);
    }
    // The empty-global contract explicitly.
    let (on, _) = both(
        &g,
        "MATCH (a:Ag)-[:NOPE]->(b:Bg) RETURN count(*) AS c, sum(b.bk) AS s, avg(b.bk) AS a",
        BTreeMap::new(),
    );
    assert_eq!(
        on,
        vec![vec![Value::Int(0), Value::Int(0), Value::Null]],
        "a global aggregate over zero rows yields one row"
    );
}

/// MULTI-VAR grouping key (`WITH a, b.gender, count(*)`) — several single-var keys
/// spanning different vars, keyed through the general (canonical) reduction.
#[test]
fn agg2_multi_var_grouping_key() {
    let g = ga2();
    let cases: &[&str] = &[
        "MATCH (a:Ag)-[:R]->(b:Bg) WITH a, b.gender AS gender, count(*) AS c RETURN a.aid AS aid, gender, c ORDER BY aid, gender",
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN a.aid AS aid, b.gender AS gender, sum(b.bk) AS s ORDER BY aid, gender",
        // A const key beside a var key.
        "MATCH (a:Ag)-[:R]->(b:Bg) RETURN 1 AS one, b.grp AS g, count(*) AS c ORDER BY g",
    ];
    for src in cases {
        agrees_and_fires(&g, src);
    }
}

/// NULL handling: `sum`/`collect` skip nulls; `avg` over an all-null group is
/// null; `min`/`max` over nulls skip them (all-null → null). The Z group (b5 only,
/// bk null) is the all-null case; the Y group mixes 30,30 with a null.
#[test]
fn agg2_null_handling() {
    let g = ga2();
    let src = "MATCH (a:Ag)-[:R]->(b:Bg) RETURN b.grp AS g, sum(b.bk) AS s, avg(b.bk) AS a, min(b.bk) AS mn, max(b.bk) AS mx, collect(b.bk) AS l ORDER BY g";
    agrees_and_fires(&g, src);
    let (on, _) = both(&g, src, BTreeMap::new());
    let z = on
        .iter()
        .find(|r| r[0] == Value::Str("Z".into()))
        .expect("a Z group (all-null bk)");
    assert_eq!(z[1], Value::Int(0), "sum over all-null → 0");
    assert_eq!(z[2], Value::Null, "avg over all-null → null");
    assert_eq!(z[3], Value::Null, "min over all-null → null");
    assert_eq!(z[4], Value::Null, "max over all-null → null");
    assert_eq!(z[5], Value::List(vec![]), "collect skips nulls → []");
    let y = on
        .iter()
        .find(|r| r[0] == Value::Str("Y".into()))
        .expect("a Y group (30,30,null)");
    assert_eq!(y[1], Value::Int(60), "sum skips the null → 60");
    assert_eq!(y[2], Value::Float(30.0), "avg over the two non-null → 30");
    assert_eq!(
        y[5],
        Value::List(vec![Value::Int(30), Value::Int(30)]),
        "collect skips the null"
    );
}

/// A HAVING-style post-WITH WHERE over a NON-count aggregate alias (Form A) — the
/// group rows stay first-seen, then the post-WITH filter and plain RETURN run.
#[test]
fn agg2_form_a_post_where_over_aggregate() {
    let g = ga2();
    let cases: &[&str] = &[
        "MATCH (a:Ag)-[:R]->(b:Bg) WITH b.grp AS g, sum(b.bk) AS s WHERE s >= 40 RETURN g, s ORDER BY s DESC, g",
        "MATCH (a:Ag)-[:R]->(b:Bg) WITH b.grp AS g, collect(b.bk) AS l, count(*) AS c WHERE c >= 2 RETURN g, l, c ORDER BY g",
    ];
    for src in cases {
        agrees_and_fires(&g, src);
    }
}
