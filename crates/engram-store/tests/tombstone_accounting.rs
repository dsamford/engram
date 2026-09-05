#![allow(non_snake_case)]
//! Segments know how many of their versions are TOMBSTONES.
//!
//! Compaction was scheduled purely by segment COUNT, which cannot tell a
//! segment of live rows from one that is mostly deletions waiting to be
//! reclaimed. Under a create/delete churn the tombstones accumulate and every
//! scan, every prefix walk and every `merge_span` keeps paying for them until
//! the count threshold happens to fire — so a delete-heavy workload gets slower
//! and slower with nothing in the schedule that notices.
//!
//! This is the metadata a delete-aware trigger needs, on the same terms as
//! `max_commit_ts`: computed ONCE at construction, because a segment is
//! immutable and the alternative is a walk per call.

use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::{Store, StoredValue};

fn pfx() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::KV,
        partition: Partition(1),
    }
}

/// A store with no deletions reports no tombstones.
#[test]
fn a_store_of_live_rows_reports_no_tombstones() {
    let s = Store::new();
    for i in 0..200u32 {
        s.put(&pfx(), &i.to_be_bytes(), StoredValue::Plain(vec![1]))
            .expect("put");
    }
    s.seal();
    let (ratio, versions) = s.tombstone_ratio();
    assert_eq!(versions, 200, "every version is counted");
    assert_eq!(ratio, 0.0, "and none of them is a tombstone");
}

/// Deletions show up in the ratio — this is the signal the trigger reads.
#[test]
fn deletions_raise_the_ratio_in_proportion() {
    let s = Store::new();
    for i in 0..200u32 {
        s.put(&pfx(), &i.to_be_bytes(), StoredValue::Plain(vec![1]))
            .expect("put");
    }
    // Delete half. Each delete is a WRITE — a tombstone version, not a removal.
    for i in 0..100u32 {
        s.delete(&pfx(), &i.to_be_bytes());
    }
    s.seal();
    let (ratio, versions) = s.tombstone_ratio();
    assert_eq!(versions, 300, "200 puts + 100 tombstones");
    let expected = 100.0 / 300.0;
    assert!(
        (ratio - expected).abs() < 1e-9,
        "expected {expected}, got {ratio}"
    );
    assert!(
        ratio > 0.2,
        "and it crosses the default 0.2 threshold, which is the whole point"
    );
}

/// An empty store must report 0.0 and NOT divide by zero — the trigger consults
/// this on every seal, including the first.
#[test]
fn an_empty_sealed_set_is_zero_not_a_division_by_zero() {
    let s = Store::new();
    let (ratio, versions) = s.tombstone_ratio();
    assert_eq!((ratio, versions), (0.0, 0));
    // And after a seal that produced nothing.
    s.seal();
    assert_eq!(s.tombstone_ratio(), (0.0, 0));
}

/// A PAGED segment contributes its footer's counts, so the trigger works on the
/// bigger-than-RAM path too.
///
/// This assertion is the inverse of what it first was. Before the v3 footer a
/// paged segment could not say how many tombstones it held, so the ratio
/// counted resident segments only and deliberately UNDER-reported — a floor,
/// so the trigger fired late rather than spuriously. v3 records the counts, so
/// paging data out no longer changes its measured density. That matters because
/// paged is the mode the large corpora actually serve in: a delete-aware
/// trigger that went blind exactly there would have been decoration.
///
/// A v2 file still says nothing, and
/// `a_v2_footer_still_opens_and_simply_says_nothing_about_tombstones` pins that
/// "cannot say" stays distinct from "clean".
#[test]
fn a_paged_segment_reports_the_same_density_as_a_resident_one() {
    let dir = {
        let mut p = std::env::temp_dir();
        p.push(format!("engram-tombstone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mkdir");
        p
    };
    let s = Store::new();
    for i in 0..200u32 {
        s.put(&pfx(), &i.to_be_bytes(), StoredValue::Plain(vec![1]))
            .expect("put");
    }
    for i in 0..100u32 {
        s.delete(&pfx(), &i.to_be_bytes());
    }
    s.seal();
    let (resident_ratio, resident_versions) = s.tombstone_ratio();
    assert!(resident_versions > 0 && resident_ratio > 0.0);

    // Page it out: the same data, now on disk.
    s.into_paged(&dir, 8 << 20).expect("into_paged");
    let (paged_ratio, paged_versions) = s.tombstone_ratio();
    assert_eq!(
        (paged_ratio, paged_versions),
        (resident_ratio, resident_versions),
        "paging the same data out must not change its tombstone density — the \
         v3 footer carries the counts, so the trigger sees the same store \
         whether it is resident or on disk"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Compaction reclaims tombstones, so the ratio must fall after one — the
/// closed loop the trigger depends on. Without this, a trigger could fire
/// forever against a ratio nothing ever lowers.
#[test]
fn compaction_lowers_the_ratio_it_was_triggered_by() {
    let s = Store::new();
    for i in 0..400u32 {
        s.put(&pfx(), &i.to_be_bytes(), StoredValue::Plain(vec![1]))
            .expect("put");
        if i % 4 == 0 {
            s.seal(); // several segments, so compaction has something to merge
        }
    }
    for i in 0..300u32 {
        s.delete(&pfx(), &i.to_be_bytes());
    }
    s.seal();
    let (before, _) = s.tombstone_ratio();
    assert!(
        before > 0.2,
        "the fixture must actually cross the threshold, got {before}"
    );
    s.compact();
    let (after, _) = s.tombstone_ratio();
    assert!(
        after < before,
        "compaction must reclaim tombstones: {before} -> {after}. If it did \
         not, a delete-aware trigger would fire on every seal for ever"
    );
}

// ─── v2 / v3 footer compatibility ───────────────────────────────────────────
//
// v3 adds the tombstone counts. It is ADDITIVE on purpose: existing paged
// stores keep serving and existing measurement baselines stay re-runnable. A
// format change that invalidated them would have made every prior number
// unreproducible, which is a worse cost than the one it was buying.

/// A v2 file — one with no tombstone counts — must still open and read.
///
/// Synthesised by rewriting a v3 footer down to v2: drop the two count fields,
/// stamp version 2, re-hash. That is exactly the layout the previous build
/// wrote, so this is a real compatibility test and not a mock of one.
#[test]
fn a_v2_footer_still_opens_and_simply_says_nothing_about_tombstones() {
    let dir = {
        let mut p = std::env::temp_dir();
        p.push(format!("engram-v2compat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("mkdir");
        p
    };
    let s = Store::new();
    for i in 0..100u32 {
        s.put(&pfx(), &i.to_be_bytes(), StoredValue::Plain(vec![7]))
            .expect("put");
    }
    for i in 0..50u32 {
        s.delete(&pfx(), &i.to_be_bytes());
    }
    s.seal();
    s.into_paged(&dir, 8 << 20).expect("into_paged");

    // Rewrite every segment file's footer as v2.
    let mut rewritten = 0;
    for entry in std::fs::read_dir(&dir).expect("read_dir") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("seg") {
            continue;
        }
        let bytes = std::fs::read(&path).expect("read");
        // v3 footer: [.. fields .., tombstones(8), versions(8), ver(4), magic(8), hash(32)]
        let n = bytes.len();
        let v3_len = 8 + 8 + 8 + 8 + 8 + 8 + 8 + 4 + 8 + 32;
        assert!(n > v3_len, "the fixture must have a full footer");
        let body = &bytes[..n - v3_len];
        let f = &bytes[n - v3_len..];
        // Keep the first five u64 fields; drop tombstones + versions.
        let keep = &f[..40];
        let mut out = Vec::with_capacity(n);
        out.extend_from_slice(body);
        out.extend_from_slice(keep);
        out.extend_from_slice(&2u32.to_le_bytes()); // FORMAT_VERSION = 2
        out.extend_from_slice(&f[f.len() - 40..f.len() - 32]); // magic
        let hashed = out.len() - (n - v3_len);
        let _ = hashed;
        let start = out.len() - (40 + 4 + 8);
        let h = blake3::hash(&out[start..]);
        out.extend_from_slice(h.as_bytes());
        std::fs::write(&path, &out).expect("write");
        rewritten += 1;
    }
    assert!(rewritten > 0, "the fixture must have rewritten a segment");

    // It opens, it reads, and it simply declines to report tombstones.
    let (reopened, _cache) = Store::open_paged_dir(&dir, 8 << 20).expect("a v2 file must open");
    assert_eq!(
        reopened.get(&pfx(), &60u32.to_be_bytes()),
        Some(vec![7]),
        "a v2 file must still answer reads"
    );
    assert_eq!(
        reopened.get(&pfx(), &10u32.to_be_bytes()),
        None,
        "including its deletes"
    );
    let (ratio, versions) = reopened.tombstone_ratio();
    assert_eq!(
        (ratio, versions),
        (0.0, 0),
        "a v2 file CANNOT SAY how many tombstones it holds, and 'cannot say' \
         must read as absent — not as a clean segment"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
