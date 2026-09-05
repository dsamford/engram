//! OCC validation skips the sealed prefix it provably need not read.
//!
//! Validation asks one question — did anyone commit to this key AFTER my
//! snapshot — and a sealed segment's footer answers it before the segment is
//! opened: `max_commit_ts` is the greatest commit timestamp any version in it
//! carries, and segments are sealed in timestamp order. So the walk stops at
//! the first segment sealed at or below the snapshot.
//!
//! Why it mattered (RC2, `docs/write-concurrency-ceiling.md`): without the
//! bound, validation walked EVERY sealed segment for EVERY read-set and
//! write-set key, under the global commit latch, and a freshly minted id is
//! never rejected early by the sparse index. On official SF1 the latch was
//! held 34.8-61.5% of wall, against a segment set that grows without bound on
//! the paged path.
//!
//! The bar is not "faster". It is **the same verdicts**: the skip must refuse
//! exactly the transactions the full walk refused. These tests assert the
//! verdicts on both sides of the boundary, and the last one is the canary —
//! it fails if the skip is ever widened past what the footer justifies.

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

/// The same shape as [`store_with_sealed_segments`], but each segment carries
/// MANY keys.
///
/// This exists because the one-key bed below could not see the regression it
/// was written to guard. `Segment::max_commit_ts` was recomputed on every
/// call — a walk over every version of every entry — and the validator
/// evaluates it before its early `break`, so the newest segment is scanned
/// unconditionally per validated key per commit, under the global commit
/// latch. With one key per segment that walk is O(1) and the cost is
/// invisible; in production, at `seal_after_versions = 65_536`, it was ~590k
/// iterations per commit.
///
/// A fixture's SHAPE is part of what it measures: a bed built from
/// single-version segments cannot distinguish O(1) from O(versions).
fn store_with_fat_sealed_segments(segments: usize, keys_per_segment: usize) -> Store {
    let s = Store::new();
    for seg in 0..segments {
        let mut t = s.begin();
        for k in 0..keys_per_segment {
            let key = format!("filler-{seg}-{k}");
            t.put(&pfx(), key.as_bytes(), v((k % 251) as u8))
                .expect("put");
        }
        t.commit().expect("commit");
        s.seal();
    }
    s
}

/// A store with `n` sealed segments, each holding one committed write to its
/// own key, plus the key every test contends on.
fn store_with_sealed_segments(n: usize) -> Store {
    let s = Store::new();
    for i in 0..n {
        let mut t = s.begin();
        let key = format!("filler-{i}");
        t.put(&pfx(), key.as_bytes(), v(i as u8)).expect("put");
        t.commit().expect("commit");
        s.seal();
    }
    s
}

#[test]
fn a_write_committed_after_the_snapshot_still_conflicts() {
    let s = store_with_sealed_segments(8);
    // Establish the contended key in a sealed segment.
    let mut setup = s.begin();
    setup.put(&pfx(), b"k", v(0)).expect("put");
    setup.commit().expect("commit");
    s.seal();

    // T reads `k`, then a committed writer moves it, then T commits.
    let mut t = s.begin();
    assert_eq!(t.get(&pfx(), b"k"), Some(vec![0]));

    let mut other = s.begin();
    other.put(&pfx(), b"k", v(1)).expect("put");
    other.commit().expect("other commits");

    t.put(&pfx(), b"other", v(2)).expect("put");
    assert!(
        matches!(t.commit(), Err(StoreError::Conflict)),
        "a read that moved after the snapshot must still abort — the skip may \
         never reach a version newer than the snapshot"
    );
}

#[test]
fn the_same_conflict_is_refused_with_the_contended_key_sealed_away() {
    // The conflicting write itself is SEALED before the victim commits, so the
    // verdict depends on a sealed segment whose max_commit_ts is above the
    // snapshot — precisely the segment the skip must NOT skip.
    let s = store_with_sealed_segments(8);
    let mut setup = s.begin();
    setup.put(&pfx(), b"k", v(0)).expect("put");
    setup.commit().expect("commit");
    s.seal();

    let mut t = s.begin();
    assert_eq!(t.get(&pfx(), b"k"), Some(vec![0]));

    let mut other = s.begin();
    other.put(&pfx(), b"k", v(1)).expect("put");
    other.commit().expect("other commits");
    s.seal(); // the conflicting version now lives in a sealed segment

    t.put(&pfx(), b"other", v(2)).expect("put");
    assert!(
        matches!(t.commit(), Err(StoreError::Conflict)),
        "the conflicting version is sealed, not resident — the walk must still \
         find it, because that segment's max_commit_ts is above the snapshot"
    );
}

#[test]
fn an_untouched_key_commits_over_a_deep_sealed_prefix() {
    // The case the skip exists for: many sealed segments, none of them newer
    // than the snapshot, and a transaction that must NOT be refused.
    let s = store_with_sealed_segments(64);
    let mut setup = s.begin();
    setup.put(&pfx(), b"k", v(0)).expect("put");
    setup.commit().expect("commit");
    s.seal();

    let mut t = s.begin();
    assert_eq!(t.get(&pfx(), b"k"), Some(vec![0]));
    t.put(&pfx(), b"k", v(9)).expect("put");
    let ts = t.commit().expect("nothing moved: this must commit");
    assert!(ts > 0);
    assert_eq!(s.get(&pfx(), b"k"), Some(vec![9]));
}

#[test]
fn every_verdict_matches_a_full_walk_over_many_interleavings() {
    // The differential. For each of a spread of segment depths and both
    // orders (conflict before / after the victim's read), the verdict must be
    // the one the semantics demand — which is what the pre-skip walk produced.
    for depth in [0usize, 1, 3, 16, 64] {
        for conflicts in [false, true] {
            let s = store_with_sealed_segments(depth);
            let mut setup = s.begin();
            setup.put(&pfx(), b"k", v(0)).expect("put");
            setup.commit().expect("commit");
            s.seal();

            let mut t = s.begin();
            assert_eq!(t.get(&pfx(), b"k"), Some(vec![0]), "depth {depth}");

            if conflicts {
                let mut other = s.begin();
                other.put(&pfx(), b"k", v(1)).expect("put");
                other.commit().expect("other commits");
                s.seal();
            }

            t.put(&pfx(), b"z", v(2)).expect("put");
            let got = t.commit();
            if conflicts {
                assert!(
                    matches!(got, Err(StoreError::Conflict)),
                    "depth {depth}: a moved read must abort"
                );
            } else {
                assert!(
                    got.is_ok(),
                    "depth {depth}: nothing moved, so nothing may abort"
                );
            }
        }
    }
}

#[test]
fn every_verdict_matches_over_fat_segments_too() {
    // The same differential as above, over segments carrying MANY versions
    // each. The verdicts must be identical to the one-key bed's — the cached
    // `max_commit_ts` is an O(1) restatement of the same value, never a
    // different answer.
    //
    // 8 segments x 4,096 keys: enough that a per-call recompute would be a
    // visible cost, and enough that a constructor which forgot to stamp the
    // field would answer 0 and skip a prefix it must not skip.
    for conflicts in [false, true] {
        let s = store_with_fat_sealed_segments(8, 4_096);
        let mut setup = s.begin();
        setup.put(&pfx(), b"k", v(0)).expect("put");
        setup.commit().expect("commit");
        s.seal();

        let mut t = s.begin();
        assert_eq!(t.get(&pfx(), b"k"), Some(vec![0]));

        if conflicts {
            let mut other = s.begin();
            other.put(&pfx(), b"k", v(1)).expect("put");
            other.commit().expect("other commits");
            s.seal();
        }

        t.put(&pfx(), b"z", v(2)).expect("put");
        let got = t.commit();
        if conflicts {
            assert!(
                matches!(got, Err(StoreError::Conflict)),
                "fat segments: a moved read must abort"
            );
        } else {
            assert!(
                got.is_ok(),
                "fat segments: nothing moved, so nothing may abort"
            );
        }
    }
}

#[test]
fn a_stale_cached_bound_would_skip_a_segment_it_must_not() {
    // The canary for the CACHE specifically, as distinct from the skip.
    //
    // A segment sealed AFTER the snapshot carries the conflicting version.
    // If the cached bound under-reported (a constructor that forgot to stamp
    // it answers 0, and 0 <= any snapshot), the walk would break at that
    // segment and the conflict would be missed — a lost update reported as a
    // successful commit, which is the worst failure this file can have.
    let s = store_with_fat_sealed_segments(4, 1_024);
    let mut setup = s.begin();
    setup.put(&pfx(), b"k", v(0)).expect("put");
    setup.commit().expect("commit");
    s.seal();

    let mut t = s.begin();
    assert_eq!(t.get(&pfx(), b"k"), Some(vec![0]));

    // The conflict lands in a FAT segment sealed after t's snapshot, so the
    // bound that protects it is one computed over thousands of versions.
    let mut other = s.begin();
    for k in 0..1_024 {
        other
            .put(&pfx(), format!("noise-{k}").as_bytes(), v(7))
            .expect("put");
    }
    other.put(&pfx(), b"k", v(1)).expect("put");
    other.commit().expect("other commits");
    s.seal();

    t.put(&pfx(), b"z", v(2)).expect("put");
    assert!(
        matches!(t.commit(), Err(StoreError::Conflict)),
        "the conflicting version sits in a fat sealed segment above the \
         snapshot: an under-reported bound would skip it and lose the update"
    );
}
