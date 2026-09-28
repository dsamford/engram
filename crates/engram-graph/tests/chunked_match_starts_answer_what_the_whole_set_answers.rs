//! Chunked match starts answer exactly what the whole start set answers: the
//! same rows, in the same order, the same errors, and the same graph after.
//!
//! `Graph::set_match_start_chunk` carries a writing statement's start
//! candidates through a path's hops a chunk at a time and tests the WHERE as
//! each row finishes; `0` is the old order of work — every candidate at once,
//! the WHERE after collection. Every statement below runs on two graphs built
//! the same way, one at chunk 0 and one at chunk 3 (so every label crosses
//! many chunk boundaries), and each answer is compared row by row IN ORDER,
//! then the whole graph is compared.
//!
//! The order is the claim that needs the test: it holds because a partial's
//! completions are pushed contiguously, so the finished rows are grouped by
//! start in candidate order however the work is cut.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any};
use engram_graph::{Graph, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const CHUNKED: &str = "interp.matcher carried its starts in chunks";

/// Each a writing statement, so each takes the per-row matcher.
const SHAPES: &[&str] = &[
    // a WHERE over one path
    "MATCH (a:P) WHERE a.id % 3 = 0 SET a.seen = true RETURN a.id",
    // a hop; the WHERE reads both ends
    "MATCH (a:P)-[r:T]->(b:P) WHERE a.id < b.id AND b.id % 2 = 1 \
     SET r.w = a.id + b.id RETURN a.id, b.id, r.w",
    // variable length: one start completes at several depths
    "MATCH (a:P)-[:T*1..3]->(b:P) WHERE a.id % 5 = 0 \
     SET b.reach = coalesce(b.reach, 0) + 1 RETURN a.id, b.id, b.reach",
    // a named path
    "MATCH p = (a:P)-[:T*1..2]->(b:P) WHERE a.id % 4 = 1 \
     SET a.w = length(p) RETURN a.id, length(p), [n IN nodes(p) | n.id]",
    // two paths, the WHERE joining them: tested where the LAST path finishes
    "MATCH (a:P), (q:Q) WHERE a.id = q.k SET q.hit = a.id RETURN a.id, q.k",
    // an unlabelled start and no WHERE (every node is a candidate)
    "MATCH ()-[r:U]->(b:P) SET b.u = coalesce(b.u, 0) + 1 RETURN id(r), b.id, b.u",
    // OPTIONAL MATCH with its own WHERE, after a filtered MATCH
    "MATCH (a:P) WHERE a.id < 12 OPTIONAL MATCH (a)-[:U]->(c:P) WHERE c.id > 20 \
     SET a.o = coalesce(c.id, -1) RETURN a.id, c.id",
    // shortestPath from an unbound start (one chunk by design; still
    // compared). Bounded: this matcher enumerates every walk before it keeps
    // the shortest, and +1/+3 steps over 40 nodes are millions of walks.
    "MATCH p = shortestPath((a:P)-[:T*1..4]->(b:P {id: 30})) WHERE a.id % 6 = 0 \
     SET a.s = length(p) RETURN a.id, length(p)",
    // a WHERE that is not a boolean: both refuse, with the same words
    "MATCH (a:P) WHERE a.id SET a.bad = 1 RETURN a.id",
    // a delete of nothing, and a delete of some
    "MATCH (a:P) WHERE a.id >= 1000000 DETACH DELETE a RETURN count(*)",
    "MATCH (a:P) WHERE a.id % 7 = 3 DETACH DELETE a RETURN count(*)",
    // what the deletes left, walked again
    "MATCH (a:P)-[:T]->(b:P)-[:T]->(c:P) WHERE a.id % 2 = 0 \
     SET c.two = coalesce(c.two, 0) + 1 RETURN a.id, b.id, c.id",
];

fn run(g: &Graph, q: &str) -> Result<Vec<Vec<Value>>, String> {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new())
        .map(|r| r.rows)
        .map_err(|e| format!("{e:?}"))
}

fn fixture(chunk: usize) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_match_start_chunk(chunk);
    for q in [
        "UNWIND range(0, 39) AS i CREATE (:P {id: i})",
        "UNWIND range(0, 9) AS i CREATE (:Q {k: i * 4})",
        "UNWIND range(0, 38) AS i MATCH (a:P {id: i}), (b:P {id: i + 1}) CREATE (a)-[:T]->(b)",
        "UNWIND range(0, 36) AS i MATCH (a:P {id: i}), (b:P {id: i + 3}) CREATE (a)-[:T]->(b)",
        // i = 0 is a self-loop
        "UNWIND range(0, 39) AS i MATCH (a:P {id: i}), (b:P {id: (i * 7) % 40}) \
         CREATE (a)-[:U]->(b)",
    ] {
        run(&g, q).unwrap_or_else(|e| panic!("fixture `{q}`: {e}"));
    }
    g
}

/// The whole graph, in an order that does not depend on how it was walked.
fn dump(g: &Graph) -> Vec<String> {
    let mut all: Vec<String> = run(g, "MATCH (n) RETURN id(n), labels(n), properties(n)")
        .expect("nodes")
        .into_iter()
        .chain(
            run(
                g,
                "MATCH (a)-[r]->(b) RETURN id(r), type(r), properties(r), id(a), id(b)",
            )
            .expect("relationships"),
        )
        .map(|r| format!("{r:?}"))
        .collect();
    all.sort();
    all
}

#[test]
fn chunked_starts_give_the_same_rows_in_the_same_order() {
    let whole = fixture(0);
    let chunked = fixture(3);
    assert_eq!(dump(&whole), dump(&chunked), "the fixtures differ");

    let mut chunks = 0;
    for q in SHAPES {
        let (a, trace) = engram_observe::with_trace(|| run(&whole, q));
        assert_eq!(
            trace.counters().get(CHUNKED).copied().unwrap_or(0),
            0,
            "`{q}`: chunk 0 binds every start at once"
        );
        let (b, trace) = engram_observe::with_trace(|| run(&chunked, q));
        chunks += trace.counters().get(CHUNKED).copied().unwrap_or(0);
        assert_eq!(a, b, "`{q}`: the chunked answer differs");
        assert_eq!(dump(&whole), dump(&chunked), "`{q}`: the graphs differ after it");
        if !q.contains("a.id SET") {
            assert!(a.is_ok(), "`{q}` failed on both arms: {a:?}");
        }
    }
    assert!(
        chunks >= 10,
        "the chunked arm must actually have chunked: {chunks} chunked matches"
    );
}
