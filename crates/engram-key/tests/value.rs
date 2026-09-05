#![allow(non_snake_case)]
//! FC-4/5/6 — the value tag registry, and the skip rule it turns on.
//!
//! The decisive tests here use tags THIS BUILD HAS NEVER ASSIGNED. FC-4's
//! wording is "a skip-unknown rule that actually holds", and a rule verified
//! only against known tags holds by coincidence: every known tag has a decoder,
//! so the skip path for the unknown ones — the only ones the rule exists for —
//! would be the one path with no coverage.

use engram_key::value::{
    ESCAPE_TAG, PayloadShape, Point2D, Point3D, Tag, TagBlock, TagHistogram, payload_shape,
    skip_value,
};

// ─── The frozen blocks ──────────────────────────────────────────────────────

#[test]
fn the_tag_blocks_are_frozen() {
    assert_eq!(Tag::from_byte(0x00).block(), TagBlock::Invalid);
    assert_eq!(Tag::from_byte(0x01).block(), TagBlock::Core);
    assert_eq!(Tag::from_byte(0x3F).block(), TagBlock::Core);
    assert_eq!(Tag::from_byte(0x40).block(), TagBlock::ReservedCore);
    assert_eq!(Tag::from_byte(0x6F).block(), TagBlock::ReservedCore);
    assert_eq!(Tag::from_byte(0x70).block(), TagBlock::Spatial);
    assert_eq!(Tag::from_byte(0x9F).block(), TagBlock::Spatial);
    assert_eq!(Tag::from_byte(0xA0).block(), TagBlock::Vector);
    assert_eq!(Tag::from_byte(0xCF).block(), TagBlock::Vector);
    assert_eq!(Tag::from_byte(0xD0).block(), TagBlock::Extension);
    assert_eq!(Tag::from_byte(0xFE).block(), TagBlock::Extension);
    assert_eq!(Tag::from_byte(0xFF).block(), TagBlock::Escape);
}

#[test]
fn FC6_point_and_vector_live_in_DIFFERENT_blocks() {
    // A Point2D and a 2-dim f64 vector have the SAME bytes. Only the tag
    // separates a coordinate that feeds a space-filling curve from an embedding
    // that feeds a similarity index — misread one as the other and every
    // downstream number is plausible and wrong, which is why the plan calls
    // conflation unrecoverable.
    assert_eq!(Tag::POINT_2D.block(), TagBlock::Spatial);
    assert_eq!(Tag::POINT_3D.block(), TagBlock::Spatial);
    assert_eq!(Tag::VECTOR_F32.block(), TagBlock::Vector);
    assert_eq!(Tag::VECTOR_F64.block(), TagBlock::Vector);
    assert_eq!(Tag::VECTOR_I8.block(), TagBlock::Vector);
    assert_ne!(Tag::POINT_2D.block(), Tag::VECTOR_F64.block());
}

// ─── FC-5: POINT, exact lengths ─────────────────────────────────────────────

#[test]
fn GOLDEN_point2d_payload_is_exactly_20_bytes() {
    // srid:u32 + x:f64 + y:f64. Written out so a length change is a visible
    // diff — FC-5 is "written down now, with correct lengths".
    let p = Point2D {
        srid: 4326,
        x: 1.5,
        y: -2.5,
    };
    let enc = p.encode();
    assert_eq!(enc.len(), 21, "tag + 20 payload bytes");
    assert_eq!(enc[0], 0x70);
    assert_eq!(&enc[1..5], &4326u32.to_le_bytes());
    assert_eq!(payload_shape(Tag::POINT_2D), Some(PayloadShape::Fixed(20)));
}

#[test]
fn GOLDEN_point3d_payload_is_exactly_28_bytes() {
    let p = Point3D {
        srid: 4979,
        x: 1.0,
        y: 2.0,
        z: 3.0,
    };
    assert_eq!(p.encode().len(), 29);
    assert_eq!(payload_shape(Tag::POINT_3D), Some(PayloadShape::Fixed(28)));
}

#[test]
fn points_round_trip_including_srid() {
    // The SRID is part of the value, not metadata: 4326 (degrees) and 7203
    // (cartesian metres) make the same (x, y) mean different places.
    let p2 = Point2D {
        srid: 7203,
        x: -0.0,
        y: f64::MAX,
    };
    assert_eq!(Point2D::decode(&p2.encode()), Some(p2));
    let p3 = Point3D {
        srid: 4979,
        x: 1.25,
        y: -9.75,
        z: 0.5,
    };
    assert_eq!(Point3D::decode(&p3.encode()), Some(p3));
}

#[test]
fn a_point_decoder_refuses_the_OTHER_point_tag() {
    // The cheapest place conflation could re-enter: a 3D point truncated to 20
    // payload bytes has a valid 2D length. The tag check is what refuses it.
    let p3 = Point3D {
        srid: 1,
        x: 1.0,
        y: 2.0,
        z: 3.0,
    };
    let mut truncated = p3.encode();
    truncated.truncate(21);
    assert_eq!(
        Point2D::decode(&truncated),
        None,
        "a truncated 3D point decoded as 2D"
    );
}

// ─── The skip rule ──────────────────────────────────────────────────────────

#[test]
fn skips_every_known_fixed_tag_by_its_frozen_length() {
    for (tag, payload) in [
        (Tag::NULL, 0usize),
        (Tag::BOOL, 1),
        (Tag::INT64, 8),
        (Tag::FLOAT64, 8),
        (Tag::DATE, 8),
        (Tag::TIME, 12),
        (Tag::LOCAL_TIME, 8),
        (Tag::DATETIME_OFFSET, 16),
        (Tag::LOCAL_DATETIME, 12),
        (Tag::DURATION, 28),
        (Tag::POINT_2D, 20),
        (Tag::POINT_3D, 28),
    ] {
        let mut buf = vec![tag.byte()];
        buf.extend(std::iter::repeat_n(0xAB, payload));
        assert_eq!(
            skip_value(&buf),
            Some(1 + payload),
            "tag 0x{:02x}",
            tag.byte()
        );
    }
}

#[test]
fn skips_a_length_prefixed_value() {
    let mut buf = vec![Tag::STRING.byte()];
    buf.extend_from_slice(&5u32.to_le_bytes());
    buf.extend_from_slice(b"hello");
    buf.extend_from_slice(b"TRAILING");
    assert_eq!(skip_value(&buf), Some(1 + 4 + 5));
}

#[test]
fn THE_RESERVATION_an_unknown_tag_is_skippable_by_its_block() {
    // The test FC-4 exists for. 0x4A (reserved core), 0x7D (spatial), 0xB3
    // (vector), 0xE1 (extension): none has ever been assigned, and a reader
    // must step over all of them by the block's length-prefixed shape. If this
    // fails, every new value type is a format break for every deployed reader,
    // and the reserved blocks were decorative.
    for unknown in [0x4Au8, 0x7D, 0xB3, 0xE1] {
        let mut buf = vec![unknown];
        buf.extend_from_slice(&3u32.to_le_bytes());
        buf.extend_from_slice(&[9, 9, 9]);
        buf.extend_from_slice(b"NEXT VALUE");
        assert_eq!(
            skip_value(&buf),
            Some(1 + 4 + 3),
            "unknown tag 0x{unknown:02x} was not skippable",
        );
    }
}

#[test]
fn the_escape_tag_is_skippable_WITHOUT_understanding_the_real_tag() {
    // u16 real tag + u32 length + payload. The real tag below (0x0142) belongs
    // to no registry that exists yet — which is the point.
    let mut buf = vec![ESCAPE_TAG];
    buf.extend_from_slice(&0x0142u16.to_le_bytes());
    buf.extend_from_slice(&4u32.to_le_bytes());
    buf.extend_from_slice(&[1, 2, 3, 4]);
    assert_eq!(skip_value(&buf), Some(1 + 2 + 4 + 4));
}

#[test]
fn a_packed_list_is_skipped_by_count_times_element_width() {
    let mut buf = vec![Tag::LIST.byte()];
    buf.extend_from_slice(&3u32.to_le_bytes());
    buf.push(Tag::INT64.byte());
    buf.extend(std::iter::repeat_n(0u8, 24));
    assert_eq!(skip_value(&buf), Some(1 + 4 + 1 + 24));
}

#[test]
fn a_packed_list_of_variable_elements_is_UNREPRESENTABLE() {
    // Packed elements carry no per-element length, so a variable element would
    // make the list unskippable — the exact hole the rule exists to close. The
    // encoder never produces this; the decoder REFUSES it rather than guessing.
    let mut buf = vec![Tag::LIST.byte()];
    buf.extend_from_slice(&2u32.to_le_bytes());
    buf.push(Tag::STRING.byte());
    buf.extend_from_slice(&[0; 32]);
    assert_eq!(skip_value(&buf), None);
}

#[test]
fn a_truncated_value_is_REFUSED_never_guessed() {
    // A mis-skip desynchronises every property AFTER the damaged one, so the
    // failure surfaces as corruption arbitrarily far from its cause. Refusal
    // keeps the blast radius at the value that is actually broken.
    let mut string = vec![Tag::STRING.byte()];
    string.extend_from_slice(&100u32.to_le_bytes());
    string.extend_from_slice(b"short");
    assert_eq!(skip_value(&string), None);

    let point = vec![Tag::POINT_2D.byte(), 0, 0, 0];
    assert_eq!(skip_value(&point), None);

    assert_eq!(skip_value(&[]), None);
    assert_eq!(
        skip_value(&[0x00]),
        None,
        "the invalid tag skipped as a value"
    );
}

#[test]
fn a_huge_declared_length_cannot_overflow_the_skip() {
    // count * per_element with an attacker-controlled count. The arithmetic is
    // done in u64, where u32::MAX * 28 cannot wrap on ANY target — a canary
    // showed the earlier checked-arithmetic version was untestable on a 64-bit
    // host (the wrap it guarded was unreachable), so the property was made
    // structural. This test now pins the refusal, not the mechanism.
    let mut buf = vec![Tag::LIST.byte()];
    buf.extend_from_slice(&u32::MAX.to_le_bytes());
    buf.push(Tag::INT64.byte());
    assert_eq!(skip_value(&buf), None);
}

#[test]
fn a_stream_of_values_walks_cleanly_over_an_unknown_one() {
    // The end-to-end shape: known, UNKNOWN, known. The middle value is from a
    // build that does not exist yet; the reader must still reach the third.
    let mut buf = Vec::new();
    buf.push(Tag::BOOL.byte());
    buf.push(1);
    buf.push(0xE7); // unknown extension tag
    buf.extend_from_slice(&2u32.to_le_bytes());
    buf.extend_from_slice(&[0xFE, 0xFF]);
    buf.push(Tag::INT64.byte());
    buf.extend_from_slice(&42i64.to_le_bytes());

    let mut offset = 0;
    let mut tags = Vec::new();
    while offset < buf.len() {
        tags.push(buf[offset]);
        offset += skip_value(&buf[offset..]).expect("every value skippable");
    }
    assert_eq!(offset, buf.len(), "the walk must land exactly on the end");
    assert_eq!(tags, vec![Tag::BOOL.byte(), 0xE7, Tag::INT64.byte()]);
}

// ─── FC-10: one tag vocabulary ──────────────────────────────────────────────

#[test]
fn the_histogram_counts_by_the_SAME_tags_the_encoder_writes() {
    let mut h = TagHistogram::new();
    h.record(Tag::STRING);
    h.record(Tag::STRING);
    h.record(Tag::POINT_2D);
    assert_eq!(h.count(Tag::STRING), 2);
    assert_eq!(h.count(Tag::POINT_2D), 1);
    assert_eq!(h.count(Tag::VECTOR_F32), 0);
    assert_eq!(h.total(), 3);
}

#[test]
fn unknown_tags_are_COUNTED_not_folded_into_other() {
    // An "other" bucket hides growth: a new tag's rollout would read as a
    // static miscellany instead of a curve. Keying by the raw byte keeps every
    // unknown tag its own series.
    let mut h = TagHistogram::new();
    h.record(Tag::from_byte(0xE7));
    h.record(Tag::from_byte(0xE7));
    h.record(Tag::from_byte(0xB3));
    assert_eq!(h.count(Tag::from_byte(0xE7)), 2);
    assert_eq!(h.count(Tag::from_byte(0xB3)), 1);
    let listed: Vec<u8> = h.entries().map(|(t, _)| t.byte()).collect();
    assert_eq!(listed, vec![0xB3, 0xE7]);
}

#[test]
fn a_VECTOR_value_is_skippable_and_its_prefix_is_BYTES() {
    // Found by the vector index's tests: the vector tags were documented with
    // a DIMENSION prefix, but skip_value steps length-prefixed payloads by
    // BYTES — it knows no element width. A dim prefix desynchronised every
    // record walk containing a vector, and get_property reported the property
    // absent: the store's index built empty over a fully populated group.
    // The skip rule's contract wins; the dimension is derived from the length.
    let mut buf = vec![Tag::VECTOR_F32.byte()];
    buf.extend_from_slice(&8u32.to_le_bytes()); // 8 BYTES = two f32s
    buf.extend_from_slice(&1.0f32.to_le_bytes());
    buf.extend_from_slice(&2.0f32.to_le_bytes());
    buf.extend_from_slice(b"NEXT");
    assert_eq!(
        skip_value(&buf),
        Some(1 + 4 + 8),
        "the prefix must be bytes, not dim"
    );
}
