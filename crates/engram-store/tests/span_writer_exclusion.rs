#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! THE WRITER-EXCLUSION PROBE, pinned.
//!
//! # What it measures and why
//!
//! `merge_span` takes ALL 64 tail shard read latches (`Tail::read_all`) and
//! holds them until it returns — across the k-way merge, the paged segments'
//! preads and BLAKE3 verification, and every visitor callback. A writer needs
//! exactly one of those shards, so for that whole window no write can enter the
//! tail.
//!
//! Crucially it is taken **only when the tail is non-empty**:
//!
//! ```text
//! let all = self.tail_has_versions().then(|| self.inner.tail.read_all());
//! ```
//!
//! That single condition explains the shape of engram's SF1 results against
//! Neo4j 5.26 — 1.49x-3.98x on the PURE profiles and 0.63x-0.75x on the MIXED
//! ones:
//!
//! - `read-only`: the tail drains at the seal, the branch is skipped, nothing
//!   is excluded (and there is no writer to exclude anyway).
//! - `write-only`: no span reads are issued, so nothing takes the latches.
//! - a MIX: `tail_has_versions()` is permanently true, so every read holds all
//!   64 and stops every writer for its duration.
//!
//! # Why this file exists
//!
//! The counters are the evidence for the change that removes this. A counter
//! that never moves is indistinguishable from one that is not wired up, and a
//! counter that always moves proves nothing about the asymmetry that is the
//! whole diagnosis. So both arms are asserted here, on the two store states
//! that produce them.
//!
//! # These tests run with `set_tail_span_copyout(false)`
//!
//! The copy-out is the fix and it is ON by default, so on the shipped
//! configuration no span read takes the latches at all and everything below
//! would read zero. The mechanism has not gone away — it is the FALLBACK, kept
//! for a range over the copy's row cap, since a bigger-than-RAM store cannot
//! copy a 17.26M-row span. This file pins the fallback's behaviour so the thing
//! the fix removes stays described, and `tail_span_copyout.rs` pins that the
//! fix removes it.

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::{
    SPAN_READS_EXCLUDING_WRITERS, SPAN_READS_LATCH_FREE, SPAN_ROWS_UNDER_LATCHES, Store,
    StoredValue,
};
use std::sync::atomic::Ordering;

/// The counters are process-wide, so the arms must not interleave.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn pfx() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::KV,
        partition: Partition(1),
    }
}

fn seed(s: &Store, n: u32) {
    for i in 0..n {
        let mut body = b"S".to_vec();
        body.extend_from_slice(&i.to_be_bytes());
        s.put(&pfx(), &body, StoredValue::Plain(vec![1]))
            .expect("put");
    }
}

/// One span read over the seeded prefix.
fn span(s: &Store) -> u64 {
    let mut seen = 0u64;
    s.for_each_key_span(&pfx(), b"S", u64::MAX, &mut |_body| {
        seen += 1;
        true
    });
    seen
}

/// BOTH ARMS, as one statement: the latches are taken when the tail holds rows
/// and skipped when it does not.
///
/// This is the asymmetry the whole diagnosis rests on. Asserted together
/// because either half alone is compatible with a broken probe: an
/// always-zero exclusion count looks like a pure-read workload, and an
/// always-nonzero one looks like a probe that ignores its condition.
#[test]
fn the_latches_are_held_only_when_the_tail_is_non_empty() {
    let _serial = serial();
    let s = Store::new();
    // The arm where the mechanism EXISTS — see the module header.
    s.set_tail_span_copyout(false);
    seed(&s, 200);

    // ── ARM 1: rows in the TAIL. Every span read excludes every writer.
    let (e0, f0, r0) = (
        SPAN_READS_EXCLUDING_WRITERS.load(Ordering::Relaxed),
        SPAN_READS_LATCH_FREE.load(Ordering::Relaxed),
        SPAN_ROWS_UNDER_LATCHES.load(Ordering::Relaxed),
    );
    let seen = span(&s);
    let excl = SPAN_READS_EXCLUDING_WRITERS.load(Ordering::Relaxed) - e0;
    let free = SPAN_READS_LATCH_FREE.load(Ordering::Relaxed) - f0;
    let rows = SPAN_ROWS_UNDER_LATCHES.load(Ordering::Relaxed) - r0;
    assert_eq!(seen, 200, "the fixture must actually have rows to scan");
    assert_eq!(
        (excl, free),
        (1, 0),
        "with a NON-EMPTY tail the span read must take all 64 shard latches, \
         and be counted as excluding writers"
    );
    assert_eq!(
        rows, 200,
        "and the rows merged under the latches are the size of the window every \
         writer waited on: {rows}"
    );

    // ── ARM 2: the same rows, SEALED. The tail is empty, the branch is
    //    skipped, and no writer is excluded by the identical query.
    s.seal();
    let (e1, f1, r1) = (
        SPAN_READS_EXCLUDING_WRITERS.load(Ordering::Relaxed),
        SPAN_READS_LATCH_FREE.load(Ordering::Relaxed),
        SPAN_ROWS_UNDER_LATCHES.load(Ordering::Relaxed),
    );
    let seen_sealed = span(&s);
    let excl2 = SPAN_READS_EXCLUDING_WRITERS.load(Ordering::Relaxed) - e1;
    let free2 = SPAN_READS_LATCH_FREE.load(Ordering::Relaxed) - f1;
    let rows2 = SPAN_ROWS_UNDER_LATCHES.load(Ordering::Relaxed) - r1;

    assert_eq!(
        seen_sealed, seen,
        "sealing changes no answer — the two arms must differ ONLY in whether \
         the latches were taken"
    );
    assert_eq!(
        (excl2, free2),
        (0, 1),
        "with an EMPTY tail the same query must take no latches and exclude \
         nobody — this is why a pure-read workload pays nothing and a mix pays \
         everything"
    );
    assert_eq!(
        rows2, 0,
        "and no rows are merged under latches that were never taken"
    );
}

/// A SINGLE row in the tail is enough to re-arm the exclusion.
///
/// The condition is `tail_has_versions()`, not "the tail is large". Under a
/// write-heavy workload that is permanently true however often the store seals
/// — which is why an experiment that raised the seal RATE 100x moved
/// `write-heavy` by nothing (981 -> 927 ops/s, inside noise). Pinned here so
/// that result reads as a property of the code rather than as a puzzle.
#[test]
fn one_row_in_the_tail_re_arms_the_exclusion() {
    let _serial = serial();
    let s = Store::new();
    // The arm where the mechanism EXISTS — see the module header.
    s.set_tail_span_copyout(false);
    seed(&s, 200);
    s.seal();
    // Sealed: no exclusion.
    let e0 = SPAN_READS_EXCLUDING_WRITERS.load(Ordering::Relaxed);
    span(&s);
    assert_eq!(
        SPAN_READS_EXCLUDING_WRITERS.load(Ordering::Relaxed) - e0,
        0,
        "the sealed baseline must exclude nobody, or the re-arm below proves \
         nothing"
    );

    // ONE write. Not a burst, not a threshold — one.
    s.put(&pfx(), b"Sx", StoredValue::Plain(vec![9]))
        .expect("put");
    let e1 = SPAN_READS_EXCLUDING_WRITERS.load(Ordering::Relaxed);
    span(&s);
    assert_eq!(
        SPAN_READS_EXCLUDING_WRITERS.load(Ordering::Relaxed) - e1,
        1,
        "ONE row in the tail re-arms the exclusion for every subsequent span \
         read — so under a continuous write stream it is armed permanently, at \
         any seal threshold"
    );
}
