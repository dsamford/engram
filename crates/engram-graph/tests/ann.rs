#![allow(non_snake_case)]
//! The ANN arm — determinism, recall against the exact arm, the planner,
//! and the no-stale-answers rule.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any};
use engram_graph::{Graph, VectorArm, hnsw::Hnsw, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn splitmix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn seeded_vec(seed: u64, dim: usize) -> Vec<f64> {
    (0..dim)
        .map(|i| {
            let r = splitmix(seed.wrapping_mul(31).wrapping_add(i as u64));
            ((r >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
        })
        .collect()
}

/// A graph with `n` :V nodes carrying seeded `dim`-d vectors, plus the index.
fn vector_graph(n: usize, dim: usize) -> Graph {
    let g = graph();
    run_stmt(
        &g,
        &parse_any("CREATE VECTOR INDEX vi FOR (v:V) ON (v.e)").expect("parses"),
        BTreeMap::new(),
    )
    .expect("index");
    for i in 0..n {
        let mut props = BTreeMap::new();
        props.insert("i".to_string(), Value::Int(i as i64));
        props.insert(
            "e".to_string(),
            Value::List(
                seeded_vec(i as u64, dim)
                    .into_iter()
                    .map(Value::Float)
                    .collect(),
            ),
        );
        g.create_node(&["V".to_string()], &props).expect("node");
    }
    g
}

fn ids_of(hits: &[(Value, f64)]) -> Vec<i64> {
    hits.iter()
        .map(|(n, _)| match n {
            Value::Node { props, .. } => match props.get("i") {
                Some(Value::Int(i)) => *i,
                other => panic!("bad i: {other:?}"),
            },
            other => panic!("not a node: {other:?}"),
        })
        .collect()
}

#[test]
fn the_hnsw_is_DETERMINISTIC() {
    let build = || {
        let mut h = Hnsw::new(8, 42);
        for i in 0..200u64 {
            let v: Vec<f32> = seeded_vec(i, 8).into_iter().map(|x| x as f32).collect();
            h.insert(i, &v);
        }
        let q: Vec<f32> = seeded_vec(777, 8).into_iter().map(|x| x as f32).collect();
        h.search(&q, 10)
    };
    assert_eq!(
        build(),
        build(),
        "two builds over one content answer identically"
    );
}

// CANARY LIMIT, measured: the recall floor detects a collapsed search beam
// (ef=1 fails it) but NOT inverted neighbour pruning — at 500 and at 2,500
// random vectors the degraded graph still clears 0.95, because random
// cosine neighbourhoods overlap too much to starve the beam. Pruning
// quality is therefore guarded only at REAL scale, by the bench harness's
// recall measurement, and this note is the record of that boundary.
#[test]
fn ann_recall_at_10_measured_against_the_exact_arm() {
    let g = vector_graph(500, 16);
    let queries = 20;
    let mut hit = 0usize;
    let mut total = 0usize;
    for qi in 0..queries {
        let q = seeded_vec(10_000 + qi, 16);
        g.set_vector_exact_max(100_000);
        let (exact, plan) = g.vector_query("vi", 10, &q).expect("exact");
        assert_eq!(plan.arm, VectorArm::Exact);
        g.set_vector_exact_max(10);
        let (ann, plan) = g.vector_query("vi", 10, &q).expect("ann");
        assert!(matches!(plan.arm, VectorArm::Ann { .. }));
        let want = ids_of(&exact);
        let got = ids_of(&ann);
        total += want.len();
        hit += want.iter().filter(|w| got.contains(w)).count();
    }
    let recall = hit as f64 / total as f64;
    assert!(
        recall >= 0.95,
        "recall@10 over {queries} queries was {recall:.3}"
    );
}

#[test]
fn the_planner_reports_its_arm_and_the_cache_its_builds() {
    let g = vector_graph(50, 8);
    let q = seeded_vec(9_999, 8);
    g.set_vector_exact_max(100);
    let (_, plan) = g.vector_query("vi", 5, &q).expect("query");
    assert_eq!(
        plan.arm,
        VectorArm::Exact,
        "at or below the crossover: exact"
    );
    assert_eq!(plan.eligible, 50);

    g.set_vector_exact_max(10);
    let (_, plan) = g.vector_query("vi", 5, &q).expect("query");
    assert_eq!(
        plan.arm,
        VectorArm::Ann { rebuilt: true },
        "first ANN use builds"
    );
    let (_, plan) = g.vector_query("vi", 5, &q).expect("query");
    assert_eq!(
        plan.arm,
        VectorArm::Ann { rebuilt: false },
        "the second reuses the cache"
    );
}

#[test]
fn a_write_between_queries_REBUILDS_and_the_new_row_is_findable() {
    // The no-stale-answers rule: an approximate index must never answer from
    // a world that no longer exists. The sharpest probe: insert an EXACT
    // duplicate of the query vector after the first build — it must come
    // back FIRST on the next query.
    let g = vector_graph(60, 8);
    g.set_vector_exact_max(10);
    let q = seeded_vec(123_456, 8);
    let (_, plan) = g.vector_query("vi", 5, &q).expect("build");
    assert!(matches!(plan.arm, VectorArm::Ann { rebuilt: true }));

    let mut props = BTreeMap::new();
    props.insert("i".to_string(), Value::Int(9_999));
    props.insert(
        "e".to_string(),
        Value::List(q.iter().copied().map(Value::Float).collect()),
    );
    g.create_node(&["V".to_string()], &props)
        .expect("the late arrival");

    let (hits, plan) = engram_observe::with_trace(|| g.vector_query("vi", 5, &q).expect("query"));
    assert_eq!(
        hits.1.arm,
        VectorArm::Ann { rebuilt: false },
        "the write was folded in INCREMENTALLY, not rebuilt"
    );
    assert!(
        plan.counters()
            .get("graph.vector index incrementally maintained")
            .is_some(),
        "the incremental path ran"
    );
    let hits = hits.0;
    assert_eq!(
        ids_of(&hits)[0],
        9_999,
        "the perfect match is not invisible"
    );
    let (_, score) = &hits[0];
    assert!(
        (score - 1.0).abs() < 1e-12,
        "and its score is the exact 1.0, not an f32 echo"
    );
}

#[test]
fn scores_are_IDENTICAL_across_arms() {
    // The ranking must not reveal the arm by its precision: ANN candidates
    // are rescored in f64, so a node's score is one number everywhere.
    let g = vector_graph(300, 12);
    let q = seeded_vec(31_337, 12);
    g.set_vector_exact_max(100_000);
    let (exact, _) = g.vector_query("vi", 10, &q).expect("exact");
    g.set_vector_exact_max(10);
    let (ann_cold, _) = g.vector_query("vi", 10, &q).expect("ann cold");
    // The SECOND query runs the warm path (cache hit, no gather) — both
    // paths rescore, and both must match the exact arm bit for bit.
    let (ann_warm, plan) = g.vector_query("vi", 10, &q).expect("ann warm");
    assert_eq!(
        plan.arm,
        VectorArm::Ann { rebuilt: false },
        "the second query is warm"
    );
    let exact_scores: BTreeMap<i64, f64> = ids_of(&exact)
        .into_iter()
        .zip(exact.iter().map(|(_, s)| *s))
        .collect();
    for ann in [&ann_cold, &ann_warm] {
        for (id, s) in ids_of(ann).into_iter().zip(ann.iter().map(|(_, s)| *s)) {
            if let Some(es) = exact_scores.get(&id) {
                assert_eq!(s, *es, "node {id} scored differently across arms");
            }
        }
    }
}

#[test]
fn a_different_query_dimension_rebuilds_for_that_dimension() {
    // Mixed-dim rows under one label: each query dimension sees ITS eligible
    // set; the cache keys on dimension so neither poisons the other.
    let g = vector_graph(40, 8);
    for i in 0..40 {
        let mut props = BTreeMap::new();
        props.insert("i".to_string(), Value::Int(1_000 + i as i64));
        props.insert(
            "e".to_string(),
            Value::List(
                seeded_vec(500 + i as u64, 4)
                    .into_iter()
                    .map(Value::Float)
                    .collect(),
            ),
        );
        g.create_node(&["V".to_string()], &props).expect("node");
    }
    g.set_vector_exact_max(10);
    let (_, plan8) = g.vector_query("vi", 5, &seeded_vec(1, 8)).expect("8d");
    assert_eq!(plan8.eligible, 40);
    assert_eq!(plan8.skipped, 40, "the 4-d rows are a different space");
    let (hits4, plan4) = g.vector_query("vi", 5, &seeded_vec(2, 4)).expect("4d");
    assert_eq!(plan4.eligible, 40);
    assert!(
        matches!(plan4.arm, VectorArm::Ann { rebuilt: true }),
        "rebuilt for the new dim"
    );
    assert!(
        ids_of(&hits4).iter().all(|i| *i >= 1_000),
        "only the 4-d rows answer a 4-d query"
    );
}

#[test]
fn the_exact_arm_caches_its_gather_and_a_write_invalidates() {
    // The cutover harness's seed cost: a sub-crossover index re-scanned the
    // nodes partition for its embeddings EVERY query (300 ms for 89 vectors
    // on portserve). The exact arm now caches its gather under the epoch,
    // exactly as the ANN arm caches its HNSW: the first query gathers (a
    // column scan), the next scores from memory (none), and a write
    // invalidates so no stale vector is scored.
    let g = vector_graph(50, 8);
    let q = seeded_vec(7_777, 8);
    g.set_vector_exact_max(100);
    // Cold: one gather, one column scan, the cache populated.
    let (cold, tc) = engram_observe::with_trace(|| g.vector_query("vi", 5, &q).expect("cold"));
    assert_eq!(
        tc.counters().get("graph.vector exact index cached"),
        Some(&1)
    );
    assert!(
        tc.counters()
            .get("store.column scans")
            .copied()
            .unwrap_or(0)
            >= 1,
        "cold gathers"
    );
    // Warm: same answer, NO gather (no column scan, no new cache build).
    let (warm, tw) = engram_observe::with_trace(|| g.vector_query("vi", 5, &q).expect("warm"));
    assert_eq!(ids_of(&cold.0), ids_of(&warm.0), "warm answer matches cold");
    assert_eq!(
        tw.counters().get("graph.vector exact index cached"),
        None,
        "warm does not re-cache"
    );
    assert_eq!(
        tw.counters().get("store.column scans"),
        None,
        "warm does not re-scan for vectors"
    );
    // A write invalidates: the next query rebuilds the cache and the new row
    // is findable (the no-stale rule, for the exact arm too).
    let mut props = BTreeMap::new();
    props.insert("i".to_string(), Value::Int(9_999));
    props.insert(
        "e".to_string(),
        Value::List(q.iter().copied().map(Value::Float).collect()),
    );
    g.create_node(&["V".to_string()], &props).expect("node"); // i = 9_999
    let (after, ta) =
        engram_observe::with_trace(|| g.vector_query("vi", 5, &q).expect("after write"));
    assert_eq!(
        ta.counters().get("graph.vector exact index cached"),
        None,
        "the write did NOT rebuild — it was folded in incrementally"
    );
    assert!(
        ta.counters()
            .get("graph.vector index incrementally maintained")
            .is_some(),
        "the incremental path ran"
    );
    let top = ids_of(&after.0);
    assert_eq!(
        top[0], 9_999,
        "the freshly written exact-match vector is FIRST: {top:?}"
    );
    assert!((after.0[0].1 - 1.0).abs() < 1e-12, "at the exact 1.0");
    // No stale answers: DELETE it, and it must vanish next query (no rebuild).
    let del_id = {
        // find its internal id via a fresh exact query then delete_node.
        // create_node returned the id; re-fetch by the unique i via a scan.
        let r = g.vector_query("vi", 60, &q).expect("all");
        r.0.iter().find(|(n, _)| matches!(n, Value::Node { props, .. } if matches!(props.get("i"), Some(Value::Int(9_999))))).map(|(n, _)| match n { Value::Node { id, .. } => *id, _ => unreachable!() }).expect("present")
    };
    g.delete_node(del_id, true).expect("delete");
    let (gone, tg) =
        engram_observe::with_trace(|| g.vector_query("vi", 5, &q).expect("after delete"));
    assert!(
        !ids_of(&gone.0).contains(&9_999),
        "the deleted vector is gone"
    );
    assert_eq!(
        tg.counters().get("graph.vector exact index cached"),
        None,
        "delete folded in, not rebuilt"
    );
}

#[test]
fn incremental_maintenance_matches_a_full_rebuild_under_mixed_writes() {
    // The cutover blocker: writes must NOT rebuild the index. Inserts,
    // updates and deletes are folded in id by id. To make each observable in
    // the top-k, the manipulated vectors are placed NEAR the query (scaled
    // copies, cosine ~1.0), so a dropped insert, a stale delete, or a missed
    // update all show up in the ranking — and each is canaried.
    let g = vector_graph(400, 8);
    g.set_vector_exact_max(50); // ANN arm
    let q = seeded_vec(555, 8);
    let near = |scale: f64| -> Value {
        Value::List(q.iter().map(|x| Value::Float(x * scale)).collect()) // cosine(q, q*scale) = 1
    };
    let (_, plan) = g.vector_query("vi", 10, &q).expect("build");
    assert!(
        matches!(plan.arm, VectorArm::Ann { rebuilt: true }),
        "first is a build"
    );

    let mk = |g: &Graph, i: i64, e: Value| -> u64 {
        let mut p = BTreeMap::new();
        p.insert("i".to_string(), Value::Int(i));
        p.insert("e".to_string(), e);
        g.create_node(&["V".to_string()], &p).expect("node")
    };
    for i in 0..5i64 {
        mk(&g, 100 + i, near(1.0 + i as f64 * 0.01));
    }
    let mut to_delete = Vec::new();
    for i in 0..5i64 {
        let id = mk(&g, 200 + i, near(2.0));
        if i < 3 {
            to_delete.push(id);
        }
    }
    // INDEX everything created so far (fold the delta), so the deletes below
    // operate on nodes already in the HNSW/vectors — the delete-of-an-indexed
    // -node path the canary guards.
    let all = g.vector_query("vi", 500, &q).expect("all");
    for id in &to_delete {
        g.delete_node(*id, true).expect("delete");
    }
    let by_i = |want: i64| -> u64 {
        all.0.iter().find(|(n, _)| matches!(n, Value::Node { props, .. } if matches!(props.get("i"), Some(Value::Int(x)) if *x == want)))
            .map(|(n, _)| match n { Value::Node { id, .. } => *id, _ => unreachable!() }).expect("found")
    };
    let moved = by_i(399);
    g.set_prop(true, moved, "e", &near(3.0))
        .expect("update onto q");

    let (inc, ti) =
        engram_observe::with_trace(|| g.vector_query("vi", 12, &q).expect("incremental"));
    assert_eq!(
        inc.1.arm,
        VectorArm::Ann { rebuilt: false },
        "folded in, not rebuilt"
    );
    assert!(
        ti.counters()
            .get("graph.vector index incrementally maintained")
            .is_some()
    );
    let inc_is: Vec<i64> = inc
        .0
        .iter()
        .filter_map(|(n, _)| match n {
            Value::Node { props, .. } => match props.get("i") {
                Some(Value::Int(x)) => Some(*x),
                _ => None,
            },
            _ => None,
        })
        .collect();
    let set: std::collections::BTreeSet<i64> = inc_is.iter().copied().collect();

    for i in 100..105i64 {
        assert!(
            set.contains(&i),
            "inserted-on-q i={i} must be in the top: {inc_is:?}"
        );
    }
    for i in [203i64, 204] {
        assert!(set.contains(&i), "surviving i={i} present: {inc_is:?}");
    }
    for i in [200i64, 201, 202] {
        assert!(
            !set.contains(&i),
            "DELETED i={i} must be gone (no stale): {inc_is:?}"
        );
    }
    assert!(
        set.contains(&399),
        "updated-onto-q i=399 must appear: {inc_is:?}"
    );
    // Full k results: a deleted node must not steal a top-k slot (it would
    // rank by its stale cached vector, then materialise to nothing).
    assert_eq!(
        inc.0.len(),
        12,
        "k live results, no slot stolen by a delete: {inc_is:?}"
    );
    for (n, sc) in &inc.0 {
        if let Value::Node { props, .. } = n {
            if let Some(Value::Int(x)) = props.get("i") {
                if (100..105).contains(x) || *x == 203 || *x == 204 || *x == 399 {
                    assert!((sc - 1.0).abs() < 1e-9, "i={x} at cosine 1.0, got {sc}");
                }
            }
        }
    }
}

#[test]
fn a_write_to_ANOTHER_index_does_not_touch_this_one() {
    // The whole point: writes elsewhere must not rebuild an unrelated index.
    let g = vector_graph(50, 8);
    g.set_vector_exact_max(100);
    let q = seeded_vec(42, 8);
    g.vector_query("vi", 5, &q).expect("build vi");
    // A second index over a DIFFERENT label, and a write to it.
    run_stmt(
        &g,
        &parse_any("CREATE VECTOR INDEX wi FOR (w:W) ON (w.e)").expect("ddl"),
        BTreeMap::new(),
    )
    .expect("wi");
    let mut p = BTreeMap::new();
    p.insert(
        "e".to_string(),
        Value::List(seeded_vec(1, 8).into_iter().map(Value::Float).collect()),
    );
    g.create_node(&["W".to_string()], &p).expect("write W");
    // Querying vi must NOT rebuild and must NOT re-cache — the W write left
    // vi's delta empty.
    let (_, t) = engram_observe::with_trace(|| g.vector_query("vi", 5, &q).expect("vi again"));
    assert_eq!(
        t.counters().get("graph.vector exact index cached"),
        None,
        "vi not rebuilt by a W write"
    );
    assert_eq!(
        t.counters().get("store.column scans"),
        None,
        "vi not re-gathered"
    );
    assert!(
        t.counters()
            .get("graph.vector index incrementally maintained")
            .is_none(),
        "vi had nothing to fold"
    );
}
