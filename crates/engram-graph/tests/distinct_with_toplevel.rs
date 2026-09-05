#![allow(non_snake_case)]
//! Fix 29 (v105): `MATCH … WITH DISTINCT <keys> [WHERE] RETURN <over the keys>
//! ORDER BY … LIMIT …` is recognised at the TOP level. The Form-A DISTINCT
//! tail existed but only the multistage tails reached it, so the KMProject
//! listing — `MATCH (p:KMProject)-[t:TRACKS_REPO]->(lore:Repo:ManagedRepo)
//! WHERE lore.provider = 'lore' AND coalesce(t.primary, false) = true AND
//! coalesce(lore.syncMode, 'onboarding') IN $modes WITH DISTINCT lore RETURN
//! lore.orgId AS orgId, … ORDER BY lore.repoId LIMIT toInteger($limit)` — ran
//! on the general path: two stages, every TRACKS_REPO relationship decoded in
//! full, each repo projected twice (2.1–2.6 ms on the mirror vs Neo4j's 1.2).
//!
//! Every answer is checked against the general path's (columnar paths off).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert(
        "modes".to_string(),
        Value::List(vec![Value::Str("push".into()), Value::Str("onboarding".into())]),
    );
    p.insert("limit".to_string(), Value::Int(25));
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

const HOP_RUNS: &str = "interp.pipeline hop runs";
const TOPLEVEL: &str = "interp.pipeline distinct WITH tail recognised at the top level";
const FULL: &str = "graph.nodes materialised in full";
const REL_FULL: &str = "graph.rels materialised in full";

/// 80 `:KMProject`, 140 `:Repo:ManagedRepo` (provider lore / github,
/// syncMode push / onboarding / absent, defaultBranch on two of three), each
/// project tracking one to three repos with `primary` true on some, false or
/// absent on the rest; several projects share a repo so DISTINCT matters.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut repos = Vec::new();
    for i in 0..140i64 {
        let mut m = BTreeMap::new();
        m.insert(
            "provider".to_string(),
            Value::Str(if i % 4 == 0 { "github".into() } else { "lore".into() }),
        );
        match i % 3 {
            0 => {
                m.insert("syncMode".to_string(), Value::Str("push".into()));
            }
            1 => {
                m.insert("syncMode".to_string(), Value::Str("onboarding".into()));
            }
            _ => {}
        }
        if i % 5 == 0 {
            m.insert("syncMode".to_string(), Value::Str("mirror".into()));
        }
        m.insert("orgId".to_string(), Value::Str(format!("org-{}", i % 7)));
        m.insert("externalId".to_string(), Value::Str(format!("ext-{i:04}")));
        m.insert("repoId".to_string(), Value::Str(format!("repo-{:04}", (i * 37) % 140)));
        if i % 3 != 2 {
            m.insert("defaultBranch".to_string(), Value::Str("trunk".into()));
        }
        repos.push(
            g.create_node(&["Repo".into(), "ManagedRepo".into()], &m)
                .expect("repo"),
        );
    }
    for i in 0..80i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Str(format!("proj-{i:03}")));
        let p = g.create_node(&["KMProject".into()], &m).expect("project");
        for k in 0..(1 + i % 3) {
            let r = repos[((i * 11 + k * 29) % 140) as usize];
            let mut rm = BTreeMap::new();
            match (i + k) % 3 {
                0 => {
                    rm.insert("primary".to_string(), Value::Bool(true));
                }
                1 => {
                    rm.insert("primary".to_string(), Value::Bool(false));
                }
                _ => {}
            }
            g.create_rel(p, "TRACKS_REPO", r, &rm).expect("tracks");
        }
    }
    g
}

const LISTING: &str = "MATCH (p:KMProject)-[t:TRACKS_REPO]->(lore:Repo:ManagedRepo) \
    WHERE lore.provider = 'lore' AND coalesce(t.primary, false) = true \
    AND coalesce(lore.syncMode, 'onboarding') IN $modes \
    WITH DISTINCT lore \
    RETURN lore.orgId AS orgId, lore.externalId AS repositoryId, lore.repoId AS managedRepoId, \
    coalesce(lore.defaultBranch, 'main') AS branch \
    ORDER BY lore.repoId LIMIT toInteger($limit)";

#[test]
fn the_km_listing_runs_on_the_pipeline_and_decodes_nothing_in_full() {
    let g = corpus();
    let want = general(&g, LISTING);
    assert!(want.len() >= 5, "fixture: {} rows", want.len());
    let (got, c) = traced(&g, LISTING);
    assert_eq!(got, want);
    assert!(count_of(&c, TOPLEVEL) > 0, "{c:?}");
    assert!(count_of(&c, HOP_RUNS) > 0, "{c:?}");
    assert_eq!(count_of(&c, FULL), 0, "the repos are gathered, not decoded in full: {c:?}");
    assert_eq!(count_of(&c, REL_FULL), 0, "the relationship's one property is a column: {c:?}");
}

/// Variants of the shape: a value key, a HAVING, several keys, a bare key
/// output, SKIP.
#[test]
fn distinct_with_variants_agree_on_the_pipeline() {
    let g = corpus();
    for src in [
        "MATCH (p:KMProject)-[:TRACKS_REPO]->(lore:ManagedRepo) \
         WITH DISTINCT lore.orgId AS orgId RETURN orgId ORDER BY orgId",
        "MATCH (p:KMProject)-[:TRACKS_REPO]->(lore:ManagedRepo) \
         WITH DISTINCT lore WHERE lore.provider = 'lore' RETURN lore.repoId AS r ORDER BY r DESC LIMIT 7",
        "MATCH (p:KMProject)-[:TRACKS_REPO]->(lore:ManagedRepo) \
         WITH DISTINCT lore, p.id AS pid RETURN pid, lore.repoId AS r ORDER BY pid, r SKIP 3 LIMIT 10",
        "MATCH (p:KMProject)-[:TRACKS_REPO]->(lore:ManagedRepo) \
         WITH DISTINCT lore RETURN lore ORDER BY lore.repoId LIMIT 5",
        // DISTINCT is by the NODE, not by the projected value: many repos
        // share a provider, and every distinct repo keeps its own row. (The
        // gathered stand-in was a bare map once, and three property-identical
        // nodes collapsed into one row.)
        "MATCH (p:KMProject)-[:TRACKS_REPO]->(lore:ManagedRepo) \
         WITH DISTINCT lore RETURN lore.provider AS provider ORDER BY provider",
    ] {
        let want = general(&g, src);
        assert!(!want.is_empty(), "fixture: `{src}`");
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert!(count_of(&c, TOPLEVEL) > 0, "`{src}`: {c:?}");
    }
}

/// CONTROLS: a RETURN reading a var the WITH dropped, a non-DISTINCT WITH,
/// and an aggregate in the RETURN keep their paths — and agree (or refuse
/// identically).
#[test]
fn shapes_outside_the_class_still_agree() {
    let g = corpus();
    for src in [
        "MATCH (p:KMProject)-[:TRACKS_REPO]->(lore:ManagedRepo) \
         WITH lore RETURN lore.repoId AS r ORDER BY r LIMIT 5",
        "MATCH (p:KMProject)-[:TRACKS_REPO]->(lore:ManagedRepo) \
         WITH DISTINCT lore RETURN count(lore) AS n",
    ] {
        let want = general(&g, src);
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert_eq!(count_of(&c, TOPLEVEL), 0, "`{src}`: {c:?}");
    }
    // A RETURN over a dropped var refuses on both paths.
    let src = "MATCH (p:KMProject)-[:TRACKS_REPO]->(lore:ManagedRepo) WITH DISTINCT lore RETURN p.id AS id";
    let q = parse_statement(src).expect("parse");
    let on = run_query(&g, &q, params()).is_err();
    g.set_columnar_scans(false);
    let off = run_query(&g, &q, params()).is_err();
    g.set_columnar_scans(true);
    assert!(on && off, "both paths refuse a read of the dropped variable");
}
