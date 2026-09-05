#![allow(non_snake_case)]
//! The chain's properties — what it detects, what it cannot, and what
//! verification does NOT require.

use engram_key::{Kind, Namespace, Partition, Realm};
use engram_log::{ChainVerify, CommitLog, Entry, HEADER_LEN, Op, RoutingHeader};

fn header(realm: u32, ts: u64) -> RoutingHeader {
    RoutingHeader {
        realm: Realm(realm),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(1),
        op: Op::Put,
        commit_ts: ts,
    }
}

fn sample_log(n: u64) -> CommitLog {
    let mut log = CommitLog::new();
    for i in 0..n {
        log.append(header(1, i + 1), vec![i as u8; 8]);
    }
    log
}

// ─── The happy path, and its golden pin ─────────────────────────────────────

#[test]
fn a_clean_chain_verifies_and_reports_its_head() {
    let log = sample_log(5);
    match log.verify() {
        ChainVerify::Intact { len, head } => {
            assert_eq!(len, 5);
            assert_eq!(
                head,
                log.head(),
                "verify's head must equal the log's published head"
            );
        }
        other => panic!("expected Intact, got {other:?}"),
    }
}

#[test]
fn GOLDEN_the_hash_rule_is_frozen() {
    // The chain rule is on-disk format: every replica recomputes these exact
    // bytes. The constants below were computed ONCE at freeze time and are
    // hard-coded — the first version of this test compared the hash with
    // itself, which pins nothing and reads exactly like a pin. A change to the
    // hashing — field order, the length prefix, the genesis domain — must show
    // up as a failing literal here, or it is an accident shipping.
    //
    // CHANGED ONCE, deliberately, 2026-08-29, before any released format: the
    // rule became `BLAKE3(prev ‖ seq ‖ header ‖ BLAKE3(len ‖ payload))` so a
    // writer hashes its payload outside the log latch (ADR-001, decision 5).
    // The literal below is that rule's; the previous one was
    // 2707cc75fe613bc046aab6a869bbbc511df443349ab7b43d8770f564023164b2.
    let log = sample_log(1);
    let hex: String = log.entries()[0]
        .hash
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        hex,
        "afae9f57177d4911373fdea14e9913d9d4c312723f788316578846aac87d4ab1"
    );

    let genesis: String = CommitLog::new()
        .head()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        genesis,
        "114bbb63f1fd971c873592bd6483b998fea7d0db04e8577029c5c80b844c1b8e"
    );
    assert_ne!(
        CommitLog::new().head(),
        [0u8; 32],
        "a zeroed head must never be a valid genesis"
    );
}

#[test]
fn the_header_round_trips_and_is_fixed_width() {
    let h = header(7, 99);
    let enc = h.encode();
    assert_eq!(enc.len(), HEADER_LEN);
    assert_eq!(RoutingHeader::decode(&enc), Some(h));
    // An unknown op byte is refused — an op is the instruction a replayer
    // executes, not skippable content.
    let mut bad = enc;
    bad[13] = 9;
    assert_eq!(RoutingHeader::decode(&bad), None);
}

// ─── Tamper detection ───────────────────────────────────────────────────────

#[test]
fn a_flipped_payload_byte_breaks_the_chain_AT_that_entry() {
    let log = sample_log(5);
    let mut entries: Vec<Entry> = log.entries().to_vec();
    entries[2].payload[0] ^= 0x01;
    assert_eq!(
        CommitLog::verify_entries(&entries),
        ChainVerify::Broken { seq: 2 }
    );
}

#[test]
fn an_edited_routing_header_breaks_the_chain() {
    // The header is plaintext at every replication site — the cheapest thing
    // for an attacker to edit, and re-routing an entry to another tenant is
    // exactly the edit worth making. It is under the hash like everything else.
    let log = sample_log(5);
    let mut entries: Vec<Entry> = log.entries().to_vec();
    entries[3].header.realm = Realm(999);
    assert_eq!(
        CommitLog::verify_entries(&entries),
        ChainVerify::Broken { seq: 3 }
    );
}

#[test]
fn swapped_entries_are_detected() {
    let log = sample_log(5);
    let mut entries: Vec<Entry> = log.entries().to_vec();
    entries.swap(1, 2);
    // The swap surfaces as a sequence gap first — 0,2,1,… — which is the
    // right verdict: every hash still matches its own contents.
    assert_eq!(
        CommitLog::verify_entries(&entries),
        ChainVerify::SequenceGap {
            expected: 1,
            found: 2
        },
    );
}

#[test]
fn a_REHASHED_tamper_still_breaks_at_the_next_entry() {
    // The attacker model that matters: edit entry 2 AND recompute its hash.
    // Entry 2 now verifies in isolation — but entry 3's stored hash chained
    // from the ORIGINAL entry 2, so the break surfaces at 3. Detection is why
    // the chain exists; localisation is one entry off, and the test pins that
    // honestly rather than claiming the tamper point itself is named.
    let log = sample_log(5);
    let mut entries: Vec<Entry> = log.entries().to_vec();
    entries[2].payload = b"forged".to_vec();
    // Recompute 2's hash correctly for its forged contents.
    let forged = {
        let mut l = CommitLog::new();
        for e in &entries[..2] {
            l.append(e.header, e.payload.clone());
        }
        l.append(entries[2].header, entries[2].payload.clone());
        l.entries()[2].hash
    };
    entries[2].hash = forged;
    assert_eq!(
        CommitLog::verify_entries(&entries),
        ChainVerify::Broken { seq: 3 }
    );
}

#[test]
fn a_dropped_entry_is_a_sequence_gap() {
    let log = sample_log(5);
    let mut entries: Vec<Entry> = log.entries().to_vec();
    entries.remove(2);
    assert_eq!(
        CommitLog::verify_entries(&entries),
        ChainVerify::SequenceGap {
            expected: 2,
            found: 3
        },
    );
}

#[test]
fn TRUNCATION_is_invisible_to_the_chain_alone_and_caught_by_the_head() {
    // A prefix of a valid chain IS a valid chain — the one attack hashing
    // cannot see. The defence is comparing against an externally attested
    // head, which is the root beacon's whole job. This test pins BOTH halves:
    // the blindness (so nobody mistakes the chain for truncation-proof) and
    // the detection (so the beacon comparison is known sufficient).
    let log = sample_log(5);
    let published_head = log.head();

    let truncated: Vec<Entry> = log.entries()[..3].to_vec();
    match CommitLog::verify_entries(&truncated) {
        ChainVerify::Intact { len, head } => {
            assert_eq!(
                len, 3,
                "the truncated chain verifies — that is the blindness"
            );
            assert_ne!(
                head, published_head,
                "…and the head comparison is what catches it"
            );
        }
        other => panic!("a truncated prefix must verify Intact, got {other:?}"),
    }
}

// ─── The header/payload split ───────────────────────────────────────────────

#[test]
fn verification_requires_no_key() {
    // The payloads here stand in for ciphertext: bytes with no structure the
    // log could interpret. Verification recomputes every hash over them AS
    // STORED — so a replication site can attest the whole chain while being
    // structurally unable to read a byte of tenant data. If this ever needed
    // a decrypt, integrity and confidentiality would trade against each other
    // at every replica.
    let mut log = CommitLog::new();
    let opaque = vec![0x93, 0x51, 0x0A, 0xFF, 0x00, 0x17]; // no valid tag, no record shape
    log.append(header(1, 1), opaque);
    assert!(matches!(log.verify(), ChainVerify::Intact { .. }));
}

#[test]
fn shredding_a_tenant_leaves_the_chain_intact() {
    // The shred model: destroying a DEK makes a tenant's payloads permanently
    // unreadable, but the LOG copies are untouched bytes — so the chain still
    // verifies everywhere, and no replica has to rewrite history to honour a
    // shred. (What changes is readability, which was key-gated all along.)
    let log = sample_log(4);
    assert!(matches!(log.verify(), ChainVerify::Intact { .. }));
    // Nothing to do: the bytes never depended on the key. The assertion is
    // that this fact is true BY CONSTRUCTION — the log has no decrypt path to
    // break.
}

// ─── CDC ────────────────────────────────────────────────────────────────────

#[test]
fn tail_returns_entries_from_a_cursor() {
    let log = sample_log(5);
    let tail = log.tail(3);
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[0].seq, 3);
    // Past the end: empty, not an error — a caught-up consumer is the normal
    // state, not an edge case.
    assert!(log.tail(99).is_empty());
    // From zero: everything.
    assert_eq!(log.tail(0).len(), 5);
}

#[test]
fn appends_continue_the_chain_across_a_verify() {
    let mut log = sample_log(3);
    let head_before = log.head();
    assert!(matches!(log.verify(), ChainVerify::Intact { .. }));
    log.append(header(1, 10), vec![9]);
    assert_ne!(log.head(), head_before);
    assert!(matches!(log.verify(), ChainVerify::Intact { .. }));
}

#[test]
fn adjacent_field_lengths_cannot_be_confused() {
    // The concatenation ambiguity: payload "AB" must not hash equal to payload
    // "A" with a header byte shifted. The length prefix inside entry_hash is
    // what separates them; this pins it from the outside.
    let mut a = CommitLog::new();
    a.append(header(1, 1), b"AB".to_vec());
    let mut b = CommitLog::new();
    b.append(header(1, 1), b"A".to_vec());
    assert_ne!(a.head(), b.head());
}
