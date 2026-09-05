//! The single-primitive-key aggregate FAST PATH (`NativeKey`) proven byte-identical
//! to the general `agg_key_of` path AND to the row-at-a-time interp, on the three
//! eligible key types — `Str`, `Int`, `Null` — and on the two cases that make the
//! guard non-trivial:
//!
//!  * a NAME COLLISION, where two distinct nodes share a group-key property VALUE:
//!    grouping is by VALUE, so they MUST merge into one group. This is the exact
//!    reason a node-degree rewrite is NOT a drop-in substitute (it would group by
//!    node); the native-key path groups by value, so it merges — proven here.
//!  * a TIE under a PARTIAL order (`ORDER BY count DESC` with no unique tie-break),
//!    where the output order of the tied groups is FIRST-SEEN order; both paths
//!    must reproduce it identically.
//!
//! Each case also asserts the fast path actually FIRED (its counter incremented),
//! so an accidental silent fall-through to the general path could not pass.

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

/// (native-key fast path ON, general `agg_key_of` path, interp) — all three must
/// agree byte-for-byte. `set_agg_native_key(false)` forces the general columnar
/// path; `set_columnar_scans(false)` forces the interpreter. The degree
/// short-circuit is held OFF so these queries exercise the reduce path this file
/// covers (it otherwise supersedes the count-by-target shapes — see agg_degree.rs).
fn three(g: &Graph, src: &str) -> (Rows, Rows, Rows) {
    g.set_columnar_scans(true);
    g.set_degree_aggregate(false);
    g.set_agg_native_key(true);
    let native = rows(g, src);
    g.set_agg_native_key(false);
    let general = rows(g, src);
    g.set_agg_native_key(true);
    g.set_columnar_scans(false);
    let interp = rows(g, src);
    g.set_columnar_scans(true);
    g.set_degree_aggregate(true);
    (native, general, interp)
}

/// Whether the native-key fast path fired for this query (degree short-circuit off,
/// so the reduce path — where the native-key fast path lives — is the one taken).
fn fired(g: &Graph, src: &str) -> bool {
    g.set_columnar_scans(true);
    g.set_degree_aggregate(false);
    g.set_agg_native_key(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    g.set_degree_aggregate(true);
    trace
        .counters()
        .get("interp.pipeline aggregate native-key group-by")
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

#[test]
fn str_key_merges_a_name_collision_byte_identically() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // t1 and t3 SHARE the name "Music" — two distinct nodes, one group VALUE.
    let t1 = node(&g, "Tag", &[("name", s("Music"))]);
    let t2 = node(&g, "Tag", &[("name", s("Sport"))]);
    let t3 = node(&g, "Tag", &[("name", s("Music"))]);
    let p1 = node(&g, "Person", &[]);
    let p2 = node(&g, "Person", &[]);
    let p3 = node(&g, "Person", &[]);
    let p4 = node(&g, "Person", &[]);
    let p5 = node(&g, "Person", &[]);
    rel(&g, p1, "HAS_INTEREST", t1);
    rel(&g, p2, "HAS_INTEREST", t1);
    rel(&g, p3, "HAS_INTEREST", t3);
    rel(&g, p4, "HAS_INTEREST", t2);
    rel(&g, p5, "HAS_INTEREST", t3);

    let src = "MATCH (p:Person)-[:HAS_INTEREST]->(t:Tag) \
        RETURN t.name AS tag, count(p) AS people ORDER BY people DESC, tag ASC";
    let (native, general, interp) = three(&g, src);
    assert_eq!(native, general, "native-key vs general agg_key_of disagree");
    assert_eq!(native, interp, "native-key vs interp disagree");
    // "Music" merges t1 (2) + t3 (2) = 4; "Sport" = 1 — grouping by VALUE.
    assert_eq!(
        native,
        vec![vec![s("Music"), i(4)], vec![s("Sport"), i(1)]],
        "same-named tags must collapse into one group"
    );
    assert!(
        fired(&g, src),
        "the Str group-by must take the native-key fast path"
    );
}

#[test]
fn int_key_tie_keeps_first_seen_order_byte_identically() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let m1 = node(&g, "Message", &[("id", i(10))]);
    let m2 = node(&g, "Message", &[("id", i(20))]);
    let m3 = node(&g, "Message", &[("id", i(30))]);
    // m1: 2 replies, m2: 2 replies (a TIE), m3: 1 reply.
    for dst in [m1, m1, m2, m2, m3] {
        let c = node(&g, "Comment", &[]);
        rel(&g, c, "REPLY_OF", dst);
    }
    // ORDER BY replies DESC ONLY — m1,m2 tie, and the tie's output order is
    // first-seen order, which both paths must reproduce identically.
    let src = "MATCH (c:Comment)-[:REPLY_OF]->(m:Message) \
        RETURN m.id AS msg, count(c) AS replies ORDER BY replies DESC";
    let (native, general, interp) = three(&g, src);
    assert_eq!(native, general, "native-key vs general disagree on a tie");
    assert_eq!(native, interp, "native-key vs interp disagree on a tie");
    assert!(fired(&g, src), "the Int group-by must take the fast path");
}

#[test]
fn null_key_groups_missing_property_byte_identically() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // Some messages carry `lang`, some do not → a NULL group key alongside real ones.
    let m1 = node(&g, "Message", &[("lang", s("en"))]);
    let m2 = node(&g, "Message", &[("lang", s("en"))]);
    let m3 = node(&g, "Message", &[]);
    let m4 = node(&g, "Message", &[]);
    let m5 = node(&g, "Message", &[("lang", s("fr"))]);
    let p = node(&g, "Person", &[]);
    for m in [m1, m2, m3, m4, m5] {
        rel(&g, p, "LIKES", m);
    }
    let src = "MATCH (p:Person)-[:LIKES]->(m:Message) \
        RETURN m.lang AS lang, count(m) AS c ORDER BY c DESC, lang ASC";
    let (native, general, interp) = three(&g, src);
    assert_eq!(
        native, general,
        "native-key vs general disagree on a null key"
    );
    assert_eq!(
        native, interp,
        "native-key vs interp disagree on a null key"
    );
    assert!(
        fired(&g, src),
        "a null-bearing key column is still native-eligible"
    );
}

/// Byte-identity AT SCALE — the BI5 shape (`count` grouped by an Int identity
/// property over a full REPLY_OF scan) across many groups with pervasive count
/// ties, where a mis-keyed group or a first-seen-order divergence would actually
/// have room to appear. (Local timing showed ~1.19× here; the vs-Neo4j number
/// needs the SF corpus + pod, and the DECISIVE win is BI-2, so no clock lives in
/// the suite — the codebase disallows ambient `Instant`.)
#[test]
fn native_key_is_byte_identical_at_scale() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let n_msg: u64 = 2_000;
    let n_com: u64 = 20_000;
    let mut msgs = Vec::with_capacity(n_msg as usize);
    for k in 0..n_msg {
        msgs.push(node(&g, "Message", &[("id", i(k as i64))]));
    }
    for c in 0..n_com {
        let cid = node(&g, "Comment", &[]);
        // Deterministic spread (no ambient RNG): a multiplicative hash mod n_msg,
        // so many messages share a reply count → pervasive ties at the boundary.
        let target = (c.wrapping_mul(2_654_435_761) % n_msg) as usize;
        rel(&g, cid, "REPLY_OF", msgs[target]);
    }
    let src = "MATCH (c:Comment)-[:REPLY_OF]->(m:Message) \
        RETURN m.id AS msg, count(c) AS replies ORDER BY replies DESC, msg ASC LIMIT 20";
    let (native, general, interp) = three(&g, src);
    assert_eq!(native, general, "native-key vs general disagree at scale");
    assert_eq!(native, interp, "native-key vs interp disagree at scale");
    assert!(
        fired(&g, src),
        "the at-scale group-by must take the fast path"
    );
}
