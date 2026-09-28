#![allow(non_snake_case)]
//! Fix 99: an aggregating top-k breaker whose keys are bare match-bound
//! variables — the grouping fix 92 pushes a listing's page below — carries
//! its keys LEAN (the ORDER BY's properties and the aggregates' arguments
//! bound per row) and hydrates the page's groups alone. The conversation
//! listing bound four properties of each of 1,122 conversations to page
//! fifty by one of them (12.3 ms against Neo4j's 4.1 on the mirror).
//!
//! Every page is checked against the same statement's full ordering; the
//! column reads per row drop to the key's one property.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params(limit: i64, offset: i64) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("userId".to_string(), Value::Str("user-1".into()));
    p.insert("limit".to_string(), Value::Int(limit));
    p.insert("offset".to_string(), Value::Int(offset));
    p
}

fn rows(g: &Graph, src: &str, p: &BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, p.clone())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(
    g: &Graph,
    src: &str,
    p: &BTreeMap<String, Value>,
) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src, p));
    (r, trace.counters().clone())
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const PAGED: &str = "interp.grouping top-k hydrated its page's keys alone";
const PUSHED: &str = "interp.top-k pushed below its grouping stage";
const COLUMN_SERVED: &str = "graph.property column served";

/// One user with 400 conversations (every fifth with a branch of three
/// messages), `updatedAt` on ten days; conversation 10 is the newest.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut m = BTreeMap::new();
    m.insert("userId".to_string(), Value::Str("user-1".into()));
    let u = g.create_node(&["User".into()], &m).expect("user");
    for i in 0..400i64 {
        let mut m = BTreeMap::new();
        m.insert(
            "conversationId".to_string(),
            Value::Str(format!("conv-{i:05}")),
        );
        m.insert("title".to_string(), Value::Str(format!("Title {i}")));
        m.insert(
            "createdAt".to_string(),
            Value::Str(format!("2026-08-{:02}T00:00:00Z", 1 + (i % 28))),
        );
        let day = if i == 10 { 30 } else { 1 + (i % 10) };
        m.insert(
            "updatedAt".to_string(),
            Value::Str(format!("2026-09-{day:02}T00:00:00Z")),
        );
        let c = g
            .create_node(&["AssistantConversation".into()], &m)
            .expect("conv");
        g.create_rel(u, "HAS_CONVERSATION", c, &BTreeMap::new())
            .expect("has");
        if i % 5 == 0 {
            let b = g
                .create_node(&["AssistantBranch".into()], &BTreeMap::new())
                .expect("branch");
            g.create_rel(c, "HAS_BRANCH", b, &BTreeMap::new())
                .expect("hb");
            for j in 0..3i64 {
                let mut mm = BTreeMap::new();
                mm.insert("content".to_string(), Value::Str(format!("m{j}")));
                let msg = g
                    .create_node(&["AssistantMessage".into()], &mm)
                    .expect("msg");
                g.create_rel(b, "HAS_MESSAGE", msg, &BTreeMap::new())
                    .expect("hm");
            }
        }
    }
    g
}

const HEAD: &str = "MATCH (u:User {userId: $userId})-[:HAS_CONVERSATION]->(c:AssistantConversation) \
    OPTIONAL MATCH (c)-[:HAS_BRANCH]->()-[:HAS_MESSAGE]->(m) WITH c, count(m) AS messageCount \
    RETURN c.conversationId AS conversationId, c.title AS title, c.createdAt AS createdAt, c.updatedAt AS updatedAt, messageCount \
    ORDER BY c.updatedAt DESC";

/// Every page is the full ordering's slice; the pushed grouping binds one
/// property per conversation and hydrates the page's three others.
#[test]
fn a_every_page_is_the_slice_and_the_keys_are_hydrated_for_the_page_alone() {
    let g = corpus();
    let full = rows(&g, HEAD, &params(0, 0));
    assert_eq!(full.len(), 400, "fixture");
    assert_eq!(full[0][0], Value::Str("conv-00010".into()));
    let paged = format!("{HEAD} SKIP toInteger($offset) LIMIT toInteger($limit)");
    for (limit, offset) in [(50, 0), (50, 50), (7, 393), (1, 0)] {
        let (got, c) = traced(&g, &paged, &params(limit, offset));
        let want: Vec<Vec<Value>> = full
            .iter()
            .skip(offset as usize)
            .take(limit as usize)
            .cloned()
            .collect();
        assert_eq!(got, want, "limit {limit} offset {offset}");
        assert_eq!(count_of(&c, PUSHED), 1, "{c:?}");
        assert_eq!(
            count_of(&c, PAGED),
            1,
            "limit {limit} offset {offset}: {c:?}"
        );
        // One column per conversation for the key, the page's three
        // others by a projected read each — never four per conversation.
        let served = count_of(&c, COLUMN_SERVED);
        assert!(
            served <= 400 + 3 * (offset + limit) as u64 + 8,
            "limit {limit} offset {offset}: {served} column reads: {c:?}"
        );
        assert!(
            count_of(&c, "graph.projected node materialisations") >= (offset + limit) as u64,
            "{c:?}"
        );
    }
}

/// CONTROL: an aggregating concluding RETURN whose bare key IS the output
/// leaves in FULL — every property, not the id alone (node equality is by
/// id, so only a property read pins it).
#[test]
fn c_a_bare_key_output_of_an_aggregating_return_leaves_in_full() {
    let g = corpus();
    let (got, c) = traced(
        &g,
        "MATCH (u:User {userId: $userId})-[:HAS_CONVERSATION]->(c:AssistantConversation) \
         RETURN c, count(*) AS n ORDER BY c.updatedAt DESC LIMIT toInteger($limit)",
        &params(3, 0),
    );
    assert_eq!(got.len(), 3);
    let Value::Node { props, .. } = &got[0][0] else {
        panic!("{:?}", got[0][0])
    };
    assert_eq!(
        props.get("conversationId"),
        Some(&Value::Str("conv-00010".into())),
        "{props:?}"
    );
    assert_eq!(
        props.get("title"),
        Some(&Value::Str("Title 10".into())),
        "{props:?}"
    );
    assert_eq!(
        props.len(),
        4,
        "every property, not the key alone: {props:?}"
    );
    assert_eq!(count_of(&c, PAGED), 0, "{c:?}");
}

/// CONTROL: a RETURN that reads only the key's ORDER BY property has
/// nothing to defer — the grouping pages without a hydration.
#[test]
fn b_a_return_reading_only_the_key_property_has_nothing_to_hydrate() {
    let g = corpus();
    let src = "MATCH (u:User {userId: $userId})-[:HAS_CONVERSATION]->(c:AssistantConversation) \
        OPTIONAL MATCH (c)-[:HAS_BRANCH]->()-[:HAS_MESSAGE]->(m) WITH c, count(m) AS messageCount \
        RETURN c.updatedAt AS updatedAt, messageCount ORDER BY c.updatedAt DESC LIMIT toInteger($limit)";
    let (got, c) = traced(&g, src, &params(20, 0));
    assert_eq!(got.len(), 20);
    assert_eq!(
        got[0],
        vec![Value::Str("2026-09-30T00:00:00Z".into()), Value::Int(3)]
    );
    assert_eq!(count_of(&c, PUSHED), 1, "{c:?}");
    assert_eq!(count_of(&c, PAGED), 0, "{c:?}");
}
