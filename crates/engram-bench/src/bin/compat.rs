//! The corpus compatibility sweep — the M7 kill criterion's front-end half.
//!
//! Reads a corpus JSONL (mechanically extracted from an application
//! codebase: every statement-shaped literal, `${…}` interpolations
//! replaced with `$dyn` and MARKED), runs the parser over every statement,
//! and reports pass rate with failures BUCKETED BY REASON — a bare
//! percentage would hide which features are missing, which is the only
//! actionable content. Dynamic statements are reported separately: their
//! failures are inconclusive about the grammar (a `$dyn` standing where a
//! label was spliced is not a parser gap).
//!
//! What this deliberately is NOT yet: the identical-decoded-values run —
//! that needs the incumbent's data beside the engine, and pretending parse
//! rate is that number is the exact claim the plan rejects.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;

use engram_cypher::{Value, json, parse_any};

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "measurements/corpus.jsonl".to_string());
    let raw = std::fs::read_to_string(&path).expect("read the corpus");
    let mut total = 0usize;
    let mut sites_total = 0i64;
    let (mut ok_static, mut ok_dynamic, mut fail_dynamic) = (0usize, 0usize, 0usize);
    let mut fail_static: Vec<(String, String, String)> = Vec::new(); // (reason, file, text)

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(Value::Map(row)) = json::from_json(line) else {
            panic!("bad corpus row: {line}");
        };
        let text = match row.get("text") {
            Some(Value::Str(s)) => s.clone(),
            _ => panic!("row without text"),
        };
        let dynamic = matches!(row.get("dynamic"), Some(Value::Bool(true)));
        let file = match row.get("file") {
            Some(Value::Str(s)) => s.clone(),
            _ => String::new(),
        };
        if let Some(Value::Int(s)) = row.get("sites") {
            sites_total += s;
        }
        total += 1;
        match parse_any(&text) {
            Ok(_) => {
                if dynamic {
                    ok_dynamic += 1;
                } else {
                    ok_static += 1;
                }
            }
            Err(e) => {
                if dynamic {
                    fail_dynamic += 1;
                } else {
                    // Bucket by the EXPECTATION, which names the missing
                    // grammar; strip positions so buckets aggregate.
                    let msg = e.to_string();
                    let reason = match msg.find(" at byte ") {
                        Some(i) => msg[..i].to_string(),
                        None => msg,
                    };
                    fail_static.push((reason, file.clone(), text.clone()));
                }
            }
        }
    }

    let static_total = ok_static + fail_static.len();
    let mut buckets: BTreeMap<String, (usize, String, String)> = BTreeMap::new();
    for (reason, file, text) in &fail_static {
        let e = buckets
            .entry(reason.clone())
            .or_insert((0, file.clone(), text.clone()));
        e.0 += 1;
    }
    let mut ranked: Vec<(&String, &(usize, String, String))> = buckets.iter().collect();
    ranked.sort_by_key(|(_, (count, _, _))| std::cmp::Reverse(*count));

    println!("corpus: {total} distinct statements ({sites_total} call sites)");
    println!(
        "static  : {ok_static}/{static_total} parse ({:.1}%)",
        100.0 * ok_static as f64 / static_total.max(1) as f64
    );
    println!(
        "dynamic : {ok_dynamic}/{} parse with $dyn substitution (failures inconclusive)",
        ok_dynamic + fail_dynamic
    );
    println!("\nstatic failures by reason:");
    for (reason, (count, file, text)) in ranked.iter().take(25) {
        let one_line: String = text
            .chars()
            .take(110)
            .map(|c| if c == '\n' { ' ' } else { c })
            .collect();
        println!("  {count:>4}  {reason}\n        e.g. {file}: {one_line}");
    }

    // The committed report.
    let mut doc = BTreeMap::new();
    doc.insert("total_distinct".into(), Value::Int(total as i64));
    doc.insert("call_sites".into(), Value::Int(sites_total));
    doc.insert("static_parsed".into(), Value::Int(ok_static as i64));
    doc.insert("static_failed".into(), Value::Int(fail_static.len() as i64));
    doc.insert("dynamic_parsed".into(), Value::Int(ok_dynamic as i64));
    doc.insert("dynamic_failed".into(), Value::Int(fail_dynamic as i64));
    doc.insert(
        "static_parse_rate".into(),
        Value::Float(ok_static as f64 / static_total.max(1) as f64),
    );
    doc.insert(
        "failure_buckets".into(),
        Value::List(
            ranked
                .iter()
                .map(|(reason, (count, file, text))| {
                    let mut m = BTreeMap::new();
                    m.insert("reason".to_string(), Value::Str((*reason).clone()));
                    m.insert("count".to_string(), Value::Int(*count as i64));
                    m.insert("example_file".to_string(), Value::Str(file.clone()));
                    m.insert(
                        "example".to_string(),
                        Value::Str(text.chars().take(300).collect()),
                    );
                    Value::Map(m)
                })
                .collect(),
        ),
    );
    let out = json::to_json(&Value::Map(doc));
    std::fs::write("measurements/compat-report.json", &out).expect("write report");
    println!(
        "\nreport written to measurements/compat-report.json ({} bytes)",
        out.len()
    );
}
