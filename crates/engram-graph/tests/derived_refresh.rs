//! The READER-INDEPENDENT publish: `Graph::refresh_stale_derived` brings
//! every cached adjacency table and membership snapshot that is behind its
//! source current, so the first read after a write-only burst finds them
//! current instead of paying for the whole burst (SF1's `contention` level:
//! 25 s on its 12th read, `measurements/official-sf1-paged-2026-08-29.md`).
//!
//! In-process here — the server's maintenance thread is the production
//! caller (`engram-server/tests/maintenance_refresh.rs` pins that path).
//! Each claim has its canary: the same read WITHOUT the refresh pays.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;

use engram_cypher::Value;
use engram_graph::{Dir, Graph};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const PERSONS: u64 = 200;
/// 100 messages per person: a 20,000-entry `(I, [HAS_CREATOR])` table, big
/// enough that re-reading the rows of the `BURST_PERSONS` a burst touches
/// (3,000 entries + 20 × (setup + ~250)) is well under half a rebuild — so
/// the refresh REPAIRS, which is the counter the claim is about. On a toy
/// table the cost gate rightly rebuilds instead.
const SEED_MESSAGES: u64 = 20_000;
/// The persons a burst's messages are spread over.
const BURST_PERSONS: u64 = 20;

/// Persons and their messages, `HAS_CREATOR` from message to person. The
/// `(I, [HAS_CREATOR])` table and the `:Message` / all-nodes membership
/// snapshots are built by one read each. Returns the graph, person ids, and
/// HAS_CREATOR's token.
fn snb_shaped() -> (Graph, Vec<u64>, u32) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_degree_table_after(0);
    let person = vec!["Person".to_string()];
    let message = vec!["Message".to_string()];
    let none = BTreeMap::new();
    let persons: Vec<u64> = (0..PERSONS)
        .map(|i| {
            let mut p = BTreeMap::new();
            p.insert("id".to_string(), Value::Int(i as i64));
            g.create_node(&person, &p).expect("person")
        })
        .collect();
    for i in 0..SEED_MESSAGES {
        let m = g.create_node(&message, &none).expect("message");
        g.create_rel(m, "HAS_CREATOR", persons[(i % PERSONS) as usize], &none)
            .expect("has_creator");
    }
    g.shared_store().seal();
    let tok = g.type_tokens_peek(&["HAS_CREATOR".to_string()]).expect("minted")[0];
    // The reads that build the structures a burst will leave stale.
    assert_eq!(in_row(&g, persons[0], tok), (SEED_MESSAGES / PERSONS) as usize);
    assert_eq!(g.members(Some("Message")).expect("members").len(), SEED_MESSAGES as usize);
    assert_eq!(
        g.members(None).expect("all").len(),
        (PERSONS + SEED_MESSAGES) as usize
    );
    (g, persons, tok)
}

fn in_row(g: &Graph, person: u64, tok: u32) -> usize {
    g.adjacent_slim(person, Dir::In, &Some(vec![tok])).len()
}

/// `n` message creates with their HAS_CREATOR, round-robin over the first
/// `BURST_PERSONS` persons, and NO read between.
fn burst(g: &Graph, persons: &[u64], n: u64) {
    let message = vec!["Message".to_string()];
    let none = BTreeMap::new();
    for i in 0..n {
        let m = g.create_node(&message, &none).expect("message");
        g.create_rel(m, "HAS_CREATOR", persons[(i % BURST_PERSONS) as usize], &none)
            .expect("has_creator");
    }
}

/// Person 0's in-degree after a burst of `n`.
fn expected_in_degree(n: u64) -> usize {
    (SEED_MESSAGES / PERSONS + n / BURST_PERSONS) as usize
}

fn count(trace: &engram_observe::Trace, k: &str) -> u64 {
    trace.counters().get(k).copied().unwrap_or(0)
}

/// The read after the burst, traced: how many in-rows person 0 has, and the
/// counters it fired.
fn traced_read(g: &Graph, person: u64, tok: u32) -> (usize, usize, engram_observe::Trace) {
    let ((rows, members), trace) = engram_observe::with_trace(|| {
        (
            in_row(g, person, tok),
            g.members(Some("Message")).expect("members").len(),
        )
    });
    (rows, members, trace)
}

#[test]
fn a_refresh_after_a_write_only_burst_leaves_the_next_read_nothing_to_do() {
    let (g, persons, tok) = snb_shaped();
    let n = 3_000;
    burst(&g, &persons, n);

    let (report, trace) = engram_observe::with_trace(|| g.refresh_stale_derived());
    assert!(
        report.adjacency_repaired >= 1,
        "the stale HAS_CREATOR table must be repaired by the refresh: {report:?}"
    );
    assert_eq!(report.adjacency_rebuilt, 0, "{report:?}");
    assert!(
        report.members_caught_up >= 2,
        ":Message and the all-nodes snapshot must be caught up: {report:?}"
    );
    assert_eq!(
        count(&trace, "graph.derived refreshed by maintenance"),
        (report.adjacency_repaired + report.members_caught_up) as u64
    );

    let (rows, members, trace) = traced_read(&g, persons[0], tok);
    assert_eq!(rows, expected_in_degree(n), "the read must see the burst");
    assert_eq!(members, (SEED_MESSAGES + n) as usize);
    assert_eq!(count(&trace, "graph.adjacency tables built"), 0, "{:?}", trace.counters());
    assert_eq!(count(&trace, "graph.adjacency tables repaired"), 0, "{:?}", trace.counters());
    assert!(count(&trace, "graph.adjacency tables reused") >= 1);
    assert_eq!(count(&trace, "graph.membership snapshots caught up"), 0);
    assert_eq!(count(&trace, "graph.membership snapshots built"), 0);
    assert!(
        count(&trace, "graph.membership snapshots still current")
            + count(&trace, "graph.membership snapshots current")
            >= 1
    );

    // Idempotent: nothing is stale now.
    assert!(!g.refresh_stale_derived().any(), "a second refresh must find nothing stale");
}

/// The canary: the SAME burst with no refresh, and the read pays — repairs
/// the table and catches the snapshots up itself.
#[test]
fn without_the_refresh_the_next_read_pays_for_the_burst() {
    let (g, persons, tok) = snb_shaped();
    burst(&g, &persons, 3_000);
    let (rows, _, trace) = traced_read(&g, persons[0], tok);
    assert_eq!(rows, expected_in_degree(3_000));
    assert!(
        count(&trace, "graph.adjacency tables repaired") + count(&trace, "graph.adjacency tables built")
            >= 1,
        "the reader must have done the work the refresh would have: {:?}",
        trace.counters()
    );
    assert!(count(&trace, "graph.membership snapshots caught up") >= 1);
}

/// With incremental caches OFF (every write invalidates everything) the
/// refresh is a deliberate no-op: it would rebuild the world on every tick.
#[test]
fn the_refresh_is_a_no_op_with_incremental_caches_off() {
    let (g, persons, _tok) = snb_shaped();
    g.set_incremental_caches(false);
    burst(&g, &persons, 100);
    assert!(!g.refresh_stale_derived().any());
}

/// The refresh refreshes what readers USE — it does not build tables nobody
/// asked for. A type never read has no cached table before or after.
#[test]
fn the_refresh_does_not_warm_unbuilt_structures() {
    let (g, persons, _tok) = snb_shaped();
    let none = BTreeMap::new();
    g.create_rel(persons[0], "KNOWS", persons[1], &none).expect("knows");
    let (report, trace) = engram_observe::with_trace(|| g.refresh_stale_derived());
    // KNOWS's type epoch moved but no KNOWS table exists; the all-nodes
    // membership did not change (no node created); only the untyped tables,
    // if cached, would be behind — and none was built by the reads above.
    assert_eq!(count(&trace, "graph.adjacency tables built"), 0, "{report:?}");
}

/// The batched membership fold reaches the same membership the serial fold
/// does, at the graph level, after a burst folded through `members()`.
#[test]
fn batched_and_serial_membership_folds_agree() {
    let mut got: Vec<Vec<u64>> = Vec::new();
    for batch in [true, false] {
        let (g, persons, _tok) = snb_shaped();
        g.set_members_batch_fold(batch);
        burst(&g, &persons, 2_500);
        // Some removals too, so the `removed` overlay is exercised.
        let messages = g.members_ids(Some("Message")).expect("ids");
        for &m in messages.iter().step_by(37).take(40) {
            g.delete_node(m, true).expect("detach delete");
        }
        got.push((*g.members_ids(Some("Message")).expect("ids")).clone());
    }
    assert_eq!(got[0], got[1], "batched and serial folds disagree");
    assert_eq!(got[0].len(), (SEED_MESSAGES + 2_500 - 40) as usize);
}
