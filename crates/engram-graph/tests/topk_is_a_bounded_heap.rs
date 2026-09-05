#![allow(non_snake_case)]
//! Fix 43: the streaming projector's bounded top-k (`ORDER BY … [SKIP s]
//! LIMIT k`) is a binary heap, not a sorted vector with an O(k) insert per
//! row. The inbox page listing keeps k = skip + limit rows: at page 10
//! (`SKIP 10000 LIMIT 1000`) every one of its 18k rows moved up to 11,000
//! entries, ~11 GB of memmove per statement — page 10 cost 2.0 s more than
//! page 1 on the mirror for the SAME rows, decodes and expressions.
//!
//! The page must equal the fully sorted result's slice — ties included,
//! which the stable sort settles by arrival (production) order.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn general(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let r = rows(g, src);
    g.set_columnar_scans(true);
    r
}

/// 20,000 items whose `createdAt` has MANY ties (200 distinct values) and
/// a unique `id`; a third carry a `note`.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..20000i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i));
        m.insert(
            "createdAt".to_string(),
            Value::Str(format!("2026-{:02}-{:02}T{:02}:00:00Z", 1 + (i * 7 % 12), 1 + (i * 13 % 28), (i * 3) % 24 / 4)),
        );
        if i % 3 == 0 {
            m.insert("note".to_string(), Value::Str(format!("n{i}")));
        }
        g.create_node(&["Item".into()], &m).expect("item");
    }
    g
}

fn full_sorted(g: &Graph, desc: bool) -> Vec<Vec<Value>> {
    let dir = if desc { "DESC" } else { "ASC" };
    general(g, &format!("MATCH (n:Item) RETURN n.id AS id ORDER BY n.createdAt {dir}"))
}

#[test]
fn a_deep_page_equals_the_full_sort_s_slice_ties_included() {
    let g = corpus();
    let all = full_sorted(&g, true);
    assert_eq!(all.len(), 20000);
    for (skip, limit) in [(10000usize, 1000usize), (0, 5), (19990, 100), (7, 3), (0, 0), (20000, 10)] {
        let src = format!(
            "MATCH (n:Item) WITH n ORDER BY n.createdAt DESC SKIP {skip} LIMIT {limit} RETURN n.id AS id"
        );
        let want: Vec<Vec<Value>> = all.iter().skip(skip).take(limit).cloned().collect();
        assert_eq!(general(&g, &src), want, "general `{src}`");
        assert_eq!(rows(&g, &src), want, "columnar `{src}`");
        let src2 = format!("MATCH (n:Item) RETURN n.id AS id ORDER BY n.createdAt DESC SKIP {skip} LIMIT {limit}");
        assert_eq!(general(&g, &src2), want, "general `{src2}`");
        assert_eq!(rows(&g, &src2), want, "columnar `{src2}`");
    }
}

#[test]
fn ascending_and_two_key_orders_page_the_same_way() {
    let g = corpus();
    let asc = full_sorted(&g, false);
    let src = "MATCH (n:Item) WITH n ORDER BY n.createdAt ASC SKIP 9999 LIMIT 500 RETURN n.id AS id";
    let want: Vec<Vec<Value>> = asc.iter().skip(9999).take(500).cloned().collect();
    assert_eq!(general(&g, src), want);
    assert_eq!(rows(&g, src), want);
    // A second key breaks the ties: the page is then unambiguous.
    let all2 = general(&g, "MATCH (n:Item) RETURN n.id AS id ORDER BY n.createdAt DESC, n.id DESC");
    let src2 = "MATCH (n:Item) WITH n ORDER BY n.createdAt DESC, n.id DESC SKIP 12345 LIMIT 777 RETURN n.id AS id";
    let want2: Vec<Vec<Value>> = all2.iter().skip(12345).take(777).cloned().collect();
    assert_eq!(general(&g, src2), want2);
    assert_eq!(rows(&g, src2), want2);
}

/// The late-projecting top-k (a bare node output, its properties deferred
/// to the survivors) pages identically.
#[test]
fn the_late_projecting_topk_pages_identically() {
    let g = corpus();
    let all = full_sorted(&g, true);
    let src = "MATCH (n:Item) RETURN n ORDER BY n.createdAt DESC SKIP 10000 LIMIT 50";
    let want_ids: Vec<Value> = all.iter().skip(10000).take(50).map(|r| r[0].clone()).collect();
    for got in [general(&g, src), rows(&g, src)] {
        let ids: Vec<Value> = got
            .iter()
            .map(|r| match &r[0] {
                Value::Node { props, .. } => props.get("id").cloned().unwrap_or(Value::Null),
                other => other.clone(),
            })
            .collect();
        assert_eq!(ids, want_ids);
    }
}
