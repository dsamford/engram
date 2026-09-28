//! A label-scan seed that NOTHING reads is bound bare — its id and the
//! pattern's labels — with no record read.
//!
//! `lean_starts_from_columns` bound a seed population from the label's columns
//! when properties were demanded, and declined when NONE were: the caller then
//! read every seed's record back (a projected get) only to learn labels the
//! label scan had already proven. SNB BI bi15's edge-centric weights seed
//! `(m1:Comment)` and use it only for its identity: 6.4M record reads at SF3,
//! ~22M at SF10, where the records no longer fit the page cache.
//!
//! DIFFERENTIAL against the columnar paths off (every seed read from its
//! record): identical answers, the bare counter asserted, and far fewer reads.
//! A seed whose properties or inline map are read is still read; a label
//! beyond its pattern that a later clause TESTS is carried from that label's
//! membership, and only `labels(a)` — the whole set — reads the record.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn run(g: &Graph, q: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    let (rows, t) = engram_observe::with_trace(|| {
        run_query(g, &s, BTreeMap::new())
            .unwrap_or_else(|e| panic!("run `{q}`: {e}"))
            .rows
    });
    (rows, t.counters().iter().map(|(k, v)| (k.clone(), *v)).collect())
}

/// The same statement with every seed read from its record.
fn control(g: &Graph, q: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let (r, _) = run(g, q);
    g.set_columnar_scans(true);
    r
}

fn get(c: &BTreeMap<String, u64>, k: &str) -> u64 {
    c.get(k).copied().unwrap_or(0)
}

const BARE: &str = "interp.seed starts bound bare: nothing reads them";
const GETS: &str = "store.gets";

/// 300 items with a fat record, each NEXT to the one after; every tenth also
/// an `:Odd`; and a `:Tag` on every third, reached from its item.
fn items() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(
        &g,
        "UNWIND range(0, 299) AS i CREATE (:Item {id: i, blob: 'a record with a body worth decoding'})",
    );
    ddl(&g, "UNWIND range(0, 298) AS i MATCH (a:Item {id: i}), (b:Item {id: i + 1}) CREATE (a)-[:NEXT]->(b)");
    ddl(&g, "MATCH (a:Item) WHERE a.id % 10 = 0 SET a:Odd");
    ddl(&g, "UNWIND range(0, 99) AS k MATCH (a:Item {id: 3 * k}) CREATE (a)-[:TAGGED]->(:Tag {k: k})");
    let _ = g.warm();
    g
}

/// The seed is used for its identity alone: a hop from it, an OPTIONAL hop, and
/// an aggregate over what they reach.
const IDENTITY_ONLY: &str = "MATCH (a:Item) MATCH (a)-[:NEXT]->(b:Item) \
     OPTIONAL MATCH (a)-[:TAGGED]->(t:Tag) \
     WITH b.id AS next, t.k AS k RETURN count(*) AS n, sum(next) AS s, count(k) AS tagged";

#[test]
fn a_seed_nothing_reads_is_bound_bare_and_answers_the_same() {
    let g = items();
    let want = control(&g, IDENTITY_ONLY);
    assert_eq!(
        want,
        vec![vec![Value::Int(299), Value::Int((1..=299).sum()), Value::Int(100)]],
        "the control's own answer"
    );
    let _ = run(&g, IDENTITY_ONLY); // warm
    let (got, c) = run(&g, IDENTITY_ONLY);
    assert_eq!(got, want, "binding the seed bare changed the answer");
    assert!(get(&c, BARE) > 0, "the seed was never bound bare: {c:?}");
    assert!(
        get(&c, GETS) < 300,
        "{} store reads: a record read per seed is back: {c:?}",
        get(&c, GETS)
    );
}

/// Read, so NOT bare: a property of the seed, an inline map on it, and the
/// whole label set — and each still answers as the control does.
#[test]
fn a_seed_something_reads_is_still_read() {
    let g = items();
    for q in [
        "MATCH (a:Item) MATCH (a)-[:NEXT]->(b:Item) OPTIONAL MATCH (a)-[:TAGGED]->(t:Tag) \
         WITH a.id AS i, t.k AS k RETURN count(*) AS n, sum(i) AS s, count(k) AS tagged",
        "MATCH (a:Item {blob: 'a record with a body worth decoding'}) MATCH (a)-[:NEXT]->(b:Item) \
         OPTIONAL MATCH (a)-[:TAGGED]->(t:Tag) WITH t.k AS k RETURN count(*) AS n, count(k) AS tagged",
        "MATCH (a:Item) MATCH (a)-[:NEXT]->(b:Item) OPTIONAL MATCH (a)-[:TAGGED]->(t:Tag) \
         WITH size(labels(a)) AS nl, t.k AS k RETURN count(*) AS n, sum(nl) AS labels",
    ] {
        let want = control(&g, q);
        let (got, c) = run(&g, q);
        assert_eq!(got, want, "`{q}`");
        assert_eq!(get(&c, BARE), 0, "`{q}` reads its seed and was bound bare: {c:?}");
    }
}

const MEMBERSHIP: &str = "interp.seed starts carry the labels a later clause tests, from membership";

/// A label beyond the pattern that a later clause TESTS rides on the seed from
/// that label's MEMBERSHIP — bare when nothing else of the seed is read, from
/// the label's columns when a property is. Both lean binds carried the
/// pattern's labels only, so `a:Odd` read false for all 30 odd items; reading
/// every record back instead cost a record read per seed.
#[test]
fn a_seed_tested_for_another_label_carries_it_from_membership() {
    let g = items();
    let bare = "MATCH (a:Item) MATCH (a)-[:NEXT]->(b:Item) OPTIONAL MATCH (a)-[:TAGGED]->(t:Tag) \
         WITH a:Odd AS odd, t.k AS k \
         RETURN count(*) AS n, sum(CASE WHEN odd THEN 1 ELSE 0 END) AS odd";
    let cols = "MATCH (a:Item) MATCH (a)-[:NEXT]->(b:Item) OPTIONAL MATCH (a)-[:TAGGED]->(t:Tag) \
         WITH a.id AS i, a:Odd AS odd, t.k AS k \
         RETURN count(*) AS n, sum(i) AS s, sum(CASE WHEN odd THEN 1 ELSE 0 END) AS odd";
    // Two labels beyond the pattern, one of which no node has, and the
    // pattern's own label tested again.
    let several = "MATCH (a:Item) MATCH (a)-[:NEXT]->(b:Item) OPTIONAL MATCH (a)-[:TAGGED]->(t:Tag) \
         WITH a:Odd AS odd, a:Nope AS nope, a:Item AS item, a:Odd AND a:Item AS both, t.k AS k \
         RETURN sum(CASE WHEN odd THEN 1 ELSE 0 END) AS odd, sum(CASE WHEN nope THEN 1 ELSE 0 END) AS nope, \
                sum(CASE WHEN item THEN 1 ELSE 0 END) AS item, sum(CASE WHEN both THEN 1 ELSE 0 END) AS both";
    for (q, expect, is_bare) in [
        (bare, vec![Value::Int(299), Value::Int(30)], true),
        (cols, vec![Value::Int(299), Value::Int((0..=298).sum()), Value::Int(30)], false),
        (several, vec![Value::Int(30), Value::Int(0), Value::Int(299), Value::Int(30)], true),
    ] {
        let want = control(&g, q);
        assert_eq!(want, vec![expect], "the control's own answer to `{q}`");
        let _ = run(&g, q); // warm
        let (got, c) = run(&g, q);
        assert_eq!(got, want, "a lean seed answered a label test wrongly: `{q}`");
        assert!(get(&c, MEMBERSHIP) > 0, "`{q}` never read its tested label from membership: {c:?}");
        assert_eq!(get(&c, BARE) > 0, is_bare, "`{q}` bare-bound counter: {c:?}");
        assert!(
            get(&c, GETS) < 300,
            "{} store reads: `{q}` read a record per seed to test a label: {c:?}",
            get(&c, GETS)
        );
    }
}

/// A PATTERN tests a bound node too: `node_satisfies` checks the bound
/// value's own labels and properties against a pattern that re-uses it — a
/// pattern predicate, an EXISTS body, a comprehension, a second path of the
/// same MATCH, a later MATCH, in this stage or the next. None of those is an
/// `a:L` expression, so each must reach the demand some other way, or a lean
/// seed decides it from labels and properties it never carried. Every shape
/// is run against the control and ALL disagreements are reported together.
#[test]
fn a_pattern_that_re_uses_the_seed_answers_as_the_record_does() {
    let g = items();
    let blob = "'a record with a body worth decoding'";
    let shapes: Vec<(String, i64)> = vec![
        // labels, same stage
        ("MATCH (a:Item) MATCH (a)-[:NEXT]->(b:Item) WHERE (a:Odd)-[:NEXT]->() RETURN count(*) AS n".into(), 30),
        ("MATCH (a:Item) MATCH (a:Odd)-[:NEXT]->(b:Item) RETURN count(*) AS n".into(), 30),
        ("MATCH (a:Item)-[:NEXT]->(b:Item) MATCH (a:Odd)-[:NEXT]->(c:Item) RETURN count(*) AS n".into(), 30),
        ("MATCH (a:Item) WHERE a.id < 100 MATCH (a:Odd)-[:NEXT]->(b:Item) RETURN count(*) AS n".into(), 10),
        ("MATCH (a:Item) WHERE EXISTS { MATCH (a:Odd)-[:NEXT]->() } RETURN count(*) AS n".into(), 30),
        ("MATCH (a:Item) WITH size([(a:Odd)-[:NEXT]->(x) | x]) AS k RETURN sum(k) AS n".into(), 30),
        ("MATCH (a:Item) RETURN sum(count { (a:Odd)-[:NEXT]->() }) AS n".into(), 30),
        // ...and with a property read too, so the seed is bound from COLUMNS
        ("MATCH (a:Item) MATCH (a)-[:NEXT]->(b:Item) WHERE (a:Odd)-[:NEXT]->() RETURN sum(a.id) AS n".into(), 4350),
        ("MATCH (a:Item) WHERE EXISTS { MATCH (a:Odd)-[:NEXT]->() } RETURN sum(a.id) AS n".into(), 4350),
        // labels, the next stage
        ("MATCH (a:Item) WITH a MATCH (a:Odd)-[:NEXT]->(b:Item) RETURN count(*) AS n".into(), 30),
        ("MATCH (a:Item) WITH a, a.id AS i WHERE i >= 0 MATCH (a:Odd)-[:NEXT]->(b:Item) RETURN count(*) AS n".into(), 30),
        ("MATCH (a:Item) WITH a, a.id AS i WITH a:Odd AS odd RETURN sum(CASE WHEN odd THEN 1 ELSE 0 END) AS n".into(), 30),
        ("MATCH (a:Item) WITH a ORDER BY a.id LIMIT 100 WHERE (a:Odd)-[:NEXT]->() RETURN count(*) AS n".into(), 10),
        // an inline map on the re-used node
        (format!("MATCH (a:Item) MATCH (a {{blob: {blob}}})-[:NEXT]->(b:Item) RETURN count(*) AS n"), 299),
        (format!("MATCH (a:Item) MATCH (a)-[:NEXT]->(b:Item) WHERE (a {{blob: {blob}}})-[:NEXT]->() RETURN count(*) AS n"), 299),
        (format!("MATCH (a:Item) WHERE EXISTS {{ MATCH (a {{blob: {blob}}})-[:NEXT]->() }} RETURN count(*) AS n"), 299),
        (format!("MATCH (a:Item) WITH a MATCH (a {{blob: {blob}}})-[:NEXT]->(b:Item) RETURN count(*) AS n"), 299),
        (format!("MATCH (a:Item)-[:NEXT]->(b:Item) MATCH (a {{blob: {blob}}})-[:NEXT]->(c:Item) RETURN count(*) AS n"), 299),
    ];
    let mut wrong = Vec::new();
    for (q, n) in &shapes {
        let want = control(&g, q);
        if want != vec![vec![Value::Int(*n)]] {
            wrong.push(format!("CONTROL `{q}`: {want:?}, expected {n}"));
            continue;
        }
        let (got, _) = run(&g, q);
        if got != want {
            wrong.push(format!("`{q}`: {got:?}, the record says {n}"));
        }
    }
    assert!(wrong.is_empty(), "{} of {} shapes disagree:\n{}", wrong.len(), shapes.len(), wrong.join("\n"));
}

/// `id(v)` and `a = b` / `a <> b` read a node's IDENTITY, never its record:
/// entities compare by id. Each used to demand the node in full — bi15's
/// `id(c1) < id(c2)`, `c1 <> c2` and `post = m2` read records per row.
#[test]
fn id_and_identity_comparisons_read_no_record() {
    let g = items();
    let q = "MATCH (a:Item) MATCH (a)-[:NEXT]->(b:Item) OPTIONAL MATCH (a)-[:TAGGED]->(t:Tag) \
         WITH id(a) AS i, id(b) AS j, a <> b AS differ, a = a AS same, t IS NULL AS untagged \
         RETURN count(*) AS n, sum(j - i) AS gap, \
                sum(CASE WHEN differ THEN 1 ELSE 0 END) AS d, \
                sum(CASE WHEN same THEN 1 ELSE 0 END) AS s, \
                sum(CASE WHEN untagged THEN 1 ELSE 0 END) AS u";
    let want = control(&g, q);
    let row = &want[0];
    assert_eq!(
        (&row[0], &row[2], &row[3], &row[4]),
        (&Value::Int(299), &Value::Int(299), &Value::Int(299), &Value::Int(199)),
        "the control's own answer: {want:?}"
    );
    let _ = run(&g, q); // warm
    let (got, c) = run(&g, q);
    assert_eq!(got, want, "reading identity alone changed the answer");
    assert!(
        get(&c, GETS) < 300,
        "{} store reads for a statement that reads no property: {c:?}",
        get(&c, GETS)
    );
}
