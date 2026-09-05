#![allow(non_snake_case)]
//! Fix 28 (v104): the pipeline's CORE projection path — `MATCH (t:RT
//! {userId: $u})-[:PGW]->(p:GWP {status: 'pending'}) RETURN p.id … ORDER BY
//! … LIMIT 25`, the GWP corpus shape — seeded from the whole label, expanded
//! EVERY seed and only then applied its WHERE, the seed's own anchor
//! included, one record read per row (517 adjacency lookups + 729 record
//! reads per statement on the mirror: 6.7 ms against Neo4j's 2.8, while the
//! aggregate path over the same chain filtered its seed from the cached
//! column). The core path now builds its chunk exactly as the aggregate path
//! does (seed prefiltered over the column cache, each predicate at its
//! earliest position), and a predicate over a hop END var with known labels
//! is answered by the strict column filter — a whole-label walk that is KEPT
//! when the population is a fair share of the label, the population alone
//! otherwise — instead of a record read per distinct end.
//!
//! Every answer is checked against the general path's (columnar paths off),
//! on the memory store and on a PAGED store with an interleaved layout — the
//! mirror's, where a whole-label column read declines to a gather.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("u".to_string(), Value::Str("u1".to_string()));
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

fn general(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let r = rows(g, src);
    g.set_columnar_scans(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const HOP_RUNS: &str = "interp.pipeline hop runs";
const SEED_FILTERED: &str = "interp.pipeline seed predicates filtered by columns";
const END_FILTERED: &str = "interp.pipeline bound-var predicate filtered by columns";
const SERVED: &str = "interp.columnar column read served from the property-column cache";
const GATHER: &str = "graph.column point-gather";
const KEPT: &str = "graph.property column kept";

/// 1,200 `:RT {userId, id}` (every third `u1`), each with one or two
/// `-[:PGW]->(:GWP {id, status, createdAt})`, beside 2,000 `:Other {userId:
/// 'u1'}` so the partition-wide `userId` probe answers more than the label
/// holds. `interleaved` puts a `:Filler` after every node so both labels are
/// sparse in the id space (the mirror's layout).
fn build(interleaved: bool) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let filler = |g: &Graph, i: i64| {
        if interleaved {
            let mut f = BTreeMap::new();
            f.insert("other".to_string(), Value::Str(format!("filler-{i}-{}", "x".repeat(40))));
            g.create_node(&["Filler".into()], &f).expect("filler");
        }
    };
    for i in 0..2000i64 {
        let mut m = BTreeMap::new();
        m.insert("userId".to_string(), Value::Str("u1".into()));
        g.create_node(&["Other".into()], &m).expect("other");
        filler(&g, i);
    }
    for i in 0..1200i64 {
        let mut m = BTreeMap::new();
        m.insert(
            "userId".to_string(),
            Value::Str(if i % 3 == 0 { "u1".into() } else { format!("u{}", 2 + i % 5) }),
        );
        m.insert("id".to_string(), Value::Str(format!("rt-{i:05}")));
        let t = g.create_node(&["RT".into()], &m).expect("rt");
        filler(&g, 10_000 + i);
        for k in 0..(1 + i % 2) {
            let mut pm = BTreeMap::new();
            pm.insert("id".to_string(), Value::Str(format!("gwp-{i:05}-{k}")));
            pm.insert(
                "status".to_string(),
                Value::Str(if (i + k) % 4 == 0 { "pending".into() } else { "done".into() }),
            );
            pm.insert("createdAt".to_string(), Value::Int(5_000_000 - i * 3 - k));
            let p = g.create_node(&["GWP".into()], &pm).expect("gwp");
            g.create_rel(t, "PGW", p, &BTreeMap::new()).expect("pgw");
            filler(&g, 20_000 + i * 2 + k);
        }
    }
    g
}

/// One directory per call: two tests of one process share a pid, and a
/// shared directory races the other test's `remove_dir_all`.
fn paged(g: Graph) -> (Graph, std::path::PathBuf) {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let store = g.shared_store();
    drop(g);
    let dir = std::env::temp_dir().join(format!(
        "engram_pipeline_core_column_filter_{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let _cache = store.into_paged(&dir, 64 * 1024).expect("into_paged");
    (Graph::new(store.clone(), Realm(1), Namespace(1)), dir)
}

const CORPUS: &str = "MATCH (t:RT {userId: $u})-[:PGW]->(p:GWP {status: 'pending'}) \
    RETURN p.id AS id, p.status AS status, t.id AS task ORDER BY p.createdAt DESC LIMIT 25";

const SHAPES: [&str; 4] = [
    CORPUS,
    // The anchor as a WHERE, the end map as a WHERE.
    "MATCH (t:RT)-[:PGW]->(p:GWP) WHERE t.userId = $u AND p.status = 'pending' \
     RETURN p.id AS id ORDER BY p.createdAt DESC LIMIT 25",
    // No ORDER BY: the plain projection tail.
    "MATCH (t:RT {userId: $u})-[:PGW]->(p:GWP {status: 'pending'}) RETURN p.id AS id, t.id AS task",
    // An end predicate the column filter cannot vectorise the same way still agrees.
    "MATCH (t:RT {userId: $u})-[:PGW]->(p:GWP) WHERE p.status IN ['pending', 'done'] AND p.createdAt > 4998000 \
     RETURN p.id AS id ORDER BY p.id LIMIT 10",
];

#[test]
fn the_core_path_filters_its_seed_and_its_hop_end_from_columns_on_the_memory_store() {
    let g = build(false);
    for src in SHAPES {
        let want = general(&g, src);
        assert!(!want.is_empty(), "fixture: `{src}`");
        let (got, first) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert!(count_of(&first, HOP_RUNS) > 0, "`{src}` runs on the core path: {first:?}");
        assert!(count_of(&first, SEED_FILTERED) > 0, "`{src}` seed filtered: {first:?}");
        assert!(count_of(&first, END_FILTERED) > 0, "`{src}` end filtered: {first:?}");
        // The whole-label walk over the END label is kept (the seed's own
        // key is a seek, walked over its sought ids): the second statement
        // is served from the cache and gathers no column for its filters or
        // its ORDER BY key.
        let (again, second) = traced(&g, src);
        assert_eq!(again, want, "`{src}` (second run)");
        assert!(count_of(&second, SERVED) >= 1, "`{src}` served from the cache: {second:?}");
        assert!(count_of(&second, END_FILTERED) > 0, "`{src}`: {second:?}");
        assert_eq!(count_of(&second, GATHER), 0, "`{src}` gathers nothing: {second:?}");
    }
}

#[test]
fn on_a_paged_interleaved_store_the_second_statement_reads_no_record_for_its_filters() {
    let (g, dir) = paged(build(true));
    let want = general(&g, CORPUS);
    assert_eq!(want.len(), 25);
    let (got, first) = traced(&g, CORPUS);
    assert_eq!(got, want);
    assert!(count_of(&first, END_FILTERED) > 0, "{first:?}");
    assert!(count_of(&first, KEPT) >= 1, "the gathered whole-label column is kept: {first:?}");
    let (again, second) = traced(&g, CORPUS);
    assert_eq!(again, want);
    assert!(count_of(&second, SERVED) >= 2, "{second:?}");
    assert_eq!(count_of(&second, GATHER), 0, "no per-statement gather for the filters: {second:?}");
    let _ = std::fs::remove_dir_all(dir);
}

/// CONTROL: a two-var predicate, an edge predicate and a relationship
/// variable's property keep their operators — and agree.
#[test]
fn predicates_outside_the_single_node_var_class_still_agree() {
    let g = build(false);
    for src in [
        "MATCH (t:RT {userId: $u})-[:PGW]->(p:GWP) WHERE t <> p RETURN count(p) AS n",
        "MATCH (t:RT {userId: $u})-[e:PGW]->(p:GWP) WHERE e.weight IS NULL RETURN p.id AS id ORDER BY id LIMIT 5",
        "MATCH (t:RT {userId: $u})-[:PGW]->(p:GWP) WHERE NOT (p)-[:PGW]->(t) RETURN p.id AS id ORDER BY id LIMIT 5",
    ] {
        let want = general(&g, src);
        let (got, _) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
    }
}
