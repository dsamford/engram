#![allow(non_snake_case)]
//! Fix 63: an aggregate whose argument is a MAP (or LIST) literal over one
//! variable — `collect({id: gp.id, type: gp.type, createdAt: gp.createdAt})`
//! — is admitted to the aggregate pipeline; `key_side` and `eval_column`
//! had no arm for the literal, so the production orchestrator statement
//! ran the general path with a projected get per hop end (6.3 ms against
//! Neo4j's 0.9 on the mirror) while its `count(gp)` spelling ran the
//! pipeline in 0.5.
//!
//! Every answer is checked against the same statement with the columnar
//! paths OFF (the general path).

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
    p.insert("minAge".to_string(), Value::Str("2026-08-20T00:00:00Z".into()));
    p.insert("minCount".to_string(), Value::Int(3));
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

/// `collect` order is unspecified, and the pipeline's chain enumerates a
/// two-hop population in a different row order from the general path's
/// (a pre-existing difference — `collect(gp.id)` shows it the same way),
/// so the collected lists are compared as multisets.
fn sort_lists(rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.into_iter()
        .map(|r| {
            r.into_iter()
                .map(|v| match v {
                    Value::List(mut items) => {
                        items.sort_by_key(|x| format!("{x:?}"));
                        Value::List(items)
                    }
                    other => other,
                })
                .collect()
        })
        .collect()
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

const PIPELINE: &str = "interp.pipeline aggregate runs";
const PROJECTED: &str = "graph.projected node materialisations";

/// 40 orchestrators × 20 tasks × 3 proposals; every fourth proposal has
/// no `type` (a null map value the general path keeps).
fn corpus() -> Graph {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "CREATE INDEX gwp_status FOR (n:GraphWriteProposal) ON (n.status)");
    for o in 0..40i64 {
        let mut m = BTreeMap::new();
        m.insert("id".into(), s(&format!("orch-{o}")));
        m.insert("userId".into(), s(&format!("u{}", o % 5)));
        m.insert("conversationId".into(), s(&format!("conv-{o}")));
        let on = g.create_node(&["MarketOrchestrator".into()], &m).expect("o");
        for t in 0..20i64 {
            let mut tm = BTreeMap::new();
            tm.insert("id".into(), s(&format!("task-{o}-{t}")));
            let tn = g.create_node(&["ResearchTask".into()], &tm).expect("t");
            g.create_rel(on, "DISPATCHED", tn, &BTreeMap::new()).expect("d");
            for p in 0..3i64 {
                let mut pm = BTreeMap::new();
                pm.insert("id".into(), s(&format!("prop-{o}-{t}-{p}")));
                if (o + t + p) % 4 != 0 {
                    pm.insert("type".into(), s("research"));
                }
                pm.insert("status".into(), s(if (t + p) % 3 == 0 { "pending" } else { "applied" }));
                pm.insert("createdAt".into(), s(&format!("2026-08-{:02}T00:00:00Z", 1 + (t + p) % 28)));
                let pn = g.create_node(&["GraphWriteProposal".into()], &pm).expect("p");
                g.create_rel(tn, "PROPOSED_GRAPH_WRITE", pn, &BTreeMap::new()).expect("pgw");
            }
        }
    }
    g
}

const ORIG: &str = "MATCH (o:MarketOrchestrator)-[:DISPATCHED]->(r:ResearchTask)-[:PROPOSED_GRAPH_WRITE]->(gp:GraphWriteProposal {status: 'pending'}) \
    WHERE gp.createdAt < $minAge \
    WITH o, collect({id: gp.id, type: gp.type, createdAt: gp.createdAt}) AS proposals \
    WHERE size(proposals) >= $minCount \
    RETURN o.id AS orchestratorId, o.userId AS userId, o.conversationId AS conversationId, proposals \
    ORDER BY orchestratorId LIMIT 100";

#[test]
fn a_collect_over_a_map_literal_runs_on_the_pipeline() {
    let g = corpus();
    let want = general(&g, ORIG);
    assert_eq!(want.len(), 40);
    // The general path keeps a null map value: `type` is absent on a quarter.
    let has_null_type = want.iter().any(|r| match &r[3] {
        Value::List(items) => items.iter().any(|it| match it {
            Value::Map(m) => matches!(m.get("type"), Some(Value::Null)),
            _ => false,
        }),
        _ => false,
    });
    assert!(has_null_type, "fixture: a null-valued map entry is kept");
    let (got, c) = traced(&g, ORIG);
    assert_eq!(sort_lists(got), sort_lists(want));
    assert_eq!(count_of(&c, PIPELINE), 1, "{c:?}");
    assert_eq!(count_of(&c, PROJECTED), 0, "no per-row projected get: {c:?}");
    // The pre-existing order difference is the chain's, not the map's:
    // the property spelling collects in the same order as the map one.
    let prop = ORIG.replace(
        "collect({id: gp.id, type: gp.type, createdAt: gp.createdAt})",
        "collect(gp.id)",
    );
    let (ids, c) = traced(&g, &prop);
    assert_eq!(count_of(&c, PIPELINE), 1, "{c:?}");
    let (maps, _) = traced(&g, ORIG);
    for (a, b) in ids.iter().zip(&maps) {
        let from_maps: Vec<Value> = match &b[3] {
            Value::List(items) => items
                .iter()
                .map(|it| match it {
                    Value::Map(m) => m.get("id").cloned().unwrap_or(Value::Null),
                    _ => Value::Null,
                })
                .collect(),
            _ => Vec::new(),
        };
        assert_eq!(a[3], Value::List(from_maps), "same production order as collect(gp.id)");
    }
}

/// A list literal and a map nested in a scalar function admit the same way;
/// a map that reads TWO variables still declines (byte-identical answer).
#[test]
fn a_list_literal_and_a_two_variable_map() {
    let g = corpus();
    let list = "MATCH (o:MarketOrchestrator)-[:DISPATCHED]->(r:ResearchTask)-[:PROPOSED_GRAPH_WRITE]->(gp:GraphWriteProposal) \
        WHERE gp.status = 'pending' \
        WITH o, collect([gp.id, gp.createdAt]) AS pairs \
        RETURN o.id AS id, size(pairs) AS n ORDER BY id";
    let want = general(&g, list);
    let (got, c) = traced(&g, list);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, PIPELINE), 1, "{c:?}");
    let two = "MATCH (o:MarketOrchestrator)-[:DISPATCHED]->(r:ResearchTask)-[:PROPOSED_GRAPH_WRITE]->(gp:GraphWriteProposal) \
        WHERE gp.status = 'pending' \
        WITH o, collect({task: r.id, prop: gp.id}) AS pairs \
        RETURN o.id AS id, size(pairs) AS n ORDER BY id";
    let want = general(&g, two);
    let (got, c) = traced(&g, two);
    assert_eq!(got, want);
    assert_eq!(count_of(&c, PIPELINE), 0, "a two-variable map declines: {c:?}");
}
