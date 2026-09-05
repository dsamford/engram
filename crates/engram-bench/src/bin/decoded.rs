//! The identical-decoded-values run — the M7 kill criterion's value half.
//!
//! Loads the closed world the incumbent exported (`shadow-export.json`:
//! anchor labels + one-hop peers + rels, captured at the DRIVER level so
//! labels survive), replays the qualifying corpus statements on Engram,
//! and compares decoded rows as multisets of canonicalised JSON.
//!
//! Canonicalisation is engine-neutral and applied to BOTH sides in this
//! process: integral floats collapse to ints (JS already collapsed them at
//! JSON.stringify), temporal strings are re-rendered through one formatter,
//! nodes lose identity (ids differ across engines by construction) and
//! keep sorted labels + properties, relationships keep type + properties.
//!
//! Statement classes the comparison must refuse to over-read:
//!   - `order_dependent` (LIMIT without ORDER BY): WHICH rows is engine
//!     choice — only the count is compared.
//!   - `id_space` (elementId()/id() in the projection): the values are
//!     definitionally different — only the count is compared.
//!   - `truncated` (incumbent capture hit its row cap): skipped outright —
//!     a truncated multiset comparison would read as a finding.
//!   - `tie_boundary` (ORDER BY … LIMIT with ties at the cut): detected,
//!     never assumed — every differing row must carry the SAME sort-key
//!     tuple on both sides, verified value by value.
//!
//! The instrument canaries ITSELF on every run: an embedded fixture with a
//! known divergence, a known match, and a known tie must classify exactly
//! before the real export is read.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;

use engram_bench::{
    canon_engram, canon_incumbent, get_bool, get_list, get_str, tie_at_limit_boundary, untag_prop,
    untag_temporal,
};
use engram_cypher::{Value, json, parse_any};
use engram_graph::{Graph, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

#[derive(Default)]
struct Tally {
    identical: usize,
    empty_on_both: usize,
    count_match_unordered_limit: usize,
    count_match_id_space: usize,
    tie_boundary: usize,
    diverged: Vec<String>,
    engram_parse_error: Vec<String>,
    engram_run_error: Vec<String>,
    neo4j_error: usize,
    skipped_truncated: usize,
    world_nodes: usize,
    world_rels: usize,
    unloadable: usize,
}

fn run_export(doc: &BTreeMap<String, Value>) -> (Tally, Vec<String>) {
    let mut t = Tally::default();
    let graph = Graph::new(Store::new(), Realm(1), Namespace(1));
    // `datetime()` in a filter compares against NOW — inject the capture's
    // own timestamp so both engines saw the same clock. (Time is injected,
    // never ambient; the export carries the only correct value.)
    if let Some(Value::Str(at)) = doc.get("captured_at") {
        if let Some(Value::DateTime {
            epoch_seconds,
            nanos,
            ..
        }) = untag_temporal("~dt", at)
        {
            graph.set_wall_ms(epoch_seconds * 1_000 + i64::from(nanos / 1_000_000));
        }
    }

    let mut id_map: BTreeMap<String, u64> = BTreeMap::new();
    for n in get_list(doc, "nodes") {
        let Value::Map(n) = n else { continue };
        let labels: Vec<String> = get_list(n, "labels")
            .iter()
            .filter_map(|l| match l {
                Value::Str(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        let props: BTreeMap<String, Value> = match n.get("props") {
            Some(Value::Map(p)) => p
                .iter()
                .map(|(k, v)| (k.clone(), untag_prop(v, &mut t.unloadable)))
                .collect(),
            _ => BTreeMap::new(),
        };
        let id = graph.create_node(&labels, &props).expect("create node");
        id_map.insert(get_str(n, "id"), id);
        t.world_nodes += 1;
    }
    let mut dangling = 0usize;
    for r in get_list(doc, "rels") {
        let Value::Map(r) = r else { continue };
        let (Some(&src), Some(&dst)) = (
            id_map.get(&get_str(r, "src")),
            id_map.get(&get_str(r, "dst")),
        ) else {
            dangling += 1;
            continue;
        };
        let props: BTreeMap<String, Value> = match r.get("props") {
            Some(Value::Map(p)) => p
                .iter()
                .map(|(k, v)| (k.clone(), untag_prop(v, &mut t.unloadable)))
                .collect(),
            _ => BTreeMap::new(),
        };
        graph
            .create_rel(src, &get_str(r, "type"), dst, &props)
            .expect("create rel");
        t.world_rels += 1;
    }
    assert_eq!(dangling, 0, "the export's rels must close over its nodes");

    let mut details: Vec<String> = Vec::new();
    for st in get_list(doc, "results") {
        let Value::Map(st) = st else { continue };
        let text = get_str(st, "text");
        let key = format!("{}:{}", get_str(st, "file"), get_str(st, "line"));
        if st.contains_key("error") {
            t.neo4j_error += 1;
            continue;
        }
        if get_bool(st, "truncated") {
            t.skipped_truncated += 1;
            continue;
        }
        let stmt = match parse_any(&text) {
            Ok(s) => s,
            Err(e) => {
                t.engram_parse_error.push(format!("{key}: {e}"));
                continue;
            }
        };
        let res = match run_stmt(&graph, &stmt, BTreeMap::new()) {
            Ok(r) => r,
            Err(e) => {
                t.engram_run_error.push(format!("{key}: {e:?}"));
                continue;
            }
        };
        let inc_rows = get_list(st, "rows");
        if get_bool(st, "order_dependent") || get_bool(st, "id_space") {
            if inc_rows.len() == res.rows.len() {
                if get_bool(st, "id_space") {
                    t.count_match_id_space += 1;
                } else {
                    t.count_match_unordered_limit += 1;
                }
            } else {
                t.diverged.push(key.clone());
                details.push(format!(
                    "{key}: COUNT {} vs {} (count-only class)\n  {}",
                    inc_rows.len(),
                    res.rows.len(),
                    text.replace('\n', " ")
                ));
            }
            continue;
        }
        // Column names must agree (unknowable from an empty incumbent set).
        let inc_cols: Vec<String> = match st.get("cols") {
            Some(Value::List(c)) => c
                .iter()
                .filter_map(|x| match x {
                    Value::Str(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        if !inc_cols.is_empty() && inc_cols != res.columns {
            t.diverged.push(key.clone());
            details.push(format!(
                "{key}: COLUMNS {:?} vs {:?}\n  {}",
                inc_cols,
                res.columns,
                text.replace('\n', " ")
            ));
            continue;
        }
        // Multiset comparison of canonicalised rows, values kept beside
        // their serialisations so the tie check can read sort keys.
        let mut inc: Vec<(String, Value)> = inc_rows
            .iter()
            .map(|row| {
                // Canonicalise per-COLUMN, never the row envelope: a row is a
                // positional tuple, so its outer list must keep column order.
                // `canon_list` (which sorts) applies only to nested collect()
                // columns — mirror the engram side (below) exactly.
                let v = match row {
                    Value::List(cols) => Value::List(cols.iter().map(canon_incumbent).collect()),
                    other => canon_incumbent(other),
                };
                (json::to_json(&v), v)
            })
            .collect();
        let mut eng: Vec<(String, Value)> = res
            .rows
            .iter()
            .map(|row| {
                let v = Value::List(row.iter().map(canon_engram).collect());
                (json::to_json(&v), v)
            })
            .collect();
        inc.sort_by(|a, b| a.0.cmp(&b.0));
        eng.sort_by(|a, b| a.0.cmp(&b.0));
        if inc.iter().map(|(s, _)| s).eq(eng.iter().map(|(s, _)| s)) {
            if inc.is_empty() {
                t.empty_on_both += 1;
            } else {
                t.identical += 1;
            }
            continue;
        }
        let only_inc: Vec<&Value> = inc
            .iter()
            .filter(|(s, _)| !eng.iter().any(|(es, _)| es == s))
            .map(|(_, v)| v)
            .collect();
        let only_eng: Vec<&Value> = eng
            .iter()
            .filter(|(s, _)| !inc.iter().any(|(is, _)| is == s))
            .map(|(_, v)| v)
            .collect();
        if tie_at_limit_boundary(&text, &res.columns, &only_inc, &only_eng) {
            t.tie_boundary += 1;
            details.push(format!(
                "{key}: TIE at the LIMIT boundary — {} row(s) swapped, same sort key\n  {}",
                only_inc.len(),
                text.replace('\n', " ")
            ));
            continue;
        }
        t.diverged.push(key.clone());
        let clip = |v: &[&Value]| -> Vec<String> {
            v.iter()
                .take(2)
                .map(|x| json::to_json(x).chars().take(400).collect())
                .collect()
        };
        details.push(format!(
            "{key}: {} vs {} rows{}\n  stmt: {}\n  only-neo4j: {:?}\n  only-engram: {:?}",
            inc.len(),
            eng.len(),
            if get_bool(st, "multi_hop") {
                " [multi_hop: boundary risk]"
            } else {
                ""
            },
            text.replace('\n', " "),
            clip(&only_inc),
            clip(&only_eng),
        ));
    }
    (t, details)
}

// ── The instrument's own canary ─────────────────────────────────────────────

/// A fixture with one exact match, one DELIBERATE divergence, and one
/// LIMIT-boundary tie. If the comparator does not classify all three
/// exactly, nothing it says about the real export can be trusted.
const CANARY: &str = r#"{
  "captured_at": "2026-01-02T03:04:05Z",
  "nodes": [
    {"id": "c1", "labels": ["C1"], "props": {"a": 1, "g": 5}},
    {"id": "c2", "labels": ["C1"], "props": {"a": 2, "g": 5}}
  ],
  "rels": [],
  "results": [
    {"file": "canary/match", "line": 1,
     "text": "MATCH (n:C1) RETURN n.a AS a ORDER BY a",
     "cols": ["a"], "rows": [[1],[2]], "truncated": false},
    {"file": "canary/diverge", "line": 2,
     "text": "MATCH (n:C1) RETURN count(*) AS c",
     "cols": ["c"], "rows": [[999]], "truncated": false},
    {"file": "canary/tie", "line": 3,
     "text": "MATCH (n:C1) RETURN n.a AS a, n.g AS g ORDER BY g DESC LIMIT 1",
     "cols": ["a", "g"], "rows": [[2, 5]], "truncated": false}
  ]
}"#;

fn self_canary() {
    let Ok(Value::Map(doc)) = json::from_json(CANARY) else {
        panic!("canary fixture failed to parse");
    };
    let (t, _) = run_export(&doc);
    assert_eq!(
        t.identical, 1,
        "canary: the matching statement must read identical"
    );
    assert_eq!(
        t.diverged.len(),
        1,
        "canary: the wrong count must be DETECTED"
    );
    assert_eq!(
        t.tie_boundary, 1,
        "canary: the LIMIT-boundary tie must classify as tie"
    );
    eprintln!("[decoded] self-canary passed (match=1, diverge=1, tie=1)");
}

fn main() {
    self_canary();
    let export_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "measurements/shadow-export.json".into());
    let out_path = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "measurements/decoded-values-report.json".into());
    let raw = std::fs::read_to_string(&export_path).expect("read the export");
    let Ok(Value::Map(doc)) = json::from_json(&raw) else {
        panic!("export is not a JSON object");
    };
    drop(raw);

    let (t, details) = run_export(&doc);
    eprintln!(
        "[decoded] world loaded: {} nodes, {} rels, {} unloadable props",
        t.world_nodes, t.world_rels, t.unloadable
    );

    let compared = t.identical
        + t.empty_on_both
        + t.count_match_unordered_limit
        + t.count_match_id_space
        + t.tie_boundary
        + t.diverged.len();
    let matched = compared - t.diverged.len();
    println!("== decoded-values comparison ==");
    println!(
        "world: {} nodes, {} rels (unloadable props: {})",
        t.world_nodes, t.world_rels, t.unloadable
    );
    println!("compared:                 {compared}");
    println!("  identical (with rows):  {}", t.identical);
    println!("  identical (both empty): {}", t.empty_on_both);
    println!(
        "  count-match unordered:  {}",
        t.count_match_unordered_limit
    );
    println!("  count-match id-space:   {}", t.count_match_id_space);
    println!("  tie at LIMIT boundary:  {}", t.tie_boundary);
    println!("  DIVERGED:               {}", t.diverged.len());
    println!("not comparable:");
    println!("  engram parse errors:    {}", t.engram_parse_error.len());
    println!("  engram run errors:      {}", t.engram_run_error.len());
    println!("  neo4j errors:           {}", t.neo4j_error);
    println!("  truncated (skipped):    {}", t.skipped_truncated);
    for d in &details {
        println!("---\n{d}");
    }
    for e in t.engram_parse_error.iter().chain(t.engram_run_error.iter()) {
        println!("!! {e}");
    }

    // The report, in the shape the measurements directory already uses.
    let mut rep = BTreeMap::new();
    rep.insert("world_nodes".to_string(), Value::Int(t.world_nodes as i64));
    rep.insert("world_rels".to_string(), Value::Int(t.world_rels as i64));
    rep.insert(
        "unloadable_props".to_string(),
        Value::Int(t.unloadable as i64),
    );
    rep.insert("compared".to_string(), Value::Int(compared as i64));
    rep.insert("identical_rows".to_string(), Value::Int(t.identical as i64));
    rep.insert(
        "identical_empty".to_string(),
        Value::Int(t.empty_on_both as i64),
    );
    rep.insert(
        "count_match_unordered_limit".to_string(),
        Value::Int(t.count_match_unordered_limit as i64),
    );
    rep.insert(
        "count_match_id_space".to_string(),
        Value::Int(t.count_match_id_space as i64),
    );
    rep.insert(
        "tie_boundary".to_string(),
        Value::Int(t.tie_boundary as i64),
    );
    rep.insert(
        "diverged".to_string(),
        Value::List(t.diverged.iter().cloned().map(Value::Str).collect()),
    );
    rep.insert(
        "engram_parse_errors".to_string(),
        Value::List(
            t.engram_parse_error
                .iter()
                .cloned()
                .map(Value::Str)
                .collect(),
        ),
    );
    rep.insert(
        "engram_run_errors".to_string(),
        Value::List(t.engram_run_error.iter().cloned().map(Value::Str).collect()),
    );
    rep.insert("neo4j_errors".to_string(), Value::Int(t.neo4j_error as i64));
    rep.insert(
        "skipped_truncated".to_string(),
        Value::Int(t.skipped_truncated as i64),
    );
    rep.insert(
        "matched_fraction".to_string(),
        Value::Float(if compared > 0 {
            matched as f64 / compared as f64
        } else {
            0.0
        }),
    );
    rep.insert(
        "divergence_details".to_string(),
        Value::List(details.iter().cloned().map(Value::Str).collect()),
    );
    std::fs::write(&out_path, json::to_json(&Value::Map(rep))).expect("write report");
    eprintln!("[decoded] report written to {out_path}");
}
