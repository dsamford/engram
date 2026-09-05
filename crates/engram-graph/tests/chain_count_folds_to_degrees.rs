#![allow(non_snake_case)]
//! Fix 72: a `count(<var>)` over the chain a MATCH binds — with nothing
//! else of that chain read by the projection that follows — folds the
//! MATCH into the projection as `sum(COUNT { <chain> })`, and a multi-hop
//! `COUNT { … }` from a bound start is answered as a walk of ids over the
//! adjacency tables whose last hop is a degree. The production
//! assistant-conversation listing expanded every message of every
//! conversation into a row (44,800 bare-bound hop ends, 90k expressions on
//! the mirror's largest user) to fold them straight back into fifty
//! integers: 107 ms against Neo4j's 12.
//!
//! Every answer is checked against the same statement with the fold OFF
//! (the clause expanded and aggregated as before).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn params(user: &str) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("userId".to_string(), s(user));
    p.insert("offset".to_string(), Value::Int(0));
    p.insert("limit".to_string(), Value::Int(50));
    p
}

fn rows(g: &Graph, src: &str, user: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params(user))
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str, user: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let (r, trace) = engram_observe::with_trace(|| rows(g, src, user));
    (r, trace.counters().clone())
}

/// The control: the same statement with the fold off.
fn unfolded(g: &Graph, src: &str, user: &str) -> Vec<Vec<Value>> {
    g.set_chain_count_fold(false);
    let r = rows(g, src, user);
    g.set_chain_count_fold(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const FOLDED: &str = "interp.chain count folded into its projection";
const CHAIN: &str = "interp.count folded a multi-hop chain";
const BARE: &str = "interp.matcher bound a hop end bare";
const EXPRS: &str = "cypher.expressions evaluated";

/// User u-big owns 20 conversations: conv-0 holds 3 branches × 800
/// messages, conv-5/10/15 hold 2 × 300, conv-19 has NO branch, the rest
/// one branch of 60. conv-1 is reached by TWO `HAS_CONVERSATION`
/// relationships (a duplicate group key). User u-other owns three small
/// conversations. Every fifth message carries no `AssistantMessage` label.
/// Five Projects with 0..4 tracks each for the one-hop RETURN form.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut um = BTreeMap::new();
    um.insert("userId".into(), s("u-big"));
    let user = g.create_node(&["User".into()], &um).expect("user");
    let mut om = BTreeMap::new();
    om.insert("userId".into(), s("u-other"));
    let other = g.create_node(&["User".into()], &om).expect("user");
    for (owner, prefix, n) in [(user, "conv", 20i64), (other, "oconv", 3)] {
        for ci in 0..n {
            let mut cm = BTreeMap::new();
            cm.insert("conversationId".into(), s(&format!("{prefix}-{ci}")));
            cm.insert("title".into(), s(&format!("Conversation {ci}")));
            cm.insert("createdAt".into(), s(&format!("2026-06-{:02}T05:33:40.324Z", 1 + ci % 28)));
            cm.insert("updatedAt".into(), s(&format!("2026-09-{:02}T17:{:02}:36.054Z", 1 + ci % 5, ci % 60)));
            let c = g.create_node(&["AssistantConversation".into()], &cm).expect("conv");
            g.create_rel(owner, "HAS_CONVERSATION", c, &BTreeMap::new()).expect("has conv");
            if prefix == "conv" && ci == 1 {
                g.create_rel(owner, "HAS_CONVERSATION", c, &BTreeMap::new()).expect("dup");
            }
            let (branches, per_branch) = if prefix != "conv" {
                (1, 5)
            } else if ci == 0 {
                (3, 800)
            } else if ci == 19 {
                (0, 0)
            } else if ci % 5 == 0 {
                (2, 300)
            } else {
                (1, 60)
            };
            for bi in 0..branches {
                let mut bm = BTreeMap::new();
                bm.insert("branchId".into(), s(&format!("{prefix}-{ci}-b{bi}")));
                let b = g.create_node(&["AssistantBranch".into()], &bm).expect("branch");
                g.create_rel(c, "HAS_BRANCH", b, &BTreeMap::new()).expect("has branch");
                for mi in 0..per_branch {
                    let mut mm = BTreeMap::new();
                    mm.insert("messageId".into(), s(&format!("{prefix}-{ci}-b{bi}-m{mi}")));
                    mm.insert("timestamp".into(), s(&format!("2026-08-{:02}T{:02}:{:02}:00Z", 1 + (mi / 1440) % 28, (mi / 60) % 24, mi % 60)));
                    mm.insert("content".into(), s(&format!("message {mi} of {prefix}-{ci}-b{bi}")));
                    let labels: Vec<String> = if mi % 5 == 4 {
                        vec!["Note".into()]
                    } else {
                        vec!["AssistantMessage".into()]
                    };
                    let m = g.create_node(&labels, &mm).expect("msg");
                    g.create_rel(b, "HAS_MESSAGE", m, &BTreeMap::new()).expect("has msg");
                }
            }
        }
    }
    for pi in 0..5i64 {
        let mut pm = BTreeMap::new();
        pm.insert("id".into(), s(&format!("proj-{pi}")));
        pm.insert("updatedAt".into(), s(&format!("2026-09-0{}T00:00:00Z", 1 + pi)));
        let p = g.create_node(&["Project".into()], &pm).expect("project");
        g.create_rel(user, "OWNS_PROJECT", p, &BTreeMap::new()).expect("owns");
        for ti in 0..pi {
            let mut tm = BTreeMap::new();
            tm.insert("id".into(), s(&format!("proj-{pi}-t{ti}")));
            let t = g.create_node(&["Track".into()], &tm).expect("track");
            g.create_rel(p, "CONTAINS_TRACK", t, &BTreeMap::new()).expect("contains");
        }
    }
    g
}

const ORIG: &str = "MATCH (u:User {userId: $userId})-[:HAS_CONVERSATION]->(c:AssistantConversation) \
    OPTIONAL MATCH (c)-[:HAS_BRANCH]->()-[:HAS_MESSAGE]->(m) \
    WITH c, count(m) AS messageCount \
    RETURN c.conversationId AS conversationId, c.title AS title, c.createdAt AS createdAt, \
    c.updatedAt AS updatedAt, messageCount \
    ORDER BY c.updatedAt DESC SKIP toInteger($offset) LIMIT toInteger($limit)";

#[test]
fn a_a_count_over_an_optional_chain_folds_into_its_projection() {
    let g = corpus();
    let want = unfolded(&g, ORIG, "u-big");
    assert_eq!(want.len(), 20);
    let (got, c) = traced(&g, ORIG, "u-big");
    assert_eq!(got, want);
    assert_eq!(count_of(&c, FOLDED), 1, "{c:?}");
    // One chain walk per conversation (the duplicate key's two rows walk
    // twice); no message end is ever bound.
    assert_eq!(count_of(&c, CHAIN), 21, "{c:?}");
    assert_eq!(count_of(&c, BARE), 0, "{c:?}");
    assert!(count_of(&c, EXPRS) < 600, "{c:?}");
    // The largest conversation counts 2,400 messages; the duplicate key
    // doubles conv-1 to 120; the branchless conv-19 counts 0.
    let by_id: BTreeMap<String, i64> = got
        .iter()
        .map(|r| match (&r[0], &r[4]) {
            (Value::Str(id), Value::Int(n)) => (id.clone(), *n),
            other => panic!("row shape {other:?}"),
        })
        .collect();
    assert_eq!(by_id["conv-0"], 2_400);
    assert_eq!(by_id["conv-1"], 120);
    assert_eq!(by_id["conv-19"], 0);
    assert_eq!(by_id["conv-5"], 600);

    // The other user: three small conversations, the same fold.
    let want = unfolded(&g, ORIG, "u-other");
    let (got, c) = traced(&g, ORIG, "u-other");
    assert_eq!(got, want);
    assert_eq!(got.len(), 3);
    assert_eq!(count_of(&c, FOLDED), 1, "{c:?}");
}

/// A labelled middle and end count only the members (the `Note` messages
/// are left out); a plain MATCH drops the conversations whose chain is
/// empty (conv-19); a WHERE on the chain moves into the body.
#[test]
fn b_a_plain_match_and_a_labelled_end_keep_their_rows() {
    let g = corpus();
    // Labelled middle + end over an OPTIONAL chain.
    let src = "MATCH (u:User {userId: $userId})-[:HAS_CONVERSATION]->(c:AssistantConversation) \
        OPTIONAL MATCH (c)-[:HAS_BRANCH]->(:AssistantBranch)-[:HAS_MESSAGE]->(m:AssistantMessage) \
        WITH c, count(m) AS n \
        RETURN c.conversationId AS id, n ORDER BY n DESC, id ASC";
    let want = unfolded(&g, src, "u-big");
    assert_eq!(want.len(), 20);
    let (got, c) = traced(&g, src, "u-big");
    assert_eq!(got, want);
    assert_eq!(count_of(&c, FOLDED), 1, "{c:?}");
    assert_eq!(count_of(&c, BARE), 0, "{c:?}");
    assert_eq!(got[0], vec![s("conv-0"), Value::Int(1_920)], "four of every five messages");
    assert!(got.contains(&vec![s("conv-19"), Value::Int(0)]), "{got:?}");
    // A plain MATCH: the branchless conversation has no row. The columnar
    // recognisers claim this pair when they may (a fused MATCH + aggregate
    // is theirs); with them off the general path — and so the fold — is
    // what answers, and its `WHERE n > 0` guard is the claim under test.
    let src = "MATCH (u:User {userId: $userId})-[:HAS_CONVERSATION]->(c:AssistantConversation) \
        MATCH (c)-[:HAS_BRANCH]->(:AssistantBranch)-[:HAS_MESSAGE]->(m:AssistantMessage) \
        WITH c, count(m) AS n \
        RETURN c.conversationId AS id, n ORDER BY n DESC, id ASC";
    let want = unfolded(&g, src, "u-big");
    assert_eq!(want.len(), 19, "conv-19 has no branch");
    g.set_columnar_scans(false);
    let want_general = unfolded(&g, src, "u-big");
    let (got, c) = traced(&g, src, "u-big");
    g.set_columnar_scans(true);
    assert_eq!(want_general, want, "the general path and the pipeline agree unfolded");
    assert_eq!(got, want);
    assert_eq!(count_of(&c, FOLDED), 1, "{c:?}");
    assert_eq!(count_of(&c, BARE), 0, "{c:?}");
    assert!(!got.iter().any(|r| r[0] == s("conv-19")), "{got:?}");
    // A WHERE on the chain moves into the body.
    let src = "MATCH (u:User {userId: $userId})-[:HAS_CONVERSATION]->(c:AssistantConversation) \
        OPTIONAL MATCH (c)-[:HAS_BRANCH]->(b)-[:HAS_MESSAGE]->(m) WHERE b.branchId ENDS WITH '-b0' \
        WITH c, count(m) AS n \
        RETURN c.conversationId AS id, n ORDER BY id ASC";
    let want = unfolded(&g, src, "u-big");
    let (got, c) = traced(&g, src, "u-big");
    assert_eq!(got, want);
    assert_eq!(count_of(&c, FOLDED), 1, "{c:?}");
    assert_eq!(got[0], vec![s("conv-0"), Value::Int(800)], "branch 0 only");
}

/// The one-hop RETURN form (the Project listing): `RETURN p,
/// count(t)` over an OPTIONAL hop to a labelled end.
#[test]
fn c_a_one_hop_return_form_folds() {
    let g = corpus();
    let src = "MATCH (u:User {userId: $userId})-[:OWNS_PROJECT]->(p:Project) \
        OPTIONAL MATCH (p)-[:CONTAINS_TRACK]->(t:Track) \
        RETURN p, count(t) AS trackCount ORDER BY p.updatedAt DESC";
    let want = unfolded(&g, src, "u-big");
    assert_eq!(want.len(), 5);
    let (got, c) = traced(&g, src, "u-big");
    assert_eq!(got, want);
    assert_eq!(count_of(&c, FOLDED), 1, "{c:?}");
    assert_eq!(count_of(&c, BARE), 0, "{c:?}");
    let id_of = |r: &Vec<Value>| match &r[0] {
        Value::Node { props, .. } => props.get("id").cloned().unwrap_or(Value::Null),
        other => panic!("not a node: {other:?}"),
    };
    assert_eq!((id_of(&got[0]), got[0][1].clone()), (s("proj-4"), Value::Int(4)));
    assert_eq!((id_of(&got[4]), got[4][1].clone()), (s("proj-0"), Value::Int(0)));
}

/// Shapes that READ the chain, count it distinctly, count the null row, or
/// start from an unbound var stay exactly as written.
#[test]
fn d_a_projection_that_reads_the_chain_declines() {
    let g = corpus();
    for src in [
        // collect() reads the rows.
        "MATCH (u:User {userId: $userId})-[:HAS_CONVERSATION]->(c:AssistantConversation) \
         OPTIONAL MATCH (c)-[:HAS_BRANCH]->(b)-[:HAS_MESSAGE]->(m) \
         WITH c, count(m) AS n, collect(DISTINCT b.branchId) AS branches \
         RETURN c.conversationId AS id, n, size(branches) AS nb ORDER BY id ASC",
        // DISTINCT ends are not paths.
        "MATCH (u:User {userId: $userId})-[:HAS_CONVERSATION]->(c:AssistantConversation) \
         OPTIONAL MATCH (c)-[:HAS_BRANCH]->()-[:HAS_MESSAGE]->(m) \
         WITH c, count(DISTINCT m) AS n RETURN c.conversationId AS id, n ORDER BY id ASC",
        // count(*) over an OPTIONAL chain counts the null row.
        "MATCH (u:User {userId: $userId})-[:HAS_CONVERSATION]->(c:AssistantConversation) \
         OPTIONAL MATCH (c)-[:HAS_BRANCH]->()-[:HAS_MESSAGE]->(m) \
         WITH c, count(*) AS n RETURN c.conversationId AS id, n ORDER BY id ASC",
        // A non-aggregate read of the end.
        "MATCH (u:User {userId: $userId})-[:HAS_CONVERSATION]->(c:AssistantConversation) \
         OPTIONAL MATCH (c)-[:HAS_BRANCH]->()-[:HAS_MESSAGE]->(m) \
         WITH c, m.content AS content, count(m) AS n RETURN c.conversationId AS id, content, n ORDER BY id ASC, content ASC LIMIT 5",
        // The chain's start is not bound before it (a scan, not a fold).
        "MATCH (c:AssistantConversation)-[:HAS_BRANCH]->()-[:HAS_MESSAGE]->(m) \
         WITH c, count(m) AS n RETURN c.conversationId AS id, n ORDER BY id ASC",
    ] {
        let want = unfolded(&g, src, "u-big");
        let (got, c) = traced(&g, src, "u-big");
        assert_eq!(got, want, "{src}");
        assert_eq!(count_of(&c, FOLDED), 0, "{src}: {c:?}");
    }
    // The count(*) row for the branchless conversation is 1, as Cypher says.
    let src = "MATCH (u:User {userId: $userId})-[:HAS_CONVERSATION]->(c:AssistantConversation) \
         OPTIONAL MATCH (c)-[:HAS_BRANCH]->()-[:HAS_MESSAGE]->(m) \
         WITH c, count(*) AS n RETURN c.conversationId AS id, n ORDER BY id ASC";
    let got = rows(&g, src, "u-big");
    assert!(got.contains(&vec![s("conv-19"), Value::Int(1)]), "{got:?}");
}
