//! LSQB q1..q9 END-TO-END, three engine paths against the `lsqbref` oracle on
//! LOCAL `snbgen` corpora — the S5b gate of `docs/lsqb-completeness-plan.md`.
//!
//! For every corpus directory named by the environment the nine counts are
//! taken four ways and must all be EQUAL:
//!   - the ORACLE (`lsqbref`, no graph engine — degree products, semi-join
//!     sums, sorted intersections, membership tests over the JSONL);
//!   - the engine with the COUNT FOLD on (the default: `pipeline::set_count_fold
//!     (true)`), which is where q1/q4/q5/q6/q8/q9 fold or probe;
//!   - the engine with the fold OFF (every hop materialised through the same
//!     columnar aggregate) — the fold's own differential twin;
//!   - the engine with columnar OFF (the per-tuple general path).
//!
//! It also reports, per query, WHICH pipeline counters fired on the default
//! path (so the expected attribution — the count fold for eight of the nine,
//! the count-only REORDER first for q2/q3, the inline edge probe for q8/q9, and
//! the OPTIONAL operator with a FOLDED leg per clause for q7 — is visible, and
//! pinned) and the LOCAL wall ratio general/fold and fold-off/fold (ratios
//! only; absolute numbers are a laptop's).
//!
//! GATED on `ENGRAM_LSQB_ALL_NINE_DIRS` — a `;`-separated list of `snbgen`
//! export dirs (the scratchpad's `snb300` and `snb1000`); without it the test
//! says so and is a no-op, so the suite never depends on a corpus it did not
//! build. The general path on 1000 persons takes minutes in a release build
//! (`lsqbref_agreement.rs` measured q3 at 200+ s):
//!
//! ```text
//! ENGRAM_LSQB_ALL_NINE_DIRS="<dir300>;<dir1000>" cargo test -p engram-bench \
//!     --release --test lsqb_all_nine -- --nocapture
//! ```
//!
//! The query texts are the lane's (`lsqb.rs`), pinned by
//! `lsqbref_agreement.rs::queries_are_the_lane_table`; they are repeated here
//! verbatim.

// A real clock for the per-query ratios — the same waiver the other bench
// targets carry; the engine crates keep the lint.
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use engram_cypher::json::from_json;
use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const QUERIES: [(&str, &str); 9] = [
    (
        "q1",
        "MATCH (:Country)<-[:IS_PART_OF]-(:City)<-[:IS_LOCATED_IN]-(:Person)\
         <-[:HAS_MEMBER]-(:Forum)-[:CONTAINER_OF]->(:Post)<-[:REPLY_OF]-(:Comment)\
         -[:HAS_TAG]->(:Tag)-[:HAS_TYPE]->(:TagClass) RETURN count(*) AS count",
    ),
    (
        "q2",
        "MATCH (person1:Person)-[:KNOWS]-(person2:Person), \
         (person1)<-[:HAS_CREATOR]-(comment:Comment)-[:REPLY_OF]->(post:Post)\
         -[:HAS_CREATOR]->(person2) RETURN count(*) AS count",
    ),
    (
        "q3",
        "MATCH (country:Country) \
         MATCH (person1:Person)-[:IS_LOCATED_IN]->(city1:City)-[:IS_PART_OF]->(country) \
         MATCH (person2:Person)-[:IS_LOCATED_IN]->(city2:City)-[:IS_PART_OF]->(country) \
         MATCH (person3:Person)-[:IS_LOCATED_IN]->(city3:City)-[:IS_PART_OF]->(country) \
         MATCH (person1)-[:KNOWS]-(person2)-[:KNOWS]-(person3)-[:KNOWS]-(person1) \
         RETURN count(*) AS count",
    ),
    (
        "q4",
        "MATCH (:Tag)<-[:HAS_TAG]-(message:Message)-[:HAS_CREATOR]->(creator:Person), \
         (message)<-[:LIKES]-(liker:Person), \
         (message)<-[:REPLY_OF]-(comment:Comment) RETURN count(*) AS count",
    ),
    (
        "q5",
        "MATCH (tag1:Tag)<-[:HAS_TAG]-(message:Message)<-[:REPLY_OF]-(comment:Comment)\
         -[:HAS_TAG]->(tag2:Tag) WHERE tag1 <> tag2 RETURN count(*) AS count",
    ),
    (
        "q6",
        "MATCH (person1:Person)-[:KNOWS]-(person2:Person)-[:KNOWS]-(person3:Person)\
         -[:HAS_INTEREST]->(tag:Tag) WHERE person1 <> person3 RETURN count(*) AS count",
    ),
    (
        "q7",
        "MATCH (:Tag)<-[:HAS_TAG]-(message:Message)-[:HAS_CREATOR]->(creator:Person) \
         OPTIONAL MATCH (message)<-[:LIKES]-(liker:Person) \
         OPTIONAL MATCH (message)<-[:REPLY_OF]-(comment:Comment) \
         RETURN count(*) AS count",
    ),
    (
        "q8",
        "MATCH (tag1:Tag)<-[:HAS_TAG]-(message:Message)<-[:REPLY_OF]-(comment:Comment)\
         -[:HAS_TAG]->(tag2:Tag) \
         WHERE NOT (comment)-[:HAS_TAG]->(tag1) AND tag1 <> tag2 \
         RETURN count(*) AS count",
    ),
    (
        "q9",
        "MATCH (person1:Person)-[:KNOWS]-(person2:Person)-[:KNOWS]-(person3:Person)\
         -[:HAS_INTEREST]->(tag:Tag) \
         WHERE NOT (person1)-[:KNOWS]-(person3) AND person1 <> person3 \
         RETURN count(*) AS count",
    ),
];

/// The counters that say which operator claimed a statement.
const ATTRIB: &[&str] = &[
    "interp.pipeline count fold",
    "interp.pipeline count fold memo",
    "interp.pipeline optional fold",
    "pipeline.count-only reordered",
    "interp.pipeline edge pred inline",
    "interp.pipeline edge pred filter",
    "interp.pipeline semijoin counted close",
    "interp.pipeline aggregate runs",
    "interp.pipeline optional runs",
    "interp.pipeline hop runs",
    "interp.pipeline join runs",
    "interp.pipeline multistage runs",
];

/// The queries the plan expects to FOLD after S1 + S2 + S3 (q7 folds its
/// OPTIONAL LEGS instead — operator D — so it is pinned separately below).
const EXPECT_FOLD: &[&str] = &["q1", "q2", "q3", "q4", "q5", "q6", "q8", "q9"];
/// The two anti-join queries must answer the probe INLINE inside the fold.
const EXPECT_INLINE_EDGE: &[&str] = &["q8", "q9"];
/// The two queries S3's count-only reorder rewrites: q2's connecting path is
/// written so its close binds the far end last, and q3 opens with a hopless
/// `(country:Country)` that `collect_hops` refuses outright.
const EXPECT_REORDER: &[&str] = &["q2", "q3"];

/// `lsqbref <dir> --json <out>`, parsed back as the flat map.
fn oracle(dir: &Path) -> BTreeMap<String, i64> {
    let out = std::env::temp_dir().join(format!(
        "lsqb-all-nine-{}-{}.json",
        std::process::id(),
        dir.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    ));
    let output = Command::new(env!("CARGO_BIN_EXE_lsqbref"))
        .arg(dir)
        .arg("--json")
        .arg(&out)
        .output()
        .expect("spawn lsqbref");
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "lsqbref failed: {}", output.status);
    let doc = std::fs::read_to_string(&out).expect("read lsqbref json");
    let _ = std::fs::remove_file(&out);
    let Value::Map(m) = from_json(&doc).expect("lsqbref output is JSON") else {
        panic!("lsqbref output is not an object: {doc}");
    };
    let mut counts = BTreeMap::new();
    for (k, v) in m {
        let Value::Int(n) = v else {
            panic!("{k}: non-integer count {v:?}")
        };
        counts.insert(k, n);
    }
    assert_eq!(
        counts.len(),
        9,
        "the oracle must answer all nine: {counts:?}"
    );
    counts
}

fn load(dir: &Path) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let stats = engram_bench::load_export(&g, dir);
    assert!(
        stats.nodes > 0 && stats.rels > 0,
        "engine loaded nothing: {stats:?}"
    );
    assert_eq!(
        stats.dangling, 0,
        "a corpus with dangling rels is not an snbgen export"
    );
    let store = g.shared_store();
    store.seal();
    store.compact();
    // The pod's 20M intermediate-row budget, so a query the general path
    // cannot finish REFUSES (a `RunError` this test reports) instead of
    // growing until the allocator aborts the process — an in-process `Graph`
    // has no budget by default.
    g.set_row_budget(Some(20_000_000));
    eprintln!(
        "[lsqb-all-nine] loaded {} nodes, {} rels from {}",
        stats.nodes,
        stats.rels,
        dir.display()
    );
    g
}

fn count_of(res: &engram_graph::QueryResult, name: &str) -> i64 {
    match res.rows.as_slice() {
        [row] => match row.as_slice() {
            [Value::Int(n)] => *n,
            other => panic!("{name}: non-integer count row {other:?}"),
        },
        other => panic!("{name}: expected one row, got {}", other.len()),
    }
}

/// The queries `ENGRAM_LSQB_ALL_NINE_SKIP` (a `,`-separated list) excludes on
/// EVERY path of every corpus — reported as SKIPPED, never as agreement.
fn skipped() -> Vec<String> {
    name_list("ENGRAM_LSQB_ALL_NINE_SKIP")
}

/// The queries `ENGRAM_LSQB_ALL_NINE_SKIP_GENERAL` excludes on the GENERAL
/// (columnar-off) path only — their oracle / fold-on / fold-off agreement is
/// still compared and still fails on a mismatch, and the general column is
/// reported as `skipped` so no reader mistakes it for agreement.
///
/// The escape hatch for a query the ENUMERATING path cannot run on a corpus
/// regardless of what the pipeline does with it. q3 on the 1000-person corpus
/// is the standing case: the general path materialises the fused four-MATCH
/// cross product (an 80 GiB allocation aborted the process, measured
/// 2026-08-29). S3's count-only reorder is a COLUMNAR pre-pass — deliberately,
/// since the general path is the differential twin the rewrite is checked
/// against — so it does not and must not rescue that arm.
fn skipped_general() -> Vec<String> {
    name_list("ENGRAM_LSQB_ALL_NINE_SKIP_GENERAL")
}

fn name_list(var: &str) -> Vec<String> {
    std::env::var(var)
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// One path over the nine: `(count, wall seconds, counters that fired)` per
/// query; a skipped query has no entry.
fn engine(g: &Graph, columnar: bool, fold: bool) -> BTreeMap<String, (i64, f64, Vec<String>)> {
    g.set_columnar_scans(columnar);
    engram_graph::pipeline::set_count_fold(fold);
    let label = match (columnar, fold) {
        (false, _) => "general",
        (true, true) => "fold-on",
        (true, false) => "fold-off",
    };
    let skip = skipped();
    let skip_general = skipped_general();
    let mut out = BTreeMap::new();
    for (name, text) in QUERIES {
        if skip.iter().any(|s| s == name) {
            eprintln!("[lsqb-all-nine] {label:<8} {name} SKIPPED (ENGRAM_LSQB_ALL_NINE_SKIP)");
            continue;
        }
        if !columnar && skip_general.iter().any(|s| s == name) {
            eprintln!(
                "[lsqb-all-nine] {label:<8} {name} SKIPPED (ENGRAM_LSQB_ALL_NINE_SKIP_GENERAL)"
            );
            continue;
        }
        let q = parse_statement(text).unwrap_or_else(|e| panic!("{name}: parse: {e:?}"));
        // Warm once (adjacency tables / member sets build on first use), then
        // time the steady run — the ratios compare paths, not build costs.
        let (_, _) = engram_observe::with_trace(|| run_query(g, &q, BTreeMap::new()));
        let t = Instant::now();
        let (res, trace) = engram_observe::with_trace(|| run_query(g, &q, BTreeMap::new()));
        let secs = t.elapsed().as_secs_f64();
        let res = res.unwrap_or_else(|e| panic!("{name} ({label}): run: {e:?}"));
        let n = count_of(&res, name);
        let fired: Vec<String> = ATTRIB
            .iter()
            .filter(|k| trace.counters().get(**k).copied().unwrap_or(0) > 0)
            .map(|k| k.trim_start_matches("interp.pipeline ").to_string())
            .collect();
        eprintln!("[lsqb-all-nine] {label:<8} {name} = {n:<12} {secs:>8.3}s  {fired:?}");
        out.insert(name.to_string(), (n, secs, fired));
    }
    engram_graph::pipeline::set_count_fold(true);
    g.set_columnar_scans(true);
    out
}

fn check_corpus(dir: &Path) {
    let oracle = oracle(dir);
    let g = load(dir);
    let fold_on = engine(&g, true, true);
    let fold_off = engine(&g, true, false);
    let general = engine(&g, false, true);

    eprintln!("[lsqb-all-nine] corpus {}", dir.display());
    eprintln!(
        "[lsqb-all-nine] {:<3} {:>12} {:>12} {:>12} {:>12}  {:>9} {:>9}  fold-on path",
        "q", "oracle", "fold-on", "fold-off", "general", "gen/fold", "off/fold"
    );
    let mut bad = Vec::new();
    let mut compared = 0usize;
    for (name, _) in QUERIES {
        let o = oracle[name];
        let (Some((on, t_on, fired)), Some((off, t_off, _))) =
            (fold_on.get(name), fold_off.get(name))
        else {
            eprintln!("[lsqb-all-nine] {name:<3} {o:>12}      SKIPPED (not compared)");
            continue;
        };
        compared += 1;
        // The GENERAL arm may be skipped for this query alone
        // (`ENGRAM_LSQB_ALL_NINE_SKIP_GENERAL`); its column then reads
        // `skipped`, never a number, so no reader takes it for agreement.
        let ge = general.get(name);
        let ok = *on == o && *off == o && ge.is_none_or(|(g, _, _)| *g == o);
        let (ge_txt, ge_ratio) = match ge {
            Some((g, t_ge, _)) => (g.to_string(), format!("{:>8.2}x", t_ge / t_on.max(1e-9))),
            None => ("skipped".to_string(), format!("{:>9}", "n/a")),
        };
        eprintln!(
            "[lsqb-all-nine] {name:<3} {o:>12} {on:>12} {off:>12} {ge_txt:>12}  {ge_ratio} {:>8.2}x  {}",
            t_off / t_on.max(1e-9),
            fired.join(",")
        );
        if !ok {
            bad.push(format!(
                "{name}: oracle {o}, fold-on {on}, fold-off {off}, general {ge_txt}"
            ));
        }
        assert!(o > 0, "{name}: the oracle counted 0 on a populated corpus");
        // The attribution the plan expects after S1 + S2, pinned.
        if EXPECT_FOLD.contains(&name) {
            assert!(
                fired.iter().any(|f| f == "count fold"),
                "{name} must fold on the default path; fired {fired:?}"
            );
        }
        if EXPECT_INLINE_EDGE.contains(&name) {
            assert!(
                fired.iter().any(|f| f == "edge pred inline"),
                "{name} must answer its anti-join inline; fired {fired:?}"
            );
        }
        if EXPECT_REORDER.contains(&name) {
            assert!(
                fired.iter().any(|f| f == "pipeline.count-only reordered"),
                "{name} must be reordered before it can fold; fired {fired:?}"
            );
        }
        if name == "q7" {
            assert!(
                fired.iter().any(|f| f == "optional runs"),
                "q7 is the OPTIONAL operator's shape; fired {fired:?}"
            );
            assert!(
                fired.iter().any(|f| f == "optional fold"),
                "q7's two legs must FOLD (operator D); fired {fired:?}"
            );
        }
    }
    assert!(
        bad.is_empty(),
        "oracle and engine disagree on {}:\n  {}",
        dir.display(),
        bad.join("\n  ")
    );
    // A skip list that empties the comparison compared nothing.
    assert!(
        compared >= 8,
        "only {compared} of nine compared on {}",
        dir.display()
    );
}

#[test]
fn all_nine_agree_with_the_oracle_on_every_corpus() {
    let Ok(dirs) = std::env::var("ENGRAM_LSQB_ALL_NINE_DIRS") else {
        eprintln!("[lsqb-all-nine] ENGRAM_LSQB_ALL_NINE_DIRS unset — skipping (see the header)");
        return;
    };
    let mut ran = 0usize;
    for dir in dirs.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let dir = Path::new(dir);
        assert!(
            dir.join("nodes.jsonl").is_file() && dir.join("rels.jsonl").is_file(),
            "{}: not an snbgen export dir",
            dir.display()
        );
        check_corpus(dir);
        ran += 1;
    }
    assert!(ran > 0, "ENGRAM_LSQB_ALL_NINE_DIRS named no directory");
}
