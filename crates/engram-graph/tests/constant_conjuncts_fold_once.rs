#![allow(non_snake_case)]
//! Fix 69: a WHERE conjunct that reads no variable — `$viewerOrgId IS NOT
//! NULL`, the parameter guard the production visibility statements wrap
//! around every equality — is a per-statement constant. It used to reach
//! every recogniser as an ordinary conjunct: the pipeline declined the
//! AcceptanceCriterion listing on its two guards alone and the general
//! path filtered 4,229 members per member (3.0 ms on the mirror) where the
//! unguarded spelling ran the pipeline in 0.14. Folded once, the guarded
//! statement takes the path the unguarded one takes.
//!
//! Every answer is checked against the same statement with the columnar
//! paths OFF and against its unguarded spelling.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params(proposal: &str, org: Option<&str>) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("proposalId".to_string(), Value::Str(proposal.into()));
    p.insert(
        "viewerOrgId".to_string(),
        org.map(|o| Value::Str(o.into())).unwrap_or(Value::Null),
    );
    p
}

fn run(g: &Graph, src: &str, p: &BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, p.clone())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str, p: &BTreeMap<String, Value>) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| run(g, src, p));
    (r, trace.counters().clone())
}

fn general(g: &Graph, src: &str, p: &BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let r = run(g, src, p);
    g.set_columnar_scans(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn sorted(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort_by_key(|r| format!("{r:?}"));
    rows
}

const FOLDED: &str = "interp.constant conjunct folded";
const EXPRESSIONS: &str = "cypher.expressions evaluated";

/// 2,000 acceptance criteria over 100 proposals, 200 test cases, every
/// criterion of the first 50 proposals VERIFIED_BY one of them; no index.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut cases = Vec::new();
    for t in 0..200i64 {
        let mut m = BTreeMap::new();
        m.insert("id".into(), s(&format!("tc-{t}")));
        m.insert("orgId".into(), s(if t % 5 == 0 { "other" } else { "default" }));
        cases.push(g.create_node(&["TestCase".into()], &m).expect("tc"));
    }
    for i in 0..2000i64 {
        let proposal = i % 100;
        let k = i / 100;
        let mut m = BTreeMap::new();
        m.insert("id".into(), s(&format!("ac-{i}")));
        m.insert("proposalId".into(), s(&format!("repo:proposal-{proposal}")));
        m.insert("orgId".into(), s(if k % 4 == 3 { "other" } else { "default" }));
        let id = g.create_node(&["AcceptanceCriterion".into()], &m).expect("ac");
        if proposal < 50 {
            let case = ((k * 13 + proposal) % 200) as usize;
            g.create_rel(id, "VERIFIED_BY", cases[case], &BTreeMap::new()).expect("verified");
        }
    }
    g
}

const GUARDED: &str = "MATCH (ac:AcceptanceCriterion {proposalId: $proposalId})-[v:VERIFIED_BY]->(tc:TestCase) \
    WHERE ($viewerOrgId IS NOT NULL AND ac.orgId = $viewerOrgId) AND ($viewerOrgId IS NOT NULL AND tc.orgId = $viewerOrgId) \
    RETURN ac.id AS criterionId, tc.id AS caseId";

const UNGUARDED: &str = "MATCH (ac:AcceptanceCriterion {proposalId: $proposalId})-[v:VERIFIED_BY]->(tc:TestCase) \
    WHERE ac.orgId = $viewerOrgId AND tc.orgId = $viewerOrgId \
    RETURN ac.id AS criterionId, tc.id AS caseId";

#[test]
fn a_the_guards_fold_and_the_guarded_statement_takes_the_unguarded_path() {
    let g = corpus();
    let p = params("repo:proposal-7", Some("default"));
    let want = sorted(general(&g, GUARDED, &p));
    assert!(want.len() >= 10 && want.len() <= 15, "{}", want.len());
    assert_eq!(sorted(run(&g, UNGUARDED, &p)), want, "the guards are true: same answer");
    let _ = run(&g, GUARDED, &p);
    let (got, c) = traced(&g, GUARDED, &p);
    assert_eq!(sorted(got), want);
    assert_eq!(count_of(&c, FOLDED), 1, "one clause folded: {c:?}");
    // The paths agree: whatever the unguarded spelling runs, the guarded one
    // runs too — and the per-member seed filter (2,000 expressions) is gone.
    let (_, cu) = traced(&g, UNGUARDED, &p);
    for key in ["interp.pipeline hop runs", "interp.streamed a read-only chain", "interp.seeds filtered by columns"] {
        assert_eq!(count_of(&c, key), count_of(&cu, key), "{key}: guarded {c:?} vs unguarded {cu:?}");
    }
    assert!(count_of(&c, EXPRESSIONS) < 500, "{c:?}");
}

/// A null parameter makes the guard False: the WHERE is `false` and the
/// statement answers nothing without a scan; a proposal nobody has is the
/// corpus case, 0 rows the same way.
#[test]
fn b_a_null_guard_empties_the_statement_without_a_scan() {
    let g = corpus();
    let p = params("repo:proposal-7", None);
    assert_eq!(general(&g, GUARDED, &p), Vec::<Vec<Value>>::new());
    let (got, c) = traced(&g, GUARDED, &p);
    assert!(got.is_empty());
    assert_eq!(count_of(&c, FOLDED), 1, "{c:?}");
    assert!(count_of(&c, EXPRESSIONS) < 50, "no per-member evaluation: {c:?}");

    let p = params("repo:nobody", Some("default"));
    assert_eq!(general(&g, GUARDED, &p), Vec::<Vec<Value>>::new());
    let _ = run(&g, GUARDED, &p);
    let (got, c) = traced(&g, GUARDED, &p);
    assert!(got.is_empty());
    assert_eq!(count_of(&c, FOLDED), 1, "{c:?}");
}

/// A WITH's WHERE folds the same way; a non-boolean constant conjunct is
/// left as written and still raises; a var-free conjunct holding a
/// subquery is not folded.
#[test]
fn c_with_where_folds_and_a_non_boolean_constant_still_raises() {
    let g = corpus();
    let p = params("repo:proposal-7", Some("default"));
    let with_form = "MATCH (ac:AcceptanceCriterion {proposalId: $proposalId}) WITH ac WHERE $viewerOrgId IS NOT NULL RETURN count(ac) AS n";
    assert_eq!(run(&g, with_form, &p), vec![vec![Value::Int(20)]]);
    let (got, c) = traced(&g, with_form, &p);
    assert_eq!(got, vec![vec![Value::Int(20)]]);
    assert_eq!(count_of(&c, FOLDED), 1, "{c:?}");

    let non_boolean = "MATCH (ac:AcceptanceCriterion {proposalId: $proposalId}) WHERE 1 AND ac.orgId = $viewerOrgId RETURN count(ac) AS n";
    let q = parse_statement(non_boolean).unwrap();
    assert!(run_query(&g, &q, p.clone()).is_err(), "a number is not a predicate, folded or not");

    let subquery = "MATCH (ac:AcceptanceCriterion {proposalId: $proposalId}) WHERE EXISTS { MATCH (:TestCase {id: 'tc-1'}) } AND ac.orgId = $viewerOrgId RETURN count(ac) AS n";
    let want = general(&g, subquery, &p);
    let (got, c) = traced(&g, subquery, &p);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, FOLDED), 0, "a subquery is not folded: {c:?}");
}
