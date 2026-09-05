#![allow(non_snake_case)]
//! OCC validation answers from a RECENT-COMMIT WINDOW instead of a point lookup
//! per key.
//!
//! Validation asks one question per key — did anyone commit to it after my
//! snapshot — and answered it with a tail probe per key, **under the global
//! commit latch**. That makes commit O(read set) at the one serialisation point
//! that cannot be parallelised, which is why a statement whose MATCH
//! materialised many rows got slower as workers were added rather than faster.
//!
//! Every write allocates its ts under that same latch (`Store::allocate`
//! returns the guard and the slot together), so a window appended at allocation
//! is ts-monotone and complete by construction. Iterating the suffix above the
//! reader's snapshot and keeping the LAST entry per key yields exactly what the
//! point loop finds — the newest committed version — because every older one
//! has a lower ts and is overwritten.
//!
//! **The bar is verdict equality, not speed.** These run the same interleavings
//! on both arms and require the same answer AND the same reported conflicting
//! key: the window is a cheaper way to compute the identical predicate, not a
//! different predicate.

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::{Store, StoreError, StoredValue};

fn pfx() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::KV,
        partition: Partition(1),
    }
}

fn v(n: u8) -> StoredValue {
    StoredValue::Plain(vec![n])
}

/// One interleaving, described so both arms run exactly the same thing.
#[derive(Clone, Copy, Debug)]
struct Case {
    name: &'static str,
    /// Keys T reads before committing.
    t_reads: &'static [&'static str],
    /// Keys T writes.
    t_writes: &'static [&'static str],
    /// Keys the OTHER transaction writes (committed first).
    other_writes: &'static [&'static str],
    /// Whether the other transaction DELETES rather than puts.
    other_deletes: bool,
    /// Whether to seal between the other's commit and T's, so the conflicting
    /// version lives in a sealed segment rather than the tail.
    seal_between: bool,
}

const CASES: &[Case] = &[
    Case {
        name: "disjoint",
        t_reads: &["a"],
        t_writes: &["b"],
        other_writes: &["z"],
        other_deletes: false,
        seal_between: false,
    },
    Case {
        name: "read-set collides",
        t_reads: &["a"],
        t_writes: &["b"],
        other_writes: &["a"],
        other_deletes: false,
        seal_between: false,
    },
    Case {
        name: "write-set collides",
        t_reads: &["a"],
        t_writes: &["b"],
        other_writes: &["b"],
        other_deletes: false,
        seal_between: false,
    },
    Case {
        name: "read collides with a DELETE",
        t_reads: &["a"],
        t_writes: &["b"],
        other_writes: &["a"],
        other_deletes: true,
        seal_between: false,
    },
    Case {
        name: "write collides with a DELETE",
        t_reads: &["a"],
        t_writes: &["b"],
        other_writes: &["b"],
        other_deletes: true,
        seal_between: false,
    },
    Case {
        name: "conflict lives in a SEALED segment",
        t_reads: &["a"],
        t_writes: &["b"],
        other_writes: &["a"],
        other_deletes: false,
        seal_between: true,
    },
    Case {
        name: "many reads, one collides",
        t_reads: &["a", "c", "d", "e", "f"],
        t_writes: &["b"],
        other_writes: &["e"],
        other_deletes: false,
        seal_between: false,
    },
    Case {
        name: "many reads, none collides",
        t_reads: &["a", "c", "d", "e", "f"],
        t_writes: &["b"],
        other_writes: &["z"],
        other_deletes: false,
        seal_between: false,
    },
];

/// Run one case and return `(committed, conflicting key if any)`.
fn run_case(c: &Case, window: bool) -> (bool, Option<Vec<u8>>) {
    let s = Store::new();
    s.set_commit_window_validation(window);
    // Seed every key the case touches, so reads resolve and deletes have
    // something to tombstone.
    for k in ["a", "b", "c", "d", "e", "f", "z"] {
        s.put(&pfx(), k.as_bytes(), v(0)).expect("seed");
    }
    s.seal();

    let mut t = s.begin();
    for k in c.t_reads {
        let _ = t.get(&pfx(), k.as_bytes());
    }

    let mut other = s.begin();
    for k in c.other_writes {
        if c.other_deletes {
            other.delete(&pfx(), k.as_bytes());
        } else {
            other.put(&pfx(), k.as_bytes(), v(1)).expect("put");
        }
    }
    other.commit().expect("the first committer wins");
    if c.seal_between {
        s.seal();
    }

    for k in c.t_writes {
        t.put(&pfx(), k.as_bytes(), v(2)).expect("put");
    }
    match t.commit_reporting() {
        Ok(_) => (true, None),
        Err((StoreError::Conflict, info)) => {
            (false, info.and_then(|i| i.conflicting.first().cloned()))
        }
        Err((e, _)) => panic!("unexpected error: {e:?}"),
    }
}

/// THE differential: every interleaving must produce the same verdict and name
/// the same conflicting key on both arms.
#[test]
fn every_interleaving_gets_the_same_verdict_on_both_arms() {
    for c in CASES {
        let point = run_case(c, false);
        let window = run_case(c, true);
        assert_eq!(
            point, window,
            "case `{}`: the window must compute the IDENTICAL predicate — same \
             verdict, same conflicting key. point={point:?} window={window:?}",
            c.name
        );
    }
}

/// Non-vacuity: the cases must actually contain both outcomes, or the
/// differential above is comparing two rows of "committed".
#[test]
fn the_case_matrix_contains_both_outcomes() {
    let mut committed = 0;
    let mut refused = 0;
    for c in CASES {
        if run_case(c, true).0 {
            committed += 1;
        } else {
            refused += 1;
        }
    }
    assert!(
        committed > 0 && refused > 0,
        "the matrix must exercise both outcomes: {committed} committed, \
         {refused} refused"
    );
}

/// THE FALLBACK, exercised deliberately: with a window too small to reach the
/// snapshot, verdicts must be unchanged.
#[test]
fn a_window_too_small_to_reach_the_snapshot_still_answers_correctly() {
    for c in CASES {
        let point = run_case(c, false);

        let s = Store::new();
        s.set_commit_window_validation(true);
        s.set_commit_window_capacity(2); // far too small
        for k in ["a", "b", "c", "d", "e", "f", "z"] {
            s.put(&pfx(), k.as_bytes(), v(0)).expect("seed");
        }
        s.seal();
        let mut t = s.begin();
        for k in c.t_reads {
            let _ = t.get(&pfx(), k.as_bytes());
        }
        let mut other = s.begin();
        for k in c.other_writes {
            if c.other_deletes {
                other.delete(&pfx(), k.as_bytes());
            } else {
                other.put(&pfx(), k.as_bytes(), v(1)).expect("put");
            }
        }
        other.commit().expect("first committer wins");
        if c.seal_between {
            s.seal();
        }
        for k in c.t_writes {
            t.put(&pfx(), k.as_bytes(), v(2)).expect("put");
        }
        let got = match t.commit_reporting() {
            Ok(_) => (true, None),
            Err((StoreError::Conflict, info)) => {
                (false, info.and_then(|i| i.conflicting.first().cloned()))
            }
            Err((e, _)) => panic!("unexpected error: {e:?}"),
        };
        assert_eq!(
            point, got,
            "case `{}`: a window that cannot reach the snapshot must fall back \
             to the point loop and answer identically",
            c.name
        );
    }
}

/// A BULK (`put_unlogged`) write is still a committed write: it reaches the
/// tail and the point loop sees it, so the window must too.
///
/// This is the case a first implementation would miss — the write skips the
/// LOG, and it is easy to conclude it therefore skips the window. It does not:
/// missing it would let the window arm commit a transaction the point loop
/// refuses, which is a lost update.
#[test]
fn a_bulk_unlogged_write_is_visible_to_the_window() {
    let verdict = |window: bool| -> bool {
        let s = Store::new();
        s.set_commit_window_validation(window);
        s.put(&pfx(), b"k", v(0)).expect("seed");
        s.seal();
        let mut t = s.begin();
        let _ = t.get(&pfx(), b"k");
        // A bulk write to the key T read, committed in between.
        s.put_unlogged(&pfx(), b"k", v(9)).expect("bulk");
        t.put(&pfx(), b"other", v(1)).expect("put");
        t.commit().is_ok()
    };
    assert!(
        !verdict(false),
        "the point loop must refuse: the read moved under an unlogged write"
    );
    assert!(
        !verdict(true),
        "and the window must refuse for the same reason — an unlogged write is \
         still a COMMITTED write, and a window that ignored it would commit a \
         transaction the point loop refuses"
    );
}

/// A transaction must not see its OWN writes in the window and abort itself.
#[test]
fn a_transaction_does_not_conflict_with_itself() {
    for window in [false, true] {
        let s = Store::new();
        s.set_commit_window_validation(window);
        s.put(&pfx(), b"k", v(0)).expect("seed");
        let mut t = s.begin();
        let _ = t.get(&pfx(), b"k");
        t.put(&pfx(), b"k", v(1)).expect("put");
        assert!(
            t.commit().is_ok(),
            "window={window}: a transaction reading then writing one key is the \
             ordinary read-modify-write and must commit"
        );
    }
}
