//! SNB BI bi15's weights, computed from the REPLY edges rather than from the
//! KNOWS pairs, answer exactly as the pair-centric text does.
//!
//! The pair-centric text walks, for EVERY KNOWS pair, all of one person's
//! messages and their replies looking for the other person — a person's
//! messages are re-walked once per friend. PostgreSQL's reference SQL does the
//! opposite: one pass over reply pairs, aggregated by the two creators, then a
//! join with KNOWS. At SF3 that is 4.6 s against engram's 190 s for the
//! pair-centric text, and the difference is the formulation, not the engine.
//!
//! The edge-centric text aggregates each interacting pair's weight ONCE (by
//! the unordered pair of creators, so both directions of a reply land in one
//! sum), and gives the projection every KNOWS pair at weight 1.0 AND each
//! interacting pair at 1 / (w + 1). A pair that interacts is therefore two
//! parallel edges, and a k = 1 shortest path takes the cheaper of them —
//! 1 / (w + 1) is never above 1.0 — which is exactly the one weight the
//! pair-centric text gives it. That argument is what this test checks: the
//! same cost, to the bit, for pairs, windows that admit every forum, some, or
//! none, and a pair with no route.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn cost(g: &Graph, q: &str, p1: i64, p2: i64, from: i64, to: i64) -> Value {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse: {e}"));
    let mut params = BTreeMap::new();
    params.insert("person1Id".to_string(), Value::Int(p1));
    params.insert("person2Id".to_string(), Value::Int(p2));
    params.insert("startDate".to_string(), Value::Int(from));
    params.insert("endDate".to_string(), Value::Int(to));
    let r = run_query(g, &s, params).unwrap_or_else(|e| panic!("run: {e}"));
    assert_eq!(r.rows.len(), 1, "bi15 answers one row: {r:?}");
    r.rows[0][0].clone()
}

/// 40 people who each KNOW the next one and the fifth one along, and one (40)
/// who knows nobody; eight forums created at 0, 100, …, 700; a post per person
/// per two forums; and three generations of comments, each replying to the
/// generation before, written by the replied-to creator's neighbour +1, +5
/// (both KNOWS pairs) or +13 (not one, so the KNOWS filter bites).
fn threads() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 40) AS i CREATE (:Person {id: i})");
    ddl(
        &g,
        "UNWIND range(0, 39) AS i UNWIND [1, 5] AS d \
         MATCH (a:Person {id: i}), (b:Person {id: (i + d) % 40}) CREATE (a)-[:KNOWS]->(b)",
    );
    ddl(&g, "UNWIND range(0, 7) AS f CREATE (:Forum {id: f, creationDate: 100 * f})");
    ddl(
        &g,
        "UNWIND range(0, 79) AS k MATCH (p:Person {id: k % 40}), (f:Forum {id: k % 8}) \
         CREATE (f)-[:CONTAINER_OF]->(:Message:Post {id: 1000 + k})-[:HAS_CREATOR]->(p)",
    );
    for (lo, hi, back) in [(0, 79, -1000), (80, 159, 80), (160, 239, 80)] {
        // generation 0 replies to the posts (1000 + k), later ones to the
        // comment 80 before them (2000 + k - 80)
        let target = if back < 0 { "1000 + k".to_string() } else { format!("2000 + k - {back}") };
        ddl(
            &g,
            &format!(
                "UNWIND range({lo}, {hi}) AS k \
                 MATCH (t:Message {{id: {target}}})-[:HAS_CREATOR]->(tc:Person) \
                 MATCH (c:Person {{id: (tc.id + [1, 5, 1, 5, 13][k % 5]) % 40}}) \
                 CREATE (c)<-[:HAS_CREATOR]-(:Message:Comment {{id: 2000 + k}})-[:REPLY_OF]->(t)"
            ),
        );
    }
    let _ = g.warm();
    g
}

const PAIR_CENTRIC: &str = "MATCH (pA:Person)-[:KNOWS]-(pB:Person) \
    WHERE id(pA) < id(pB) \
    OPTIONAL MATCH (pA)<-[:HAS_CREATOR]-(m1:Message)-[:REPLY_OF]-(m2:Message)-[:HAS_CREATOR]->(pB) \
    OPTIONAL MATCH (m1)-[:REPLY_OF*0..]->(:Post)<-[:CONTAINER_OF]-(forum:Forum) \
      WHERE forum.creationDate >= $startDate AND forum.creationDate <= $endDate \
    WITH pA, pB, \
         sum(CASE WHEN forum IS NOT NULL \
                  THEN (CASE WHEN (m1:Post OR m2:Post) THEN 1.0 ELSE 0.5 END) \
                  ELSE 0.0 END) AS w \
    WITH collect({source: id(pA), target: id(pB), weight: 1.0 / (w + 1.0)}) AS edges \
    CALL engram.algo.project({name: 'q15', nodeLabels: ['Person'], edges: edges, orientation: 'UNDIRECTED'}) \
    YIELD projection \
    MATCH (person1:Person {id: $person1Id}), (person2:Person {id: $person2Id}) \
    CALL engram.algo.kshortestpaths.stream({projection: projection, sourceNode: id(person1), targetNode: id(person2), k: 1}) \
    YIELD totalCost \
    WITH collect(totalCost) AS costs \
    UNWIND (CASE WHEN size(costs) = 0 THEN [-1.0] ELSE costs END) AS c \
    RETURN max(c) AS totalCost";

/// Driven from the REPLY (a Comment — only comments reply, in LDBC and here),
/// so the head is a label scan the seed split parallelises, and with NO label
/// test on a message: the replied-to message is a Post exactly when it is the
/// root the forum walk ends at (`post = m2`), which is PostgreSQL's
/// `ParentMessageId IS NULL`. A label test outside the pattern binds the node
/// from its record — a store read per message, which at SF10 no longer fits
/// the page cache.
const EDGE_CENTRIC: &str = "MATCH (m1:Comment) \
    MATCH (m1)-[:REPLY_OF]->(m2:Message) \
    MATCH (m1)-[:HAS_CREATOR]->(c1:Person), (m2)-[:HAS_CREATOR]->(c2:Person) \
    WHERE c1 <> c2 AND (c1)-[:KNOWS]-(c2) \
    OPTIONAL MATCH (m1)-[:REPLY_OF*0..]->(post:Post)<-[:CONTAINER_OF]-(forum:Forum) \
      WHERE forum.creationDate >= $startDate AND forum.creationDate <= $endDate \
    WITH CASE WHEN id(c1) < id(c2) THEN id(c1) ELSE id(c2) END AS a, \
         CASE WHEN id(c1) < id(c2) THEN id(c2) ELSE id(c1) END AS b, \
         CASE WHEN forum IS NULL THEN 0.0 WHEN post = m2 THEN 1.0 ELSE 0.5 END AS x \
    WITH a, b, sum(x) AS w \
    WITH collect({source: a, target: b, weight: 1.0 / (w + 1.0)}) AS interacting \
    MATCH (pA:Person)-[:KNOWS]-(pB:Person) WHERE id(pA) < id(pB) \
    WITH interacting, collect({source: id(pA), target: id(pB), weight: 1.0}) AS knows \
    CALL engram.algo.project({name: 'q15', nodeLabels: ['Person'], edges: knows + interacting, orientation: 'UNDIRECTED'}) \
    YIELD projection \
    MATCH (person1:Person {id: $person1Id}), (person2:Person {id: $person2Id}) \
    CALL engram.algo.kshortestpaths.stream({projection: projection, sourceNode: id(person1), targetNode: id(person2), k: 1}) \
    YIELD totalCost \
    WITH collect(totalCost) AS costs \
    UNWIND (CASE WHEN size(costs) = 0 THEN [-1.0] ELSE costs END) AS c \
    RETURN max(c) AS totalCost";

#[test]
fn the_edge_centric_weights_answer_as_the_pair_centric_ones() {
    let g = threads();
    let mut interacted = 0;
    for (from, to) in [(0, 700), (200, 400), (10_000, 20_000)] {
        for (p1, p2) in [(0, 20), (3, 17), (7, 33), (12, 13), (0, 40)] {
            let want = cost(&g, PAIR_CENTRIC, p1, p2, from, to);
            let got = cost(&g, EDGE_CENTRIC, p1, p2, from, to);
            assert_eq!(got, want, "pair ({p1}, {p2}), window [{from}, {to}]");
            if let Value::Float(f) = want {
                // a cost that is not a whole number crossed an interacting pair
                if f.fract() != 0.0 {
                    interacted += 1;
                }
            }
        }
        assert_eq!(
            cost(&g, EDGE_CENTRIC, 0, 40, from, to),
            Value::Float(-1.0),
            "a person with no route answers -1.0"
        );
    }
    assert!(interacted > 0, "no route crossed a weighted pair; this compares nothing");
}
