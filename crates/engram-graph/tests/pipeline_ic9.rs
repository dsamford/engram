#![allow(non_snake_case)]
//! Differential tests for LDBC SNB IC9 firing the PLAIN multi-stage pipeline
//! (`pipeline::run_multistage`) rather than declining to `run_streaming`.
//!
//! IC9's shape is `[Match(var-length, inline/param-anchored, WHERE person<>friend),
//! With(DISTINCT friend), Match(single: friend<-HAS_CREATOR-message + WHERE),
//! Return(top-k)]`:
//!
//! ```cypher
//! MATCH (person:Person {id:$pid})-[:KNOWS*1..2]-(friend:Person)
//! WHERE person <> friend WITH DISTINCT friend
//! MATCH (friend)<-[:HAS_CREATOR]-(message:Message)
//! WHERE message.creationDate < $date
//! RETURN message.id, friend.id, message.content, message.creationDate
//! ORDER BY message.creationDate DESC, message.id ASC LIMIT 20
//! ```
//!
//! Stage 1 now reuses the SAME machinery `recognise_multistage_join` uses: the
//! inline `{id:$pid}` / `person.id = $pid` seek anchor (`s1_anchor`), the
//! frontier-BFS var-length hop consumed DISTINCT-only by the WITH, and the split
//! conjunction WHERE (incl. the two-var `person <> friend`). Stage 2 (a single
//! hop out of `friend`, a single-var WHERE on `message`, a top-k RETURN) was
//! already handled by the plain multistage tail and is untouched.
//!
//! THE CONTRACT: for every accepted shape, `set_columnar_scans(true)` (the
//! pipeline) equals `set_columnar_scans(false)` (`run_streaming`) — the full ROW
//! SET *and its order*, byte-for-byte — the pipeline FIRES (the 'multistage runs'
//! counter, NOT `interp.streamed a read-only chain`), and full-node
//! materialisation is BOUNDED BY the LIMIT (late-materialise of the top-k only),
//! not one full node per reached message. Declined shapes fall back and still
//! agree, without firing.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// The IC9 fixture. Persons p0..p4 (id 100..104); Messages via HAS_CREATOR
/// (Message->Person) carrying `id` / `content` / `creationDate`.
///
///   KNOWS (Person->Person, directed; traversed UNDIRECTED `*1..2`):
///     p0->p1, p1->p2, p2->p3. p4 is ISOLATED. So an undirected `*1..2` from p0
///     reaches {p0,p1,p2}; `WHERE person <> friend` drops p0, leaving the
///     DISTINCT-friend reach set {p1,p2}. p3 is 3 hops away (NOT reached) and p4
///     never (isolated) — the reach-set restriction the anchor + BFS enforce.
///   HAS_CREATOR (Message->Person) with (id, creationDate, content):
///     p1 (id 101): m10(cd 500,'a'), m11(cd 300,'b'), m16(cd 350,'g')
///     p2 (id 102): m12(cd 400,'c'), m13(cd 600,'d'), m17(cd 450,'h')
///     p3 (id 103): m14(cd 550,'e')  — creator NOT reached, EXCLUDED
///     p0 (id 100): m15(cd 700,'f')  — creator is `person`, dropped by person<>friend
///
/// With `$pid = 100`, `$date = 650`, the qualifying (friend, message) rows are
/// p1/p2's six messages (all cd < 650); p3's and p0's are excluded. Ordered by
/// `creationDate DESC, id ASC` that is m13,m10,m17,m12,m16,m11.
fn gic9() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let p: Vec<u64> = (0..5)
        .map(|k| {
            let mut m = BTreeMap::new();
            m.insert("id".to_string(), Value::Int(100 + k));
            g.create_node(&["Person".into()], &m).expect("person")
        })
        .collect();
    // KNOWS — p4 deliberately isolated; p3 reachable only in 3 hops from p0.
    for (s, d) in [(0usize, 1usize), (1, 2), (2, 3)] {
        g.create_rel(p[s], "KNOWS", p[d], &BTreeMap::new())
            .expect("KNOWS");
    }
    // (creator index, message id, creationDate, content).
    let msgs: &[(usize, i64, i64, &str)] = &[
        (1, 10, 500, "a"),
        (1, 11, 300, "b"),
        (1, 16, 350, "g"),
        (2, 12, 400, "c"),
        (2, 13, 600, "d"),
        (2, 17, 450, "h"),
        (3, 14, 550, "e"),
        (0, 15, 700, "f"),
    ];
    for &(creator, id, cd, content) in msgs {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(id));
        m.insert("creationDate".to_string(), Value::Int(cd));
        m.insert("content".to_string(), Value::Str(content.to_string()));
        let msg = g.create_node(&["Message".into()], &m).expect("message");
        g.create_rel(msg, "HAS_CREATOR", p[creator], &BTreeMap::new())
            .expect("HAS_CREATOR");
    }
    g
}

/// A TIE fixture: every message shares one creationDate, so `ORDER BY
/// creationDate DESC, id ASC` degenerates to `id ASC` and a LIMIT cuts a run of
/// ties. p0->p1->p2 KNOWS; friends of p0 = {p1,p2}. p1 authors ids 20,21,22 and
/// p2 authors 23,24, all cd 500 — `LIMIT 3` keeps {20,21,22} and the boundary tie
/// (id 22 in, id 23 out, both cd 500) is decided by the `id ASC` tiebreak.
fn gic9_ties() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let p: Vec<u64> = (0..3)
        .map(|k| {
            let mut m = BTreeMap::new();
            m.insert("id".to_string(), Value::Int(100 + k));
            g.create_node(&["Person".into()], &m).expect("person")
        })
        .collect();
    for (s, d) in [(0usize, 1usize), (1, 2)] {
        g.create_rel(p[s], "KNOWS", p[d], &BTreeMap::new())
            .expect("KNOWS");
    }
    for &(creator, id) in &[(1usize, 20i64), (1, 21), (1, 22), (2, 23), (2, 24)] {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(id));
        m.insert("creationDate".to_string(), Value::Int(500));
        m.insert("content".to_string(), Value::Str(format!("t{id}")));
        let msg = g.create_node(&["Message".into()], &m).expect("message");
        g.create_rel(msg, "HAS_CREATOR", p[creator], &BTreeMap::new())
            .expect("HAS_CREATOR");
    }
    g
}

/// A LARGE-fan-out fixture: friends {p1,p2} of p0 author MANY messages (20 each,
/// 40 qualifying), so a small `LIMIT` makes the late-materialise bound visible —
/// the pipeline decodes full nodes for the top-k winners ONLY, while the streamed
/// oracle decodes one per reached message (the SF1 665ms `mat_node` floor, in
/// miniature).
fn gic9_big() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let p: Vec<u64> = (0..3)
        .map(|k| {
            let mut m = BTreeMap::new();
            m.insert("id".to_string(), Value::Int(100 + k));
            g.create_node(&["Person".into()], &m).expect("person")
        })
        .collect();
    for (s, d) in [(0usize, 1usize), (1, 2)] {
        g.create_rel(p[s], "KNOWS", p[d], &BTreeMap::new())
            .expect("KNOWS");
    }
    // 40 messages: creators alternate p1/p2, distinct ids and creationDates, all
    // below the $date cutoff so every one qualifies (the enumerated set the
    // streamed path must decode in full).
    for id in 0i64..40 {
        let creator = if id % 2 == 0 { 1usize } else { 2usize };
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(id));
        m.insert("creationDate".to_string(), Value::Int(1000 + id));
        m.insert("content".to_string(), Value::Str(format!("c{id}")));
        let msg = g.create_node(&["Message".into()], &m).expect("message");
        g.create_rel(msg, "HAS_CREATOR", p[creator], &BTreeMap::new())
            .expect("HAS_CREATOR");
    }
    g
}

fn rows(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse '{src}': {e}"));
    run_query(g, &q, params)
        .unwrap_or_else(|e| panic!("run '{src}': {e}"))
        .rows
}

/// Run `src` with the pipeline ON and the general path OFF, returning both row
/// sets (ORDER preserved — the order is under test).
fn both(
    g: &Graph,
    src: &str,
    params: BTreeMap<String, Value>,
) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_columnar_scans(true);
    let on = rows(g, src, params.clone());
    g.set_columnar_scans(false);
    let off = rows(g, src, params);
    g.set_columnar_scans(true);
    (on, off)
}

/// Whether the PLAIN multi-stage pipeline fired for `src` under `params`.
fn fired(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, params));
    trace
        .counters()
        .get("interp.pipeline multistage runs")
        .copied()
        .unwrap_or(0)
        == 1
}

/// Whether `src` fell to the nested `run_streaming` path (columnar ON) — the
/// marker an accepted IC9 must NOT trip and a full decline MUST.
fn streamed(g: &Graph, src: &str, params: BTreeMap<String, Value>) -> bool {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, params));
    trace
        .sometimes_hit()
        .contains("interp.streamed a read-only chain")
}

/// The value of a named counter after running `src` with columnar `on`.
fn counter(g: &Graph, src: &str, params: BTreeMap<String, Value>, on: bool, key: &str) -> u64 {
    g.set_columnar_scans(on);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src, params));
    g.set_columnar_scans(true);
    trace.counters().get(key).copied().unwrap_or(0)
}

/// TOTAL per-node materialisations for `src`: full decodes (`graph.node`) PLUS
/// projected decodes (`graph.node_projected`). This is the `mat_node` cost the
/// streamed path pays once per reached message; the pipeline pays it only for the
/// late-materialised top-k winners.
fn mat_total(g: &Graph, src: &str, params: BTreeMap<String, Value>, on: bool) -> u64 {
    counter(
        g,
        src,
        params.clone(),
        on,
        "graph.nodes materialised in full",
    ) + counter(g, src, params, on, "graph.projected node materialisations")
}

fn i(n: i64) -> Value {
    Value::Int(n)
}

fn s(x: &str) -> Value {
    Value::Str(x.to_string())
}

/// `$pid`/`$date` params keyed WITHOUT the `$` (as `run_query` expects).
fn ic9_params(pid: i64, date: i64) -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("pid".to_string(), i(pid));
    p.insert("date".to_string(), i(date));
    p
}

/// The full IC9 statement with an INLINE `{id: $pid}` start anchor.
const IC9_INLINE: &str = "MATCH (person:Person {id: $pid})-[:KNOWS*1..2]-(friend:Person) \
     WHERE person <> friend \
     WITH DISTINCT friend \
     MATCH (friend)<-[:HAS_CREATOR]-(message:Message) \
     WHERE message.creationDate < $date \
     RETURN message.id AS mid, friend.id AS fid, message.content AS content, \
            message.creationDate AS cd \
     ORDER BY message.creationDate DESC, message.id ASC LIMIT 20";

/// The same statement with the anchor expressed as a conjunctive WHERE equality
/// `person.id = $pid AND person <> friend` (no inline map).
const IC9_PARAM_WHERE: &str = "MATCH (person:Person)-[:KNOWS*1..2]-(friend:Person) \
     WHERE person.id = $pid AND person <> friend \
     WITH DISTINCT friend \
     MATCH (friend)<-[:HAS_CREATOR]-(message:Message) \
     WHERE message.creationDate < $date \
     RETURN message.id AS mid, friend.id AS fid, message.content AS content, \
            message.creationDate AS cd \
     ORDER BY message.creationDate DESC, message.id ASC LIMIT 20";

/// The LITERAL IC9 ORDER BY: the keys are the RETURN ALIASES (`cd`, `mid`), NOT
/// the pattern properties. This is the shape the real LDBC query ships and the
/// one that used to DECLINE to `run_streaming` (`classify_key` resolved only
/// pattern vars/props); the alias resolver now sorts by the expressions `cd` /
/// `mid` project, so stage-2's core top-k fires identically to the pattern-prop
/// form.
const IC9_ALIAS_ORDER: &str = "MATCH (person:Person {id: $pid})-[:KNOWS*1..2]-(friend:Person) \
     WHERE person <> friend \
     WITH DISTINCT friend \
     MATCH (friend)<-[:HAS_CREATOR]-(message:Message) \
     WHERE message.creationDate < $date \
     RETURN message.id AS mid, friend.id AS fid, message.content AS content, \
            message.creationDate AS cd \
     ORDER BY cd DESC, mid ASC LIMIT 20";

/// The six qualifying rows in `creationDate DESC, id ASC` order (the whole
/// LIMIT-20 result on `gic9()` with `$pid=100`, `$date=650`).
fn expected_full() -> Vec<Vec<Value>> {
    vec![
        vec![i(13), i(102), s("d"), i(600)],
        vec![i(10), i(101), s("a"), i(500)],
        vec![i(17), i(102), s("h"), i(450)],
        vec![i(12), i(102), s("c"), i(400)],
        vec![i(16), i(101), s("g"), i(350)],
        vec![i(11), i(101), s("b"), i(300)],
    ]
}

// ─── ACCEPTS ──────────────────────────────────────────────────────────────────

/// IC9 with an INLINE `{id: $pid}` start anchor. ON==OFF row-for-row AND in order;
/// the pipeline FIRES; it does NOT stream. The reach-set restriction is
/// load-bearing: p3's message (m14, creator 3 hops away) and p0's (m15, `person`
/// itself, dropped by `person <> friend`) are BOTH absent.
#[test]
fn ic9_inline_anchor_on_equals_off_and_fires() {
    let g = gic9();
    let params = ic9_params(100, 650);
    let (on, off) = both(&g, IC9_INLINE, params.clone());
    assert_eq!(
        on, off,
        "inline-anchor IC9 ON must equal OFF row-for-row and in order"
    );
    assert_eq!(
        on,
        expected_full(),
        "IC9 must return the six reached-friend messages, cd DESC then id ASC"
    );
    assert!(
        fired(&g, IC9_INLINE, params.clone()),
        "the inline-anchor IC9 must FIRE the multistage pipeline"
    );
    assert!(
        !streamed(&g, IC9_INLINE, params),
        "the inline-anchor IC9 must NOT stream"
    );
}

/// IC9 with the anchor as a CONJUNCTIVE WHERE `person.id = $pid AND person <>
/// friend` (no inline map) — the split-conjunction + seek path. Same rows, order,
/// firing as the inline form.
#[test]
fn ic9_param_where_anchor_on_equals_off_and_fires() {
    let g = gic9();
    let params = ic9_params(100, 650);
    let (on, off) = both(&g, IC9_PARAM_WHERE, params.clone());
    assert_eq!(on, off, "conjunctive-WHERE IC9 ON must equal OFF");
    assert_eq!(
        on,
        expected_full(),
        "the WHERE-equality anchor must yield the same six rows as the inline form"
    );
    assert!(
        fired(&g, IC9_PARAM_WHERE, params.clone()),
        "the WHERE-anchored IC9 must FIRE"
    );
    assert!(
        !streamed(&g, IC9_PARAM_WHERE, params),
        "the WHERE-anchored IC9 must NOT stream"
    );
}

/// A LITERAL inline anchor `{id: 100}` (no param) drives the seed exactly as the
/// `$pid` form — the `is_scalar_or_param` anchor accepts a literal too.
#[test]
fn ic9_literal_inline_anchor_fires() {
    let g = gic9();
    let src = "MATCH (person:Person {id: 100})-[:KNOWS*1..2]-(friend:Person) \
         WHERE person <> friend WITH DISTINCT friend \
         MATCH (friend)<-[:HAS_CREATOR]-(message:Message) \
         WHERE message.creationDate < 650 \
         RETURN message.id AS mid, friend.id AS fid, message.content AS content, \
                message.creationDate AS cd \
         ORDER BY message.creationDate DESC, message.id ASC LIMIT 20";
    let (on, off) = both(&g, src, BTreeMap::new());
    assert_eq!(on, off, "literal-anchor IC9 ON must equal OFF");
    assert_eq!(on, expected_full(), "literal anchor yields the six rows");
    assert!(
        fired(&g, src, BTreeMap::new()),
        "literal-anchor IC9 must FIRE"
    );
}

/// The LITERAL IC9 ORDER BY over the RETURN ALIASES (`ORDER BY cd DESC, mid ASC`)
/// — the shape that used to DECLINE. Stage-2's core top-k now resolves each alias
/// to the expression it projects (`cd` -> `message.creationDate`, `mid` ->
/// `message.id`), so it FIRES and produces the SAME six rows, in the SAME order,
/// as the pattern-property form. ON==OFF; fires; does not stream.
#[test]
fn ic9_alias_order_by_fires_equals_pattern_prop() {
    let g = gic9();
    let params = ic9_params(100, 650);
    let (on, off) = both(&g, IC9_ALIAS_ORDER, params.clone());
    assert_eq!(
        on, off,
        "alias-ORDER-BY IC9 ON must equal OFF row-for-row and in order"
    );
    assert_eq!(
        on,
        expected_full(),
        "ORDER BY the aliases must yield the same six rows as ORDER BY the properties"
    );
    // The pattern-prop form and the alias form must agree exactly — the alias
    // resolves to the projected expression, nothing else.
    let (prop_on, _) = both(&g, IC9_INLINE, params.clone());
    assert_eq!(
        on, prop_on,
        "ORDER BY alias must equal ORDER BY the aliased pattern property"
    );
    assert!(
        fired(&g, IC9_ALIAS_ORDER, params.clone()),
        "the alias-ORDER-BY IC9 must FIRE the multistage pipeline"
    );
    assert!(
        !streamed(&g, IC9_ALIAS_ORDER, params),
        "the alias-ORDER-BY IC9 must NOT stream"
    );
}

/// TIES at the LIMIT boundary. Every message shares `creationDate = 500`, so the
/// order is the `id ASC` tiebreak and `LIMIT 3` keeps exactly {20,21,22}: the
/// boundary tie (id 22 kept, id 23 dropped, both cd 500) is resolved identically
/// by the pipeline's `native_topk` and the streamed oracle. ON==OFF (order); fires.
#[test]
fn ic9_ties_at_limit_boundary() {
    let g = gic9_ties();
    let params = ic9_params(100, 600);
    let src = "MATCH (person:Person {id: $pid})-[:KNOWS*1..2]-(friend:Person) \
         WHERE person <> friend WITH DISTINCT friend \
         MATCH (friend)<-[:HAS_CREATOR]-(message:Message) \
         WHERE message.creationDate < $date \
         RETURN message.id AS mid, friend.id AS fid, message.creationDate AS cd \
         ORDER BY message.creationDate DESC, message.id ASC LIMIT 3";
    let (on, off) = both(&g, src, params.clone());
    assert_eq!(on, off, "tie-boundary IC9 ON must equal OFF in order");
    assert_eq!(
        on,
        vec![
            vec![i(20), i(101), i(500)],
            vec![i(21), i(101), i(500)],
            vec![i(22), i(101), i(500)],
        ],
        "the id-ASC tiebreak keeps {{20,21,22}} at the LIMIT-3 boundary"
    );
    assert!(fired(&g, src, params), "the tie-boundary IC9 must FIRE");
}

/// LATE-MATERIALISE: with 40 qualifying messages and `LIMIT 5`, the pipeline
/// decodes nodes only for the top-k winners (friend + message per winner, bounded
/// by `2 * LIMIT`) and reads the ORDER BY keys from gathered columns (ZERO per-row
/// projected node decodes), while the streamed oracle pays a `mat_node` (projected)
/// decode once per reached message — the SF1 665ms `mat_node` floor the pipeline
/// removes. ON==OFF; the pipeline's TOTAL node materialisations are bounded by
/// `2 * LIMIT` and STRICTLY below the stream's.
#[test]
fn ic9_materialisation_bounded_by_k() {
    let g = gic9_big();
    let params = ic9_params(100, 5000);
    let src = "MATCH (person:Person {id: $pid})-[:KNOWS*1..2]-(friend:Person) \
         WHERE person <> friend WITH DISTINCT friend \
         MATCH (friend)<-[:HAS_CREATOR]-(message:Message) \
         WHERE message.creationDate < $date \
         RETURN message.id AS mid, friend.id AS fid, message.content AS content, \
                message.creationDate AS cd \
         ORDER BY message.creationDate DESC, message.id ASC LIMIT 5";
    let (on, off) = both(&g, src, params.clone());
    assert_eq!(on, off, "large-fan-out IC9 ON must equal OFF");
    assert_eq!(on.len(), 5, "LIMIT 5 returns five rows");
    assert!(
        fired(&g, src, params.clone()),
        "the large-fan-out IC9 must FIRE"
    );

    let limit = 5u64;
    // The pipeline late-materialises full nodes ONLY for the top-k winners
    // (friend + message per winner => bounded by 2 * LIMIT) and does ZERO per-row
    // projected node reads — the ORDER BY keys come from gathered columns.
    let on_full = counter(
        &g,
        src,
        params.clone(),
        true,
        "graph.nodes materialised in full",
    );
    let on_projected = counter(
        &g,
        src,
        params.clone(),
        true,
        "graph.projected node materialisations",
    );
    assert!(
        on_full <= 2 * limit,
        "the pipeline must late-materialise only the top-k (<= 2*LIMIT = {}), got {on_full}",
        2 * limit
    );
    assert_eq!(
        on_projected, 0,
        "the pipeline reads ORDER BY keys via columns, never a per-row projected node decode"
    );

    // The streamed oracle pays a `mat_node` (projected) decode once per reached
    // message — 40 qualifying here, ~100k at SF1 — the dominant cost the pipeline
    // avoids. Compare TOTAL node materialisations (full + projected).
    let on_total = mat_total(&g, src, params.clone(), true);
    let off_total = mat_total(&g, src, params, false);
    assert!(
        off_total >= 40,
        "the streamed oracle materialises one node per reached message (>= 40), got {off_total}"
    );
    assert!(
        on_total <= 2 * limit,
        "the pipeline's total node materialisations must be bounded by k, got {on_total}"
    );
    assert!(
        on_total < off_total,
        "pipeline materialisations ({on_total}) must be far below the stream's ({off_total})"
    );
}

// ─── DECLINES ───────────────────────────────────────────────────────────────

/// Shapes IC9's stage-1 recognition must DECLINE — each falls back to the general
/// path (ON==OFF) and does NOT fire the multistage counter. Mirrors the join
/// path's declines: an UNBOUNDED var-length, a NON-SPLITTABLE (OR) WHERE, a
/// MULTI-ENTRY inline anchor map, and a var-length whose end is NOT consumed
/// DISTINCT-only by the WITH.
#[test]
fn ic9_declines_out_of_scope_stage1() {
    let g = gic9();
    let params = ic9_params(100, 650);
    let declines: &[&str] = &[
        // UNBOUNDED var-length `*1..` — `collect_hops` records only bounded
        // frontier-BFS hops; an open upper bound declines the whole query.
        "MATCH (person:Person {id: $pid})-[:KNOWS*1..]-(friend:Person) \
         WHERE person <> friend WITH DISTINCT friend \
         MATCH (friend)<-[:HAS_CREATOR]-(message:Message) \
         WHERE message.creationDate < $date \
         RETURN message.id AS mid ORDER BY message.creationDate DESC LIMIT 20",
        // NON-SPLITTABLE WHERE — a top-level OR is not a conjunction of tractable
        // predicates, so `recognise_where_preds` declines.
        "MATCH (person:Person {id: $pid})-[:KNOWS*1..2]-(friend:Person) \
         WHERE person <> friend OR friend.id = 999 WITH DISTINCT friend \
         MATCH (friend)<-[:HAS_CREATOR]-(message:Message) \
         WHERE message.creationDate < $date \
         RETURN message.id AS mid ORDER BY message.creationDate DESC LIMIT 20",
        // MULTI-ENTRY inline anchor map — `start_prop_anchor` accepts exactly one
        // scalar/param entry; two entries decline (`collect_hops` returns None).
        "MATCH (person:Person {id: $pid, foo: 1})-[:KNOWS*1..2]-(friend:Person) \
         WHERE person <> friend WITH DISTINCT friend \
         MATCH (friend)<-[:HAS_CREATOR]-(message:Message) \
         WHERE message.creationDate < $date \
         RETURN message.id AS mid ORDER BY message.creationDate DESC LIMIT 20",
        // A var-length hop whose end `friend` is carried by a PLAIN (non-DISTINCT)
        // WITH is NOT frontier-BFS-eligible — `varlen_distinct_consumed` declines.
        "MATCH (person:Person {id: $pid})-[:KNOWS*1..2]-(friend:Person) \
         WHERE person <> friend WITH friend \
         MATCH (friend)<-[:HAS_CREATOR]-(message:Message) \
         WHERE message.creationDate < $date \
         RETURN message.id AS mid ORDER BY message.creationDate DESC LIMIT 20",
    ];
    for src in declines {
        let (on, off) = both(&g, src, params.clone());
        assert_eq!(
            on, off,
            "declined IC9 shape must still agree via the general path: '{src}'"
        );
        assert!(
            !fired(&g, src, params.clone()),
            "this stage-1 shape must DECLINE the multistage pipeline: '{src}'"
        );
    }
}
