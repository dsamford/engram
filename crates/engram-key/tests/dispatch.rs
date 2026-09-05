#![allow(non_snake_case)]
//! FC-2 — KIND-dispatched body decoding, and what the outcomes have to keep apart.

use engram_key::kind::{BodyOutcome, DecodedBody, KindError, KindRegistry};
use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm, encode_key, split_key};

fn node_body(b: &[u8]) -> Option<DecodedBody> {
    if b.len() != 8 {
        return None;
    }
    let id = u64::from_be_bytes(b.try_into().ok()?);
    Some(DecodedBody {
        summary: format!("node {id}"),
        consumed: 8,
    })
}

fn greedy_but_short(b: &[u8]) -> Option<DecodedBody> {
    Some(DecodedBody {
        summary: "partial".into(),
        consumed: b.len().saturating_sub(1),
    })
}

#[test]
fn a_registered_decoder_reads_its_own_body() {
    let mut reg = KindRegistry::new();
    reg.register(Kind::NODE, node_body).expect("registers");

    let key = encode_key(
        &KeyPrefix {
            realm: Realm(1),
            namespace: Namespace(1),
            kind: Kind::NODE,
            partition: Partition(1),
        },
        &42u64.to_be_bytes(),
        7,
    );
    let (p, body, _) = split_key(&key).expect("valid");
    match reg.decode(p.kind, body) {
        BodyOutcome::Decoded(d) => assert_eq!(d.summary, "node 42"),
        other => panic!("expected a decode, got {other:?}"),
    }
}

#[test]
fn an_UNREGISTERED_kind_is_not_an_error() {
    // The FC-2 property. An older reader meeting a newer writer's KIND must
    // pass the row through untouched, not report corruption — otherwise every
    // rolling deploy looks like data loss for the duration of the rollout.
    let reg = KindRegistry::new();
    assert_eq!(
        reg.decode(Kind::from_byte(0xC7), &[1, 2, 3]),
        BodyOutcome::Unregistered
    );
}

#[test]
fn UNREGISTERED_and_CORRUPT_are_different_answers() {
    // Two facts with two different responses: one is a version skew and the
    // row is fine, the other is damage. A single "could not decode" would make
    // an operator chase corruption that does not exist, or ignore corruption
    // that does.
    let mut reg = KindRegistry::new();
    reg.register(Kind::NODE, node_body).expect("registers");

    assert_eq!(reg.decode(Kind::EDGE, &[0; 8]), BodyOutcome::Unregistered);
    assert_eq!(reg.decode(Kind::NODE, &[0; 3]), BodyOutcome::Corrupt);
}

#[test]
fn a_decoder_that_leaves_TRAILING_bytes_is_reported() {
    // A decoder consuming less than the body holds is wrong about the format,
    // not lenient. Accepting it silently is how a format change ships with half
    // the readers quietly ignoring the new half of every record.
    let mut reg = KindRegistry::new();
    reg.register(Kind::EDGE, greedy_but_short)
        .expect("registers");
    assert_eq!(
        reg.decode(Kind::EDGE, &[1, 2, 3, 4]),
        BodyOutcome::Trailing {
            consumed: 3,
            len: 4
        }
    );
}

#[test]
fn registering_twice_is_REFUSED() {
    // A collision between two features where the loser decodes as the winner —
    // producing plausible rows attributed to the wrong thing, which no
    // assertion downstream would catch.
    let mut reg = KindRegistry::new();
    reg.register(Kind::NODE, node_body).expect("first");
    assert_eq!(
        reg.register(Kind::NODE, node_body),
        Err(KindError::AlreadyRegistered(0x01))
    );
}

#[test]
fn an_INVALID_kind_cannot_be_registered() {
    let mut reg = KindRegistry::new();
    assert_eq!(
        reg.register(Kind::from_byte(0x00), node_body),
        Err(KindError::Invalid(0x00))
    );
    assert!(reg.is_empty());
}

#[test]
fn the_codec_never_learns_what_a_body_IS() {
    // The point of FC-2, asserted structurally: `split_key` returns the body as
    // BYTES. If it ever grew a match on KIND, adding a KIND would become an
    // edit to the frozen artifact — the one file that must stop changing.
    let key = encode_key(
        &KeyPrefix {
            realm: Realm(1),
            namespace: Namespace(1),
            // A KIND no build has ever heard of.
            kind: Kind::from_byte(0xD3),
            partition: Partition(1),
        },
        &[9, 9, 9],
        1,
    );
    let (p, body, ts) = split_key(&key).expect("an unknown KIND still splits");
    assert_eq!(p.kind.byte(), 0xD3);
    assert_eq!(body, &[9, 9, 9]);
    assert_eq!(ts, 1);
}
