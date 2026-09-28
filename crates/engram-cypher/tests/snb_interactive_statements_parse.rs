#![allow(non_snake_case)]
//! SNB Interactive IC1–IC14 and IS1–IS7, parsed against this engine's Cypher.
//!
//! The coverage plan records this family as "**draft** (11 of 14) —
//! `snb-statements.json`, hardcoded parameters, never executed; IC7, IC10, IC14
//! absent". The catalogue has moved since: it carries TWENTY-ONE statements,
//! the full IC1–IC14 plus the seven short reads, with IC7, IC10 and IC14 all
//! present. So the plan's headline is out of date in the family's favour, and
//! the interesting question is no longer "how many are written" but "how many
//! can this engine read".
//!
//! This is the same gate that paid for itself on FinBench, where it took a
//! family from "not covered" to a ledger naming exactly what blocks it, for the
//! cost of a fixture. It needs no corpus, no server and no bench node.
//!
//! It is a PARSE claim and nothing more. Whether a statement that parses also
//! ANSWERS — and answers what LDBC's published output says — is a separate
//! claim that needs the official corpus and substitution parameters, neither of
//! which this repository has yet.


/// The catalogue writes parameters as `${name}`. Substitute literals of the
/// right SHAPE, because the question is whether the STATEMENT parses.
fn substitute(text: &str) -> String {
    let mut out = text.to_string();
    // Strings first: these appear inside quotes in the catalogue, so the
    // replacement must not add its own.
    for (k, v) in [
        ("${firstName}", "John"),
        ("${lastName}", "Smith"),
        ("${countryXName}", "India"),
        ("${countryYName}", "China"),
        ("${countryName}", "India"),
        ("${tagName}", "Che_Guevara"),
        ("${tagClassName}", "MusicalArtist"),
        ("${languages}", "en"),
    ] {
        out = out.replace(k, v);
    }
    for (k, v) in [
        ("${personId}", "933"),
        ("${personId2}", "1129"),
        ("${friendId}", "1129"),
        ("${messageId}", "1030792151054"),
        ("${commentId}", "1030792151054"),
        ("${postId}", "1030792151054"),
        ("${forumId}", "1236"),
        ("${maxDate}", "1350864000000"),
        ("${minDate}", "1287230400000"),
        ("${startDate}", "1287230400000"),
        ("${endDate}", "1350864000000"),
        ("${durationDays}", "30"),
        ("${month}", "10"),
        ("${limit}", "20"),
        ("${minPathDistance}", "3"),
        ("${maxPathDistance}", "5"),
        ("${classYear}", "2010"),
        ("${workFromYear}", "2010"),
    ] {
        out = out.replace(k, v);
    }
    while let Some(i) = out.find("${") {
        let Some(j) = out[i..].find('}') else { break };
        out.replace_range(i..i + j + 1, "1");
    }
    out
}

/// `(name, cypher text, declared status)`.
fn catalogue() -> Vec<(String, String, String)> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../engram-bench/catalogue/snb-interactive.json"
    );
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read the SNB Interactive catalogue at {path}: {e}"));

    let mut names: Vec<String> = Vec::new();
    for n in 1..=14 {
        names.push(format!("IC{n}"));
    }
    for n in 1..=7 {
        names.push(format!("IS{n}"));
    }

    let mut out = Vec::new();
    for name in names {
        let key = format!("\"{name}\"");
        let Some(start) = text.find(&key) else { continue };
        let rest = &text[start..];
        let Some(cy) = rest.find("\"cypher\"") else { continue };
        let after = &rest[cy..];
        // The declared status sits beside the text in the same object.
        let status = after
            .find("\"status\"")
            .and_then(|s| {
                let seg = &after[s + 8..];
                let q1 = seg.find('"')?;
                let q2 = seg[q1 + 1..].find('"')?;
                Some(seg[q1 + 1..q1 + 1 + q2].to_string())
            })
            .unwrap_or_default();
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
        out.push((name, s, status));
    }
    out
}

#[test]
fn the_catalogue_carries_the_full_interactive_workload() {
    // Fixture guard: every ledger below is over whatever this returns, so a
    // reader that silently found three statements would report a clean sweep
    // of nothing. It also pins the plan's correction — IC7, IC10 and IC14 are
    // recorded there as ABSENT and are in fact present.
    let c = catalogue();
    assert_eq!(
        c.len(),
        21,
        "expected all 21 statements to carry cypher text -- IC1-14 and IS1-7. \
         IC14 carried none until the published v1 text was added; extracted \
         {}: {:?}",
        c.len(),
        c.iter().map(|(n, _, _)| n).collect::<Vec<_>>()
    );
    // The coverage plan records IC7, IC10 and IC14 as ABSENT. Two of the three
    // are present and written; IC14 is the one that genuinely is not, and the
    // catalogue declares that itself rather than leaving a gap to discover.
    for name in ["IC7", "IC10", "IC14"] {
        assert!(
            c.iter().any(|(n, t, _)| n == name && t.contains("MATCH")),
            "{name} is recorded as absent in the coverage plan but is present \
             and written here"
        );
    }
    // IC14 carries the v1 text specifically. `allShortestPaths` and the 1.0 /
    // 0.5 reply weights are what distinguish it from v2, which is a different
    // query (one cheapest path, integer weights) and is NOT a substitute.
    let ic14 = c
        .iter()
        .find(|(n, _, _)| n == "IC14")
        .map(|(_, t, _)| t.clone())
        .unwrap_or_default();
    assert!(
        ic14.contains("allShortestPaths") && ic14.contains("0.5"),
        "IC14 must carry the V1 text -- all shortest paths, float reply \
         weights -- not v2's single cheapest path: {ic14:?}"
    );
}

#[test]
fn every_interactive_statement_parses_or_the_failures_are_named() {
    let c = catalogue();
    let mut ok = Vec::new();
    let mut failed = Vec::new();
    let mut declared_unsupported = Vec::new();
    for (name, text, status) in &c {
        if status == "unsupported" {
            declared_unsupported.push(name.clone());
        }
        match engram_cypher::parse_any(&substitute(text)) {
            Ok(_) => ok.push(name.clone()),
            Err(e) => failed.push((name.clone(), format!("{e}"))),
        }
    }
    eprintln!("[snb-ic] parsed {}/{}", ok.len(), c.len());
    eprintln!("[snb-ic] OK: {ok:?}");
    for (n, e) in &failed {
        eprintln!("[snb-ic] PARSE FAILED — {n}: {e}");
    }
    if !declared_unsupported.is_empty() {
        eprintln!("[snb-ic] declared `unsupported` in the catalogue: {declared_unsupported:?}");
    }
    // A LEDGER, not a pass mark: which statements this engine can read is a
    // decision input. The assertion is that the ledger was produced over real
    // statements rather than over an empty read.
    assert!(
        !ok.is_empty(),
        "not one statement parsed, so the reader or the substitution is broken \
         rather than the engine: {failed:?}"
    );
}

#[test]
fn the_short_reads_all_parse() {
    // IS1-7 are single-hop lookups — no shortestPath, no variable length, no
    // aggregation over a large frontier. If any of THESE fails to parse it is
    // a plain gap in the dialect rather than an advanced-feature question, and
    // it is worth failing the suite over.
    let c = catalogue();
    let mut failed = Vec::new();
    for (name, text, _) in c.iter().filter(|(n, _, _)| n.starts_with("IS")) {
        if let Err(e) = engram_cypher::parse_any(&substitute(text)) {
            failed.push(format!("{name}: {e}"));
        }
    }
    assert!(
        failed.is_empty(),
        "every SNB Interactive SHORT read must parse; these did not: {failed:?}"
    );
}
