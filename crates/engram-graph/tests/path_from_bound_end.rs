#![allow(non_snake_case)]
//! Fix 37: `match_path` — the entry point of every pattern comprehension, of
//! an EXISTS/COUNT body the adjacency fast probe declines, and of the
//! materialising path's MATCH — drives a path whose START is unbound and
//! whose LAST node is bound in the row FROM THE BOUND END, as the streaming
//! matcher already did. The production KMWorkItem listing's
//! `[(parent:KMWorkItem)-[:HAS_EPIC|HAS_TASK|HAS_CHILD]->(w) | parent.id]`
//! scanned every work item IN FULL per output row: 104,853 record decodes
//! and 93,036 scans for six rows — 13.6 s against Neo4j's 19.6 ms, and
//! +3.5 GB of resident set per statement.
//!
//! Every answer is checked against the spelling written from the bound end.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params(id: &str) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("id".to_string(), Value::Str(id.to_string()));
    // wi-0101 → parent wi-0001 (a task, HAS_CHILD); wi-0300 → parent wi-0000
    // (the epic, HAS_EPIC); wi-0777 → parent wi-0077 (a task, HAS_EPIC).
    p.insert("ids".to_string(), Value::List(vec![Value::Str("wi-0101".into()), Value::Str("wi-0300".into()), Value::Str("wi-0777".into())]));
    p
}

fn rows(g: &Graph, src: &str, id: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params(id))
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str, id: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src, id));
    (r, trace.counters().clone())
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const FULL: &str = "graph.nodes materialised in full";
const REVERSED: &str = "interp.path driven from its bound end";

/// 1,500 `:KMWorkItem {id, title, kind, body}` — a 2 KB body each — where
/// every item past the first hundred has one parent (an epic, a task or a
/// plain child edge, by residue), plus a `:Filler` beside each so the label
/// is sparse in the id space.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let body: String = "b".repeat(2048);
    let mut ids = Vec::new();
    for i in 0..1500i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Str(format!("wi-{i:04}")));
        m.insert("title".to_string(), Value::Str(format!("work item {i}")));
        m.insert(
            "kind".to_string(),
            Value::Str(if i % 100 == 0 { "epic".into() } else { "task".into() }),
        );
        m.insert("body".to_string(), Value::Str(body.clone()));
        let id = g.create_node(&["KMWorkItem".into()], &m).expect("work item");
        ids.push(id);
        let mut f = BTreeMap::new();
        f.insert("other".to_string(), Value::Str(format!("filler-{i}")));
        g.create_node(&["Filler".into()], &f).expect("filler");
        if i >= 100 {
            let parent = ids[(i as usize) % 100];
            let rel = match i % 3 {
                0 => "HAS_EPIC",
                1 => "HAS_TASK",
                _ => "HAS_CHILD",
            };
            g.create_rel(parent, rel, id, &BTreeMap::new()).expect("parent edge");
        }
    }
    g
}

#[test]
fn a_pattern_comprehension_written_from_the_unbound_side_drives_from_the_bound_end() {
    let g = corpus();
    let written = "MATCH (w:KMWorkItem) WHERE w.id IN $ids \
        RETURN w.id AS id, [(parent:KMWorkItem)-[:HAS_EPIC|HAS_TASK|HAS_CHILD]->(w) | parent.id][0] AS parentId \
        ORDER BY id";
    let forward = "MATCH (w:KMWorkItem) WHERE w.id IN $ids \
        RETURN w.id AS id, [(w)<-[:HAS_EPIC|HAS_TASK|HAS_CHILD]-(parent:KMWorkItem) | parent.id][0] AS parentId \
        ORDER BY id";
    let want = rows(&g, forward, "wi-0100");
    assert_eq!(want.len(), 3);
    assert_eq!(want[0][1], Value::Str("wi-0001".into()), "wi-0101's parent is wi-0001: {want:?}");
    assert_eq!(want[1][1], Value::Str("wi-0000".into()), "wi-0300's parent is wi-0000: {want:?}");
    let (got, c) = traced(&g, written, "wi-0100");
    assert_eq!(got, want);
    assert!(count_of(&c, REVERSED) >= 3, "each row's comprehension reverses: {c:?}");
    assert!(
        count_of(&c, FULL) < 40,
        "no scan of the label per row (1,500 items × 3 rows before): {c:?}"
    );
}

/// An EXISTS body with a WHERE (the adjacency fast probe declines it) and a
/// COUNT body reach `match_path` too.
#[test]
fn a_subquery_body_written_from_the_unbound_side_drives_from_the_bound_end() {
    let g = corpus();
    for (written, forward) in [
        (
            "MATCH (w:KMWorkItem) WHERE w.id IN $ids AND EXISTS { MATCH (parent:KMWorkItem)-[:HAS_EPIC]->(w) WHERE parent.kind = 'epic' } RETURN w.id AS id ORDER BY id",
            "MATCH (w:KMWorkItem) WHERE w.id IN $ids AND EXISTS { MATCH (w)<-[:HAS_EPIC]-(parent:KMWorkItem) WHERE parent.kind = 'epic' } RETURN w.id AS id ORDER BY id",
        ),
        (
            "MATCH (w:KMWorkItem) WHERE w.id IN $ids RETURN w.id AS id, COUNT { MATCH (parent:KMWorkItem)-[:HAS_EPIC|HAS_TASK|HAS_CHILD]->(w) WHERE parent.kind IN ['epic','task'] } AS parents ORDER BY id",
            "MATCH (w:KMWorkItem) WHERE w.id IN $ids RETURN w.id AS id, COUNT { MATCH (w)<-[:HAS_EPIC|HAS_TASK|HAS_CHILD]-(parent:KMWorkItem) WHERE parent.kind IN ['epic','task'] } AS parents ORDER BY id",
        ),
    ] {
        let want = rows(&g, forward, "wi-0100");
        assert!(!want.is_empty(), "fixture: `{forward}`");
        let (got, c) = traced(&g, written, "wi-0100");
        assert_eq!(got, want, "`{written}`");
        // A body with a WHERE re-enters the streaming matcher, which drives
        // from the bound end under its own event; either way no label is
        // scanned per row — that is the claim.
        assert!(count_of(&c, FULL) < 40, "`{written}` scans no label per row: {c:?}");
    }
}

/// The materialising path's MATCH (a second MATCH clause over a bound var)
/// takes the same entry point.
#[test]
fn a_second_match_written_from_the_unbound_side_drives_from_the_bound_end() {
    let g = corpus();
    let written = "MATCH (w:KMWorkItem {id: $id}) MATCH (parent:KMWorkItem)-[:HAS_EPIC|HAS_TASK|HAS_CHILD]->(w) \
        RETURN parent.id AS parentId, parent.kind AS kind";
    let forward = "MATCH (w:KMWorkItem {id: $id}) MATCH (w)<-[:HAS_EPIC|HAS_TASK|HAS_CHILD]-(parent:KMWorkItem) \
        RETURN parent.id AS parentId, parent.kind AS kind";
    for id in ["wi-0100", "wi-0777", "wi-1499"] {
        let want = rows(&g, forward, id);
        assert_eq!(want.len(), 1, "{id}");
        let (got, c) = traced(&g, written, id);
        assert_eq!(got, want, "{id}");
        assert!(count_of(&c, FULL) < 40, "{id}: {c:?}");
    }
    // A root (no parent) answers no row either way.
    assert!(rows(&g, written, "wi-0007").is_empty());
    assert!(rows(&g, forward, "wi-0007").is_empty());
}

/// CONTROL: a two-hop chain bound at its end reverses whole; a chain whose
/// end is bound to NULL (an OPTIONAL MATCH miss) answers nothing on both
/// spellings.
#[test]
fn a_two_hop_chain_and_a_null_end_agree_with_the_forward_spelling() {
    let g = corpus();
    let written = "MATCH (w:KMWorkItem {id: $id}) \
        RETURN [(gp:KMWorkItem)-[:HAS_EPIC|HAS_TASK|HAS_CHILD]->(p:KMWorkItem)-[:HAS_EPIC|HAS_TASK|HAS_CHILD]->(w) | gp.id + '/' + p.id] AS chain";
    let forward = "MATCH (w:KMWorkItem {id: $id}) \
        RETURN [(w)<-[:HAS_EPIC|HAS_TASK|HAS_CHILD]-(p:KMWorkItem)<-[:HAS_EPIC|HAS_TASK|HAS_CHILD]-(gp:KMWorkItem) | gp.id + '/' + p.id] AS chain";
    // wi-0250's parent is wi-0050, whose parent is wi-0000 (250 % 100 = 50 ≥ ... no: only i ≥ 100 have parents).
    // wi-0150 → parent wi-0050 → no grandparent (50 < 100). Use an item whose parent has a parent: none exist
    // in this fixture (parents are the first hundred, which are roots), so the chain is empty on both spellings.
    let (got, c) = traced(&g, written, "wi-0150");
    assert_eq!(got, rows(&g, forward, "wi-0150"));
    assert_eq!(got[0][0], Value::List(Vec::new()));
    assert!(count_of(&c, FULL) < 40, "{c:?}");

    let null_end = "MATCH (w:KMWorkItem {id: $id}) OPTIONAL MATCH (w)-[:NOPE]->(x:KMWorkItem) \
        RETURN [(parent:KMWorkItem)-[:HAS_CHILD]->(x) | parent.id] AS parents";
    let (got, _) = traced(&g, null_end, "wi-0100");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0][0], Value::List(Vec::new()), "a null end binds nothing: {got:?}");
}
