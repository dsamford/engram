#![allow(non_snake_case)]
//! Fix 89: an EXISTS body that is a chain from its bound start, whose
//! remainder past the first hop reads nothing of the start, is a function
//! of the first hop's END — the visibility test of a project's work items
//! is the project's. The KMWorkItem visibility listing on the mirror
//! evaluated its two such bodies once per item (31k seeds, 77k hop ends
//! bound, 183 ms against Neo4j's 121) for 77 projects' worth of answers.
//! Now the remainder is evaluated once per distinct first-hop end per
//! statement, and the body answers true at the first true end.
//!
//! Every answer is checked against the same statement with the columnar
//! paths OFF (the general matcher, which also runs the memo — so the
//! control is the memo switched off by shape, not by flag).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("viewerId".to_string(), Value::Str("user-1".into()));
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

const MEMO_HIT: &str = "interp.exists remainder answered from its first-hop memo";
const MEMO_MISS: &str = "interp.exists remainder evaluated for its first-hop memo";

/// 80 projects, 12,000 work items (150 per project); three users; user-1 an
/// ACTIVE member of projects 0..20 and an inactive member of 20..30; a
/// team granted projects 40..45 with user-1 an active member of the team.
fn corpus() -> (Graph, i64) {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut users = Vec::new();
    for k in 0..3i64 {
        let mut m = BTreeMap::new();
        m.insert("userId".to_string(), Value::Str(format!("user-{k}")));
        users.push(g.create_node(&["User".into()], &m).expect("user"));
    }
    let mut projects = Vec::new();
    for k in 0..80i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Str(format!("proj-{k:03}")));
        let p = g.create_node(&["KMProject".into()], &m).expect("project");
        projects.push(p);
        if k < 20 {
            g.create_rel(users[1], "MEMBER_OF", p, &BTreeMap::new())
                .expect("member");
        } else if k < 30 {
            let mut rm = BTreeMap::new();
            rm.insert("state".to_string(), Value::Str("inactive".into()));
            g.create_rel(users[1], "MEMBER_OF", p, &rm).expect("member");
        }
    }
    let team = g
        .create_node(&["Team".into()], &BTreeMap::new())
        .expect("team");
    for p in &projects[40..45] {
        g.create_rel(team, "GRANTS", *p, &BTreeMap::new())
            .expect("grant");
    }
    g.create_rel(users[1], "MEMBER_OF", team, &BTreeMap::new())
        .expect("team member");
    for i in 0..12_000i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Str(format!("wi-{i:05}")));
        m.insert(
            "updatedAt".to_string(),
            Value::Str(format!("2026-09-{:02}T{:02}:00:00Z", 1 + (i % 28), i % 24)),
        );
        let w = g.create_node(&["KMWorkItem".into()], &m).expect("item");
        g.create_rel(
            w,
            "BELONGS_TO_PROJECT",
            projects[(i / 150) as usize],
            &BTreeMap::new(),
        )
        .expect("rel");
    }
    // Visible: projects 0..20 (3,000 items) and 40..45 (750 items).
    (g, 3_750)
}

const MEMBER_BODY: &str = "EXISTS { MATCH (w)-[:BELONGS_TO_PROJECT]->(:KMProject)<-[_visMem:MEMBER_OF]-(:User {userId: $viewerId}) WHERE coalesce(_visMem.state,'active') = 'active' }";
const TEAM_BODY: &str = "EXISTS { MATCH (w)-[:BELONGS_TO_PROJECT]->(:KMProject)<-[:GRANTS]-(:Team)<-[_visTgm:MEMBER_OF]-(:User {userId: $viewerId}) WHERE coalesce(_visTgm.state,'active') = 'active' }";

/// The visibility count: each body's remainder is evaluated once per
/// project (80 misses per body), every other item answers from the memo.
#[test]
fn a_the_visibility_bodies_are_answered_once_per_project() {
    let (g, visible) = corpus();
    let src =
        format!("MATCH (w:KMWorkItem) WHERE {MEMBER_BODY} OR {TEAM_BODY} RETURN count(w) AS n");
    let (got, c) = traced(&g, &src);
    assert_eq!(got, vec![vec![Value::Int(visible)]]);
    let misses = count_of(&c, MEMO_MISS);
    assert!(
        (80..=160).contains(&misses),
        "one remainder per project per body: {c:?}"
    );
    assert!(count_of(&c, MEMO_HIT) >= 12_000 - 160, "{c:?}");
    // The bodies still seed once per item (the OR's operands), but bind no
    // project or user end per item.
    assert!(
        count_of(&c, "interp.matcher bound a hop end to its demand") < 1_000,
        "{c:?}"
    );
}

/// The listing itself — the production shape's items, their project id and
/// parent — pages the visible items in order, as the general path does.
#[test]
fn b_the_listing_agrees_with_the_general_path() {
    let (g, _) = corpus();
    let src = format!(
        "MATCH (w:KMWorkItem) WHERE ( ( coalesce(w.scope,'') IN ['organization'] AND w.orgId = 'e2e' ) OR w.userId = $viewerId OR {MEMBER_BODY} OR {TEAM_BODY} ) \
         RETURN w.id AS id, [(w)-[:BELONGS_TO_PROJECT]->(p:KMProject) | p.id][0] AS projectId \
         ORDER BY w.updatedAt DESC, id SKIP 0 LIMIT 200"
    );
    g.set_columnar_scans(false);
    let want = rows(&g, &src);
    g.set_columnar_scans(true);
    let (got, c) = traced(&g, &src);
    assert_eq!(got.len(), 200);
    assert_eq!(got, want);
    assert!(count_of(&c, MEMO_HIT) > 10_000, "{c:?}");
}

/// CONTROLS: a remainder that reads the START is not a function of the
/// first hop's end; a single-hop body has no remainder; a remainder hop
/// sharing the first hop's type could re-use its relationship. Each keeps
/// the whole-path matcher and answers the same.
#[test]
fn c_bodies_the_memo_cannot_key_keep_the_matcher() {
    let (g, visible) = corpus();
    for (src, want) in [
        (
            "MATCH (w:KMWorkItem) WHERE EXISTS { MATCH (w)-[:BELONGS_TO_PROJECT]->(p:KMProject)<-[:MEMBER_OF]-(u:User {userId: $viewerId}) WHERE w.id <> u.userId } RETURN count(w) AS n".to_string(),
            4_500,
        ),
        (
            "MATCH (w:KMWorkItem) WHERE EXISTS { MATCH (w)-[:BELONGS_TO_PROJECT]->(:KMProject) } RETURN count(w) AS n".to_string(),
            12_000,
        ),
        (
            "MATCH (w:KMWorkItem) WHERE EXISTS { MATCH (w)-[:BELONGS_TO_PROJECT]->(:KMProject)<-[:BELONGS_TO_PROJECT]-(o:KMWorkItem) WHERE o.id <> w.id } RETURN count(w) AS n".to_string(),
            12_000,
        ),
    ] {
        let (got, c) = traced(&g, &src);
        assert_eq!(got, vec![vec![Value::Int(want)]], "`{src}`");
        assert_eq!(count_of(&c, MEMO_MISS), 0, "`{src}`: {c:?}");
        assert_eq!(count_of(&c, MEMO_HIT), 0, "`{src}`: {c:?}");
    }
    let _ = visible;
}

/// The memo is per STATEMENT: a second statement evaluates its remainders
/// again (a write between them could have changed the answer).
#[test]
fn d_the_memo_is_cleared_between_statements() {
    let (g, visible) = corpus();
    let src = format!("MATCH (w:KMWorkItem) WHERE {MEMBER_BODY} RETURN count(w) AS n");
    let (got, c1) = traced(&g, &src);
    assert_eq!(got, vec![vec![Value::Int(3_000)]]);
    let (got, c2) = traced(&g, &src);
    assert_eq!(got, vec![vec![Value::Int(3_000)]]);
    assert_eq!(
        count_of(&c1, MEMO_MISS),
        count_of(&c2, MEMO_MISS),
        "{c1:?}\n{c2:?}"
    );
    assert!(count_of(&c2, MEMO_MISS) >= 80, "{c2:?}");
    assert!(count_of(&c2, MEMO_HIT) >= 12_000 - 80, "{c2:?}");
    let _ = visible;
}
