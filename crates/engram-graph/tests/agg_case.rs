#![allow(non_snake_case)]
//! `sum(CASE WHEN <chained range> THEN 1 ELSE 0 END)` grouped — the A2 primitive
//! behind IC4. `eval_column`/`key_side` gained a `Case` arm, and a chained
//! comparison `a <= x < b` already desugars to `And(Bin,Bin)`, so the aggregate
//! site arg vectorises. Byte-identical to the interp, and the group-by aggregate
//! fires columnar (does not stream).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // Posts in two tag-groups with creationDate values straddling the range
    // [10, 20): some inside, some below, some at/above the upper bound.
    let mk = |group: &str, date: i64| {
        let mut m = BTreeMap::new();
        m.insert("grp".to_string(), Value::Str(group.into()));
        m.insert("date".to_string(), Value::Int(date));
        g.create_node(&["Post".into()], &m).expect("post");
    };
    for d in [5, 10, 12, 19, 20, 25] {
        mk("A", d); // in-range: 10,12,19 → 3
    }
    for d in [9, 15, 30] {
        mk("B", d); // in-range: 15 → 1
    }
    g
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run: {e}"))
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

fn fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    !trace
        .sometimes_hit()
        .contains("interp.streamed a read-only chain")
}

fn i(n: i64) -> Value {
    Value::Int(n)
}
fn s(x: &str) -> Value {
    Value::Str(x.into())
}

#[test]
fn sum_case_range_grouped_is_byte_identical_and_fires() {
    let g = g();
    let src = "MATCH (p:Post) RETURN p.grp AS grp, \
        sum(CASE WHEN 10 <= p.date < 20 THEN 1 ELSE 0 END) AS inRange \
        ORDER BY inRange DESC, grp ASC";
    let (on, off) = both(&g, src);
    assert_eq!(on, off, "sum(CASE range) columnar vs interp disagree");
    assert_eq!(
        on,
        vec![vec![s("A"), i(3)], vec![s("B"), i(1)]],
        "A has 3 posts in [10,20), B has 1"
    );
    assert!(
        fired(&g, src),
        "the sum(CASE …) group-by must run columnar, not stream"
    );
}

/// Two CASE sums in one projection (IC4's `postCount` + `inValidPostCount`
/// shape), with a HAVING-style Form-A WITH filter.
#[test]
fn two_case_sums_with_having_matches() {
    let g = g();
    let src = "MATCH (p:Post) \
        WITH p.grp AS grp, \
          sum(CASE WHEN 10 <= p.date < 20 THEN 1 ELSE 0 END) AS inRange, \
          sum(CASE WHEN p.date < 10 THEN 1 ELSE 0 END) AS below \
        WHERE inRange > 0 AND below = 0 \
        RETURN grp, inRange ORDER BY inRange DESC, grp ASC";
    let (on, off) = both(&g, src);
    assert_eq!(on, off, "two-CASE-sum + HAVING columnar vs interp disagree");
    // A: inRange=3, below (date<10 → 5) = 1 → filtered out. B: inRange=1,
    // below (date<10 → 9) = 1 → filtered out. So both groups have below>0.
    assert_eq!(
        on,
        Vec::<Vec<Value>>::new(),
        "both groups have a below-range post"
    );
}
