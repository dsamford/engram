//! WITH's sub-clauses AFTER its WHERE (`WITH v WHERE v < 4 ORDER BY v DESC LIMIT 2`)
//! -- the Neo4j-accepted order the platform's story-tracker query uses, and the
//! first read the shadow instrument ever refused with a parse error.
//!
//! Every expected answer below was taken from Neo4j 5.26.27 on 2026-09-04 with the
//! identical statement. The two orders are NOT the same query: the canonical
//! `ORDER BY … LIMIT … WHERE` limits first and filters the survivors; this form
//! filters first and orders/limits what is left.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ints(src: &str) -> Vec<i64> {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let r = run_query(&g, &q, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e}"));
    r.rows
        .iter()
        .map(|row| match &row[0] {
            Value::Int(i) => *i,
            other => panic!("`{src}`: expected an int, got {other:?}"),
        })
        .collect()
}

#[test]
fn where_before_order_by_limit_filters_first() {
    // Neo4j: v 3 2
    assert_eq!(
        ints("UNWIND [1,2,3,4,5,6] AS v WITH v WHERE v < 4 ORDER BY v DESC LIMIT 2 RETURN v"),
        vec![3, 2]
    );
}

#[test]
fn the_canonical_order_limits_first_and_is_a_different_query() {
    // Neo4j: (no rows) -- the top two are 6 and 5, and neither is < 4.
    assert_eq!(
        ints("UNWIND [1,2,3,4,5,6] AS v WITH v ORDER BY v DESC LIMIT 2 WHERE v < 4 RETURN v"),
        Vec::<i64>::new()
    );
}

#[test]
fn where_before_skip_limit_without_order_by() {
    // Neo4j: v 2
    assert_eq!(
        ints("UNWIND [1,2,3,4,5,6] AS v WITH v WHERE v < 4 SKIP 1 LIMIT 1 RETURN v"),
        vec![2]
    );
}

#[test]
fn where_before_order_by_skip_limit() {
    // Neo4j: v 5
    assert_eq!(
        ints("UNWIND [1,2,3,4,5,6] AS v WITH v WHERE v > 3 ORDER BY v DESC SKIP 1 LIMIT 1 RETURN v"),
        vec![5]
    );
}

#[test]
fn where_on_an_aggregate_then_order_by_the_key() {
    // Neo4j: k, c -> 1, 3  (odd numbers: three of them; evens also three; DESC key wins)
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let src = "UNWIND [1,2,3,4,5,6] AS v WITH v % 2 AS k, count(*) AS c WHERE c > 2 \
               ORDER BY k DESC LIMIT 1 RETURN k, c";
    let q = parse_statement(src).unwrap();
    let r = run_query(&g, &q, BTreeMap::new()).unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(1), Value::Int(3)]]);
}

#[test]
fn distinct_then_where_then_order_by() {
    // Neo4j: v 3 2
    assert_eq!(
        ints("UNWIND [3,1,2,3] AS v WITH DISTINCT v WHERE v > 1 ORDER BY v DESC LIMIT 5 RETURN v"),
        vec![3, 2]
    );
}

#[test]
fn the_story_tracker_shape_runs_end_to_end() {
    // The production statement's shape: an aggregate over a pattern, a WHERE on
    // it, then ORDER BY / LIMIT, then RETURN of the carried variable's properties.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let setup = "CREATE (a:Article {id: 'a'}), (e1:Entity), (e2:Entity), (e3:Entity), \
                 (x1:Article {id: 'x1'}), (x2:Article {id: 'x2'}), \
                 (s1:Story {id: 's1', status: 'live'}), (s2:Story {id: 's2', status: 'live'}), \
                 (a)-[:MENTIONS]->(e1), (a)-[:MENTIONS]->(e2), (a)-[:MENTIONS]->(e3), \
                 (x1)-[:MENTIONS]->(e1), (x1)-[:PART_OF]->(s1), \
                 (x2)-[:MENTIONS]->(e1), (x2)-[:MENTIONS]->(e2), (x2)-[:PART_OF]->(s2)";
    run_query(&g, &parse_statement(setup).unwrap(), BTreeMap::new()).unwrap();
    let src = "MATCH (a:Article {id: 'a'})-[:MENTIONS]->(e:Entity)<-[:MENTIONS]-(x:Article)-[:PART_OF]->(s:Story) \
               WHERE s.status <> 'stale' \
               WITH s, count(DISTINCT e) AS shared WHERE shared >= 1 \
               ORDER BY shared DESC LIMIT 5 \
               RETURN s.id AS id, shared";
    let r = run_query(&g, &parse_statement(src).unwrap(), BTreeMap::new()).unwrap();
    assert_eq!(r.columns, vec!["id", "shared"]);
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Str("s2".into()), Value::Int(2)],
            vec![Value::Str("s1".into()), Value::Int(1)],
        ]
    );
}

#[test]
fn order_by_after_the_where_sees_only_the_projected_names() {
    // Neo4j: Variable `v` not defined. The canonical order may still sort by a
    // pre-projection variable; this order may not.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let q = parse_statement("UNWIND [3,1,2] AS v WITH v AS w WHERE w > 1 ORDER BY v RETURN w").unwrap();
    assert!(run_query(&g, &q, BTreeMap::new()).is_err());
    assert_eq!(
        ints("UNWIND [3,1,2] AS v WITH v AS w ORDER BY v WHERE w > 1 RETURN w"),
        vec![2, 3]
    );
}

#[test]
fn the_story_tracker_shape_over_zero_rows_returns_no_rows_and_no_error() {
    // The common production case: no story matches, the aggregating WITH yields
    // zero rows, and the desugared `WITH *` has nothing to expand. It must be
    // an empty result, not "a projection needs at least one item" (the
    // streaming projector refused an empty star where `project()` did not;
    // the first musl build of this fix failed exactly here over Bolt).
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let src = "MATCH (a:Article {id: 'nope'})-[:MENTIONS]->(e:Entity)<-[:MENTIONS]-(x:Article)-[:PART_OF]->(s:Story) \
               WHERE s.status <> 'stale' AND s.updatedAt > $cutoff \
               WITH s, count(DISTINCT e) AS shared WHERE shared >= 1 \
               ORDER BY shared DESC LIMIT 5 \
               RETURN s.id AS id, s.title AS title";
    let mut params = BTreeMap::new();
    params.insert("cutoff".to_string(), Value::Str("2026".into()));
    let r = run_query(&g, &parse_statement(src).unwrap(), params).unwrap();
    assert!(r.rows.is_empty());
    // And a plain `RETURN *` over zero rows is fine too.
    let r = run_query(&g, &parse_statement("MATCH (n:Nothing) WITH n RETURN *").unwrap(), BTreeMap::new()).unwrap();
    assert!(r.rows.is_empty());
}
