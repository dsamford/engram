#![allow(non_snake_case)]
//! Fix 54: a persisted index-at-seal covers the WHOLE partition, and a
//! LABEL-SCOPED slot that took it as-is answered every scoped probe with
//! every label's rows. On the read-only mirror the clock never moves, so
//! every scoped slot took the partition's index: `{status: "pending"}` over
//! a 701-node label walked the partition's thousands of `pending` entries
//! per probe — 4.8 ms, where a value nobody carries cost 0.4. The scoped
//! slot now takes the persisted index RESTRICTED to the label's members
//! (one membership test per entry, no store read, once per slot); the
//! unscoped slot still takes it whole.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const RESTRICTED: &str = "graph.range index served from disk, restricted to the label";
const WHOLE: &str = "graph.range index served from disk";
const BUILT: &str = "graph.range index builds";
const HIT: &str = "graph.range index cache hit";

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// 2,000 `Big` nodes, every one `pending`, and 10 `Small` nodes of which
/// four are — the partition-wide `status` index holds 2,004 `pending`
/// entries, the label's holds four.
fn corpus() -> (Store, Vec<u64>, Realm, Namespace) {
    let (realm, ns) = (Realm(1), Namespace(1));
    let g = Graph::new(Store::new(), realm, ns);
    for i in 0..2_000i64 {
        let mut m = BTreeMap::new();
        m.insert("n".to_string(), Value::Int(i));
        m.insert("status".to_string(), Value::Str("pending".to_string()));
        g.create_node(&["Big".into()], &m).expect("big");
    }
    let mut small_pending = Vec::new();
    for i in 0..10i64 {
        let mut m = BTreeMap::new();
        m.insert("n".to_string(), Value::Int(i));
        let status = if i % 3 == 0 { "pending" } else { "done" };
        m.insert("status".to_string(), Value::Str(status.to_string()));
        let id = g.create_node(&["Small".into()], &m).expect("small");
        if status == "pending" {
            small_pending.push(id);
        }
    }
    g.shared_store().seal();
    (g.shared_store(), small_pending, realm, ns)
}

#[test]
fn a_scoped_slot_takes_the_persisted_index_restricted_to_its_label() {
    let (store, small_pending, realm, ns) = corpus();
    assert_eq!(small_pending.len(), 4, "0, 3, 6, 9");
    let dir = std::env::temp_dir().join("engram_scoped_slot_persisted");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let _ = store.into_paged(&dir, 1024 * 1024).expect("into_paged");
    let g = Graph::new(store.clone(), realm, ns);
    assert_eq!(g.persist_indexes(&dir, &["status"]).expect("persist"), 1);
    drop(g);
    drop(store);

    let (reopened, _cache) = Store::open_paged_dir(&dir, 8 * 1024).expect("open_paged_dir");
    let g = Graph::new(reopened, realm, ns);
    let pending = Value::Str("pending".to_string());

    // The SCOPED probe: the persisted index, restricted to `Small` — the
    // label's four ids and nothing of `Big`'s two thousand.
    let (ids, tr) = engram_observe::with_trace(|| {
        g.index_probe_eq_scoped("status", &pending, None, Some("Small"))
            .expect("probe")
            .expect("servable")
    });
    let c = tr.counters();
    assert_eq!(ids, small_pending, "the scoped probe answers the label's ids alone");
    assert_eq!(count_of(c,RESTRICTED), 1, "{c:?}");
    assert_eq!(count_of(c,"index.restricted to a label"), 1, "{c:?}");
    assert_eq!(count_of(c,BUILT), 0, "no rebuild: {c:?}");
    assert_eq!(count_of(c,WHOLE), 0, "the scoped slot did not take the whole index: {c:?}");

    // The second scoped probe is a cache hit on the restricted index.
    let (again, tr) = engram_observe::with_trace(|| {
        g.index_probe_eq_scoped("status", &pending, None, Some("Small"))
            .expect("probe")
            .expect("servable")
    });
    let c = tr.counters();
    assert_eq!(again, small_pending);
    assert_eq!(count_of(c,HIT), 1, "{c:?}");
    assert_eq!(count_of(c,RESTRICTED), 0, "{c:?}");

    // The UNSCOPED probe still takes the persisted index whole.
    let (all, tr) = engram_observe::with_trace(|| {
        g.index_probe_eq("status", &pending, None)
            .expect("probe")
            .expect("servable")
    });
    let c = tr.counters();
    assert_eq!(all.len(), 2_004);
    assert_eq!(count_of(c,WHOLE), 1, "{c:?}");
    assert_eq!(count_of(c,RESTRICTED), 0, "{c:?}");

    // Through the engine, both labels answer their own counts.
    assert_eq!(
        rows(&g, "MATCH (s:Small {status: 'pending'}) RETURN count(s) AS n"),
        vec![vec![Value::Int(4)]]
    );
    assert_eq!(
        rows(&g, "MATCH (b:Big {status: 'pending'}) RETURN count(b) AS n"),
        vec![vec![Value::Int(2_000)]]
    );
    assert_eq!(
        rows(&g, "MATCH (s:Small {status: 'done'}) RETURN s.n AS n ORDER BY n"),
        [1i64, 2, 4, 5, 7, 8]
            .iter()
            .map(|i| vec![Value::Int(*i)])
            .collect::<Vec<_>>()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
