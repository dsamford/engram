#![allow(non_snake_case)]
//! Artifact #1's properties.
//!
//! The load-bearing test here is not "it round-trips" — it is that **byte order
//! equals logical order for every pair**. Round-tripping is satisfied by an
//! encoding with no ordering property at all, and an LSM built on one would
//! read and write correctly while every range scan returned the wrong rows.

use engram_key::{
    COMMIT_TS_LEN, CellId, EntityId, KeyPrefix, Kind, KindBlock, Namespace, PREFIX_LEN, Partition,
    Realm, Structural, decode_commit_ts, decode_var_bytes, encode_commit_ts, encode_key,
    encode_var_bytes, split_key,
};

fn prefix(realm: u32, ns: u32, kind: Kind, part: u32) -> KeyPrefix {
    KeyPrefix {
        realm: Realm(realm),
        namespace: Namespace(ns),
        kind,
        partition: Partition(part),
    }
}

// ─── The frozen layout ──────────────────────────────────────────────────────

#[test]
fn GOLDEN_the_encoded_bytes_are_frozen() {
    // These bytes are the on-disk format. If this test fails, either the layout
    // moved — which is unfixable once data exists — or the change was
    // deliberate and this vector is the diff that makes it reviewable. There is
    // no third case, which is why the expectation is written out byte by byte
    // rather than computed.
    let key = encode_key(
        &prefix(1, 2, Kind::NODE, 3),
        &EntityId(7).to_body(),
        0x0102_0304_0506_0708,
    );

    assert_eq!(
        key,
        vec![
            0x00, 0x00, 0x00, 0x01, // realm      = 1, big-endian
            0x00, 0x00, 0x00, 0x02, // namespace  = 2
            0x01, //                   KIND       = NODE
            0x00, 0x00, 0x00, 0x03, // partition  = 3
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07, // body: entity 7
            // !0x0102030405060708 — INVERTED, so newer sorts first.
            0xFE, 0xFD, 0xFC, 0xFB, 0xFA, 0xF9, 0xF8, 0xF7,
        ],
    );
    assert_eq!(PREFIX_LEN, 13);
    assert_eq!(COMMIT_TS_LEN, 8);
}

trait Body {
    fn to_body(&self) -> Vec<u8>;
}
impl Body for EntityId {
    fn to_body(&self) -> Vec<u8> {
        let mut v = Vec::new();
        self.encode_into(&mut v);
        v
    }
}

// ─── Memcomparability ───────────────────────────────────────────────────────

#[test]
fn byte_order_equals_logical_order_across_every_component() {
    // Ordered by the frozen tuple: realm, then namespace, then KIND, then
    // partition, then body. Each row must sort before the next BY BYTES.
    let ordered = [
        (0u32, 0u32, Kind::NODE, 0u32, 0u64),
        (0, 0, Kind::NODE, 0, 1),
        (0, 0, Kind::NODE, 1, 0),
        (0, 0, Kind::EDGE, 0, 0),
        (0, 1, Kind::NODE, 0, 0),
        (1, 0, Kind::NODE, 0, 0),
    ];

    let encoded: Vec<Vec<u8>> = ordered
        .iter()
        .map(|(r, n, k, p, e)| encode_key(&prefix(*r, *n, *k, *p), &EntityId(*e).to_body(), 0))
        .collect();

    for w in encoded.windows(2) {
        assert!(
            w[0] < w[1],
            "byte order disagrees with logical order: {:?} !< {:?}",
            w[0],
            w[1]
        );
    }
}

#[test]
fn a_high_u32_still_sorts_after_a_low_one() {
    // The test a little-endian encoding fails. 0x00000100 vs 0x00000001: LE
    // puts 0x00,0x01,0x00,0x00 against 0x01,0x00,0x00,0x00 and gets it exactly
    // backwards, while every round-trip test still passes.
    let lo = encode_key(&prefix(1, 0, Kind::NODE, 0), &[], 0);
    let hi = encode_key(&prefix(256, 0, Kind::NODE, 0), &[], 0);
    assert!(lo < hi);
}

#[test]
fn NEWER_versions_sort_FIRST() {
    // The whole reason `commit_ts` is inverted. Without it, reading "the
    // current value" means scanning a version chain to its end — and the older
    // the row, the more it costs, which is the opposite of what a workload
    // wants.
    let older = encode_key(&prefix(1, 1, Kind::NODE, 1), &EntityId(1).to_body(), 100);
    let newer = encode_key(&prefix(1, 1, Kind::NODE, 1), &EntityId(1).to_body(), 200);
    assert!(
        newer < older,
        "a newer version must sort before an older one"
    );
}

#[test]
fn the_version_chain_of_one_entity_is_contiguous() {
    // Everything for entity 1 must sort together, with nothing from entity 2
    // interleaved — otherwise a point read is not a prefix scan.
    let mut keys: Vec<Vec<u8>> = Vec::new();
    for entity in [1u64, 2] {
        for ts in [10u64, 20, 30] {
            keys.push(encode_key(
                &prefix(1, 1, Kind::NODE, 1),
                &EntityId(entity).to_body(),
                ts,
            ));
        }
    }
    keys.sort();

    let entity_of = |k: &[u8]| -> u64 {
        let (_, body, _) = split_key(k).expect("valid key");
        u64::from_be_bytes(body.try_into().expect("8-byte body"))
    };
    let order: Vec<u64> = keys.iter().map(|k| entity_of(k)).collect();
    assert_eq!(order, vec![1, 1, 1, 2, 2, 2]);
}

#[test]
fn a_partition_is_a_contiguous_range() {
    // Partition sits BEFORE the entity id for exactly this reason. Reversed,
    // one partition's rows would be scattered across the whole entity space and
    // a partition scan would become a full scan that happens to filter.
    let mut keys = Vec::new();
    for part in [1u32, 2] {
        for entity in [10u64, 20] {
            keys.push(encode_key(
                &prefix(1, 1, Kind::NODE, part),
                &EntityId(entity).to_body(),
                0,
            ));
        }
    }
    keys.sort();
    let parts: Vec<u32> = keys
        .iter()
        .map(|k| split_key(k).expect("valid").0.partition.0)
        .collect();
    assert_eq!(parts, vec![1, 1, 2, 2]);
}

#[test]
fn every_realm_is_a_contiguous_prefix() {
    // The isolation boundary, and the DEK derivation, are both this property.
    let mut keys = Vec::new();
    for realm in [7u32, 9] {
        for ns in [1u32, 2] {
            keys.push(encode_key(&prefix(realm, ns, Kind::NODE, 0), &[], 0));
        }
    }
    keys.sort();
    let realms: Vec<u32> = keys
        .iter()
        .map(|k| split_key(k).expect("valid").0.realm.0)
        .collect();
    assert_eq!(realms, vec![7, 7, 9, 9]);
}

// ─── Round-trip ─────────────────────────────────────────────────────────────

#[test]
fn split_key_recovers_prefix_body_and_ts() {
    let body = EntityId(0xDEAD_BEEF).to_body();
    let key = encode_key(&prefix(11, 22, Kind::EDGE, 33), &body, 999);
    let (p, b, ts) = split_key(&key).expect("valid key");
    assert_eq!(p.realm, Realm(11));
    assert_eq!(p.namespace, Namespace(22));
    assert_eq!(p.kind, Kind::EDGE);
    assert_eq!(p.partition, Partition(33));
    assert_eq!(b, &body[..]);
    assert_eq!(ts, 999);
}

#[test]
fn a_truncated_key_is_REFUSED_not_guessed() {
    // A key decoded on a guess is a row attributed to the wrong tenant, which
    // is a cross-tenant read that no audit would flag because the row looks
    // entirely normal once it has been mis-attributed.
    let key = encode_key(&prefix(1, 1, Kind::NODE, 1), &[], 1);
    for cut in 0..key.len() {
        assert!(split_key(&key[..cut]).is_none(), "a {cut}-byte key decoded");
    }
    assert!(split_key(&key).is_some());
}

#[test]
fn KeyPrefix_decode_refuses_a_short_buffer_ON_ITS_OWN() {
    // Found by a canary. The truncation test above passes even with
    // `KeyPrefix::decode`'s guard removed, because `split_key`'s own length
    // check catches the same inputs first — so the guard was untested and a
    // DIRECT caller of `decode` would have received a fabricated zeroed prefix:
    // realm 0, namespace 0, partition 0. That is a real tenant, and a row
    // attributed to it looks entirely normal.
    //
    // Two guards covering one input is not the same as two tested guards.
    let mut full = Vec::new();
    KeyPrefix {
        realm: Realm(5),
        namespace: Namespace(6),
        kind: Kind::NODE,
        partition: Partition(7),
    }
    .encode_into(&mut full);
    assert_eq!(full.len(), PREFIX_LEN);

    for cut in 0..PREFIX_LEN {
        assert!(
            KeyPrefix::decode(&full[..cut]).is_none(),
            "decode accepted a {cut}-byte buffer and invented the missing fields",
        );
    }
    assert!(KeyPrefix::decode(&full).is_some());
}

#[test]
fn commit_ts_round_trips_at_the_boundaries() {
    for ts in [0u64, 1, u64::MAX - 1, u64::MAX] {
        let mut v = Vec::new();
        encode_commit_ts(ts, &mut v);
        assert_eq!(decode_commit_ts(&v), Some(ts), "ts {ts} did not round-trip");
    }
}

// ─── FC-3: the unescaped fixed-width component ──────────────────────────────

#[test]
fn a_cell_id_is_written_VERBATIM() {
    // FC-3. A Z-order or Hilbert cell id carries its locality in its exact bit
    // layout; escaping would insert bytes that are not part of the curve and
    // the spatial ordering would not survive into the key. The 0x00 below is
    // the one an escaping encoder would have expanded.
    let mut out = Vec::new();
    CellId([0x12, 0x00, 0x34, 0xFF]).encode_into(&mut out);
    assert_eq!(out, vec![0x12, 0x00, 0x34, 0xFF]);
}

#[test]
fn cell_ids_preserve_curve_order() {
    let mut a = Vec::new();
    let mut b = Vec::new();
    CellId([0x00, 0x01]).encode_into(&mut a);
    CellId([0x00, 0x02]).encode_into(&mut b);
    assert!(a < b);
}

// ─── Variable-length escaping ───────────────────────────────────────────────

#[test]
fn a_prefix_sorts_before_a_longer_value_that_extends_it() {
    // The prefix ambiguity escaping exists to close. Unescaped, `[1]` followed
    // by a later component compares that component's bytes against `[1, 0]`'s
    // own payload, and the answer depends on data unrelated to the comparison.
    let mut short = Vec::new();
    let mut long = Vec::new();
    encode_var_bytes(&[1], &mut short);
    encode_var_bytes(&[1, 0], &mut long);
    // Something follows, as it always does in a real key.
    short.extend_from_slice(&[0xFF, 0xFF]);
    long.extend_from_slice(&[0x00, 0x00]);
    assert!(
        short < long,
        "a prefix must sort before the value that extends it"
    );
}

#[test]
fn an_embedded_zero_does_not_terminate_the_component() {
    let mut out = Vec::new();
    encode_var_bytes(&[0x00, 0x41, 0x00], &mut out);
    let (payload, n) = decode_var_bytes(&out).expect("decodes");
    assert_eq!(payload, vec![0x00, 0x41, 0x00]);
    assert_eq!(n, out.len());
}

#[test]
fn var_bytes_ordering_matches_payload_ordering() {
    let mut samples: Vec<Vec<u8>> = vec![
        vec![],
        vec![0x00],
        vec![0x00, 0x00],
        vec![0x01],
        vec![0x01, 0x00],
        vec![0xFF],
    ];
    samples.sort();
    let encoded: Vec<Vec<u8>> = samples
        .iter()
        .map(|p| {
            let mut v = Vec::new();
            encode_var_bytes(p, &mut v);
            v
        })
        .collect();
    for (i, w) in encoded.windows(2).enumerate() {
        assert!(
            w[0] < w[1],
            "pair {i} out of order: {:?} vs {:?}",
            samples[i],
            samples[i + 1]
        );
    }
}

#[test]
fn a_corrupt_escape_is_REFUSED() {
    // 0x00 followed by anything but 0xFF or 0x01 is not a component this
    // encoder produced. Guessing would silently truncate a key.
    assert!(decode_var_bytes(&[0x41, 0x00, 0x42]).is_none());
    assert!(
        decode_var_bytes(&[0x41]).is_none(),
        "an unterminated component decoded"
    );
}

// ─── FC-1: the KIND registry ────────────────────────────────────────────────

#[test]
fn the_reserved_blocks_are_frozen() {
    assert_eq!(Kind::from_byte(0x00).block(), KindBlock::Invalid);
    assert_eq!(Kind::from_byte(0x01).block(), KindBlock::Core);
    assert_eq!(Kind::from_byte(0x3F).block(), KindBlock::Core);
    assert_eq!(Kind::from_byte(0x40).block(), KindBlock::ReservedCore);
    assert_eq!(Kind::from_byte(0x7F).block(), KindBlock::ReservedCore);
    assert_eq!(Kind::from_byte(0x80).block(), KindBlock::Protected);
    assert_eq!(Kind::from_byte(0xBF).block(), KindBlock::Protected);
    assert_eq!(Kind::from_byte(0xC0).block(), KindBlock::Extension);
    assert_eq!(Kind::from_byte(0xFE).block(), KindBlock::Extension);
    assert_eq!(Kind::from_byte(0xFF).block(), KindBlock::Escape);
}

#[test]
fn a_zero_byte_is_NOT_a_valid_kind() {
    // The most likely corruption is a zeroed byte. It must not decode as a real
    // kind, or a corrupt key becomes a plausible one.
    assert!(!Kind::from_byte(0x00).is_valid());
    assert!(Kind::NODE.is_valid());
}

#[test]
fn an_UNKNOWN_kind_in_the_protected_block_is_still_protected() {
    // The direction that matters. Defaulting an unrecognised kind to
    // unprotected would make forward compatibility and a plaintext leak the
    // same event — an older reader would accept a plaintext put to a kind a
    // newer writer protects.
    assert!(
        Kind::from_byte(0x9A).is_protected(),
        "an unknown protected-block kind read as unprotected"
    );
    assert!(Kind::PROTECTED_PROPERTY.is_protected());
    assert!(!Kind::NODE.is_protected());
    assert!(!Kind::from_byte(0xC5).is_protected());
}

#[test]
fn the_escape_value_is_reserved_and_unused() {
    // Spent now so it is available later. One byte gives 256 kinds and the
    // encoding is frozen; this is the only moment the ceiling can be lifted.
    assert_eq!(engram_key::ESCAPE_KIND, 0xFF);
    assert_eq!(
        Kind::from_byte(engram_key::ESCAPE_KIND).block(),
        KindBlock::Escape
    );
    assert!(!Kind::from_byte(engram_key::ESCAPE_KIND).is_protected());
}
