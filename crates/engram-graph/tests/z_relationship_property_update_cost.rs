#![allow(non_snake_case)]
// A real clock: the cost under test is wall time.
#![allow(clippy::disallowed_methods)]
//! How expensive is UPDATING a property on an existing relationship?
//!
//! MEASURED on the SF3 bench corpus, 2026-09-21, isolated with read controls:
//!
//! | statement | time |
//! |---|---|
//! | `MATCH (:Person)-[r:T]->(:Person) WITH r LIMIT 1 RETURN count(*)` | 0 s |
//! | the same `+ SET r.w = 39.0` | **109 s** |
//! | the same shape on a NODE property | 0 s |
//! | `CREATE` of 565,247 relationships | 48 s (~85 us each) |
//!
//! and the cost is PER-STATEMENT, not per-row — one row cost 106 s and a
//! hundred rows cost 111 s — while a corpus-loaded type amortised it (84 s to
//! 8 s on a second write) and a type minted at runtime never did.
//!
//! This file asks whether that reproduces LOCALLY and, if so, whether it grows
//! with the relationship-type size. A local reproduction is worth a lot: the
//! bench observation costs a 16.8 GB store copy and minutes per data point.

use std::collections::BTreeMap;
use std::time::Instant;

use engram_cypher::{parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn time_ms(g: &Graph, q: &str) -> u128 {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    let t = Instant::now();
    run_query(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
    t.elapsed().as_millis()
}

/// `n` nodes, each joined to the next by `:T`, so the type has ~`n` edges.
fn graph_with(n: usize) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(
        &g,
        &format!("UNWIND range(0, {}) AS i CREATE (:P {{id: i}})", n - 1),
    );
    // NOT `MATCH (a:P), (b:P) WHERE b.id = a.id + 1` -- that is a CARTESIAN
    // product of n^2 pairs, which at n = 64,000 is 4.1 BILLION. The first
    // version of this fixture did exactly that and exhausted the allocator.
    // Worth the comment because the crash looked like an engine memory defect
    // and was a defect in the measuring apparatus.
    ddl(
        &g,
        &format!(
            "UNWIND range(0, {}) AS i MATCH (a:P {{id: i}}), (b:P {{id: i + 1}}) \
             CREATE (a)-[:T {{w: 40.0}}]->(b)",
            n - 2
        ),
    );
    g
}

/// The READ control and the WRITE, at one size. Prints both so the ratio is
/// visible even when the assertion passes.
fn read_and_write_ms(n: usize) -> (u128, u128) {
    let g = graph_with(n);
    let read = time_ms(
        &g,
        "MATCH (:P)-[r:T]->(:P) WITH r LIMIT 1 RETURN count(*) AS n",
    );
    let write = time_ms(
        &g,
        "MATCH (:P)-[r:T]->(:P) WITH r LIMIT 1 SET r.w = 39.0 RETURN count(*) AS n",
    );
    (read, write)
}

#[test]
#[ignore = "timing probe, not an assertion — run with --ignored --nocapture"]
fn how_does_a_single_relationship_update_scale_with_type_size() {
    println!(
        "{:>8}  {:>10}  {:>10}  {:>8}",
        "edges", "read ms", "write ms", "ratio"
    );
    for n in [1_000usize, 4_000, 16_000, 64_000] {
        let (read, write) = read_and_write_ms(n);
        let ratio = if read == 0 {
            f64::INFINITY
        } else {
            write as f64 / read as f64
        };
        println!("{n:>8}  {read:>10}  {write:>10}  {ratio:>8.1}");
    }
}

/// A NODE property update at the same size, for contrast. On the bench corpus
/// this was 0 s against the relationship's 109 s.
#[test]
#[ignore = "timing probe, not an assertion — run with --ignored --nocapture"]
fn a_node_update_for_contrast() {
    for n in [1_000usize, 16_000, 64_000] {
        let g = graph_with(n);
        let node = time_ms(
            &g,
            "MATCH (p:P) WITH p LIMIT 1 SET p.probe = 1 RETURN count(*) AS n",
        );
        let rel = time_ms(
            &g,
            "MATCH (:P)-[r:T]->(:P) WITH r LIMIT 1 SET r.w = 39.0 RETURN count(*) AS n",
        );
        println!("edges={n:>6}  node update {node:>6} ms   rel update {rel:>6} ms");
    }
}

/// Does a SECOND update on the same type cost the same as the first?
///
/// On the bench corpus the runtime-minted type never amortised — three
/// consecutive single-row updates cost 106 s each — while the corpus-loaded
/// type went 84 s to 8 s.
#[test]
#[ignore = "timing probe, not an assertion — run with --ignored --nocapture"]
fn does_a_second_update_on_the_same_type_amortise() {
    let g = graph_with(64_000);
    let q = "MATCH (:P)-[r:T]->(:P) WITH r LIMIT 1 SET r.w = 39.0 RETURN count(*) AS n";
    for i in 1..=4 {
        println!("update #{i}: {} ms", time_ms(&g, q));
    }
}

/// WHAT does the update spend its time on? Counter dump at one size, for the
/// read and the write, so the difference between the two columns is visible.
#[test]
#[ignore = "diagnostic, not an assertion — run with --ignored --nocapture"]
fn what_does_the_relationship_update_actually_do() {
    let g = graph_with(16_000);
    let read = "MATCH (:P)-[r:T]->(:P) WITH r LIMIT 1 RETURN count(*) AS n";
    let write = "MATCH (:P)-[r:T]->(:P) WITH r LIMIT 1 SET r.w = 39.0 RETURN count(*) AS n";

    let snap = |q: &str| {
        let s = parse_statement(q).expect("parses");
        let (_, trace) = engram_observe::with_trace(|| {
            run_query(&g, &s, BTreeMap::new()).expect("runs");
        });
        trace
            .counters()
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect::<BTreeMap<String, u64>>()
    };

    let r = snap(read);
    let w = snap(write);

    let mut keys: Vec<&String> = r.keys().chain(w.keys()).collect();
    keys.sort();
    keys.dedup();
    println!("{:>12} {:>12}   counter", "read", "write");
    for k in keys {
        let a = r.get(k).copied().unwrap_or(0);
        let b = w.get(k).copied().unwrap_or(0);
        // Only the rows where the write does materially more work.
        if b > a.saturating_mul(2) || (a == 0 && b > 0) {
            println!("{a:>12} {b:>12}   {k}");
        }
    }
}

/// A write no longer materialises the nodes of its match.
///
/// This case previously asserted the GAP — that one `SET` under `LIMIT 1`
/// decoded the whole type — and it failed, as its own text asked it to, when
/// `demands_after` learned to see through a `SET`. It now pins the behaviour
/// that replaced it.
///
/// The RELATIONSHIP is still materialised in full here, and correctly so:
/// `WITH r` is a BARE USE, and a row that carries `r` forward carries its
/// properties. Nothing reads the anonymous endpoints, so they are bound
/// leanly.
#[test]
fn a_write_no_longer_materialises_the_NODES_of_its_match() {
    let g = graph_with(4_000);
    let q = "MATCH (:P)-[r:T]->(:P) WITH r LIMIT 1 SET r.w = 39.0 RETURN count(*) AS n";
    let s = parse_statement(q).expect("parses");
    let (_, trace) = engram_observe::with_trace(|| {
        run_query(&g, &s, BTreeMap::new()).expect("runs");
    });
    let c = trace.counters();
    let get = |k: &str| c.get(k).copied().unwrap_or(0);

    assert_eq!(
        get("store.puts"),
        1,
        "the statement writes exactly one property"
    );
    assert_eq!(
        get("graph.nodes materialised in full"),
        0,
        "the anonymous endpoints are read by nothing and must not be decoded"
    );
    assert!(
        get("interp.matcher bound a hop end bare") > 1_000,
        "they must be bound leanly instead, got {}",
        get("interp.matcher bound a hop end bare")
    );
}

/// Is it the write to `r`, or the mere PRESENCE of a write in the statement?
///
/// `mat_end` already prunes an end nothing reads ("Fix 68: an end NOTHING
/// reads … needs no record") and rejects a non-member from membership
/// ("Fix 74"). Every one of those skips is guarded by
/// `!graph.in_txn_with_writes()`, for a stated reason: inside a writing
/// transaction the overlay's buffered labels must win over a membership
/// snapshot that predates them.
///
/// If that guard is the cause, then a statement whose write touches something
/// ENTIRELY UNRELATED to the pattern pays the same price — the optimisation is
/// disabled per-statement, not per-entity.
#[test]
#[ignore = "diagnostic, not an assertion — run with --ignored --nocapture"]
fn is_it_the_write_to_r_or_ANY_write_in_the_statement() {
    let count = |g: &Graph, q: &str| -> (u64, u64) {
        let s = parse_statement(q).expect("parses");
        let (_, trace) = engram_observe::with_trace(|| {
            run_query(g, &s, BTreeMap::new()).expect("runs");
        });
        let c = trace.counters();
        (
            c.get("graph.nodes materialised in full")
                .copied()
                .unwrap_or(0),
            c.get("graph.rels materialised in full")
                .copied()
                .unwrap_or(0),
        )
    };
    for (label, q) in [
        (
            "read only, no write at all",
            "MATCH (:P)-[r:T]->(:P) WITH r LIMIT 1 RETURN count(*) AS n",
        ),
        (
            "writes the matched relationship",
            "MATCH (:P)-[r:T]->(:P) WITH r LIMIT 1 SET r.w = 39.0 RETURN count(*) AS n",
        ),
        (
            "writes an UNRELATED brand-new node",
            "MATCH (:P)-[r:T]->(:P) WITH r LIMIT 1 CREATE (:Zzz {k: 1}) RETURN count(*) AS n",
        ),
    ] {
        let g = graph_with(8_000);
        let (nodes, rels) = count(&g, q);
        println!("{nodes:>8} nodes  {rels:>8} rels   {label}");
    }
}

/// A GAP, pinned as a gap — an executable specification for the fix.
///
/// `mat_end` already prunes an end nothing reads and rejects a non-member from
/// the label's membership without reading a record. Every such skip is guarded
/// by `!graph.in_txn_with_writes()`, and that guard is **statement-wide**: a
/// statement that creates a brand-new, unrelated `:Zzz` node materialises the
/// whole `(:P)-[:T]->(:P)` pattern in full, exactly as one that writes the
/// matched relationship does.
///
/// The guard's REASON is real — inside a writing transaction the overlay's
/// buffered labels must win over a membership snapshot that predates them —
/// but it only applies to entities the transaction actually touched. A newly
/// created `:Zzz` cannot change `:P` membership or a `:T` record.
///
/// The machinery for a narrower guard exists: `Txn::writes` is a BTreeMap in
/// logical-key order and `pending_body_prefix_present` already range-scans it,
/// so "has this transaction written under THIS prefix" is answerable. What it
/// needs is the correct prefix set per guard site — and a guard that wrongly
/// says "untouched" serves STALE DATA, which is worse than being slow. That is
/// why this is a specification here rather than a change.
///
/// **When this test fails, the guard has been narrowed — read it and delete
/// it.** Asserted on counters, which are deterministic, not on timings.
#[test]
fn an_UNRELATED_write_STILL_disables_the_read_sides_pruning() {
    let g = graph_with(4_000);
    // The write touches a brand-new node of a label the pattern never mentions.
    let q = "MATCH (:P)-[r:T]->(:P) WITH r LIMIT 1 CREATE (:Zzz {k: 1}) RETURN count(*) AS n";
    let s = parse_statement(q).expect("parses");
    let (_, trace) = engram_observe::with_trace(|| {
        run_query(&g, &s, BTreeMap::new()).expect("runs");
    });
    let nodes = trace
        .counters()
        .get("graph.nodes materialised in full")
        .copied()
        .unwrap_or(0);
    assert!(
        nodes > 1_000,
        "KNOWN GAP: creating an unrelated `:Zzz` node should not force the \
         `(:P)-[:T]->(:P)` pattern to be materialised in full, yet {nodes} \
         nodes were decoded. If this is now small, the `in_txn_with_writes` \
         guard has been narrowed to what the transaction actually touched -- \
         delete this test."
    );
}

/// Is the cost the WRITE, or the columnar machinery being switched off?
///
/// `Graph::columnar_scans_enabled()` is
/// `self.columnar_scans.get() && !self.in_txn_with_writes()` — a MASTER SWITCH
/// that turns every columnar path off inside any writing statement. If that is
/// the cause, a READ with columnar scans disabled by hand will pay the same
/// cost as the write, with no write anywhere in it.
#[test]
#[ignore = "diagnostic, not an assertion — run with --ignored --nocapture"]
fn is_the_cost_the_write_or_the_columnar_switch() {
    let q_read = "MATCH (:P)-[r:T]->(:P) WITH r LIMIT 1 RETURN count(*) AS n";
    let q_write = "MATCH (:P)-[r:T]->(:P) WITH r LIMIT 1 SET r.w = 39.0 RETURN count(*) AS n";
    let count = |g: &Graph, q: &str| -> (u64, u64) {
        let s = parse_statement(q).expect("parses");
        let (_, trace) = engram_observe::with_trace(|| {
            run_query(g, &s, BTreeMap::new()).expect("runs");
        });
        let c = trace.counters();
        (
            c.get("graph.nodes materialised in full")
                .copied()
                .unwrap_or(0),
            c.get("store.gets").copied().unwrap_or(0),
        )
    };

    let g = graph_with(8_000);
    let (n1, g1) = count(&g, q_read);
    println!("{n1:>8} nodes {g1:>8} gets   READ, columnar ON (the default)");

    let g2 = graph_with(8_000);
    g2.set_columnar_scans(false);
    let (n2, gg2) = count(&g2, q_read);
    println!("{n2:>8} nodes {gg2:>8} gets   READ, columnar OFF BY HAND — no write anywhere");

    let g3 = graph_with(8_000);
    let (n3, gg3) = count(&g3, q_write);
    println!("{n3:>8} nodes {gg3:>8} gets   WRITE (columnar off because it writes)");
}

/// Which EXECUTION PATH does each arm take?
#[test]
#[ignore = "diagnostic, not an assertion — run with --ignored --nocapture"]
fn which_path_does_the_write_take() {
    let g = graph_with(8_000);
    for (label, q) in [
        (
            "READ ",
            "MATCH (:P)-[r:T]->(:P) WITH r LIMIT 1 RETURN count(*) AS n",
        ),
        (
            "WRITE",
            "MATCH (:P)-[r:T]->(:P) WITH r LIMIT 1 SET r.w = 39.0 RETURN count(*) AS n",
        ),
    ] {
        let s = parse_statement(q).expect("parses");
        let (_, trace) = engram_observe::with_trace(|| {
            run_query(&g, &s, BTreeMap::new()).expect("runs");
        });
        let c = trace.counters();
        let mut hits: Vec<String> = c
            .iter()
            .filter(|(k, v)| {
                **v > 0 && (k.contains("pipeline") || k.contains("stream") || k.contains("matcher"))
            })
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        hits.sort();
        println!(
            "{label}: {}",
            if hits.is_empty() {
                "<no pipeline/stream/matcher counters>".to_string()
            } else {
                hits.join("  ")
            }
        );
    }
}

/// A LATER match in a writing statement — bi19's shape.
///
/// The first clause writes; a subsequent MATCH then runs with writes buffered,
/// which is where `mat_end`'s skips were declining for the whole statement
/// rather than for the entities the write touched.
#[test]
#[ignore = "diagnostic, not an assertion — run with --ignored --nocapture"]
fn a_later_match_inside_a_writing_statement() {
    let g = graph_with(8_000);
    let q = "MATCH (p:P {id: 0}) SET p.x = 1 WITH p MATCH (a:P)-[:T]->(b:P) RETURN count(b) AS n";
    let s = parse_statement(q).expect("parses");
    let (_, trace) = engram_observe::with_trace(|| {
        run_query(&g, &s, BTreeMap::new()).expect("runs");
    });
    let c = trace.counters();
    let get = |k: &str| c.get(k).copied().unwrap_or(0);
    println!(
        "nodes_full={} rels_full={} gets={} bare_ends={} lean_rels={}",
        get("graph.nodes materialised in full"),
        get("graph.rels materialised in full"),
        get("store.gets"),
        get("interp.matcher bound a hop end bare"),
        get("interp.matcher bound a lean relationship"),
    );
}

/// The same write WITHOUT a bare `WITH r` carrying the relationship forward.
///
/// `WITH r` is a BARE USE and demands the relationship in full — correctly, it
/// is a value the row carries. This shape writes without naming it bare, which
/// is what a precomputation looks like.
#[test]
#[ignore = "diagnostic, not an assertion — run with --ignored --nocapture"]
fn a_write_that_does_not_carry_the_entity_forward() {
    let g = graph_with(8_000);
    for (label, q) in [
        (
            "read  ",
            "MATCH (a:P)-[r:T]->(b:P) WHERE a.id < 3 RETURN count(*) AS n",
        ),
        (
            "write ",
            "MATCH (a:P)-[r:T]->(b:P) WHERE a.id < 3 SET r.w = 39.0 RETURN count(*) AS n",
        ),
    ] {
        let s = parse_statement(q).expect("parses");
        let (_, trace) = engram_observe::with_trace(|| {
            run_query(&g, &s, BTreeMap::new()).expect("runs");
        });
        let c = trace.counters();
        let get = |k: &str| c.get(k).copied().unwrap_or(0);
        println!(
            "{label} gets={:<7} nodes_full={:<7} rels_full={:<7} bare_ends={:<7} lean_rels={}",
            get("store.gets"),
            get("graph.nodes materialised in full"),
            get("graph.rels materialised in full"),
            get("interp.matcher bound a hop end bare"),
            get("interp.matcher bound a lean relationship"),
        );
    }
}
