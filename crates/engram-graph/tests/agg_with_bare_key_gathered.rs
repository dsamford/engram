#![allow(non_snake_case)]
//! An aggregating WITH's BARE group-key carry — `WITH p, collect(DISTINCT
//! a.id) AS ids` — used to materialise the key in FULL per group, although the
//! RETURN after it read only `p.id` (fix 25, v101; on the production mirror
//! 73 full Proposal records per statement, 6.6 ms against Neo4j's 1.9). The
//! carry's readers are the later clauses: when every one of them reads the
//! var by property, the gathered property Map is what they read.
//!
//! Every answer is checked against the general path's (columnar paths off).

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn params() -> BTreeMap<String, Value> {
    let mut p = BTreeMap::new();
    p.insert("s".to_string(), Value::Str("review".to_string()));
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

fn general(g: &Graph, src: &str) -> Vec<Vec<Value>> {
    g.set_columnar_scans(false);
    let r = rows(g, src);
    g.set_columnar_scans(true);
    r
}

fn count_of(c: &BTreeMap<String, u64>, key: &str) -> u64 {
    c.get(key).copied().unwrap_or(0)
}

const FULL: &str = "graph.nodes materialised in full";
const GATHERED: &str = "interp.agg bare group key gathered for its later reads";
const OPTIONAL: &str = "interp.pipeline optional runs";
const AGG: &str = "interp.pipeline aggregate runs";

/// 300 `:Pr {id, status, description}` (every third `review`, a wide
/// description) each with 0–2 `-[:HA]->(:Art {id})`.
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    for i in 0..300i64 {
        let mut m = BTreeMap::new();
        m.insert("id".to_string(), Value::Str(format!("prop_{i:04}")));
        m.insert(
            "status".to_string(),
            Value::Str(if i % 3 == 0 { "review".into() } else { "draft".into() }),
        );
        m.insert("priority".to_string(), Value::Int(i % 5));
        m.insert("description".to_string(), Value::Str("d".repeat(2000)));
        let p = g.create_node(&["Pr".into()], &m).expect("pr");
        // 0–2 artifacts, varying across the `review` proposals too (every
        // third), so the OPTIONAL end is null on some groups and bound on others.
        for k in 0..((i / 3) % 3) {
            let mut a = BTreeMap::new();
            a.insert("id".to_string(), Value::Str(format!("art_{i:04}_{k}")));
            let art = g.create_node(&["Art".into()], &a).expect("art");
            g.create_rel(p, "HA", art, &BTreeMap::new()).expect("ha");
        }
    }
    g
}

/// The production shape (`+ one OPTIONAL MATCH collect` on the mirror): the
/// pipeline's OPTIONAL operator with an aggregating WITH and a plain RETURN
/// over the aliases. (An ORDER BY over the key's own property declines the
/// operator to the general path, which has its own demand analysis.)
const PROP_READS: &str = "MATCH (p:Pr) WHERE p.status = $s OPTIONAL MATCH (p)-[:HA]->(a:Art) WITH p, collect(DISTINCT a.id) AS ids RETURN p.id AS id, ids LIMIT 25";
const POST_WHERE: &str = "MATCH (p:Pr) WHERE p.status = $s OPTIONAL MATCH (p)-[:HA]->(a:Art) WITH p, count(a) AS c WHERE p.priority > 1 RETURN p.id AS id, c LIMIT 10";
/// CONTROL: the RETURN uses the key BARE — the full node stays.
const BARE_RETURN: &str = "MATCH (p:Pr) WHERE p.status = $s OPTIONAL MATCH (p)-[:HA]->(a:Art) WITH p, collect(DISTINCT a.id) AS ids RETURN p, ids ORDER BY p.priority DESC LIMIT 5";
/// CONTROL: Form B — the aggregating RETURN itself carries the bare key.
const FORM_B: &str = "MATCH (p:Pr) WHERE p.status = $s RETURN p, count(*) AS n ORDER BY p.id LIMIT 5";

#[test]
fn a_bare_carry_read_only_by_property_after_the_with_is_gathered_not_materialised() {
    let g = corpus();
    for src in [PROP_READS, POST_WHERE] {
        let want = general(&g, src);
        assert!(!want.is_empty(), "fixture: rows for `{src}`");
        let _ = rows(&g, src); // warm the columns the walk keeps
        let (got, c) = traced(&g, src);
        assert_eq!(got, want, "`{src}`");
        assert!(count_of(&c, OPTIONAL) > 0 || count_of(&c, AGG) > 0, "the pipeline must run `{src}`: {c:?}");
        assert!(count_of(&c, GATHERED) > 0, "`{src}` gathers the bare carry: {c:?}");
        assert_eq!(count_of(&c, FULL), 0, "`{src}` materialises nothing in full: {c:?}");
    }
}

/// An aggregating RETURN whose bare key IS the output keeps the full key.
/// (A bare RETURN item beside a top-k is gathered and its survivors hydrated
/// since fix 31 (v106) — see `a_bare_return_beside_a_topk_is_gathered_and_hydrated`.)
#[test]
fn an_aggregating_return_keeps_the_full_key() {
    let g = corpus();
    let src = FORM_B;
    let want = general(&g, src);
    let (got, c) = traced(&g, src);
    assert_eq!(got, want, "`{src}`");
    assert_eq!(count_of(&c, GATHERED), 0, "`{src}`: {c:?}");
    assert!(count_of(&c, FULL) > 0, "`{src}` needs the node: {c:?}");
}

#[test]
fn a_bare_return_beside_a_topk_is_gathered_and_hydrated() {
    let g = corpus();
    let src = BARE_RETURN;
    let want = general(&g, src);
    assert_eq!(want.len(), 5, "fixture: `{src}`");
    let (got, c) = traced(&g, src);
    assert_eq!(got, want, "`{src}`");
    assert!(count_of(&c, GATHERED) > 0, "`{src}` gathers the carry: {c:?}");
    assert_eq!(
        count_of(&c, "interp.agg bare return item hydrated for a survivor"),
        5,
        "`{src}`: {c:?}"
    );
    assert_eq!(count_of(&c, FULL), 5, "`{src}` decodes only the survivors: {c:?}");
}
