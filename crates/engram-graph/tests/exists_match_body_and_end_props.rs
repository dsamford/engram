#![allow(non_snake_case)]
//! Two existence-predicate spellings the columnar paths declined, measured on
//! the production mirror (2026-09-04):
//!
//! 1. `NOT EXISTS { MATCH (n)-[:T]->(:L) }` — a Query body whose only clause
//!    is a plain MATCH — cost 2,788 ms over the email label where the SAME
//!    predicate spelled `NOT exists((n)-[:T]->(:L))` cost 95 ms: the Query body
//!    was never lifted to an adjacency probe and demanded the node in FULL, so
//!    the count materialised 38k email records. `pattern_body` now reads both
//!    spellings as one pattern.
//! 2. `exists((g)-[:T…]->(:Country {iso3: $a}))` — a far end carrying an inline
//!    property map — took 20.3 s over 44k GeopoliticalEvent (Neo4j 40 ms): the
//!    probe refused the map, the stage declined, and the general path ran the
//!    pattern matcher per row. The map now resolves ONCE into the far-end ids
//!    and each member probes its adjacency against that set.
//!
//! The contract for both: the columnar path answers exactly what the general
//! path answers (rows, order), the operator that should fire fires (counted),
//! and the shapes that must still decline (a map reading a variable, an
//! unlabelled far end with a map) decline and agree.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("a".to_string(), Value::Str("USA".to_string()));
    p.insert("b".to_string(), Value::Str("ISR".to_string()));
    p
}

fn rows(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    let q = parse_statement(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_query(g, &q, params())
        .unwrap_or_else(|e| panic!("run `{src}`: {e}"))
        .rows
}

/// `src` with the columnar operators ON, then the general path (the oracle).
fn both(g: &Graph, src: &str) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    g.set_columnar_scans(true);
    let on = rows(g, src);
    g.set_columnar_scans(false);
    let off = rows(g, src);
    g.set_columnar_scans(true);
    (on, off)
}

fn counter(g: &Graph, src: &str, key: &str) -> u64 {
    g.set_columnar_scans(true);
    let (_, trace) = engram_observe::with_trace(|| rows(g, src));
    trace.counters().get(key).copied().unwrap_or(0)
}

const AGG: &str = "interp.columnar aggregate scans";
const STAGE: &str = "interp.columnar stages";
const END_MAP: &str = "interp.columnar probe resolved its far-end map once";

const COUNTRIES: [&str; 5] = ["USA", "ISR", "SRB", "HRV", "DEU"];

/// 700 `:Doc` events (dense in id space, above the seek floor), 5
/// `:Country` nodes, `OCCURS_IN` / `MENTIONS_COUNTRY` edges by residue, and
/// an `:Ask` child under every fourth even doc — the shapes of the two
/// production statements in miniature.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    let mut countries = Vec::new();
    for iso in COUNTRIES {
        let mut m = BTreeMap::new();
        m.insert("iso3".to_string(), Value::Str(iso.to_string()));
        countries.push(g.create_node(&["Country".into()], &m).expect("country"));
    }
    for i in 0..700i64 {
        let mut m = BTreeMap::new();
        m.insert(
            "kind".to_string(),
            Value::Str(if i % 7 == 0 { "note" } else { "email" }.to_string()),
        );
        m.insert("n".to_string(), Value::Int(i));
        if i % 3 != 0 {
            m.insert("startAt".to_string(), Value::Str(format!("2026-08-{:02}", 1 + i % 28)));
        }
        if i % 11 == 0 {
            m.insert(
                "affected".to_string(),
                Value::List(vec![Value::Str("DEU".into()), Value::Str("USA".into())]),
            );
        }
        if i % 13 == 0 {
            m.insert("region".to_string(), Value::Str("ISR".to_string()));
        }
        let d = g.create_node(&["Doc".into()], &m).expect("doc");
        // Country edges: every doc with i % 5 == k occurs in country k; every
        // doc with i % 9 == 0 also mentions USA (a second type into the set).
        let c = countries[(i % 5) as usize];
        g.create_rel(d, "OCCURS_IN", c, &BTreeMap::new()).expect("rel");
        if i % 9 == 0 {
            g.create_rel(d, "MENTIONS_COUNTRY", countries[0], &BTreeMap::new())
                .expect("rel");
        }
        if i % 8 == 0 {
            let mut a = BTreeMap::new();
            a.insert("k".to_string(), Value::Int(i));
            a.insert("resolved".to_string(), Value::Bool(i % 16 == 0));
            let ask = g.create_node(&["Ask".into()], &a).expect("ask");
            g.create_rel(d, "HAS", ask, &BTreeMap::new()).expect("rel");
        }
    }
    g
}

// ─── 1. `EXISTS { MATCH … }` is the pattern spelling ─────────────────────────

const ANTI_JOIN_MATCH: &str = "MATCH (n:Doc) WHERE n.kind = 'email' AND NOT EXISTS { MATCH (n)-[:HAS]->(:Ask) } AND coalesce(n.n, 0) < 650 RETURN count(n) AS n";
const ANTI_JOIN_PATTERN: &str = "MATCH (n:Doc) WHERE n.kind = 'email' AND NOT EXISTS { (n)-[:HAS]->(:Ask) } AND coalesce(n.n, 0) < 650 RETURN count(n) AS n";
const ANTI_JOIN_FN: &str = "MATCH (n:Doc) WHERE n.kind = 'email' AND NOT exists((n)-[:HAS]->(:Ask)) AND coalesce(n.n, 0) < 650 RETURN count(n) AS n";

#[test]
fn a_single_match_query_body_lifts_to_the_probe_like_the_pattern_spelling() {
    let g = corpus();
    let (on, off) = both(&g, ANTI_JOIN_MATCH);
    assert_eq!(on, off, "columnar vs general disagree on the MATCH spelling");
    assert_eq!(rows(&g, ANTI_JOIN_PATTERN), on, "the bare-pattern spelling must agree");
    assert_eq!(rows(&g, ANTI_JOIN_FN), on, "the exists() spelling must agree");
    for src in [ANTI_JOIN_MATCH, ANTI_JOIN_PATTERN, ANTI_JOIN_FN] {
        assert!(
            counter(&g, src, AGG) > 0,
            "`{src}` must run as a columnar aggregate (the probe lifted)"
        );
    }
    // Fixture sanity: emails (i % 7 != 0) below 650 without an Ask (i % 8 != 0).
    let expect = (0..650i64).filter(|i| i % 7 != 0 && i % 8 != 0).count() as i64;
    assert_eq!(on, vec![vec![Value::Int(expect)]]);
}

/// `COUNT { MATCH (n)-[:HAS]->() }` is the degree, the same as `COUNT { (n)-[:HAS]->() }`.
#[test]
fn a_single_match_count_body_is_the_degree() {
    let g = corpus();
    let with_match = "MATCH (n:Doc) RETURN sum(COUNT { MATCH (n)-[:HAS]->() }) AS s";
    let bare = "MATCH (n:Doc) RETURN sum(COUNT { (n)-[:HAS]->() }) AS s";
    let (on, off) = both(&g, with_match);
    assert_eq!(on, off);
    assert_eq!(rows(&g, bare), on);
    assert!(counter(&g, with_match, AGG) > 0);
    assert_eq!(on, vec![vec![Value::Int((0..700i64).filter(|i| i % 8 == 0).count() as i64)]]);
}

/// A body with a WHERE, or with more than one clause, keeps its treatment —
/// and still agrees.
#[test]
fn bodies_that_are_not_one_plain_match_still_agree() {
    let g = corpus();
    for src in [
        "MATCH (n:Doc) WHERE n.kind = 'email' AND EXISTS { MATCH (n)-[:HAS]->(a:Ask) WHERE a.resolved = false } RETURN count(n) AS n",
        "MATCH (n:Doc) WHERE n.kind = 'email' AND EXISTS { MATCH (n)-[:HAS]->(a:Ask) WITH a WHERE a.k > 100 RETURN a } RETURN count(n) AS n",
        "MATCH (n:Doc) WHERE n.kind = 'email' AND EXISTS { OPTIONAL MATCH (n)-[:HAS]->(a:Ask) } RETURN count(n) AS n",
        "MATCH (n:Doc) WHERE NOT EXISTS { MATCH (n)-[:HAS]->(:Ask) } WITH n ORDER BY n.n DESC LIMIT 5 RETURN n.n AS n",
    ] {
        let (on, off) = both(&g, src);
        assert_eq!(on, off, "columnar vs general disagree on `{src}`");
    }
}

// ─── 2. a far end with an inline property map ────────────────────────────────

const GEO_COUNT: &str = "MATCH (g:Doc) WHERE exists((g)-[:OCCURS_IN|MENTIONS_COUNTRY]->(:Country {iso3: $a})) RETURN count(g) AS n";
const GEO_OR: &str = "MATCH (g:Doc) WHERE g.startAt IS NOT NULL AND ($a IN coalesce(g.affected, []) OR g.region = $a OR exists((g)-[:OCCURS_IN|MENTIONS_COUNTRY]->(:Country {iso3: $a}))) AND ($b IN coalesce(g.affected, []) OR g.region = $b OR exists((g)-[:OCCURS_IN|MENTIONS_COUNTRY]->(:Country {iso3: $b}))) RETURN count(g) AS n";
const GEO_STAGE: &str = "MATCH (g:Doc) WHERE g.startAt IS NOT NULL AND exists((g)-[:OCCURS_IN]->(:Country {iso3: $a})) WITH g, g.n AS n WHERE n >= 10 WITH collect({n: n, s: g.startAt}) AS events WITH [e IN events WHERE e.n < 400 | e.n] AS small, [e IN events | e.n] AS all RETURN size(small) AS a, size(all) AS b";
const GEO_LITERAL: &str = "MATCH (g:Doc) WHERE exists((g)-[:OCCURS_IN]->(:Country {iso3: 'SRB'})) RETURN count(g) AS n";
const GEO_TWO_KEYS: &str = "MATCH (g:Doc) WHERE exists((g)-[:OCCURS_IN]->(:Country {iso3: $a, missing: 1})) RETURN count(g) AS n";

#[test]
fn a_far_end_map_resolves_once_and_the_probe_answers_from_adjacency() {
    let g = corpus();
    for src in [GEO_COUNT, GEO_OR, GEO_STAGE, GEO_LITERAL, GEO_TWO_KEYS] {
        let (on, off) = both(&g, src);
        assert_eq!(on, off, "columnar vs general disagree on `{src}`");
        assert!(
            counter(&g, src, END_MAP) > 0,
            "`{src}` must resolve its far-end map once and run columnar"
        );
    }
    assert!(counter(&g, GEO_COUNT, AGG) > 0);
    assert!(counter(&g, GEO_STAGE, STAGE) > 0);
    // Fixture sanity: USA is country 0 — docs with i % 5 == 0, plus i % 9 == 0.
    let usa = (0..700i64).filter(|i| i % 5 == 0 || i % 9 == 0).count() as i64;
    assert_eq!(rows(&g, GEO_COUNT), vec![vec![Value::Int(usa)]]);
    assert_eq!(rows(&g, GEO_TWO_KEYS), vec![vec![Value::Int(0)]]);
}

/// A map that reads a variable, or an unlabelled far end with a map, is not
/// a once-resolved set — it keeps the general path, and agrees.
#[test]
fn maps_the_probe_cannot_resolve_once_decline_and_agree() {
    let g = corpus();
    for src in [
        "MATCH (g:Doc) WHERE exists((g)-[:OCCURS_IN]->(:Country {iso3: g.region})) RETURN count(g) AS n",
        "MATCH (g:Doc) WHERE exists((g)-[:OCCURS_IN]->({iso3: $a})) RETURN count(g) AS n",
        "MATCH (g:Doc) WHERE exists((g)-[:OCCURS_IN]->(:Country {iso3: $a, name: g.kind})) RETURN count(g) AS n",
    ] {
        let (on, off) = both(&g, src);
        assert_eq!(on, off, "columnar vs general disagree on `{src}`");
        assert_eq!(counter(&g, src, END_MAP), 0, "`{src}` must not resolve a far-end map");
    }
}

/// A far end WIDE enough to seek with a DECLARED key on the map probes the
/// index for its candidates instead of walking the label — and answers the
/// same as a walk.
#[test]
fn a_declared_key_on_a_wide_far_end_seeks_its_candidates() {
    use engram_cypher::parse_any;
    use engram_graph::run_stmt;
    let g = corpus();
    g.set_label_scoped_indexes(true);
    run_stmt(
        &g,
        &parse_any("CREATE INDEX big_k IF NOT EXISTS FOR (n:Big) ON (n.k)").expect("parse"),
        BTreeMap::new(),
    )
    .expect("ddl");
    // 600 `:Big` (above the seek floor); every doc points at Big (i % 600).
    let mut bigs = Vec::new();
    for k in 0..600i64 {
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), Value::Int(k));
        bigs.push(g.create_node(&["Big".into()], &m).expect("big"));
    }
    let docs = rows(&g, "MATCH (d:Doc) RETURN id(d) AS id ORDER BY id");
    for (i, r) in docs.iter().enumerate() {
        let Value::Int(id) = r[0] else { panic!() };
        g.create_rel(id as u64, "NEAR", bigs[i % 600], &BTreeMap::new())
            .expect("rel");
    }
    let src = "MATCH (d:Doc) WHERE exists((d)-[:NEAR]->(:Big {k: 7})) RETURN count(d) AS n";
    let (on, off) = both(&g, src);
    assert_eq!(on, off);
    assert!(counter(&g, src, "interp.columnar probe sought its far end") > 0);
    assert!(counter(&g, src, END_MAP) > 0);
    assert_eq!(on, vec![vec![Value::Int((0..700i64).filter(|i| i % 600 == 7).count() as i64)]]);
}

/// The far-end set is a CANDIDATE filter on the neighbour, never a node read:
/// a country nothing points at, and a second relationship type into the set,
/// both answer exactly as the general path does.
#[test]
fn the_probe_tests_membership_of_the_neighbour_in_the_resolved_set() {
    let g = corpus();
    let none = "MATCH (g:Doc) WHERE exists((g)-[:OCCURS_IN]->(:Country {iso3: 'XXX'})) RETURN count(g) AS n";
    let mentions = "MATCH (g:Doc) WHERE exists((g)-[:MENTIONS_COUNTRY]->(:Country {iso3: $a})) RETURN count(g) AS n";
    let (on, off) = both(&g, none);
    assert_eq!(on, off);
    assert_eq!(on, vec![vec![Value::Int(0)]]);
    let (on, off) = both(&g, mentions);
    assert_eq!(on, off);
    assert_eq!(
        on,
        vec![vec![Value::Int((0..700i64).filter(|i| i % 9 == 0).count() as i64)]]
    );
}
