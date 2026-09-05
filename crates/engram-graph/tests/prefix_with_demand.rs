#![allow(non_snake_case)]
//! Fix 24 (v100), attributed on the production mirror with the v98 trace
//! marker:
//!
//! 1. a node carried by a NON-BREAKING `WITH` demanded FULL when anything
//!    after it read the node at all — `MATCH (t:ResearchTask) WHERE
//!    t.userId = $u WITH t RETURN t.id` materialised 416 records in full
//!    (12.8 ms vs Neo4j 2.9) — because the stage planner summarised no
//!    "properties read after it" for a prefix WITH;
//! 2. a start with an inline map and no index on its key (`(t:ResearchTask
//!    {userId: $u})`) never reached the column-filtered seed and materialised
//!    the whole label in full (517) for `node_satisfies` to test the map;
//! 3. the pipeline applied a seed var's own predicates through
//!    `load_var_columns`, which has no cache: 517 point reads of `userId`
//!    on every `MATCH (t:ResearchTask {userId: $u})-[:PROPOSED_GRAPH_WRITE]
//!    ->(p:GraphWriteProposal) RETURN count(p)` (6.8 ms vs 1.7).
//!
//! Every answer is checked against the fixture; the controls keep the old
//! behaviour where it is the right one.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("u".to_string(), Value::Str("u1".to_string()));
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

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const FULL: &str = "graph.nodes materialised in full";
const PREFIX_PROPS: &str = "interp.prefix projection demanded only the properties read after it";
const MAP_TAKEN: &str = "interp.seed column filter took the pattern map";
const FILTERED: &str = "interp.seeds filtered by columns";
const SEED_PREDS: &str = "interp.pipeline seed predicates filtered by columns";
const SERVED: &str = "interp.columnar column read served from the property-column cache";
const GATHER: &str = "graph.column point-gather";

/// 1,200 `:RT {userId, id}` — every third carries `userId = 'u1'` (400) —
/// each with one `-[:PGW]->(:GWP)`, beside 2,000 `:Other {userId: 'u1'}`.
/// No index anywhere: the shapes under test are the ones the catalogue
/// does not cover, and the OTHER label makes the partition-wide `userId`
/// probe answer more ids than `:RT` holds, so the label scan wins — the
/// mirror's case, where `userId` is carried by every user-owned label.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for _ in 0..2000 {
        let mut m = BTreeMap::new();
        m.insert("userId".to_string(), Value::Str("u1".into()));
        g.create_node(&["Other".into()], &m).expect("other");
    }
    for i in 0..1200i64 {
        let mut m = BTreeMap::new();
        m.insert(
            "userId".to_string(),
            Value::Str(if i % 3 == 0 { "u1".into() } else { format!("u{}", 2 + i % 5) }),
        );
        m.insert("id".to_string(), Value::Str(format!("rt-{i:05}")));
        let t = g.create_node(&["RT".into()], &m).expect("rt");
        let mut pm = BTreeMap::new();
        pm.insert("status".to_string(), Value::Str("pending".into()));
        let p = g.create_node(&["GWP".into()], &pm).expect("gwp");
        g.create_rel(t, "PGW", p, &BTreeMap::new()).expect("pgw");
    }
    g
}

#[test]
fn a_prefix_with_demands_only_what_the_rest_of_the_stage_reads() {
    let g = corpus();
    let _ = rows(&g, "MATCH (t:RT) WHERE t.userId = $u RETURN count(t) AS n"); // warm: keeps `userId`
    for (src, want) in [
        (
            "MATCH (t:RT) WHERE t.userId = $u WITH t RETURN t.id AS id ORDER BY id LIMIT 3",
            vec![
                vec![Value::Str("rt-00000".into())],
                vec![Value::Str("rt-00003".into())],
                vec![Value::Str("rt-00006".into())],
            ],
        ),
        (
            "MATCH (t:RT) WHERE t.userId = $u WITH t RETURN min(t.id) AS m",
            vec![vec![Value::Str("rt-00000".into())]],
        ),
        (
            "MATCH (t:RT) WHERE t.userId = $u WITH t RETURN count(t) AS n",
            vec![vec![Value::Int(400)]],
        ),
    ] {
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert_eq!(count_of(&c, FULL), 0, "`{src}` materialised in full: {c:?}");
        assert!(count_of(&c, PREFIX_PROPS) > 0, "`{src}`: {c:?}");
    }
    // CONTROL: a bare later use keeps the full node.
    let (got, c) = traced(&g, "MATCH (t:RT) WHERE t.userId = $u WITH t RETURN t ORDER BY t.id LIMIT 2");
    assert_eq!(got.len(), 2);
    assert!(count_of(&c, FULL) > 0, "a bare RETURN t is a whole use: {c:?}");
}

#[test]
fn a_var_free_pattern_map_joins_the_column_filtered_seed() {
    let g = corpus();
    let _ = rows(&g, "MATCH (t:RT) WHERE t.userId = $u RETURN count(t) AS n"); // warm
    let (got, c) = traced(&g, "MATCH (t:RT {userId: $u}) WITH t RETURN count(t) AS n");
    assert_eq!(got, vec![vec![Value::Int(400)]]);
    assert!(count_of(&c, MAP_TAKEN) > 0, "{c:?}");
    assert!(count_of(&c, FILTERED) > 0, "{c:?}");
    assert_eq!(count_of(&c, FULL), 0, "the map is tested from the column, not the record: {c:?}");
    // With the hop after the WITH — the production shape.
    let (got, c) = traced(
        &g,
        "MATCH (t:RT {userId: $u}) WITH t MATCH (t)-[:PGW]->(p:GWP) RETURN count(p) AS n",
    );
    assert_eq!(got, vec![vec![Value::Int(400)]]);
    assert_eq!(count_of(&c, FULL), 0, "{c:?}");
    // CONTROL: a CORRELATED map entry reads a variable and is left to the
    // per-row seed; the answer agrees.
    let (got, c) = traced(
        &g,
        "UNWIND [$u] AS uid MATCH (t:RT {userId: uid}) RETURN count(t) AS n",
    );
    assert_eq!(got, vec![vec![Value::Int(400)]]);
    assert_eq!(count_of(&c, MAP_TAKEN), 0, "{c:?}");
}

#[test]
fn the_pipelines_seed_predicates_are_answered_from_the_cached_column() {
    let g = corpus();
    const HOP: &str = "MATCH (t:RT {userId: $u})-[:PGW]->(p:GWP) RETURN count(p) AS n";
    // First read: the filter walks the label whole and KEEPS the column.
    let (got, c1) = traced(&g, HOP);
    assert_eq!(got, vec![vec![Value::Int(400)]]);
    assert!(count_of(&c1, SEED_PREDS) > 0, "{c1:?}");
    // Second read: the column is served from the cache — no point-gather,
    // no record read for the seed.
    let (got, c2) = traced(&g, HOP);
    assert_eq!(got, vec![vec![Value::Int(400)]]);
    assert!(count_of(&c2, SEED_PREDS) > 0, "{c2:?}");
    assert!(count_of(&c2, SERVED) > 0, "served from the cache: {c2:?}");
    assert_eq!(count_of(&c2, GATHER), 0, "no point-gather: {c2:?}");
    assert_eq!(count_of(&c2, "store.gets"), 0, "no record read: {c2:?}");
    // The WHERE form is the same shape.
    let (got, c3) = traced(
        &g,
        "MATCH (t:RT)-[:PGW]->(p:GWP) WHERE t.userId = $u RETURN count(p) AS n",
    );
    assert_eq!(got, vec![vec![Value::Int(400)]]);
    assert!(count_of(&c3, SEED_PREDS) > 0, "{c3:?}");
    // STRICT: a non-boolean seed predicate is not silently dropped — the
    // statement still raises, as it always did.
    let q = parse_statement("MATCH (t:RT)-[:PGW]->(p:GWP) WHERE t.userId RETURN count(p) AS n").unwrap();
    assert!(run_query(&g, &q, params()).is_err(), "a string is not a predicate");
}
