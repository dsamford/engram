#![allow(non_snake_case)]
//! Lever G' (fix 27, v103): a bare node carried out of an aggregating WITH
//! into a concluding top-k RETURN — `WITH p, collect(DISTINCT a.id) AS ids
//! RETURN p, ids ORDER BY p.priority DESC, p.proposedAt DESC LIMIT 25` — is
//! bound LEAN at the WITH (only the properties the RETURN's other items and
//! ORDER BY read) and hydrated in FULL at the RETURN for its k survivors
//! alone. Until this held every grouped node was decoded in full for the k
//! the statement paged (the mirror's Proposal listing: 7.2 ms against
//! Neo4j's 2.1).
//!
//! Every answer is checked against the same statement with late projection
//! OFF (the full-materialising path), row for row.

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

/// Traced on the GENERAL path (columnar paths off): the lever under test is
/// the streaming interpreter's; the OPTIONAL-MATCH pipeline takes the same
/// shapes since fix 30 and materialises its groups its own way.
fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    g.set_columnar_scans(false);
    let (r, trace) = engram_observe::with_trace(|| rows(g, src));
    g.set_columnar_scans(true);
    (r, trace.counters().clone())
}

fn eager(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    g.set_late_projection(false);
    let r = rows(g, src);
    g.set_late_projection(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const FULL: &str = "graph.nodes materialised in full";
const LEAN: &str = "interp.breaker bound a bare carry lean for the RETURN's top-k";
const HYDRATED: &str = "interp.late projection re-materialised a carried node for a survivor";
const EAGER: &str = "interp.late full carry hydrated eagerly";

/// 1,500 `:Proposal {status, priority, proposedAt, title, body}` (every
/// third pending; `body` is 2 KB so a full decode is visible), each with
/// 0–2 `-[:HAS_ARTIFACT]->(:Artifact {id})` and, for even ids, one
/// `-[:FOR_REPO]->(:Repo {id})`.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let body: String = "x".repeat(2048);
    for i in 0..1500i64 {
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
        for k in 0..(i % 3) {
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
fn a_bare_carry_into_a_topk_return_is_hydrated_for_the_survivors_only() {
    let g = corpus();
    let want = eager(&g, LISTING);
    assert_eq!(want.len(), 25, "fixture pages 25 of 500 pending");
    let (got, c) = traced(&g, LISTING);
    assert_eq!(got, want);
    assert!(count_of(&c, LEAN) > 0, "the WITH bound p lean: {c:?}");
    assert_eq!(count_of(&c, HYDRATED), 25, "only the survivors are hydrated: {c:?}");
    assert_eq!(count_of(&c, FULL), 25, "nothing else is decoded in full: {c:?}");
    assert_eq!(count_of(&c, EAGER), 0, "{c:?}");
    // The output IS the full node: every property the fixture wrote.
    let Value::Node { props, .. } = &got[0][0] else {
        panic!("first column is the node: {:?}", got[0][0]);
    };
    assert_eq!(props.len(), 5, "{props:?}");
}

/// SKIP + LIMIT hydrates skip+limit survivors; an alias on the bare item,
/// an ORDER BY through that alias, and an ORDER BY on another column all
/// keep the shape.
#[test]
fn skip_alias_and_foreign_order_keys_keep_the_shape() {
    let g = corpus();
    // (statement, rows returned, survivors hydrated = skip + limit)
    for (src, n, k) in [
        (
            "MATCH (p:Proposal) WHERE p.status = $s \
             OPTIONAL MATCH (p)-[:HAS_ARTIFACT]->(a:Artifact) \
             WITH p, collect(DISTINCT a.id) AS ids \
             RETURN p, ids ORDER BY p.priority DESC, p.proposedAt DESC SKIP 5 LIMIT 10",
            10,
            15,
        ),
        (
            "MATCH (p:Proposal) WHERE p.status = $s \
             OPTIONAL MATCH (p)-[:HAS_ARTIFACT]->(a:Artifact) \
             WITH p, collect(DISTINCT a.id) AS ids \
             RETURN p AS proposal, ids ORDER BY proposal.priority DESC, p.proposedAt ASC LIMIT 7",
            7,
            7,
        ),
        (
            "MATCH (p:Proposal) WHERE p.status = $s \
             OPTIONAL MATCH (p)-[:HAS_ARTIFACT]->(a:Artifact) \
             WITH p, collect(DISTINCT a.id) AS ids \
             RETURN p, size(ids) AS n, p.title AS t ORDER BY n DESC, p.proposedAt DESC LIMIT 5",
            5,
            5,
        ),
    ] {
        let want = eager(&g, src);
        assert_eq!(want.len(), n, "`{src}`");
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert!(count_of(&c, LEAN) > 0, "`{src}`: {c:?}");
        assert_eq!(count_of(&c, HYDRATED), k as u64, "`{src}`: {c:?}");
        assert_eq!(count_of(&c, FULL), k as u64, "`{src}`: {c:?}");
    }
}

/// An ordering WITH before the RETURN: the WITH's own late projection keeps
/// the node lean across its top-k and the RETURN hydrates its survivors.
#[test]
fn a_lean_carry_across_two_topks_is_hydrated_once_at_the_end() {
    let g = corpus();
    let src = "MATCH (p:Proposal) WHERE p.status = $s \
               WITH p ORDER BY p.priority DESC LIMIT 100 \
               RETURN p ORDER BY p.proposedAt ASC LIMIT 10";
    let want = eager(&g, src);
    assert_eq!(want.len(), 10);
    let (got, c) = traced(&g, src);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, HYDRATED), 10, "{c:?}");
    assert_eq!(count_of(&c, FULL), 10, "{c:?}");
}

/// CONTROLS: no LIMIT, a whole-entity use (`labels(p)`), DISTINCT, and an
/// ORDER BY on the node itself all keep the full carry — and agree.
#[test]
fn shapes_outside_the_late_full_class_keep_the_full_carry() {
    let g = corpus();
    for src in [
        "MATCH (p:Proposal) WHERE p.status = $s \
         OPTIONAL MATCH (p)-[:HAS_ARTIFACT]->(a:Artifact) \
         WITH p, collect(DISTINCT a.id) AS ids \
         RETURN p, ids ORDER BY p.priority DESC, p.proposedAt DESC",
        "MATCH (p:Proposal) WHERE p.status = $s \
         OPTIONAL MATCH (p)-[:HAS_ARTIFACT]->(a:Artifact) \
         WITH p, collect(DISTINCT a.id) AS ids \
         RETURN labels(p) AS l, p.title AS t, ids ORDER BY p.priority DESC, p.proposedAt DESC LIMIT 5",
        "MATCH (p:Proposal) WHERE p.status = $s \
         OPTIONAL MATCH (p)-[:HAS_ARTIFACT]->(a:Artifact) \
         WITH p, collect(DISTINCT a.id) AS ids \
         RETURN DISTINCT p, ids ORDER BY p.priority DESC, p.proposedAt DESC LIMIT 5",
    ] {
        let want = eager(&g, src);
        assert!(!want.is_empty(), "fixture: `{src}`");
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert_eq!(count_of(&c, LEAN), 0, "`{src}`: {c:?}");
        assert_eq!(count_of(&c, HYDRATED), 0, "`{src}`: {c:?}");
    }
    // Fix 72: a pattern-shaped subquery names the carry as a bare ENDPOINT
    // — an identity use, the expansion starts from its id — so the shape
    // is INSIDE the class: the carry binds lean and the five survivors
    // hydrate, and the rows agree with the eager form.
    let src = "MATCH (p:Proposal) WHERE p.status = $s \
         OPTIONAL MATCH (p)-[:HAS_ARTIFACT]->(a:Artifact) \
         WITH p, collect(DISTINCT a.id) AS ids \
         RETURN p, ids, count { (p)-[:FOR_REPO]->() } AS repos ORDER BY p.priority DESC LIMIT 5";
    let want = eager(&g, src);
    assert_eq!(want.len(), 5, "fixture: `{src}`");
    let (got, c) = traced(&g, src);
    assert_eq!(got, want, "`{src}`");
    assert_eq!(count_of(&c, LEAN), 1, "`{src}`: {c:?}");
    assert_eq!(count_of(&c, HYDRATED), 5, "`{src}`: {c:?}");
}
