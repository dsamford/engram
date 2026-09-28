//! The range index orders typed temporals — each type as its own key class,
//! within it in exactly the order `Value::lt3` compares — and a sidecar written
//! before that could not is refused whenever it may have left a date out.
//!
//! Until 2026-09-24 `IndexKey::from_tagged` answered `None` for every temporal
//! tag, so on a corpus whose `creationDate` is a DATETIME the range index held
//! no dates at all, and IC9's index-ordered top-k declined on every run.

use engram_key::value::Tag;
use engram_store::{IndexDef, IndexKey, PropertyId, RangeIndex};

fn tagged(tag: Tag, payload: &[u8]) -> Vec<u8> {
    let mut v = vec![tag.byte()];
    v.extend_from_slice(payload);
    v
}

/// DATETIME_OFFSET: [epoch i64][nanos u32][offset i32].
fn dt(secs: i64, nanos: u32, offset: i32) -> Vec<u8> {
    let mut p = secs.to_le_bytes().to_vec();
    p.extend_from_slice(&nanos.to_le_bytes());
    p.extend_from_slice(&offset.to_le_bytes());
    tagged(Tag::DATETIME_OFFSET, &p)
}

/// DATETIME_ZONE_ID: [len u32][epoch i64][nanos u32][offset i32][zone].
fn dt_zone(secs: i64, nanos: u32, offset: i32, zone: &str) -> Vec<u8> {
    let mut p = secs.to_le_bytes().to_vec();
    p.extend_from_slice(&nanos.to_le_bytes());
    p.extend_from_slice(&offset.to_le_bytes());
    p.extend_from_slice(zone.as_bytes());
    let mut v = vec![Tag::DATETIME_ZONE_ID.byte()];
    v.extend_from_slice(&(p.len() as u32).to_le_bytes());
    v.extend_from_slice(&p);
    v
}

fn key(t: &[u8]) -> IndexKey {
    IndexKey::from_tagged(t).unwrap_or_else(|| panic!("unorderable: {t:?}"))
}

#[test]
fn a_datetime_keys_on_its_instant_and_nothing_else() {
    // The same instant presented three ways is ONE key — `eq3` says they are
    // equal, so an index that told them apart would split a tie group.
    let utc = key(&dt(1_300_000_000, 5, 0));
    assert_eq!(utc, key(&dt(1_300_000_000, 5, 7200)));
    assert_eq!(utc, key(&dt_zone(1_300_000_000, 5, 3600, "Europe/Paris")));
    // and instants order by seconds, then nanoseconds, negatives included
    let order = [
        key(&dt(-86_400, 0, 0)),
        key(&dt(-1, 999_999_999, 0)),
        key(&dt(0, 0, 0)),
        key(&dt(0, 1, -3600)),
        key(&dt(1_300_000_000, 4, 0)),
        utc.clone(),
        key(&dt(1_300_000_001, 0, 0)),
    ];
    for w in order.windows(2) {
        assert!(w[0] < w[1], "{:?} must sort below {:?}", w[0], w[1]);
    }
}

#[test]
fn a_time_keys_on_utc_as_lt3_compares_it() {
    // 10:00+02:00 is 08:00 UTC: below 09:00Z, equal to 08:00Z.
    let time = |nanos: i64, offset: i32| {
        let mut p = nanos.to_le_bytes().to_vec();
        p.extend_from_slice(&offset.to_le_bytes());
        key(&tagged(Tag::TIME, &p))
    };
    let h = 3_600_000_000_000i64;
    assert!(time(10 * h, 7200) < time(9 * h, 0));
    assert_eq!(time(10 * h, 7200), time(8 * h, 0));
}

#[test]
fn every_temporal_type_is_its_own_class() {
    let date = key(&tagged(Tag::DATE, &15_000i64.to_le_bytes()));
    let local_time = key(&tagged(Tag::LOCAL_TIME, &1i64.to_le_bytes()));
    let mut ldt = 0i64.to_le_bytes().to_vec();
    ldt.extend_from_slice(&0u32.to_le_bytes());
    let local_dt = key(&tagged(Tag::LOCAL_DATETIME, &ldt));
    let datetime = key(&dt(i64::MIN, 0, 0));
    // the numeric and string classes keep their documented places below
    let classes = [
        IndexKey::Int(i64::MAX),
        IndexKey::Float(f64::INFINITY),
        IndexKey::Str(vec![0xFF]),
        date,
        local_time,
        datetime,
        local_dt,
    ];
    for w in classes.windows(2) {
        assert!(w[0].class() < w[1].class() && w[0] < w[1], "{:?} / {:?}", w[0], w[1]);
    }
    // A DURATION stays unorderable: P1M against P30D has no answer.
    let mut dur = Vec::new();
    for _ in 0..3 {
        dur.extend_from_slice(&1i64.to_le_bytes());
    }
    dur.extend_from_slice(&0i32.to_le_bytes());
    assert_eq!(IndexKey::from_tagged(&tagged(Tag::DURATION, &dur)), None);
}

#[test]
fn a_successor_is_the_next_key_and_nothing_lies_between() {
    for k in [
        IndexKey::Int(41),
        IndexKey::Int(i64::MAX),
        IndexKey::Float(-0.0),
        IndexKey::Float(2.5),
        IndexKey::Str(b"abc".to_vec()),
        IndexKey::Date(19_000),
        IndexKey::Date(i64::MAX),
        IndexKey::DateTime(1_300_000_000, u32::MAX),
        IndexKey::LocalDateTime(7, 3),
    ] {
        let s = k.successor();
        assert!(k < s, "{k:?} -> {s:?} is not above it");
    }
    assert_eq!(IndexKey::DateTime(5, 7).successor(), IndexKey::DateTime(5, 8));
    assert_eq!(IndexKey::DateTime(5, u32::MAX).successor(), IndexKey::DateTime(6, 0));
    assert_eq!(IndexKey::Float(-0.0).successor(), IndexKey::Float(0.0));
}

fn index(entries: Vec<(IndexKey, Vec<u8>)>, unindexable: u64) -> RangeIndex {
    RangeIndex::from_entries(IndexDef::new(1, PropertyId(9)), 42, entries, unindexable)
}

/// A v2 image rewritten as the v1 format: the magic swapped and the BLAKE3
/// re-sealed, which is exactly what a file written before temporals were
/// orderable looks like when it holds only v1 key tags.
fn as_v1(v2: &[u8]) -> Vec<u8> {
    let mut body = v2[..v2.len() - 32].to_vec();
    assert_eq!(&body[..8], b"ENGRIDX2");
    body[..8].copy_from_slice(b"ENGRIDX1");
    let h = blake3::hash(&body);
    body.extend_from_slice(h.as_bytes());
    body
}

#[test]
fn a_sidecar_round_trips_every_key_class() {
    let entries = vec![
        (IndexKey::Int(-3), 1u64.to_be_bytes().to_vec()),
        (IndexKey::Str(b"x".to_vec()), 2u64.to_be_bytes().to_vec()),
        (IndexKey::Date(15_000), 3u64.to_be_bytes().to_vec()),
        (IndexKey::Time(-5), 4u64.to_be_bytes().to_vec()),
        (IndexKey::LocalTime(9), 5u64.to_be_bytes().to_vec()),
        (IndexKey::DateTime(1_300_000_000, 7), 6u64.to_be_bytes().to_vec()),
        (IndexKey::LocalDateTime(-1, 2), 7u64.to_be_bytes().to_vec()),
    ];
    let idx = index(entries.clone(), 0);
    let back = RangeIndex::from_bytes(&idx.to_bytes(), IndexDef::new(1, PropertyId(9)))
        .expect("a v2 image loads");
    let lo = IndexKey::Int(i64::MIN);
    let hi = IndexKey::LocalDateTime(i64::MAX, u32::MAX);
    assert_eq!(back.range(&lo, &hi).bodies, idx.range(&lo, &hi).bodies);
    assert_eq!(back.range(&lo, &hi).bodies.len(), entries.len());
}

#[test]
fn a_v1_sidecar_is_trusted_only_when_it_skipped_nothing() {
    let ints = vec![
        (IndexKey::Int(1), 1u64.to_be_bytes().to_vec()),
        (IndexKey::Int(2), 2u64.to_be_bytes().to_vec()),
    ];
    let def = || IndexDef::new(1, PropertyId(9));
    // Skipped nothing: complete under either rule, so still adopted.
    let clean = as_v1(&index(ints.clone(), 0).to_bytes());
    assert!(RangeIndex::from_bytes(&clean, def()).is_some(), "a complete v1 file is refused");
    // Skipped something: under v1 that may have been every date the property
    // holds, and its vintage can still match — refused, so the store rebuilds.
    let lossy = as_v1(&index(ints, 5).to_bytes());
    assert!(
        RangeIndex::from_bytes(&lossy, def()).is_none(),
        "a v1 file that left values out would be adopted as complete"
    );
}
