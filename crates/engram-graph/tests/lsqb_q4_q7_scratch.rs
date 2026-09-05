#![allow(non_snake_case)]
//! SCRATCH ATTRIBUTION TEST — miniature LSQB q4/q7 (SF1 measured q4 6,850 ms vs
//! q7 187,548 ms on the pod; this pins WHICH engine path each shape takes).
//!
//! q4 (one MATCH, three comma paths, `RETURN count(*)`) must be CLAIMED by the
//! columnar AGGREGATE pipeline (`recognise_aggregate` → `run_aggregate`, the
//! "interp.pipeline aggregate runs" counter). q7 (same base MATCH but the LIKES
//! and REPLY_OF legs as TWO `OPTIONAL MATCH` clauses) must be CLAIMED by the
//! columnar OPTIONAL left join — `recognise_optional` accepts `[Match,
//! Match(opt)+, {Return|With→Return}]` and `run_optional` runs one
//! `left_join_null_extend` round per clause (`fuse_consecutive_matches` still
//! never fuses an OPTIONAL: two clauses null-fill independently, one fused
//! pattern would not). Its aggregate tail records BOTH the aggregate and the
//! optional counters. A single-OPTIONAL variant of the same query is the
//! control. All three agree ON == OFF, row-for-row.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// Tag t0,t1; Message m0..m2; Person pe0..pe2; Comment c0,c1.
/// HAS_TAG (message->tag): m0->t0, m1->t0, m2->t1.
/// HAS_CREATOR (message->person): m0->pe0, m1->pe1, m2->pe2.
/// LIKES (person->message): pe1->m0, pe2->m0, pe0->m1 — m2 has NO likes.
/// REPLY_OF (comment->message): c0->m0, c1->m0 — m1,m2 have NO replies.
///
/// q4 (inner join): m0 contributes 2 likers × 2 comments = 4; m1 has no
/// comment, m2 no liker → total 4.
/// q7 (two left joins): m0 → 4; m1 → 1 (liker pe0, null comment); m2 → 1
/// (null liker, null comment) → total 6.
fn lsqb() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mk = |label: &str, key: &str, v: i64| {
        let mut p = BTreeMap::new();
        p.insert(key.to_string(), Value::Int(v));
        g.create_node(&[label.into()], &p).expect(label)
    };
    let t = [mk("Tag", "tid", 0), mk("Tag", "tid", 1)];
    let m = [
        mk("Message", "mid", 0),
        mk("Message", "mid", 1),
        mk("Message", "mid", 2),
    ];
    let pe = [
        mk("Person", "pid", 0),
        mk("Person", "pid", 1),
        mk("Person", "pid", 2),
    ];
    let c = [mk("Comment", "cid", 0), mk("Comment", "cid", 1)];
    for (msg, tag) in [(0, 0), (1, 0), (2, 1)] {
        g.create_rel(m[msg], "HAS_TAG", t[tag], &BTreeMap::new())
            .expect("HAS_TAG");
    }
    for (msg, person) in [(0, 0), (1, 1), (2, 2)] {
        g.create_rel(m[msg], "HAS_CREATOR", pe[person], &BTreeMap::new())
            .expect("HAS_CREATOR");
    }
    for (person, msg) in [(1, 0), (2, 0), (0, 1)] {
        g.create_rel(pe[person], "LIKES", m[msg], &BTreeMap::new())
            .expect("LIKES");
    }
    for (com, msg) in [(0, 0), (1, 0)] {
        g.create_rel(c[com], "REPLY_OF", m[msg], &BTreeMap::new())
            .expect("REPLY_OF");
    }
    g
}

const Q4: &str = "MATCH (:Tag)<-[:HAS_TAG]-(message:Message)-[:HAS_CREATOR]->(creator:Person), \
                  (message)<-[:LIKES]-(liker:Person), \
                  (message)<-[:REPLY_OF]-(comment:Comment) \
                  RETURN count(*) AS n";

const Q7: &str = "MATCH (:Tag)<-[:HAS_TAG]-(message:Message)-[:HAS_CREATOR]->(creator:Person) \
                  OPTIONAL MATCH (message)<-[:LIKES]-(liker:Person) \
                  OPTIONAL MATCH (message)<-[:REPLY_OF]-(comment:Comment) \
                  RETURN count(*) AS n";

/// The control: q7 with only ONE of its optional legs — the shape
/// `recognise_optional` accepts.
const Q7_ONE_OPT: &str =
    "MATCH (:Tag)<-[:HAS_TAG]-(message:Message)-[:HAS_CREATOR]->(creator:Person) \
     OPTIONAL MATCH (message)<-[:LIKES]-(liker:Person) \
     RETURN count(*) AS n";

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// Run `src` with the pipeline ON then OFF; return both row sets.
fn both(g: &Graph, src: &str) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_columnar_scans(true);
    let on = rows(g, src);
    g.set_columnar_scans(false);
    let off = rows(g, src);
    g.set_columnar_scans(true);
    (on, off)
}

/// One row set per engine path.
type Rows = Vec<Vec<Value>>;

/// The OPTIONAL FOLD's differential: the fold ON (the default), the fold OFF
/// (every leg expanded and merged), and columnar OFF (the per-tuple path).
fn optional_triple(g: &Graph, src: &str) -> (Rows, Rows, Rows) {
    g.set_columnar_scans(true);
    engram_graph::pipeline::set_count_fold(true);
    let on = rows(g, src);
    engram_graph::pipeline::set_count_fold(false);
    let fold_off = rows(g, src);
    engram_graph::pipeline::set_count_fold(true);
    g.set_columnar_scans(false);
    let general = rows(g, src);
    g.set_columnar_scans(true);
    (on, fold_off, general)
}

fn optional_fold_fired(g: &Graph, src: &str) -> u64 {
    g.set_columnar_scans(true);
    engram_graph::pipeline::set_count_fold(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace
        .counters()
        .get("interp.pipeline optional fold")
        .copied()
        .unwrap_or(0)
}

/// The pipeline-operator counters for `src` with columnar ON:
/// (hop runs, aggregate runs, optional runs).
fn pipeline_counters(g: &Graph, src: &str) -> (u64, u64, u64) {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    let get = |k: &str| trace.counters().get(k).copied().unwrap_or(0);
    (
        get("interp.pipeline hop runs"),
        get("interp.pipeline aggregate runs"),
        get("interp.pipeline optional runs"),
    )
}

/// q4's comma-join + `count(*)` is claimed by the columnar AGGREGATE pipeline
/// and agrees with the general path.
#[test]
fn q4_shape_fires_the_aggregate_pipeline() {
    let g = lsqb();
    let (on, off) = both(&g, Q4);
    assert_eq!(on, off, "columnar vs general disagree for q4");
    assert_eq!(on, vec![vec![Value::Int(4)]], "q4 inner-join count");
    let (hop, agg, opt) = pipeline_counters(&g, Q4);
    assert_eq!(
        (hop, agg, opt),
        (0, 1, 0),
        "q4 must run as ONE columnar aggregate (hop/agg/opt = {hop}/{agg}/{opt})"
    );
}

/// q7's TWO OPTIONAL MATCH clauses are claimed by the columnar OPTIONAL left
/// join (two null-extension rounds feeding ONE aggregate tail): the aggregate
/// and optional counters both record, the hop operator does not, and the
/// per-clause null-fill count (6) is the interpreter's.
#[test]
fn q7_shape_fires_the_optional_operator() {
    let g = lsqb();
    let (on, off) = both(&g, Q7);
    assert_eq!(on, off, "columnar vs general disagree for q7");
    assert_eq!(on, vec![vec![Value::Int(6)]], "q7 double-left-join count");
    let (hop, agg, opt) = pipeline_counters(&g, Q7);
    assert_eq!(
        (hop, agg, opt),
        (0, 1, 1),
        "q7 must run as the columnar OPTIONAL left join (hop/agg/opt = {hop}/{agg}/{opt})"
    );
}

/// The control: q7 with ONE optional leg takes the same operator, with one
/// fewer null-extension round.
#[test]
fn q7_with_one_optional_fires_the_optional_operator() {
    let g = lsqb();
    let (on, off) = both(&g, Q7_ONE_OPT);
    assert_eq!(on, off, "columnar vs general disagree for the control");
    // m0 → 2 likers, m1 → 1, m2 → 1 null row: count(*) = 4.
    assert_eq!(on, vec![vec![Value::Int(4)]], "single-left-join count");
    // The Agg tail of `run_optional` runs through `finish_aggregate` then
    // re-wraps with `finish_optional`, so BOTH counters record (pipeline.rs,
    // `run_optional`'s tail dispatch).
    let (hop, agg, opt) = pipeline_counters(&g, Q7_ONE_OPT);
    assert_eq!(
        (hop, agg, opt),
        (0, 1, 1),
        "one OPTIONAL must fire the left-join operator (hop/agg/opt = {hop}/{agg}/{opt})"
    );
}

/// OPERATOR D — the OPTIONAL FOLD. q7's two legs bind nothing the statement
/// reads and every site is `count(*)`, so each leg is COUNTED per outer row
/// (weight `max(1, matches)` — the null-fill row counts as ONE under
/// `count(*)`) instead of expanded and merged. Both legs fold, so the counter
/// records twice; the count is unchanged on all three paths.
#[test]
fn q7_legs_fold_and_agree() {
    let g = lsqb();
    let (on, fold_off, general) = optional_triple(&g, Q7);
    assert_eq!(on, general, "optional fold ON vs general disagree");
    assert_eq!(fold_off, general, "optional fold OFF vs general disagree");
    assert_eq!(on, vec![vec![Value::Int(6)]], "q7 double-left-join count");
    assert_eq!(
        optional_fold_fired(&g, Q7),
        2,
        "one fold per OPTIONAL clause"
    );
    // The control folds its single leg.
    let (on, fold_off, general) = optional_triple(&g, Q7_ONE_OPT);
    assert_eq!(on, general);
    assert_eq!(fold_off, general);
    assert_eq!(on, vec![vec![Value::Int(4)]]);
    assert_eq!(optional_fold_fired(&g, Q7_ONE_OPT), 1);
    // With the fold lever OFF the legs are expanded and merged as before.
    engram_graph::pipeline::set_count_fold(false);
    let (_, trace) = engram_observe::with_trace(|| rows(&g, Q7));
    engram_graph::pipeline::set_count_fold(true);
    assert_eq!(
        trace
            .counters()
            .get("interp.pipeline optional fold")
            .copied(),
        None,
        "the lever is read at plan time"
    );
}

/// The legs that must NOT fold. `count(liker)` counts a null-fill row as ZERO,
/// which `max(1, ·)` would get wrong; a leg var read by the projection has no
/// column to be read from; a leg WHERE is not evaluated inside the fold; and a
/// leg binding a RELATIONSHIP variable has no real column for it either. Each
/// falls back to the ordinary left join and still agrees.
#[test]
fn a_leg_the_fold_must_not_claim_falls_back() {
    let g = lsqb();
    let base = "MATCH (:Tag)<-[:HAS_TAG]-(message:Message)-[:HAS_CREATOR]->(creator:Person) \
                OPTIONAL MATCH (message)<-[:LIKES]-(liker:Person) ";
    for tail in [
        // A non-star site over the nullable var: the null-fill row counts 0.
        "RETURN count(liker) AS n",
        "RETURN count(*) AS n, count(liker) AS m",
        // The leg var is READ by a grouping key.
        "RETURN liker.pid AS k, count(*) AS n ORDER BY k",
        // A leg WHERE.
        "WHERE liker.pid > 0 RETURN count(*) AS n",
    ] {
        let src = format!("{base}{tail}");
        let (on, fold_off, general) = optional_triple(&g, &src);
        assert_eq!(on, general, "columnar vs general: `{src}`");
        assert_eq!(fold_off, general, "fold OFF vs general: `{src}`");
        assert_eq!(
            optional_fold_fired(&g, &src),
            0,
            "the optional fold must DECLINE: `{src}`"
        );
    }
    // A leg binding a RELATIONSHIP variable the query never reads still
    // declines: `semijoin`/`expand` give it a real column and the fold has only
    // a placeholder.
    let rel_var = "MATCH (:Tag)<-[:HAS_TAG]-(message:Message)-[:HAS_CREATOR]->(creator:Person) \
                   OPTIONAL MATCH (message)<-[r:LIKES]-(liker:Person) RETURN count(*) AS n";
    let (on, fold_off, general) = optional_triple(&g, rel_var);
    assert_eq!(on, general);
    assert_eq!(fold_off, general);
    assert_eq!(optional_fold_fired(&g, rel_var), 0);
}
