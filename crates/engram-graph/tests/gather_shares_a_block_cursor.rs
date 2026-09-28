#![allow(non_snake_case)]
//! Fix 100: a point-gather over a sorted id set on a PAGED store reads each
//! block once — the run of reads shares a block cursor per segment and reads
//! the property off the block's bytes — where every id used to cost a cache
//! touch (a shard lock, a map probe, an `Arc` clone) and a record copy: the
//! MENTIONS aggregate's 37,270-id gather on the mirror paid ~7 µs a get.
//!
//! The rows are pinned against the labelled spelling (which reads through the
//! label's column, never the gather) on a paged store whose ends carry two
//! labels, so no covering label is discovered and the gather runs.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params(user: &str) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("userId".to_string(), Value::Str(user.into()));
    p
}

fn rows(g: &Graph, src: &str, p: &BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, p.clone())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(
    g: &Graph,
    src: &str,
    p: &BTreeMap<String, Value>,
) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src, p));
    (r, trace.counters().clone())
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const REUSED: &str = "paged.block reused by the gather cursor";
const GATHER: &str = "graph.column point-gather";
const DISCOVERED: &str = "interp.pipeline unlabelled var's label discovered from its members";

/// 2,000 entities and 2,000 topics interleaved in the id space (each with
/// a `name` and most with a `type`), twenty fillers after each so the
/// mentioned ids are sparse in the span; one user mentions 1,200 entities
/// and 300 topics `(k % 4) + 1` times. Spilled to a paged store.
fn corpus() -> (Graph, std::path::PathBuf) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut ends = Vec::new();
    for k in 0..4000i64 {
        let mut m = BTreeMap::new();
        let label = if k % 2 == 0 { "Entity" } else { "Topic" };
        m.insert("name".to_string(), Value::Str(format!("{label}-{k:05}")));
        if k % 7 != 0 {
            m.insert(
                "type".to_string(),
                Value::Str(["person", "org", "place"][(k % 3) as usize].into()),
            );
        }
        ends.push(g.create_node(&[label.into()], &m).expect("end"));
        // Twenty fillers after every end: the mentioned ids are SPARSE in
        // the id span (as the mirror's are), so the range scan over the span
        // declines on rows visited and the point-gather runs.
        for _ in 0..20 {
            let mut f = BTreeMap::new();
            f.insert("pad".to_string(), Value::Str("f".repeat(120)));
            g.create_node(&["Filler".into()], &f).expect("filler");
        }
    }
    let mut m = BTreeMap::new();
    m.insert("userId".to_string(), Value::Str("user-0".into()));
    let u = g.create_node(&["UserDataNode".into()], &m).expect("user");
    for k in 0..1500i64 {
        // Entities at even ids, topics at odd: 1,200 entities, 300 topics.
        let end = if k < 1200 {
            ends[(k * 2) as usize]
        } else {
            ends[((k - 1200) * 2 + 1) as usize]
        };
        for _ in 0..(k % 4) + 1 {
            g.create_rel(u, "MENTIONS", end, &BTreeMap::new())
                .expect("m");
        }
    }
    let store = g.shared_store();
    drop(g);
    let dir = std::env::temp_dir().join(format!("engram_gather_cursor_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let _cache = store
        .into_paged(&dir, 64 * 1024 * 1024)
        .expect("into_paged");
    (Graph::new(store, Realm(1), Namespace(1)), dir)
}

const STMT: &str = "MATCH (n:UserDataNode {userId: $userId})-[:MENTIONS]->(e) \
    RETURN e.name AS name, coalesce(e.type, 'unknown') AS type, count(*) AS cnt \
    ORDER BY cnt DESC, name LIMIT 40";

/// The mixed-label ends gather by point reads that share a block cursor;
/// the rows are the two labelled spellings' union.
#[test]
fn a_the_mixed_ends_gather_through_a_shared_block_cursor() {
    let (g, dir) = corpus();
    let (got, c) = traced(&g, STMT, &params("user-0"));
    assert_eq!(got.len(), 40);
    assert_eq!(
        count_of(&c, DISCOVERED),
        0,
        "two labels: no covering label: {c:?}"
    );
    assert!(count_of(&c, GATHER) >= 1, "{c:?}");
    assert!(count_of(&c, REUSED) >= 500, "{c:?}");
    // The expectation: the fours by name — entities k % 4 == 3 (k < 1200)
    // and topics (k - 1200) % 4 == 3 — sorted by name, the first forty.
    let mut want: Vec<Vec<Value>> = Vec::new();
    for k in 0..1500i64 {
        if k % 4 != 3 {
            continue;
        }
        let id = if k < 1200 { k * 2 } else { (k - 1200) * 2 + 1 };
        let label = if id % 2 == 0 { "Entity" } else { "Topic" };
        let t = if id % 7 != 0 {
            ["person", "org", "place"][(id % 3) as usize]
        } else {
            "unknown"
        };
        want.push(vec![
            Value::Str(format!("{label}-{id:05}")),
            Value::Str(t.into()),
            Value::Int(4),
        ]);
    }
    want.sort_by(|a, b| match (&a[0], &b[0]) {
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        _ => unreachable!(),
    });
    want.truncate(40);
    assert_eq!(got, want);
    let _ = std::fs::remove_dir_all(dir);
}
