#![allow(non_snake_case)]
//! SNB BI BI1–BI20, parsed against this engine's Cypher.
//!
//! The coverage plan records this family as "not covered", and I reported the
//! catalogue as holding ZERO queries. Both readings were wrong, and the second
//! was mine: the entries live at `snb_bi.queries`, not at a top-level
//! `queries`, and there are TWENTY of them — three dialects each (Cypher, SQL,
//! Ladybug), with projection column schemas and per-variant parameter files.
//! Acting on the wrong reading I overwrote the file with thinner drafts and
//! restored it from the Lore clone. Recorded because the lesson is cheap here
//! and expensive anywhere else: confirm a file's SHAPE before concluding it is
//! empty, and never write over what you have not read.
//!
//! What the family genuinely lacks is the gate every other one now has. This is
//! it: BI is the largest analytic workload in the set — 20 templates that each
//! touch a large fraction of the database — and whether this engine can READ
//! them is the first question, ahead of any corpus or timing work.
//!
//! A PARSE claim and nothing else. Whether a statement that parses also answers
//! what LDBC's `output-sf10-validation-umbra` says needs that corpus and the
//! substitution parameters, neither of which this repository has.

use std::collections::BTreeMap;

/// The catalogue carries each query with its published header comment and
/// `:params` block still attached:
///
/// ```text
/// // Q1. Posting summary
/// /*
/// :params { datetime: datetime('2011-12-01T00:00:00.000') }
/// */
/// MATCH (message:Message) ...
/// ```
///
/// The `//` line and the block comment are Cypher comments and a parser may
/// accept them, but the `:params` line is a Neo4j BROWSER directive, not
/// Cypher. Stripping to the end of the block comment is what LDBC's own
/// runners do, and leaving it in would test the comment handling rather than
/// the query.
fn strip_header(text: &str) -> &str {
    match text.find("*/") {
        Some(i) => text[i + 2..].trim_start(),
        None => text.trim_start(),
    }
}

/// Substitute literals of the right SHAPE. Types matter here more than in the
/// other families: BI's parameters include DATETIME values, and a bare integer
/// where a `datetime()` is expected parses fine but means something else.
fn substitute(text: &str) -> String {
    let mut out = text.to_string();
    for (k, v) in [
        ("$datetime", "datetime('2011-12-01T00:00:00.000')"),
        ("$dateA", "datetime('2010-01-01')"),
        ("$dateB", "datetime('2012-01-01')"),
        ("$date", "datetime('2010-01-29')"),
        ("$startDate", "datetime('2010-01-01')"),
        ("$endDate", "datetime('2012-01-01')"),
        ("$tagClass", "'MusicalArtist'"),
        ("$tagA", "'Arnold_Schwarzenegger'"),
        ("$tagB", "'Che_Guevara'"),
        ("$tag", "'Arnold_Schwarzenegger'"),
        ("$country1", "'China'"),
        ("$country2", "'India'"),
        ("$country", "'China'"),
        ("$languages", "['en', 'de']"),
        ("$personId", "30786325588624"),
        ("$lengthThreshold", "20"),
        ("$maxKnowsLimit", "4"),
        ("$maxPathDistance", "5"),
        ("$minPathDistance", "3"),
        ("$delta", "4"),
    ] {
        out = out.replace(k, v);
    }
    // Longest-first above, so `$tagClass` is replaced before `$tag` and
    // `$dateA` before `$date`. Anything left is a name this fixture does not
    // know; an integer keeps the parse honest rather than leaving a bare `$`
    // the lexer rejects for a reason unrelated to the query.
    while let Some(i) = out.find('$') {
        let rest = &out[i + 1..];
        let n = rest
            .char_indices()
            .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
            .count();
        if n == 0 {
            break;
        }
        out.replace_range(i..i + 1 + n, "1");
    }
    out
}

/// `(name, cypher text, declared status)` for every BI entry that HAS text.
fn catalogue() -> Vec<(String, String, String)> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../engram-bench/catalogue/snb-bi.json"
    );
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read the SNB BI catalogue at {path}: {e}"));

    let mut out = Vec::new();
    for n in 1..=20 {
        // The key must be the one that opens an OBJECT. `"bi4"` and `"bi6"`
        // also appear earlier in the file as PROSE, inside the
        // `precomputation` block ("umbra/dml/precomp/bi-4.sql builds ..."), and
        // a plain `find` matched those -- so two real queries were silently
        // skipped while the ledger still read "15/15 parsed", a clean sweep of
        // the wrong set. Requiring `": {` after the key is what distinguishes
        // an entry from a mention of one.
        let key = format!("\"bi{n}\"");
        let Some(start) = text
            .match_indices(&key)
            .map(|(i, _)| i)
            .find(|&i| {
                text[i + key.len()..]
                    .trim_start()
                    .strip_prefix(':')
                    .is_some_and(|r| r.trim_start().starts_with('{'))
            })
        else {
            continue;
        };
        let rest = &text[start..];
        // The next entry's key bounds this one, so a missing field is not read
        // out of the following query's body.
        let end = rest[key.len()..]
            .match_indices("\"bi")
            .map(|(x, _)| x + key.len())
            .find(|&x| rest[x + 3..].starts_with(|c: char| c.is_ascii_digit()))
            .unwrap_or(rest.len());
        let seg = &rest[..end];
        let Some(cy) = seg.find("\"cypher\"") else {
            continue;
        };
        let after = &seg[cy..];
        let status = after
            .find("\"status\"")
            .and_then(|s| {
                let g = &after[s + 8..];
                let q1 = g.find('"')?;
                let q2 = g[q1 + 1..].find('"')?;
                Some(g[q1 + 1..q1 + 1 + q2].to_string())
            })
            .unwrap_or_default();
        let Some(t) = after.find("\"text\"") else {
            continue;
        };
        let body = &after[t + 6..];
        let Some(q1) = body.find('"') else { continue };
        let mut s = String::new();
        let mut esc = false;
        for c in body[q1 + 1..].chars() {
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
        if !s.trim().is_empty() {
            out.push((format!("bi{n}"), s, status));
        }
    }
    out
}

#[test]
fn the_catalogue_carries_the_BI_workload() {
    // Fixture guard, and the one that would have caught my "it is empty"
    // reading: assert the entries are THERE and carry real query text before
    // any ledger is built over them.
    let c = catalogue();
    assert!(
        c.len() >= 17,
        "expected at least 17 BI entries with cypher text (bi15, bi19 and bi20 \
         declare themselves unsupported); extracted {}: {:?}",
        c.len(),
        c.iter().map(|(n, _, _)| n).collect::<Vec<_>>()
    );
    for (name, text, _) in &c {
        assert!(
            text.contains("MATCH") || text.contains("CALL"),
            "{name} does not look like a query: {text:?}"
        );
    }
}

#[test]
fn the_header_strip_removes_the_browser_directive_not_the_query() {
    // `:params { ... }` is a Neo4j Browser directive and is not Cypher. The
    // strip must remove it and keep everything after — a strip that ate the
    // first clause would make every parse below pass on a truncated query.
    let stripped = strip_header(
        "// Q1. Posting summary\n/*\n:params { datetime: datetime('2011') }\n*/\nMATCH (m:Message)\nRETURN count(m)",
    );
    assert!(
        stripped.starts_with("MATCH (m:Message)"),
        "the strip must land exactly on the query: {stripped:?}"
    );
    assert!(
        !stripped.contains(":params"),
        "the browser directive must be gone: {stripped:?}"
    );
}

#[test]
fn every_BI_statement_parses_or_the_failures_are_named() {
    let c = catalogue();
    let mut ok = Vec::new();
    let mut failed = Vec::new();
    let mut unsupported = Vec::new();
    for (name, text, status) in &c {
        if status == "unsupported" {
            unsupported.push(name.clone());
            continue;
        }
        let prepared = substitute(strip_header(text));
        match engram_cypher::parse_any(&prepared) {
            Ok(_) => ok.push(name.clone()),
            Err(e) => failed.push((name.clone(), format!("{e}"))),
        }
    }
    eprintln!("[snb-bi] parsed {}/{}", ok.len(), ok.len() + failed.len());
    eprintln!("[snb-bi] OK: {ok:?}");
    for (n, e) in &failed {
        eprintln!("[snb-bi] PARSE FAILED — {n}: {e}");
    }
    if !unsupported.is_empty() {
        eprintln!("[snb-bi] declared `unsupported`: {unsupported:?}");
    }
    // A LEDGER. Which of the twenty this engine can read is a decision input,
    // and BI is the family the plan says to decide about on a two-week gate.
    assert!(
        !ok.is_empty(),
        "not one BI statement parsed, so the reader or the substitution is \
         broken rather than the engine: {failed:?}"
    );
    let _ = BTreeMap::<u8, u8>::new();
}

/// Strip `//` line comments, respecting string literals.
///
/// Not a parser nicety — a REQUIREMENT of every runner that carries a query as
/// one field of one line. The BI battery is driven from a TSV, and the first
/// version built it with `body.split_whitespace().join(" ")`. Nine of the
/// seventeen queries carry a `//` comment (bi1's is mid-clause: `WITH
/// count(message) AS totalMessageCountInt // this should be a subquery`), so
/// collapsing the newlines put the rest of the query BEHIND the comment marker.
/// Eight queries then failed at execution with `found Eof` and "cannot conclude
/// with MATCH" — which reads exactly like a parser gap, and is not one. The
/// queries here parsed fine the whole time, because this file never collapsed
/// them.
fn strip_line_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut quote: Option<char> = None;
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if let Some(q) = quote {
            out.push(c);
            if c == '\\' {
                if let Some(n) = it.next() {
                    out.push(n);
                }
            } else if c == q {
                quote = None;
            }
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            out.push(c);
            continue;
        }
        if c == '/' && it.peek() == Some(&'/') {
            for n in it.by_ref() {
                if n == '\n' {
                    out.push('\n');
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// One line per query is how the battery ships them, so parsing them that way
/// is the claim that matters — parsing the pretty-printed form proves nothing
/// about the runner.
#[test]
fn every_BI_statement_parses_when_collapsed_onto_one_line() {
    let c = catalogue();
    let mut failed = Vec::new();
    for (name, text, status) in &c {
        if status == "unsupported" {
            continue;
        }
        let prepared = substitute(strip_header(text));
        let one_line = strip_line_comments(&prepared)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            !one_line.contains("//"),
            "{name}: a `//` survived the strip, so the collapse would still \
             swallow the tail: {one_line}"
        );
        if let Err(e) = engram_cypher::parse_any(&one_line) {
            failed.push((name.clone(), format!("{e}")));
        }
    }
    assert!(
        failed.is_empty(),
        "these parse multi-line but not on one line, which is the form the \
         battery runs: {failed:?}"
    );
}

#[test]
fn a_comment_marker_inside_a_string_literal_is_not_a_comment() {
    // The guard on the guard. A naive `find("//")` would cut this query in
    // half at a URL, and the damage would look identical to the bug above.
    let s = "MATCH (n) WHERE n.url = 'http://example.com/x' // trailing\nRETURN n";
    let out = strip_line_comments(s);
    assert!(
        out.contains("'http://example.com/x'"),
        "the literal must survive intact: {out}"
    );
    assert!(
        !out.contains("trailing"),
        "the real comment must still go: {out}"
    );
}
