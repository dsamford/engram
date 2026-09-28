#![allow(non_snake_case)]
//! Fix 81: the seek-seeded hop aggregate (fix 58) expanded every seed's
//! adjacency into edge pairs and ran the per-row loop over them — the
//! start-only WHERE evaluated once per EDGE, a bare `count(*)` bound and
//! folded per edge, a far-end group key evaluated per edge though it is a
//! function of the end (250k expressions and 310 ms for one user's 18.7k
//! emails on the production mirror, against Neo4j's 156). Now the start's
//! predicate keeps or drops each seed BEFORE its adjacency is read; a fold
//! that reads nothing of the far end sums each survivor's degree; a fold
//! whose keys and residual predicate read the far end alone folds once per
//! distinct far end, weighted by its edge count; anything else walks the
//! survivors' edges as before.
//!
//! Every answer is checked against the same statement with the columnar
//! paths OFF (the general path). Expressions are counted per top-level
//! evaluation, so "per seed, not per edge" is a bound the trace proves.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn params(user: &str) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("userId".to_string(), Value::Str(user.to_string()));
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

const SEEDED: &str = "interp.columnar hop scan seeded from a sought end";
const FILTERED: &str = "interp.columnar seeded hop filtered its seeds by the start's predicate";
const DEGREES: &str = "interp.columnar seeded hop summed degrees per seed";
const PER_END: &str = "interp.columnar seeded hop folded per distinct far end";
const EXPRS: &str = "cypher.expressions evaluated";

/// Per user: 400 emails, 2,000 MENTIONS edges to entities, 40 more to
/// topics (not entities); one email in eight quarantined, one in eight
/// clean.
const SEEDS: u64 = 400;
const EDGES: u64 = 2_040;

/// 4,800 emails over 12 users (400 each, a twelfth of a label well past
/// the seek floor) with DECLARED `userId` and `nodeType` indexes; every
/// email mentions five of 300 entities, every tenth mentions a topic (a
/// node that is not an entity); per user, one email in eight is
/// quarantined and one in eight clean.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(
        &g,
        "CREATE INDEX udn_user FOR (n:UserDataNode) ON (n.userId)",
    );
    ddl(
        &g,
        "CREATE INDEX udn_type FOR (n:UserDataNode) ON (n.nodeType)",
    );
    let mut ents = Vec::new();
    for k in 0..300i64 {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), Value::Str(format!("Entity {k:03}")));
        m.insert(
            "type".to_string(),
            Value::Str(if k % 3 == 0 {
                "org".into()
            } else {
                "person".into()
            }),
        );
        ents.push(g.create_node(&["Entity".into()], &m).expect("entity"));
    }
    let mut topics = Vec::new();
    for k in 0..3i64 {
        let mut m = BTreeMap::new();
        m.insert("label".to_string(), Value::Str(format!("Topic {k}")));
        topics.push(g.create_node(&["Topic".into()], &m).expect("topic"));
    }
    for i in 0..4800i64 {
        let k = i / 12;
        let mut m = BTreeMap::new();
        m.insert("nodeType".to_string(), Value::Str("email".into()));
        m.insert("userId".to_string(), Value::Str(format!("u{:02}", i % 12)));
        m.insert("nodeId".to_string(), Value::Str(format!("mail-{i:05}")));
        if k % 8 == 0 {
            m.insert("abuseStatus".to_string(), Value::Str("quarantined".into()));
        } else if k % 8 == 1 {
            m.insert("abuseStatus".to_string(), Value::Str("clean".into()));
        }
        let n = g.create_node(&["UserDataNode".into()], &m).expect("email");
        for j in 0..5i64 {
            let e = ents[((i * 7 + j * 31) % 300) as usize];
            g.create_rel(n, "MENTIONS", e, &BTreeMap::new())
                .expect("mentions");
        }
        if k % 10 == 0 {
            g.create_rel(n, "MENTIONS", topics[(k % 3) as usize], &BTreeMap::new())
                .expect("topic");
        }
    }
    g
}

/// The production statement: the mentioned-entity aggregate over ONE
/// user's emails, its top twenty.
const ORIG: &str = "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'})-[:MENTIONS]->(e) \
    WHERE n.abuseStatus IS NULL OR n.abuseStatus IN ['clean', 'approved'] \
    RETURN e.name AS name, e.type AS type, count(*) AS cnt ORDER BY cnt DESC, name LIMIT 20";

/// A bare `count(*)` (or `count(e)`) from a sought start reads no edge:
/// each seed's degree is its count, and the inline map's equalities are
/// evaluated once per seed — under 600 expressions for 2,040 edges.
#[test]
fn a_a_bare_count_sums_degrees_per_seed() {
    let g = corpus();
    let p = params("u03");
    for src in [
        "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'})-[:MENTIONS]->(e) RETURN count(*) AS n",
        "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'})-[:MENTIONS]->(e) RETURN count(e) AS n",
    ] {
        let want = general(&g, src, &p);
        assert_eq!(
            want,
            vec![vec![Value::Int(EDGES as i64)]],
            "fixture: `{src}`"
        );
        let (got, c) = traced(&g, src, &p);
        assert_eq!(got, want, "`{src}`");
        assert_eq!(count_of(&c, SEEDED), 1, "`{src}`: {c:?}");
        assert_eq!(count_of(&c, DEGREES), 1, "`{src}`: {c:?}");
        assert_eq!(count_of(&c, PER_END), 0, "`{src}`: {c:?}");
        let exprs = count_of(&c, EXPRS);
        assert!(
            (SEEDS..600).contains(&exprs),
            "`{src}`: per seed, not per edge — {exprs} expressions"
        );
    }
}

/// A labelled far end counts member peers only: the topic edges are
/// MENTIONS too, and `(e:Entity)` leaves them out.
#[test]
fn b_a_labelled_far_end_counts_member_peers_only() {
    let g = corpus();
    let p = params("u05");
    let src = "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'})-[:MENTIONS]->(e:Entity) RETURN count(*) AS n";
    let want = general(&g, src, &p);
    assert_eq!(want, vec![vec![Value::Int(2_000)]], "fixture");
    let (got, c) = traced(&g, src, &p);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, SEEDED), 1, "{c:?}");
    assert_eq!(count_of(&c, DEGREES), 1, "{c:?}");
    assert!(count_of(&c, EXPRS) < 600, "{c:?}");
}

/// The start's predicate drops a seed before its adjacency is read: the
/// quarantined eighth contributes nothing, and the WHERE is evaluated per
/// seed, never per edge.
#[test]
fn c_the_starts_predicate_drops_seeds_before_their_edges() {
    let g = corpus();
    let p = params("u07");
    let src = "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'})-[:MENTIONS]->(e) \
        WHERE n.abuseStatus IS NULL OR n.abuseStatus IN ['clean', 'approved'] \
        RETURN count(*) AS n";
    let want = general(&g, src, &p);
    let Value::Int(n) = want[0][0] else {
        panic!("{want:?}");
    };
    assert!(n < EDGES as i64 && n > (EDGES as i64) / 2, "fixture: {n}");
    let (got, c) = traced(&g, src, &p);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, FILTERED), 1, "{c:?}");
    assert_eq!(count_of(&c, DEGREES), 1, "{c:?}");
    assert!(count_of(&c, EXPRS) < 600, "{c:?}");
}

/// The production listing: its keys are a function of the far end, so
/// the fold runs once per distinct end (weighted by its edge count) —
/// under 1,500 expressions for 2,040 edges, the same twenty rows.
#[test]
fn d_a_far_end_key_folds_once_per_distinct_end() {
    let g = corpus();
    let p = params("u02");
    let want = general(&g, ORIG, &p);
    assert_eq!(want.len(), 20, "fixture");
    let (got, c) = traced(&g, ORIG, &p);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, SEEDED), 1, "{c:?}");
    assert_eq!(count_of(&c, FILTERED), 1, "{c:?}");
    assert_eq!(count_of(&c, PER_END), 1, "{c:?}");
    assert_eq!(count_of(&c, DEGREES), 0, "{c:?}");
    let exprs = count_of(&c, EXPRS);
    assert!(
        exprs < 1_500,
        "per distinct end, not per edge — {exprs} expressions"
    );
    // Without the LIMIT, every group — the topics' null keys included.
    let all = "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'})-[:MENTIONS]->(e) \
        RETURN e.name AS name, e.type AS type, count(*) AS cnt ORDER BY cnt DESC, name";
    let want = general(&g, all, &p);
    let (got, c) = traced(&g, all, &p);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, PER_END), 1, "{c:?}");
}

/// A residual conjunct over the far end is evaluated per distinct end
/// too, after the start's own conjunct dropped its seeds.
#[test]
fn e_a_far_end_residual_is_evaluated_per_end() {
    let g = corpus();
    let p = params("u09");
    let src = "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'})-[:MENTIONS]->(e) \
        WHERE n.abuseStatus IS NULL AND e.type = 'person' \
        RETURN e.name AS name, count(*) AS n ORDER BY n DESC, name LIMIT 5";
    let want = general(&g, src, &p);
    assert_eq!(want.len(), 5, "fixture");
    let (got, c) = traced(&g, src, &p);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, FILTERED), 1, "{c:?}");
    assert_eq!(count_of(&c, PER_END), 1, "{c:?}");
    assert!(count_of(&c, EXPRS) < 1_500, "{c:?}");
}

/// A key over the START with a bare count is a degree sum per seed.
#[test]
fn f_a_start_key_with_a_bare_count_sums_degrees() {
    let g = corpus();
    let p = params("u04");
    let src = "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'})-[:MENTIONS]->(e) \
        RETURN n.nodeId AS id, count(*) AS n ORDER BY n DESC, id LIMIT 10";
    let want = general(&g, src, &p);
    assert_eq!(want.len(), 10, "fixture");
    let (got, c) = traced(&g, src, &p);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, DEGREES), 1, "{c:?}");
    assert!(count_of(&c, EXPRS) < 1_000, "{c:?}");
}

/// The shapes the fast folds decline still seed and still agree: a key
/// over both ends, a non-star aggregate, a conjunct that reads both ends.
#[test]
fn g_the_other_shapes_walk_the_survivors_edges() {
    let g = corpus();
    let p = params("u06");
    for src in [
        "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'})-[:MENTIONS]->(e) \
         RETURN n.nodeId AS id, e.type AS type, count(*) AS n ORDER BY id, type LIMIT 10",
        "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'})-[:MENTIONS]->(e) \
         RETURN e.type AS type, count(DISTINCT e.name) AS n ORDER BY type",
        "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'})-[:MENTIONS]->(e) \
         WHERE n.nodeId <> e.name RETURN e.type AS type, count(*) AS n ORDER BY type",
        "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'})-[:MENTIONS]->(e) \
         WHERE n.abuseStatus IS NULL RETURN e.type AS type, min(e.name) AS first ORDER BY type",
    ] {
        let want = general(&g, src, &p);
        assert!(!want.is_empty(), "fixture: `{src}`");
        let (got, c) = traced(&g, src, &p);
        assert_eq!(got, want, "`{src}`");
        assert_eq!(count_of(&c, SEEDED), 1, "`{src}`: {c:?}");
        assert_eq!(count_of(&c, DEGREES), 0, "`{src}`: {c:?}");
        assert_eq!(count_of(&c, PER_END), 0, "`{src}`: {c:?}");
    }
}

/// The whole-type walk (a start nothing seeks) carries the split
/// predicate unchanged: the start's conjunct and the rest, per row.
#[test]
fn h_the_whole_type_walk_is_unchanged() {
    let g = corpus();
    let p = params("u01");
    for src in [
        "MATCH (n:UserDataNode {nodeType: 'email'})-[:MENTIONS]->(e) \
         WHERE n.abuseStatus IS NULL RETURN e.type AS type, count(*) AS n ORDER BY type",
        "MATCH (n:UserDataNode {nodeType: 'email'})-[:MENTIONS]->(e:Entity) \
         WHERE n.abuseStatus IS NULL AND e.type = 'org' RETURN count(*) AS n",
    ] {
        let want = general(&g, src, &p);
        let (got, c) = traced(&g, src, &p);
        assert_eq!(got, want, "`{src}`");
        assert_eq!(count_of(&c, SEEDED), 0, "`{src}`: {c:?}");
        assert_eq!(count_of(&c, FILTERED), 0, "`{src}`: {c:?}");
    }
}

/// A user nobody is: the seek selects nothing and the fold is empty — no
/// column loaded, no edge read.
#[test]
fn i_an_unknown_user_folds_nothing() {
    let g = corpus();
    let p = params("nobody");
    assert!(general(&g, ORIG, &p).is_empty());
    let (got, c) = traced(&g, ORIG, &p);
    assert!(got.is_empty(), "{got:?}");
    assert_eq!(count_of(&c, SEEDED), 1, "{c:?}");
    assert_eq!(count_of(&c, "store.gets"), 0, "{c:?}");
    let (got, _) = traced(
        &g,
        "MATCH (n:UserDataNode {userId: $userId, nodeType: 'email'})-[:MENTIONS]->(e) RETURN count(*) AS n",
        &p,
    );
    assert_eq!(got, vec![vec![Value::Int(0)]]);
}
