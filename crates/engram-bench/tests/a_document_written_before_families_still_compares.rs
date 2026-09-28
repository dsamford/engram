//! The SF10 campaign in flight must still be comparable with everything after it.
//!
//! On 2026-09-11 a benchmark campaign was running on a remote node against a
//! binary built BEFORE catalogue families existed. Its result documents carry
//! `catalogue_digest` and no `catalogue_families` key. Every run taken after
//! this change carries both.
//!
//! Those two shapes have to compare, or the campaign is orphaned — and the way
//! it would be orphaned is the reason this test is a DOCUMENT test and not a
//! struct test. Nothing fails at measurement time. The run succeeds, the
//! document is written, and the comparison is simply never produced, months
//! later, in a session that has no idea a schema changed.
//!
//! The unit tests in `report.rs` already exercise the fallback in its refusing
//! direction (both sides cleared, digests disagree -> refuse). This exercises
//! the ASYMMETRIC direction that the campaign actually is: one old document,
//! one new one, the same frozen statements.json, must be OK. That direction is
//! the one a reader would assume rather than check.

use engram_bench::report::{CompareRefusal, compare, parse};

/// The whole-file digest of the frozen catalogue, as the in-flight run stamped
/// it. Written as a literal rather than read from `catalogue::digest()` so this
/// test states the campaign's actual value; `the_frozen_lsqb_digest_is_pinned`
/// is where the two are tied together.
const IN_FLIGHT: &str = "6d90c1513a6acd0e";

/// A result document as the PRE-family binary wrote it: no `catalogue_families`.
fn old_document(engine: &str, catalogue_digest: &str) -> String {
    format!(
        r#"{{
          "engine": "{engine}",
          "workload": "lsqb",
          "catalogue_digest": "{catalogue_digest}",
          "writes_mode": "n/a",
          "fairness": {{"thread_cap": 6, "cache_budget_mb": 8192, "clients": 1, "seconds": 120}},
          "fairness_check": {{"status": "declared"}},
          "rig": {{"name": "pod-6c", "cpu_quota_cores": 6}},
          "rig_check": {{"status": "verified"}}
        }}"#
    )
}

/// The same document as a POST-family binary writes it: the map is present and
/// stamps every family the binary held, including the three that were only
/// carried.
fn new_document(engine: &str, catalogue_digest: &str, lsqb_family_digest: &str) -> String {
    format!(
        r#"{{
          "engine": "{engine}",
          "workload": "lsqb",
          "catalogue_digest": "{catalogue_digest}",
          "catalogue_families": {{
            "lsqb-stress": "{lsqb_family_digest}",
            "snb-interactive": "0965f35a68edc279",
            "snb-bi": "692b9e37d3a5d39c",
            "finbench": "d57a9654ec5aab55"
          }},
          "writes_mode": "n/a",
          "fairness": {{"thread_cap": 6, "cache_budget_mb": 8192, "clients": 1, "seconds": 120}},
          "fairness_check": {{"status": "declared"}},
          "rig": {{"name": "pod-6c", "cpu_quota_cores": 6}},
          "rig_check": {{"status": "verified"}}
        }}"#
    )
}

#[test]
fn the_in_flight_campaign_compares_against_a_run_taken_after_families_landed() {
    let old = parse(&old_document("engram", IN_FLIGHT)).expect("pre-family document parses");
    assert!(
        old.catalogue_family_digests.is_empty(),
        "a document with no `catalogue_families` key must read as an EMPTY map, not \
         as a map with something invented in it — the fallback depends on being able \
         to tell 'this run did not say' from 'this run said something'"
    );

    let new = parse(&new_document("neo4j", IN_FLIGHT, IN_FLIGHT)).expect("post-family parses");

    assert_eq!(
        compare(&[&old, &new]),
        Ok(()),
        "the SF10 campaign running on 2026-09-11 wrote documents in the OLD shape. \
         If they stop comparing against runs taken afterwards, the campaign is \
         orphaned and the three new catalogue families are what orphaned it — which \
         is the precise outcome per-family digests were introduced to prevent."
    );
}

#[test]
fn adding_the_three_new_families_moves_no_digest_the_campaign_was_measured_under() {
    // A binary that carries MORE families than the one that produced a
    // document must not thereby refuse it. This is the same shape as the test
    // above, stated as the rule rather than as the incident, because the next
    // family added is the one nobody will re-check.
    let old = parse(&old_document("engram", IN_FLIGHT)).expect("parses");
    let many = new_document("ladybug", IN_FLIGHT, IN_FLIGHT).replace(
        r#""finbench": "d57a9654ec5aab55""#,
        r#""finbench": "d57a9654ec5aab55", "graphalytics": "0000badc0ffee000""#,
    );
    let many = parse(&many).expect("parses");
    assert_eq!(compare(&[&old, &many]), Ok(()));
}

#[test]
fn a_run_against_different_frozen_bytes_is_still_refused() {
    // The guard must not have been softened into nothing by the fallback. An
    // old document measured against a DIFFERENT statements.json is exactly the
    // case the whole-file digest exists to catch, and it still catches it —
    // per-family digests aimed the refusal, they did not remove it.
    let old = parse(&old_document("engram", IN_FLIGHT)).expect("parses");
    let recut = parse(&new_document(
        "neo4j",
        "0000000000000001",
        "0000000000000001",
    ))
    .expect("parses");
    assert!(
        matches!(
            compare(&[&old, &recut]),
            Err(CompareRefusal::CatalogueDigest { .. })
        ),
        "an old document and a run against re-cut catalogue bytes must refuse on the \
         whole file: the old side cannot name a family, so the question 'was it the \
         same statement text' is only answerable about the file"
    );
}

#[test]
fn a_run_recorded_before_the_notes_were_reworded_compares_with_one_after() {
    // On 2026-09-28 the frozen file's prose notes were reworded for publication
    // and its digest moved; no statement did. Every LSQB and stress document
    // recorded before then carries the old digest, and must still compare with
    // a run taken afterwards, in both the whole-file and the per-family path.
    let after = format!("{:016x}", engram_bench::catalogue::digest());
    assert_ne!(after, IN_FLIGHT, "the file was reworded, so the digests differ");

    let old = parse(&old_document("engram", IN_FLIGHT)).expect("parses");
    let new = parse(&new_document("neo4j", &after, &after)).expect("parses");
    assert_eq!(compare(&[&old, &new]), Ok(()), "whole-file path");

    let old_family = parse(&new_document("engram", IN_FLIGHT, IN_FLIGHT)).expect("parses");
    assert_eq!(compare(&[&old_family, &new]), Ok(()), "per-family path");
    assert_eq!(compare(&[&new, &old_family]), Ok(()), "either order");
}
