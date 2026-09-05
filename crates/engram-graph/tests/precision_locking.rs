#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! §7 — precision locking closes the phantom the engine admits today.
//!
//! # The anomaly, stated precisely
//!
//! Two transactions. T1 does `MATCH (n:P {tag: 'x'}) ... ` — it enumerates a
//! SET — and then writes on the basis of what it found. T2 commits a NEW node
//! satisfying that same pattern and commits first. T1 never read T2's node,
//! because it did not exist at T1's snapshot, so the node is not in T1's read
//! set and read-set validation has nothing to test. T1 commits.
//!
//! The result is not serialisable: in any serial order T1 either sees T2's node
//! or T2 does not exist yet, and T1's answer matches neither.
//! `docs/concurrency-direction.md` records this under *Known limitations*, so
//! it is a documented gap rather than a surprise — and this file is where it
//! stops being one.
//!
//! # Why the OFF arm is a real test and not a formality
//!
//! It PINS the anomaly. A named, asserted statement of what the engine does
//! today is what makes the ON arm's assertion mean something: without it, "the
//! ON arm aborts" is compatible with an engine that aborts everything.
//!
//! # What §7 does not claim
//!
//! Coverage is incremental. An unbound scan whose pattern is a label set plus
//! row-independent property equalities is checked. A correlated property map, a
//! relationship pattern, an inequality in a WHERE — none of those are, and each
//! keeps exactly today's read-set rule. That is asserted here too, because a
//! guarantee whose limits are not written down gets read as unlimited.

use std::collections::BTreeMap;

use engram_cypher::parse_statement;
use engram_graph::{Graph, GraphTxn, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn graph_on(store: &Store) -> Graph {
    Graph::new(store.clone(), Realm(1), Namespace(1))
}

/// Run a statement inside `txn`, leaving the transaction OPEN.
fn run_in(g: &Graph, txn: GraphTxn, src: &str) -> (GraphTxn, usize) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse {src}: {e}"));
    let (txn, r) = g.with_txn(txn, || run_query(g, &q, BTreeMap::new()));
    let rows = r.unwrap_or_else(|e| panic!("run {src}: {e:?}")).rows.len();
    (txn, rows)
}

/// A whole statement in its own transaction, committed.
fn stmt(g: &Graph, src: &str) {
    let txn = g.open_txn();
    let (txn, _) = run_in(g, txn, src);
    g.commit_owned(txn)
        .unwrap_or_else(|e| panic!("commit {src}: {e:?}"));
}

/// THE ANOMALY, staged deterministically.
///
/// `T1` scans `(:P {tag: 'x'})` and then writes. Between its scan and its
/// commit, `T2` commits a NEW `(:P {tag: 'x'})`. Returns whether `T1`
/// committed.
///
/// One store, two graphs, so the two transactions are genuinely independent —
/// a single graph's `ACTIVE_TXN` is per-thread and one of the two would
/// silently become the other's buffered writes.
fn stage_phantom(precision: bool) -> (bool, u64) {
    let store = Store::new();
    let t1 = graph_on(&store);
    let t2 = graph_on(&store);
    t1.set_precision_locking(precision);
    t2.set_precision_locking(precision);

    // Seed: two matching nodes and one that does not match.
    stmt(&t1, "CREATE (:P {tag: 'x', n: 1})");
    stmt(&t1, "CREATE (:P {tag: 'x', n: 2})");
    stmt(&t1, "CREATE (:P {tag: 'y', n: 3})");
    stmt(&t1, "CREATE (:Total {v: 0})");

    // T1: scan the set, then write on the basis of what it saw.
    let txn = t1.open_txn();
    let (txn, seen) = run_in(&t1, txn, "MATCH (n:P {tag: 'x'}) RETURN n");
    assert_eq!(seen, 2, "T1 must see exactly the two seeded matches");

    // T2: a NEW node satisfying T1's pattern, committed in between.
    stmt(&t2, "CREATE (:P {tag: 'x', n: 99})");

    // T1 writes its conclusion and commits.
    let (txn, _) = run_in(&t1, txn, "MATCH (t:Total) SET t.v = 2");
    let committed = t1.commit_owned(txn).is_ok();
    let phantoms = engram_store::PHANTOM_CONFLICTS.load(std::sync::atomic::Ordering::Relaxed);
    (committed, phantoms)
}

/// The counter is process-wide, so the arms must not interleave.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// BOTH ARMS, as one statement: the anomaly is admitted today and closed by
/// §7. Asserted together so neither half can drift into meaninglessness.
#[test]
fn the_phantom_is_admitted_today_and_closed_by_precision_locking() {
    let _serial = serial();

    let before_off = engram_store::PHANTOM_CONFLICTS.load(std::sync::atomic::Ordering::Relaxed);
    let (off_committed, off_after) = stage_phantom(false);
    let off_phantoms = off_after - before_off;

    let before_on = engram_store::PHANTOM_CONFLICTS.load(std::sync::atomic::Ordering::Relaxed);
    let (on_committed, on_after) = stage_phantom(true);
    let on_phantoms = on_after - before_on;

    eprintln!(
        "[precision locking] T1 committed over a phantom: {off_committed} with the \
         lever off, {on_committed} with it on ({off_phantoms} / {on_phantoms} \
         predicate aborts)"
    );
    assert!(
        off_committed,
        "THE PINNED ANOMALY: without precision locking, T1 commits a decision \
         made from a set it did not see the whole of. If this ever fails, the \
         engine's guarantee changed and the ON arm below proves nothing new."
    );
    assert_eq!(
        off_phantoms, 0,
        "and it must abort for no predicate reason at all, or the arms are not \
         isolating the mechanism"
    );
    assert!(
        !on_committed,
        "WITH precision locking, the row T2 committed satisfies the predicate \
         T1 depended on, so T1 must abort"
    );
    assert_eq!(
        on_phantoms, 1,
        "and it must abort for THAT reason — a conflict counted elsewhere means \
         something other than the predicate caused it: {on_phantoms}"
    );
}

/// §7 MUST NOT ABORT MORE where there is no phantom.
///
/// The whole risk of a predicate validator is over-abortion: a restriction that
/// admits rows the pattern would have rejected turns every concurrent write in
/// the store into a conflict, and the isolation upgrade arrives as a throughput
/// collapse nobody asked for.
#[test]
fn a_non_matching_concurrent_write_does_not_abort() {
    let _serial = serial();
    let store = Store::new();
    let t1 = graph_on(&store);
    let t2 = graph_on(&store);
    t1.set_precision_locking(true);
    t2.set_precision_locking(true);

    stmt(&t1, "CREATE (:P {tag: 'x', n: 1})");
    stmt(&t1, "CREATE (:Total {v: 0})");

    let txn = t1.open_txn();
    let (txn, seen) = run_in(&t1, txn, "MATCH (n:P {tag: 'x'}) RETURN n");
    assert_eq!(seen, 1);

    // Three concurrent commits, none of which satisfies T1's pattern:
    // a different label, a different property value, and a missing property.
    stmt(&t2, "CREATE (:Q {tag: 'x', n: 50})");
    stmt(&t2, "CREATE (:P {tag: 'y', n: 51})");
    stmt(&t2, "CREATE (:P {n: 52})");

    let (txn, _) = run_in(&t1, txn, "MATCH (t:Total) SET t.v = 1");
    assert!(
        t1.commit_owned(txn).is_ok(),
        "none of the three concurrent nodes satisfies (:P {{tag: 'x'}}), so \
         none of them is a phantom for T1 — a validator that aborts here is \
         reading its own restriction wrong, and would turn every unrelated \
         write in the store into a conflict"
    );
}

/// A concurrent DELETE of a row T1 never read is not a phantom either.
///
/// A phantom is a row that APPEARED. A tombstone is a row that went away, and
/// the case where T1 read it before it went is already read-set validation's.
/// Testing the tombstone would abort on a deletion T1 provably did not depend
/// on — and there is no record left to test the predicate against anyway.
#[test]
fn a_concurrent_delete_of_an_unread_row_is_not_a_phantom() {
    let _serial = serial();
    let store = Store::new();
    let t1 = graph_on(&store);
    let t2 = graph_on(&store);
    t1.set_precision_locking(true);
    t2.set_precision_locking(true);

    stmt(&t1, "CREATE (:P {tag: 'x', n: 1})");
    stmt(&t1, "CREATE (:P {tag: 'z', n: 2})");
    stmt(&t1, "CREATE (:Total {v: 0})");

    let txn = t1.open_txn();
    let (txn, seen) = run_in(&t1, txn, "MATCH (n:P {tag: 'x'}) RETURN n");
    assert_eq!(seen, 1, "T1 reads only the tag-x node");

    // T2 deletes the node T1 did NOT read.
    stmt(&t2, "MATCH (n:P {tag: 'z'}) DELETE n");

    let (txn, _) = run_in(&t1, txn, "MATCH (t:Total) SET t.v = 1");
    assert!(
        t1.commit_owned(txn).is_ok(),
        "a deletion of a row outside T1's predicate is not a phantom for T1"
    );
}

/// THE FALLBACK, stated rather than assumed: a pattern the extractor cannot
/// represent keeps today's rule, and the phantom stays admitted.
///
/// This is the honest limit of the item as shipped. Writing it down as a
/// passing test — rather than leaving it as prose — is what stops the guarantee
/// being read as "serialisable", which it is not yet.
#[test]
fn an_unrepresentable_predicate_falls_back_and_still_admits_its_phantom() {
    let _serial = serial();
    let store = Store::new();
    let t1 = graph_on(&store);
    let t2 = graph_on(&store);
    t1.set_precision_locking(true);
    t2.set_precision_locking(true);

    stmt(&t1, "CREATE (:P {n: 1})");
    stmt(&t1, "CREATE (:P {n: 5})");
    stmt(&t1, "CREATE (:Total {v: 0})");

    // An INEQUALITY in a WHERE — not a restriction set, so `extract` never
    // sees it and nothing is registered.
    let txn = t1.open_txn();
    let (txn, seen) = run_in(&t1, txn, "MATCH (n:P) WHERE n.n > 3 RETURN n");
    assert_eq!(seen, 1);

    stmt(&t2, "CREATE (:P {n: 9})");

    let (txn, _) = run_in(&t1, txn, "MATCH (t:Total) SET t.v = 1");
    assert!(
        t1.commit_owned(txn).is_ok(),
        "an inequality is beyond the extractor, so this predicate keeps \
         read-set validation and its phantom stays admitted — the limit of \
         §7 as shipped, pinned so it is a known limit rather than a surprise"
    );
}

/// AN OVERSIZED DELTA skips the predicate pass rather than truncating it.
///
/// The window holds up to `COMMIT_WINDOW_CAP` entries, and every one the guard
/// accepts costs a store read and a record decode UNDER THE COMMIT LATCH. Left
/// unbounded that is a convoy at the one serialisation point this whole
/// programme exists to clear, arriving as a latency collapse rather than a
/// wrong answer — the harder kind to attribute.
///
/// Past the cap the pass is SKIPPED, not truncated. Truncating would check some
/// predicates and silently not others while still reporting the commit as
/// validated. Skipping loses the coverage honestly and leaves read-set
/// validation, which is today's rule — coverage is what is allowed to degrade
/// here, never soundness.
///
/// The observable consequence is that the phantom comes BACK, which is exactly
/// what "coverage degraded" means and is asserted as such.
#[test]
fn an_oversized_delta_skips_the_pass_rather_than_truncating_it() {
    let _serial = serial();
    let store = Store::new();
    let t1 = graph_on(&store);
    let t2 = graph_on(&store);
    t1.set_precision_locking(true);
    t2.set_precision_locking(true);

    stmt(&t1, "CREATE (:P {tag: 'x', n: 1})");
    stmt(&t1, "CREATE (:Total {v: 0})");

    let txn = t1.open_txn();
    let (txn, seen) = run_in(&t1, txn, "MATCH (n:P {tag: 'x'}) RETURN n");
    assert_eq!(seen, 1);

    // The phantom, plus enough concurrent commits to push the delta past the
    // cap. Each statement writes several rows, so a few thousand statements
    // is comfortably over 4,096 keys.
    stmt(&t2, "CREATE (:P {tag: 'x', n: 99})");
    for i in 0..2_500i64 {
        stmt(&t2, &format!("CREATE (:Filler {{k: {i}}})"));
    }

    let before = engram_store::PHANTOM_CONFLICTS.load(std::sync::atomic::Ordering::Relaxed);
    let (txn, _) = run_in(&t1, txn, "MATCH (t:Total) SET t.v = 1");
    let committed = t1.commit_owned(txn).is_ok();
    let phantoms = engram_store::PHANTOM_CONFLICTS.load(std::sync::atomic::Ordering::Relaxed) - before;

    eprintln!(
        "[precision locking] oversized delta: committed={committed},          predicate aborts={phantoms}"
    );
    assert_eq!(
        phantoms, 0,
        "the pass must be SKIPPED — an abort here means it ran anyway and the          cap is not bounding the latched work it was added to bound"
    );
    assert!(
        committed,
        "and skipping means read-set validation stands alone, which is today's          rule: the transaction commits, the phantom is admitted again, and the          coverage loss is honest rather than hidden"
    );
}

/// The lever must reach the whole mechanism: with it OFF, nothing is even
/// recorded, so the guard is never installed and the commit path is exactly
/// what it was.
#[test]
fn the_lever_off_records_nothing() {
    let _serial = serial();
    let store = Store::new();
    let g = graph_on(&store);
    g.set_precision_locking(false);
    assert!(!g.precision_locking_enabled());

    stmt(&g, "CREATE (:P {tag: 'x'})");
    let before = engram_store::PHANTOM_CONFLICTS.load(std::sync::atomic::Ordering::Relaxed);
    let txn = g.open_txn();
    let (txn, _) = run_in(&g, txn, "MATCH (n:P {tag: 'x'}) RETURN n");
    let (txn, _) = run_in(&g, txn, "CREATE (:Other {k: 1})");
    assert!(g.commit_owned(txn).is_ok());
    assert_eq!(
        engram_store::PHANTOM_CONFLICTS.load(std::sync::atomic::Ordering::Relaxed),
        before,
        "the off arm must not reach the predicate validator at all"
    );
}
