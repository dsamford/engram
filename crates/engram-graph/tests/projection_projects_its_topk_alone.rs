#![allow(non_snake_case)]
//! Fix 91: a RETURN with ORDER BY and a LIMIT evaluated every item of every
//! row before the page kept its k — the conversation listing's aggregating
//! tail projected five properties of each of one user's 1,122 conversations
//! to keep fifty (10 expressions a row, 14 ms against Neo4j's 4.3 on the
//! mirror). When every ORDER BY expression reads the input row alone, the
//! rows are keyed first, the `skip + limit` best kept by the bounded heap,
//! and only those are projected — in the order the full sort would have
//! placed them.
//!
//! Every page is checked against the same statement's full ordering.

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

const TOPK: &str = "interp.top-k keyed its rows and projected the survivors alone";
const EXPRS: &str = "cypher.expressions evaluated";

/// One user with 1,200 conversations (a fifth with a branch of three
/// messages), `updatedAt` spread so that ties exist.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut m = BTreeMap::new();
    m.insert("userId".to_string(), Value::Str("user-1".into()));
    let u = g.create_node(&["User".into()], &m).expect("user");
    for i in 0..1200i64 {
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
        m.insert(
            "updatedAt".to_string(),
            Value::Str(format!(
                "2026-09-{:02}T{:02}:00:00Z",
                1 + (i % 28),
                (i / 28) % 24
            )),
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

// The listing with a HAVING on its grouping, so the page stays the
// RETURN's own top-k — fix 92 moves the plain listing's page below its
// grouping instead, and a non-aggregating stage defers the page's
// properties already (fix 56).
const HEAD: &str = "MATCH (u:User {userId: $userId})-[:HAS_CONVERSATION]->(c:AssistantConversation) \
    OPTIONAL MATCH (c)-[:HAS_BRANCH]->()-[:HAS_MESSAGE]->(m) WITH c, count(m) AS messageCount WHERE messageCount >= 0 \
    RETURN c.conversationId AS conversationId, c.title AS title, c.createdAt AS createdAt, c.updatedAt AS updatedAt, messageCount \
    ORDER BY c.updatedAt DESC, c.conversationId";

/// The listing's page is the full ordering's slice, and the items are
/// projected for the fifty kept rows, not the 1,200.
#[test]
fn a_the_page_is_the_full_orderings_slice_projected_for_its_rows_alone() {
    let g = corpus();
    let full = rows(&g, HEAD, &params(0, 0));
    assert_eq!(full.len(), 1200, "fixture");
    let paged = format!("{HEAD} SKIP toInteger($offset) LIMIT toInteger($limit)");
    // The page that keeps every row projects every row: the baseline.
    let (all, c) = traced(&g, &paged, &params(1200, 0));
    assert_eq!(all, full);
    assert_eq!(count_of(&c, TOPK), 0, "a page that keeps every row: {c:?}");
    let baseline = count_of(&c, EXPRS);
    for (limit, offset) in [(50, 0), (50, 50), (7, 1193), (1, 0)] {
        let (got, c) = traced(&g, &paged, &params(limit, offset));
        let want: Vec<Vec<Value>> = full
            .iter()
            .skip(offset as usize)
            .take(limit as usize)
            .cloned()
            .collect();
        assert_eq!(got, want, "limit {limit} offset {offset}");
        let unkept = 1200 - (offset + limit) as u64;
        if unkept > 0 {
            assert_eq!(
                count_of(&c, TOPK),
                1,
                "limit {limit} offset {offset}: {c:?}"
            );
            // The keys for every row, the five items for the kept rows, and
            // the rest of the statement — the five items of every unkept
            // conversation are never evaluated.
            assert!(
                count_of(&c, EXPRS) + 4 * unkept <= baseline,
                "limit {limit} offset {offset}: {} expressions against {baseline} for the whole: {c:?}",
                count_of(&c, EXPRS)
            );
        } else {
            assert_eq!(count_of(&c, TOPK), 0, "a page that keeps every row: {c:?}");
        }
    }
}

/// CONTROLS: an ORDER BY over an output alias needs the items first; an
/// unordered LIMIT and a DISTINCT keep the one pass. Each answers the full
/// ordering's slice (or the same set) as before.
#[test]
fn b_an_alias_order_an_unordered_limit_and_distinct_keep_the_one_pass() {
    let g = corpus();
    let by_alias = "MATCH (u:User {userId: $userId})-[:HAS_CONVERSATION]->(c:AssistantConversation) \
        WITH c, 0 AS messageCount \
        RETURN c.conversationId AS conversationId, c.updatedAt AS updatedAt, messageCount \
        ORDER BY updatedAt DESC, conversationId LIMIT toInteger($limit)";
    let full = rows(
        &g,
        &by_alias.replace(" LIMIT toInteger($limit)", ""),
        &params(0, 0),
    );
    let (got, c) = traced(&g, by_alias, &params(20, 0));
    assert_eq!(got, full[..20].to_vec());
    assert_eq!(count_of(&c, TOPK), 0, "{c:?}");
    // The same alias order spelt over the node: keyed first.
    let by_node = by_alias.replace(
        "ORDER BY updatedAt DESC, conversationId",
        "ORDER BY c.updatedAt DESC, c.conversationId",
    );
    let (got, c) = traced(&g, &by_node, &params(20, 0));
    assert_eq!(got, full[..20].to_vec());
    assert_eq!(count_of(&c, TOPK), 1, "{c:?}");
    let unordered = "MATCH (u:User {userId: $userId})-[:HAS_CONVERSATION]->(c:AssistantConversation) \
        WITH c, 0 AS z RETURN c.conversationId AS id, z LIMIT toInteger($limit)";
    let (got, c) = traced(&g, unordered, &params(20, 0));
    assert_eq!(got.len(), 20);
    assert_eq!(count_of(&c, TOPK), 0, "{c:?}");
    let distinct = "MATCH (u:User {userId: $userId})-[:HAS_CONVERSATION]->(c:AssistantConversation) \
        WITH c, 0 AS z RETURN DISTINCT c.createdAt AS d, z ORDER BY d LIMIT toInteger($limit)";
    let (got, c) = traced(&g, distinct, &params(5, 0));
    assert_eq!(got.len(), 5);
    assert_eq!(count_of(&c, TOPK), 0, "{c:?}");
}
