#![allow(non_snake_case)]
//! A seek's ids DRIVE what follows them (fix 23, v99):
//!
//! 1. a count whose whole predicate is prefixes / equalities on declared
//!    keys is the size of the index ranges' intersection — no walk, no
//!    column (`covered_count` learned prefixes);
//! 2. a walk over a seek's ids takes ONLY those ids' entries from a cached
//!    column, not the column's whole id-range slice;
//! 3. the general path's column-filtered seed seeks the predicate's
//!    equalities and prefixes on declared keys and walks over the sought
//!    ids instead of the label.
//!
//! The production shape (2026-09-04, v97 measured on the pod): `MATCH
//! (g:GeopoliticalEvent) WHERE g.eventId STARTS WITH 'edgar-8k-' RETURN
//! count(g)` sought 3.9k of 44k and then cloned 44k strings to bind 3.9k
//! (7.3 ms vs Neo4j 1.4); the multi-clause original evaluated three conjuncts
//! (two datetime parses) over all 44k on the general path (32 ms vs 10.9).
//!
//! Every answer is checked against the seek-less path's; every counter is
//! asserted on a declared key and asserted ABSENT on an undeclared one.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, src: &str) {
    run_stmt(g, &parse_any(src).expect("parse"), BTreeMap::new()).expect("ddl");
}

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("pre".to_string(), Value::Str("edgar-8k-".to_string()));
    p.insert("narrow".to_string(), Value::Str("edgar-8k-00".to_string()));
    p
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// The answer with the seek on, and with every property seek off.
fn both(g: &Graph, src: &str) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_property_seek(true);
    let on = rows(g, src);
    g.set_property_seek(false);
    let off = rows(g, src);
    g.set_property_seek(true);
    (on, off)
}

fn traced(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    g.set_property_seek(true);
    let (r, trace) = engram_observe::with_trace(|| rows(g, src));
    (r, trace.counters().clone())
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const COVERED_PREFIX: &str = "interp.columnar covered count sought a prefix";
const COVERED: &str = "interp.columnar covered count";
const RESTRICTED: &str = "interp.columnar cached column restricted to the population";
const SEEK_WALKED: &str = "interp.columnar aggregate walked its probes over a seek";
const SERVED: &str = "interp.columnar column read served from the property-column cache";
const FILTER_SOUGHT: &str = "interp.seed column filter walked over a seek";
const FILTERED: &str = "interp.seeds filtered by columns";

/// `n` `:Ev` above the seek floor: every `every`-th carries
/// `eventId = edgar-8k-<i>`, the rest `other-<i>`; `n` (the int) on all;
/// `startAt` on all but every seventh.
fn corpus(declare: bool, n: i64, every: i64) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_label_scoped_indexes(true);
    if declare {
        ddl(&g, "CREATE INDEX ev_id IF NOT EXISTS FOR (n:Ev) ON (n.eventId)");
    }
    for i in 0..n {
        let mut m = BTreeMap::new();
        m.insert(
            "eventId".to_string(),
            Value::Str(if i % every == 0 {
                format!("edgar-8k-{i:06}")
            } else {
                format!("other-{i:06}")
            }),
        );
        m.insert("n".to_string(), Value::Int(i));
        if i % 7 != 0 {
            m.insert("startAt".to_string(), Value::Str(format!("2026-08-{:02}", 1 + i % 28)));
        }
        g.create_node(&["Ev".into()], &m).expect("ev");
    }
    g
}

const PREFIX_COUNT: &str = "MATCH (e:Ev) WHERE e.eventId STARTS WITH $pre RETURN count(e) AS n";
const TWO_PREFIXES: &str =
    "MATCH (e:Ev) WHERE e.eventId STARTS WITH $pre AND e.eventId STARTS WITH $narrow RETURN count(e) AS n";
const PREFIX_AND_EQ: &str =
    "MATCH (e:Ev) WHERE e.eventId STARTS WITH $pre AND e.eventId = 'edgar-8k-000004' RETURN count(e) AS n";
const PREFIX_AND_RESIDUAL: &str =
    "MATCH (e:Ev) WHERE e.eventId STARTS WITH $pre AND e.n % 5 = 0 RETURN count(e) AS n";
/// A statement the columnar recognisers do not claim (two MATCH clauses,
/// the second correlated), so the first MATCH is a general-path clause
/// scan — the production shape's path. (An UNWIND after the MATCH is
/// claimed by the columnar stage, which seeks on its own.)
const GENERAL: &str =
    "MATCH (e:Ev) WHERE e.eventId STARTS WITH $pre AND e.n % 5 = 0 MATCH (e2:Ev) WHERE e2.n = e.n RETURN count(e2) AS n";
/// Keeps the `eventId` and `n` columns in the property-column cache: a
/// whole-label walk whose predicate is not vectorisable (the `%`).
const WARM: &str = "MATCH (e:Ev) WHERE e.n % 7 = 0 AND e.eventId <> 'x' RETURN count(e) AS n";

#[test]
fn a_prefix_only_count_is_answered_from_the_index_range() {
    let g = corpus(true, 4000, 4);
    for src in [PREFIX_COUNT, TWO_PREFIXES, PREFIX_AND_EQ] {
        let (on, off) = both(&g, src);
        assert_eq!(on, off, "covered vs walk disagree on `{src}`");
        let (_, c) = traced(&g, src);
        assert!(count_of(&c, COVERED_PREFIX) > 0, "`{src}` must seek its prefix: {c:?}");
        assert!(count_of(&c, COVERED) > 0, "`{src}` must be a covered count: {c:?}");
        assert_eq!(count_of(&c, SEEK_WALKED), 0, "`{src}` must not walk: {c:?}");
    }
    assert_eq!(rows(&g, PREFIX_COUNT), vec![vec![Value::Int(1000)]]);
    // edgar-8k-00xxxx: i < 10000 always, so every prefixed id; the narrow
    // prefix `edgar-8k-00` covers i in 0..4000 — all 1,000 of them.
    assert_eq!(rows(&g, TWO_PREFIXES), vec![vec![Value::Int(1000)]]);
    assert_eq!(rows(&g, PREFIX_AND_EQ), vec![vec![Value::Int(1)]]);
    // A residual the index cannot answer is NOT covered: the walk (or the
    // per-id seek) answers, and agrees.
    let (on, off) = both(&g, PREFIX_AND_RESIDUAL);
    assert_eq!(on, off);
    assert_eq!(on, vec![vec![Value::Int(200)]]);
    let (_, c) = traced(&g, PREFIX_AND_RESIDUAL);
    assert_eq!(count_of(&c, COVERED), 0, "a residual is never covered: {c:?}");
}

/// CONTROL: an undeclared key is never prefix-covered — nothing is built
/// the operator never asked for — and the answers still agree.
#[test]
fn an_undeclared_key_is_not_covered() {
    let g = corpus(false, 4000, 4);
    let (on, off) = both(&g, PREFIX_COUNT);
    assert_eq!(on, off);
    let (_, c) = traced(&g, PREFIX_COUNT);
    assert_eq!(count_of(&c, COVERED_PREFIX), 0, "{c:?}");
    assert_eq!(count_of(&c, COVERED), 0, "{c:?}");
}

/// A walk over a seek's ids takes ONLY those ids' entries from the cached
/// column. The population is past the per-id cap (2,400 of 6,000) so the
/// aggregate walks over the seek; the columns were kept by a whole-label
/// walk just before; the restriction counter fires per column served.
#[test]
fn a_walk_over_a_seek_takes_only_its_ids_from_the_cached_column() {
    let g = corpus(true, 6000, 2); // 3,000 prefixed: exactly half — not a reduction
    let (on, off) = both(&g, PREFIX_AND_RESIDUAL);
    assert_eq!(on, off);
    // 40% prefixed: 2,400 of 6,000 — past the per-id cap, inside the walk's.
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_label_scoped_indexes(true);
    ddl(&g, "CREATE INDEX ev_id IF NOT EXISTS FOR (n:Ev) ON (n.eventId)");
    for i in 0..6000i64 {
        let mut m = BTreeMap::new();
        m.insert(
            "eventId".to_string(),
            Value::Str(if i % 5 < 2 { format!("edgar-8k-{i:06}") } else { format!("other-{i:06}") }),
        );
        m.insert("n".to_string(), Value::Int(i));
        g.create_node(&["Ev".into()], &m).expect("ev");
    }
    // Keep both columns.
    let (_, warm) = traced(&g, WARM);
    assert_eq!(count_of(&warm, RESTRICTED), 0, "a whole-label walk restricts nothing: {warm:?}");
    // Now the seek-walk: both columns from the cache, restricted to the
    // 2,400 sought ids.
    let (on, c) = traced(&g, PREFIX_AND_RESIDUAL);
    assert!(count_of(&c, SEEK_WALKED) > 0, "2,400 of 6,000 walks over the seek: {c:?}");
    assert_eq!(count_of(&c, SERVED), 2, "eventId and n served from the cache: {c:?}");
    assert_eq!(count_of(&c, RESTRICTED), 2, "both restricted to the population: {c:?}");
    g.set_property_seek(false);
    let off = rows(&g, PREFIX_AND_RESIDUAL);
    g.set_property_seek(true);
    assert_eq!(on, off);
    assert_eq!(
        on,
        vec![vec![Value::Int((0..6000i64).filter(|i| i % 5 < 2 && i % 5 == 0).count() as i64)]]
    );
}

/// The general path's column-filtered seed seeks the prefix and walks over
/// the sought ids; the answer is the seek-less scan's.
#[test]
fn the_general_paths_column_filter_walks_over_a_declared_prefix_seek() {
    let g = corpus(true, 4000, 4);
    let (on, off) = both(&g, GENERAL);
    assert_eq!(on, off, "general path: seek vs scan disagree");
    assert_eq!(on, vec![vec![Value::Int(200)]]); // 200 events, one e2 each
    let (_, c) = traced(&g, GENERAL);
    assert!(count_of(&c, FILTER_SOUGHT) > 0, "the column filter must seek: {c:?}");
    assert!(count_of(&c, FILTERED) > 0, "…and still filter by columns: {c:?}");
    // With the columns kept by the first run, the second run's walk over
    // the seek takes only the sought ids' entries.
    let (_, c2) = traced(&g, GENERAL);
    assert!(
        count_of(&c2, RESTRICTED) > 0 || count_of(&c2, SERVED) == 0,
        "a cached column is restricted to the sought ids: {c2:?}"
    );
    // CONTROL: undeclared — the filter runs over the label as before.
    let g = corpus(false, 4000, 4);
    let (on, off) = both(&g, GENERAL);
    assert_eq!(on, off);
    let (_, c) = traced(&g, GENERAL);
    assert_eq!(count_of(&c, FILTER_SOUGHT), 0, "{c:?}");
    assert!(count_of(&c, FILTERED) > 0, "{c:?}");
}

const PREFERRED: &str = "interp.columnar aggregate walked a selective seek instead of vectorising";
const VECTORISED: &str = "interp.columnar aggregate counted over cached columns";
/// A vectorisable residual behind a prefix — the shape that vectorised over
/// the whole label with no short-circuit (`datetime()` per member, 44k of
/// them for the 3.9k the prefix names).
const PREFIX_AND_VECTORISABLE: &str =
    "MATCH (e:Ev) WHERE e.eventId STARTS WITH $pre AND toString(e.n) STARTS WITH '1' RETURN count(e) AS n";

/// With the columns cached, a seek naming fewer than an eighth of the label
/// is walked BEFORE the column-at-a-time count; a wider seek still lets the
/// vectorised count answer. Both agree with the walk.
#[test]
fn a_selective_seek_is_walked_before_the_vectorised_count() {
    // 400 of 4,000: an eighth is 500 — preferred.
    let g = corpus(true, 4000, 10);
    let (first, _) = traced(&g, WARM); // keeps `n` and `eventId`
    assert!(!first.is_empty());
    let (on, c) = traced(&g, PREFIX_AND_VECTORISABLE);
    assert!(count_of(&c, PREFERRED) > 0, "400 of 4,000 walks over the seek: {c:?}");
    assert_eq!(count_of(&c, VECTORISED), 0, "{c:?}");
    assert!(count_of(&c, RESTRICTED) > 0, "…over the cached columns, restricted: {c:?}");
    g.set_property_seek(false);
    let off = rows(&g, PREFIX_AND_VECTORISABLE);
    g.set_property_seek(true);
    assert_eq!(on, off);
    assert_eq!(
        on,
        vec![vec![Value::Int(
            (0..4000i64).filter(|i| i % 10 == 0 && i.to_string().starts_with('1')).count() as i64
        )]]
    );
    // 1,000 of 4,000: a quarter — the vectorised count keeps the shape.
    let g = corpus(true, 4000, 4);
    let _ = traced(&g, WARM);
    let (on, c) = traced(&g, PREFIX_AND_VECTORISABLE);
    assert_eq!(count_of(&c, PREFERRED), 0, "{c:?}");
    assert!(count_of(&c, VECTORISED) > 0, "a quarter of the label vectorises: {c:?}");
    g.set_property_seek(false);
    let off = rows(&g, PREFIX_AND_VECTORISABLE);
    assert_eq!(on, off);
}
