#![allow(non_snake_case)]
// Real OS threads: this is the M3 concurrency layer and its test EXISTS to drive
// concurrent commits, which the workspace lint forbids everywhere the model is
// still single-threaded. The store is `Send + Sync` (the D2 revision), so this
// is now legitimate — and the test is the proof it holds under real interleaving.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

//! Optimistic (MVCC-OCC) transactions: atomic multi-key commit, read-your-writes,
//! first-committer-wins conflict detection over read AND write sets, and the
//! concurrency proof — N threads serialize their read-modify-writes correctly.

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

fn acct_key(a: u64) -> Vec<u8> {
    let mut k = b"acct-".to_vec();
    k.extend_from_slice(&a.to_be_bytes());
    k
}

fn bal_bytes(v: i64) -> StoredValue {
    StoredValue::Plain(v.to_le_bytes().to_vec())
}

fn decode_bal(bytes: Vec<u8>) -> i64 {
    i64::from_le_bytes(bytes.as_slice().try_into().expect("8 bytes"))
}

#[test]
fn a_transaction_commits_its_whole_write_set_atomically() {
    let s = Store::new();
    let mut t = s.begin();
    t.put(&pfx(), b"a", StoredValue::Plain(vec![1])).expect("a");
    t.put(&pfx(), b"b", StoredValue::Plain(vec![2])).expect("b");
    // Invisible before commit — the buffered writes are not published.
    assert_eq!(s.get(&pfx(), b"a"), None);
    assert_eq!(s.get(&pfx(), b"b"), None);
    let ts = t.commit().expect("commits");
    // Both appear, and both carry the SAME commit ts (one atomic visibility point).
    assert_eq!(s.get(&pfx(), b"a"), Some(vec![1]));
    assert_eq!(s.get(&pfx(), b"b"), Some(vec![2]));
    assert!(ts > 0);
}

#[test]
fn read_your_writes_within_a_transaction() {
    let s = Store::new();
    let mut t = s.begin();
    assert_eq!(t.get(&pfx(), b"a"), None);
    t.put(&pfx(), b"a", StoredValue::Plain(vec![9]))
        .expect("put");
    assert_eq!(
        t.get(&pfx(), b"a"),
        Some(vec![9]),
        "a put reads back within the txn"
    );
    t.delete(&pfx(), b"a");
    assert_eq!(
        t.get(&pfx(), b"a"),
        None,
        "a buffered delete reads as absent"
    );
    t.commit().expect("commits");
    assert_eq!(s.get(&pfx(), b"a"), None);
}

#[test]
fn disjoint_writers_both_commit() {
    let s = Store::new();
    let mut a = s.begin();
    let mut b = s.begin();
    a.put(&pfx(), b"x", StoredValue::Plain(vec![1])).expect("x");
    b.put(&pfx(), b"y", StoredValue::Plain(vec![2])).expect("y");
    a.commit().expect("a commits");
    // b wrote a DIFFERENT key from a snapshot before a committed — no overlap,
    // so it commits too.
    b.commit().expect("b commits (disjoint keys)");
    assert_eq!(s.get(&pfx(), b"x"), Some(vec![1]));
    assert_eq!(s.get(&pfx(), b"y"), Some(vec![2]));
}

#[test]
fn first_committer_wins_on_write_write_conflict() {
    let s = Store::new();
    s.put(&pfx(), b"k", StoredValue::Plain(vec![0]))
        .expect("seed");
    // Two transactions from overlapping snapshots both write k.
    let mut a = s.begin();
    let mut b = s.begin();
    a.put(&pfx(), b"k", StoredValue::Plain(vec![1])).expect("a");
    b.put(&pfx(), b"k", StoredValue::Plain(vec![2])).expect("b");
    a.commit().expect("a wins");
    assert_eq!(
        b.commit(),
        Err(StoreError::Conflict),
        "b's write of k was raced"
    );
    assert_eq!(
        s.get(&pfx(), b"k"),
        Some(vec![1]),
        "the winner's value stands"
    );
}

#[test]
fn a_stale_READ_aborts_the_transaction() {
    let s = Store::new();
    s.put(&pfx(), b"k", StoredValue::Plain(vec![0]))
        .expect("seed");
    let mut a = s.begin();
    assert_eq!(
        a.get(&pfx(), b"k"),
        Some(vec![0]),
        "A reads k into its read-set"
    );
    // A concurrent committer changes k AFTER A's snapshot.
    s.put(&pfx(), b"k", StoredValue::Plain(vec![5]))
        .expect("racer");
    // A writes an unrelated key and tries to commit — its READ of k is now stale,
    // so it must abort even though its WRITE does not conflict.
    a.put(&pfx(), b"other", StoredValue::Plain(vec![1]))
        .expect("other");
    assert_eq!(a.commit(), Err(StoreError::Conflict), "a stale read aborts");
    assert_eq!(
        s.get(&pfx(), b"other"),
        None,
        "nothing from the aborted txn published"
    );
}

#[test]
fn a_read_only_transaction_commits_without_publishing() {
    let s = Store::new();
    s.put(&pfx(), b"k", StoredValue::Plain(vec![7]))
        .expect("seed");
    let before = s.now_ts();
    let mut t = s.begin();
    assert_eq!(t.get(&pfx(), b"k"), Some(vec![7]));
    t.commit().expect("read-only commits");
    assert_eq!(
        s.now_ts(),
        before,
        "a read-only commit mints no new commit ts"
    );
}

/// THE concurrency proof. N real threads each perform `PER` read-modify-write
/// increments of one shared counter through OCC transactions, retrying on
/// conflict. If the store serialized the RMWs correctly the counter equals
/// THREADS*PER exactly — a lost update (the bug OCC exists to prevent) would
/// leave it lower. This can only run because the store is `Send + Sync` now.
#[test]
fn concurrent_read_modify_writes_do_not_lose_updates() {
    let s = Store::new();
    s.put(
        &pfx(),
        b"ctr",
        StoredValue::Plain(0u32.to_le_bytes().to_vec()),
    )
    .expect("seed");

    const THREADS: usize = 8;
    const PER: usize = 50;

    let mut handles = Vec::with_capacity(THREADS);
    for _ in 0..THREADS {
        let s = s.clone();
        handles.push(std::thread::spawn(move || {
            let mut conflicts = 0u64;
            for _ in 0..PER {
                loop {
                    let mut t = s.begin();
                    let cur = t
                        .get(&pfx(), b"ctr")
                        .map(|v| u32::from_le_bytes(v.try_into().expect("4 bytes")))
                        .unwrap_or(0);
                    t.put(
                        &pfx(),
                        b"ctr",
                        StoredValue::Plain((cur + 1).to_le_bytes().to_vec()),
                    )
                    .expect("put");
                    match t.commit() {
                        Ok(_) => break,
                        Err(StoreError::Conflict) => {
                            conflicts += 1;
                            continue; // lost the race — retry from a fresh snapshot
                        }
                        Err(e) => panic!("unexpected: {e}"),
                    }
                }
            }
            conflicts
        }));
    }

    let mut total_conflicts = 0u64;
    for h in handles {
        total_conflicts += h.join().expect("thread");
    }

    let final_val = u32::from_le_bytes(
        s.get(&pfx(), b"ctr")
            .expect("ctr")
            .try_into()
            .expect("4 bytes"),
    );
    assert_eq!(
        final_val as usize,
        THREADS * PER,
        "every increment must survive — no lost updates ({total_conflicts} conflicts retried)"
    );
    // The RMW is genuinely contended, so SOME commits must have lost the race
    // and retried; a run with zero conflicts would mean the threads never
    // actually overlapped and the test proved nothing.
    assert!(
        total_conflicts > 0,
        "expected real contention — 0 conflicts means the threads never overlapped"
    );
}

/// Serializability under RANDOMIZED heavy contention — the achievable
/// interleaving-search gate (loom cannot model the store's `arc_swap` tier, so an
/// exhaustive model check is out; this drives MANY real interleavings instead).
/// ACCTS accounts each seeded to START; THREADS threads each perform PER random
/// transfers (read source + dest, move funds if solvent, commit-or-retry on
/// conflict). The invariant only a SERIALIZABLE (no-lost-update) execution keeps:
/// the TOTAL balance is conserved and no account goes negative. A lost update
/// under concurrency would create or destroy money — this is the concurrent-WRITE
/// contention workload the read-only benchmark corpus does not contain.
#[test]
fn concurrent_transfers_conserve_total_balance() {
    const ACCTS: u64 = 16;
    const START: i64 = 1000;
    const THREADS: usize = 8;
    const PER: usize = 200;

    let s = Store::new();
    for a in 0..ACCTS {
        s.put(&pfx(), &acct_key(a), bal_bytes(START)).expect("seed");
    }

    let mut handles = Vec::with_capacity(THREADS);
    for tid in 0..THREADS {
        let s = s.clone();
        handles.push(std::thread::spawn(move || {
            // Deterministic per-thread xorshift PRNG — no clock, no rng crate, so
            // the workload is reproducible; the CONCURRENCY supplies the variety.
            let mut rng = 0x9E3779B97F4A7C15u64 ^ (tid as u64 + 1).wrapping_mul(0xD1B54A32D192ED03);
            let mut next = move || {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                rng
            };
            let mut conflicts = 0u64;
            for _ in 0..PER {
                let src = next() % ACCTS;
                let mut dst = next() % ACCTS;
                if dst == src {
                    dst = (dst + 1) % ACCTS;
                }
                let amt = (next() % 50) as i64 + 1;
                loop {
                    let mut t = s.begin();
                    let sb = t.get(&pfx(), &acct_key(src)).map(decode_bal).unwrap_or(0);
                    let db = t.get(&pfx(), &acct_key(dst)).map(decode_bal).unwrap_or(0);
                    if sb >= amt {
                        t.put(&pfx(), &acct_key(src), bal_bytes(sb - amt)).unwrap();
                        t.put(&pfx(), &acct_key(dst), bal_bytes(db + amt)).unwrap();
                    }
                    match t.commit() {
                        Ok(_) => break,
                        Err(StoreError::Conflict) => {
                            conflicts += 1;
                            continue; // lost the race — retry from a fresh snapshot
                        }
                        Err(e) => panic!("unexpected: {e}"),
                    }
                }
            }
            conflicts
        }));
    }

    let mut total_conflicts = 0u64;
    for h in handles {
        total_conflicts += h.join().expect("thread");
    }

    let mut total = 0i64;
    for a in 0..ACCTS {
        let b = decode_bal(s.get(&pfx(), &acct_key(a)).expect("account present"));
        assert!(
            b >= 0,
            "account {a} went negative ({b}) — a non-serializable overdraft"
        );
        total += b;
    }
    assert_eq!(
        total,
        ACCTS as i64 * START,
        "total balance must be conserved — a lost update created/destroyed money ({total_conflicts} conflicts retried)"
    );
    assert!(
        total_conflicts > 0,
        "expected real contention — 0 conflicts means the threads never overlapped"
    );
}
