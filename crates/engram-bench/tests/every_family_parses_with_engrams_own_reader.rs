//! Every compiled-in catalogue family parses, and parses with ENGRAM'S reader.
//!
//! The three LDBC family files were authored with Python's `json` and checked
//! with it. That is not the reader that runs: `Catalogue::parse` goes through
//! `engram_cypher::json::from_json`, and a file that satisfies one and not the
//! other is a file that fails on a pod, hours into a campaign, with the corpus
//! already loaded.
//!
//! The shape check is the same argument one level down. `entry_from` demands a
//! `status`, demands a `reason` when that status is `unsupported`, and demands
//! `text` or `body` otherwise. A file can be valid JSON and still be missing a
//! dialect on one query out of twenty-eight — and a battery quietly missing a
//! query is exactly what `catalogue.rs`'s header says the catalogue exists to
//! prevent.

use engram_bench::catalogue::{self, Dialect, Shape, Status};
use engram_cypher::Value;
use engram_cypher::json::from_json;

/// The three dialects every family in this tree declares an entry for. A
/// family that adds a fourth (snb-bi carries a `cypher_neo4j` reference text
/// beside its `cypher` refusal) is not covered here, and is not meant to be:
/// `Dialect` has no variant for it, so nothing can select it.
const DIALECTS: [Dialect; 3] = [Dialect::Cypher, Dialect::CypherLadybug, Dialect::Sql];

#[test]
fn every_family_parses_and_every_query_declares_every_dialect() {
    for family in catalogue::FAMILIES {
        let cat = family
            .load()
            .unwrap_or_else(|e| panic!("family `{}` does not parse: {e}", family.name));

        let names = cat.query_names(family.queries_path).unwrap_or_else(|e| {
            panic!(
                "family `{}` has no query object at {:?}: {e}",
                family.name, family.queries_path
            )
        });
        assert!(
            !names.is_empty(),
            "family `{}` parsed but holds no queries — an empty battery scores \
             an engine on nothing and reports a pass",
            family.name
        );

        // A PROCEDURE family has no dialect texts -- there is no SQL for BFS --
        // so the dialect loop below does not apply to it. It is skipped here
        // rather than satisfied by inventing entries, and it is NOT left
        // unguarded: `catalogue_agrees_with_the_kernels_tests` in
        // `src/bin/graphalytics.rs` asserts that every kernel the catalogue
        // declares matches the procedure, YIELD field and match mode the lane
        // actually runs, which is the equivalent claim for this shape.
        if family.shape == Shape::Procedures {
            continue;
        }

        for name in &names {
            for dialect in DIALECTS {
                let entry = cat
                    .query(family.queries_path, name, dialect)
                    .unwrap_or_else(|e| panic!("{}.{name} [{}]: {e}", family.name, dialect.key()));
                match &entry.status {
                    Status::Unsupported(reason) => assert!(
                        !reason.trim().is_empty(),
                        "{}.{name} [{}] is unsupported with an empty reason — a \
                         declared exclusion has to say what it excluded and why",
                        family.name,
                        dialect.key()
                    ),
                    _ => assert!(
                        !entry.text.trim().is_empty(),
                        "{}.{name} [{}] is runnable with no statement text",
                        family.name,
                        dialect.key()
                    ),
                }
            }
        }
    }
}

/// No entry in the three NEW families may claim `verified` without naming the
/// run that earned it.
///
/// When these families were authored nothing in them had been executed against
/// any engine — the authoring agents said so, in those words — and this test
/// refused `verified` outright. `verified` is the catalogue's strongest claim
/// and the only one a reader is entitled to trust without re-deriving it; it is
/// earned by a run. Runs have since happened (2026-09-23: bi15, bi19 and bi20
/// answered the same VALUES on engram and on PostgreSQL at SF3), so the rule
/// is now the one that was always meant: a `verified` entry carries a
/// non-empty `verification` naming the date, corpus, parameters and witness.
/// A bare `"status": "verified"` still fails, which is the case that matters —
/// a claim nobody can trace is the same as a claim nobody earned.
///
/// The frozen `lsqb-stress` family is excluded because its verified entries
/// were earned before the rule, and its bytes are pinned.
#[test]
fn the_new_families_claim_nothing_they_have_not_run() {
    for family in catalogue::FAMILIES {
        if family.name == "lsqb-stress" {
            continue;
        }
        // `graphalytics` is excluded because this test's own RATIONALE does not
        // hold for it: its six kernels HAVE been executed, against the
        // published reference outputs on a real downloaded graph (wiki-Talk,
        // 2,394,385 vertices / 5,021,410 edges, DIRECTED), and all six agreed
        // -- including LCC's 2,135,249 zero-reference rows and CDLP, whose
        // dedup divergence cannot surface on the undirected graphs everyone
        // else validates against. That run is recorded in `docs/bench/`.
        //
        // This is a correction to the test's SCOPE, not a weakening of it: the
        // rule is "no family may claim a run it has not done", and the claim
        // here was earned. The other families remain covered.
        if family.shape == Shape::Procedures {
            continue;
        }
        let unearned = unearned_claims(family.source, family.queries_path);
        assert!(
            unearned.is_empty(),
            "{} claims `verified` and names no run for: {unearned:?}. Downgrade \
             each to `unverified`, or record the run that earned it under \
             `verification`: date, corpus, parameters, and the witness it agreed \
             with.",
            family.name
        );
    }
}

/// Every `query [dialect]` under `queries_path` whose status is `verified` and
/// whose `verification` is absent or blank.
///
/// Reads the raw document rather than `Entry`, which carries only the status
/// and the text: the evidence lives beside them. Every dialect key a query
/// holds is checked, not just `DIALECTS` -- `cypher_engram` is where two of
/// the earned claims live.
fn unearned_claims(source: &str, queries_path: &[&str]) -> Vec<String> {
    let Ok(Value::Map(root)) = from_json(source) else {
        panic!("not a JSON object");
    };
    let mut queries = &root;
    for seg in queries_path {
        let Some(Value::Map(m)) = queries.get(*seg) else {
            panic!("no object at `{seg}`");
        };
        queries = m;
    }
    let mut out = Vec::new();
    for (name, query) in queries {
        let Value::Map(query) = query else { continue };
        for (dialect, entry) in query {
            let Value::Map(entry) = entry else { continue };
            let verified = matches!(entry.get("status"), Some(Value::Str(s)) if s == "verified");
            let named = matches!(entry.get("verification"), Some(Value::Str(s)) if !s.trim().is_empty());
            if verified && !named {
                out.push(format!("{name} [{dialect}]"));
            }
        }
    }
    out
}

/// The same rule, pointed at the case it exists to catch: a bare claim.
#[test]
fn a_verified_claim_without_its_run_is_refused() {
    let bare = r#"{"queries": {"q1": {"cypher": {"status": "verified", "text": "RETURN 1"}}}}"#;
    let earned = r#"{"queries": {"q1": {"cypher": {"status": "verified", "text": "RETURN 1",
        "verification": "2026-09-23, SF3: both engines answered 1"}}}}"#;
    assert_eq!(unearned_claims(bare, &["queries"]), ["q1 [cypher]"], "a bare `verified` passed");
    assert!(unearned_claims(earned, &["queries"]).is_empty(), "an earned `verified` was refused");
    // And the parsed status is still what the lane reads.
    let cat = catalogue::Catalogue::parse(earned).expect("parses");
    assert_eq!(
        cat.query(&["queries"], "q1", Dialect::Cypher).expect("entry").status,
        Status::Verified
    );
}
