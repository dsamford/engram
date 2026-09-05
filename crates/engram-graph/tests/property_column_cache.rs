#![allow(non_snake_case)]
//! The property-column cache: a columnar walk over a whole label keeps the
//! columns it assembled, and the next walk over that label reads them back
//! instead of re-assembling them by a point read per member.
//!
//! On the production mirror every wide-label count re-read the same values on
//! every execution — the NewsArticle enrichment count paid 300k point reads
//! (836 ms against Neo4j's 191) for 150k values it had read a statement
//! earlier; the UserDataNode classified count 76k (82 ms against 4). The cache
//! is a pure performance choice: the entries handed back ARE the gather's,
//! so every answer here is checked against the uncached path.
//!
//! The contract: a second read is served from the cache and agrees (values
//! and presence, count and projection, a hop's end population); a commit
//! retires every column and the next read rebuilds and reflects the write;
//! a zero budget disables it and a budget too small for a column keeps the
//! column out; a label wider than the aggregate's batch walks whole once to
//! keep its columns and batches against them afterwards.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, BTreeMap::new())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, engram_observe::Trace) {
    engram_observe::with_trace(|| rows(g, src))
}

fn count(t: &engram_observe::Trace, k: &str) -> u64 {
    t.counters().get(k).copied().unwrap_or(0)
}

const SERVED: &str = "graph.property column served";
const KEPT: &str = "graph.property column kept";
const RETIRED: &str = "graph.property column retired by a commit";
const NOT_KEPT: &str = "graph.property column not kept: over budget";
const READ_FROM_CACHE: &str = "interp.columnar column read served from the property-column cache";
const WALKED_WHOLE: &str = "interp.columnar aggregate walked whole to keep its columns";
const VECTORISED: &str = "interp.columnar aggregate counted over cached columns";

/// 1,200 `:Doc` interleaved with 1,200 `:Other` in id space (the mirror's
/// layout — every label's span is the whole partition, so every column read
/// is a gather), with a `kind`, a numeric `n`, a `flag` on the evens and a
/// `note` on every third doc; each doc points at one of 20 `:Tag`s.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut tags = Vec::new();
    for t in 0..20i64 {
        let mut m = BTreeMap::new();
        m.insert("t".to_string(), Value::Int(t));
        tags.push(g.create_node(&["Tag".into()], &m).expect("tag"));
    }
    for i in 0..1200i64 {
        let mut m = BTreeMap::new();
        m.insert(
            "kind".to_string(),
            Value::Str(if i % 7 == 0 { "note" } else { "email" }.to_string()),
        );
        m.insert("n".to_string(), Value::Int(i));
        if i % 2 == 0 {
            m.insert("flag".to_string(), Value::Bool(true));
        }
        if i % 3 == 0 {
            m.insert("note".to_string(), Value::Str(format!("n{i}")));
        }
        if i % 4 == 0 {
            m.insert(
                "tags".to_string(),
                Value::List(vec![
                    Value::Str("x".into()),
                    Value::Str(if i % 8 == 0 { "y" } else { "z" }.into()),
                ]),
            );
        }
        let d = g.create_node(&["Doc".into()], &m).expect("doc");
        g.create_rel(d, "TAGGED", tags[(i % 20) as usize], &BTreeMap::new())
            .expect("rel");
        let mut o = BTreeMap::new();
        o.insert("kind".to_string(), Value::Str("other".to_string()));
        o.insert("n".to_string(), Value::Int(-i));
        g.create_node(&["Other".into()], &o).expect("other");
    }
    g
}

const COUNT: &str = "MATCH (d:Doc) WHERE d.kind = 'email' AND d.flag = true AND d.note IS NULL RETURN count(d) AS n";
// No seekable equality here: an equality on `kind` would seek the range
// index and never walk a column at all (the right plan, and not this test's).
const PROJECT: &str = "MATCH (d:Doc) WHERE d.n % 7 = 0 RETURN d.n AS n, d.note AS note ORDER BY n LIMIT 5";
const STAGE: &str = "MATCH (d:Doc) WHERE d.flag = true WITH d.n AS n ORDER BY n DESC LIMIT 3 RETURN collect(n) AS top";
// A hop with a probe over the label: the columnar aggregate lifts the probe
// and walks the label's columns (a `(d)-[:TAGGED]->(t)` chain would run the
// pipeline's expand instead, which reads no whole-label column).
const PROBE: &str = "MATCH (d:Doc) WHERE d.kind = 'email' AND exists((d)-[:TAGGED]->(:Tag {t: 3})) RETURN count(d) AS c";

#[test]
fn a_second_read_over_the_label_is_served_from_the_cache_and_agrees() {
    for src in [COUNT, PROJECT, STAGE, PROBE] {
        // A fresh graph per shape: the shapes share columns (`flag`, `n`),
        // and a column one shape kept would serve the next shape's FIRST
        // read — which is the point of the cache, and not this test's.
        let g = corpus();
        let (first, t1) = traced(&g, src);
        let (second, t2) = traced(&g, src);
        assert_eq!(first, second, "cached vs assembled disagree on `{src}`");
        assert!(
            count(&t1, KEPT) > 0,
            "`{src}`: the first walk must keep the columns it assembled; counters: {:?}",
            t1.counters()
        );
        // Read back by a walk, or — for a plain count — counted over the
        // columns as vectors without a walk at all.
        assert!(
            count(&t2, SERVED) > 0 && (count(&t2, READ_FROM_CACHE) > 0 || count(&t2, VECTORISED) > 0),
            "`{src}`: the second read must read them back (served {}, read {}, vectorised {})",
            count(&t2, SERVED),
            count(&t2, READ_FROM_CACHE),
            count(&t2, VECTORISED)
        );
        assert_eq!(count(&t2, KEPT), 0, "`{src}`: nothing new to keep on the second walk");
    }
    // Fixture sanity: emails (i % 7 != 0), even (flag), not a multiple of 3 (no note).
    let g = corpus();
    let expect = (0..1200i64).filter(|i| i % 7 != 0 && i % 2 == 0 && i % 3 != 0).count() as i64;
    assert_eq!(rows(&g, COUNT), vec![vec![Value::Int(expect)]]);
}

/// A plain count whose columns are cached is answered COLUMN-AT-A-TIME —
/// no scope bound per member — and agrees with the walk, for a predicate
/// of comparisons, presence tests, boolean connectives, IN and coalesce.
#[test]
fn a_cached_count_is_evaluated_over_the_columns_as_vectors() {
    let g = corpus();
    for src in [
        COUNT,
        "MATCH (d:Doc) WHERE d.kind IN ['email', 'memo'] AND (d.note IS NOT NULL OR d.n > 1000) RETURN count(d) AS n",
        "MATCH (d:Doc) WHERE coalesce(d.flag, false) = true AND NOT d.kind = 'note' RETURN count(*) AS n",
        "MATCH (d:Doc) WHERE d.n > 1100 RETURN count(d) AS n",
        // A needle in a COLUMN of lists — the production country-pair scans'
        // `$a IN coalesce(g.affectedCountries, [])`.
        "MATCH (d:Doc) WHERE 'y' IN coalesce(d.tags, []) OR (d.kind = 'note' AND 'x' IN d.tags) RETURN count(d) AS n",
    ] {
        let expect = rows(&g, src); // the first read: the walk, which keeps the columns
        let (got, t) = traced(&g, src);
        assert_eq!(got, expect, "vectorised vs walk disagree on `{src}`");
        assert!(
            count(&t, VECTORISED) > 0,
            "`{src}` must count over the cached columns; counters: {:?}",
            t.counters()
        );
    }
    // A predicate `eval_column` cannot vectorise (non-constant arithmetic)
    // keeps the walk — and agrees.
    let src = "MATCH (d:Doc) WHERE d.n % 5 = 0 RETURN count(d) AS n";
    let expect = rows(&g, src);
    let (got, t) = traced(&g, src);
    assert_eq!(got, expect);
    assert!(count(&t, SERVED) > 0, "served from the cache…");
    assert_eq!(count(&t, "interp.columnar aggregate counted over cached columns"), 0, "…but walked");
}

/// A commit retires every column: the next read rebuilds and reflects the
/// write — never the value it read before it.
#[test]
fn a_commit_retires_the_columns_and_the_next_read_reflects_the_write() {
    let g = corpus();
    let before = rows(&g, COUNT);
    let (_, t) = traced(&g, COUNT);
    assert!(count(&t, SERVED) > 0);
    // Flip one counted email's flag off: the count must drop by exactly one.
    rows(&g, "MATCH (d:Doc {n: 2}) SET d.flag = false RETURN d.n");
    let (after, t) = traced(&g, COUNT);
    assert!(count(&t, RETIRED) > 0, "the stale column must be retired, not served");
    assert_eq!(count(&t, SERVED), 0);
    assert!(count(&t, KEPT) > 0, "the rebuilt column is kept again");
    let Value::Int(b) = before[0][0] else { panic!() };
    assert_eq!(after, vec![vec![Value::Int(b - 1)]]);
    // And the rebuilt column serves the read after that.
    let (again, t) = traced(&g, COUNT);
    assert_eq!(again, after);
    assert!(count(&t, SERVED) > 0);
}

/// A zero budget disables the cache outright; a budget below the column's
/// size keeps that column out — and every answer is the same.
#[test]
fn the_budget_bounds_what_is_kept_and_never_the_answer() {
    let g = corpus();
    let expect = rows(&g, COUNT);
    g.set_prop_column_budget(0);
    let (r, t) = traced(&g, COUNT);
    assert_eq!(r, expect);
    assert_eq!(count(&t, KEPT), 0);
    assert_eq!(count(&t, SERVED), 0);
    // A budget of one kilobyte: the kind column (1,200 strings) is over it.
    g.set_prop_column_budget(1024);
    let (r, t) = traced(&g, COUNT);
    assert_eq!(r, expect);
    assert!(count(&t, NOT_KEPT) > 0);
    assert_eq!(count(&t, SERVED), 0);
    let (r, t) = traced(&g, COUNT);
    assert_eq!(r, expect);
    assert_eq!(count(&t, SERVED), 0, "nothing was kept, so nothing is served");
    // Back to the default: kept and served again.
    g.set_prop_column_budget(engram_graph::PROP_COLUMN_BUDGET_BYTES);
    let (_, t) = traced(&g, COUNT);
    assert!(count(&t, KEPT) > 0);
    let (r, t) = traced(&g, COUNT);
    assert_eq!(r, expect);
    assert!(count(&t, SERVED) > 0);
}

/// A label wider than the aggregate's batch walks WHOLE once so its columns
/// are kept, then batches against the cache — and the batched read agrees.
#[test]
fn a_batched_aggregate_walks_whole_once_then_batches_against_the_cache() {
    let g = corpus();
    g.set_columnar_agg_batch_size(256); // 1,200 members → five batches
    let (first, t1) = traced(&g, COUNT);
    assert!(count(&t1, WALKED_WHOLE) > 0, "the first read walks whole to keep");
    assert!(count(&t1, KEPT) > 0);
    assert_eq!(count(&t1, "interp.columnar aggregate batched"), 0);
    let (second, t2) = traced(&g, COUNT);
    assert_eq!(first, second);
    // A plain count is now vectorised over the cache; a keyed aggregate
    // over the same wide label batches against it.
    assert!(count(&t2, SERVED) > 0, "the second read reads the cache");
    assert!(count(&t2, VECTORISED) > 0, "…and counts over it as vectors");
    assert_eq!(count(&t2, WALKED_WHOLE), 0);
    let keyed = "MATCH (d:Doc) WHERE d.flag = true RETURN d.kind AS k, count(d) AS n ORDER BY k";
    let (k1, _) = traced(&g, keyed);
    let (k2, t3) = traced(&g, keyed);
    assert_eq!(k1, k2);
    assert!(
        count(&t3, "interp.columnar aggregate batched") > 0,
        "the keyed aggregate's second read batches"
    );
    assert!(count(&t3, SERVED) > 0, "…against the cache");
}

/// The memory report attributes what the cache holds.
#[test]
fn the_memory_report_counts_the_columns() {
    let g = corpus();
    assert_eq!(g.memory_report().prop_columns, 0);
    rows(&g, COUNT);
    let r = g.memory_report();
    assert!(r.prop_columns >= 3, "kind, flag and note: {}", r.prop_columns);
    assert!(r.prop_column_bytes > 0);
    g.set_prop_column_budget(0);
    assert_eq!(g.memory_report().prop_columns, 0, "a zero budget empties the cache");
}
