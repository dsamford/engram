//! An LSQB statement exists ONCE, and it is still the bytes that were measured.
//!
//! # What this replaced, and why it had to be replaced
//!
//! Until the convergence cutover this file held the opposite gate: `lsqb.rs`
//! carried its own `QUERIES` table, and the test re-read that source, resolved
//! its string literals the way `rustc` does, and refused if the two copies ever
//! disagreed. `src/bin/lsqb.rs` now reads `catalogue/statements.json` directly,
//! so that comparison became the catalogue against itself. A gate that cannot
//! fail is not a gate, and one that cannot fail while still LOOKING like a gate
//! is worse than none — it is the thing a reviewer points at instead of
//! looking.
//!
//! Two claims survive the cutover, and both can still fail:
//!
//! 1. **The duplication stays closed.** No LSQB statement appears as a literal
//!    anywhere in `src/bin/lsqb.rs`. The search runs over a source that has had
//!    `rustc`'s two literal-joining rules applied — a backslash at end-of-line
//!    strips the newline AND the next line's leading whitespace; adjacent
//!    literals concatenate — because a query pasted back in would arrive
//!    wrapped in exactly that form, and a naive substring search would sail
//!    straight past it. That is not hypothetical: the wrapped form is how all
//!    nine were written here for the whole life of the file.
//!
//! 2. **The bytes have not moved.** `tests/golden/lsqb-cypher-statements.txt`
//!    records the nine queries and the two census statements exactly as the
//!    `lsqb` binary sent them BEFORE the cutover — dumped from the binary's own
//!    table, not transcribed. Every LSQB number under `measurements/` was
//!    produced by these bytes, and to a plan cache a statement differing by one
//!    space is a different statement. An edit to the catalogue's Cypher must
//!    therefore break this test and be re-recorded deliberately, rather than
//!    landing quietly and silently re-baselining a year of numbers.
//!
//! The golden is not a second copy of the catalogue. It is the record of what
//! was measured; when it and the catalogue disagree, that disagreement is a
//! change somebody has to own.

use std::path::Path;

use engram_bench::catalogue::{Catalogue, Dialect};

/// The statements as the pre-cutover binary sent them, `key|statement` per
/// line. Fields are pipe-separated to match `golden/stress-op-sequence.txt`.
const GOLDEN: &str = include_str!("golden/lsqb-cypher-statements.txt");

/// Apply `rustc`'s two literal-joining rules to a whole source file, so a
/// re-pasted query is found in the form somebody would actually paste it.
///
/// This is a DETECTOR, not a parser, and the direction of its error is the
/// point: both rules only ever REMOVE characters, so the joined text is a
/// superset of whatever any literal in the file spells. It can raise a false
/// alarm — loud, and answerable — but it cannot quietly miss a real copy.
fn join_literals(src: &str) -> String {
    let cs: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0usize;
    while i < cs.len() {
        match cs[i] {
            // A backslash at end-of-line: the newline and the next line's
            // indent are not part of the string.
            '\\' if i + 1 < cs.len() && (cs[i + 1] == '\n' || cs[i + 1] == '\r') => {
                i += 1;
                while i < cs.len() && matches!(cs[i], '\n' | '\r' | ' ' | '\t') {
                    i += 1;
                }
            }
            // `"` whitespace `"` — two adjacent literals are one literal. A
            // quote followed by anything else (a comma, a paren) is a real
            // quote and survives.
            '"' => {
                let mut j = i + 1;
                while j < cs.len() && matches!(cs[j], ' ' | '\t' | '\n' | '\r') {
                    j += 1;
                }
                if j < cs.len() && cs[j] == '"' {
                    i = j + 1;
                } else {
                    out.push('"');
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Every statement the Cypher lane sends, as `key|statement`, in golden order:
/// the two census statements, then the queries in catalogue order.
fn catalogue_statements(cat: &Catalogue) -> Vec<String> {
    let mut out = Vec::new();
    let (nodes, persons) = cat.lsqb_census(Dialect::Cypher).expect("lsqb census");
    out.push(format!("census.nodes|{nodes}"));
    out.push(format!("census.persons|{persons}"));
    for name in cat.lsqb_names().expect("lsqb names") {
        let entry = cat
            .lsqb(&name, Dialect::Cypher)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        out.push(format!("{name}|{}", entry.text));
    }
    out
}

#[test]
fn no_lsqb_statement_is_restated_in_the_binary() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bin/lsqb.rs");
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let joined = join_literals(&src);
    let cat = Catalogue::load().expect("catalogue");

    let mut checked = 0usize;
    for name in cat.lsqb_names().expect("lsqb names") {
        let entry = cat
            .lsqb(&name, Dialect::Cypher)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        if !entry.status.runnable() {
            continue;
        }
        assert!(
            !joined.contains(&entry.text),
            "\n{name} is spelled out in src/bin/lsqb.rs as well as in the catalogue.\n\
             Two copies of a query is how two engines end up asked two different\n\
             questions — and the divergence that matters is the one that leaves the\n\
             COUNT identical and only changes the work, because then nothing fails.\n\
             Delete the copy and read `catalogue::Catalogue::lsqb`.\n  {}",
            entry.text
        );
        checked += 1;
    }
    let (nodes, persons) = cat.lsqb_census(Dialect::Cypher).expect("lsqb census");
    for stmt in [&nodes, &persons] {
        assert!(
            !joined.contains(stmt),
            "\na census statement is spelled out in src/bin/lsqb.rs as well as in \
             the catalogue:\n  {stmt}"
        );
        checked += 1;
    }
    // A loop that checked nothing passes every assertion it never made.
    assert_eq!(
        checked, 11,
        "nine queries and two census statements must have been searched for"
    );
}

#[test]
fn the_statements_are_the_bytes_the_baseline_was_measured_with() {
    let cat = Catalogue::load().expect("catalogue");
    let got = catalogue_statements(&cat);
    let want: Vec<&str> = GOLDEN.lines().filter(|l| !l.is_empty()).collect();

    // Line by line, so a failure names the query that moved rather than
    // dumping eleven statements and leaving the reader to spot the space.
    for (i, w) in want.iter().enumerate() {
        let g = got
            .get(i)
            .unwrap_or_else(|| panic!("the catalogue is missing the golden's line {i}: {w}"));
        let (key, want_text) = w.split_once('|').expect("malformed golden line");
        let (got_key, got_text) = g.split_once('|').expect("malformed rendering");
        assert_eq!(
            got_key, key,
            "golden line {i}: the statements are out of order"
        );
        assert_eq!(
            got_text, want_text,
            "\n{key}: the catalogue's Cypher is NOT the text this project's LSQB \
             numbers were measured with.\n  measured: {want_text}\n  catalogue: {got_text}\n\
             If the change is deliberate, re-record the golden and say in the commit \
             that every prior LSQB number was taken with the old text."
        );
    }
    assert_eq!(
        got.len(),
        want.len(),
        "the catalogue and the golden disagree on how many statements exist"
    );
    assert_eq!(got.len(), 11, "nine queries and two census statements");
    // The golden's format truncates silently if a statement ever grows a
    // newline or a pipe, which would make a moved byte invisible here.
    for line in &got {
        let (_, text) = line.split_once('|').expect("malformed rendering");
        assert!(
            !text.contains('\n') && !text.contains('|'),
            "a statement carries a newline or a pipe; the golden's line format \
             cannot record it: {text:?}"
        );
    }
}

#[test]
fn the_literal_joiner_sees_a_query_pasted_back_in_wrapped() {
    // THE CANARY. Every entry in the deleted table was written in exactly this
    // shape. If the joiner did not apply the continuation rule, a query pasted
    // back in the obvious way would be invisible to the check above, which
    // would then pass while the duplication was open — the one failure this
    // whole file exists to make impossible.
    let pasted = "        adapted: Some(\n            \"MATCH (a)\\\n             \
                  -[:KNOWS]-(b) RETURN count(*) AS count\",\n        ),";
    assert!(
        join_literals(pasted).contains("MATCH (a)-[:KNOWS]-(b) RETURN count(*) AS count"),
        "the continuation rule is not being applied"
    );
    // Adjacent literals concatenate.
    assert!(join_literals("\"AB\" \"CD\"").contains("ABCD"));
    assert!(join_literals("\"AB\"\n             \"CD\"").contains("ABCD"));
    // But a comma between them is a separator, not a join: the joiner must not
    // invent text that no literal spells, or it would cry wolf.
    assert!(!join_literals("f(\"AB\", \"CD\")").contains("ABCD"));
    // And ordinary source passes through untouched.
    assert_eq!(join_literals("let x = \"a\";"), "let x = \"a\";");
}
