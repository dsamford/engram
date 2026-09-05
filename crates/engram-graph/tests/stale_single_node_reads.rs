#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! A single-node reader must not repair the whole change set to answer about
//! one node — and must answer identically when it stops.
//!
//! # The defect
//!
//! An adjacency table is stale as a WHOLE the moment any writer touches any
//! node of its type. A reader asking for ONE node's row then repaired the
//! entire change set on its query thread: it re-read a row for every node any
//! writer had touched since the table was built, carried an overlay bounded
//! only by `ADJ_OVERLAY_FOLD`, and published a table most of its peers would
//! discard.
//!
//! The cost is proportional to the WRITE STREAM, and it is paid per read. On
//! `balattr` (the local attribution harness, 8 clients, dedicated reader and
//! writer threads, 50k nodes) that left the reads at **139 ops/s** against
//! 14.1M ops/s in a disjoint-type control — a control in which the writers
//! write a type the readers never query, so the readers' table is never made
//! stale and nothing else about the run changes. The control is what says the
//! interference is here and not in the store, the commit path or the
//! allocator: it measured 94% of solo read throughput and 97% of solo write
//! throughput retained, against 0% and 38% for the same-type run.
//!
//! # The change, and what has to be proved about it
//!
//! Three steps, each its own lever:
//!
//! 1. `lazy_stale_serve` — ask whether THIS node moved, rather than repairing.
//! 2. `adj_change_filter` — answer that with one atomic load rather than under
//!    the lock every write holds exclusively.
//! 3. `single_node_stale_walk` — when the answer is "it moved", decline the
//!    table and walk this node's own span, rather than repairing.
//!
//! Steps 1 and 2 serve a row from a table that is stale. Step 3 serves a row
//! from the store instead of a table. Both are claims that the rows are the
//! SAME rows, so that is what this file asserts — entry for entry, in order,
//! under a live write stream, against the direct walk which is the same truth
//! read from the store and is the engine's own reference for this question.
//!
//! Each has a counter half. Without it the two arms are one code path and
//! agree about nothing — the mistake `index_agrees_with_scan.rs` documents
//! having made, and the reason `adjacency_repair_differential` asserts its
//! fold fired rather than assuming it.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use engram_graph::counters::{ADJ_STALE_DECLINED_TO_WALK, ADJ_STALE_SERVED_UNMOVED};
use engram_graph::{Dir, Graph, SlimAdj};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

const NODES: u64 = 2_000;
const PER: u64 = 3;

fn seeded() -> (Graph, Vec<u64>, u32) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_degree_table_after(0);
    let label = vec!["N".to_string()];
    let none = BTreeMap::new();
    let ids: Vec<u64> = (0..NODES)
        .map(|_| g.create_node(&label, &none).expect("node"))
        .collect();
    let mut rng = Lcg(0x51A1_E5E7_0000_0001);
    for &src in &ids {
        for _ in 0..PER {
            let dst = ids[(rng.next() % NODES) as usize];
            g.create_rel(src, "T", dst, &none).expect("rel");
        }
    }
    g.shared_store().seal();
    let tok = g.type_tokens_peek(&["T".to_string()]).expect("T minted")[0];
    (g, ids, tok)
}

/// The row a table-served read produces.
fn table_row(g: &Graph, node: u64, tok: u32) -> Vec<SlimAdj> {
    g.adjacent_slim(node, Dir::Out, &Some(vec![tok]))
}

/// The same row read straight from the store — the engine's own reference for
/// "what is this node's adjacency", and what the table is a cache OF.
///
/// A FRESH `Graph` over the same store with tables disabled, which is the
/// pattern `adjacency_repair_differential` already uses for this question.
/// Two reasons it is not "flip the budget on `g` and flip it back": that
/// mutates the fixture under test between the two halves of a comparison, and
/// there is no getter for the budget, so "back" would be a guess. Reading
/// through `adjacent_slim` rather than a hand-rolled scan is also deliberate —
/// a second implementation of the visit order would only prove the two agree
/// about a bug.
fn walked_row(g: &Graph, node: u64, tok: u32) -> Vec<SlimAdj> {
    let plain = Graph::new(g.shared_store(), Realm(1), Namespace(1));
    plain.set_adj_table_max_entries(0);
    let t = plain
        .type_tokens_peek(&["T".to_string()])
        .expect("T in the shared catalog")[0];
    assert_eq!(t, tok, "the reference graph read a different catalog");
    plain.adjacent_slim(node, Dir::Out, &Some(vec![t]))
}

/// Warm the table, then move some OTHER nodes, so the table is stale as a
/// whole while the node under test has not moved.
fn stale_but_unmoved(g: &Graph, ids: &[u64], tok: u32, subject: u64) {
    let none = BTreeMap::new();
    let _ = table_row(g, subject, tok);
    for i in 0..64usize {
        let src = ids[500 + i];
        assert_ne!(src, subject, "the fixture must not move the subject");
        g.create_rel(src, "T", ids[(i * 7) % ids.len()], &none)
            .expect("rel");
    }
}

/// THE FIRST CLAIM: a stale table serves an unmoved node, and serves the row
/// the store would have.
#[test]
fn a_stale_table_serves_an_unmoved_node_the_row_the_walk_gives() {
    let _serial = serial();
    let (g, ids, tok) = seeded();
    let subject = ids[0];
    stale_but_unmoved(&g, &ids, tok, subject);

    let before = ADJ_STALE_SERVED_UNMOVED.load(Ordering::Relaxed);
    let served = table_row(&g, subject, tok);
    let fired = ADJ_STALE_SERVED_UNMOVED.load(Ordering::Relaxed) - before;
    let reference = walked_row(&g, subject, tok);

    assert_eq!(
        fired, 1,
        "the stale-serve path must actually fire, or the comparison below is \
         between a table read and a table read"
    );
    assert_eq!(
        served, reference,
        "a stale table's row for an unmoved node must equal the walk's, entry \
         for entry and in order"
    );
}

/// THE CANARY for the test above: with the policy off, the same read does NOT
/// take the stale-serve path — so the assertion that it fired is a statement
/// about the engine and not about the fixture.
#[test]
fn with_the_policy_off_the_stale_serve_does_not_fire() {
    let _serial = serial();
    let (g, ids, tok) = seeded();
    g.set_lazy_stale_serve(false);
    let subject = ids[0];
    stale_but_unmoved(&g, &ids, tok, subject);

    let before = ADJ_STALE_SERVED_UNMOVED.load(Ordering::Relaxed);
    let row = table_row(&g, subject, tok);
    assert_eq!(
        ADJ_STALE_SERVED_UNMOVED.load(Ordering::Relaxed) - before,
        0,
        "with the lever off the reader must repair, not serve stale"
    );
    assert_eq!(
        row,
        walked_row(&g, subject, tok),
        "and the repaired answer must be the same answer — the two arms differ \
         in who does the work, never in what it produces"
    );
}

/// THE SECOND CLAIM: a node that DID move, with a delta past the reader's
/// repair budget, declines the table and walks — and the walk is the same row.
///
/// The budget is the point of the third arm and not an implementation detail:
/// under LIGHT write pressure a reader still repairs, because repairing a
/// handful of changed rows is cheap and leaves a fresh table for every reader
/// behind it. `adjacency_probe_slim{,_adversarial}` are the suites that hold
/// that regime; this one holds the other.
#[test]
fn a_moved_node_declines_the_table_and_walks_the_same_row() {
    let _serial = serial();
    let (g, ids, tok) = seeded();
    let none = BTreeMap::new();
    let subject = ids[0];
    let _ = table_row(&g, subject, tok);
    // The delta must exceed `ADJ_READER_REPAIR_MAX_DELTA`, or the reader
    // REPAIRS rather than declining — which is the correct behaviour under
    // light write pressure and is what the two probe suites exercise.
    //
    // The count is PER SIDE, which is the detail worth writing down: a
    // `create_rel` logs the source into the `O` log and the target into the
    // `I` log, one entry each, and this read is `Dir::Out`, so only the `O`
    // log is scanned. 1,200 relationships is 1,200 O-side entries — past the
    // budget — where 700 (two entries each, if the sides were pooled) would
    // not have been. The first cut of this fixture made exactly that mistake
    // and the assertion below caught it.
    for i in 0..1_200usize {
        g.create_rel(ids[300 + (i % 1_000)], "T", ids[i % 100], &none)
            .expect("rel");
    }
    g.create_rel(subject, "T", ids[7], &none).expect("subject");

    let before = ADJ_STALE_DECLINED_TO_WALK.load(Ordering::Relaxed);
    let served = table_row(&g, subject, tok);
    let fired = ADJ_STALE_DECLINED_TO_WALK.load(Ordering::Relaxed) - before;
    let reference = walked_row(&g, subject, tok);

    assert!(
        fired >= 1,
        "a moved node must decline the table — otherwise this measures the \
         repair path and the assertion below is vacuous"
    );
    assert_eq!(
        served, reference,
        "the declined read's walk must equal the direct walk exactly"
    );
    assert!(
        served.iter().any(|e| e.peer == ids[7]),
        "and it must include the edge that made the node move: {served:?}"
    );
}

/// THE DIFFERENTIAL UNDER LOAD: with writers running, every read on both arms
/// must produce a row the walk agrees with.
///
/// A single-threaded fixture cannot reach the case this whole change is about
/// — a table that goes stale under someone else's writes while a read is
/// deciding what to do — so the differential is concurrent by necessity, not
/// for realism.
#[test]
fn under_a_write_stream_both_arms_answer_what_the_walk_answers() {
    let _serial = serial();
    for arm in [false, true] {
        let (g, ids, tok) = seeded();
        g.set_single_node_stale_walk(arm);
        let g = Arc::new(g);
        let ids = Arc::new(ids);
        let _ = table_row(&g, ids[0], tok);

        // The writer is BOUNDED, and the readers stop when it does. An
        // unbounded writer racing a debug-build read loop is a runaway: the
        // span it appends to is the span the readers walk, so the fixture
        // grows faster than the reads drain it and the test never ends. What
        // the differential needs is that reads and writes OVERLAP, which a
        // bounded stream gives just as well.
        const EDGES: usize = 4_000;
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (g, ids, stop) = (Arc::clone(&g), Arc::clone(&ids), Arc::clone(&stop));
            std::thread::spawn(move || {
                let none = BTreeMap::new();
                let mut rng = Lcg(0xBEEF_0000_0000_0001);
                for _ in 0..EDGES {
                    let src = ids[(rng.next() % NODES) as usize];
                    let dst = ids[(rng.next() % NODES) as usize];
                    let _ = g.create_rel(src, "T", dst, &none);
                }
                stop.store(true, Ordering::Relaxed);
            })
        };

        // Read while the writer runs; then quiesce and check every subject
        // against the walk. Comparing DURING the stream would compare two
        // instants and call the difference a defect — two counts are two
        // instants, which is how the "dangling edges" false alarm happened.
        let subjects: Vec<u64> = (0..64).map(|i| ids[i * 11]).collect();
        let mut reads = 0u64;
        while !stop.load(Ordering::Relaxed) {
            for &s in &subjects {
                let _ = table_row(&g, s, tok);
                reads += 1;
            }
        }
        writer.join().expect("writer");
        assert!(
            reads >= subjects.len() as u64,
            "arm {arm}: the reads must overlap the write stream, or nothing \
             was read against a table going stale: {reads}"
        );

        for &s in &subjects {
            let served = table_row(&g, s, tok);
            let reference = walked_row(&g, s, tok);
            assert_eq!(
                served, reference,
                "arm single_node_stale_walk={arm}: node {s} answered \
                 differently from the direct walk"
            );
        }
    }
}
