#![allow(non_snake_case)]
//! Fix 92: a RETURN's ORDER BY / SKIP / LIMIT that reads a grouping's keys
//! alone moves BELOW the grouping — the conversation listing counted the
//! messages of every one of a user's 1,122 conversations to keep fifty
//! (14 ms against Neo4j's 4.3 on the mirror). The keys are grouped,
//! ordered and paged first, their input multiplicity re-expanded, and the
//! OPTIONAL chain is counted for the page's conversations only.
//!
//! Every page is checked against the same statement's full ordering (a
//! RETURN without LIMIT is never moved), and the moved statement's chain
//! counts are bounded by the page.

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

const PUSHED: &str = "interp.top-k pushed below its grouping stage";
const CHAIN: &str = "interp.count folded a multi-hop chain";

/// One user with 300 conversations — every fifth with a branch of three
/// messages, `updatedAt` on ten distinct days (many ties), conversation 10
/// the newest AND reached by TWO relationships (its count doubles, as it
/// always did).
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut m = BTreeMap::new();
    m.insert("userId".to_string(), Value::Str("user-1".into()));
    let u = g.create_node(&["User".into()], &m).expect("user");
    for i in 0..300i64 {
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
        if i == 10 {
            g.create_rel(u, "HAS_CONVERSATION", c, &BTreeMap::new())
                .expect("has twice");
        }
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

/// Every page is the full ordering's slice — the doubled conversation
/// first with its doubled count — and the chain is counted for the page's
/// conversations alone.
#[test]
fn a_every_page_is_the_full_orderings_slice_with_the_chain_counted_for_the_page_alone() {
    let g = corpus();
    let (full, c) = traced(&g, HEAD, &params(0, 0));
    assert_eq!(full.len(), 300, "fixture");
    assert_eq!(count_of(&c, PUSHED), 0, "no LIMIT: nothing to move: {c:?}");
    assert_eq!(full[0][0], Value::Str("conv-00010".into()), "the newest");
    assert_eq!(
        full[0][4],
        Value::Int(6),
        "two relationships double the count, as they always did"
    );
    let paged = format!("{HEAD} SKIP toInteger($offset) LIMIT toInteger($limit)");
    for (limit, offset) in [(50, 0), (50, 50), (7, 293), (1, 0), (300, 0), (10, 295)] {
        let (got, c) = traced(&g, &paged, &params(limit, offset));
        let want: Vec<Vec<Value>> = full
            .iter()
            .skip(offset as usize)
            .take(limit as usize)
            .cloned()
            .collect();
        assert_eq!(got, want, "limit {limit} offset {offset}");
        assert_eq!(
            count_of(&c, PUSHED),
            1,
            "limit {limit} offset {offset}: {c:?}"
        );
        // The page's conversations, plus the doubled one's second row when
        // it is on the page — never the 300.
        let bound = (offset + limit).min(300) as u64 + 1;
        assert!(
            count_of(&c, CHAIN) <= bound,
            "limit {limit} offset {offset}: {} chain counts, bound {bound}: {c:?}",
            count_of(&c, CHAIN)
        );
    }
    // Through an alias of the key's property.
    let by_alias = format!(
        "{} SKIP toInteger($offset) LIMIT toInteger($limit)",
        HEAD.replace("ORDER BY c.updatedAt DESC", "ORDER BY updatedAt DESC")
    );
    let (got, c) = traced(&g, &by_alias, &params(50, 0));
    assert_eq!(got, full[..50].to_vec());
    assert_eq!(count_of(&c, PUSHED), 1, "{c:?}");
}

/// CONTROLS, each left as written (and still right): an ORDER BY over the
/// aggregate, a plain MATCH before the grouping, a WHERE on the grouping,
/// an OPTIONAL clause reading the user, a RETURN DISTINCT, a RETURN alias
/// shadowing the key.
#[test]
fn b_an_aggregate_order_a_plain_match_a_having_a_scope_read_a_distinct_and_a_shadow_stay() {
    let g = corpus();
    let base = "MATCH (u:User {userId: $userId})-[:HAS_CONVERSATION]->(c:AssistantConversation) \
        OPTIONAL MATCH (c)-[:HAS_BRANCH]->()-[:HAS_MESSAGE]->(m) WITH c, count(m) AS messageCount \
        RETURN c.conversationId AS conversationId, messageCount ORDER BY c.updatedAt DESC LIMIT toInteger($limit)";
    let (moved, c) = traced(&g, base, &params(20, 0));
    assert_eq!(count_of(&c, PUSHED), 1, "the base moves: {c:?}");
    assert_eq!(moved.len(), 20);

    let by_agg = base.replace(
        "ORDER BY c.updatedAt DESC",
        "ORDER BY messageCount DESC, c.conversationId",
    );
    let (got, c) = traced(&g, &by_agg, &params(3, 0));
    assert_eq!(count_of(&c, PUSHED), 0, "{c:?}");
    assert_eq!(got[0], vec![Value::Str("conv-00010".into()), Value::Int(6)]);
    assert_eq!(got[1], vec![Value::Str("conv-00000".into()), Value::Int(3)]);

    let plain = base.replace(
        "OPTIONAL MATCH (c)-[:HAS_BRANCH]->()",
        "MATCH (c)-[:HAS_BRANCH]->()",
    );
    let (got, c) = traced(&g, &plain, &params(100, 0));
    assert_eq!(count_of(&c, PUSHED), 0, "{c:?}");
    assert_eq!(got.len(), 60, "the sixty conversations with a branch");

    let having = base.replace(
        "AS messageCount RETURN",
        "AS messageCount WHERE messageCount > 0 RETURN",
    );
    let (got, c) = traced(&g, &having, &params(100, 0));
    assert_eq!(count_of(&c, PUSHED), 0, "{c:?}");
    assert_eq!(got.len(), 60);

    let reads_u = base.replace(
        "OPTIONAL MATCH (c)-[:HAS_BRANCH]->()",
        "OPTIONAL MATCH (u)-[:HAS_CONVERSATION]->(c)-[:HAS_BRANCH]->()",
    );
    let (got, c) = traced(&g, &reads_u, &params(20, 0));
    assert_eq!(count_of(&c, PUSHED), 0, "{c:?}");
    assert_eq!(got.len(), 20);

    let distinct = base.replace(
        "RETURN c.conversationId AS conversationId, messageCount ORDER BY c.updatedAt DESC",
        "RETURN DISTINCT c.conversationId AS conversationId, c.updatedAt AS updatedAt, messageCount ORDER BY updatedAt DESC",
    );
    let (got, c) = traced(&g, &distinct, &params(20, 0));
    assert_eq!(count_of(&c, PUSHED), 0, "{c:?}");
    assert_eq!(got.len(), 20);

    let shadow = base.replace(
        "c.conversationId AS conversationId, messageCount ORDER BY c.updatedAt DESC",
        "c.conversationId AS c, messageCount ORDER BY c DESC",
    );
    let (got, c) = traced(&g, &shadow, &params(2, 0));
    assert_eq!(count_of(&c, PUSHED), 0, "{c:?}");
    assert_eq!(
        got[0][0],
        Value::Str("conv-00299".into()),
        "ordered by the alias, as written"
    );
}
