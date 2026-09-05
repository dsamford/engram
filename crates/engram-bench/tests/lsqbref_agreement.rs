//! `lsqbref` ↔ engine AGREEMENT — the oracle's second gate.
//!
//! The oracle (`src/bin/lsqbref.rs`) computes the nine LSQB counts with no
//! graph engine; its first gate is hand-derived counts on a ten-node fixture
//! (its own unit tests). This is the second: on corpora small enough for the
//! engine's GENERAL path to finish, every one of the nine counts the oracle
//! produces must equal what the engine produces for the same query text —
//! on the general path (`Graph::set_columnar_scans(false)`) AND on the
//! default columnar path, because the engine's two paths are themselves a
//! differential pair and this is the one place all three meet.
//!
//! The corpora are built HERE (`snbgen`, 300 and 1000 persons, seed 1 —
//! deterministic byte for byte) rather than read from an environment
//! variable, so the suite never depends on a corpus it did not build and the
//! test is never a silent no-op. Both binaries are invoked through
//! `CARGO_BIN_EXE_*`, which cargo sets for integration tests of the package
//! that owns them.
//!
//! The query texts are copied verbatim from `lsqb.rs`'s table and a test
//! pins them to that source file, so this cannot drift into comparing the
//! oracle against a different query than the lane measures.

// A real clock for the per-query timings the table reports — the same waiver
// the other bench targets carry; the engine crates keep the lint.
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use engram_cypher::json::from_json;
use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// The nine, verbatim from `lsqb.rs` (pinned by `queries_are_the_lane_table`).
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

/// A fresh, process-unique corpus directory under the system temp dir.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lsqbref-agreement-{}-{name}",
        std::process::id()
    ));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("clear stale scratch dir");
    }
    dir
}

/// `snbgen <dir> <persons> 1`.
fn generate(dir: &Path, persons: u64) {
    let status = Command::new(env!("CARGO_BIN_EXE_snbgen"))
        .arg(dir)
        .arg(persons.to_string())
        .arg("1")
        .status()
        .expect("spawn snbgen");
    assert!(status.success(), "snbgen failed: {status}");
    assert!(dir.join("nodes.jsonl").is_file() && dir.join("rels.jsonl").is_file());
}

/// `lsqbref <dir> --json <out>`, parsed back as the flat map.
fn oracle(dir: &Path) -> BTreeMap<String, i64> {
    let out = dir.join("lsqbref.json");
    let output = Command::new(env!("CARGO_BIN_EXE_lsqbref"))
        .arg(dir)
        .arg("--json")
        .arg(&out)
        .output()
        .expect("spawn lsqbref");
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "lsqbref failed: {}", output.status);
    let doc = std::fs::read_to_string(&out).expect("read lsqbref.json");
    let Value::Map(m) = from_json(&doc).expect("lsqbref output is JSON") else {
        panic!("lsqbref output is not an object: {doc}");
    };
    let mut counts = BTreeMap::new();
    for (k, v) in m {
        let Value::Int(n) = v else { panic!("{k}: non-integer count {v:?}") };
        counts.insert(k, n);
    }
    assert_eq!(counts.len(), 9, "the oracle must answer all nine: {counts:?}");
    counts
}

/// Load the corpus in-process and run the nine through the engine on one
/// path. `columnar = false` is the general path.
fn engine(dir: &Path, columnar: bool) -> BTreeMap<String, i64> {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let stats = engram_bench::load_export(&g, dir);
    assert!(stats.nodes > 0 && stats.rels > 0, "engine loaded nothing: {stats:?}");
    assert_eq!(stats.dangling, 0, "a corpus with dangling rels is not the one snbgen wrote");
    let store = g.shared_store();
    store.seal();
    store.compact();
    g.set_columnar_scans(columnar);
    let path = if columnar { "columnar" } else { "general" };
    let mut counts = BTreeMap::new();
    for (name, text) in QUERIES {
        let q = parse_statement(text).unwrap_or_else(|e| panic!("{name}: parse: {e:?}"));
        let t = Instant::now();
        let res = run_query(&g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("{name}: run: {e:?}"));
        eprintln!(
            "[lsqbref-agreement] engine {path:<8} {name} in {:.1}s",
            t.elapsed().as_secs_f64()
        );
        let n = match res.rows.as_slice() {
            [row] => match row.as_slice() {
                [Value::Int(n)] => *n,
                other => panic!("{name}: non-integer count row {other:?}"),
            },
            other => panic!("{name}: expected one row, got {}", other.len()),
        };
        counts.insert(name.to_string(), n);
    }
    counts
}

/// Generate, count three ways, print the agreement table, assert.
fn agree(persons: u64) {
    let dir = scratch(&format!("snb{persons}"));
    generate(&dir, persons);
    let oracle = oracle(&dir);
    let general = engine(&dir, false);
    let columnar = engine(&dir, true);

    eprintln!("[lsqbref-agreement] snbgen persons={persons} seed=1");
    eprintln!("[lsqbref-agreement] {:<4} {:>12} {:>12} {:>12}  verdict", "q", "oracle", "general", "columnar");
    let mut disagreements = Vec::new();
    for (name, _) in QUERIES {
        let (o, ge, co) = (oracle[name], general[name], columnar[name]);
        let ok = o == ge && o == co;
        eprintln!(
            "[lsqbref-agreement] {name:<4} {o:>12} {ge:>12} {co:>12}  {}",
            if ok { "agree" } else { "DISAGREE" }
        );
        if !ok {
            disagreements.push(format!("{name}: oracle {o}, general {ge}, columnar {co}"));
        }
        // A zero on a populated corpus would be the vacuous kind of agreement;
        // every one of the nine is non-empty on these corpora.
        assert!(o > 0, "{name}: the oracle counted 0 on a populated corpus");
    }
    assert!(
        disagreements.is_empty(),
        "oracle and engine disagree on {persons} persons:\n  {}",
        disagreements.join("\n  ")
    );
    std::fs::remove_dir_all(&dir).expect("remove scratch dir");
}

/// Always on: a 40-person corpus every one of the nine is non-empty on
/// (q6 ≈ 2×10⁵ rows, q9 ≈ 9×10⁴) and the engine's general path finishes in
/// seconds even unoptimised.
#[test]
fn oracle_agrees_with_engine_on_40_persons() {
    agree(40);
}

/// The full pair the oracle is gated on: 300 and 1000 persons. The engine's
/// general path on the 1000-person corpus materialises 1.35×10⁷ rows for q6
/// and takes minutes in a release build (q3 alone ran 200+ s, measured), so
/// this runs only when asked for, and says so rather than passing silently:
///
/// ```text
/// ENGRAM_LSQBREF_AGREEMENT=full cargo test -p engram-bench --release \
///     --test lsqbref_agreement -- --nocapture
/// ```
fn full_or_skip(persons: u64) {
    if std::env::var("ENGRAM_LSQBREF_AGREEMENT").as_deref() != Ok("full") {
        eprintln!(
            "[lsqbref-agreement] {persons} persons SKIPPED — set ENGRAM_LSQBREF_AGREEMENT=full \
             (and build --release) to run the engine on it; the 40-person differential still ran"
        );
        return;
    }
    agree(persons);
}

#[test]
fn oracle_agrees_with_engine_on_300_persons() {
    full_or_skip(300);
}

#[test]
fn oracle_agrees_with_engine_on_1000_persons() {
    full_or_skip(1000);
}

/// The texts above ARE the lane's table: read `lsqb.rs`'s source with its
/// `\`-continuations joined and require every text to appear in it.
#[test]
fn queries_are_the_lane_table() {
    let src: String = include_str!("../src/bin/lsqb.rs").chars().filter(|&c| c != '\r').collect();
    let mut joined = String::with_capacity(src.len());
    let mut rest = src.as_str();
    while let Some(i) = rest.find("\\\n") {
        joined.push_str(&rest[..i]);
        rest = rest[i + 2..].trim_start_matches([' ', '\t']);
    }
    joined.push_str(rest);
    for (name, text) in QUERIES {
        assert!(joined.contains(text), "{name}: differs from lsqb.rs:\n{text}");
    }
}
