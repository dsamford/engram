#![allow(non_snake_case)]
//! Fix 31 (v106): lever G' on the pipeline's Form-A aggregate tail. A group-key
//! carry the top-k RETURN outputs BARE and otherwise reads only by property —
//! `WITH p, collect(DISTINCT a.id) AS ids, r.id AS rid RETURN p, ids, rid
//! ORDER BY p.priority DESC, p.proposedAt DESC LIMIT 25` — is gathered like
//! any other carry (its ORDER BY props) and the k output rows are hydrated
//! with the full node afterwards. Until this held the bare item kept every
//! group's key in full: 73 Proposal records decoded for the 25 paged, once
//! the OPTIONAL left join (fix 30) took the listing off the streaming path
//! whose own G' (fix 27) hydrates only the survivors.
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

const FULL: &str = "graph.nodes materialised in full";
const HYDRATED: &str = "interp.agg bare return item hydrated for a survivor";
const OPTIONAL_RUNS: &str = "interp.pipeline optional runs";
const AGG_RUNS: &str = "interp.pipeline aggregate runs";
const HOP_RUNS: &str = "interp.pipeline hop runs";

/// 600 `:Proposal {status, priority, proposedAt, title, body}` (every third
/// pending, `body` 1 KB so a full decode is visible), 0–2 artifacts each
/// (cycling over the pending ones too), a repo on even ids.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let body: String = "y".repeat(1024);
    for i in 0..600i64 {
        let mut m = BTreeMap::new();
        m.insert(
            "status".to_string(),
            Value::Str(if i % 3 == 0 { "pending".into() } else { "done".into() }),
        );
        m.insert("priority".to_string(), Value::Int(i % 7));
        m.insert("proposedAt".to_string(), Value::Int(1_000_000 - i));
        m.insert("title".to_string(), Value::Str(format!("proposal {i}")));
        m.insert("body".to_string(), Value::Str(body.clone()));
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
    RETURN p, ids, rid ORDER BY p.priority DESC, p.proposedAt DESC LIMIT 25";

#[test]
fn the_listing_hydrates_only_its_survivors_on_the_optional_pipeline() {
    let g = corpus();
    let want = general(&g, LISTING);
    assert_eq!(want.len(), 25);
    let (got, c) = traced(&g, LISTING);
    assert_eq!(got, want);
    assert!(count_of(&c, OPTIONAL_RUNS) > 0, "{c:?}");
    assert_eq!(count_of(&c, HYDRATED), 25, "{c:?}");
    assert_eq!(count_of(&c, FULL), 25, "only the survivors are decoded in full: {c:?}");
    let Value::Node { props, .. } = &got[0][0] else {
        panic!("first column is the node: {:?}", got[0][0]);
    };
    assert_eq!(props.len(), 5, "the output is the full node: {props:?}");
}

/// The single-MATCH aggregate tail and the DISTINCT-WITH tail (fix 29) with
/// a bare RETURN item beside a top-k; SKIP counts toward the survivors; an
/// alias on the bare item and an ORDER BY through it keep the shape.
#[test]
fn aggregate_and_distinct_tails_hydrate_their_survivors() {
    let g = corpus();
    for (src, k) in [
        (
            "MATCH (p:Proposal)-[:HAS_ARTIFACT]->(a:Artifact) \
             WITH p, count(a) AS n RETURN p, n ORDER BY n DESC, p.priority DESC, p.proposedAt LIMIT 10",
            10,
        ),
        (
            "MATCH (p:Proposal)-[:HAS_ARTIFACT]->(a:Artifact) \
             WITH DISTINCT p RETURN p ORDER BY p.priority DESC, p.proposedAt LIMIT 7",
            7,
        ),
        (
            "MATCH (p:Proposal)-[:HAS_ARTIFACT]->(a:Artifact) \
             WITH p, count(a) AS n RETURN p AS proposal, n ORDER BY proposal.priority DESC, n, p.proposedAt SKIP 4 LIMIT 6",
            6,
        ),
    ] {
        let want = general(&g, src);
        assert_eq!(want.len(), k, "`{src}`");
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert!(count_of(&c, AGG_RUNS) + count_of(&c, HOP_RUNS) > 0, "`{src}`: {c:?}");
        assert!(count_of(&c, HYDRATED) >= k as u64, "`{src}`: {c:?}");
        assert!(count_of(&c, FULL) <= count_of(&c, HYDRATED), "`{src}`: nothing beyond the survivors: {c:?}");
    }
}

/// CONTROLS: no LIMIT, a whole-entity read, a post-WHERE over the bare var,
/// and a DISTINCT RETURN keep the full carry — and agree.
#[test]
fn shapes_outside_the_class_keep_the_full_carry_and_agree() {
    let g = corpus();
    for src in [
        "MATCH (p:Proposal)-[:HAS_ARTIFACT]->(a:Artifact) \
         WITH p, count(a) AS n RETURN p, n ORDER BY n DESC, p.priority DESC",
        "MATCH (p:Proposal)-[:HAS_ARTIFACT]->(a:Artifact) \
         WITH p, count(a) AS n RETURN labels(p) AS l, p.title AS t, n ORDER BY n DESC, p.priority DESC LIMIT 5",
        "MATCH (p:Proposal)-[:HAS_ARTIFACT]->(a:Artifact) \
         WITH p, count(a) AS n WHERE p.priority > 2 RETURN p, n ORDER BY n DESC, p.priority DESC LIMIT 5",
        "MATCH (p:Proposal)-[:HAS_ARTIFACT]->(a:Artifact) \
         WITH p, count(a) AS n RETURN DISTINCT p, n ORDER BY n DESC, p.priority DESC LIMIT 5",
    ] {
        let want = general(&g, src);
        assert!(!want.is_empty(), "fixture: `{src}`");
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        if src.contains("WHERE p.priority") {
            // A property-only post-WHERE admits the carry: still hydrated.
            assert!(count_of(&c, HYDRATED) > 0, "`{src}`: {c:?}");
        } else {
            assert_eq!(count_of(&c, HYDRATED), 0, "`{src}`: {c:?}");
        }
    }
}
