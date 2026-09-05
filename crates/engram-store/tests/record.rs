#![allow(non_snake_case)]
//! The record layer, and the rule that matters most in it: a WRITER preserves
//! what it does not understand.

use engram_key::value::Tag;
use engram_store::{PropertyId, Record, RecordError, get_property};

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

/// A tagged value whose tag THIS BUILD has never assigned (extension block).
fn unknown_value() -> Vec<u8> {
    let mut out = vec![0xE4];
    out.extend_from_slice(&3u32.to_le_bytes());
    out.extend_from_slice(&[7, 7, 7]);
    out
}

#[test]
fn a_record_round_trips() {
    let mut r = Record::new();
    r.set(PropertyId(1), int64(42));
    r.set(PropertyId(9), string("title"));
    let decoded = Record::decode(&r.encode()).expect("decodes");
    assert_eq!(decoded, r);
    assert_eq!(decoded.get(PropertyId(1)), Some(int64(42).as_slice()));
}

#[test]
fn THE_RULE_read_modify_write_preserves_an_unknown_property() {
    // The half of forward compatibility that usually gets lost. A newer build
    // writes property 50 with a tag this build has never seen; this build then
    // does an ordinary update to property 1. If the unknown property does not
    // survive that round trip, the FIRST old-build write after a new-build
    // write destroys the new build's data — and nothing errors anywhere, which
    // is what makes it the failure worth a named test.
    let mut newer = Record::new();
    newer.set(PropertyId(1), int64(1));
    newer.set(PropertyId(50), unknown_value());
    let on_disk = newer.encode();

    // The older build's update:
    let mut ours = Record::decode(&on_disk).expect("unknown property must not fail the decode");
    ours.set(PropertyId(1), int64(2));
    let rewritten = ours.encode();

    // The newer build reads its property back, intact.
    let reread = Record::decode(&rewritten).expect("decodes");
    assert_eq!(
        reread.get(PropertyId(50)),
        Some(unknown_value().as_slice()),
        "the unknown property did not survive a read-modify-write",
    );
    assert_eq!(reread.get(PropertyId(1)), Some(int64(2).as_slice()));
}

#[test]
fn get_property_reads_one_without_decoding_the_rest() {
    // The skip rule's payoff at the record layer — and it must work even when
    // the properties BEFORE the target are unknown, because that is the case
    // where "decode everything then pick one" would fail.
    let mut r = Record::new();
    r.set(PropertyId(3), unknown_value());
    r.set(PropertyId(7), int64(99));
    let buf = r.encode();

    assert_eq!(get_property(&buf, PropertyId(7)), Some(int64(99)));
    assert_eq!(get_property(&buf, PropertyId(3)), Some(unknown_value()));
    assert_eq!(get_property(&buf, PropertyId(999)), None);
}

#[test]
fn a_duplicate_property_is_a_CONFLICT_not_a_merge() {
    // Two writers disagreeing about one property. Last-wins would be a lost
    // update wearing the shape of a merge; the decode refuses instead.
    let mut buf = 2u32.to_le_bytes().to_vec();
    buf.extend_from_slice(&5u32.to_le_bytes());
    buf.extend_from_slice(&int64(1));
    buf.extend_from_slice(&5u32.to_le_bytes());
    buf.extend_from_slice(&int64(2));
    assert_eq!(
        Record::decode(&buf),
        Err(RecordError::DuplicateProperty(PropertyId(5)))
    );
}

#[test]
fn trailing_bytes_after_the_declared_count_are_refused() {
    // The count and the content disagree; trusting either alone hides the
    // other's corruption.
    let mut r = Record::new();
    r.set(PropertyId(1), int64(1));
    let mut buf = r.encode();
    buf.extend_from_slice(&[0xAA]);
    assert!(matches!(
        Record::decode(&buf),
        Err(RecordError::Truncated { .. })
    ));
}

#[test]
fn a_truncated_record_is_refused_with_the_offset() {
    let mut r = Record::new();
    r.set(PropertyId(1), string("hello world"));
    let buf = r.encode();
    for cut in [3usize, 6, 10, buf.len() - 1] {
        assert!(
            Record::decode(&buf[..cut]).is_err(),
            "a {cut}-byte record decoded"
        );
    }
}

#[test]
fn an_empty_record_is_valid_and_distinct_from_absent() {
    // A node with zero properties exists. Empty-record and no-record are
    // different facts — the same absent-vs-empty distinction the value layer
    // draws with the NULL tag, one level up.
    let r = Record::new();
    let decoded = Record::decode(&r.encode()).expect("empty decodes");
    assert!(decoded.is_empty());
}
