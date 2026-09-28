#![allow(non_snake_case)]
//! Fix 83: a hop whose start fans out to a large share of the far end's
//! label bound every end from a projected record read whenever the label's
//! columns were not cached — nothing had read the label whole. The
//! conversation listing (`MATCH (u:User {userId: $userId})-[:HAS_CONVERSATION]
//! ->(c:AssistantConversation) OPTIONAL MATCH (c)-[:HAS_BRANCH]->()-[:HAS_MESSAGE]
//! ->(m) WITH c, count(m) AS messageCount RETURN c.conversationId, c.title, …
//! ORDER BY c.updatedAt DESC SKIP … LIMIT …`) paid 1,123 projected gets for
//! one user's 1,122 conversations of the label's ~1,300 (20.8 ms on the
//! mirror against Neo4j's 4.8). Now, before such a start's adjacency is
//! expanded, the far end's demanded columns are read whole and kept, and
//! every end binds from them (fix 60's path). The bounds are the columnar
//! population read's (fix 78): a fan-out of at least an eighth of the
//! label, a label of at most 262,144 members — and at least 64 ends, below
//! which the projected reads are cheaper than a column's fixed cost.
//!
//! Every answer is checked against the same statement with the columnar
//! paths OFF.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params(user: &str, limit: i64) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("userId".to_string(), Value::Str(user.to_string()));
    p.insert("limit".to_string(), Value::Int(limit));
    p.insert("offset".to_string(), Value::Int(0));
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

fn general(g: &Graph, src: &str, p: &BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let r = rows(g, src, p);
    g.set_columnar_scans(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const WARMED: &str = "interp.matcher warmed a hop end label's columns for a wide fan-out";
const WARMED_MISSES: &str = "interp.matcher warmed a hop end label's columns after repeated misses";
const COLUMNS: &str = "interp.matcher bound a hop end from the label's cached columns";
const PROJECTED: &str = "store.projected gets";

/// Three users; 1,300 conversations, the first 1,100 one user's, the rest
/// spread; every fourth conversation has a branch of three messages.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut users = Vec::new();
    for k in 0..3i64 {
        let mut m = BTreeMap::new();
        m.insert("userId".to_string(), Value::Str(format!("user-{k}")));
        users.push(g.create_node(&["User".into()], &m).expect("user"));
    }
    for i in 0..1300i64 {
        let mut m = BTreeMap::new();
        m.insert(
            "conversationId".to_string(),
            Value::Str(format!("conv-{i:05}")),
        );
        m.insert("title".to_string(), Value::Str(format!("Title {i}")));
        m.insert(
            "createdAt".to_string(),
            Value::Str(format!("2026-08-{:02}T{:02}:00:00Z", 1 + (i % 28), i % 24)),
        );
        m.insert(
            "updatedAt".to_string(),
            Value::Str(format!(
                "2026-09-{:02}T{:02}:{:02}:00Z",
                1 + (i % 28),
                i % 24,
                i % 60
            )),
        );
        m.insert("blob".to_string(), Value::Str("b".repeat(400)));
        let c = g
            .create_node(&["AssistantConversation".into()], &m)
            .expect("conv");
        // The rest: forty to user-0 (under fix 87's sixty-four-miss floor),
        // the others to user-2.
        let owner = if i < 1100 {
            users[1]
        } else if i % 5 == 0 {
            users[0]
        } else {
            users[2]
        };
        g.create_rel(owner, "HAS_CONVERSATION", c, &BTreeMap::new())
            .expect("has");
        if i % 4 == 0 {
            let b = g
                .create_node(&["AssistantBranch".into()], &BTreeMap::new())
                .expect("branch");
            g.create_rel(c, "HAS_BRANCH", b, &BTreeMap::new())
                .expect("hb");
            for j in 0..3i64 {
                let mut mm = BTreeMap::new();
                mm.insert("content".to_string(), Value::Str(format!("m{j}")));
                mm.insert("timestamp".to_string(), Value::Int(j));
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

const LISTING: &str = "MATCH (u:User {userId: $userId})-[:HAS_CONVERSATION]->(c:AssistantConversation) \
    OPTIONAL MATCH (c)-[:HAS_BRANCH]->()-[:HAS_MESSAGE]->(m) WITH c, count(m) AS messageCount \
    RETURN c.conversationId AS conversationId, c.title AS title, c.createdAt AS createdAt, c.updatedAt AS updatedAt, messageCount \
    ORDER BY c.updatedAt DESC SKIP toInteger($offset) LIMIT toInteger($limit)";

const LAST_CONTENT: &str = "MATCH (u:User {userId: $userId})-[:HAS_CONVERSATION]->(c:AssistantConversation) \
    WITH c ORDER BY c.updatedAt DESC LIMIT toInteger($limit) \
    OPTIONAL MATCH (c)-[:HAS_BRANCH]->()-[:HAS_MESSAGE]->(m:AssistantMessage) \
    WITH c, m ORDER BY m.timestamp DESC WITH c, head(collect(m.content)) AS lastContent \
    RETURN c.conversationId AS conversationId, c.title AS title, c.updatedAt AS updatedAt, lastContent ORDER BY c.updatedAt DESC";

/// The wide user's listing warms the label's four columns on its FIRST run
/// and binds every conversation from them — no projected read per end.
#[test]
fn a_a_wide_fan_out_warms_the_far_ends_columns_and_binds_from_them() {
    let g = corpus();
    let p = params("user-1", 50);
    let want = general(&g, LISTING, &p);
    assert_eq!(want.len(), 50, "fixture");
    let (got, c) = traced(&g, LISTING, &p);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, WARMED), 1, "{c:?}");
    assert!(count_of(&c, COLUMNS) >= 1_100, "{c:?}");
    assert!(count_of(&c, PROJECTED) < 64, "{c:?}");
    // Warm already: the next run binds from the cache without warming.
    let (got, c) = traced(&g, LISTING, &p);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, WARMED), 0, "{c:?}");
    assert!(count_of(&c, COLUMNS) >= 1_100, "{c:?}");
}

/// A narrow fan-out — one user's 40 conversations, under an eighth of the
/// label and under fix 87's sixty-four-miss floor — keeps the projected
/// read per end.
#[test]
fn b_a_narrow_fan_out_keeps_the_projected_reads() {
    let g = corpus();
    let p = params("user-0", 50);
    let want = general(&g, LISTING, &p);
    assert_eq!(want.len(), 40, "fixture");
    let (got, c) = traced(&g, LISTING, &p);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, WARMED), 0, "{c:?}");
    assert_eq!(count_of(&c, WARMED_MISSES), 0, "{c:?}");
    assert_eq!(count_of(&c, COLUMNS), 0, "{c:?}");
    // Fix 99: the pushed grouping binds each end on its key property alone
    // and hydrates the page's three others by a projected read each — a
    // page that covers every end reads twice per end.
    assert!((75..95).contains(&count_of(&c, PROJECTED)), "{c:?}");
}

/// The last-content listing orders the wide user's conversations by
/// `updatedAt` before its top eight: the same warm, the same rows.
#[test]
fn c_the_last_content_listing_warms_too() {
    let g = corpus();
    let p = params("user-1", 8);
    let want = general(&g, LAST_CONTENT, &p);
    assert_eq!(want.len(), 8, "fixture");
    let (got, c) = traced(&g, LAST_CONTENT, &p);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, WARMED), 1, "{c:?}");
    assert!(count_of(&c, COLUMNS) >= 1_100, "{c:?}");
}

/// A fan-out past the batch floor but under an eighth of a big label is
/// not warmed for its FAN-OUT — 100 ends of 10,000 — but the statement's
/// sixty-fourth miss warms it anyway (fix 87): sixty-four projected reads,
/// then the column.
#[test]
fn d_a_fan_out_under_an_eighth_of_a_big_label_is_not_warmed_for_its_fan_out() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut m = BTreeMap::new();
    m.insert("userId".to_string(), Value::Str("user-x".into()));
    let u = g.create_node(&["User".into()], &m).expect("user");
    for i in 0..10_000i64 {
        let mut m = BTreeMap::new();
        m.insert(
            "conversationId".to_string(),
            Value::Str(format!("conv-{i:05}")),
        );
        m.insert("title".to_string(), Value::Str(format!("Title {i}")));
        m.insert(
            "createdAt".to_string(),
            Value::Str("2026-08-01T00:00:00Z".into()),
        );
        m.insert(
            "updatedAt".to_string(),
            Value::Str(format!("2026-09-01T00:{:02}:{:02}Z", (i / 60) % 60, i % 60)),
        );
        let c = g
            .create_node(&["AssistantConversation".into()], &m)
            .expect("conv");
        if i % 100 == 0 {
            g.create_rel(u, "HAS_CONVERSATION", c, &BTreeMap::new())
                .expect("has");
        }
    }
    let p = params("user-x", 50);
    let want = general(&g, LISTING, &p);
    assert_eq!(want.len(), 50, "fixture");
    let (got, c) = traced(&g, LISTING, &p);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, WARMED), 0, "{c:?}");
    assert_eq!(count_of(&c, WARMED_MISSES), 1, "{c:?}");
    // Sixty-four misses, then the column; fix 99 adds the page's fifty
    // hydrations of the three deferred properties.
    assert!(
        (100..130).contains(&count_of(&c, PROJECTED)),
        "sixty-four misses, then the column, then the page: {c:?}"
    );
    assert!(count_of(&c, COLUMNS) >= 30, "{c:?}");
}
