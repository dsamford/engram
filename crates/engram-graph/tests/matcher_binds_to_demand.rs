#![allow(non_snake_case)]
//! Fix 51: the per-row matcher (`match_path` — every OPTIONAL MATCH of the
//! clause executor, every EXISTS / COUNT body the fast probe declines, every
//! pattern comprehension) binds each hop end to the properties the rest of
//! the statement or the subquery body READS of it, instead of the full
//! record; a bound start with no inline map is the row's own value; an
//! anonymous hop walks adjacency keys alone.
//!
//! The production KMProject dashboard listing (`MATCH (p:KMProject) WHERE
//! true OPTIONAL MATCH (:User {userId: $u})-[mm:MEMBER_OF]->(p) OPTIONAL
//! MATCH (w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) WITH p, mm,
//! max(w.updatedAt) AS lastItemAt RETURN properties(p), … 8 COUNT {} … 2
//! comprehensions`) materialised 34,869 nodes and 16,780 relationships IN
//! FULL per statement on the mirror — 1.1 s against Neo4j's 24 ms.
//!
//! Every answer is checked against the same statement with the demand
//! switched off (columnar paths off keeps the executor; the demand cannot be
//! switched off, so the oracle is the values themselves, computed from the
//! fixture) and a bare use of the same variable, which still comes in full.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("u".to_string(), Value::Str("user-3".to_string()));
    p.insert("windowStart".to_string(), Value::Str("2026-08-20".to_string()));
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
const FULL_RELS: &str = "graph.rels materialised in full";
const BOUND: &str = "interp.matcher bound a hop end to its demand";
const REUSED: &str = "interp.matcher reused the bound start";

/// 20 projects, 4,000 items (200 per project) with a FAT payload property,
/// 5 users each a member of 4 projects (with a `state` on the edge), and
/// item i done when i % 5 == 0.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut projs = Vec::new();
    for i in 0..20i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Str(format!("proj-{i:02}")));
        m.insert("name".to_string(), Value::Str(format!("project {i}")));
        m.insert("color".to_string(), Value::Str("blue".to_string()));
        projs.push(g.create_node(&["Proj".into()], &m).expect("proj"));
    }
    let mut users = Vec::new();
    for i in 0..5i64 {
        let mut m = BTreeMap::new();
        m.insert("userId".to_string(), Value::Str(format!("user-{i}")));
        m.insert("displayName".to_string(), Value::Str(format!("User {i}")));
        users.push(g.create_node(&["User".into()], &m).expect("user"));
    }
    for (ui, u) in users.iter().enumerate() {
        for k in 0..4usize {
            let p = projs[(ui * 4 + k) % 20];
            let mut m = BTreeMap::new();
            m.insert("role".to_string(), Value::Str(if k == 0 { "owner" } else { "member" }.to_string()));
            if k == 3 {
                m.insert("state".to_string(), Value::Str("inactive".to_string()));
            }
            g.create_rel(*u, "MEMBER_OF", p, &m).expect("member");
        }
    }
    for i in 0..4000i64 {
        let mut m = BTreeMap::new();
        m.insert("itemId".to_string(), Value::Str(format!("item-{i:04}")));
        m.insert("updatedAt".to_string(), Value::Str(format!("2026-08-{:02}T{:02}:00:00Z", 1 + (i % 28), i % 24)));
        m.insert("status".to_string(), Value::Str(if i % 5 == 0 { "done" } else { "open" }.to_string()));
        m.insert("completedAt".to_string(), if i % 5 == 0 { Value::Str(format!("2026-08-{:02}", 10 + (i % 20))) } else { Value::Null });
        m.insert("payload".to_string(), Value::Str("x".repeat(400)));
        let w = g.create_node(&["Item".into()], &m).expect("item");
        g.create_rel(w, "BELONGS_TO", projs[(i % 20) as usize], &BTreeMap::new())
            .expect("belongs");
    }
    g
}

/// The production shape: an OPTIONAL MATCH read for ONE property through an
/// aggregating WITH, then a RETURN of COUNT {} subqueries and comprehensions
/// over the group key. Every count and value is known from the fixture.
#[test]
fn the_dashboard_listing_reads_one_property_per_item_not_the_record() {
    let g = corpus();
    let src = "MATCH (p:Proj) WHERE true \
        OPTIONAL MATCH (:User {userId: $u})-[mm:MEMBER_OF]->(p) \
        OPTIONAL MATCH (w:Item)-[:BELONGS_TO]->(p) \
        WITH p, mm, max(w.updatedAt) AS lastItemAt \
        RETURN p.id AS id, coalesce(mm.role, 'none') AS myRole, lastItemAt, \
               COUNT { (:Item)-[:BELONGS_TO]->(p) } AS itemCount, \
               COUNT { (wi:Item)-[:BELONGS_TO]->(p) WHERE NOT wi.status IN ['done'] } AS openCount, \
               COUNT { (:User)-[am:MEMBER_OF]->(p) WHERE coalesce(am.state, 'active') = 'active' } AS memberCount, \
               [ (iw:Item)-[:BELONGS_TO]->(p) WHERE iw.completedAt >= $windowStart | iw.itemId ][..2] AS recent \
        ORDER BY id";
    let (got, c) = traced(&g, src);
    assert_eq!(got.len(), 20);
    // proj-12 = user-3's owner project (ui 3, k 0 → proj 12); proj-15 its inactive one.
    let by_id: BTreeMap<String, Vec<Value>> = got
        .iter()
        .map(|r| match &r[0] {
            Value::Str(s) => (s.clone(), r.clone()),
            other => panic!("id column: {other:?}"),
        })
        .collect();
    let p12 = &by_id["proj-12"];
    assert_eq!(p12[1], Value::Str("owner".into()), "myRole");
    // Items of proj-12: i ≡ 12 (mod 20) → 200 items; 40 done (i % 5 == 0 ⇔ i ≡ 0 mod 5;
    // i = 12 + 20k, 12 + 20k ≡ 2 (mod 5) → NONE are done).
    assert_eq!(p12[3], Value::Int(200), "itemCount");
    assert_eq!(p12[4], Value::Int(200), "openCount (no item of proj-12 is done)");
    // Members of proj-12: user-3 (owner, active) only … plus whoever else lands on 12:
    // ui*4+k ≡ 12 (mod 20): (3,0) only → 1.
    assert_eq!(p12[5], Value::Int(1), "memberCount");
    // proj-00: i ≡ 0 (mod 20) → every item i % 5 == 0 → all 200 done.
    let p0 = &by_id["proj-00"];
    assert_eq!(p0[3], Value::Int(200));
    assert_eq!(p0[4], Value::Int(0), "openCount (every item of proj-00 is done)");
    // user-3's projects are (3*4+k) % 20 ∈ {12,13,14,15}: no edge to proj-00.
    assert_eq!(p0[1], Value::Str("none".into()), "myRole for a project $u is not in");
    // proj-15 is user-3's INACTIVE membership (k = 3): the role rides on the
    // edge regardless, the active-member count excludes it.
    let p15 = &by_id["proj-15"];
    assert_eq!(p15[1], Value::Str("member".into()));
    assert_eq!(p15[5], Value::Int(0), "the inactive membership is not counted");
    // The demand: `w` is read for `updatedAt` only, the COUNT {} ends for
    // their labels (and `status` / `state`), the comprehension for
    // `completedAt` / `itemId`. Nothing is read in full: 4,000 fat items ×
    // (1 OPTIONAL + 3 subqueries) would be 16,000 full records.
    assert!(count_of(&c, BOUND) > 0, "hop ends bound to their demand: {c:?}");
    assert!(count_of(&c, REUSED) > 0, "the bound project reused: {c:?}");
    // The executor reads each PROJECT in full a few times (the seed, the
    // group key); no fat ITEM record ever — 16,000 projections, not 16,000
    // full decodes.
    assert!(
        count_of(&c, FULL) <= 3 * 20,
        "no fat item record read in full (the 20 projects a few times at most): {c:?}"
    );
    // Fix 70: the label-only `COUNT { (:Item)-[:BELONGS_TO]->(p) }` bodies
    // are answered from adjacency and membership alone, column-at-a-time
    // (4,000 of the 16,000 ends never bound at all); the ends whose status
    // / completedAt / itemId are read stay projected — their columns are
    // not cached here, so the matcher binds them.
    assert!(count_of(&c, "graph.projected node materialisations") >= 12_000, "{c:?}");
    assert!(count_of(&c, "interp.subquery hop evaluated column-at-a-time") >= 20, "{c:?}");
    assert!(
        count_of(&c, FULL_RELS) <= 60,
        "only the named MEMBER_OF edges (their props are read), never the 4,000 anonymous BELONGS_TO: {c:?}"
    );
}

/// A bare use keeps the full record: `RETURN w` / `properties(w)` / a later
/// pattern reading `w` in full sees every property, and the anonymous hop
/// still walks adjacency keys.
#[test]
fn a_bare_use_still_comes_in_full() {
    let g = corpus();
    let full = rows(&g, "MATCH (p:Proj {id: 'proj-07'}) OPTIONAL MATCH (w:Item)-[:BELONGS_TO]->(p) WITH w ORDER BY w.itemId LIMIT 2 RETURN properties(w) AS w");
    assert_eq!(full.len(), 2);
    let Value::Map(m) = &full[0][0] else {
        panic!("properties(w): {:?}", full[0][0]);
    };
    assert_eq!(m.get("itemId"), Some(&Value::Str("item-0007".into())));
    assert_eq!(m.get("payload").map(|v| matches!(v, Value::Str(s) if s.len() == 400)), Some(true), "the fat payload is there");
    // An open item has no completedAt (a null property is absent): four properties.
    assert_eq!(m.len(), 4, "every property: {m:?}");
    assert!(m.get("completedAt").is_none());
    // A property read after a bare carry: full, and correct.
    let got = rows(&g, "MATCH (p:Proj {id: 'proj-07'}) OPTIONAL MATCH (w:Item)-[:BELONGS_TO]->(p) WITH p, w ORDER BY w.itemId LIMIT 3 RETURN w.itemId AS id, w.status AS s, size(w.payload) AS n");
    assert_eq!(got.len(), 3);
    assert_eq!(got[0], vec![Value::Str("item-0007".into()), Value::Str("open".into()), Value::Int(400)]);
    assert_eq!(got[1], vec![Value::Str("item-0027".into()), Value::Str("open".into()), Value::Int(400)]);
}

/// The demanded properties are exactly what a later clause reads — a WITH
/// that carries the node forward for a property the RETURN reads gets it.
#[test]
fn a_property_read_two_clauses_later_is_in_the_demand() {
    let g = corpus();
    let src = "MATCH (p:Proj {id: 'proj-07'}) OPTIONAL MATCH (w:Item)-[:BELONGS_TO]->(p) WITH p, w WHERE w.status = 'open' WITH p, w ORDER BY w.itemId WITH p, collect(w) AS ws RETURN p.id AS id, size(ws) AS n, [x IN ws | x.updatedAt][0] AS firstAt";
    let got = rows(&g, src);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0][0], Value::Str("proj-07".into()));
    // proj-07: i ≡ 7 (mod 20) → i % 5 == 2 → none done → 200 open.
    assert_eq!(got[0][1], Value::Int(200));
    assert_eq!(got[0][2], Value::Str("2026-08-08T07:00:00Z".into()), "item-0007's updatedAt");
    // A pattern reusing `w` later is an identity use; the far end's map keys are read.
    let src2 = "MATCH (p:Proj {id: 'proj-07'}) OPTIONAL MATCH (w:Item)-[:BELONGS_TO]->(p) WITH w ORDER BY w.itemId LIMIT 1 MATCH (w)-[:BELONGS_TO]->(q:Proj {color: 'blue'}) RETURN q.id AS q, w.itemId AS w";
    assert_eq!(
        rows(&g, src2),
        vec![vec![Value::Str("proj-07".into()), Value::Str("item-0007".into())]]
    );
}
