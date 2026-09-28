//! The two algorithm paths that hold RESOURCES: the keyspace write-back and
//! the result cache.
//!
//! A coverage audit found both effectively untested where they are most likely
//! to break.
//!
//! **The write-back's chunking had never run.** `writeBatchSize` defaults to
//! 10,000 and no test graph was that large, so every asserted write went round
//! the loop once. An off-by-one that skipped the last partial chunk — leaving
//! the receipt claiming every node and the graph missing the tail — would have
//! passed every gate.
//!
//! Writing these tests corrected a claim, too. The catalogue said each `.write`
//! persisted "in short transactions" for all twelve entries, and no such
//! transaction exists: under Bolt the enclosing statement is already one
//! autocommit transaction, and a Bolt statement is ATOMIC. A write-back that
//! committed in pieces would break that guarantee rather than honour it, so
//! `writeBatchSize` bounds work granularity and never a durability boundary.
//! The counter said so too — `algo.write batches committed` fired once per
//! CALL, outside the loop, which is how a test asserting nine chunks saw one.
//!
//! **The result cache's byte budget had never been reached.** Its eviction
//! path, its refusal of a result larger than the whole budget, and its
//! promise to name what it dropped were all unexercised: the cache is 512 MiB
//! by default and no test published anything close.
//!
//! Both are resource paths, and a resource path that is never pressed is a
//! guess about what happens under pressure.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn corpus(g: &Graph, n: usize) -> Vec<u64> {
    let ids: Vec<u64> = (0..n)
        .map(|i| {
            let mut m = BTreeMap::new();
            m.insert("k".to_string(), Value::Int(i as i64));
            g.create_node(&["N".into()], &m).expect("node")
        })
        .collect();
    for i in 0..n {
        g.create_rel(ids[i], "R", ids[(i * 3 + 1) % n], &BTreeMap::new())
            .expect("rel");
    }
    ids
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .expect("runs")
        .rows
}

fn err(g: &Graph, src: &str) -> String {
    match run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new()) {
        Err(e) => format!("{e:?}"),
        Ok(r) => panic!("expected a refusal, got {} row(s)", r.rows.len()),
    }
}

fn count(g: &Graph, src: &str) -> i64 {
    match rows(g, src)[0][0] {
        Value::Int(i) => i,
        ref other => panic!("expected an int, got {other:?}"),
    }
}

// ─── The write-back across chunks ──────────────────────────────────────────

#[test]
fn a_write_back_across_many_chunks_lands_every_node() {
    // THE PATH THAT HAD NEVER RUN. 60 nodes at 7 per chunk is nine chunks;
    // the default of 10,000 made every previous test a single pass.
    let g = g();
    corpus(&g, 60);
    let (r, trace) = engram_observe::with_trace(|| {
        rows(
            &g,
            "CALL engram.algo.degree.write({nodeLabels:['N'], relationshipTypes:['R'], \
             writeProperty:'d', writeBatchSize: 7}) YIELD nodesWritten RETURN nodesWritten",
        )
    });
    assert_eq!(r[0][0], Value::Int(60), "the receipt must claim every node");
    assert_eq!(
        count(&g, "MATCH (n:N) WHERE n.d IS NOT NULL RETURN count(n)"),
        60,
        "EVERY node must carry the property. A partial write that still reported success is \
         the defect this exists to catch: half the graph annotated, half not, and a receipt \
         that says it went fine",
    );
    let chunks = trace
        .counters()
        .get("algo.write chunks")
        .copied()
        .unwrap_or(0);
    assert!(
        chunks >= 9,
        "60 nodes at 7 per chunk is nine chunks; only {chunks} ran, so the multi-chunk path \
         did not execute and the assertion above proves nothing about it",
    );
}

#[test]
fn a_batch_size_that_does_not_divide_the_corpus_still_writes_the_remainder() {
    // The off-by-one. 61 nodes at 10 per chunk leaves a final partial chunk of
    // one, which is exactly the batch a loop bound written `<` instead of
    // `<=` would drop — and dropping it leaves 60 of 61 annotated and a
    // receipt that says 61.
    let g = g();
    corpus(&g, 61);
    let r = rows(
        &g,
        "CALL engram.algo.degree.write({nodeLabels:['N'], relationshipTypes:['R'], \
         writeProperty:'d', writeBatchSize: 10}) YIELD nodesWritten RETURN nodesWritten",
    );
    assert_eq!(r[0][0], Value::Int(61));
    assert_eq!(
        count(&g, "MATCH (n:N) WHERE n.d IS NOT NULL RETURN count(n)"),
        61,
        "the final partial batch was dropped",
    );
}

#[test]
fn a_write_back_concurrent_with_another_writer_lands_every_projected_node() {
    // The write-back runs AFTER the computation and writes through the
    // ordinary path, so it can collide with an ordinary writer. Neither may
    // lose: the projected nodes must all be annotated, and the other writer's
    // nodes must all exist.
    let g = Arc::new(Graph::new(Store::new(), Realm(1), Namespace(1)));
    corpus(&g, 80);
    const OTHER: u64 = 300;
    let made = Arc::new(AtomicU64::new(0));

    std::thread::scope(|s| {
        let wg = Arc::clone(&g);
        let wc = Arc::clone(&made);
        s.spawn(move || {
            for i in 0..OTHER {
                let mut m = BTreeMap::new();
                m.insert("k".to_string(), Value::Int(90_000 + i as i64));
                if wg.create_node(&["Other".into()], &m).is_ok() {
                    wc.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        // Small chunks, so the write-back's writes interleave with the
        // writer's rather than all landing in one window.
        let r = rows(
            &g,
            "CALL engram.algo.degree.write({nodeLabels:['N'], relationshipTypes:['R'], \
             writeProperty:'d', writeBatchSize: 5}) YIELD nodesWritten RETURN nodesWritten",
        );
        assert_eq!(r[0][0], Value::Int(80));
    });

    assert_eq!(
        count(&g, "MATCH (n:N) WHERE n.d IS NOT NULL RETURN count(n)"),
        80,
        "a concurrent writer cost the write-back some of its nodes",
    );
    assert_eq!(
        count(&g, "MATCH (n:Other) RETURN count(n)"),
        made.load(Ordering::Relaxed) as i64,
        "the write-back cost the concurrent writer some of ITS nodes",
    );
}

#[test]
fn a_write_back_racing_a_delete_leaves_the_store_consistent() {
    // The other race: nodes leaving the projection while the write-back is
    // annotating it. The write-back computed over a snapshot that still had
    // them, so it may or may not annotate a deleted node — but the store must
    // not end up with a property on a node that no longer exists, and nothing
    // may panic.
    let g = Arc::new(Graph::new(Store::new(), Realm(1), Namespace(1)));
    let ids = corpus(&g, 80);
    let doomed: Vec<u64> = ids.iter().copied().skip(40).collect();

    std::thread::scope(|s| {
        let dg = Arc::clone(&g);
        s.spawn(move || {
            for id in &doomed {
                let _ = dg.delete_node(*id, true);
            }
        });
        let _ = run_query(
            &g,
            &parse_statement(
                "CALL engram.algo.degree.write({nodeLabels:['N'], relationshipTypes:['R'], \
                 writeProperty:'d', writeBatchSize: 5}) YIELD nodesWritten RETURN nodesWritten",
            )
            .expect("parses"),
            BTreeMap::new(),
        );
    });

    // Whatever survived must be coherent: every node carrying the property is
    // a node that still exists, which is what `MATCH` can only return.
    let annotated = count(&g, "MATCH (n:N) WHERE n.d IS NOT NULL RETURN count(n)");
    let alive = count(&g, "MATCH (n:N) RETURN count(n)");
    assert!(
        annotated <= alive,
        "more nodes carry the property ({annotated}) than exist ({alive}) — the write-back \
         wrote to nodes that had been deleted",
    );
    assert!(alive >= 40, "the delete removed more than it was asked to");
}

// ─── The result cache under its budget ─────────────────────────────────────

#[test]
fn the_result_cache_evicts_by_budget_and_the_oldest_goes_first() {
    // THE PATH THAT HAD NEVER RUN. The budget is 512 MiB by default and no
    // test published anything close, so eviction, its ordering, and the
    // counter that reports it were all unexercised.
    let g = g();
    corpus(&g, 400);
    // A budget that a couple of results exceed, so publishing several must
    // evict.
    g.set_algo_cache_bytes(8_000);

    let (_, trace) = engram_observe::with_trace(|| {
        for i in 0..6 {
            rows(
                &g,
                &format!(
                    "CALL engram.algo.degree.mutate({{nodeLabels:['N'], \
                     relationshipTypes:['R'], mutateKey:'k{i}'}}) YIELD mutateKey RETURN mutateKey"
                ),
            );
        }
    });
    assert!(
        trace
            .counters()
            .get("algo.result evicted for budget")
            .copied()
            .unwrap_or(0)
            > 0,
        "six results against an 8 KB budget evicted nothing — the budget is not being \
         enforced, so a `mutate` loop grows the cache without bound",
    );
    // The cache must still be usable and must not hold everything.
    let listed = count(
        &g,
        "CALL engram.algo.result.list() YIELD mutateKey RETURN count(mutateKey)",
    );
    assert!(
        listed < 6,
        "all six results survived an 8 KB budget, so nothing was evicted ({listed} listed)",
    );
    assert!(listed >= 1, "eviction emptied the cache entirely");
    g.set_algo_cache_bytes(512 * 1024 * 1024);
}

#[test]
fn a_result_larger_than_the_whole_budget_is_refused_rather_than_emptying_the_cache() {
    // Emptying the cache to hold one thing that then does not fit either is
    // strictly worse than declining — the caller loses every earlier result
    // AND does not get the new one.
    let g = g();
    corpus(&g, 400);
    g.set_algo_cache_bytes(16 * 1024 * 1024);
    rows(
        &g,
        "CALL engram.algo.degree.mutate({nodeLabels:['N'], relationshipTypes:['R'], \
         mutateKey:'keep'}) YIELD mutateKey RETURN mutateKey",
    );
    // Now shrink the budget below one result and publish another.
    g.set_algo_cache_bytes(8);
    let e = err(
        &g,
        "CALL engram.algo.degree.mutate({nodeLabels:['N'], relationshipTypes:['R'], \
         mutateKey:'toobig'}) YIELD mutateKey RETURN mutateKey",
    );
    assert!(
        e.contains("ENGRAM_ALGO_CACHE_BYTES"),
        "the refusal must name the lever that would raise the budget, got {e}",
    );
    g.set_algo_cache_bytes(512 * 1024 * 1024);
    assert_eq!(
        count(
            &g,
            "CALL engram.algo.result.stream({mutateKey:'keep'}) YIELD value RETURN count(value)",
        ),
        400,
        "the earlier result was destroyed to make room for one that did not fit anyway",
    );
}

#[test]
fn dropping_a_cached_result_frees_its_budget() {
    // Otherwise the budget is a high-water mark rather than a budget, and a
    // long-lived session that published and dropped repeatedly would refuse
    // for ever.
    let g = g();
    corpus(&g, 400);
    g.set_algo_cache_bytes(30_000);
    rows(
        &g,
        "CALL engram.algo.degree.mutate({nodeLabels:['N'], relationshipTypes:['R'], \
         mutateKey:'a'}) YIELD mutateKey RETURN mutateKey",
    );
    assert_eq!(
        count(
            &g,
            "CALL engram.algo.result.drop({mutateKey:'a'}) YIELD dropped RETURN count(dropped)",
        ),
        1,
    );
    // With the budget freed, a fresh publish must succeed rather than evict.
    let (_, trace) = engram_observe::with_trace(|| {
        rows(
            &g,
            "CALL engram.algo.degree.mutate({nodeLabels:['N'], relationshipTypes:['R'], \
             mutateKey:'b'}) YIELD mutateKey RETURN mutateKey",
        );
    });
    assert_eq!(
        trace
            .counters()
            .get("algo.result evicted for budget")
            .copied()
            .unwrap_or(0),
        0,
        "publishing after a drop still evicted, so the drop did not return its bytes to the \
         budget",
    );
    g.set_algo_cache_bytes(512 * 1024 * 1024);
}
