//! An expand whose new var the NEXT hop closes from, onto ONE node every live
//! row shares, binds only the peers that can close — and answers exactly as
//! the per-tuple path does, rows and order.
//!
//! SNB BI bi18 is the shape: `(tag)<-[:HAS_INTEREST]-(person1)-[:KNOWS]-
//! (mutualFriend)-[:KNOWS]-(person2)-[:HAS_INTEREST]->(tag)`. The second KNOWS
//! expand built a row for every friend of every friend (2.5M at SF3, each with
//! its own isomorphism set) and the close onto `tag` then dropped nearly all of
//! them, one adjacency binary search per row.
//!
//! The contract is `pipeline_semijoin`'s: `set_columnar_scans(true)` must equal
//! `set_columnar_scans(false)`, the row set AND its order.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn run(g: &Graph, q: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    let (rows, t) = engram_observe::with_trace(|| {
        run_query(g, &s, BTreeMap::new())
            .unwrap_or_else(|e| panic!("run `{q}`: {e}"))
            .rows
    });
    (rows, t.counters().iter().map(|(k, v)| (k.clone(), *v)).collect())
}

fn control(g: &Graph, q: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let (rows, _) = run(g, q);
    g.set_columnar_scans(true);
    rows
}

fn counter(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

const FILTERED: &str = "interp.pipeline expand bound only the peers the next hop closes from";

/// Three tags, 200 people who each know three others (a ring with two chords,
/// both directions stored once), each interested in one tag by id — and every
/// fifth person in `t0` as well; person 3 is interested in `t0` TWICE (a
/// parallel edge the close must multiply by).
fn people() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 2) AS t CREATE (:Tag {name: 't' + toString(t)})");
    ddl(&g, "UNWIND range(0, 199) AS i CREATE (:Person {id: i})");
    ddl(
        &g,
        "MATCH (a:Person), (b:Person) \
         WHERE b.id = (a.id + 1) % 200 OR b.id = (a.id + 7) % 200 OR b.id = (a.id + 31) % 200 \
         CREATE (a)-[:KNOWS]->(b)",
    );
    ddl(
        &g,
        "MATCH (p:Person), (t:Tag) WHERE t.name = 't' + toString(p.id % 3) \
         CREATE (p)-[:HAS_INTEREST]->(t)",
    );
    ddl(
        &g,
        "MATCH (p:Person), (t:Tag {name: 't0'}) WHERE p.id % 5 = 0 AND p.id % 3 <> 0 \
         CREATE (p)-[:HAS_INTEREST]->(t)",
    );
    ddl(
        &g,
        "MATCH (p:Person {id: 3}), (t:Tag {name: 't0'}) CREATE (p)-[:HAS_INTEREST]->(t)",
    );
    let _ = g.warm();
    g
}

/// SNB BI bi18, as the catalogue spells it.
fn bi18(tag: &str) -> String {
    format!(
        "MATCH (tag:Tag {{name: '{tag}'}})<-[:HAS_INTEREST]-(person1:Person)-[:KNOWS]-(mutualFriend:Person)\
         -[:KNOWS]-(person2:Person)-[:HAS_INTEREST]->(tag) \
         WHERE person1 <> person2 AND NOT (person1)-[:KNOWS]-(person2) \
         RETURN person1.id AS person1Id, person2.id AS person2Id, \
                count(DISTINCT mutualFriend) AS mutualFriendCount \
         ORDER BY mutualFriendCount DESC, person1Id ASC, person2Id ASC LIMIT 20"
    )
}

#[test]
fn bi18_binds_only_the_people_interested_in_its_tag() {
    let g = people();
    for tag in ["t0", "t1", "t2"] {
        let q = bi18(tag);
        let want = control(&g, &q);
        let (got, c) = run(&g, &q);
        assert!(!want.is_empty(), "vacuous: {q}");
        assert_eq!(got, want, "\n  {q}");
        assert!(counter(&c, FILTERED) > 0, "the expand was not filtered: {q}\n{c:?}");
    }
}

#[test]
fn every_neighbouring_close_answers_as_the_per_tuple_path_does() {
    let g = people();
    for q in [
        // rows in production order, no aggregate: the order must survive the filter
        "MATCH (t:Tag {name: 't2'})<-[:HAS_INTEREST]-(p1:Person)-[:KNOWS]-(m:Person)\
         -[:KNOWS]-(p2:Person)-[:HAS_INTEREST]->(t) RETURN p1.id, m.id, p2.id",
        // the doubled interest: person 3's rows multiply
        "MATCH (t:Tag {name: 't0'})<-[:HAS_INTEREST]-(p1:Person)-[:KNOWS]-(p2:Person)\
         -[:HAS_INTEREST]->(t) RETURN p1.id, p2.id",
        // an undirected close
        "MATCH (t:Tag {name: 't1'})<-[:HAS_INTEREST]-(p1:Person)-[:KNOWS]->(m:Person)\
         -[:KNOWS]->(p2:Person)-[:HAS_INTEREST]-(t) RETURN p1.id, m.id, p2.id",
        // a close onto a target that differs per row: a triangle, no filter
        "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)-[:KNOWS]->(a) \
         RETURN a.id, b.id, c.id",
        // a close whose expand is the path's first hop from the constant
        "MATCH (t:Tag {name: 't0'})<-[:HAS_INTEREST]-(p:Person)-[:HAS_INTEREST]->(t) RETURN p.id",
        // a type nothing ever minted on the close: no row
        "MATCH (t:Tag {name: 't0'})<-[:HAS_INTEREST]-(p1:Person)-[:KNOWS]-(p2:Person)\
         -[:NEVER_MINTED]->(t) RETURN count(*) AS n",
    ] {
        let want = control(&g, q);
        let (got, _) = run(&g, q);
        assert_eq!(got, want, "\n  {q}");
    }
}
