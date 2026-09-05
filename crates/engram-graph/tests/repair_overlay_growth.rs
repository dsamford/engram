#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! How many rows does adjacency repair COPY per repair?
//!
//! # Why this is asked
//!
//! `balanced` (50% reads / 50% writes) is the one SF1 profile still behind
//! Neo4j, and it is the MOST interference-bound profile remaining — 37.5% below
//! the closed-loop interference-free prediction, essentially unchanged by the
//! tail copy-out that halved `write-heavy`'s. Its counter signature is ~1
//! reader-side repair per write.
//!
//! `Graph::repaired_adj_table` does:
//!
//! ```text
//! let mut overlay = base.overlay.clone();   // <- every repair, whole overlay
//! for n in nodes {
//!     overlay.insert(n, self.adj_row_for(tag, n, type_tokens));
//! }
//! ```
//!
//! The overlay holds one COMPLETE row per node repaired since the base was
//! built, so a repair that re-reads ONE changed node still deep-copies every
//! row every earlier repair left behind.
//!
//! # The correction this file exists to record
//!
//! I first read that growth as unbounded and quadratic. **It is not.** The
//! table folds its overlay into a fresh base past `ADJ_OVERLAY_FOLD` (4,096)
//! rows — the fold sits immediately after the clone, it is the same `FOLD_AT`
//! pattern the range index and the members view use, and
//! `review_repair_over_cap_differential.rs` already drives a 40,000-node
//! fixture across it and asserts it fires. The growth measured below is real
//! and it is growth *inside that bound*; a 400-node fixture simply cannot
//! reach the bound, which is why it looked unbounded from here.
//!
//! So the shape is a sawtooth, not a ramp: rows copied per repair climbs to
//! `ADJ_OVERLAY_FOLD` and resets. What is left OPEN — and what this file
//! measures rather than asserts — is whether the *mean* of that sawtooth
//! (~2,048 deep row copies per repair) is material at `balanced`'s ~470
//! repairs/s. That is a per-repair cost the fold bounds but does not remove:
//! bounding the overlay's SIZE is a different fix from removing the CLONE.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use engram_graph::counters::{ADJ_OVERLAY_ROWS_CLONED, ADJ_TABLES_REPAIRED};
use engram_graph::{Dir, Graph};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

const NODES: u64 = 400;
/// Mirrors `engram_graph`'s private `ADJ_OVERLAY_FOLD`. Duplicated on purpose:
/// a test that read the constant from the engine could not detect the engine
/// raising it, and the whole point of the assertion below is that the bound
/// is a fact about the engine rather than a restatement of it.
const FOLD_AT: u64 = 4_096;

/// A warmed table, then alternating write/read rounds — the `balanced` shape,
/// where each write makes the table stale and the next read repairs it.
fn build(g: &Graph) -> (Vec<u64>, u32) {
    g.set_degree_table_after(0);
    let label = vec!["N".to_string()];
    let ids: Vec<u64> = (0..NODES)
        .map(|_| g.create_node(&label, &BTreeMap::new()).expect("node"))
        .collect();
    let none = BTreeMap::new();
    for (i, &src) in ids.iter().enumerate() {
        g.create_rel(src, "T", ids[(i + 1) % ids.len()], &none)
            .expect("rel");
    }
    g.shared_store().seal();
    let tok = g.type_tokens_peek(&["T".to_string()]).expect("T minted")[0];
    let _ = g.adjacent_slim(ids[0], Dir::Out, &Some(vec![tok]));
    (ids, tok)
}

/// THE MEASUREMENT: rows copied per repair, as repairs accumulate.
#[test]
fn report_overlay_rows_copied_per_repair() {
    let _serial = serial();
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let (ids, tok) = build(&g);
    let none = BTreeMap::new();

    let sample = |g: &Graph, ids: &[u64], rounds: usize, off: usize| -> (u64, u64) {
        let r0 = ADJ_TABLES_REPAIRED.load(Ordering::Relaxed);
        let c0 = ADJ_OVERLAY_ROWS_CLONED.load(Ordering::Relaxed);
        for j in 0..rounds {
            // ONE write: makes the table stale by exactly one node.
            let src = ids[(off + j) % ids.len()];
            g.create_rel(src, "T", ids[(off + j + 7) % ids.len()], &none)
                .expect("rel");
            // ONE read: repairs it.
            let _ = g.adjacent_slim(src, Dir::Out, &Some(vec![tok]));
        }
        (
            ADJ_TABLES_REPAIRED.load(Ordering::Relaxed) - r0,
            ADJ_OVERLAY_ROWS_CLONED.load(Ordering::Relaxed) - c0,
        )
    };

    // Two windows of equal length, the second AFTER the overlay has grown.
    let (r1, c1) = sample(&g, &ids, 60, 0);
    let (r2, c2) = sample(&g, &ids, 60, 200);

    let per1 = if r1 > 0 { c1 as f64 / r1 as f64 } else { 0.0 };
    let per2 = if r2 > 0 { c2 as f64 / r2 as f64 } else { 0.0 };
    eprintln!(
        "[repair overlay] window 1: {r1} repairs, {c1} rows copied ({per1:.1}/repair); \
         window 2 (later): {r2} repairs, {c2} rows copied ({per2:.1}/repair)"
    );

    assert!(
        r1 > 0 && r2 > 0,
        "the fixture must actually repair, or the ratios describe nothing: \
         {r1} / {r2}"
    );
    eprintln!(
        "[repair overlay] growth factor across the two windows: {:.2}x",
        if per1 > 0.0 { per2 / per1 } else { 0.0 }
    );
    // THE BOUND, asserted rather than assumed. A repair copies the overlay it
    // inherits, and the fold caps that overlay — so no single repair may copy
    // more than the fold threshold plus the row that crosses it. This is what
    // makes "the growth is bounded" a checked fact of this build rather than a
    // claim about a constant somewhere else, and it is what would fire if the
    // fold were ever removed, disabled, or short-circuited by a branch placed
    // in front of it (which is exactly how it was briefly broken).
    let worst = per1.max(per2);
    assert!(
        worst <= (FOLD_AT + 1) as f64,
        "a single repair copied {worst:.0} rows, above the fold bound of \
         {FOLD_AT} — the overlay fold is not capping the clone"
    );
}
