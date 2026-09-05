#![allow(non_snake_case)]
//! The derived range index: typed ordering, MVCC vintage, and FC-9/FC-11.

use engram_key::value::Tag;
use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::{IndexDef, IndexKey, PropertyId, RangeIndex, Record, Store, StoredValue};

const PROP: PropertyId = PropertyId(7);

fn group() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(1),
    }
}

fn int64(v: i64) -> Vec<u8> {
    let mut out = vec![Tag::INT64.byte()];
    out.extend_from_slice(&v.to_le_bytes());
    out
}

fn string(s: &str) -> Vec<u8> {
    let mut out = vec![Tag::STRING.byte()];
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    out
}

fn put_row(s: &Store, body: &[u8], value: Vec<u8>) -> u64 {
    let mut r = Record::new();
    r.set(PROP, value);
    s.put(&group(), body, StoredValue::Plain(r.encode()))
        .expect("row")
}

#[test]
fn NEGATIVE_integers_order_correctly() {
    // The canary-shaped test: little-endian byte order puts -1
    // (0xFF...FF) after every positive number, so a byte-comparing index
    // passes every all-positive test and misorders the first negative value
    // in production. The comparator is typed; this pins it.
    let s = Store::new();
    put_row(&s, b"neg", int64(-5));
    put_row(&s, b"zero", int64(0));
    put_row(&s, b"pos", int64(5));

    let idx = RangeIndex::build(&s, &group(), IndexDef::new(1, PROP), s.now_ts());
    let ans = idx.range(&IndexKey::Int(i64::MIN), &IndexKey::Int(i64::MAX));
    assert_eq!(
        ans.bodies,
        vec![b"neg".to_vec(), b"zero".to_vec(), b"pos".to_vec()]
    );

    // And a range that straddles zero includes the negative side.
    let ans = idx.range(&IndexKey::Int(-10), &IndexKey::Int(1));
    assert_eq!(ans.bodies, vec![b"neg".to_vec(), b"zero".to_vec()]);
}

#[test]
fn the_range_is_half_open_and_in_key_order() {
    let s = Store::new();
    for (body, v) in [(&b"a"[..], 10i64), (b"b", 20), (b"c", 30), (b"d", 40)] {
        put_row(&s, body, int64(v));
    }
    let idx = RangeIndex::build(&s, &group(), IndexDef::new(1, PROP), s.now_ts());
    let ans = idx.range(&IndexKey::Int(20), &IndexKey::Int(40));
    assert_eq!(
        ans.bodies,
        vec![b"b".to_vec(), b"c".to_vec()],
        "[lo, hi): 20 in, 40 out"
    );
}

#[test]
fn the_answer_carries_its_VINTAGE() {
    // An index answer without its as_of gets read as current — the staleness
    // twin of a percentile without its sample size.
    let s = Store::new();
    put_row(&s, b"a", int64(1));
    let build_ts = s.now_ts();
    let idx = RangeIndex::build(&s, &group(), IndexDef::new(1, PROP), build_ts);

    // The store moves on; the index does not.
    put_row(&s, b"b", int64(2));

    let ans = idx.range(&IndexKey::Int(0), &IndexKey::Int(10));
    assert_eq!(
        ans.bodies,
        vec![b"a".to_vec()],
        "the index answers at its snapshot"
    );
    assert_eq!(ans.as_of, build_ts, "…and says so");
}

#[test]
fn MVCC_a_build_at_a_past_ts_sees_the_past() {
    let s = Store::new();
    let t1 = put_row(&s, b"k", int64(1));
    put_row(&s, b"k", int64(100));
    s.delete(&group(), b"gone");

    let idx = RangeIndex::build(&s, &group(), IndexDef::new(1, PROP), t1);
    let ans = idx.range(&IndexKey::Int(0), &IndexKey::Int(1000));
    assert_eq!(ans.bodies, vec![b"k".to_vec()]);
    // At t1 the value was 1 — a range excluding 100 but including 1 finds it.
    let ans = idx.range(&IndexKey::Int(0), &IndexKey::Int(50));
    assert_eq!(
        ans.bodies,
        vec![b"k".to_vec()],
        "the index must hold the value AS OF its ts"
    );
}

#[test]
fn a_tombstoned_row_is_not_in_the_index() {
    let s = Store::new();
    put_row(&s, b"live", int64(1));
    put_row(&s, b"dead", int64(2));
    s.delete(&group(), b"dead");

    let idx = RangeIndex::build(&s, &group(), IndexDef::new(1, PROP), s.now_ts());
    let ans = idx.range(&IndexKey::Int(0), &IndexKey::Int(10));
    assert_eq!(ans.bodies, vec![b"live".to_vec()]);
}

#[test]
fn rebuilds_are_DETERMINISTIC() {
    // What makes drop-and-rebuild a repair rather than a gamble.
    let s = Store::new();
    for i in 0..20i64 {
        put_row(&s, &[i as u8], int64(i * 3 % 7));
    }
    let ts = s.now_ts();
    let a = RangeIndex::build(&s, &group(), IndexDef::new(1, PROP), ts);
    let b = RangeIndex::build(&s, &group(), IndexDef::new(1, PROP), ts);
    let full = (IndexKey::Int(i64::MIN), IndexKey::Int(i64::MAX));
    assert_eq!(a.range(&full.0, &full.1), b.range(&full.0, &full.1));
}

#[test]
fn unorderable_rows_are_COUNTED_on_every_answer() {
    // An index that quietly ignores a type reports "no matches" in the same
    // words as one that indexed it. The count rides on the ANSWER, where the
    // reader is, not in a log.
    let s = Store::new();
    put_row(&s, b"int", int64(1));
    let mut r = Record::new();
    r.set(PROP, vec![Tag::BOOL.byte(), 1]); // BOOL: not orderable by this index
    s.put(&group(), b"boolrow", StoredValue::Plain(r.encode()))
        .expect("row");

    let idx = RangeIndex::build(&s, &group(), IndexDef::new(1, PROP), s.now_ts());
    let ans = idx.range(&IndexKey::Int(0), &IndexKey::Int(10));
    assert_eq!(ans.bodies, vec![b"int".to_vec()]);
    assert_eq!(
        ans.unindexable, 1,
        "the answer must admit it is a floor over typed rows"
    );
}

#[test]
fn strings_and_cross_type_entries_have_a_stable_order() {
    let s = Store::new();
    put_row(&s, b"s1", string("apple"));
    put_row(&s, b"s2", string("banana"));
    put_row(&s, b"i1", int64(999));

    let idx = RangeIndex::build(&s, &group(), IndexDef::new(1, PROP), s.now_ts());
    let ans = idx.range(&IndexKey::Str(b"a".to_vec()), &IndexKey::Str(b"z".to_vec()));
    assert_eq!(ans.bodies, vec![b"s1".to_vec(), b"s2".to_vec()]);
    // Ints sort before every string in the documented cross-type order, so a
    // string range never catches them.
    assert_eq!(idx.len(), 3);
}

#[test]
fn FC9_the_definition_survives_fields_it_does_not_know() {
    // The definition IS a record, so a newer build's extra fields ride through
    // an older build's read-modify-write — open-endedness inherited from the
    // record layer rather than re-implemented.
    let def = IndexDef::new(9, PROP);
    let mut rec = def.as_record().clone();
    rec.set(PropertyId(500), {
        let mut v = vec![0xE9]; // an extension tag this build has never assigned
        v.extend_from_slice(&2u32.to_le_bytes());
        v.extend_from_slice(&[1, 2]);
        v
    });

    let rehydrated = IndexDef::from_record(Record::decode(&rec.encode()).expect("decodes"))
        .expect("known fields intact");
    assert_eq!(rehydrated.index_id(), 9);
    assert_eq!(rehydrated.property(), PROP);
    assert_eq!(
        rehydrated.as_record().get(PropertyId(500)),
        rec.get(PropertyId(500)),
        "the unknown definition field was dropped",
    );
}

#[test]
fn a_row_without_the_property_is_simply_absent() {
    let s = Store::new();
    put_row(&s, b"has", int64(1));
    let r = Record::new();
    s.put(&group(), b"lacks", StoredValue::Plain(r.encode()))
        .expect("row");

    let idx = RangeIndex::build(&s, &group(), IndexDef::new(1, PROP), s.now_ts());
    assert_eq!(idx.len(), 1);
    let ans = idx.range(&IndexKey::Int(i64::MIN), &IndexKey::Int(i64::MAX));
    assert_eq!(
        ans.unindexable, 0,
        "absent is not unorderable — different facts"
    );
}

#[test]
fn iter_desc_below_is_lazy_newest_first_and_half_open() {
    let s = Store::new();
    for (body, v) in [
        (&b"a"[..], 10i64),
        (b"b", 20),
        (b"c", 30),
        (b"d", 40),
        (b"e", 50),
    ] {
        put_row(&s, body, int64(v));
    }
    let idx = RangeIndex::build(&s, &group(), IndexDef::new(1, PROP), s.now_ts());

    // DESC below 45: keys 40, 30, 20, 10 (45 excludes 50), newest first.
    let got: Vec<(i64, Vec<u8>)> = idx
        .iter_desc_below(&IndexKey::Int(45))
        .map(|(k, b)| match k {
            IndexKey::Int(i) => (*i, b.to_vec()),
            _ => panic!("int key"),
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (40, b"d".to_vec()),
            (30, b"c".to_vec()),
            (20, b"b".to_vec()),
            (10, b"a".to_vec()),
        ],
        "descending, half-open at the top (50 excluded)"
    );

    // Laziness: taking 2 does not touch the rest — and it stops at the newest.
    let top2: Vec<i64> = idx
        .iter_desc_below(&IndexKey::Int(i64::MAX))
        .take(2)
        .map(|(k, _)| match k {
            IndexKey::Int(i) => *i,
            _ => 0,
        })
        .collect();
    assert_eq!(top2, vec![50, 40], "newest two, without scanning the rest");

    assert_eq!(
        idx.max_key_below(&IndexKey::Int(45)),
        Some(&IndexKey::Int(40))
    );
    assert_eq!(
        idx.max_key_below(&IndexKey::Int(5)),
        None,
        "nothing below 5"
    );
}

#[test]
fn range_index_serialisation_round_trips_and_detects_corruption() {
    let s = Store::new();
    for (body, v) in [(&b"a"[..], 10i64), (b"b", 20), (b"c", 30)] {
        put_row(&s, body, int64(v));
    }
    put_row(&s, b"neg", int64(-5));
    put_row(&s, b"name", string("Music"));
    let idx = RangeIndex::build(&s, &group(), IndexDef::new(1, PROP), s.now_ts());

    let bytes = idx.to_bytes();
    let back = RangeIndex::from_bytes(&bytes, IndexDef::new(1, PROP)).expect("round-trips");
    let full = |i: &RangeIndex| {
        i.range(&IndexKey::Int(i64::MIN), &IndexKey::Str(vec![0xFF; 64]))
            .bodies
    };
    assert_eq!(
        full(&idx),
        full(&back),
        "round-tripped index answers differ"
    );
    assert_eq!(back.len(), idx.len());

    // A flipped byte in the body fails the BLAKE3 check → rejected.
    let mut bad = bytes.clone();
    bad[24] ^= 0xFF;
    assert!(
        RangeIndex::from_bytes(&bad, IndexDef::new(1, PROP)).is_none(),
        "a corrupt index file must be rejected, not trusted"
    );
    // Truncation → rejected.
    assert!(RangeIndex::from_bytes(&bytes[..20], IndexDef::new(1, PROP)).is_none());
}

// ─── the removal overlay is BUCKETED ────────────────────────────────────────
//
// `with_changes` used to clone the whole `removed` BTreeSet on every catch-up.
// Bounded by FOLD_AT that is up to 4,096 `Vec<u8>` clones per call, so the
// per-write cost sawtoothed from ~0 up to 4,096 and back — a cost that GROWS
// with ops executed.
//
// That sat unnoticed while the index was consulted about once per benchmark
// level. Letting a multi-key pattern map seek a declared index moved it to
// once per OPERATION on a create/delete churn, which would have moved the
// decay rather than removing it. So the two land together, and this is the
// half that proves the bucket answers identically to the set it replaced.

/// The bucketed overlay must answer EXACTLY what a fresh build answers, over a
/// long churn — not merely "the same set", but the same ordered bodies.
#[test]
fn a_bucketed_removal_overlay_answers_exactly_like_a_rebuild() {
    let s = Store::new();
    let def = IndexDef::new(1, PROP);
    // A base big enough that folding and bucket merges both happen.
    for i in 0..2_000i64 {
        put_row(&s, &i.to_be_bytes(), int64(i));
    }
    let mut idx = RangeIndex::build(&s, &group(), def.clone(), s.now_ts());

    // Churn: delete a row, add a new one, repeatedly — the shape that grows
    // `removed` and the shape the delete profile actually runs.
    for round in 0..3_000i64 {
        let victim = round % 2_000;
        let body = victim.to_be_bytes().to_vec();
        s.delete(&group(), &body);
        let fresh = 10_000 + round;
        let ts = put_row(&s, &fresh.to_be_bytes(), int64(fresh));

        let mut changes = std::collections::BTreeMap::new();
        changes.insert(body, None);
        changes.insert(fresh.to_be_bytes().to_vec(), Some(IndexKey::Int(fresh)));
        idx = idx
            .with_changes(&changes, ts)
            .expect("orderable rows carry forward");

        // Every 250 rounds, prove the carried-forward index still equals a
        // rebuild at the same instant. Checking every round would make the
        // test O(rounds * base) and prove nothing extra.
        if round % 250 == 0 {
            let rebuilt = RangeIndex::build(&s, &group(), def.clone(), ts);
            let lo = IndexKey::Int(i64::MIN);
            let hi = IndexKey::Int(i64::MAX);
            assert_eq!(
                idx.range(&lo, &hi).bodies,
                rebuilt.range(&lo, &hi).bodies,
                "round {round}: the bucketed overlay must answer exactly what a \
                 rebuild does, in the same order"
            );
            assert_eq!(
                idx.len(),
                rebuilt.len(),
                "round {round}: and report the same live count"
            );
        }
    }
}

/// The bucket must make the O(|removed|) merge RARE, not merely correct — a
/// version that merged every call would pass the equality test above while
/// leaving the cost it was written to remove.
#[test]
fn the_removal_bucket_makes_the_merge_rare() {
    let s = Store::new();
    let def = IndexDef::new(1, PROP);
    for i in 0..2_000i64 {
        put_row(&s, &i.to_be_bytes(), int64(i));
    }
    let mut idx = RangeIndex::build(&s, &group(), def, s.now_ts());

    const ROUNDS: i64 = 1_000;
    let (_, trace) = engram_observe::with_trace(|| {
        for round in 0..ROUNDS {
            let body = (round % 2_000).to_be_bytes().to_vec();
            let mut changes = std::collections::BTreeMap::new();
            changes.insert(body, None);
            idx = idx
                .with_changes(&changes, s.now_ts() + round as u64 + 1)
                .expect("orderable");
        }
    });
    let merges = trace
        .counters()
        .get("index.overlay removal buckets merged")
        .copied()
        .unwrap_or(0);
    eprintln!("[removal bucket] {ROUNDS} single-removal catch-ups caused {merges} merge(s)");
    assert!(
        merges * 32 < ROUNDS as u64,
        "{merges} merges for {ROUNDS} changes is not amortised — the bucket must \
         absorb runs of removals, or the per-call clone is still O(|removed|)"
    );
}
