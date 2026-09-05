#![allow(non_snake_case)]
//! A column read is budgeted on the rows it VISITS, not only on the entries it
//! hands over — and on a paged store it stops fetching blocks at the budget.
//!
//! The production finding (2026-09-04): a paged segment has no column blocks,
//! so a "column" read is `resolve_rows_only` walking `[first member, last
//! member)` row by row. The mirror's labels interleave in id space, so a
//! 15-node label's span was the whole node partition: ~5M rows fetched,
//! verified, decoded and cloned per PROPERTY read (6 s each, gigabytes of
//! transient) to find 15 that carried it — and the budget, which counted only
//! the rows that carried the property, never fired. These tests pin the
//! contract that closes it: a span wider than the budget DECLINES (`None`),
//! cheaply, on both backings; a span within it is served exactly as before.

use engram_key::value::Tag;
use engram_key::{KeyPrefix, Kind, Namespace, Partition, Realm};
use engram_store::record::{PropertyId, Record};
use engram_store::{Store, StoredValue};

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

fn prefix() -> KeyPrefix {
    KeyPrefix {
        realm: Realm(1),
        namespace: Namespace(1),
        kind: Kind::NODE,
        partition: Partition(1),
    }
}

fn body(i: u32) -> Vec<u8> {
    i.to_be_bytes().to_vec()
}

/// The MEMBER shape: carries property 1 (the column under test).
fn member_record(i: u32) -> Vec<u8> {
    let mut r = Record::new();
    r.set(PropertyId(1), int64(i64::from(i)));
    r.set(PropertyId(2), string(&format!("member-{i}")));
    r.encode()
}

/// The FILLER shape: another population interleaved in the same id space,
/// NOT carrying property 1. Wide enough that thousands of them span many
/// blocks once paged.
fn filler_record(i: u32) -> Vec<u8> {
    let mut r = Record::new();
    r.set(PropertyId(7), string(&format!("filler-{i}-{}", "x".repeat(48))));
    r.encode()
}

const FILLERS: u32 = 40_000;

/// Two members bracketing FILLERS fillers: the members' span is the whole
/// partition, and only 2 of its rows carry the property.
fn interleaved() -> Store {
    let s = Store::new();
    s.put(&prefix(), &body(0), StoredValue::Plain(member_record(0)))
        .expect("put");
    for i in 1..=FILLERS {
        s.put(&prefix(), &body(i), StoredValue::Plain(filler_record(i)))
            .expect("put");
    }
    s.put(
        &prefix(),
        &body(FILLERS + 1),
        StoredValue::Plain(member_record(FILLERS + 1)),
    )
    .expect("put");
    s.seal();
    s
}

fn scan(s: &Store, budget: usize) -> (Option<u64>, Vec<Vec<u8>>) {
    let mut got = Vec::new();
    let visited = s.scan_column_range_with(
        &prefix(),
        &[],
        None,
        1,
        u64::MAX,
        budget,
        &mut |b, _| got.push(b.to_vec()),
    );
    (visited, got)
}

#[test]
fn a_span_wider_than_the_budget_declines_before_handing_anything_over() {
    let s = interleaved();
    // The budget a 2-member label would carry (4×members, the graph's floor)
    // is far below the 40,002 rows the span holds: decline — and nothing is
    // handed over, because the decision is made on the walk, not after it.
    let (visited, got) = scan(&s, 8);
    assert_eq!(visited, None, "a span of 40k rows must decline a budget of 8");
    assert!(got.is_empty(), "declined before any entry was handed over");
    assert_eq!(
        s.scan_column_presence_at(&prefix(), &[], None, 1, u64::MAX, 8),
        None,
        "the presence read declines on the same walk"
    );
}

#[test]
fn a_span_within_the_budget_is_served_exactly_as_before() {
    let s = interleaved();
    let (visited, got) = scan(&s, usize::MAX);
    assert_eq!(visited, Some(2), "two rows carry the property");
    assert_eq!(got, vec![body(0), body(FILLERS + 1)]);
    let present = s
        .scan_column_presence_at(&prefix(), &[], None, 1, u64::MAX, usize::MAX)
        .expect("served");
    assert_eq!(present, vec![body(0), body(FILLERS + 1)]);
    // A budget that covers the span serves it; one row short declines.
    let rows = (FILLERS + 2) as usize;
    assert_eq!(scan(&s, rows).0, Some(2), "budget == rows visited serves");
    assert_eq!(scan(&s, rows - 1).0, None, "one row short declines");
}

/// The walk is bounded in BYTES held as well as rows visited: a wide label
/// whose row budget is generous still cannot fill the override map with
/// gigabytes of records before declining. The production NewsArticle
/// enrichment count held 2–3.8 GB per execution this way (reported by the
/// Bolt layer's rss-growth line) and was the transient that OOM-killed the
/// 12Gi pod. Same decline, same `None`, reached on bytes.
#[test]
fn a_span_that_would_hold_more_bytes_than_the_budget_declines() {
    let s = interleaved();
    // A generous but FINITE row budget (the byte budget rides only on
    // budgeted reads — see the unbounded case below), and 64 KB of bytes: the
    // 40k fillers (~70 B each) blow through it long before the walk reaches
    // the second member.
    s.set_column_scan_byte_budget(64 * 1024);
    let ((visited, got), trace) = engram_observe::with_trace(|| scan(&s, 1_000_000));
    assert_eq!(visited, None, "a span holding megabytes must decline a 64 KB byte budget");
    assert!(got.is_empty(), "declined before any entry was handed over");
    assert!(
        trace
            .counters()
            .get("store.row-form span walk stopped on its byte budget")
            .copied()
            .unwrap_or(0)
            > 0,
        "and the decline must be the BYTE budget"
    );
    // Restoring the default serves it exactly as before.
    s.set_column_scan_byte_budget(256 << 20);
    assert_eq!(scan(&s, 1_000_000).0, Some(2));
}

/// An UNBOUNDED read is not subject to the byte budget: its callers have no
/// decline path (`scan_column_at` `expect`s completion). v88 applied the
/// budget to it and eight production statements panicked a worker and lost
/// their connection. The row-budgeted read above declines; this one serves.
#[test]
fn an_unbounded_read_ignores_the_byte_budget_and_completes() {
    let s = interleaved();
    s.set_column_scan_byte_budget(1024);
    // `scan_column_at` is the unbounded entry point — it must return both
    // carriers whatever the byte budget says, never abort.
    let got = s.scan_column_at(&prefix(), &[], 1, u64::MAX);
    let bodies: Vec<Vec<u8>> = got.into_iter().map(|(b, _)| b).collect();
    assert_eq!(bodies, vec![body(0), body(FILLERS + 1)]);
    // And the budgeted entry point with an explicit unbounded row budget is
    // the same read: it completes too.
    assert_eq!(scan(&s, usize::MAX).0, Some(2), "usize::MAX rows means unbounded, bytes included");
    // A budgeted read with the same 1 KB still declines on bytes.
    assert_eq!(scan(&s, usize::MAX - 1).0, None, "a finite row budget carries the byte budget");
}

#[test]
fn on_a_paged_store_the_decline_stops_fetching_blocks() {
    let s = interleaved();
    let dir = std::env::temp_dir().join(format!(
        "engram_column_visit_budget_{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    // A cache too small to retain the walk: every block a walk crosses is a
    // pread, so preads count blocks fetched.
    let _cache = s.into_paged(&dir, 16 * 1024).expect("into_paged");

    let preads = |budget: usize| -> (Option<u64>, u64) {
        let ((visited, _), trace) = engram_observe::with_trace(|| scan(&s, budget));
        (
            visited,
            trace.counters().get("paged.pread").copied().unwrap_or(0),
        )
    };
    let (declined, few) = preads(8);
    assert_eq!(declined, None, "declines on the paged backing too");
    let (served, many) = preads(usize::MAX);
    assert_eq!(served, Some(2), "byte-identical service when in budget");
    assert!(
        many >= 8,
        "fixture: the unbounded walk must cross many blocks (crossed {many})"
    );
    assert!(
        few * 4 < many,
        "the declined walk must stop fetching at its budget: {few} preads against {many} for the whole span"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
