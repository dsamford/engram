#![allow(non_snake_case)]
//! `VALIDATE_FROM_WINDOW` / `VALIDATE_FELL_BACK` / `VALIDATE_FALLBACK_KEYS` are
//! the discriminating measurement for the SF10 write-path gap, so they are
//! tested rather than trusted: an instrument that cannot tell the two paths
//! apart would report a clean answer either way.
//!
//! ITS OWN TEST BINARY, deliberately. The three counters are process-global
//! atomics, so any other test committing in the same binary would race the
//! deltas asserted here and make this pass or fail for reasons unrelated to the
//! code under test.

use engram_key::{Kind, KeyPrefix, Namespace, Partition, Realm};
use engram_store::{
    Store, StoredValue, VALIDATE_FALLBACK_KEYS, VALIDATE_FELL_BACK, VALIDATE_FROM_WINDOW,
};
use std::sync::atomic::Ordering::Relaxed;

fn pfx() -> KeyPrefix {
    KeyPrefix { realm: Realm(1), namespace: Namespace(1), kind: Kind::KV, partition: Partition(1) }
}
fn key(i: usize) -> Vec<u8> {
    format!("k{i:06}").into_bytes()
}

/// One commit reading `reads` keys and writing one.
fn commit_reading(store: &Store, reads: usize, w: usize) {
    let mut t = store.begin();
    for i in 0..reads {
        let _ = t.get(&pfx(), &key(i));
    }
    t.put(&pfx(), &key(w), StoredValue::Plain(vec![1u8; 8])).expect("put");
    let _ = t.commit();
}

#[test]
fn the_counters_distinguish_the_window_from_the_fallback() {
    const READS: usize = 500;
    let store = Store::new();
    for i in 0..2_000 {
        store.put(&pfx(), &key(i), StoredValue::Plain(vec![0u8; 8])).expect("seed");
    }

    // ── ARM A: the commit window answers ──────────────────────────────────
    store.set_commit_window_validation(true);
    let (w0, f0, k0) = (
        VALIDATE_FROM_WINDOW.load(Relaxed),
        VALIDATE_FELL_BACK.load(Relaxed),
        VALIDATE_FALLBACK_KEYS.load(Relaxed),
    );
    commit_reading(&store, READS, 1_500);
    let (dw_a, df_a, dk_a) = (
        VALIDATE_FROM_WINDOW.load(Relaxed) - w0,
        VALIDATE_FELL_BACK.load(Relaxed) - f0,
        VALIDATE_FALLBACK_KEYS.load(Relaxed) - k0,
    );

    // ── ARM B: the point loop, exactly as an exhausted window would ───────
    store.set_commit_window_validation(false);
    let (w1, f1, k1) = (
        VALIDATE_FROM_WINDOW.load(Relaxed),
        VALIDATE_FELL_BACK.load(Relaxed),
        VALIDATE_FALLBACK_KEYS.load(Relaxed),
    );
    commit_reading(&store, READS, 1_501);
    let (dw_b, df_b, dk_b) = (
        VALIDATE_FROM_WINDOW.load(Relaxed) - w1,
        VALIDATE_FELL_BACK.load(Relaxed) - f1,
        VALIDATE_FALLBACK_KEYS.load(Relaxed) - k1,
    );

    println!("window arm:   from_window+{dw_a} fell_back+{df_a} fallback_keys+{dk_a}");
    println!("fallback arm: from_window+{dw_b} fell_back+{df_b} fallback_keys+{dk_b}");

    assert_eq!(dw_a, 1, "the window arm must record exactly one windowed validation");
    assert_eq!(df_a, 0, "the window arm must NOT record a fallback");
    assert_eq!(dk_a, 0, "the window arm walks no fallback keys");

    assert_eq!(df_b, 1, "the fallback arm must record exactly one fallback");
    assert_eq!(dw_b, 0, "the fallback arm must NOT record a windowed validation");
    // read set + write set; the write key is also read-validated only if read,
    // so the floor is the read set and the ceiling is read set + 1.
    assert!(
        dk_b >= READS as u64 && dk_b <= READS as u64 + 1,
        "the fallback must bill its whole read set: expected ~{READS}, got {dk_b}"
    );
}
