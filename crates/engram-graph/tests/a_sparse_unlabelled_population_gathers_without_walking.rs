#![allow(non_snake_case)]
//! Fix 106: an unlabelled population SPARSE in its id span gathers its
//! columns without walking the span first, and one pass of point reads
//! serves every property read.
//!
//! The production MENTIONS top-30 (`MATCH (n:UserDataNode {userId: $u})-
//! [:MENTIONS]->(e) RETURN e.name, coalesce(e.type, 'unknown'), count(*) …
//! ORDER BY cnt DESC LIMIT 30`) names no label for its 37k ends, which are
//! spread over the whole id space. Fix 93's covering-label discovery
//! declined (an end outside every candidate label), and each of the two
//! properties then walked its budget of rows — cloning every row visited
//! into the override map — before declining to the gather that answers
//! exactly, and the 37k records were gathered once per property.
//!
//! The rows are pinned against a hand-computed expectation; a DENSE
//! population still walks its span (no gather), and a population one label
//! covers still reads through that label (fix 93).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const SKIPPED: &str = "interp.columnar column read skipped the span walk for a sparse population";
const SHARED: &str = "graph.column record-gather";
const GATHER: &str = "graph.column point-gather";
const DECLINED: &str = "store.column scan declined on rows visited";
const DISCOVERED: &str = "interp.pipeline unlabelled var's label discovered from its members";

const STMT: &str = "MATCH (n:UserDataNode {userId: $u})-[:MENTIONS]->(e) \
    RETURN e.name AS name, coalesce(e.type, 'unknown') AS type, count(*) AS cnt \
    ORDER BY cnt DESC, name ASC LIMIT toInteger(30)";

const ENDS: i64 = 600;
const SEEDS: i64 = 300;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("u".to_string(), Value::Str("user-1".into()));
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

fn entity_type(e: i64) -> Option<&'static str> {
    (e % 7 != 0).then(|| ["person", "organization", "place", "topic"][(e % 4) as usize])
}

/// The ends each seed mentions: two spread over every end, and a fifth of
/// the seeds' first edge on the first twenty — a top-30 that means
/// something. `None` is the outlier (a `Person`, not an `Entity`).
fn targets(i: i64, outlier: bool) -> Vec<Option<i64>> {
    let mut t = Vec::new();
    for j in 0..2 {
        t.push(Some(if j == 0 && i % 5 == 0 {
            (i * 31) % 20
        } else {
            (i * 7 + j * 13) % ENDS
        }));
    }
    if outlier && i % 97 == 0 {
        t.push(None);
    }
    t
}

/// Six hundred `Entity` ends, each followed by `fillers` id-adjacent
/// `Filler` nodes; three hundred seeds of one user; optionally one `Person`
/// end no candidate label covers. The column budget factor is 2 so the
/// sparse and dense cases sit on either side of the eight-budget rule.
fn corpus(fillers: i64, outlier: bool) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    g.set_columnar_column_budget_factor(2);
    for src in [
        "CREATE INDEX ud_user FOR (n:UserDataNode) ON (n.userId)",
        "CREATE INDEX ud_user_type FOR (n:UserDataNode) ON (n.userId, n.nodeType)",
    ] {
        let ddl = parse_any(src).expect("parse ddl");
        run_stmt(&g, &ddl, BTreeMap::new()).expect("index");
    }
    let mut ends = Vec::new();
    for e in 0..ENDS {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), Value::Str(format!("Entity {e}")));
        if let Some(t) = entity_type(e) {
            m.insert("type".to_string(), Value::Str(t.into()));
        }
        m.insert("entityId".to_string(), Value::Str(format!("ent-{e:04}")));
        ends.push(g.create_node(&["Entity".into()], &m).expect("entity"));
        for f in 0..fillers {
            let mut m = BTreeMap::new();
            m.insert("k".to_string(), Value::Int(e * fillers + f));
            g.create_node(&["Filler".into()], &m).expect("filler");
        }
    }
    let person = outlier.then(|| {
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), Value::Str("Person 0".into()));
        m.insert("type".to_string(), Value::Str("person".into()));
        g.create_node(&["Person".into()], &m).expect("person")
    });
    for i in 0..SEEDS {
        let mut m = BTreeMap::new();
        m.insert("userId".to_string(), Value::Str("user-1".into()));
        m.insert("nodeType".to_string(), Value::Str("email".into()));
        m.insert("nodeId".to_string(), Value::Str(format!("node-{i:04}")));
        let n = g.create_node(&["UserDataNode".into()], &m).expect("seed");
        for t in targets(i, outlier) {
            let end = match t {
                Some(e) => ends[e as usize],
                None => person.expect("outlier"),
            };
            g.create_rel(n, "MENTIONS", end, &BTreeMap::new())
                .expect("mentions");
        }
    }
    let mut other = BTreeMap::new();
    other.insert("userId".to_string(), Value::Str("user-2".into()));
    other.insert("nodeType".to_string(), Value::Str("email".into()));
    let o = g
        .create_node(&["UserDataNode".into()], &other)
        .expect("other");
    g.create_rel(o, "MENTIONS", ends[0], &BTreeMap::new())
        .expect("other mentions");
    g
}

fn expected(outlier: bool) -> Vec<Vec<Value>> {
    let mut counts: BTreeMap<(String, String), i64> = BTreeMap::new();
    for i in 0..SEEDS {
        for t in targets(i, outlier) {
            let key = match t {
                Some(e) => (
                    format!("Entity {e}"),
                    entity_type(e).unwrap_or("unknown").to_string(),
                ),
                None => ("Person 0".to_string(), "person".to_string()),
            };
            *counts.entry(key).or_insert(0) += 1;
        }
    }
    let mut all: Vec<((String, String), i64)> = counts.into_iter().collect();
    all.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.0.cmp(&b.0.0)));
    all.into_iter()
        .take(30)
        .map(|((name, ty), cnt)| vec![Value::Str(name), Value::Str(ty), Value::Int(cnt)])
        .collect()
}

/// The production shape over a sparse, uncovered population: no span walk,
/// no scan decline, ONE gather serving both properties; the rows are the
/// hand-computed top 30.
#[test]
fn a_a_sparse_uncovered_population_gathers_both_properties_in_one_pass() {
    let g = corpus(20, true);
    let (got, c) = traced(&g, STMT);
    assert_eq!(got, expected(true));
    assert_eq!(count_of(&c, DISCOVERED), 0, "{c:?}");
    assert_eq!(count_of(&c, SKIPPED), 1, "{c:?}");
    assert_eq!(count_of(&c, GATHER), 1, "{c:?}");
    assert_eq!(count_of(&c, SHARED), 1, "{c:?}");
    assert_eq!(count_of(&c, DECLINED), 0, "{c:?}");
    // Warm: the same answer, the same work.
    let (got, c) = traced(&g, STMT);
    assert_eq!(got, expected(true));
    assert_eq!(count_of(&c, SKIPPED), 1, "{c:?}");
    assert_eq!(count_of(&c, GATHER), 1, "{c:?}");
}

/// The columnar hop aggregate (batch.rs's column read) reads its unlabelled
/// end the same way: the email-seeded spelling over the sparse uncovered
/// population skips the walk for each property and gathers once. (Here the
/// small seed label is read whole; on the mirror the seeds are sought.)
#[test]
fn c_the_hop_aggregate_skips_the_walk_for_its_unlabelled_end() {
    let g = corpus(20, true);
    let stmt = STMT.replace("{userId: $u}", "{userId: $u, nodeType: 'email'}");
    let (got, c) = traced(&g, &stmt);
    assert_eq!(got, expected(true));
    assert_eq!(
        count_of(&c, "interp.columnar hop aggregate scans"),
        1,
        "{c:?}"
    );
    assert_eq!(count_of(&c, SKIPPED), 2, "{c:?}");
    assert_eq!(count_of(&c, DECLINED), 0, "{c:?}");
    assert_eq!(count_of(&c, GATHER), 1, "{c:?}");
    assert_eq!(count_of(&c, SHARED), 1, "{c:?}");
}

/// CONTROLS: a dense population walks its span and gathers nothing; a
/// population one label covers reads through that label (fix 93) and
/// gathers nothing.
#[test]
fn b_a_dense_population_walks_and_a_covered_one_reads_its_label() {
    let g = corpus(0, true);
    let (got, c) = traced(&g, STMT);
    assert_eq!(got, expected(true));
    assert_eq!(count_of(&c, SKIPPED), 0, "{c:?}");
    assert_eq!(count_of(&c, GATHER), 0, "{c:?}");
    assert_eq!(count_of(&c, SHARED), 0, "{c:?}");
    // A covered population reads through its discovered label: cold, the
    // label's walk (itself declined for the sparse span) gathers ONCE and
    // keeps the label's columns; warm, the kept columns serve it.
    let g = corpus(20, false);
    let (got, c) = traced(&g, STMT);
    assert_eq!(got, expected(false));
    assert_eq!(count_of(&c, DISCOVERED), 1, "{c:?}");
    assert_eq!(count_of(&c, SKIPPED), 0, "{c:?}");
    assert_eq!(count_of(&c, "graph.property column kept"), 3, "{c:?}");
    let (got, c) = traced(&g, STMT);
    assert_eq!(got, expected(false));
    assert_eq!(count_of(&c, DISCOVERED), 1, "{c:?}");
    assert_eq!(count_of(&c, SKIPPED), 0, "{c:?}");
    assert_eq!(count_of(&c, GATHER), 0, "{c:?}");
}
