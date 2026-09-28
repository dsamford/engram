//! The regression baselines checked in under `measurements/baselines/` must stay
//! usable as the report format evolves: each parses, is comparable with itself,
//! binds its own parameters, and reports no regression against itself.
//!
//! A baseline that stopped parsing would not fail the scheduled gate loudly —
//! `harness report` would exit 1 on the file and a lane that reads only exit 4
//! as "regressed" would carry on green. This test is where that surfaces.

use engram_bench::report;

fn baseline(name: &str) -> report::Comparable {
    let path = format!(
        "{}/../../measurements/baselines/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    report::parse(&text).unwrap_or_else(|e| panic!("{path} no longer parses: {e}"))
}

#[test]
fn each_checked_in_baseline_parses_and_gates_clean_against_itself() {
    for (name, workload, quotable_at_least) in [
        ("snb-bi-sf3-cypher.json", "snb-bi", 19),
        ("snb-bi-sf3-cypher_engram.json", "snb-bi", 8),
        // IC7 and IC10 are refused on a typed corpus (their reference texts do
        // epoch arithmetic on a DATETIME), as they are on Neo4j.
        ("snb-interactive-sf3-cypher.json", "snb-interactive", 19),
    ] {
        let b = baseline(name);
        assert_eq!(b.workload, workload, "{name}");
        let quotable = b.rows.iter().filter(|r| r.quotable).count();
        assert!(
            quotable >= quotable_at_least,
            "{name}: {quotable} quotable rows, recorded with {quotable_at_least}"
        );
        assert!(
            b.rows.iter().filter(|r| r.quotable).all(|r| r.probe.is_some()),
            "{name}: a quotable row carries no binding, so a parameter change could not be seen"
        );
        report::compare(&[&b, &b]).unwrap_or_else(|e| panic!("{name} refuses itself: {e}"));
        assert!(report::parameter_mismatches(&b, &b).is_empty(), "{name}");
        assert!(report::regressions(&b, &b, 0.0).is_empty(), "{name}");
        // and as the README gates it: two repetitions, a 5 ms floor
        assert!(
            report::regressions_in_every(&b, &[&b, &b], 0.0, 5.0).is_empty(),
            "{name}"
        );
    }
}

#[test]
fn bi17_is_not_in_the_baselines() {
    // bi17 is run apart from the family battery (it is the heaviest query in
    // the set), and the baselines' query lists leave it out by name.
    for name in ["snb-bi-sf3-cypher.json", "snb-bi-sf3-cypher_engram.json"] {
        let b = baseline(name);
        assert!(
            b.rows.iter().all(|r| !r.key.starts_with("bi17")),
            "{name} carries bi17"
        );
        assert!(
            b.rows.iter().all(|r| r.why.as_deref() != Some("status abandoned-upstream")),
            "{name}: a row was abandoned after a timeout"
        );
    }
}
