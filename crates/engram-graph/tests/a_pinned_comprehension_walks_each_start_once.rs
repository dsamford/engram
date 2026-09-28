//! A pattern comprehension pinned by its own filter to a start it was pinned
//! to before, in the same statement, takes that start's walk back instead of
//! walking again — with the same answer.
//!
//! SNB Interactive IC14 pins each weight term to both ends of each
//! relationship on each shortest path. At SF3 its 42 paths are two hops long,
//! so the two people they join start one walk per relationship they touch —
//! 84 each — and the comprehension memo, keyed on the relationship, never
//! repeats: 168 evaluations, 1.68M adjacency visits, 23 s serial.

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

fn counter(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

const REUSED: &str = "interp.comprehension reused the walk of a start it was pinned to before";

/// The tests of this binary run one at a time. The comprehension memo is keyed
/// by a statement GENERATION every statement bumps (`StatementScope`), so a
/// statement of another test running meanwhile retires this one's pinned walks
/// and the reuse count read below loses some: the full suite once read 18
/// where the file alone reads its twenty-odd, with the same answer.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
const PINNED: &str = "interp.comprehension start pinned by its own filter";

/// Thirty people; 0 and 1 are joined through six friends (2-7), so six
/// two-hop shortest paths run between them. Every person posts five times;
/// people comment on each other's posts and on each other's comments.
fn forum() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 29) AS i CREATE (:Person {id: i})");
    ddl(
        &g,
        "MATCH (a:Person), (b:Person) WHERE a.id IN [0, 1] AND b.id >= 2 AND b.id <= 7 \
         CREATE (a)-[:KNOWS]->(b)",
    );
    ddl(
        &g,
        "MATCH (a:Person), (b:Person) WHERE a.id >= 2 AND b.id = a.id + 10 CREATE (a)-[:KNOWS]->(b)",
    );
    ddl(
        &g,
        "UNWIND range(0, 29) AS i UNWIND range(0, 4) AS k MATCH (p:Person {id: i}) \
         CREATE (p)<-[:HAS_CREATOR]-(:Post:Message {id: i * 100 + k})",
    );
    ddl(
        &g,
        "MATCH (c:Person), (post:Post)-[:HAS_CREATOR]->(p:Person) \
         WHERE c.id <> p.id AND (c.id + p.id + post.id) % 4 = 0 \
         CREATE (c)<-[:HAS_CREATOR]-(:Comment:Message {id: 10000 + c.id * 1000 + post.id})-[:REPLY_OF]->(post)",
    );
    ddl(
        &g,
        "MATCH (c:Person), (cm:Comment)-[:HAS_CREATOR]->(p:Person) \
         WHERE c.id <> p.id AND (c.id * 7 + cm.id) % 11 = 0 \
         CREATE (c)<-[:HAS_CREATOR]-(:Comment:Message {id: 900000 + c.id * 100000 + cm.id})-[:REPLY_OF]->(cm)",
    );
    let _ = g.warm();
    g
}

/// IC14 as the catalogue spells it (with a tie-break on the path, so the two
/// spellings' orders are comparable), its pins written as `pin`.
fn ic14(pin: &str) -> String {
    let w = |shape: &str, weight: &str| {
        format!(
            "[r in rels_in_path | reduce(w=0.0, v in [{shape} \
             WHERE ({pin}a.id = startNode(r).id and b.id=endNode(r).id) \
                OR ({pin}a.id=endNode(r).id and b.id=startNode(r).id) | {weight}] | w+v)]"
        )
    };
    format!(
        "MATCH path = allShortestPaths((person1:Person {{id: 0}})-[:KNOWS*0..]-(person2:Person {{id: 1}})) \
         WITH collect(path) as paths UNWIND paths as path \
         WITH path, relationships(path) as rels_in_path \
         WITH [n in nodes(path) | n.id] as personIdsInPath, {} as weight1, {} as weight2 \
         WITH personIdsInPath, reduce(w=0.0,v in weight1| w+v) as w1, reduce(w=0.0,v in weight2| w+v) as w2 \
         RETURN personIdsInPath, (w1+w2) as pathWeight ORDER BY pathWeight desc, personIdsInPath",
        w(
            "(a:Person)<-[:HAS_CREATOR]-(:Comment)-[:REPLY_OF]->(:Post)-[:HAS_CREATOR]->(b:Person)",
            "1.0"
        ),
        w(
            "(a:Person)<-[:HAS_CREATOR]-(:Comment)-[:REPLY_OF]->(:Comment)-[:HAS_CREATOR]->(b:Person)",
            "0.5"
        ),
    )
}

#[test]
fn ic14_walks_each_pinned_start_once_per_statement() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let g = forum();
    let (got, c) = run(&g, &ic14(""));
    // THE CONTROL: `a.id + 0 = …` is the same filter, but not a pin the
    // extraction reads — every evaluation walks the whole pattern and filters.
    let (want, cc) = run(&g, &ic14("0 + "));
    assert_eq!(counter(&cc, PINNED), 0, "the control pinned its start: {cc:?}");
    assert_eq!(got, want, "reusing a pinned walk changed IC14's answer");
    assert_eq!(got.len(), 6, "six two-hop paths join 0 and 1: {got:?}");
    assert!(
        got.iter().any(|r| r[1] != Value::Float(0.0)),
        "vacuous: every path weighs nothing: {got:?}"
    );
    assert!(counter(&c, PINNED) > 0, "the start was not pinned: {c:?}");
    // 6 paths x 2 relationships x 2 comprehensions, two pins each: 0 and 1
    // are pinned 12 times apiece and walked once apiece per comprehension.
    assert!(counter(&c, REUSED) >= 20, "the pinned walks were not reused: {c:?}");
}

#[test]
fn a_second_statement_walks_again() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let g = forum();
    let q = ic14("");
    let (first, _) = run(&g, &q);
    // A write between the statements: the second must see it.
    ddl(
        &g,
        "MATCH (c:Person {id: 2}), (post:Post)-[:HAS_CREATOR]->(p:Person {id: 0}) \
         CREATE (c)<-[:HAS_CREATOR]-(:Comment:Message {id: 7777777})-[:REPLY_OF]->(post)",
    );
    let (second, _) = run(&g, &q);
    let (control, _) = run(&g, &ic14("0 + "));
    assert_eq!(second, control, "a walk outlived its statement");
    assert_ne!(first, second, "vacuous: the write changed no weight");
}

#[test]
fn a_pinned_start_binds_only_what_the_comprehension_reads() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // The pinned start was bound WHOLE and cloned into every row of the walk
    // and of every reuse of it: IC14 at SF3 paid ~42 us a match for it, over
    // a walk that costs 7 us a match from a lean start. It now binds to the
    // comprehension's demand; with the lean-seed lever off it stays whole,
    // and the answers agree.
    let g = forum();
    let single = "MATCH (x:Person {id: 0}) RETURN size([(a:Person)<-[:HAS_CREATOR]-(:Comment)\
                  -[:REPLY_OF]->(:Post)-[:HAS_CREATOR]->(b:Person) \
                  WHERE (a.id = x.id AND b.id = 2) OR (a.id = 2 AND b.id = x.id) | 1.0]) AS n";
    const WHOLE: &str = "graph.nodes materialised in full";
    for q in [single.to_string(), ic14("")] {
        g.set_lean_subquery_seed(false);
        let (want, off) = run(&g, &q);
        g.set_lean_subquery_seed(true);
        let (got, on) = run(&g, &q);
        assert_eq!(got, want, "binding the pinned start lean changed `{q}`");
        assert!(counter(&on, PINNED) > 0, "`{q}` was not pinned: {on:?}");
        assert!(
            counter(&on, WHOLE) < counter(&off, WHOLE),
            "`{q}` still read its pinned starts whole: {} whole reads against {} with the \
             lever off",
            counter(&on, WHOLE),
            counter(&off, WHOLE)
        );
    }
    let (n, _) = run(&g, single);
    assert!(
        matches!(n.first().and_then(|r| r.first()), Some(Value::Int(k)) if *k > 0),
        "vacuous: the pinned comprehension matched nothing: {n:?}"
    );
}
