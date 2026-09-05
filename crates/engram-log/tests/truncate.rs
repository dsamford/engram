#![allow(non_snake_case)]
//! Log truncation — a RETENTION statement, not a history edit.

use engram_key::{Kind, Namespace, Partition, Realm};
use engram_log::{ChainVerify, CommitLog, Op, RoutingHeader};

fn header(ts: u64) -> RoutingHeader {
    RoutingHeader {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(1),
        op: Op::Put,
        commit_ts: ts,
    }
}

#[test]
fn truncation_preserves_seq_allocation_head_and_suffix_verify() {
    let mut log = CommitLog::new();
    for i in 0..10u64 {
        log.append(header(i), vec![i as u8]);
    }
    let head_before = log.head();
    let dropped = log.truncate_below(6);
    assert_eq!(dropped, 6);
    assert_eq!(log.len(), 10, "len still counts truncated entries");
    assert_eq!(
        log.head(),
        head_before,
        "head unchanged — nothing newer dropped"
    );
    assert_eq!(log.tail(0).len(), 4, "only the suffix is retained");
    assert_eq!(log.tail(0)[0].seq, 6);
    // The suffix verifies against the RETAINED predecessor hash.
    match log.verify() {
        ChainVerify::Intact { head, .. } => assert_eq!(head, head_before),
        other => panic!("truncated log must verify: {other:?}"),
    }
    // New appends continue the sequence — no duplicate seq, chain intact.
    log.append(header(10), vec![99]);
    assert_eq!(log.tail(10)[0].seq, 10);
    assert!(matches!(log.verify(), ChainVerify::Intact { .. }));
}

#[test]
fn a_truncated_logs_own_entries_REFUSE_full_recovery() {
    // Fail-closed: the suffix has no genesis, so a from-scratch recovery
    // must refuse rather than silently rebuild a partial history.
    let mut log = CommitLog::new();
    for i in 0..5u64 {
        log.append(header(i), vec![i as u8]);
    }
    log.truncate_below(3);
    match CommitLog::verify_entries(log.tail(0)) {
        ChainVerify::SequenceGap {
            expected: 0,
            found: 3,
        } => {}
        other => panic!("expected a sequence gap from genesis, got {other:?}"),
    }
}

#[test]
fn truncating_everything_keeps_the_head_for_the_next_append() {
    let mut log = CommitLog::new();
    for i in 0..3u64 {
        log.append(header(i), vec![i as u8]);
    }
    let head = log.head();
    assert_eq!(log.truncate_below(100), 3, "all dropped");
    assert_eq!(log.head(), head, "head survives a full truncation");
    assert_eq!(log.len(), 3);
    log.append(header(3), vec![7]);
    assert_eq!(log.tail(0)[0].seq, 3);
    assert!(matches!(log.verify(), ChainVerify::Intact { .. }));
}
