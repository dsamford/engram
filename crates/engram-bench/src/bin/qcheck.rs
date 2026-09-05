//! Query-support checker: parse + run every statement in an SNB-style
//! `statements.json` against an EMPTY graph, reporting per-query PARSE_ERR /
//! RUN_ERR / OK. An empty graph is enough to surface the FEATURE gaps
//! (unsupported functions, clauses, and path forms error regardless of data);
//! data-dependent behaviour is verified separately against the loaded corpus.
//! Usage: `qcheck <statements.json>`.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;

use engram_cypher::{Value, json, parse_statement};
use engram_graph::{Graph, run_query};
use engram_key::{Namespace, Realm};
use engram_store::Store;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "statements.json".to_string());
    let raw = std::fs::read_to_string(&path).expect("read statements");
    let Value::List(arr) = json::from_json(&raw).expect("parse json") else {
        panic!("statements.json is not a JSON array");
    };
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    // Optional arg 2: a corpus export dir to LOAD, so data-dependent runtime
    // errors (a query that traverses real data) surface, not just feature gaps
    // that error on empty data.
    if let Some(dir) = std::env::args().nth(2) {
        let stats = engram_bench::load_export(&g, std::path::Path::new(&dir));
        let store = g.shared_store();
        store.seal();
        store.compact();
        eprintln!("[qcheck] loaded {} nodes, {} rels", stats.nodes, stats.rels);
    }
    let (mut ok, mut perr, mut rerr) = (0u32, 0u32, 0u32);
    for entry in arr {
        let Value::Map(m) = entry else { continue };
        let id = match m.get("id") {
            Some(Value::Str(s)) => s.clone(),
            _ => "?".to_string(),
        };
        let text = match m.get("text") {
            Some(Value::Str(s)) => s.clone(),
            _ => continue,
        };
        match parse_statement(&text) {
            Err(e) => {
                perr += 1;
                println!("{id}\tPARSE_ERR\t{e:?}");
            }
            Ok(q) => {
                let (res, trace) =
                    engram_observe::with_trace(|| run_query(&g, &q, BTreeMap::new()));
                // Which columnar operator (if any) fired — else it fell to the
                // per-tuple `run_streaming` interp (the slow path for the ICs).
                let path = [
                    ("core", "interp.pipeline hop runs"),
                    ("aggregate", "interp.pipeline aggregate runs"),
                    ("multistage", "interp.pipeline multistage runs"),
                    ("join", "interp.pipeline join runs"),
                    ("multistage-join", "interp.pipeline multistage-join runs"),
                    ("ic5", "interp.pipeline ic5 runs"),
                    ("optional", "interp.pipeline optional runs"),
                ]
                .iter()
                .find(|(_, c)| trace.counters().get(*c).copied().unwrap_or(0) > 0)
                .map(|(n, _)| *n)
                .unwrap_or("INTERP");
                match res {
                    Err(e) => {
                        rerr += 1;
                        println!("{id}\tRUN_ERR\t{e}");
                    }
                    Ok(_) => {
                        ok += 1;
                        println!("{id}\tOK\t{path}");
                    }
                }
            }
        }
    }
    println!("\nSUMMARY: ok={ok} parse_err={perr} run_err={rerr}");
}
