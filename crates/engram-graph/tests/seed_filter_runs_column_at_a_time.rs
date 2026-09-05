#![allow(non_snake_case)]
//! Fix 67: the general path's seed column filter (`filter_ids`) bound a
//! scope and walked its predicate PER MEMBER even when every column it
//! read was cached. The production AcceptanceCriterion listing — no index
//! on `proposalId` on either engine — evaluated 4,235 expressions over its
//! 2k cached members per statement: 3.0 ms on the mirror against Neo4j's
//! 0.7 ms label scan. A whole-label filter over cached columns now runs
//! column-at-a-time, the projection path's evaluator since fix 40.
//!
//! Every answer is checked against the same statement with the columnar
//! paths OFF (the per-member general path).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params(proposal: &str) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("proposalId".to_string(), Value::Str(proposal.into()));
    p.insert("viewerOrgId".to_string(), Value::Str("default".into()));
    p
}

fn rows(g: &Graph, src: &str, proposal: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params(proposal))
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str, proposal: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src, proposal));
    (r, trace.counters().clone())
}

fn general(g: &Graph, src: &str, proposal: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let r = rows(g, src, proposal);
    g.set_columnar_scans(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

const VECTORISED: &str = "interp.seed column filter evaluated column-at-a-time";
const FILTERED: &str = "interp.seeds filtered by columns";
const EXPRESSIONS: &str = "cypher.expressions evaluated";

/// 2,000 acceptance criteria over 100 proposals (20 each), orgId `default`
/// for three of every four; every criterion of the first 50 proposals is
/// VERIFIED_BY one of 200 test cases (orgId `default` for all but each
/// fifth). NO index is declared.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut sm = BTreeMap::new();
    sm.insert("id".into(), s("suite-1"));
    let suite = g.create_node(&["Suite".into()], &sm).expect("suite");
    let mut cases = Vec::new();
    for t in 0..200i64 {
        let mut m = BTreeMap::new();
        m.insert("id".into(), s(&format!("tc-{t}")));
        m.insert("title".into(), s(&format!("Test case {t}")));
        m.insert("orgId".into(), s(if t % 5 == 0 { "other" } else { "default" }));
        let tc = g.create_node(&["TestCase".into()], &m).expect("tc");
        g.create_rel(tc, "BELONGS_TO", suite, &BTreeMap::new()).expect("belongs");
        cases.push(tc);
    }
    for i in 0..2000i64 {
        let proposal = i % 100;
        let mut m = BTreeMap::new();
        m.insert("id".into(), s(&format!("ac-{i}")));
        m.insert("proposalId".into(), s(&format!("repo:proposal-{proposal}")));
        // Moduli independent of the proposal index (i = proposal + 100k):
        // `i % 4` would give every criterion of one proposal the same org.
        let k = i / 100;
        m.insert("orgId".into(), s(if k % 4 == 3 { "other" } else { "default" }));
        m.insert("text".into(), s(&format!("Criterion {i}")));
        let id = g.create_node(&["AcceptanceCriterion".into()], &m).expect("ac");
        if proposal < 50 {
            let mut rm = BTreeMap::new();
            rm.insert("verifiedAt".into(), s(&format!("2026-09-{:02}T00:00:00Z", 1 + (i % 28))));
            let case = ((k * 13 + proposal) % 200) as usize;
            g.create_rel(id, "VERIFIED_BY", cases[case], &rm).expect("verified");
        }
    }
    g
}

/// The production shape's parameter guards fold away since fix 69 (the
/// statement then runs the pipeline); an EXISTS conjunct keeps this one
/// on the general path, where the seed column filter is the lever. Every
/// test case belongs to the one suite, so the EXISTS changes no answer.
const ORIG: &str = "MATCH (ac:AcceptanceCriterion {proposalId: $proposalId})-[v:VERIFIED_BY]->(tc:TestCase) \
    WHERE ac.orgId = $viewerOrgId AND tc.orgId = $viewerOrgId AND EXISTS { (tc)-[:BELONGS_TO]->(:Suite) } \
    RETURN ac.id AS criterionId, properties(tc) AS tc, properties(v) AS v";

fn sorted(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort_by_key(|r| format!("{r:?}"));
    rows
}

#[test]
fn a_the_whole_label_filter_runs_column_at_a_time_once_its_columns_are_cached() {
    let g = corpus();
    // A proposal nobody has: the corpus case, 0 rows.
    assert_eq!(general(&g, ORIG, "repo:nobody"), Vec::<Vec<Value>>::new());
    // The first run walks the label per member and KEEPS the columns.
    let (got, c) = traced(&g, ORIG, "repo:nobody");
    assert!(got.is_empty());
    assert_eq!(count_of(&c, FILTERED), 1, "{c:?}");
    let walked = count_of(&c, EXPRESSIONS);
    assert!(walked >= 2000, "the first run evaluates per member: {c:?}");
    // The second run reads the cached columns as vectors.
    let (got, c) = traced(&g, ORIG, "repo:nobody");
    assert!(got.is_empty());
    assert_eq!(count_of(&c, VECTORISED), 1, "{c:?}");
    assert_eq!(count_of(&c, FILTERED), 1, "{c:?}");
    assert!(
        count_of(&c, EXPRESSIONS) < 64,
        "no expression per member once the columns are cached: {c:?}"
    );
}

/// A proposal WITH criteria: byte-identical rows, the vectorised filter
/// keeps exactly the criteria the per-member walk kept.
#[test]
fn b_the_survivors_are_the_walks() {
    let g = corpus();
    let want = sorted(general(&g, ORIG, "repo:proposal-7"));
    // 20 criteria, 15 of them `default`, their test cases `default` for 4 of 5.
    assert!(want.len() >= 10 && want.len() <= 15, "{}", want.len());
    let _ = rows(&g, ORIG, "repo:proposal-7");
    let (got, c) = traced(&g, ORIG, "repo:proposal-7");
    assert_eq!(sorted(got), want);
    assert_eq!(count_of(&c, VECTORISED), 1, "{c:?}");
    // The hop ends and their WHERE still evaluate per row; the seed does not.
    assert!(count_of(&c, EXPRESSIONS) < 200, "{c:?}");
}

/// A predicate the vectoriser declines keeps the per-member walk and the
/// same answer.
#[test]
fn c_an_opaque_predicate_keeps_the_walk() {
    let g = corpus();
    let opaque = "MATCH (ac:AcceptanceCriterion) WHERE ac.proposalId = $proposalId AND size([x IN [1,2,3] WHERE x > toInteger(ac.orgId IS NULL)]) > 1 \
        RETURN count(ac) AS n";
    let want = general(&g, opaque, "repo:proposal-7");
    let _ = rows(&g, opaque, "repo:proposal-7");
    let (got, c) = traced(&g, opaque, "repo:proposal-7");
    assert_eq!(got, want);
    assert_eq!(count_of(&c, VECTORISED), 0, "{c:?}");
}
