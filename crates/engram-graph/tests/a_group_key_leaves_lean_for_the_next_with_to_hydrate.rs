//! A node a breaker carries BARE is bound lean — on what that stage reads of
//! it — when the next clause is a LIMITed, non-aggregating WITH that outputs
//! it bare: that WITH's collector hydrates its survivors, before projecting
//! them, and every later clause sees the whole node. A DISTINCT with a LIMIT
//! and no ORDER BY stops its producer once it holds that many distinct rows.
//!
//! SNB BI bi4's prefix is `MATCH (country)<-…-(person)<-[:HAS_MEMBER]-(forum)
//! WHERE forum.creationDate > $date WITH country, forum, count(person) AS
//! numberOfMembers ORDER BY … WITH DISTINCT forum AS topForum LIMIT 100 …`:
//! `forum` was bound WHOLE on every membership row — 4,982,242 full reads at
//! SF3, 35 s of a serial statement — to keep a hundred forums.

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

/// The same statement with late projection off: every carry bound whole.
fn control(g: &Graph, q: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    g.set_late_projection(false);
    let out = run(g, q);
    g.set_late_projection(true);
    out
}

fn counter(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

const LEAN: &str = "interp.breaker bound a bare carry lean for the next WITH to hydrate";
const STOPPED: &str = "interp.distinct limit stopped the producer";
/// Since rev54 the grouping hands on only the rows the DISTINCT reads, which
/// then has nothing to stop.
const KEPT: &str = "interp.grouping kept only the groups a downstream DISTINCT LIMIT reads";
const FULL: &str = "graph.nodes materialised in full";

/// Three countries of two cities; 60 people; 30 forums, each with a long
/// `title` and a `blurb` (so a lean forum and a whole one differ), created on
/// day `id`, each with members from every country.
fn forums() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 2) AS c CREATE (:Country {id: c})");
    ddl(
        &g,
        "MATCH (co:Country) UNWIND range(0, 1) AS k \
         CREATE (:City {id: co.id * 10 + k})-[:IS_PART_OF]->(co)",
    );
    ddl(
        &g,
        "UNWIND range(0, 59) AS i MATCH (ci:City {id: (i % 3) * 10 + (i % 2)}) \
         CREATE (:Person {id: i})-[:IS_LOCATED_IN]->(ci)",
    );
    ddl(
        &g,
        "UNWIND range(0, 29) AS f \
         CREATE (:Forum {id: f, creationDate: f, title: 'forum number ' + toString(f), \
                         blurb: 'a long description of forum ' + toString(f * 7919)})",
    );
    ddl(
        &g,
        "MATCH (f:Forum), (p:Person) WHERE (p.id * 7 + f.id * 3) % 5 < 2 + f.id % 3 \
         CREATE (f)-[:HAS_MEMBER]->(p)",
    );
    let _ = g.warm();
    g
}

const PREFIX: &str = "MATCH (country:Country)<-[:IS_PART_OF]-(:City)<-[:IS_LOCATED_IN]-(person:Person)\
    <-[:HAS_MEMBER]-(forum:Forum) WHERE forum.creationDate > 5 \
    WITH country, forum, count(person) AS numberOfMembers \
    ORDER BY numberOfMembers DESC, forum.id ASC, country.id \
    WITH DISTINCT forum AS topForum LIMIT 4";

#[test]
fn bi4s_prefix_keeps_its_forums_whole_after_binding_them_lean() {
    let g = forums();
    // Every later read of the survivors — a property, the whole map — must
    // see the WHOLE node the control's full binding gives.
    let q = format!(
        "{PREFIX} RETURN topForum.id AS id, topForum.blurb AS blurb, \
         size(keys(topForum)) AS k ORDER BY id"
    );
    let (want, cw) = control(&g, &q);
    let (got, c) = run(&g, &q);
    assert_eq!(got, want, "the survivors were not hydrated whole");
    assert_eq!(got.len(), 4, "{got:?}");
    assert!(counter(&c, LEAN) > 0, "the lever did not fire: {c:?}");
    assert!(
        counter(&c, STOPPED) > 0 || counter(&c, KEPT) > 0,
        "the DISTINCT read every group's row: {c:?}"
    );
    assert!(
        counter(&c, FULL) + 50 < counter(&cw, FULL),
        "{} full reads lean against {} whole",
        counter(&c, FULL),
        counter(&cw, FULL)
    );
}

#[test]
fn every_neighbouring_shape_answers_as_the_whole_binding_does() {
    let g = forums();
    for q in [
        // bi4 whole, its CALL body reading the top forums by identity
        format!(
            "{PREFIX} WITH collect(topForum) AS topForums \
             CALL {{ WITH topForums UNWIND topForums AS t MATCH (p:Person)<-[:HAS_MEMBER]-(t) \
                     RETURN p, 0 AS c }} \
             RETURN p.id AS id, sum(c) AS s ORDER BY id LIMIT 7"
        ),
        // the collected list read whole, and by property
        format!("{PREFIX} WITH collect(topForum) AS fs RETURN [f IN fs | f.title] AS titles"),
        format!("{PREFIX} RETURN topForum ORDER BY topForum.id"),
        // the next WITH ordered by a property read through its alias (top-k)
        "MATCH (country:Country)<-[:IS_PART_OF]-(:City)<-[:IS_LOCATED_IN]-(person:Person)\
         <-[:HAS_MEMBER]-(forum:Forum) \
         WITH country, forum, count(person) AS n \
         WITH forum AS f ORDER BY f.title DESC LIMIT 3 RETURN f.id AS id, f.blurb AS b"
            .to_string(),
        // a next WITH that aggregates: not a hydrating collector
        "MATCH (country:Country)<-[:IS_PART_OF]-(:City)<-[:IS_LOCATED_IN]-(person:Person)\
         <-[:HAS_MEMBER]-(forum:Forum) \
         WITH country, forum, count(person) AS n \
         WITH forum, sum(n) AS total LIMIT 3 RETURN forum.blurb AS b, total ORDER BY b"
            .to_string(),
        // a plain DISTINCT + LIMIT
        "MATCH (f:Forum) RETURN DISTINCT f.creationDate % 7 AS d LIMIT 3".to_string(),
        "MATCH (f:Forum)-[:HAS_MEMBER]->(p:Person) WITH DISTINCT f.id % 4 AS k SKIP 1 LIMIT 2 \
         RETURN k"
            .to_string(),
    ] {
        let (want, _) = control(&g, &q);
        let (got, _) = run(&g, &q);
        assert!(!want.is_empty(), "vacuous: {q}");
        assert_eq!(got, want, "\n  {q}");
    }
}
