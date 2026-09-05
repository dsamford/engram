//! Hostile-input tests for the wire decoder.
//!
//! Everything here is reachable by an UNAUTHENTICATED client from a single TCP
//! connection, which is what separates these from the conformance tests in
//! `wire.rs`: a bug here is not a wrong answer, it is a dead process.
//!
//! The decoder was already careful about the failure most hand-written codecs
//! get wrong — it never pre-allocates from a length prefix it has not yet
//! consumed (`take()` is bounds-checked first, and `list` caps its capacity
//! hint at 1024). What it had no defence against was DEPTH, because PackStream
//! spends one byte per nesting level.

use engram_bolt::packstream::{Decoder, MAX_DEPTH, PackError};

/// `0x91` is "a list of one". Repeated, each byte opens another container.
const TINY_LIST: u8 = 0x91;
/// `0xB1` is "a structure with one field"; the tag byte follows.
const TINY_STRUCT: u8 = 0xB1;

/// Nesting at the limit still decodes — the guard must not break real messages.
///
/// A driver's deepest realistic value is a path of nodes carrying property
/// maps, around five levels. Proving the boundary itself works is what makes
/// the refusal below a limit rather than a wall in an arbitrary place.
#[test]
fn nesting_at_the_limit_is_accepted() {
    // MAX_DEPTH containers, then a scalar. `decode` opens one level per list,
    // so this is exactly at the boundary.
    let mut bytes = vec![TINY_LIST; MAX_DEPTH as usize];
    bytes.push(0x00); // Int(0) — the innermost scalar
    let got = Decoder::new(&bytes).decode();
    assert!(
        got.is_ok(),
        "nesting AT the limit must decode, else the limit is really limit-1: {got:?}"
    );
}

/// One level past the limit is refused BY NAME.
///
/// The refusal must be `TooDeep`, not a generic parse failure: an operator
/// reading a log has to be able to tell a driver sending something unusual from
/// someone probing for a stack overflow.
#[test]
fn nesting_past_the_limit_is_refused_by_name() {
    let mut bytes = vec![TINY_LIST; MAX_DEPTH as usize + 1];
    bytes.push(0x00);
    match Decoder::new(&bytes).decode() {
        Err(PackError::TooDeep { limit, .. }) => assert_eq!(limit, MAX_DEPTH),
        other => panic!("expected TooDeep, got {other:?}"),
    }
}

/// The actual attack: a large message of pure nesting.
///
/// THIS IS THE CANARY FOR THE FIX. Before the depth limit, 64 KiB of `0x91`
/// produced 65,536 stack frames and aborted the process — so this test could
/// not have been written as a `Result` check at all, because there was no
/// `Err` to observe. That it now returns an error rather than killing the test
/// runner IS the assertion.
#[test]
fn a_64kib_nesting_bomb_returns_an_error_instead_of_aborting() {
    let bytes = vec![TINY_LIST; 64 * 1024];
    let got = Decoder::new(&bytes).decode();
    assert!(
        matches!(got, Err(PackError::TooDeep { .. })),
        "a nesting bomb must be refused, not survived by luck: {got:?}"
    );
}

/// Structures nest through a different arm than lists, so they get their own
/// bomb — a limit wired into `list` but not `struct` is a limit with a hole,
/// and both arms are one byte per level.
#[test]
fn a_structure_nesting_bomb_is_refused() {
    // Each level is `0xB1 <tag>`: two bytes, one level.
    let mut bytes = Vec::new();
    for _ in 0..(MAX_DEPTH as usize + 8) {
        bytes.push(TINY_STRUCT);
        bytes.push(0x4E); // an arbitrary tag
    }
    bytes.push(0x00);
    let got = Decoder::new(&bytes).decode();
    assert!(
        matches!(got, Err(PackError::TooDeep { .. })),
        "structure nesting must be bounded too: {got:?}"
    );
}

/// Maps are the third recursive arm. Same reasoning.
#[test]
fn a_map_nesting_bomb_is_refused() {
    // `0xA1` is a one-entry map; the key must be a string, so `0x81 'k'`.
    let mut bytes = Vec::new();
    for _ in 0..(MAX_DEPTH as usize + 8) {
        bytes.push(0xA1);
        bytes.push(0x81);
        bytes.push(b'k');
    }
    bytes.push(0x00);
    let got = Decoder::new(&bytes).decode();
    assert!(
        matches!(got, Err(PackError::TooDeep { .. })),
        "map nesting must be bounded too: {got:?}"
    );
}

/// Depth is per-decode, not per-decoder: decoding many shallow values in
/// sequence must not accumulate toward the limit.
///
/// This is the regression that a naive `self.depth += 1` with an early return
/// on error would introduce — the decrement is skipped on the error path and
/// the decoder is poisoned for every later message on that connection.
#[test]
fn depth_does_not_leak_across_sibling_values() {
    // A list of many one-element lists: siblings, each opening and closing.
    let mut bytes = vec![0xD4, 200]; // list, 200 items
    for _ in 0..200 {
        bytes.push(TINY_LIST);
        bytes.push(0x00);
    }
    let got = Decoder::new(&bytes).decode();
    assert!(
        got.is_ok(),
        "200 SIBLING containers are depth 2, not depth 200: {got:?}"
    );
}

/// A refused deep value must not poison the decoder for subsequent reads.
#[test]
fn depth_is_restored_after_a_refusal() {
    let mut deep = vec![TINY_LIST; MAX_DEPTH as usize + 1];
    deep.push(0x00);
    let mut d = Decoder::new(&deep);
    assert!(matches!(d.decode(), Err(PackError::TooDeep { .. })));

    // A fresh, shallow message on a fresh decoder must be unaffected — the
    // counter is per-decoder, and this proves the unwind path restored it.
    let shallow = [TINY_LIST, 0x00];
    assert!(
        Decoder::new(&shallow).decode().is_ok(),
        "a shallow value must still decode after a deep one was refused"
    );
}

/// Truncation still reports truncation, not depth.
///
/// The guard sits in front of the recursion, so it is the kind of change that
/// can start swallowing other errors. This pins that it does not.
#[test]
fn truncation_is_still_reported_as_truncation() {
    let bytes = [TINY_LIST, TINY_LIST]; // opens two lists, never supplies a scalar
    match Decoder::new(&bytes).decode() {
        Err(PackError::Truncated { .. }) => {}
        other => panic!("expected Truncated, got {other:?}"),
    }
}
