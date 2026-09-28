//! A pattern comprehension whose filter pins its start runs once per pinned
//! start, not once over the whole graph.
//!
//! SNB Interactive IC14 weights each relationship of a path by counting
//! message exchanges between its two endpoints:
//!
//! ```text
//! [(a:Person)<-[:HAS_CREATOR]-(:Comment)-[:REPLY_OF]->(:Post)-[:HAS_CREATOR]->(b:Person)
//!  WHERE (a.id = startNode(r).id AND b.id = endNode(r).id) OR (…reversed…) | 1.0]
//! ```
//!
//! `a` and `b` are FRESH pattern variables, tied to the path's endpoints only
//! by an id-equality `WHERE` — so neither end is bound and the comprehension
//! enumerates every `Person<-Comment->Post->Person` path in the graph, once
//! per relationship in the path. At SF3 that is over 300 s and 55.7 GiB,
//! against Neo4j's 1 s at SF10.
//!
//! # The safety argument these tests are built around
//!
//! The filter is STILL applied to every row the seeded run produces. So an
//! extracted id set that is too WIDE costs time and changes nothing, and only
//! one that is too NARROW can lose a match. Every test here therefore attacks
//! narrowing: a branch that does not pin the start, a correlated bound, two
//! branches pinning different keys, a pin that matches nothing.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn g() -> Graph {
    Graph::new(Store::new(), Realm(1), Namespace(1))
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    run_query(g, &parse_statement(src).expect("parses"), BTreeMap::new())
        .expect("runs")
        .rows
}

/// Four people; 0 writes to 1, 2 writes to 3. `id` is an INTEGER, as SNB's is.
fn fixture() -> Graph {
    let g = g();
    let mut people = Vec::new();
    for i in 0..4i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i));
        m.insert("name".to_string(), Value::Str(format!("p{i}")));
        people.push(g.create_node(&["Person".into()], &m).expect("node"));
    }
    // a message from `people[i]` that replies to a post by `people[j]`
    let link = |i: usize, j: usize| {
        let c = g
            .create_node(&["Comment".into()], &BTreeMap::new())
            .expect("n");
        let p = g
            .create_node(&["Post".into()], &BTreeMap::new())
            .expect("n");
        g.create_rel(c, "HAS_CREATOR", people[i], &BTreeMap::new())
            .expect("r");
        g.create_rel(c, "REPLY_OF", p, &BTreeMap::new()).expect("r");
        g.create_rel(p, "HAS_CREATOR", people[j], &BTreeMap::new())
            .expect("r");
    };
    link(0, 1);
    link(2, 3);
    let _ = g.warm();
    g
}

const PAT: &str =
    "[(a:Person)<-[:HAS_CREATOR]-(:Comment)-[:REPLY_OF]->(:Post)-[:HAS_CREATOR]->(b:Person)";

#[test]
fn the_disjunctive_id_filter_counts_only_its_pair() {
    let g = fixture();
    let r = rows(
        &g,
        &format!(
            "RETURN size({PAT} \
             WHERE (a.id = 0 AND b.id = 1) OR (a.id = 1 AND b.id = 0) | 1]) AS n"
        ),
    );
    assert_eq!(r[0][0], Value::Int(1), "exactly the 0->1 exchange: {r:?}");
}

#[test]
fn the_other_pair_is_counted_separately() {
    let g = fixture();
    let r = rows(
        &g,
        &format!(
            "RETURN size({PAT} \
             WHERE (a.id = 2 AND b.id = 3) OR (a.id = 3 AND b.id = 2) | 1]) AS n"
        ),
    );
    assert_eq!(r[0][0], Value::Int(1), "{r:?}");
}

#[test]
fn a_pair_with_no_exchange_counts_zero() {
    let g = fixture();
    let r = rows(
        &g,
        &format!(
            "RETURN size({PAT} \
             WHERE (a.id = 0 AND b.id = 3) OR (a.id = 3 AND b.id = 0) | 1]) AS n"
        ),
    );
    assert_eq!(r[0][0], Value::Int(0), "{r:?}");
}

#[test]
fn it_engages_on_that_filter() {
    let g = fixture();
    let t = engram_observe::with_trace(|| {
        rows(
            &g,
            &format!(
                "RETURN size({PAT} \
                 WHERE (a.id = 0 AND b.id = 1) OR (a.id = 1 AND b.id = 0) | 1]) AS n"
            ),
        )
    })
    .1;
    assert!(
        t.counters()
            .contains_key("interp.comprehension start pinned by its own filter"),
        "the start was not pinned: {:?}",
        t.counters()
    );
}

#[test]
fn a_branch_that_does_not_pin_the_start_declines() {
    // THE SHARPEST NARROWING RISK. The second branch constrains only `b`, so
    // `a` may be ANY person — an id set for `a` cannot stand for that, and the
    // extraction must decline. Both exchanges qualify through that branch.
    let g = fixture();
    let (r, t) = engram_observe::with_trace(|| {
        rows(
            &g,
            &format!("RETURN size({PAT} WHERE (a.id = 0 AND b.id = 1) OR b.id = 3 | 1]) AS n"),
        )
    });
    assert_eq!(r[0][0], Value::Int(2), "both exchanges qualify: {r:?}");
    assert!(
        !t.counters()
            .contains_key("interp.comprehension start pinned by its own filter"),
        "a free branch must not be narrowed away: {:?}",
        t.counters()
    );
}

#[test]
fn branches_pinning_different_keys_decline() {
    // One branch pins `a.id`, the other `a.name`. A single key's id set cannot
    // represent both, so the extraction must decline — and both exchanges must
    // still be counted.
    let g = fixture();
    let (r, t) = engram_observe::with_trace(|| {
        rows(
            &g,
            &format!(
                "RETURN size({PAT}                  WHERE (a.id = 0 AND b.id = 1) OR (a.name = 'p2' AND b.id = 3) | 1]) AS n"
            ),
        )
    });
    assert_eq!(r[0][0], Value::Int(2), "both exchanges qualify: {r:?}");
    assert!(
        !t.counters()
            .contains_key("interp.comprehension start pinned by its own filter"),
        "two different keys cannot be one id set: {:?}",
        t.counters()
    );
}

#[test]
fn a_correlated_bound_is_left_to_the_ordinary_walk() {
    // The pin reads `b`, which the PATTERN binds — it cannot be evaluated
    // before the walk, so the extraction must decline and the answer must
    // still be right.
    let g = fixture();
    let r = rows(
        &g,
        &format!("RETURN size({PAT} WHERE a.id = b.id - 1 | 1]) AS n"),
    );
    assert_eq!(
        r[0][0],
        Value::Int(2),
        "0->1 and 2->3 both differ by one: {r:?}"
    );
}

#[test]
fn a_pin_matching_no_node_yields_nothing_and_walks_nothing() {
    let g = fixture();
    let r = rows(
        &g,
        &format!(
            "RETURN size({PAT} \
             WHERE (a.id = 999 AND b.id = 1) OR (a.id = 998 AND b.id = 0) | 1]) AS n"
        ),
    );
    assert_eq!(r[0][0], Value::Int(0), "{r:?}");
}

#[test]
fn the_outer_row_supplies_the_pinned_values() {
    // IC14's pins are not literals — they read `startNode(r).id` off the outer
    // row. The extraction must evaluate against the OUTER scope.
    let g = fixture();
    // the path IC14 weights: a KNOWS edge between the two endpoints
    let _ = rows(
        &g,
        "MATCH (x:Person {id: 0}), (y:Person {id: 1}) CREATE (x)-[:KNOWS]->(y) RETURN 1 AS ok",
    );
    let r = rows(
        &g,
        &format!(
            "MATCH (x:Person {{id: 0}}) \
             RETURN size({PAT} WHERE a.id = x.id AND b.id = x.id + 1 | 1]) AS n"
        ),
    );
    assert_eq!(r[0][0], Value::Int(1), "{r:?}");
}

#[test]
fn ic14s_exact_term_pins_its_start() {
    // THE SHAPE THIS FEATURE EXISTS FOR, and the nine tests above did not
    // cover it. IC14's comprehension is nested inside a `reduce` inside a list
    // comprehension, and its pins read `startNode(r).id` — a GRAPH-AWARE
    // function, not a plain property read.
    //
    // The extraction evaluated pins with `eval`, which passes NO hooks, so
    // `startNode(r)` failed to evaluate, every branch looked unpinned and the
    // whole thing declined. The FLAT spelling of the identical filter pinned
    // fine — so every test above passed while the query this was built for
    // silently took the old path. Pins now evaluate with the same hooks the
    // comprehension itself runs under.
    let g = fixture();
    // the path IC14 weights: a KNOWS edge between the two endpoints
    let _ = rows(
        &g,
        "MATCH (x:Person {id: 0}), (y:Person {id: 1}) CREATE (x)-[:KNOWS]->(y) RETURN 1 AS ok",
    );
    let src = "MATCH path = allShortestPaths((p1:Person {id: 0})-[:KNOWS*0..]-(p2:Person {id: 1}))        WITH path, relationships(path) AS rs        WITH [r IN rs | reduce(w = 0.0, v IN          [(a:Person)<-[:HAS_CREATOR]-(:Comment)-[:REPLY_OF]->(:Post)-[:HAS_CREATOR]->(b:Person)           WHERE (a.id = startNode(r).id AND b.id = endNode(r).id)              OR (a.id = endNode(r).id AND b.id = startNode(r).id) | 1.0] | w + v)] AS w1        RETURN w1";
    let (r, t) = engram_observe::with_trace(|| rows(&g, src));
    assert!(!r.is_empty(), "the path is found and weighted: {r:?}");
    assert!(
        t.counters()
            .contains_key("interp.comprehension start pinned by its own filter"),
        "a comprehension nested in a reduce, pinned by a graph-aware function,          must still pin: {:?}",
        t.counters()
    );
}
