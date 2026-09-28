#![allow(non_snake_case)]
//! Does a variable-length path written from its UNBOUND end seed from the
//! bound one? FinBench tcr2's shape, which timed out at SF10.
//!
//! `MATCH (p:Person {id: $id})-[:own]->(a:Account), q=(o:Account)-[:transfer*1..3]->(a)`
//! binds `a` in the first path, then writes the second path starting from `o`,
//! which is unbound. Seeded from `o` it is every account; seeded from `a` and
//! walked backwards it is one person's accounts.

use std::collections::BTreeMap;

use engram_cypher::{parse_any, parse_statement};
use engram_graph::{Graph, run_query, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn ddl(g: &Graph, q: &str) {
    let s = parse_any(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
}

fn trace(g: &Graph, q: &str) -> BTreeMap<String, u64> {
    let s = parse_statement(q).unwrap_or_else(|e| panic!("parse `{q}`: {e}"));
    let (_, t) = engram_observe::with_trace(|| {
        run_query(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{q}`: {e}"));
    });
    t.counters().iter().map(|(k, v)| (k.clone(), *v)).collect()
}

#[test]
#[ignore = "diagnostic — run with --ignored --nocapture"]
fn which_end_does_a_backwards_varlength_path_seed_from() {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    ddl(&g, "UNWIND range(0, 1999) AS i CREATE (:Account {id: i})");
    ddl(&g, "UNWIND range(0, 49) AS i CREATE (:Person {id: i})");
    ddl(
        &g,
        "UNWIND range(0, 49) AS i MATCH (p:Person {id: i}), (a:Account {id: i}) CREATE (p)-[:own]->(a)",
    );
    ddl(
        &g,
        "UNWIND range(0, 1998) AS i MATCH (a:Account {id: i}), (b:Account {id: (i * 7 + 3) % 2000}) CREATE (a)-[:transfer {timestamp: i}]->(b)",
    );

    for (label, q) in [
        (
            "written from the UNBOUND end (tcr2's form)",
            "MATCH (person:Person {id: 3})-[:own]->(account:Account), p=(other:Account)-[t:transfer*1..3]->(account) RETURN count(DISTINCT other) AS n",
        ),
        (
            "written from the BOUND end, reversed by hand",
            "MATCH (person:Person {id: 3})-[:own]->(account:Account), p=(account)<-[t:transfer*1..3]-(other:Account) RETURN count(DISTINCT other) AS n",
        ),
    ] {
        let c = trace(&g, q);
        let pick = |k: &str| c.get(k).copied().unwrap_or(0);
        let mut seeds: Vec<(String, u64)> = c
            .iter()
            .filter(|(k, v)| {
                **v > 0
                    && (k.contains("seed")
                        || k.contains("scan")
                        || k.contains("revers")
                        || k.contains("materialis"))
            })
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        seeds.sort();
        println!("--- {label}");
        println!("    store.gets = {}", pick("store.gets"));
        for (k, v) in seeds {
            println!("    {v:>8}  {k}");
        }
    }
}
