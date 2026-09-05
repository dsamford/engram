#![allow(non_snake_case)]
//! Track B M1 gate at the QUERY level: the same graph, queried through the
//! Cypher engine, returns identical results whether its store is RESIDENT or
//! PAGED (segments on disk, faulted in through a cache smaller than the graph).
//!
//! The mechanism: build + populate a resident graph, capture query results,
//! then `into_paged` the shared store and run the SAME queries through a FRESH
//! graph handle (cold caches, so every read genuinely hits the paged store).
//! Results must match byte-for-byte, and the paged path must actually be
//! exercised (`pread`s happened). This exercises the paths the store-level
//! differential does not: label scans, property projection, traversals and
//! aggregates through the columnar/adjacency read layers.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run {src}: {e}"))
        .rows
}

/// A representative spread: point lookup by property id, a filtered label scan
/// with ORDER BY, a one-hop and a two-hop traversal, and an aggregate.
const QUERIES: &[&str] = &[
    "MATCH (n:Person {id: 7}) RETURN n.name, n.age",
    "MATCH (n:Person) WHERE n.age >= 40 RETURN n.id ORDER BY n.id",
    "MATCH (a:Person {id: 1})-[:KNOWS]->(b:Person) RETURN b.id ORDER BY b.id",
    "MATCH (a:Person {id: 1})-[:KNOWS]->()-[:KNOWS]->(c:Person) RETURN DISTINCT c.id ORDER BY c.id",
    "MATCH (n:Person) RETURN count(n) AS c",
    "MATCH (n:Person) WHERE n.age < 25 RETURN count(n) AS young",
    // The COUNT FOLD over the paged CSR (the same `adjacent_slim_for_each`
    // accessor, a memoised KNOWS level): a 2-hop walk count, global and keyed.
    "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) RETURN count(*) AS walks",
    "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) WHERE a.age < 25 RETURN a.id AS id, count(*) AS walks ORDER BY id",
    // The ANTI-JOIN over the paged CSR (`edge_count_slim` with its
    // `sorted_by_peer` flag established on the paged-built table): the
    // 2-hop walks whose ends are NOT directly KNOWS-linked, as a fold (inline)
    // and as a row filter.
    "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) WHERE NOT (a)-[:KNOWS]->(c) RETURN count(*) AS open",
    "MATCH (a:Person {id: 3})-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) WHERE NOT (a)-[:KNOWS]->(c) RETURN c.id ORDER BY c.id",
    // The shape families the production corpus caught AFTER the differentials
    // passed (v109–v111): a subquery / comprehension written from the INNER
    // variable's side (the bound var on the right — the KMWorkItem listing
    // scanned its label per row, and the columnar stages evaluated the
    // EXISTS hook-less), a comprehension beside a carried node after a top-k
    // WITH (the revival pick decoded every candidate in full), a labelled
    // NOT EXISTS anti-join, and a label disjunction in the WHERE. Each runs
    // resident, paged AND with the columnar paths off below.
    "MATCH (b:Person {id: 7}) RETURN [(a:Person)-[:KNOWS]->(b) | a.id] AS fans",
    "MATCH (b:Person) WHERE b.id < 20 AND EXISTS { MATCH (a:Person)-[:KNOWS]->(b) WHERE a.age > 30 } RETURN b.id ORDER BY b.id",
    "MATCH (b:Person) WHERE b.id < 20 RETURN b.id AS id, COUNT { MATCH (a:Person)-[:KNOWS]->(b) } AS fans ORDER BY id",
    "MATCH (b:Person) WHERE b.id < 20 WITH b.id AS id, COUNT { MATCH (a:Person)-[:KNOWS]->(b) WHERE a.age < 40 } AS young WHERE young > 0 RETURN id, young ORDER BY id",
    "MATCH (n:Person) WHERE n.age >= 40 AND NOT EXISTS { MATCH (n)-[:KNOWS]->(:Person) } RETURN count(n) AS lonely",
    "MATCH (n:Person) WHERE n.age >= 30 AND NOT EXISTS { MATCH (n)-[:KNOWS]->(:Person) } AND coalesce(n.age, 0) < 45 WITH n ORDER BY n.age ASC, n.id DESC LIMIT 1 OPTIONAL MATCH (n)-[:KNOWS]->(f:Person) WITH n, size([x IN collect({name: f.name}) WHERE x.name IS NOT NULL]) AS friends RETURN n.id AS id, n.name AS name, friends",
    "MATCH (n:Person) WHERE n.age >= 30 WITH n ORDER BY n.age DESC, n.id LIMIT 3 OPTIONAL MATCH (n)-[:KNOWS]->(f:Person) WITH n, size([x IN collect({name: f.name}) WHERE x.name IS NOT NULL]) AS friends RETURN n.id AS id, n.name AS name, friends ORDER BY id",
    "MATCH (n:Person) WHERE n.age IN [20, 21] AND (n:Person OR n:Robot) RETURN count(n) AS n",
];

fn build_resident() -> (Store, Realm, Namespace) {
    let (realm, ns) = (Realm(1), Namespace(1));
    let g = Graph::new(Store::new(), realm, ns);
    // 400 Persons (id, name, age) — enough to span multiple 16 KiB blocks — and
    // a KNOWS ring plus some chords, so one- and two-hop traversals are non-empty.
    let mut ids = Vec::new();
    for i in 0..400i64 {
        let mut p = BTreeMap::new();
        p.insert("id".to_string(), Value::Int(i));
        p.insert("name".to_string(), Value::Str(format!("person-{i}")));
        p.insert("age".to_string(), Value::Int(18 + (i % 60)));
        ids.push(g.create_node(&["Person".into()], &p).expect("node"));
    }
    let empty = BTreeMap::new();
    for i in 0..400usize {
        g.create_rel(ids[i], "KNOWS", ids[(i + 1) % 400], &empty)
            .expect("knows");
        g.create_rel(ids[i], "KNOWS", ids[(i + 7) % 400], &empty)
            .expect("chord");
    }
    g.shared_store().seal();
    (g.shared_store(), realm, ns)
}

#[test]
fn paged_query_results_equal_resident() {
    let (store, realm, ns) = build_resident();

    // Resident answers.
    let resident = Graph::new(store.clone(), realm, ns);
    let want: Vec<Vec<Vec<Value>>> = QUERIES.iter().map(|q| rows(&resident, q)).collect();
    // Not vacuous: real rows come back.
    assert!(
        want.iter().any(|r| !r.is_empty()),
        "resident produced no rows"
    );
    // THREE-WAY: the same statements with the columnar paths OFF (the
    // general interpreter) must agree too. The columnar recognisers are a
    // performance choice and never a new answer or a new failure — three
    // production shapes reached a hook-less evaluator and ERRORED only with
    // the columnar paths on, which a resident-vs-paged comparison (both on)
    // cannot see.
    resident.set_columnar_scans(false);
    for (i, q) in QUERIES.iter().enumerate() {
        let general = rows(&resident, q);
        assert_eq!(general, want[i], "columnar vs general diverged on: {q}");
    }
    resident.set_columnar_scans(true);

    // Convert the SHARED store to paged with a cache far smaller than the graph.
    let dir = std::env::temp_dir().join("engram_m1_paged_query");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let _cache = store.into_paged(&dir, 8 * 1024).expect("into_paged");

    // A FRESH graph over the paged store — cold caches, so reads hit disk.
    let paged = Graph::new(store.clone(), realm, ns);
    let (got, trace) =
        engram_observe::with_trace(|| QUERIES.iter().map(|q| rows(&paged, q)).collect::<Vec<_>>());

    for (i, q) in QUERIES.iter().enumerate() {
        assert_eq!(got[i], want[i], "paged vs resident diverged on: {q}");
    }
    // The paged read path was genuinely exercised (blocks faulted in from disk).
    assert!(
        trace.counters().get("paged.pread").copied().unwrap_or(0) > 0,
        "no paged pread happened — the query did not actually hit the paged store"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn persisted_index_is_loaded_at_open_and_served_without_rebuild() {
    // INDEX-AT-SEAL: an index persisted next to the segments (`idx-<token>.idx`)
    // is loaded when the store is reopened and served to the first query WITHOUT
    // a rebuild — while remaining byte-identical to the from-scratch answer.
    let (store, realm, ns) = build_resident();

    // Resident answers for two property-seek queries (the shapes that drive the
    // range index): an equality point lookup and an equality filter with order.
    let resident = Graph::new(store.clone(), realm, ns);
    let seek_id = "MATCH (a:Person {id: 7})-[:KNOWS]->(b:Person) RETURN b.id ORDER BY b.id";
    let seek_age =
        "MATCH (a:Person {age: 30})-[:KNOWS]->(b:Person) RETURN b.id ORDER BY b.id LIMIT 5";
    let want_id = rows(&resident, seek_id);
    let want_age = rows(&resident, seek_age);
    assert!(!want_id.is_empty() && !want_age.is_empty(), "seeks vacuous");
    drop(resident);

    let dir = std::env::temp_dir().join("engram_index_at_seal");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");

    // Page the store to disk, then persist the two seek indexes beside it.
    let _ = store.into_paged(&dir, 1024 * 1024).expect("into_paged");
    let g = Graph::new(store.clone(), realm, ns);
    let written = g
        .persist_indexes(&dir, &["id", "age"])
        .expect("persist_indexes");
    assert_eq!(written, 2, "both minted properties should persist");
    // Sidecars are actually on disk.
    for token_file in ["id", "age"] {
        let any = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("idx-"));
        assert!(any, "no idx-*.idx sidecar written (checking {token_file})");
    }
    drop(g);
    drop(store);

    // Reopen from disk. The load itself must pick up the persisted indexes.
    let ((reopened, _cache), open_trace) = engram_observe::with_trace(|| {
        engram_store::Store::open_paged_dir(&dir, 8 * 1024).expect("open_paged_dir")
    });
    assert!(
        open_trace
            .counters()
            .get("store.loaded persisted indexes")
            .copied()
            .unwrap_or(0)
            > 0,
        "reopen did not load the persisted index sidecars"
    );

    // The first queries must be served from the loaded index — no rebuild — and
    // return the same rows as resident.
    let g2 = Graph::new(reopened, realm, ns);
    let (got, trace) = engram_observe::with_trace(|| (rows(&g2, seek_id), rows(&g2, seek_age)));
    assert_eq!(got.0, want_id, "id seek diverged over persisted index");
    assert_eq!(got.1, want_age, "age seek diverged over persisted index");
    let served = trace
        .counters()
        .get("graph.range index served from disk")
        .copied()
        .unwrap_or(0);
    let built = trace
        .counters()
        .get("graph.range index builds")
        .copied()
        .unwrap_or(0);
    assert!(
        served >= 1,
        "no index was served from disk (served={served})"
    );
    assert_eq!(built, 0, "an index was rebuilt despite a valid sidecar");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn persisted_index_is_discarded_after_a_write_advances_the_clock() {
    // SAFETY: a persisted index is stamped with the clock it was built at. Once a
    // write advances `now_ts` past that vintage, the on-disk index predates rows
    // it would have to cover, so it MUST be discarded and rebuilt — never served
    // stale. The result stays correct either way; this pins the rebuild.
    let (store, realm, ns) = build_resident();
    let dir = std::env::temp_dir().join("engram_index_at_seal_stale");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let _ = store.into_paged(&dir, 1024 * 1024).expect("into_paged");
    let g = Graph::new(store.clone(), realm, ns);
    assert_eq!(g.persist_indexes(&dir, &["id"]).expect("persist"), 1);
    drop(g);
    drop(store);

    let (reopened, _cache) =
        engram_store::Store::open_paged_dir(&dir, 8 * 1024).expect("open_paged_dir");
    let g = Graph::new(reopened, realm, ns);

    // A write advances the commit clock past the sidecar's vintage.
    let mut p = BTreeMap::new();
    p.insert("id".to_string(), Value::Int(10_000));
    p.insert("name".to_string(), Value::Str("newcomer".into()));
    p.insert("age".to_string(), Value::Int(7));
    let newcomer = g.create_node(&["Person".into()], &p).expect("write");
    let empty = BTreeMap::new();
    // Give the new node an out-edge so the anchored traversal is well-formed.
    g.create_rel(newcomer, "KNOWS", newcomer, &empty).ok();

    // The same anchored seek must now REBUILD (stale sidecar rejected), and the
    // newly-written node must be seekable (proving the rebuild covers it).
    let q = "MATCH (a:Person {id: 7})-[:KNOWS]->(b:Person) RETURN b.id ORDER BY b.id";
    let (out, trace) = engram_observe::with_trace(|| rows(&g, q));
    assert!(!out.is_empty(), "seek vacuous after write");
    assert_eq!(
        trace
            .counters()
            .get("graph.range index served from disk")
            .copied()
            .unwrap_or(0),
        0,
        "a stale sidecar was served after the clock advanced"
    );
    assert!(
        trace
            .counters()
            .get("graph.range index builds")
            .copied()
            .unwrap_or(0)
            >= 1,
        "the index was not rebuilt after the sidecar went stale"
    );
    // The rebuilt index sees the newcomer.
    let seen = rows(
        &g,
        "MATCH (a:Person {id: 10000})-[:KNOWS]->(b:Person) RETURN b.id",
    );
    assert!(
        !seen.is_empty(),
        "rebuilt index did not cover the new write"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_paged_dir_graph_has_correct_counts_and_no_cross_product() {
    // REGRESSION: a graph over `Store::open_paged_dir` has an EMPTY log (data is
    // in sealed segments, never replayed), so the count store must still be
    // treated as pre-populated and rebuilt — else label counts read 0,
    // property-seek declines, and an anchored top-k chain fans out to a
    // cross-product (the M3 durable-open bug the pod proof surfaced).
    let (store, realm, ns) = build_resident();
    let dir = std::env::temp_dir().join("engram_open_paged_graph_regression");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    // Persist to disk, then drop everything resident.
    let _ = store
        .into_paged(&dir, 1024 * 1024)
        .expect("into_paged writes files");
    drop(store);

    // Fresh graph over a store OPENED FROM DISK (empty log).
    let (paged_store, _cache) =
        engram_store::Store::open_paged_dir(&dir, 8 * 1024).expect("open_paged_dir");
    let g = Graph::new(paged_store, realm, ns);

    // Label counts must be correct (the crux — this read 0 before the fix).
    assert_eq!(
        g.count_label_nodes("Person"),
        400,
        "label count wrong over open_paged_dir"
    );

    // An anchored top-k chain must NOT cross-product: person 1's KNOWS neighbours
    // ordered — a bounded result, not 400×400.
    let q = parse_statement(
        "MATCH (a:Person {id: 1})-[:KNOWS]->(b:Person) RETURN b.id ORDER BY b.id DESC LIMIT 5",
    )
    .expect("parse");
    let rows = run_query(&g, &q, BTreeMap::new())
        .expect("run must not blow the row budget")
        .rows;
    assert!(
        rows.len() <= 5 && !rows.is_empty(),
        "expected a small bounded result, got {}",
        rows.len()
    );

    let _ = std::fs::remove_dir_all(&dir);
}
