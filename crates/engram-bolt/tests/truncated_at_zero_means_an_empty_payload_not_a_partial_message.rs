//! `decode: Truncated { at: 0 }` is what the Neo4j LSQB arm reported for q9,
//! and the whole diagnosis turned on reading that offset literally: the
//! decoder's only `Truncated` site is `take`, which reports
//! `at: self.buf.len()`, so an offset of ZERO can only mean the decoder was
//! handed an EMPTY buffer — never a message that ran out part-way through.
//! That is what named the cause (a peer's keep-alive NOOP read as an empty
//! message) rather than the shapes it looked like: a chunk-boundary bug, a
//! buffer ceiling, or a PackStream type Neo4j emits and this project's own
//! server does not.
//!
//! Nothing tested that equivalence, so a later change to `take`'s offset —
//! `self.at` reads like the more obvious choice — would silently retire the
//! only diagnostic that made the field report legible, and the next person
//! reading `at: 0` would be told "the message was cut short" by an error that
//! means the opposite.
//!
//! The second test is a CANARY that needs no source reverted: it runs the
//! pre-fix chunk reader — the same loop minus its `payload.is_empty()` branch
//! — over the bytes a keep-alive puts on the wire, and pins that it produces
//! that exact error. The fixed shape beside it reads the message. The real
//! `Client` over a real socket is covered by
//! `a_keep_alive_noop_chunk_is_not_a_message.rs`; this file pins the two
//! halves of the inference that test's header asserts in prose.

use std::io::{Cursor, Read};

use engram_bolt::packstream::{Decoder, Pack, PackError, encode_struct};
use engram_cypher::Value;

const MSG_SUCCESS: u8 = 0x70;

/// The bytes of one chunked Bolt message: a 2-byte length, the payload, and
/// the zero-length terminator.
fn framed(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
    out.extend_from_slice(&[0, 0]);
    out
}

/// A SUCCESS carrying an empty metadata map, as PackStream bytes.
fn success_payload() -> Vec<u8> {
    let mut payload = Vec::new();
    encode_struct(
        MSG_SUCCESS,
        &[Pack::Value(Value::Map(Default::default()))],
        &mut payload,
    )
    .expect("a SUCCESS encodes");
    payload
}

/// The chunk reader as it stood BEFORE the fix: a zero-length chunk always
/// ends the message, whether or not one had begun.
fn read_msg_reverted<R: Read>(r: &mut R) -> Result<Pack, PackError> {
    let mut payload = Vec::new();
    loop {
        let mut len = [0u8; 2];
        r.read_exact(&mut len).expect("chunk header");
        let n = u16::from_be_bytes(len) as usize;
        if n == 0 {
            break;
        }
        let start = payload.len();
        payload.resize(start + n, 0);
        r.read_exact(&mut payload[start..]).expect("chunk body");
    }
    Decoder::new(&payload).decode()
}

/// The chunk reader as it stands now: a zero-length chunk with nothing
/// assembled is a keep-alive NOOP and carries no message.
fn read_msg_current<R: Read>(r: &mut R) -> Result<Pack, PackError> {
    let mut payload = Vec::new();
    loop {
        let mut len = [0u8; 2];
        r.read_exact(&mut len).expect("chunk header");
        let n = u16::from_be_bytes(len) as usize;
        if n == 0 {
            if payload.is_empty() {
                continue;
            }
            break;
        }
        let start = payload.len();
        payload.resize(start + n, 0);
        r.read_exact(&mut payload[start..]).expect("chunk body");
    }
    Decoder::new(&payload).decode()
}

/// An empty buffer is the ONLY input that reports offset zero; every input
/// that is non-empty but incomplete reports where it actually ran out.
#[test]
fn only_an_empty_payload_decodes_to_truncated_at_zero() {
    assert_eq!(
        Decoder::new(&[]).decode(),
        Err(PackError::Truncated { at: 0 }),
        "an empty payload must report offset zero — this is the identity the \
         q9 diagnosis read the field report through"
    );
    // The literal that appeared in the log, pinned as a literal.
    assert_eq!(
        format!("{:?}", Decoder::new(&[]).decode().unwrap_err()),
        "Truncated { at: 0 }"
    );

    // Genuinely partial messages, one per shape that could plausibly be cut by
    // a chunk boundary. None of them may report zero, or `at: 0` stops naming
    // the empty case.
    for (label, bytes) in [
        ("a struct marker with no tag", &[0xB1u8][..]),
        ("a struct with its field missing", &[0xB1, MSG_SUCCESS][..]),
        ("an INT_64 with one of eight bytes", &[0xCB, 0x00][..]),
        ("a STRING_16 with half a length", &[0xD1, 0xFF][..]),
        ("a LIST_8 short of its element", &[0xD4, 0x01][..]),
        ("a map promising a pair it lacks", &[0xA1][..]),
    ] {
        match Decoder::new(bytes).decode() {
            Err(PackError::Truncated { at }) => assert!(
                at > 0,
                "{label} is a PARTIAL message and must not report offset zero"
            ),
            other => panic!("{label}: expected Truncated, got {other:?}"),
        }
    }
}

/// The canary. A keep-alive NOOP in front of a reply is exactly the shape the
/// pre-fix reader turned into `Truncated { at: 0 }`.
#[test]
fn the_pre_fix_reader_turns_a_keep_alive_into_that_exact_error() {
    let mut wire = vec![0u8, 0]; // one NOOP
    wire.extend_from_slice(&framed(&success_payload()));

    assert_eq!(
        read_msg_reverted(&mut Cursor::new(wire.clone())),
        Err(PackError::Truncated { at: 0 }),
        "the reverted reader must reproduce the failure the Neo4j arm reported"
    );
    match read_msg_current(&mut Cursor::new(wire)) {
        Ok(Pack::Struct { tag, .. }) => assert_eq!(tag, MSG_SUCCESS),
        other => panic!("the fixed reader must read the reply, got {other:?}"),
    }
}

/// A long query draws many keep-alives, not one: Neo4j emits a NOOP every
/// `server.bolt.connection_keep_alive` (one minute by default) for as long as
/// the request runs, so a q9 that takes a quarter of an hour at a larger scale
/// puts a run of them in front of the reply. Skipping the first is not enough.
#[test]
fn a_run_of_keep_alives_in_front_of_a_reply_is_skipped_entirely() {
    for count in [1usize, 2, 15] {
        let mut wire = vec![0u8; count * 2];
        wire.extend_from_slice(&framed(&success_payload()));
        match read_msg_current(&mut Cursor::new(wire)) {
            Ok(Pack::Struct { tag, .. }) => assert_eq!(tag, MSG_SUCCESS, "{count} keep-alives"),
            other => panic!("{count} keep-alives: {other:?}"),
        }
    }
}

/// A zero-length chunk AFTER a payload has begun still terminates the message
/// — the fix must not have turned the terminator into a NOOP as well.
#[test]
fn a_zero_length_chunk_after_a_payload_still_ends_the_message() {
    let payload = success_payload();
    let mut wire = Vec::new();
    // Split the payload across two chunks, so the terminator is reached with
    // bytes already assembled.
    wire.extend_from_slice(&1u16.to_be_bytes());
    wire.push(payload[0]);
    wire.extend_from_slice(&((payload.len() - 1) as u16).to_be_bytes());
    wire.extend_from_slice(&payload[1..]);
    wire.extend_from_slice(&[0, 0]);
    // A second message follows, so a reader that swallowed the terminator
    // would run past it rather than stopping.
    wire.extend_from_slice(&framed(&payload));

    let mut cur = Cursor::new(wire);
    match read_msg_current(&mut cur) {
        Ok(Pack::Struct { tag, .. }) => assert_eq!(tag, MSG_SUCCESS),
        other => panic!("expected the first message to end at its terminator, got {other:?}"),
    }
    match read_msg_current(&mut cur) {
        Ok(Pack::Struct { tag, .. }) => assert_eq!(tag, MSG_SUCCESS),
        other => panic!("expected the second message, got {other:?}"),
    }
}
