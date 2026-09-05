#![allow(non_snake_case)]
//! Fix 30 (v105): the OPTIONAL left-join tail admits a group key over a
//! NULLABLE optional var — the bare var, or a direct `var.prop` — with every
//! null-fill row keyed as Null. The Proposal listing `MATCH (p:Proposal)
//! WHERE p.status = $s OPTIONAL MATCH (p)-[:HAS_ARTIFACT]->(a) OPTIONAL MATCH
//! (p)-[:FOR_REPO]->(r) WITH p, collect(DISTINCT a.id) AS ids, r.id AS rid
//! RETURN … ORDER BY p.priority DESC, p.proposedAt DESC LIMIT 25` declined
//! the pipeline on `r.id AS rid` and ran on the general path — 292 projected
//! reads per statement on the mirror (4.3 ms against Neo4j's 2.1).
//!
//! Every answer is checked against the general path's (columnar paths off).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("s".to_string(), Value::Str("pending".to_string()));
    p
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src));
    (r, trace.counters().clone())
}

fn general(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let r = rows(g, src);
    g.set_columnar_scans(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const OPTIONAL_RUNS: &str = "interp.pipeline optional runs";
const ADMITTED: &str = "interp.pipeline optional admitted a nullable group key";

/// 600 `:Proposal {status, priority, proposedAt}` (every third pending), each
/// with 0–2 `-[:HAS_ARTIFACT]->(:Artifact {id})` (cycling over the pending
/// ones too) and, for even ids, one `-[:FOR_REPO]->(:Repo {id})` — so a third
/// of the pending proposals have no artifact and half have no repo
/// (null-fill rows on both legs).
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..600i64 {
        let mut m = BTreeMap::new();
        m.insert(
            "status".to_string(),
            Value::Str(if i % 3 == 0 { "pending".into() } else { "done".into() }),
        );
        m.insert("priority".to_string(), Value::Int(i % 7));
        m.insert("proposedAt".to_string(), Value::Int(1_000_000 - i));
        m.insert("title".to_string(), Value::Str(format!("proposal {i}")));
        let p = g.create_node(&["Proposal".into()], &m).expect("proposal");
        for k in 0..((i / 3) % 3) {
            let mut am = BTreeMap::new();
            am.insert("id".to_string(), Value::Str(format!("art-{i}-{k}")));
            let a = g.create_node(&["Artifact".into()], &am).expect("artifact");
            g.create_rel(p, "HAS_ARTIFACT", a, &BTreeMap::new()).expect("has");
        }
        if i % 2 == 0 {
            let mut rm = BTreeMap::new();
            rm.insert("id".to_string(), Value::Str(format!("repo-{}", i % 11)));
            let r = g.create_node(&["Repo".into()], &rm).expect("repo");
            g.create_rel(p, "FOR_REPO", r, &BTreeMap::new()).expect("for");
        }
    }
    g
}

const LISTING: &str = "MATCH (p:Proposal) WHERE p.status = $s \
    OPTIONAL MATCH (p)-[:HAS_ARTIFACT]->(a:Artifact) \
    OPTIONAL MATCH (p)-[:FOR_REPO]->(r:Repo) \
    WITH p, collect(DISTINCT a.id) AS ids, r.id AS rid \
    RETURN p.id AS id, p.title AS title, ids, rid ORDER BY p.priority DESC, p.proposedAt DESC LIMIT 25";

#[test]
fn a_direct_property_key_over_a_nullable_var_runs_on_the_optional_pipeline() {
    let g = corpus();
    let want = general(&g, LISTING);
    assert_eq!(want.len(), 25);
    // The fixture exercises the null key: some paged rows have no repo.
    assert!(want.iter().any(|r| matches!(r[3], Value::Null)), "a null rid is paged: {want:?}");
    let (got, c) = traced(&g, LISTING);
    assert_eq!(got, want);
    assert!(count_of(&c, ADMITTED) > 0, "{c:?}");
    assert!(count_of(&c, OPTIONAL_RUNS) > 0, "{c:?}");
}

/// A bare nullable var as a key, the key as the single native fast-path key,
/// several nullable keys, the null group counted, and the bare-RETURN form.
#[test]
fn nullable_key_variants_agree() {
    let g = corpus();
    for src in [
        "MATCH (p:Proposal) WHERE p.status = $s OPTIONAL MATCH (p)-[:FOR_REPO]->(r:Repo) \
         WITH r, count(*) AS n RETURN r.id AS id, n ORDER BY n DESC, id LIMIT 12",
        "MATCH (p:Proposal) WHERE p.status = $s OPTIONAL MATCH (p)-[:FOR_REPO]->(r:Repo) \
         WITH r.id AS rid, count(p) AS n RETURN rid, n ORDER BY n DESC, rid",
        "MATCH (p:Proposal) WHERE p.status = $s OPTIONAL MATCH (p)-[:HAS_ARTIFACT]->(a:Artifact) \
         OPTIONAL MATCH (p)-[:FOR_REPO]->(r:Repo) \
         WITH r.id AS rid, a.id AS aid, count(*) AS n RETURN rid, aid, n ORDER BY n DESC, rid, aid LIMIT 20",
        "MATCH (p:Proposal) WHERE p.status = $s OPTIONAL MATCH (p)-[:FOR_REPO]->(r:Repo) \
         RETURN r.id AS rid, count(p) AS n ORDER BY n DESC, rid",
        "MATCH (p:Proposal) WHERE p.status = $s OPTIONAL MATCH (p)-[:HAS_ARTIFACT]->(a:Artifact) \
         OPTIONAL MATCH (p)-[:FOR_REPO]->(r:Repo) \
         WITH p, collect(DISTINCT a.id) AS ids, r.id AS rid \
         RETURN p, ids, rid ORDER BY p.priority DESC, p.proposedAt DESC LIMIT 25",
    ] {
        let want = general(&g, src);
        assert!(!want.is_empty(), "fixture: `{src}`");
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert!(count_of(&c, ADMITTED) > 0, "`{src}`: {c:?}");
        assert!(count_of(&c, OPTIONAL_RUNS) > 0, "`{src}`: {c:?}");
    }
}

/// CONTROLS: a null-mapping expression over the nullable var as a key, and
/// an ORDER BY on the nullable var, still decline — and agree.
#[test]
fn null_mapping_keys_and_nullable_order_keys_still_decline() {
    let g = corpus();
    for src in [
        "MATCH (p:Proposal) WHERE p.status = $s OPTIONAL MATCH (p)-[:FOR_REPO]->(r:Repo) \
         WITH coalesce(r.id, 'none') AS rid, count(*) AS n RETURN rid, n ORDER BY n DESC, rid",
        "MATCH (p:Proposal) WHERE p.status = $s OPTIONAL MATCH (p)-[:FOR_REPO]->(r:Repo) \
         RETURN p.id AS id, r.id AS rid ORDER BY r.id, id LIMIT 10",
    ] {
        let want = general(&g, src);
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert_eq!(count_of(&c, OPTIONAL_RUNS), 0, "`{src}`: {c:?}");
    }
}
