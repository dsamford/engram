//! The DEGREE SHORT-CIRCUIT (`try_degree_aggregate`) proven byte-identical to the
//! general chunk-build + reduce AND to the interp, on the shape it targets
//! (`count(src) GROUP BY dst.<prop>`), including the two hazards it handles with NO
//! schema assumption. VALUE MERGE: two distinct target nodes sharing the group
//! property value collapse into ONE group (grouping is by value, not by node). TIE
//! under a PARTIAL order: the tied groups keep first-seen-value order. Plus decline
//! cases (`count(DISTINCT …)`, a WHERE) that fall to the general path and still
//! answer identically, and byte-identity at scale.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

type Rows = Vec<Vec<Value>>;

fn node(g: &Graph, label: &str, props: &[(&str, Value)]) -> u64 {
    let mut m = BTreeMap::new();
    for (k, v) in props {
        m.insert((*k).to_string(), v.clone());
    }
    g.create_node(&[label.into()], &m).expect("node")
}

fn rel(g: &Graph, src: u64, ty: &str, dst: u64) {
    g.create_rel(src, ty, dst, &BTreeMap::new()).expect("rel");
}

fn rows(g: &Graph, src: &str) -> Rows {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run: {e}"))
        .rows
}

/// (degree short-circuit ON, general chunk-build+reduce, interp) — all three must
/// agree byte-for-byte. `set_degree_aggregate(false)` forces the chunk path;
/// `set_columnar_scans(false)` forces the interpreter.
fn three_deg(g: &Graph, src: &str) -> (Rows, Rows, Rows) {
    g.set_columnar_scans(true);
    g.set_degree_aggregate(true);
    let deg = rows(g, src);
    g.set_degree_aggregate(false);
    let general = rows(g, src);
    g.set_degree_aggregate(true);
    g.set_columnar_scans(false);
    let interp = rows(g, src);
    g.set_columnar_scans(true);
    (deg, general, interp)
}

fn deg_fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    g.set_degree_aggregate(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace
        .counters()
        .get("interp.pipeline degree aggregate")
        .copied()
        .unwrap_or(0)
        > 0
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}
fn i(n: i64) -> Value {
    Value::Int(n)
}

fn replies_graph() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let m1 = node(&g, "Message", &[("id", i(10))]);
    let m2 = node(&g, "Message", &[("id", i(20))]);
    let m3 = node(&g, "Message", &[("id", i(30))]);
    // m1: 3 replies, m2: 1, m3: 2.
    for dst in [m1, m1, m1, m2, m3, m3] {
        let c = node(&g, "Comment", &[]);
        rel(&g, c, "REPLY_OF", dst);
    }
    g
}

#[test]
fn degree_by_id_matches_general_and_interp() {
    let g = replies_graph();
    let src = "MATCH (c:Comment)-[:REPLY_OF]->(m:Message) \
        RETURN m.id AS msg, count(c) AS replies ORDER BY replies DESC, msg ASC LIMIT 20";
    let (deg, general, interp) = three_deg(&g, src);
    assert_eq!(
        deg, general,
        "degree short-circuit vs general reduce disagree"
    );
    assert_eq!(deg, interp, "degree short-circuit vs interp disagree");
    assert_eq!(
        deg,
        vec![vec![i(10), i(3)], vec![i(30), i(2)], vec![i(20), i(1)],],
        "reply counts per message"
    );
    assert!(
        deg_fired(&g, src),
        "the count-by-target shape must short-circuit"
    );
}

#[test]
fn degree_merges_a_name_collision() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // t1 and t3 SHARE the name "Music" — two nodes, one group VALUE.
    let t1 = node(&g, "Tag", &[("name", s("Music"))]);
    let t2 = node(&g, "Tag", &[("name", s("Sport"))]);
    let t3 = node(&g, "Tag", &[("name", s("Music"))]);
    for (p_dst, _) in [(t1, 0), (t1, 0), (t3, 0), (t2, 0), (t3, 0)] {
        let p = node(&g, "Person", &[]);
        rel(&g, p, "HAS_INTEREST", p_dst);
    }
    let src = "MATCH (p:Person)-[:HAS_INTEREST]->(t:Tag) \
        RETURN t.name AS tag, count(p) AS people ORDER BY people DESC, tag ASC";
    let (deg, general, interp) = three_deg(&g, src);
    assert_eq!(deg, general, "degree vs general disagree on a value merge");
    assert_eq!(deg, interp, "degree vs interp disagree on a value merge");
    // "Music" merges t1 (2) + t3 (2) = 4; "Sport" = 1 — grouping is BY VALUE.
    assert_eq!(deg, vec![vec![s("Music"), i(4)], vec![s("Sport"), i(1)]]);
    assert!(
        deg_fired(&g, src),
        "the count-by-target shape must short-circuit"
    );
}

#[test]
fn degree_tie_under_partial_order_keeps_first_seen() {
    let g = replies_graph(); // m1=3, m2=1, m3=2 — make a tie:
    let extra = node(&g, "Message", &[("id", i(40))]);
    for _ in 0..2 {
        let c = node(&g, "Comment", &[]);
        rel(&g, c, "REPLY_OF", extra); // m40 also = 2, ties with m30
    }
    // ORDER BY replies DESC ONLY — m30 and m40 tie at 2; the tie order is
    // first-seen-value order, which all three paths must reproduce identically.
    let src = "MATCH (c:Comment)-[:REPLY_OF]->(m:Message) \
        RETURN m.id AS msg, count(c) AS replies ORDER BY replies DESC";
    let (deg, general, interp) = three_deg(&g, src);
    assert_eq!(deg, general, "degree vs general disagree on a tie");
    assert_eq!(deg, interp, "degree vs interp disagree on a tie");
    assert!(deg_fired(&g, src));
}

#[test]
fn degree_declines_count_distinct_but_answers_identically() {
    let g = replies_graph();
    let src = "MATCH (c:Comment)-[:REPLY_OF]->(m:Message) \
        RETURN m.id AS msg, count(DISTINCT c) AS replies ORDER BY replies DESC, msg ASC";
    let (deg, general, interp) = three_deg(&g, src);
    assert_eq!(deg, general);
    assert_eq!(deg, interp);
    assert!(
        !deg_fired(&g, src),
        "count(DISTINCT …) must DECLINE the degree short-circuit"
    );
}

#[test]
fn degree_declines_with_a_where_but_answers_identically() {
    let g = replies_graph();
    // A WHERE means the count is filtered — the degree short-circuit must decline.
    let src = "MATCH (c:Comment)-[:REPLY_OF]->(m:Message) WHERE m.id > 10 \
        RETURN m.id AS msg, count(c) AS replies ORDER BY replies DESC, msg ASC";
    let (deg, general, interp) = three_deg(&g, src);
    assert_eq!(deg, general);
    assert_eq!(deg, interp);
    assert!(
        !deg_fired(&g, src),
        "a WHERE must DECLINE the degree short-circuit"
    );
}

#[test]
fn degree_byte_identical_at_scale() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let n_msg: u64 = 2_000;
    let n_com: u64 = 20_000;
    let mut msgs = Vec::with_capacity(n_msg as usize);
    for k in 0..n_msg {
        msgs.push(node(&g, "Message", &[("id", i(k as i64))]));
    }
    for c in 0..n_com {
        let cid = node(&g, "Comment", &[]);
        let target = (c.wrapping_mul(2_654_435_761) % n_msg) as usize;
        rel(&g, cid, "REPLY_OF", msgs[target]);
    }
    let src = "MATCH (c:Comment)-[:REPLY_OF]->(m:Message) \
        RETURN m.id AS msg, count(c) AS replies ORDER BY replies DESC, msg ASC LIMIT 20";
    let (deg, general, interp) = three_deg(&g, src);
    assert_eq!(deg, general, "degree vs general disagree at scale");
    assert_eq!(deg, interp, "degree vs interp disagree at scale");
    assert!(deg_fired(&g, src));
}

#[test]
fn degree_topk_prelimit_with_boundary_ties() {
    // The top-k-before-project drop: 3 messages with 3 replies, 8 with 2, 6 with 1;
    // LIMIT 5. The 5th-largest count is 2, so the six 1-reply messages are dropped
    // BEFORE finalize and the many 2-reply ties AT the boundary are kept — the
    // pre-limit must produce the SAME top-5 as sorting all 17 groups.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut id = 0i64;
    let mut mk = |replies: usize| {
        let m = node(&g, "Message", &[("id", i(id))]);
        id += 1;
        for _ in 0..replies {
            let c = node(&g, "Comment", &[]);
            rel(&g, c, "REPLY_OF", m);
        }
    };
    for _ in 0..3 {
        mk(3);
    }
    for _ in 0..8 {
        mk(2);
    }
    for _ in 0..6 {
        mk(1);
    }
    let src = "MATCH (c:Comment)-[:REPLY_OF]->(m:Message) \
        RETURN m.id AS msg, count(c) AS replies ORDER BY replies DESC, msg ASC LIMIT 5";
    let (deg, general, interp) = three_deg(&g, src);
    assert_eq!(
        deg, general,
        "pre-limit vs general disagree at the boundary tie"
    );
    assert_eq!(
        deg, interp,
        "pre-limit vs interp disagree at the boundary tie"
    );
    // top-5: ids 0,1,2 (count 3), then ids 3,4 (count 2, lowest ids among the ties).
    assert_eq!(
        deg,
        vec![
            vec![i(0), i(3)],
            vec![i(1), i(3)],
            vec![i(2), i(3)],
            vec![i(3), i(2)],
            vec![i(4), i(2)],
        ],
        "the pre-limit must keep the exact byte-identical top-5"
    );
    assert!(deg_fired(&g, src));
}
