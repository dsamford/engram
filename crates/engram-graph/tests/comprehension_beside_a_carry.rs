#![allow(non_snake_case)]
//! Fix 36a: a list comprehension (reduce, list predicate, map projection)
//! beside a carried node that reads NOTHING of that node is not a
//! whole-entity use of it. `group_key_prop_only` declined every such
//! expression outright, so `WITH n, [x IN collect({name: ent.name}) WHERE
//! x.name IS NOT NULL] AS entities` left `n` out of the prop-only carries
//! and the breaker before it demanded the FULL node for every row: the
//! production email revival pick decoded 16,084 emails — bodies along,
//! 5,900 block misses — for a top-1 whose RETURN read three properties
//! (2.1 s on the mirror against Neo4j's 74 ms).
//!
//! Every answer is checked against the same statement with late
//! projection OFF (every carry demanded in full).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("maxRevivals".to_string(), Value::Int(3));
    p.insert("n".to_string(), Value::Int(1));
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

fn full_demand(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    g.set_late_projection(false);
    let r = rows(g, src);
    g.set_late_projection(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const FULL: &str = "graph.nodes materialised in full";
const BESIDE: &str = "interp.comprehension beside a carry reads nothing of it";

/// 2,000 classified emails with 3 KB bodies; every third mentions an
/// Interest (excluded by the anti-join), every fifth a couple of Entities;
/// `reinforceRevivals` runs 0..4.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let body: String = "b".repeat(3072);
    let mut interest = BTreeMap::new();
    interest.insert("id".to_string(), Value::Str("int-1".into()));
    let interest = g.create_node(&["Interest".into()], &interest).expect("interest");
    let mut ents = Vec::new();
    for k in 0..6i64 {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), Value::Str(format!("Entity {k}")));
        m.insert("type".to_string(), Value::Str(if k % 2 == 0 { "org".into() } else { "person".into() }));
        ents.push(g.create_node(&["Entity".into()], &m).expect("entity"));
    }
    for i in 0..2000i64 {
        let mut m = BTreeMap::new();
        m.insert("nodeType".to_string(), Value::Str("email".into()));
        m.insert("classified".to_string(), Value::Bool(i % 7 != 0));
        m.insert("userId".to_string(), Value::Str(format!("u{}", i % 40)));
        m.insert("nodeId".to_string(), Value::Str(format!("mail-{i:05}")));
        m.insert("subject".to_string(), Value::Str(format!("subject {i}")));
        m.insert("createdAt".to_string(), Value::Str(format!("2026-08-{:02}T{:02}:00:00Z", 1 + i % 28, i % 24)));
        if i % 4 != 0 {
            m.insert("reinforceRevivals".to_string(), Value::Int(i % 5));
        }
        m.insert("rawData".to_string(), Value::Str(body.clone()));
        let n = g.create_node(&["UserDataNode".into()], &m).expect("email");
        if i % 3 == 0 {
            g.create_rel(n, "MENTIONS_INTEREST", interest, &BTreeMap::new()).expect("mi");
        }
        if i % 5 == 0 {
            for k in 0..2 {
                g.create_rel(n, "MENTIONS", ents[((i / 5 + k) % 6) as usize], &BTreeMap::new()).expect("m");
            }
        }
    }
    g
}

const PICK: &str = "MATCH (n:UserDataNode) \
    WHERE n.nodeType = 'email' AND n.classified = true \
      AND NOT EXISTS { MATCH (n)-[:MENTIONS_INTEREST]->(:Interest) } \
      AND coalesce(n.reinforceRevivals, 0) < $maxRevivals \
    WITH n ORDER BY coalesce(n.reinforceRevivals, 0) ASC, n.createdAt DESC \
    LIMIT toInteger($n) \
    OPTIONAL MATCH (n)-[:MENTIONS]->(ent:Entity) \
    WITH n, [x IN collect({ name: ent.name, type: ent.type }) WHERE x.name IS NOT NULL] AS entities \
    RETURN n.nodeId AS nodeId, n.userId AS userId, n.subject AS subject, entities";

#[test]
fn the_revival_pick_binds_its_carry_lean() {
    let g = corpus();
    let want = full_demand(&g, PICK);
    assert_eq!(want.len(), 1);
    let (got, c) = traced(&g, PICK);
    assert_eq!(got, want);
    // Either the general path binds the carry lean for the top-k (lever
    // G'), or — fix 57 — the columnar stage pages the carry and hydrates
    // the one survivor; both decode the survivor, never the population.
    assert!(
        count_of(&c, BESIDE) > 0
            || count_of(&c, "interp.columnar stage hydrated a bare node for a survivor") > 0,
        "{c:?}"
    );
    assert!(
        count_of(&c, FULL) < 20,
        "the pick decodes its survivor, not the population: {c:?}"
    );
}

/// A comprehension that DOES read the carry keeps the full demand, and a
/// bare use through it agrees with the full-demand answer.
#[test]
fn a_comprehension_that_reads_the_carry_is_still_a_whole_use() {
    let g = corpus();
    let src = "MATCH (n:UserDataNode) WHERE n.nodeType = 'email' AND n.classified = true \
        AND coalesce(n.reinforceRevivals, 0) < $maxRevivals \
        WITH n ORDER BY n.createdAt DESC LIMIT 3 \
        OPTIONAL MATCH (n)-[:MENTIONS]->(ent:Entity) \
        WITH n, [x IN collect(ent.name) WHERE x <> n.subject] AS names \
        RETURN n.nodeId AS nodeId, size(keys(n)) AS width, names ORDER BY nodeId";
    let want = full_demand(&g, src);
    assert_eq!(want.len(), 3);
    let (got, c) = traced(&g, src);
    assert_eq!(got, want);
    // The comprehension reads `n.subject`, so `n` is a whole-entity carry:
    // the survivors reach it decoded in FULL — on the general path every
    // candidate was (as before), and with fix 57 the columnar stage pages
    // the carry and hydrates the three survivors in full before the
    // comprehension runs. Never lean.
    assert!(
        count_of(&c, FULL) >= 3,
        "a comprehension that reads the carry sees it in full: {c:?}"
    );
    assert_eq!(count_of(&c, BESIDE), 0, "never bound lean: {c:?}");
}

/// The other three shapes: reduce, list predicate, map projection — each
/// beside a carried node it never reads.
#[test]
fn reduce_predicate_and_map_projection_beside_a_carry() {
    let g = corpus();
    for src in [
        "MATCH (n:UserDataNode) WHERE n.nodeType = 'email' AND n.classified = true \
         WITH n ORDER BY n.createdAt DESC LIMIT 2 \
         OPTIONAL MATCH (n)-[:MENTIONS]->(ent:Entity) \
         WITH n, reduce(s = '', x IN collect(ent.name) | s + x) AS joined \
         RETURN n.nodeId AS nodeId, joined ORDER BY nodeId",
        "MATCH (n:UserDataNode) WHERE n.nodeType = 'email' AND n.classified = true \
         WITH n ORDER BY n.createdAt DESC LIMIT 2 \
         OPTIONAL MATCH (n)-[:MENTIONS]->(ent:Entity) \
         WITH n, any(x IN collect(ent.type) WHERE x = 'org') AS hasOrg \
         RETURN n.nodeId AS nodeId, hasOrg ORDER BY nodeId",
        "MATCH (n:UserDataNode) WHERE n.nodeType = 'email' AND n.classified = true \
         WITH n ORDER BY n.createdAt DESC LIMIT 2 \
         OPTIONAL MATCH (n)-[:MENTIONS]->(ent:Entity) \
         WITH n, collect(ent { .name, .type }) AS ents \
         RETURN n.nodeId AS nodeId, size(ents) AS k ORDER BY nodeId",
    ] {
        let want = full_demand(&g, src);
        assert_eq!(want.len(), 2, "`{src}`");
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert!(count_of(&c, FULL) < 20, "`{src}`: {c:?}");
    }
}
