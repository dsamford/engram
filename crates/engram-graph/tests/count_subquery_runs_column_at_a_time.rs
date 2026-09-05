#![allow(non_snake_case)]
//! Fix 70: a `COUNT { … }` / `EXISTS { … }` body that is one typed hop from
//! a bound node to a labelled unbound end, its WHERE over that end's
//! properties, is evaluated column-at-a-time from the label's cached
//! columns instead of a scope bind and an expression walk per neighbour.
//! The production KMProject dashboard evaluates eight status `COUNT {}`s
//! per project row over ~15k items: one such count cost 9–16 ms on the
//! mirror (about a microsecond per item, Neo4j 0.07) and the eight 75 ms
//! of the statement's 149 (Neo4j 22).
//!
//! Every answer is checked against the same statement with the columnar
//! paths OFF (the per-row matcher).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

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

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

const VECTORISED: &str = "interp.subquery hop evaluated column-at-a-time";
const EXPRESSIONS: &str = "cypher.expressions evaluated";

/// 20 projects × 200 items (4,000 Item); statuses cycle over five values
/// with every seventh item's status ABSENT (the `coalesce` case); the
/// items of even projects are `kind: 'task'`, odd projects `'epic'`.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for p in 0..20i64 {
        let mut pm = BTreeMap::new();
        pm.insert("id".into(), s(&format!("proj-{p:02}")));
        pm.insert("kind".into(), s(if p % 2 == 0 { "task" } else { "epic" }));
        let pn = g.create_node(&["Proj".into()], &pm).expect("p");
        for i in 0..200i64 {
            let mut m = BTreeMap::new();
            m.insert("id".into(), s(&format!("item-{p}-{i}")));
            if (p + i) % 7 != 0 {
                m.insert(
                    "status".into(),
                    s(["backlog", "todo", "in_progress", "done", "cancelled"][((p + i) % 5) as usize]),
                );
            }
            m.insert("kind".into(), s(if p % 2 == 0 { "task" } else { "epic" }));
            m.insert("points".into(), Value::Int((i % 8) + 1));
            let w = g.create_node(&["Item".into()], &m).expect("w");
            g.create_rel(w, "BELONGS_TO", pn, &BTreeMap::new()).expect("belongs");
        }
    }
    // The columns the counts read, cached by a whole-label aggregate.
    let _ = rows(&g, "MATCH (w:Item) RETURN count(w.status) AS a, sum(w.points) AS b, count(w.kind) AS c");
    let _ = rows(&g, "MATCH (p:Proj) RETURN count(p.kind) AS c");
    g
}

const DASH: &str = "MATCH (p:Proj) \
    RETURN p.id AS id, \
    COUNT { (:Item)-[:BELONGS_TO]->(p) } AS total, \
    COUNT { (wi:Item)-[:BELONGS_TO]->(p) WHERE NOT wi.status IN ['done','cancelled'] } AS open, \
    COUNT { (b:Item)-[:BELONGS_TO]->(p) WHERE coalesce(b.status, 'backlog') = 'backlog' } AS backlog, \
    COUNT { (d:Item)-[:BELONGS_TO]->(p) WHERE coalesce(d.status, 'backlog') = 'done' } AS done, \
    COUNT { (x:Item)-[:BELONGS_TO]->(p) WHERE x.status IS NULL AND x.points > 4 } AS unset_big \
    ORDER BY id";

#[test]
fn a_the_dashboards_counts_run_over_cached_columns() {
    let g = corpus();
    let want = general(&g, DASH);
    assert_eq!(want.len(), 20);
    // proj-00: statuses (0+i)%5 with i%7==0 absent → backlog = i%5==0 minus
    // absent + the absent ones (coalesce) …: just check a few structural facts.
    assert_eq!(want[0][1], Value::Int(200), "total");
    let (got, c) = traced(&g, DASH);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, VECTORISED), 20 * 5, "five counts per project row: {c:?}");
    assert!(
        count_of(&c, EXPRESSIONS) < 400,
        "no expression per neighbour (4,000 items × 4 predicates would be 16k): {c:?}"
    );
}

/// EXISTS stops at the first hit; a count whose end is the OUTBOUND side of
/// the hop (`(p)-[:T]->(x)`) works the same; a WHERE reading the outer row
/// declines to the matcher and still agrees.
#[test]
fn b_exists_the_outbound_form_and_a_declined_where_agree() {
    let g = corpus();
    let exists = "MATCH (p:Proj) WHERE EXISTS { (w:Item)-[:BELONGS_TO]->(p) WHERE w.status = 'blocked' } RETURN count(p) AS n";
    assert_eq!(general(&g, exists), vec![vec![Value::Int(0)]]);
    let (got, c) = traced(&g, exists);
    assert_eq!(got, vec![vec![Value::Int(0)]]);
    assert_eq!(count_of(&c, VECTORISED), 20, "{c:?}");

    let exists_hit = "MATCH (p:Proj) WHERE EXISTS { (w:Item)-[:BELONGS_TO]->(p) WHERE w.status = 'done' AND w.points = 8 } RETURN count(p) AS n";
    let want = general(&g, exists_hit);
    let (got, c) = traced(&g, exists_hit);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, VECTORISED), 20, "{c:?}");

    // The same relationship, walked from the item side: `(w)-[:BELONGS_TO]->(p)`
    // with w bound and p the unbound labelled far end.
    let outbound = "MATCH (w:Item {id: 'item-3-17'}) RETURN COUNT { (w)-[:BELONGS_TO]->(q:Proj) WHERE q.kind = 'epic' } AS n";
    assert_eq!(general(&g, outbound), vec![vec![Value::Int(1)]]);
    let (got, c) = traced(&g, outbound);
    assert_eq!(got, vec![vec![Value::Int(1)]]);
    assert_eq!(count_of(&c, VECTORISED), 1, "{c:?}");

    let correlated = "MATCH (p:Proj) RETURN p.id AS id, COUNT { (w:Item)-[:BELONGS_TO]->(p) WHERE w.kind = p.kind } AS same ORDER BY id";
    let want = general(&g, correlated);
    assert!(want.iter().all(|r| r[1] == Value::Int(200)));
    let (got, c) = traced(&g, correlated);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, VECTORISED), 0, "a WHERE reading the outer row keeps the matcher: {c:?}");
}
