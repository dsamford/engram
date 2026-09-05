#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! The constraint-list cache may skip the schema-epoch PROBE, never the
//! schema-epoch READ SET ENTRY.
//!
//! Every constrained write calls `constraints_snapshot`. It used to decide
//! whether its cached list was current by reading `kv/con\0epoch` through the
//! transaction. That key is ABSENT until the first constraint DDL, and an
//! absent KV key is the worst case for the read path: the sparse index cannot
//! reject it, so the probe descends every sealed segment — on paged SF1, up to
//! ~117 block-cache acquisitions, `pread`s and BLAKE3 verifications, per write.
//!
//! The value was never used for anything except a cache-currency test that
//! `bump_constraint_epoch` already performs directly. So a cache hit now
//! REGISTERS the key instead of reading it.
//!
//! The bar is not "faster". It is that the abort behaviour is **identical**:
//! validation asks only whether a key moved since the snapshot, never what it
//! said. These tests assert the guarantee on BOTH lever arms — if it held only
//! on one, the lever would be trading isolation for throughput, which is the
//! one trade this change may not make.

use std::collections::BTreeMap;

use engram_cypher::{parse_any, parse_statement};
use engram_graph::{Graph, GraphError, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph_over(store: Store) -> Graph {
    Graph::new(store, Realm(1), Namespace(1))
}

fn run(g: &Graph, src: &str) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"));
}

fn try_run(g: &Graph, src: &str) -> Result<(), String> {
    let q = parse_statement(src).map_err(|e| e.to_string())?;
    run_query(g, &q, BTreeMap::new())
        .map(|_| ())
        .map_err(|e| format!("{e:?}"))
}

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn buffered(g: &Graph, src: &str) -> engram_graph::GraphTxn {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let txn = g.open_txn();
    let (txn, r) = g.with_txn(txn, || run_query(g, &q, BTreeMap::new()));
    r.unwrap_or_else(|e| panic!("txn `{src}`: {e}"));
    txn
}

fn count(trace: &engram_observe::Trace, k: &str) -> u64 {
    trace.counters().get(k).copied().unwrap_or(0)
}

fn rows(g: &Graph, src: &str) -> usize {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
        .len()
}

/// THE guarantee, on both arms: a constraint DDL that commits after an
/// enforcing writer's snapshot must abort that writer.
///
/// This is what the epoch key is FOR. If skipping the probe also skipped the
/// read-set entry, this transaction would commit against a constraint list it
/// never saw — a silent isolation regression that no throughput number would
/// reveal.
#[test]
fn a_ddl_after_the_snapshot_aborts_the_enforcing_writer_on_both_arms() {
    for on in [false, true] {
        let g = graph_over(Store::new());
        g.set_constraint_epoch_cache(on);
        ddl(&g, "CREATE CONSTRAINT u FOR (n:U) REQUIRE n.u IS UNIQUE");
        // Warm the cache, so the ON arm genuinely takes its hit path below
        // rather than the cold path that still probes.
        run(&g, "CREATE (:U {u: 1})");

        let a = buffered(&g, "CREATE (:U {u: 2})");
        // A DDL commits in between, moving the schema epoch.
        ddl(&g, "CREATE CONSTRAINT v FOR (n:V) REQUIRE n.v IS UNIQUE");

        assert!(
            matches!(g.commit_owned(a), Err(GraphError::TxnConflict)),
            "arm on={on}: a constraint DDL committing after the snapshot MUST \
             abort the in-flight enforcing writer — the epoch key must be in \
             its read set whether we probed it or registered it"
        );
    }
}

/// Non-vacuity: the ON arm really takes the cache path, the OFF arm really
/// does not. Without this the differential above could be comparing the probe
/// arm against itself and would pass while the lever did nothing.
#[test]
fn the_lever_actually_switches_the_path() {
    let g = graph_over(Store::new());
    g.set_constraint_epoch_cache(true);
    ddl(&g, "CREATE CONSTRAINT u FOR (n:U) REQUIRE n.u IS UNIQUE");
    run(&g, "CREATE (:U {u: 1})"); // warm
    let (_, on_trace) = engram_observe::with_trace(|| run(&g, "CREATE (:U {u: 2})"));
    assert!(
        count(&on_trace, "graph.constraint epoch served from cache") >= 1,
        "ON arm must serve the epoch from cache, got {:?}",
        on_trace.counters()
    );

    let g2 = graph_over(Store::new());
    g2.set_constraint_epoch_cache(false);
    ddl(&g2, "CREATE CONSTRAINT u FOR (n:U) REQUIRE n.u IS UNIQUE");
    run(&g2, "CREATE (:U {u: 1})"); // warm
    let (_, off_trace) = engram_observe::with_trace(|| run(&g2, "CREATE (:U {u: 2})"));
    assert_eq!(
        count(&off_trace, "graph.constraint epoch served from cache"),
        0,
        "OFF arm must still probe — it is the differential's control"
    );
}

/// Enforcement itself is unchanged: the same duplicates are refused, the same
/// distinct values are admitted, and the corpus ends identical on both arms.
#[test]
fn enforcement_is_byte_for_byte_the_same_on_both_arms() {
    let mut finals = Vec::new();
    for on in [false, true] {
        let g = graph_over(Store::new());
        g.set_constraint_epoch_cache(on);
        ddl(&g, "CREATE CONSTRAINT u FOR (n:U) REQUIRE n.u IS UNIQUE");

        let mut verdicts = Vec::new();
        for i in 0..40u64 {
            // Every third value repeats an earlier one, so the refusal
            // pattern is part of what must match.
            let v = if i % 3 == 0 { i / 3 } else { i + 1000 };
            verdicts.push(try_run(&g, &format!("CREATE (:U {{u: {v}}})")).is_ok());
        }
        // A label change and a SET, because those take the enforcement path too.
        verdicts.push(try_run(&g, "MATCH (n:U {u: 1001}) SET n.u = 9999").is_ok());
        verdicts.push(try_run(&g, "MATCH (n:U {u: 1002}) SET n.u = 9999").is_ok());

        let population = rows(&g, "MATCH (n:U) RETURN n.u");
        finals.push((verdicts, population));
    }
    assert_eq!(
        finals[0], finals[1],
        "the lever may change what the engine PAYS, never what it ANSWERS"
    );
}

/// The cold path still learns the epoch. If a cache miss stopped recording it,
/// a later probe-arm hit would compare against a stale value and serve a list
/// the DDL had already superseded.
#[test]
fn a_cold_miss_still_records_the_epoch_so_later_hits_are_correct() {
    let g = graph_over(Store::new());
    g.set_constraint_epoch_cache(true);
    ddl(&g, "CREATE CONSTRAINT u FOR (n:U) REQUIRE n.u IS UNIQUE");
    // First constrained write after the DDL: a COLD miss.
    run(&g, "CREATE (:U {u: 1})");
    // Second: a hit. The constraint must still be enforced.
    assert!(
        try_run(&g, "CREATE (:U {u: 1})").is_err(),
        "the duplicate must still be refused through the cache-hit path"
    );
    // And a fresh DDL must still invalidate what the hit path serves.
    ddl(&g, "DROP CONSTRAINT u");
    assert!(
        try_run(&g, "CREATE (:U {u: 1})").is_ok(),
        "after DROP the duplicate must be admitted — a cache that survived the \
         DDL would keep enforcing a constraint that no longer exists"
    );
}
