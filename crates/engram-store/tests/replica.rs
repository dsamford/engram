#![allow(non_snake_case)]
//! The replica, PITR, and restore verification.

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::{
    ApplyError, Replica, RestoreVerdict, Store, StoredValue, recover_to, verify_restore,
};

fn prefix() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(1),
    }
}

fn primary_with(n: u8) -> Store {
    let s = Store::new();
    for i in 0..n {
        s.put(&prefix(), &[i], StoredValue::Plain(vec![i, 0xAB]))
            .expect("put");
    }
    s
}

// ─── The replica ────────────────────────────────────────────────────────────

#[test]
fn a_replica_catches_up_in_chunks_and_reads_identically() {
    let primary = primary_with(9);
    let entries = primary.log_tail(0);
    let mut replica = Replica::new();

    // Chunked apply with a deliberate RETRANSMIT overlap — normal catch-up.
    let r = replica.apply(&entries[0..4]).expect("first chunk");
    assert_eq!((r.applied, r.skipped), (4, 0));
    let r = replica.apply(&entries[2..9]).expect("overlapping chunk");
    assert_eq!(
        (r.applied, r.skipped),
        (5, 2),
        "the overlap is SKIPPED, not an error"
    );

    for i in 0..9u8 {
        assert_eq!(
            replica.store().get(&prefix(), &[i]),
            primary.get(&prefix(), &[i]),
            "key {i} reads identically"
        );
    }
    assert_eq!(
        replica.applied_ts(),
        primary.now_ts(),
        "timestamps are the PRIMARY's"
    );
    assert!(replica.bookmark_satisfied(primary.now_ts()));
    assert!(!replica.bookmark_satisfied(primary.now_ts() + 1));
}

/// A replica must read the NEWEST version of a key it has been sent several
/// versions of.
///
/// # The defect this pins
///
/// `apply_replicated` inserted each version with
/// `chain.partition_point(|v| v.commit_ts > ts)`. The version chain is stored
/// OLDEST-FIRST — `put` says so — so on an ascending chain that predicate is
/// false at the head and `partition_point` returns 0 for every entry. Each
/// replicated version was therefore inserted at the OLDEST position, the chain
/// came out reversed, and a reader walking from the back found the FIRST value
/// ever written and answered with it.
///
/// A wrong answer, silently, on the replication path.
///
/// # Why no existing test saw it
///
/// Every replica test wrote one version per key. With a chain of length 0 or 1
/// the correct and the inverted insertion point are both 0, so the bug is
/// invisible to any fixture that never updates a key twice. That is the whole
/// finding: the fixture, not the assertion, was what needed fixing.
#[test]
fn a_replica_reads_the_NEWEST_of_several_versions_of_one_key() {
    let primary = Store::new();
    // Five versions of ONE key. The value records which version it is, so a
    // wrong answer names itself.
    for v in 1u8..=5 {
        primary
            .put(&prefix(), b"hot", StoredValue::Plain(vec![v]))
            .expect("put");
    }
    // A second key with a single version, so the test still covers the shape
    // the older tests covered.
    primary
        .put(&prefix(), b"cold", StoredValue::Plain(vec![99]))
        .expect("put");

    let entries = primary.log_tail(0);
    let mut replica = Replica::new();
    replica.apply(&entries).expect("apply all");

    assert_eq!(
        replica.store().get(&prefix(), b"hot").as_deref(),
        Some([5u8].as_slice()),
        "the replica must read the LAST version written, not the first — a \
         reversed version chain answers with the oldest value and looks like a \
         stale read rather than a bug"
    );
    assert_eq!(
        replica.store().get(&prefix(), b"hot"),
        primary.get(&prefix(), b"hot"),
        "replica and primary must agree on the hot key"
    );
    assert_eq!(
        replica.store().get(&prefix(), b"cold"),
        primary.get(&prefix(), b"cold"),
        "replica and primary must agree on the single-version key"
    );
}

/// Point-in-time recovery over a multi-version key.
///
/// `recover_to` walks the same chains, so a reversed chain would make every
/// as-of read answer with the wrong version too. Asserted separately because
/// PITR reaching the right answer through a broken chain by luck is exactly the
/// kind of thing that holds until it does not.
#[test]
fn recover_to_picks_the_right_version_of_a_repeatedly_written_key() {
    let primary = Store::new();
    let mut stamps = Vec::new();
    for v in 1u8..=5 {
        let ts = primary
            .put(&prefix(), b"hot", StoredValue::Plain(vec![v]))
            .expect("put");
        stamps.push(ts);
    }
    let entries = primary.log_tail(0);

    // As of each write's own timestamp, the value is that write's.
    for (i, ts) in stamps.iter().enumerate() {
        let at = recover_to(&entries, *ts).expect("recover_to");
        assert_eq!(
            at.get(&prefix(), b"hot").as_deref(),
            Some([(i + 1) as u8].as_slice()),
            "as of the timestamp of write {}, the value must be that write's",
            i + 1
        );
    }
}

#[test]
fn a_gap_REFUSES_and_never_skips_the_hole() {
    let primary = primary_with(5);
    let entries = primary.log_tail(0);
    let mut replica = Replica::new();
    replica.apply(&entries[0..2]).expect("prefix");
    match replica.apply(&entries[3..]) {
        Err(ApplyError::Gap {
            expected: 2,
            found: 3,
        }) => {}
        other => panic!("expected the gap refusal, got {other:?}"),
    }
    // Nothing past the gap applied; the missing entry closes it.
    assert_eq!(replica.next_seq(), 2);
    replica.apply(&entries[2..]).expect("the hole closed");
    assert_eq!(replica.next_seq(), 5);
}

#[test]
fn a_tampered_entry_refuses_AT_APPLY_with_its_seq() {
    let primary = primary_with(5);
    let mut entries = primary.log_tail(0);
    entries[3].payload[0] ^= 1;
    let mut replica = Replica::new();
    match replica.apply(&entries) {
        Err(ApplyError::ChainMismatch { seq: 3 }) => {}
        other => panic!("expected the chain refusal at 3, got {other:?}"),
    }
    assert_eq!(
        replica.next_seq(),
        3,
        "everything before the tamper stays applied"
    );
}

#[test]
fn a_FORK_with_recomputed_hashes_still_refuses() {
    // The strong case: an attacker rewrites an entry AND recomputes every
    // hash downstream — a self-consistent alternative history. Against a
    // replica that has already applied the true prefix, the fork's first
    // rewritten entry cannot extend the replica's own head.
    let primary = primary_with(6);
    let true_entries = primary.log_tail(0);

    // Build the fork: same first 3 writes, then a divergent 4th.
    let fork = Store::new();
    for i in 0..3u8 {
        fork.put(&prefix(), &[i], StoredValue::Plain(vec![i, 0xAB]))
            .expect("put");
    }
    fork.put(&prefix(), &[9], StoredValue::Plain(vec![0xEE]))
        .expect("divergent");
    let fork_entries = fork.log_tail(0);

    let mut replica = Replica::new();
    replica.apply(&true_entries[0..4]).expect("the true prefix");
    match replica.apply(&fork_entries[3..]) {
        Err(ApplyError::ChainMismatch { .. }) => {}
        Err(ApplyError::Gap { .. }) => {}
        other => panic!("the fork must not apply, got {other:?}"),
    }
}

// ─── PITR ───────────────────────────────────────────────────────────────────

#[test]
fn recover_to_rebuilds_the_state_AS_OF_a_timestamp() {
    let primary = Store::new();
    let t1 = primary
        .put(&prefix(), b"k", StoredValue::Plain(b"v1".to_vec()))
        .expect("put");
    let _t2 = primary
        .put(&prefix(), b"k", StoredValue::Plain(b"v2".to_vec()))
        .expect("put");
    let t3 = primary
        .put(&prefix(), b"other", StoredValue::Plain(b"x".to_vec()))
        .expect("put");

    let entries = primary.log_tail(0);
    let at_t1 = recover_to(&entries, t1).expect("pitr");
    assert_eq!(
        at_t1.get(&prefix(), b"k"),
        Some(b"v1".to_vec()),
        "the overwrite is NOT there"
    );
    assert_eq!(
        at_t1.get(&prefix(), b"other"),
        None,
        "the later key is NOT there"
    );
    let full = recover_to(&entries, t3).expect("pitr at head");
    assert_eq!(full.get(&prefix(), b"k"), Some(b"v2".to_vec()));
}

// ─── Restore verification ───────────────────────────────────────────────────

#[test]
fn a_faithful_restore_verifies_and_every_corruption_is_named() {
    let primary = primary_with(6);
    let entries = primary.log_tail(0);

    let restored = Store::recover(&entries).expect("restore");
    assert_eq!(
        verify_restore(&entries, &restored),
        RestoreVerdict::Faithful { entries: 6 },
        "the head hashes agree"
    );

    // A restore that silently dropped the tail: counts and head diverge.
    let short = Store::recover(&entries[..5]).expect("short restore");
    match verify_restore(&entries, &short) {
        RestoreVerdict::Diverged {
            source_entries: 6,
            restored_entries: 5,
            heads_match: false,
        } => {}
        other => panic!("expected divergence, got {other:?}"),
    }

    // A tampered SOURCE refuses before the restored store is even consulted.
    let mut bad = entries.clone();
    bad[2].payload[0] ^= 1;
    assert_eq!(
        verify_restore(&bad, &restored),
        RestoreVerdict::SourceBroken { seq: 2 }
    );
}

#[test]
fn a_content_swap_with_equal_counts_is_caught_by_the_HEAD() {
    // Counts can agree while content does not — the decisive check is the
    // head hash, and this pins that it is actually consulted.
    let primary = primary_with(4);
    let entries = primary.log_tail(0);
    let other = Store::new();
    for i in 0..4u8 {
        other
            .put(&prefix(), &[i + 40], StoredValue::Plain(vec![0xFF]))
            .expect("put");
    }
    match verify_restore(&entries, &other) {
        RestoreVerdict::Diverged {
            heads_match: false, ..
        } => {}
        other => panic!("expected the head mismatch, got {other:?}"),
    }
}
