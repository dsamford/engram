#![allow(non_snake_case)]
//! Fix 61: a property-demanded seed over a large label (fix 56's lean
//! top-k carry, fix 41's lean starts) must never read the whole PARTITION.
//! The retired "candidate batch" path scanned every property column and
//! the label-set column over the entire store, filtered to the label's
//! members — admitted by a share gate that let the 15k-member KMWorkItem
//! label through on the paged mirror, so the visibility listing walked the
//! 5M-record store six times per run (96 s against Neo4j's 112 ms, +9 GB).
//! The seed now binds lean from the label's OWN columns, kept in the
//! property-column cache for the next statement.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("viewerId".to_string(), Value::Str("viewer-1".into()));
    p.insert("assigneeId".to_string(), Value::Str("viewer-1".into()));
    p.insert("limit".to_string(), Value::Int(50));
    p
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src));
    (r, trace.counters().clone())
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

/// 5,000 work items beside 40,000 filler nodes of other labels — the
/// label is a minority of the partition, as every production label is.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let body: String = "b".repeat(512);
    for i in 0..40_000i64 {
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), Value::Int(i));
        m.insert("content".to_string(), Value::Str(body.clone()));
        let label = if i % 2 == 0 { "Filler" } else { "Email" };
        g.create_node(&[label.into()], &m).expect("filler");
        if i % 8 == 0 {
            let mut w = BTreeMap::new();
            let n = i / 8;
            w.insert("id".to_string(), Value::Str(format!("wi-{n:05}")));
            w.insert("status".to_string(), Value::Str(if n % 7 == 0 { "done".into() } else { "open".into() }));
            w.insert("updatedAt".to_string(), Value::Str(format!("2026-08-{:02}T{:02}:00:00Z", 1 + (n % 28), n % 24)));
            w.insert("assigneeId".to_string(), Value::Str(if n % 5 == 0 { "viewer-1".into() } else { format!("u{}", n % 5) }));
            w.insert("userId".to_string(), Value::Str(format!("u{}", n % 9)));
            w.insert("content".to_string(), Value::Str(body.clone()));
            g.create_node(&["KMWorkItem".into()], &w).expect("item");
        }
    }
    g
}

const LISTING: &str = "MATCH (w:KMWorkItem) \
    WHERE (w.userId = $viewerId OR w.assigneeId = $assigneeId) AND NOT w.status IN ['done', 'cancelled'] \
    RETURN properties(w) AS w ORDER BY w.updatedAt DESC LIMIT toInteger($limit)";

#[test]
fn a_property_demanded_seed_binds_from_the_labels_columns() {
    let g = corpus();
    g.set_late_projection(false);
    let want = rows(&g, LISTING);
    g.set_late_projection(true);
    assert_eq!(want.len(), 50);
    let (got, c) = traced(&g, LISTING);
    assert_eq!(got, want);
    assert_eq!(
        count_of(&c, "interp.stage bound a whole-node output lean for the top-k"),
        1,
        "fix 56 binds the carry lean: {c:?}"
    );
    assert!(
        count_of(&c, "interp.seed starts bound from the label column") > 0,
        "the lean seed reads the label's columns: {c:?}"
    );
    // Only the survivors are decoded in full; nothing reads the 40,000
    // filler records (a point read per member at most).
    assert_eq!(count_of(&c, "graph.nodes materialised in full"), 50, "{c:?}");
    assert!(count_of(&c, "store.gets") <= 5_100, "reads bounded by the label: {c:?}");
    // The columns are KEPT: the second run reads no record before the top-k.
    let (again, c) = traced(&g, LISTING);
    assert_eq!(again, want);
    assert!(
        count_of(&c, "interp.columnar column read served from the property-column cache") > 0,
        "{c:?}"
    );
    assert!(count_of(&c, "store.gets") <= 60, "second run: the survivors alone: {c:?}");
}
