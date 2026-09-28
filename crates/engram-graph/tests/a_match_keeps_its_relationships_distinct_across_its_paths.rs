//! openCypher scopes relationship isomorphism to the MATCH CLAUSE: no
//! relationship binds twice anywhere in `MATCH p1, p2, …`, while separate
//! clauses (`MATCH p1 MATCH p2`) may reuse one. The matchers here kept it per
//! PATH, which made the comma form answer like separate clauses.
//!
//! SNB BI bi17 relies on the clause rule — its two HAS_MEMBER paths from
//! `forum1` make person2 and person3 different people — and Engram counted each
//! person replying to their own message as well (SF0.1, most-tagged tag: 148
//! where the rule gives 138). `enforce_clause_rel_uniqueness` states the rule
//! as WHERE conjuncts; these pin the answers against the rule itself, in the
//! interpreter and with the columnar recognisers on.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const KEPT: &str = "interp.MATCH relationships kept distinct across its paths";
const DECLINED: &str = "interp.MATCH relationship uniqueness across paths declined: a `*` projection";
const APART: &str = "interp.MATCH relationship pair already kept apart by the WHERE";

/// Forum 1 has members 10 and 11; forum 2 has member 20 alone. A chain
/// 1 -> 2 -> 3 of NEXT edges. Member 10 wrote message 100.
fn corpus(columnar: bool) -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let node = |label: &str, id: i64| {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Int(id));
        g.create_node(&[label.into()], &m).expect("node")
    };
    let rel = |a: u64, t: &str, b: u64| {
        g.create_rel(a, t, b, &BTreeMap::new()).expect("rel");
    };
    let f1 = node("F", 1);
    let f2 = node("F", 2);
    let p10 = node("P", 10);
    let p11 = node("P", 11);
    let p20 = node("P", 20);
    rel(f1, "MEMBER", p10);
    rel(f1, "MEMBER", p11);
    rel(f2, "MEMBER", p20);
    let m100 = node("M", 100);
    rel(p10, "WROTE", m100);
    let c1 = node("C", 1);
    let c2 = node("C", 2);
    let c3 = node("C", 3);
    rel(c1, "NEXT", c2);
    rel(c2, "NEXT", c3);
    g.set_columnar_scans(columnar);
    g
}

fn run(g: &Graph, src: &str) -> (Vec<Vec<Value>>, BTreeMap<String, u64>) {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    let (r, trace) = engram_observe::with_trace(|| {
        run_query(g, &q, BTreeMap::new())
            .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
            .rows
    });
    (r, trace.counters().clone())
}

fn ints(rows: &[Vec<Value>]) -> Vec<Vec<i64>> {
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Int(i) => *i,
                    other => panic!("expected an int, got {other:?}"),
                })
                .collect()
        })
        .collect()
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

#[test]
fn two_comma_paths_never_bind_one_relationship_twice() {
    for columnar in [false, true] {
        let g = corpus(columnar);
        let (rows, c) = run(
            &g,
            "MATCH (f:F)-[:MEMBER]->(a:P), (f)-[:MEMBER]->(b:P) \
             RETURN a.id, b.id ORDER BY a.id, b.id",
        );
        assert_eq!(
            ints(&rows),
            vec![vec![10, 11], vec![11, 10]],
            "one MATCH: a and b are reached over different MEMBER edges (columnar {columnar})"
        );
        assert!(count_of(&c, KEPT) >= 1, "the rewrite fired (columnar {columnar})");
    }
}

#[test]
fn separate_clauses_may_still_reuse_a_relationship() {
    for columnar in [false, true] {
        let g = corpus(columnar);
        let (rows, c) = run(
            &g,
            "MATCH (f:F)-[:MEMBER]->(a:P) MATCH (f)-[:MEMBER]->(b:P) \
             RETURN a.id, b.id ORDER BY a.id, b.id",
        );
        assert_eq!(
            ints(&rows),
            vec![vec![10, 10], vec![10, 11], vec![11, 10], vec![11, 11], vec![20, 20]],
            "two MATCH clauses: the same edge may serve both (columnar {columnar})"
        );
        assert_eq!(count_of(&c, KEPT), 0, "separate clauses are not rewritten");
    }
}

#[test]
fn the_bi17_shape_undirected_from_one_forum_makes_two_people() {
    for columnar in [false, true] {
        let g = corpus(columnar);
        let (rows, _) = run(
            &g,
            "MATCH (f:F)<-[:MEMBER]->(a:P), (f)<-[:MEMBER]->(b:P) \
             RETURN f.id, count(*) ORDER BY f.id",
        );
        // forum 1: (10,11) and (11,10); forum 2 has one member and so no pair
        assert_eq!(ints(&rows), vec![vec![1, 2]], "columnar {columnar}");
        let (explicit, _) = run(
            &g,
            "MATCH (f:F)<-[:MEMBER]->(a:P) MATCH (f)<-[:MEMBER]->(b:P) WHERE a <> b \
             RETURN f.id, count(*) ORDER BY f.id",
        );
        assert_eq!(rows, explicit, "the rule equals `a <> b` here (columnar {columnar})");
    }
}

#[test]
fn a_single_hop_is_kept_off_every_relationship_of_a_variable_length_list() {
    for columnar in [false, true] {
        let g = corpus(columnar);
        // x = 1 walks [1->2] and [1->2->3]; the second path's only edge from
        // u = 1 is 1->2, which lies on both walks, so nothing survives.
        let (rows, c) = run(
            &g,
            "MATCH (x:C {id: 1})-[:NEXT*1..2]->(y:C), (u:C {id: 1})-[:NEXT]->(v:C) \
             RETURN y.id, v.id",
        );
        assert!(rows.is_empty(), "every walk uses 1->2 (columnar {columnar}): {rows:?}");
        assert!(count_of(&c, KEPT) >= 1);
        // from u = 2 the edge 2->3 is on the long walk only
        let (rows, _) = run(
            &g,
            "MATCH (x:C {id: 1})-[:NEXT*1..2]->(y:C), (u:C {id: 2})-[:NEXT]->(v:C) \
             RETURN y.id, v.id",
        );
        assert_eq!(ints(&rows), vec![vec![2, 3]], "columnar {columnar}");
        // two lists: [1->2] and [2->3] share nothing; any pair holding 1->2
        // twice, or 2->3 twice, is refused
        let (rows, _) = run(
            &g,
            "MATCH (x:C {id: 1})-[:NEXT*1..2]->(y:C), (u:C)-[:NEXT*1..1]->(v:C) \
             RETURN y.id, u.id ORDER BY y.id, u.id",
        );
        assert_eq!(ints(&rows), vec![vec![2, 2]], "columnar {columnar}");
    }
}

#[test]
fn paths_over_different_relationship_types_are_untouched() {
    let g = corpus(false);
    let (rows, c) = run(
        &g,
        "MATCH (f:F)-[:MEMBER]->(a:P), (a)-[:WROTE]->(m:M) RETURN f.id, a.id, m.id",
    );
    assert_eq!(ints(&rows), vec![vec![1, 10, 100]]);
    assert_eq!(count_of(&c, KEPT), 0, "no pair of types can meet, so no rewrite");
}

#[test]
fn a_star_projection_is_declined_and_counted() {
    let g = corpus(false);
    let (_, c) = run(&g, "MATCH (f:F)-[:MEMBER]->(a:P), (f)-[:MEMBER]->(b:P) RETURN *");
    assert_eq!(count_of(&c, KEPT), 0);
    assert!(
        count_of(&c, DECLINED) >= 1,
        "the hidden names would surface in `*`, so the decline must be visible"
    );
}

/// Two relationships are one only if their ends are, so a pair whose ends are
/// provably different nodes needs no predicate: a WHERE that says so, or two
/// inline maps pinning one key to different constants (SNB BI bi14's two
/// countries). The same constant, a map on one side only, or no statement
/// about the ends keeps the predicate.
#[test]
fn a_pair_whose_ends_cannot_meet_adds_nothing() {
    let g = corpus(false);
    let pairs = |src: &str, params: &[(&str, Value)]| {
        let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
        let p: BTreeMap<String, Value> = params.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect();
        let (rows, trace) = engram_observe::with_trace(|| {
            run_query(&g, &q, p).unwrap_or_else(|e| panic!("run `{src}`: {e}")).rows
        });
        (rows, count_of(trace.counters(), KEPT), count_of(trace.counters(), APART))
    };
    let two = "MATCH (f:F {id: $a})-[:MEMBER]->(x:P), (g:F {id: $b})-[:MEMBER]->(y:P) RETURN count(*)";
    // different constants: the forums differ, so their MEMBER edges do
    let (rows, kept, apart) = pairs(two, &[("a", Value::Int(1)), ("b", Value::Int(2))]);
    assert_eq!(ints(&rows), vec![vec![2]], "forum 1 has two members, forum 2 one");
    assert_eq!((kept, apart), (0, 1), "pinned to different ids: no predicate");
    // the same constant: one forum, and its two edges must still differ
    let (rows, kept, _) = pairs(two, &[("a", Value::Int(1)), ("b", Value::Int(1))]);
    assert_eq!(ints(&rows), vec![vec![2]], "(10,11) and (11,10) only");
    assert!(kept >= 1, "the same forum on both sides keeps the predicate");
    // a map on one side only proves nothing about the other
    let (rows, kept, _) = pairs(
        "MATCH (f:F {id: $a})-[:MEMBER]->(x:P), (g:F)-[:MEMBER]->(y:P) RETURN count(*)",
        &[("a", Value::Int(1))],
    );
    assert_eq!(ints(&rows), vec![vec![4]], "forum 1's two members against forum 1's other member and forum 2's one");
    assert!(kept >= 1, "one pinned side keeps the predicate");
    // a WHERE that keeps the ends apart
    let (rows, kept, apart) = pairs(
        "MATCH (f:F)-[:MEMBER]->(x:P), (f)-[:MEMBER]->(y:P) WHERE NOT x = y RETURN count(*)",
        &[],
    );
    assert_eq!(ints(&rows), vec![vec![2]]);
    assert_eq!((kept, apart), (0, 1), "the WHERE already keeps the members apart");
}
