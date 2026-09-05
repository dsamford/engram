#![allow(non_snake_case)]
//! Fix 44: a label test on a walked population that is NOT one of the
//! population's own labels — `(a:WebSource OR a:EmailSource)` over the
//! KnowledgeArticles — is answered from the label's membership snapshot,
//! like the population's own labels always were, instead of a walk of the
//! LABEL-SET column over the population's id span. The mirror's ids
//! interleave labels, so 55 articles spanned millions of ids and the walk
//! was budget-bound every statement (2.1–3.1 ms against Neo4j's 0.7).
//!
//! Every answer is checked against the general path (columnar paths off).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const MEMBERSHIP: &str = "interp.columnar label test answered from membership";

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
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

/// 60 knowledge articles, each followed by 40 filler nodes so the label's
/// id span is 40× its size; half are also :WebSource, a quarter
/// :EmailSource, the rest neither; abuse statuses cycle.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let statuses = ["quarantined", "approved", "rejected", "clean"];
    for i in 0..60i64 {
        let mut labels = vec!["KnowledgeArticle".to_string()];
        if i % 2 == 0 {
            labels.push("WebSource".into());
        } else if i % 4 == 1 {
            labels.push("EmailSource".into());
        }
        let mut m = BTreeMap::new();
        m.insert("articleId".to_string(), Value::Str(format!("ka-{i:03}")));
        m.insert("abuseStatus".to_string(), Value::Str(statuses[(i % 4) as usize].into()));
        m.insert(
            "abuseStatusUpdatedAt".to_string(),
            Value::Str(format!("2026-08-{:02}T00:00:00Z", 1 + (i % 28))),
        );
        m.insert("kind".to_string(), Value::Str(if i % 3 == 0 { "guide".into() } else { "note".into() }));
        g.create_node(&labels, &m).expect("article");
        for k in 0..40i64 {
            let mut f = BTreeMap::new();
            f.insert("n".to_string(), Value::Int(i * 40 + k));
            g.create_node(&["Filler".into()], &f).expect("filler");
        }
    }
    g
}

fn check(g: &Graph, src: &str) -> BTreeMap<String, u64> {
    let want = general(g, src);
    let first = rows(g, src);
    assert_eq!(first, want, "first run `{src}`");
    let (got, c) = traced(g, src);
    assert_eq!(got, want, "second run `{src}`");
    c
}

#[test]
fn the_label_disjunction_count_reads_memberships() {
    let g = corpus();
    let src = "MATCH (a:KnowledgeArticle) WHERE a.abuseStatus IN ['quarantined','approved','rejected'] AND (a:WebSource OR a:EmailSource) RETURN count(a) AS n";
    let c = check(&g, src);
    assert!(count_of(&c, MEMBERSHIP) > 0, "{c:?}");
}

#[test]
fn the_quarantined_min_the_grouped_kind_and_a_projection_read_memberships() {
    let g = corpus();
    for src in [
        "MATCH (a:KnowledgeArticle) WHERE a.abuseStatus = 'quarantined' AND (a:WebSource OR a:EmailSource) RETURN min(a.abuseStatusUpdatedAt) AS oldest",
        "MATCH (a:KnowledgeArticle) WHERE a.abuseStatus IN ['quarantined','approved'] AND (a:WebSource OR a:EmailSource) RETURN a.kind AS kind, count(*) AS n ORDER BY kind",
        "MATCH (a:KnowledgeArticle) WHERE a:WebSource AND NOT a:EmailSource RETURN a.articleId AS id ORDER BY id LIMIT 12",
        "MATCH (a:KnowledgeArticle) WHERE NOT (a:WebSource OR a:EmailSource) RETURN count(a) AS n",
        "MATCH (a:KnowledgeArticle) WHERE a:NeverMinted RETURN count(a) AS n",
    ] {
        let c = check(&g, src);
        assert!(count_of(&c, MEMBERSHIP) > 0, "`{src}`: {c:?}");
    }
}

/// CONTROL: a label test on the population's OWN label reads the same
/// membership, and a statement with no label test never touches one.
#[test]
fn a_statement_without_a_label_test_reads_no_membership_for_it() {
    let g = corpus();
    let src = "MATCH (a:KnowledgeArticle) WHERE a.abuseStatus IN ['quarantined','approved','rejected'] RETURN count(a) AS n";
    let c = check(&g, src);
    assert_eq!(count_of(&c, MEMBERSHIP), 0, "{c:?}");
}
