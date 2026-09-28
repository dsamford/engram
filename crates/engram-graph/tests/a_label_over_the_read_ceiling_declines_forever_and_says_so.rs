#![allow(non_snake_case)]
//! Fix 118: three sites refuse a whole-label property read on the SAME
//! constant, and two of them are the sites that would MINT the column the
//! third needs. So a label past `WHOLE_LABEL_READ_MAX` declines the
//! vectorised read, can never acquire the column that would lift the
//! decline, and therefore declines identically for ever — the slow path is
//! permanent, not cold.
//!
//! Measured at SF1 before this counter existed: `plat-optional-count` took
//! 137 ms against Neo4j's 28, `store.gets` was 113,065 for a query returning
//! twenty-five rows, and eight seeds run cold then run again measured 213 ms
//! then 265 ms — no learning, ever. Minting the column from an unrelated
//! whole-label scan dropped the SAME statement from 214 ms to 26 on the same
//! server, changing no data, which is what proves it is residency and not
//! size. None of that was visible: a permanent decline that is silent reads
//! exactly like a fast path, and locating it took four measurements and a
//! twenty-five-agent source read.
//!
//! The ceiling is a lever here (`set_whole_label_read_max`) for the reason
//! `parallel_min_rows` is one: the behaviour only exists past the ceiling,
//! and building a 262,144-node label to exercise one branch is a benchmark,
//! not a test.
//!
//! Fix 121 then replaces the decline with a bounded gather of just the hop's
//! own ends, so a 3M-node label whose hop touches a few thousand pays for the
//! few thousand. Note what this file can prove about it: that it fires, and
//! that it changes no answer. It CANNOT prove the speed-up, because that is a
//! paged-store block-sharing property and these tests run in memory, where a
//! sorted gather of N ids costs the same N gets as N point reads.
//!
//! Canary: delete the `counted!` at batch.rs's decline and test `a` fails on
//! a zero count while every timing-free assertion still passes — which is
//! the point, because that was the state of the world for the whole of the
//! v82-v175 span.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const DECLINED_READ: &str = "interp.subquery hop declined: label over the whole-read ceiling";
const LOADED_WHOLE: &str = "interp.subquery hop loaded its far end's column whole";
const GETS: &str = "store.gets";
const GATHERED: &str = "interp.subquery hop gathered only its own ends";

const PERSONS: i64 = 40;
const MESSAGES: i64 = 4_000;

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse ddl"), BTreeMap::new()).expect("ddl");
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
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

/// The platform shape's corpus in miniature: persons who KNOW each other, and
/// messages with a `browserUsed` the predicate tests.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX snb_person_id FOR (n:Person) ON (n.id)");
    let mut persons = Vec::with_capacity(PERSONS as usize);
    for i in 0..PERSONS {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i));
        persons.push(g.create_node(&["Person".into()], &m).expect("person"));
    }
    // Person 0 knows the next eight; those eight are the shape's outer rows.
    for k in 1..9 {
        g.create_rel(persons[0], "KNOWS", persons[k], &BTreeMap::new())
            .expect("knows");
    }
    for i in 0..MESSAGES {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(i));
        // Every third message is Chrome, so the predicate is selective and
        // the count is fixed by arithmetic rather than by the first read.
        let browser = if i % 3 == 0 { "Chrome" } else { "Firefox" };
        m.insert("browserUsed".to_string(), Value::Str(browser.into()));
        let id = g.create_node(&["Message".into()], &m).expect("message");
        g.create_rel(
            id,
            "HAS_CREATOR",
            persons[(i % PERSONS) as usize],
            &BTreeMap::new(),
        )
        .expect("creator");
    }
    g
}

/// The platform shape's INNER LEG, written as the `COUNT {}` subquery that
/// `fold_chain_counts` rewrites the OPTIONAL form into.
///
/// It is spelled out rather than left as `OPTIONAL MATCH … count(m)` because
/// fix 125 changed which operator claims that statement: with the inline seed
/// anchor accepted, the vectorised OPTIONAL left join takes it and it never
/// reaches `count_hop_ends_vectorised` at all — `store.gets` fell from ~800
/// to 7 on this corpus. That win has its own file. What THIS one is about is
/// the ceiling behaviour of the subquery hop, so it names that hop directly
/// instead of depending on a rewrite that may re-route again.
const SHAPE: &str = "MATCH (f:Person) WHERE f.id >= 1 AND f.id <= 8 \
     RETURN f.id AS id, \
     COUNT { (f)<-[:HAS_CREATOR]-(m:Message) WHERE m.browserUsed = 'Chrome' } AS n \
     ORDER BY id";

/// Person p authors messages i where `i % PERSONS == p`: at stride 40 over
/// 4,000 that is a hundred each, ids p, p+40, …, p+3960. Of those the Chrome
/// ones are `i % 3 == 0`. A hundred ends per friend clears fix 121's floor
/// (`LEAN_COLUMN_BATCH` = 64), which is what makes the gather reachable here.
fn want() -> Vec<Vec<Value>> {
    (1..9)
        .map(|p: i64| {
            let chrome = (0..MESSAGES / PERSONS)
                .filter(|k| (p + PERSONS * k) % 3 == 0)
                .count() as i64;
            vec![Value::Int(p), Value::Int(chrome)]
        })
        .collect()
}

#[test]
fn a_a_label_over_the_ceiling_declines_the_read_and_counts_it() {
    let g = corpus();
    let expect = want();
    // Under the ceiling first: the vectorised read is taken, and NO decline
    // is counted. This is the control — without it, a counter that never
    // fires would pass the test below by accident.
    g.set_whole_label_read_max(u64::MAX);
    let (got, c) = traced(&g, SHAPE);
    assert_eq!(got, expect, "the answer under the ceiling");
    assert_eq!(
        count_of(&c, DECLINED_READ),
        0,
        "declined while UNDER the ceiling: {c:?}"
    );
    assert!(
        count_of(&c, LOADED_WHOLE) >= 1,
        "the whole-column load never ran, so the control proves nothing: {c:?}"
    );

    // Now past it. Same corpus, same statement, same answer — a different path.
    let g = corpus();
    g.set_whole_label_read_max(1);
    let (got, c) = traced(&g, SHAPE);
    assert_eq!(got, expect, "the answer past the ceiling must be IDENTICAL");
    assert!(
        count_of(&c, DECLINED_READ) >= 1,
        "past the ceiling the read did not decline, so the diagnosis is mis-sited: {c:?}"
    );
    assert_eq!(
        count_of(&c, LOADED_WHOLE),
        0,
        "past the ceiling it still loaded a column whole: {c:?}"
    );
}

#[test]
fn b_nothing_ever_mints_the_column_the_read_site_is_waiting_for() {
    // What makes the decline PERMANENT rather than cold: the read site needs
    // a resident column, and no site will mint one for a label over the
    // ceiling. Asserted as the observable consequence — across six runs the
    // whole-column load never happens — rather than by naming the two warm
    // sites, which live on the interpreter's matcher path and are simply not
    // reached by a statement that goes straight to the subquery hop. An
    // earlier draft asserted their counters here and failed for that reason.
    let g = corpus();
    g.set_whole_label_read_max(1);
    let mut declines = 0;
    for round in 0..6 {
        let (_, c) = traced(&g, SHAPE);
        assert_eq!(
            count_of(&c, LOADED_WHOLE),
            0,
            "round {round}: something minted the column, so the decline is not \
             permanent and the diagnosis is wrong: {c:?}"
        );
        declines += count_of(&c, DECLINED_READ);
    }
    assert!(
        declines >= 6,
        "the read site declined only {declines} times over six runs — it should \
         decline on every one, since nothing ever mints what it is waiting for"
    );
}

// The two MINT sites (`warm_label_columns_after_misses` at interp.rs:8200 and
// `warm_hop_end_columns` at interp.rs:8263) also refuse on this constant, and
// that is what makes the decline self-perpetuating. They are NOT asserted by
// counter here, and deliberately so: both live on the interpreter's matcher
// path, and every statement that reaches this label's property through a
// shape a test can write is claimed first by the vectorised pipeline. Two
// attempts to reach them — the folded OPTIONAL leg and a plain bind of the far
// end — both landed on `interp.pipeline hop runs` instead.
//
// Their refusal is established by reading the source, and its CONSEQUENCE is
// what `b_nothing_ever_mints_the_column_the_read_site_is_waiting_for` and
// `c_the_decline_never_lifts_no_matter_how_often_it_runs` assert observably:
// across six runs nothing mints the column and the read site declines every
// time. Asserting a counter that no reachable statement can fire would be a
// test that passes for the wrong reason.

#[test]
fn c_the_decline_never_lifts_no_matter_how_often_it_runs() {
    // The measured signature at SF1 was 213 ms then 265 ms over two passes of
    // the same eight seeds — no learning. Here the same claim is made without
    // a clock: the work is identical on every one of six executions.
    let g = corpus();
    g.set_whole_label_read_max(1);
    // The mint sites now consult the BYTE budget, and this corpus's column
    // fits it — so without this the column is minted and the gather never
    // runs. The gather is still the path whenever the column does NOT fit,
    // which is the real case it was written for (a 9M-row label whose values
    // are strings), so it is pinned here with the mint refused rather than
    // deleted.
    let expect = want();
    let mut gets = Vec::new();
    let mut declines = Vec::new();
    for round in 0..6 {
        let (got, c) = traced(&g, SHAPE);
        assert_eq!(got, expect, "round {round}");
        gets.push(count_of(&c, GETS));
        declines.push(count_of(&c, DECLINED_READ));
    }
    // The FIRST execution pays one-off warming that has nothing to do with
    // the ceiling (the Person seek's index, the label membership), so the
    // claim is about the steady state: from the second execution on, the work
    // is identical for ever. Measured here 136 then 89, 89, 89, 89, 89 — the
    // same shape as SF1's 213 ms then 265 ms with no trend.
    let steady = &gets[1..];
    assert!(
        steady.iter().all(|n| *n == steady[0]),
        "the record reads still changed after the first run, so something IS \
         learning and the decline is cold rather than permanent: {gets:?}"
    );
    assert!(
        steady[0] > 0,
        "the steady state reads nothing, so this corpus does not exercise the \
         per-end record path at all: {gets:?}"
    );
    assert!(
        declines.iter().all(|n| *n == declines[0] && *n >= 1),
        "the decline count changed across repeats: {declines:?}"
    );
    // And the control: under the ceiling the same six repeats read far less,
    // which is the difference the ceiling is costing.
    let g2 = corpus();
    g2.set_whole_label_read_max(u64::MAX);
    let (_, first) = traced(&g2, SHAPE);
    let (_, sixth) = (0..5).map(|_| traced(&g2, SHAPE)).last().expect("repeats");
    let _ = first;
    assert!(
        count_of(&sixth, GETS) < gets[0],
        "under the ceiling the shape read {} records, past it {} — the ceiling \
         is supposed to be the expensive path: {sixth:?}",
        count_of(&sixth, GETS),
        gets[0]
    );
}

#[test]
fn e_fix_121_gathers_only_the_hop_s_ends_instead_of_declining() {
    // The decline is now a CHOICE. Knowing the hop's ends before the column
    // loop turns the cost from |label| into |ends|, so a label far past the
    // ceiling whose hop touches a small fraction of it gathers exactly that
    // fraction. ON and OFF must agree on every answer — this is a cost change.
    let g = corpus();
    g.set_whole_label_read_max(1);
    let expect = want();

    // WARM FIRST. The first execution over a fresh corpus pays one-off costs
    // (the Person index, label membership) that have nothing to do with the
    // gather, and measuring arm one cold against arm two warm attributes that
    // difference to the fix. An earlier draft of this test did exactly that
    // and "proved" a win of 136 against 89 with the gather never running.
    let _ = rows(&g, SHAPE);

    engram_graph::pipeline::set_subquery_end_gather(false);
    let (off_rows, off) = traced(&g, SHAPE);
    engram_graph::pipeline::set_subquery_end_gather(true);
    let (on_rows, on) = traced(&g, SHAPE);

    assert_eq!(off_rows, expect, "the OFF arm's answer");
    assert_eq!(
        on_rows, expect,
        "the ON arm changed an ANSWER, not just a cost"
    );

    assert!(
        count_of(&on, GATHERED) > 0,
        "the gather never ran, so this test proves nothing about fix 121 — the \
         fan-out is probably under its floor: {on:?}"
    );
    assert_eq!(
        count_of(&off, GATHERED),
        0,
        "the gather ran with its lever OFF: {off:?}"
    );
    // What this test can and cannot prove. On an IN-MEMORY store a sorted
    // gather of N ids costs the same N `store.gets` as N scattered point
    // reads — measured here at 809 against 811 — because there are no blocks
    // to share. The win fix 121 exists for is a PAGED-store property: the
    // ends arrive sorted, so they hit far fewer blocks than the same ids read
    // one at a time in adjacency order. That number belongs on the benchmark
    // pod against SF1, not here, and claiming it from this corpus would be
    // inventing it.
    //
    // So the assertion is the honest one: the gather must not cost MORE.
    assert!(
        count_of(&on, GETS) <= count_of(&off, GETS) + 8,
        "the gather read materially more than the per-end path: {} against {}",
        count_of(&on, GETS),
        count_of(&off, GETS)
    );
    eprintln!(
        "[fix 121] store gets: {} gathering the hop's ends, {} reading one record per end \
         (in-memory: no block sharing, so parity here is the expected result)",
        count_of(&on, GETS),
        count_of(&off, GETS)
    );
}
