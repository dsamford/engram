#![allow(non_snake_case)]
//! Fix 49: a top-level positive `EXISTS { (w)-[…]->(:L {k: $x}) }` (or bare
//! pattern predicate) conjunct whose far end is constant-seekable seeds the
//! label scan from the REVERSED probe — the ids the path binds to `w` when
//! walked from that end — so the general path never scans the label and
//! never materialises a member the probe rules out.
//!
//! The production KM listing `MATCH (w:KMWorkItem) WHERE true AND EXISTS {
//! (w)-[:BELONGS_TO_PROJECT]->(:KMProject {id: $projectId}) } RETURN
//! properties(w), [(w)-[:BELONGS_TO_PROJECT]->(p:KMProject) | p.id][0] …
//! ORDER BY w.sortOrder SKIP … LIMIT …` materialised all 15.5k work items in
//! full and opened a visitor scan per item for the 63 the project's incoming
//! edges name — 1.8–2.0 s against Neo4j's 13 ms.
//!
//! Every answer is checked against the spelling written from the seekable
//! end and against the untouched general path (columnar paths and hop
//! reversal off).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("pid".to_string(), Value::Str("proj-07".to_string()));
    p.insert("offset".to_string(), Value::Int(0));
    p.insert("limit".to_string(), Value::Int(10));
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

/// The untouched general path: columnar paths off AND hop reversal off (the
/// seed is gated on the latter).
fn general(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    g.set_hop_reversal(false);
    let r = rows(g, src);
    g.set_hop_reversal(true);
    g.set_columnar_scans(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const SEEDED: &str = "interp.seed driven from an existence probe's constant end";
const FULL: &str = "graph.nodes materialised in full";

/// 30 projects with a DECLARED `id` index (and an undeclared `name`); 3,000
/// items, item i belonging to project i % 30 (100 per project), with a few
/// properties; the items sit ABOVE the seek's label floor.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX proj_id FOR (n:Proj) ON (n.id)");
    ddl(&g, "CREATE INDEX item_id FOR (n:Item) ON (n.itemId)");
    let mut projs = Vec::new();
    for i in 0..30i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Str(format!("proj-{i:02}")));
        m.insert("name".to_string(), Value::Str(format!("project {i}")));
        projs.push(g.create_node(&["Proj".into()], &m).expect("proj"));
    }
    for i in 0..3000i64 {
        let mut m = BTreeMap::new();
        m.insert("itemId".to_string(), Value::Str(format!("item-{i:04}")));
        m.insert("sortOrder".to_string(), Value::Int((i * 7) % 101));
        m.insert("title".to_string(), Value::Str(format!("item {i} of {}", i % 30)));
        // Every fifth item OF EACH PROJECT is done (20 of its 100).
        m.insert("status".to_string(), Value::Str(if (i / 30) % 5 == 0 { "done" } else { "open" }.to_string()));
        let w = g.create_node(&["Item".into()], &m).expect("item");
        g.create_rel(w, "BELONGS_TO", projs[(i % 30) as usize], &BTreeMap::new())
            .expect("belongs");
    }
    g
}

#[test]
fn the_listing_is_seeded_from_the_projects_incoming_edges() {
    let g = corpus();
    for (written, from_end) in [
        (
            // The production shape: a bare `properties(w)`, a comprehension, an ordered page.
            "MATCH (w:Item) WHERE true AND EXISTS { (w)-[:BELONGS_TO]->(:Proj {id: $pid}) } RETURN properties(w) AS w, [(w)-[:BELONGS_TO]->(p:Proj) | p.id][0] AS pid ORDER BY w.sortOrder ASC, w.itemId SKIP toInteger($offset) LIMIT toInteger($limit)",
            "MATCH (p:Proj {id: $pid})<-[:BELONGS_TO]-(w:Item) RETURN properties(w) AS w, [(w)-[:BELONGS_TO]->(p2:Proj) | p2.id][0] AS pid ORDER BY w.sortOrder ASC, w.itemId SKIP toInteger($offset) LIMIT toInteger($limit)",
        ),
        (
            // The bare pattern-predicate spelling, unordered page.
            "MATCH (w:Item) WHERE (w)-[:BELONGS_TO]->(:Proj {id: $pid}) RETURN w.itemId AS id ORDER BY id",
            "MATCH (p:Proj {id: $pid})<-[:BELONGS_TO]-(w:Item) RETURN w.itemId AS id ORDER BY id",
        ),
        (
            // A further conjunct on the seed and a property projection.
            "MATCH (w:Item) WHERE w.status = 'open' AND EXISTS { (w)-[:BELONGS_TO]->(:Proj {id: $pid}) } RETURN w.itemId AS id, w.sortOrder AS s ORDER BY s, id",
            "MATCH (p:Proj {id: $pid})<-[:BELONGS_TO]-(w:Item) WHERE w.status = 'open' RETURN w.itemId AS id, w.sortOrder AS s ORDER BY s, id",
        ),
        (
            // The count, on the general path (the columnar aggregate is off in `general`).
            "MATCH (w:Item) WHERE EXISTS { (w)-[:BELONGS_TO]->(:Proj {id: $pid}) } RETURN count(w) AS n",
            "MATCH (p:Proj {id: $pid})<-[:BELONGS_TO]-(w:Item) RETURN count(w) AS n",
        ),
        (
            // An inline map on an UNDECLARED key does not pre-empt the probe:
            // that seek would probe an unscoped index and lose to the label.
            "MATCH (w:Item {status: 'done'}) WHERE EXISTS { (w)-[:BELONGS_TO]->(:Proj {id: $pid}) } RETURN count(w) AS n",
            "MATCH (p:Proj {id: $pid})<-[:BELONGS_TO]-(w:Item {status: 'done'}) RETURN count(w) AS n",
        ),
    ] {
        let want = general(&g, from_end);
        assert!(!want.is_empty(), "fixture: `{from_end}`");
        assert_eq!(general(&g, written), want, "general path: `{written}`");
        // The columnar paths declined this shape on the mirror; force the
        // general path so the seed is what is measured.
        g.set_columnar_scans(false);
        let (got, c) = traced(&g, written);
        g.set_columnar_scans(true);
        assert_eq!(got, want, "`{written}`");
        assert!(count_of(&c, SEEDED) > 0, "`{written}` seeds from the probe: {c:?}");
        // The probe walk, the start, the re-checked conjunct and the
        // comprehension each read a candidate — a handful of reads per
        // candidate for the project's 100 items, never the 3,000-item label
        // (the untouched path read every member at least once).
        assert!(
            count_of(&c, FULL) < 1500,
            "`{written}` reads the project's items, not the label: {c:?}"
        );
    }
}

/// CONTROLS: a negated conjunct, a disjunction, an end on an UNDECLARED key,
/// a start that seeks a DECLARED key of its own (map or WHERE form) and a
/// body with an inner WHERE are left to the paths they always took — same
/// rows, no probe seed.
#[test]
fn negations_disjunctions_undeclared_ends_and_seekable_starts_are_left_alone() {
    let g = corpus();
    for src in [
        "MATCH (w:Item) WHERE NOT EXISTS { (w)-[:BELONGS_TO]->(:Proj {id: $pid}) } RETURN count(w) AS n",
        "MATCH (w:Item) WHERE EXISTS { (w)-[:BELONGS_TO]->(:Proj {id: $pid}) } OR w.sortOrder < 3 RETURN count(w) AS n",
        "MATCH (w:Item) WHERE EXISTS { (w)-[:BELONGS_TO]->(:Proj {name: 'project 7'}) } RETURN count(w) AS n",
        "MATCH (w:Item {itemId: 'item-0007'}) WHERE EXISTS { (w)-[:BELONGS_TO]->(:Proj {id: $pid}) } RETURN count(w) AS n",
        "MATCH (w:Item) WHERE w.itemId = 'item-0007' AND EXISTS { (w)-[:BELONGS_TO]->(:Proj {id: $pid}) } RETURN count(w) AS n",
        "MATCH (w:Item) WHERE EXISTS { (w)-[:BELONGS_TO]->(:Proj {id: $pid}) WHERE w.status = 'open' } RETURN count(w) AS n",
    ] {
        let want = general(&g, src);
        g.set_columnar_scans(false);
        let (got, c) = traced(&g, src);
        g.set_columnar_scans(true);
        assert_eq!(got, want, "`{src}`");
        assert_eq!(count_of(&c, SEEDED), 0, "`{src}` is left as written: {c:?}");
    }
}

/// The probe seed is a SUPERSET filter: a member the probe names but a later
/// conjunct rejects is dropped, and a project no item belongs to answers the
/// empty page — as the untouched path does.
#[test]
fn the_seed_never_answers_by_itself() {
    let g = corpus();
    let mut p = params();
    p.insert("pid".to_string(), Value::Str("proj-99".to_string()));
    let q = parse_statement(
        "MATCH (w:Item) WHERE EXISTS { (w)-[:BELONGS_TO]->(:Proj {id: $pid}) } RETURN w.itemId AS id ORDER BY id",
    )
    .expect("parse");
    g.set_columnar_scans(false);
    let (r, trace) = engram_observe::with_trace(|| run_query(&g, &q, p.clone()).expect("run"));
    g.set_columnar_scans(true);
    assert!(r.rows.is_empty(), "no item belongs to proj-99");
    assert!(count_of(trace.counters(), SEEDED) > 0);
    // Every item of proj-07 is 'open' or 'done'; the status conjunct prunes.
    let open = general(&g, "MATCH (w:Item) WHERE EXISTS { (w)-[:BELONGS_TO]->(:Proj {id: $pid}) } AND w.status = 'done' RETURN count(w) AS n");
    assert_eq!(open, vec![vec![Value::Int(20)]], "fixture: 20 of proj-07's 100 items are done");
    g.set_columnar_scans(false);
    let got = rows(&g, "MATCH (w:Item) WHERE EXISTS { (w)-[:BELONGS_TO]->(:Proj {id: $pid}) } AND w.status = 'done' RETURN count(w) AS n");
    g.set_columnar_scans(true);
    assert_eq!(got, open);
}
