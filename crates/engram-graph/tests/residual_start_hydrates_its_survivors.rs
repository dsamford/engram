#![allow(non_snake_case)]
//! Fix 65: a concluding `properties(n)` RETURN with NO LIMIT binds its
//! seed lean and hydrates the survivors when the single start carries a
//! residual test past its seek — two or more equalities, or any other
//! conjunct reading it. Fix 56 did this for a top-k only; the repository
//! listing (`MATCH (n:UserDataNode {userId: $userId, nodeType:
//! 'repository'}) RETURN properties(n) AS n ORDER BY n.createdAt DESC`)
//! decoded 120 seek candidates in full for its 14 rows on the mirror.
//!
//! Every answer is checked against the same statement with late projection
//! OFF (every seed candidate decoded in full) and against the expectation
//! computed from the fixture.

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
    p.insert("userId".to_string(), Value::Str("u1".into()));
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

/// The statement with late projection off: every seed candidate bound in
/// full, the answer this fix must reproduce byte for byte.
fn eager(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    g.set_late_projection(false);
    let r = rows(g, src);
    g.set_late_projection(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

const RESIDUAL: &str = "interp.stage bound a whole-node output lean for its residual";
const EAGER: &str = "interp.late full carry hydrated eagerly";
const FULL: &str = "graph.nodes materialised in full";

fn item_props(i: i64) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    m.insert("id".into(), s(&format!("item-{i}")));
    m.insert("title".into(), s(&format!("Item {i}")));
    m.insert("nodeType".into(), s(if i % 5 == 0 { "repository" } else { "email" }));
    m.insert("userId".into(), s(&format!("u{}", (i / 5) % 9)));
    // Unique per item, so `ORDER BY createdAt DESC` is a total order.
    m.insert(
        "createdAt".into(),
        s(&format!("2026-08-{:02}T00:{:02}:{:02}Z", 1 + (i / 60) % 28, (i / 60) % 60, i % 60)),
    );
    m
}

/// 600 items, 120 of them repositories spread over nine users (u1 owns
/// 13 or 14); an index on `nodeType` ONLY, so the seek yields every
/// repository and `userId` is the residual. Every item is OWNED_BY its
/// user node.
fn corpus() -> (Graph, Vec<BTreeMap<String, Value>>) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX item_type FOR (n:Item) ON (n.nodeType)");
    let mut users = BTreeMap::new();
    for u in 0..9i64 {
        let mut m = BTreeMap::new();
        m.insert("id".into(), s(&format!("u{u}")));
        users.insert(format!("u{u}"), g.create_node(&["User".into()], &m).expect("user"));
    }
    let mut items = Vec::new();
    for i in 0..600i64 {
        let props = item_props(i);
        let id = g.create_node(&["Item".into()], &props).expect("item");
        let Some(Value::Str(u)) = props.get("userId") else { unreachable!() };
        g.create_rel(id, "OWNED_BY", users[u.as_str()], &BTreeMap::new()).expect("owned");
        items.push(props);
    }
    (g, items)
}

/// u1's repositories as `[properties]` rows, newest first.
fn expected(items: &[BTreeMap<String, Value>]) -> Vec<Vec<Value>> {
    let mut mine: Vec<&BTreeMap<String, Value>> = items
        .iter()
        .filter(|m| m.get("userId") == Some(&s("u1")) && m.get("nodeType") == Some(&s("repository")))
        .collect();
    let created = |m: &BTreeMap<String, Value>| match &m["createdAt"] {
        Value::Str(v) => v.clone(),
        other => panic!("createdAt is a string, not {other:?}"),
    };
    mine.sort_by_key(|m| std::cmp::Reverse(created(m)));
    mine.into_iter().map(|m| vec![Value::Map(m.clone())]).collect()
}

const ORIG: &str = "MATCH (n:Item {userId: $userId, nodeType: 'repository'}) \
    RETURN properties(n) AS n ORDER BY n.createdAt DESC";

#[test]
fn a_two_key_map_start_hydrates_its_survivors_only() {
    let (g, items) = corpus();
    let want = expected(&items);
    assert!(want.len() >= 13 && want.len() <= 14, "{}", want.len());
    assert_eq!(eager(&g, ORIG), want);
    // The index is built on the first probe; count the second run.
    let _ = rows(&g, ORIG);
    let (got, c) = traced(&g, ORIG);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, RESIDUAL), 1, "{c:?}");
    assert_eq!(count_of(&c, EAGER), want.len() as u64, "{c:?}");
    assert_eq!(
        count_of(&c, FULL),
        want.len() as u64,
        "only the survivors are decoded in full, not the 120 candidates: {c:?}"
    );
}

/// The WHERE spelling of the same residual admits the same way; the
/// unordered form too.
#[test]
fn b_a_where_equality_is_a_residual_as_well() {
    let (g, items) = corpus();
    let want = expected(&items);
    let where_form = "MATCH (n:Item {nodeType: 'repository'}) WHERE n.userId = $userId \
        RETURN properties(n) AS n ORDER BY n.createdAt DESC";
    assert_eq!(eager(&g, where_form), want);
    let _ = rows(&g, where_form);
    let (got, c) = traced(&g, where_form);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, RESIDUAL), 1, "{c:?}");
    assert_eq!(count_of(&c, FULL), want.len() as u64, "{c:?}");

    let unordered = "MATCH (n:Item {userId: $userId, nodeType: 'repository'}) RETURN properties(n) AS n";
    let mut want_any = eager(&g, unordered);
    let (mut got, c) = traced(&g, unordered);
    want_any.sort_by_key(|r| format!("{r:?}"));
    got.sort_by_key(|r| format!("{r:?}"));
    assert_eq!(got, want_any);
    assert_eq!(count_of(&c, RESIDUAL), 1, "{c:?}");
    assert_eq!(count_of(&c, FULL), want.len() as u64, "{c:?}");
}

/// Not admitted: a start every candidate of which survives (no residual —
/// a bare label, a sole equality), a hop from the start (one start would
/// hydrate per output row), and the top-k spelling, which keeps fix 56's
/// path (k survivors hydrated, no eager hydration).
#[test]
fn c_no_residual_a_hop_and_a_top_k_stay_as_they_were() {
    let (g, items) = corpus();
    let bare = "MATCH (n:Item) RETURN properties(n) AS n ORDER BY n.createdAt DESC";
    let want = eager(&g, bare);
    assert_eq!(want.len(), 600);
    let (got, c) = traced(&g, bare);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, RESIDUAL), 0, "{c:?}");
    assert_eq!(count_of(&c, EAGER), 0, "{c:?}");
    assert_eq!(count_of(&c, FULL), 600, "{c:?}");

    let sole = "MATCH (n:Item {nodeType: 'repository'}) RETURN properties(n) AS n ORDER BY n.createdAt DESC";
    let want = eager(&g, sole);
    assert_eq!(want.len(), 120);
    let _ = rows(&g, sole);
    let (got, c) = traced(&g, sole);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, RESIDUAL), 0, "{c:?}");
    assert_eq!(count_of(&c, EAGER), 0, "{c:?}");
    assert_eq!(count_of(&c, FULL), 120, "{c:?}");

    let hop = "MATCH (n:Item {userId: $userId, nodeType: 'repository'})-[:OWNED_BY]->(u:User) \
        RETURN properties(n) AS n, u.id AS owner ORDER BY n.createdAt DESC";
    let want: Vec<Vec<Value>> = expected(&items)
        .into_iter()
        .map(|mut r| {
            r.push(s("u1"));
            r
        })
        .collect();
    assert_eq!(eager(&g, hop), want);
    let _ = rows(&g, hop);
    let (got, c) = traced(&g, hop);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, RESIDUAL), 0, "{c:?}");
    assert_eq!(count_of(&c, EAGER), 0, "{c:?}");

    let topk = format!("{ORIG} LIMIT 5");
    let want: Vec<Vec<Value>> = expected(&items).into_iter().take(5).collect();
    assert_eq!(eager(&g, &topk), want);
    let (got, c) = traced(&g, &topk);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, RESIDUAL), 0, "{c:?}");
    assert_eq!(count_of(&c, EAGER), 0, "{c:?}");
    assert_eq!(count_of(&c, FULL), 5, "{c:?}");
}
