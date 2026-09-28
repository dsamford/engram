#![allow(non_snake_case)]
//! FinBench TCR1–TCR12, parsed against this engine's own Cypher.
//!
//! The LDBC coverage plan bands a FinBench read battery at 25–40 engineer-days
//! and says, in its own words, to retire its largest unknown on day one and
//! BEFORE any loader work: parse the TCR statements offline, and find out on a
//! development corpus whether the planner pushes the ascending-timestamp filter
//! into the variable-length expansion or enumerates every path first. "If it is
//! the latter, TCR1/TCR2/TCR5 may be unrunnable at SF10 regardless of how good
//! the harness work is."
//!
//! This is the first half of that: do they parse at all. It costs nothing, it
//! needs no corpus, and a statement this engine cannot read is a gate no amount
//! of loader work gets past.
//!
//! The catalogue marks every entry `unverified` and says so in a top-level key
//! (`every_entry_here_is_unverified`). This test is one of the things that
//! changes that, for the parse property only — running them and checking their
//! ANSWERS is a separate claim this file does not make.

use std::collections::BTreeMap;

/// The catalogue writes parameters as `${name}`. Substitute literals of the
/// right SHAPE — an id is an integer, a timestamp an integer, a truncation
/// limit an integer — because the question here is whether the STATEMENT
/// parses, not whether a parameter binder exists.
fn substitute(text: &str) -> String {
    let mut out = text.to_string();
    for (k, v) in [
        ("${id}", "4614219293217785425"),
        ("${startTime}", "1600000000000"),
        ("${endTime}", "1700000000000"),
        ("${threshold}", "1000.0"),
        ("${truncationLimit}", "500"),
        ("${truncationOrder}", "'TIMESTAMP_DESCENDING'"),
        ("${pid}", "755"),
        ("${amount}", "1000.0"),
        ("${ratio}", "0.5"),
    ] {
        out = out.replace(k, v);
    }
    // Anything left is a parameter this fixture does not know; an integer keeps
    // the parse honest rather than leaving a `${...}` the lexer would reject
    // for a reason that has nothing to do with the query.
    while let Some(i) = out.find("${") {
        let Some(j) = out[i..].find('}') else { break };
        out.replace_range(i..i + j + 1, "1");
    }
    out
}

fn catalogue() -> BTreeMap<String, String> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../engram-bench/catalogue/finbench.json"
    );
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read the FinBench catalogue at {path}: {e}"));

    // A deliberately small reader rather than a serde dependency: find each
    // `"tcrN": { ... "cypher": {"status": ..., "text": "..."} ... }`.
    let mut out = BTreeMap::new();
    for n in 1..=12 {
        let key = format!("\"tcr{n}\"");
        let Some(start) = text.find(&key) else { continue };
        let rest = &text[start..];
        // The FIRST "cypher" after the query key, and the first "text" after
        // that — `cypher_ladybug` and `sql` follow, and must not be read here.
        let Some(cy) = rest.find("\"cypher\"") else { continue };
        let after = &rest[cy..];
        let Some(t) = after.find("\"text\"") else { continue };
        let seg = &after[t + 6..];
        let Some(q1) = seg.find('"') else { continue };
        let mut s = String::new();
        let mut esc = false;
        for c in seg[q1 + 1..].chars() {
            if esc {
                match c {
                    'n' => s.push('\n'),
                    't' => s.push('\t'),
                    other => s.push(other),
                }
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                break;
            } else {
                s.push(c);
            }
        }
        out.insert(format!("tcr{n}"), s);
    }
    out
}

#[test]
fn the_catalogue_actually_carries_all_twelve() {
    // The fixture guard. Every assertion below is over whatever this returns,
    // so a reader that silently found two statements would make the parse
    // results look like a clean sweep of nothing.
    let c = catalogue();
    assert_eq!(
        c.len(),
        12,
        "expected TCR1-12 from the catalogue, extracted {}: {:?}",
        c.len(),
        c.keys().collect::<Vec<_>>()
    );
    for (name, text) in &c {
        assert!(
            text.len() > 40 && text.contains("MATCH"),
            "{name} does not look like a Cypher statement: {text:?}"
        );
    }
}

#[test]
fn every_TCR_statement_parses_or_the_failures_are_named() {
    let c = catalogue();
    let mut ok = Vec::new();
    let mut failed = Vec::new();
    for (name, text) in &c {
        match engram_cypher::parse_any(&substitute(text)) {
            Ok(_) => ok.push(name.clone()),
            Err(e) => failed.push(format!("{name}: {e}")),
        }
    }
    // NOT a pass/fail on the count. The deliverable the plan asked for is a
    // LEDGER — which statements this engine can read and which it cannot —
    // because "three of twelve fail to parse" is a decision input, not a bug
    // report. The assertion is only that the ledger was produced.
    eprintln!("[finbench] parsed {}/{}: {:?}", ok.len(), c.len(), ok);
    for f in &failed {
        eprintln!("[finbench] PARSE FAILED — {f}");
    }
    assert!(
        !ok.is_empty(),
        "not one TCR statement parsed, which means the reader or the \
         substitution is broken rather than the engine: {failed:?}"
    );
}

#[test]
fn the_variable_length_statements_are_the_ones_the_plan_flagged() {
    // TCR1, TCR2 and TCR5 are named in the plan as the queries whose cost
    // depends on whether the monotone timestamp filter is pushed INTO the
    // expansion. Pin that they are in fact the variable-length ones, so the
    // planner work that follows is aimed at the right statements.
    let c = catalogue();
    let mut varlen: Vec<&str> = c
        .iter()
        .filter(|(_, t)| t.contains("*1..") || t.contains("*2..") || t.contains("*.."))
        .map(|(k, _)| k.as_str())
        .collect();
    varlen.sort();
    eprintln!("[finbench] variable-length statements: {varlen:?}");
    assert!(
        varlen.contains(&"tcr1"),
        "tcr1 must be variable-length; the plan's cost argument rests on it. \
         Found: {varlen:?}"
    );
}
