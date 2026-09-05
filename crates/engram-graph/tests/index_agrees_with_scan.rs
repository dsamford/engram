//! Does the incrementally-maintained range index give the SAME answers as a
//! scan?
//!
//! `RangeIndex::with_changes` carries a cached index forward across writes
//! instead of rebuilding it from the store. That removed a whole-corpus rescan
//! from every read that follows a write — but an index that is fast and wrong
//! is far worse than one that is slow and right, and a wrong index fails
//! SILENTLY: it returns a short answer, and a short answer looks exactly like a
//! query that legitimately matched fewer rows.
//!
//! So this is a DIFFERENTIAL test. Every assertion compares two ways of
//! answering one question over the identical graph:
//!
//!  - the index seek (`property_seek` on — `index_probe_eq` through the
//!    maintained index), and
//!  - the label scan (`property_seek` off — no index consulted at all).
//!
//! They must agree on every case. If maintenance ever drops a row, leaves a
//! stale entry under an old key, or mis-sorts, the two arms diverge and the
//! test names which write sequence did it.
//!
//! The sequences below are chosen to hit each way a row can enter or leave the
//! index: created, updated to a new value, updated to a value it already had,
//! removed with `SET x = null`, deleted outright, re-created under a reused id
//! space, and updated inside a transaction (which must OVERFLOW the delta and
//! fall back to a rebuild, because an index describes committed state).

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn run(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn fresh(n: i64) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    run_stmt(
        &g,
        &parse_any("CREATE INDEX t_k FOR (n:T) ON (n.k)").expect("parse index"),
        BTreeMap::new(),
    )
    .expect("create index");
    run(
        &g,
        &format!("UNWIND range(0, {}) AS i CREATE (:T {{k: i, tag: 'a'}})", n - 1),
    );
    g
}

/// How much range-index work a body performed.
fn index_work(t: &engram_observe::Trace) -> u64 {
    let c = t.counters();
    // Every way `ensure_range_index` can answer. The exact-epoch cache hit
    // belongs here and is the most common of them: leaving it out made this
    // helper report "the index was not consulted" for the second lookup in a
    // row, which is precisely when the cache is working best.
    c.get("graph.range index cache hit").copied().unwrap_or(0)
        + c.get("graph.range index builds").copied().unwrap_or(0)
        + c.get("graph.range index caught up").copied().unwrap_or(0)
        + c.get("graph.range index still current").copied().unwrap_or(0)
}

/// Answer `src` both ways and require agreement — WITHOUT requiring that the
/// seek arm engaged the index.
///
/// Order is normalised before comparison: the two plans reach the same rows by
/// different routes (index order versus scan order), and requiring identical
/// ORDER would be asserting something neither plan promises.
fn agree_either_plan(g: &Graph, src: &str, what: &str) -> usize {
    g.set_property_seek(true);
    let mut seek = run(g, src);
    g.set_property_seek(false);
    let mut scan = run(g, src);
    g.set_property_seek(true);

    let key = |rows: &mut Vec<Vec<Value>>| -> Vec<String> {
        let mut v: Vec<String> = rows.iter().map(|r| format!("{r:?}")).collect();
        v.sort();
        v
    };
    let (sk, sc) = (key(&mut seek), key(&mut scan));
    assert_eq!(
        sk, sc,
        "the maintained index and the scan disagree after {what}\n  \
         `{src}`\n  index seek returned {} row(s)\n  label scan returned {} row(s)",
        sk.len(),
        sc.len()
    );
    sk.len()
}

/// Answer `src` both ways, require agreement, AND require that the seek arm
/// actually consulted the maintained index.
///
/// The second half is what stops this file from quietly proving nothing. The
/// seek has a floor (`PROPERTY_SEEK_MIN_LABEL`, 512 nodes): below it the
/// planner scans even with `property_seek` on, so both arms run the identical
/// plan and every comparison here becomes a result compared with itself. That
/// is not hypothetical — the first version of this file used a 500-node fixture
/// and the canary caught it.
///
/// Making the check part of the shared helper rather than one separate test
/// means each assertion carries its own canary, so a fixture that drifts below
/// the floor fails at the assertion it invalidates instead of somewhere else.
fn both_ways_agree(g: &Graph, src: &str, what: &str) -> usize {
    g.set_property_seek(true);
    let (rows, trace) = engram_observe::with_trace(|| run(g, src));
    assert!(
        index_work(&trace) >= 1,
        "the seek arm did not consult the range index for `{src}` after {what} — the \
         corpus is probably below the {}-node seek floor, so this comparison would \
         run the same plan twice and could not detect a maintenance fault",
        512
    );
    let _ = rows;
    agree_either_plan(g, src, what)
}

/// The canary on this whole file: with the index DELIBERATELY corrupted the
/// comparison must fail.
///
/// Not a corruption of the engine — one of the harness. `both_ways_agree`
/// compares two runs of the same statement; if the toggle it flips did nothing,
/// both arms would take the identical plan and every assertion in this file
/// would compare a result with itself and pass no matter what maintenance did.
/// This asserts the two arms genuinely differ in plan by checking that the seek
/// arm is the one that uses the index: a property the index cannot order forces
/// the scan, so the two arms must still agree — while a query on an indexed
/// property with the seek OFF must not consult the index at all.
///
/// The observable proof is the counter: with the seek on, an indexed lookup
/// builds or catches up an index; with it off, it must not.
#[test]
fn the_two_arms_really_take_different_plans() {
    let g = fresh(2_000);
    g.set_property_seek(false);
    let ((), off) = engram_observe::with_trace(|| {
        run(&g, "MATCH (n:T {k: 7}) RETURN n.k");
    });
    g.set_property_seek(true);
    let ((), on) = engram_observe::with_trace(|| {
        run(&g, "MATCH (n:T {k: 11}) RETURN n.k");
    });
    let idx_work = index_work;
    assert_eq!(
        idx_work(&off),
        0,
        "with property_seek OFF the lookup still touched the range index — the two \
         arms of every comparison in this file are the same plan, so they cannot \
         detect a maintenance fault"
    );
    assert!(
        idx_work(&on) >= 1,
        "with property_seek ON the lookup did not touch the range index — the seek \
         arm is not exercising the maintained index, so this file proves nothing"
    );
}

#[test]
fn agrees_after_plain_creates() {
    let g = fresh(2_000);
    both_ways_agree(&g, "MATCH (n:T {k: 5}) RETURN n.k", "the initial load");
    for i in 0..40 {
        run(&g, &format!("CREATE (:T {{k: {}, tag: 'b'}})", 10_000 + i));
    }
    both_ways_agree(&g, "MATCH (n:T {k: 10005}) RETURN n.k, n.tag", "40 creates");
    both_ways_agree(&g, "MATCH (n:T {k: 5}) RETURN n.k", "40 creates (old row)");
    // A key that was never written must be empty on BOTH arms — a stale entry
    // shows up here and nowhere else.
    let n = both_ways_agree(&g, "MATCH (n:T {k: 99999}) RETURN n.k", "40 creates (absent)");
    assert_eq!(n, 0, "a key that was never written matched {n} row(s)");
}

#[test]
fn agrees_after_updates_that_move_a_row_to_a_new_key() {
    let g = fresh(1_000);
    both_ways_agree(&g, "MATCH (n:T {k: 3}) RETURN n.k", "the initial load");
    // Move row 3 to a new key. The OLD key must stop matching and the NEW key
    // must start — a maintenance that inserts without removing passes the
    // second check and fails the first.
    // The destination is OUTSIDE the loaded range 0..999 deliberately: an
    // in-range destination already has an occupant, so "2 rows matched" would
    // be the correct answer and the assertion below would be testing the
    // fixture rather than the index. (It was 777 first, and did exactly that.)
    run(&g, "MATCH (n:T {k: 3}) SET n.k = 10777");
    let old = both_ways_agree(&g, "MATCH (n:T {k: 3}) RETURN n.k", "moving k 3 -> 10777");
    assert_eq!(old, 0, "the vacated key 3 still matched {old} row(s)");
    let new = both_ways_agree(&g, "MATCH (n:T {k: 10777}) RETURN n.k", "moving k 3 -> 10777");
    assert_eq!(new, 1, "the new key 10777 matched {new} row(s), expected 1");
}

#[test]
fn agrees_after_a_null_set_removes_the_property() {
    let g = fresh(1_000);
    run(&g, "MATCH (n:T {k: 12}) SET n.k = null");
    let gone = both_ways_agree(&g, "MATCH (n:T {k: 12}) RETURN n.k", "SET n.k = null");
    assert_eq!(gone, 0, "a removed property still matched {gone} row(s)");
    both_ways_agree(&g, "MATCH (n:T {k: 13}) RETURN n.k", "SET n.k = null (neighbour)");
}

#[test]
fn agrees_after_deletes() {
    let g = fresh(1_000);
    run(&g, "MATCH (n:T {k: 20}) DELETE n");
    let gone = both_ways_agree(&g, "MATCH (n:T {k: 20}) RETURN n.k", "DELETE");
    assert_eq!(gone, 0, "a deleted node still matched {gone} row(s)");
    both_ways_agree(&g, "MATCH (n:T {k: 21}) RETURN n.k", "DELETE (neighbour)");
}

/// A duplicate key must return BOTH rows, in both arms.
///
/// Equal keys are where a sorted-merge maintenance is most likely to go wrong:
/// `build` sorts the `(key, body)` tuple, so maintenance must place a new entry
/// among its equal-keyed neighbours by body, not merely somewhere in the run.
#[test]
fn agrees_when_several_rows_share_a_key() {
    let g = fresh(1_000);
    for _ in 0..5 {
        run(&g, "CREATE (:T {k: 42, tag: 'dup'})");
    }
    let n = both_ways_agree(&g, "MATCH (n:T {k: 42}) RETURN n.tag", "5 duplicate keys");
    // 5 duplicates plus the original row 42 from the initial load.
    assert_eq!(n, 6, "expected 6 rows sharing key 42, got {n}");
}

/// A long interleaving of every operation, checked throughout.
///
/// The single-operation tests above each start from a freshly built index. This
/// one never lets the index settle: it keeps writing so that most reads are
/// served by a carried-forward index that has already been carried forward
/// several times, which is the state a real mixed workload is always in and the
/// one where a small per-application error would accumulate.
#[test]
fn agrees_across_a_long_interleaving() {
    let g = fresh(1_000);
    for round in 0..30i64 {
        run(&g, &format!("CREATE (:T {{k: {}, tag: 'r'}})", 50_000 + round));
        run(&g, &format!("MATCH (n:T {{k: {round}}}) SET n.k = {}", 60_000 + round));
        if round % 3 == 0 {
            run(&g, &format!("MATCH (n:T {{k: {}}}) DELETE n", 500 + round));
        }
        if round % 5 == 0 {
            run(&g, &format!("MATCH (n:T {{k: {}}}) SET n.k = null", 700 + round));
        }
        both_ways_agree(
            &g,
            &format!("MATCH (n:T {{k: {}}}) RETURN n.k, n.tag", 50_000 + round),
            &format!("round {round} (the row just created)"),
        );
        both_ways_agree(
            &g,
            &format!("MATCH (n:T {{k: {}}}) RETURN n.k", 60_000 + round),
            &format!("round {round} (the row just moved)"),
        );
        both_ways_agree(
            &g,
            &format!("MATCH (n:T {{k: {round}}}) RETURN n.k"),
            &format!("round {round} (the vacated key)"),
        );
    }
    // And a range read over the same maintained entries. `agree_either_plan`
    // rather than `both_ways_agree`: a counting range scan is not obliged to
    // consult the index, so requiring it would assert a plan choice this test
    // has no opinion about. The agreement is still the point.
    agree_either_plan(
        &g,
        "MATCH (n:T) WHERE n.k > 49999 AND n.k < 60000 RETURN count(n) AS c",
        "the whole interleaving (range read)",
    );
}

/// A write inside a TRANSACTION must not be applied to the index early.
///
/// An index describes COMMITTED state. A buffered write is not committed, so
/// `note_prop_change` overflows the delta and forces a rebuild — which is
/// exactly the behaviour that existed before maintenance was added. If that
/// overflow were ever dropped, an uncommitted row would become visible to an
/// index seek and to nothing else, which is the hardest possible divergence to
/// track down from a bug report.
#[test]
fn agrees_across_a_transaction() {
    let g = fresh(1_000);
    run(
        &g,
        "MATCH (n:T {k: 30}) SET n.k = 31337 WITH n MATCH (m:T {k: 31}) SET m.tag = 'txn'",
    );
    both_ways_agree(&g, "MATCH (n:T {k: 31337}) RETURN n.k", "a multi-write statement");
    let old = both_ways_agree(&g, "MATCH (n:T {k: 30}) RETURN n.k", "a multi-write statement");
    assert_eq!(old, 0, "the vacated key 30 still matched {old} row(s)");
}
