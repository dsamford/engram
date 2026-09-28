#![allow(non_snake_case)]
//! `finbench2jsonl` against the file shapes the SHIPPED corpus actually has.
//!
//! The first version of this converter decided vertex-vs-edge by asking whether
//! the header began `fromId|toId`. That is true of exactly 4 of FinBench's 13
//! edge files — the self-joins. The other nine name their endpoints after the
//! entities (`MediumSignInAccount` is `mediumId|accountId`), so nine edge files
//! were read as vertex files, nine labels were invented from their names, and
//! the converter reported 18,988 nodes for a corpus holding 5,580. It exited 0
//! and wrote a well-formed corpus, which is the part worth guarding: nothing
//! downstream could tell.
//!
//! These fixtures are the real header shapes, small enough to assert exactly.

use std::io::Write;
use std::path::{Path, PathBuf};

fn write(dir: &Path, name: &str, body: &str) {
    std::fs::create_dir_all(dir).expect("mkdir");
    let mut f = std::fs::File::create(dir.join(name)).expect("create");
    write!(f, "{body}").expect("write");
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fb2j-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Returns (stdout+stderr, out dir).
fn convert(root: &Path) -> (String, PathBuf) {
    let out = root.join("jsonl");
    let exe = env!("CARGO_BIN_EXE_finbench2jsonl");
    let o = std::process::Command::new(exe)
        .arg(root)
        .arg(&out)
        .output()
        .expect("run finbench2jsonl");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    (text, out)
}

fn lines(p: &Path) -> Vec<String> {
    std::fs::read_to_string(p)
        .unwrap_or_else(|e| panic!("read {p:?}: {e}"))
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// The two endpoint-naming conventions the shipped corpus mixes.
fn corpus(root: &Path) {
    let snap = root.join("snapshot");
    write(
        &snap,
        "Account.csv",
        "accountId|createTime|isBlocked\n\
         4614219293217785425|2020-01-10 06:22:20.222|false\n\
         4619004367821865972|2020-01-27 17:55:09.206|false\n",
    );
    write(
        &snap,
        "Medium.csv",
        "mediumId|mediumType|isBlocked\n4398046511620|card|false\n",
    );
    // Convention A — the self-join, `fromId|toId`, WITH properties and a
    // deliberate multi-edge: two transfers between the same ordered pair.
    write(
        &snap,
        "AccountTransferAccount.csv",
        "fromId|toId|amount|createTime\n\
         4614219293217785425|4619004367821865972|3851891.85|2020-08-29 15:28:58.647\n\
         4614219293217785425|4619004367821865972|734861.37|2020-09-25 02:36:14.926\n",
    );
    // Convention B — endpoints named after the entities. This is the shape
    // that the first discriminator misread as a vertex file.
    write(
        &snap,
        "MediumSignInAccount.csv",
        "mediumId|accountId|createTime|location\n\
         4398046511620|4614219293217785425|2020-08-17 07:30:47.471|Vietnam\n",
    );
}

#[test]
fn only_the_entity_files_become_labels() {
    let d = tmp("labels");
    corpus(&d);
    let (log, out) = convert(&d);
    let nodes = lines(&out.join("nodes.jsonl"));
    assert_eq!(
        nodes.len(),
        3,
        "two Accounts and one Medium is three nodes; an edge file read as a \
         vertex file inflates this silently.\n{log}"
    );
    assert!(
        !log.contains("MediumSignInAccount:") || log.contains("Medium -signIn-> Account"),
        "MediumSignInAccount must be split as an EDGE, not adopted as a label.\n{log}"
    );
}

#[test]
fn both_endpoint_naming_conventions_are_read_as_edges() {
    let d = tmp("conventions");
    corpus(&d);
    let (log, out) = convert(&d);
    let rels = lines(&out.join("rels.jsonl"));
    assert_eq!(
        rels.len(),
        3,
        "two TRANSFERs and one SIGN_IN is three relationships.\n{log}"
    );
    // THE TYPE IS THE VERB by default, because that is what the catalogue's
    // Cypher names — `[transfer:transfer*1..3]`, `[signIn:signIn]` — and the
    // queries are the contract. LDBC spells it BOTH ways in its own tree
    // (neo4j/ uses the verb, galaxybase-cypher/ uses the file name), and this
    // converter previously emitted the file name, which meant every TCR query
    // naming a relationship type matched NOTHING. Measured 2026-09-15 on Neo4j
    // at SF0.01: twelve of twelve executed without error and returned 0 rows,
    // while PostgreSQL — whose schema uses the verb names — returned rows.
    assert!(
        rels.iter().any(|r| r.contains("\"t\":\"signIn\"")),
        "the default TYPE is the verb `signIn`, which is what the catalogue's Cypher matches on.
{rels:?}"
    );
    assert!(
        rels.iter().any(|r| r.contains("\"t\":\"transfer\"")),
        "the default TYPE is the verb `transfer`.
{rels:?}"
    );
}

#[test]
fn the_multi_edge_survives_with_its_own_properties() {
    // FinBench's defining property. Measured on the shipped SF0.01 corpus,
    // 24.6% of AccountTransferAccount edges share an endpoint pair, so a
    // converter that collapsed them would drop a quarter of the benchmark's
    // largest edge type without failing.
    let d = tmp("multi");
    corpus(&d);
    let (log, out) = convert(&d);
    let rels = lines(&out.join("rels.jsonl"));
    let transfers: Vec<&String> = rels
        .iter()
        .filter(|r| r.contains("\"t\":\"transfer\""))
        .collect();
    assert_eq!(
        transfers.len(),
        2,
        "both edges between the one pair must survive.\n{log}"
    );
    assert!(
        transfers.iter().any(|r| r.contains("3851891.85"))
            && transfers.iter().any(|r| r.contains("734861.37")),
        "each multi-edge keeps its OWN amount; collapsing to one value would \
         still count two.\n{transfers:?}"
    );
}

#[test]
fn relationship_properties_are_carried_and_declared() {
    let d = tmp("props");
    corpus(&d);
    let (_, out) = convert(&d);
    let meta = std::fs::read_to_string(out.join("meta.json")).expect("meta.json");
    assert!(
        meta.contains("\"rel_props\":true"),
        "a corpus whose edges carry properties must SAY so in meta.json: {meta}"
    );
    assert!(
        meta.contains("\"key\":\"gid\""),
        "FinBench ids exceed u32, so the corpus must declare gid keying: {meta}"
    );
    let rels = lines(&out.join("rels.jsonl"));
    assert!(
        rels.iter().all(|r| r.contains("\"p\":{")),
        "every edge in this fixture carries at least one property.\n{rels:?}"
    );
}

#[test]
fn a_row_that_does_not_match_its_header_is_refused_not_guessed() {
    // An unquoted pipe inside FinBench's generated `comment` prose would do
    // this. Silently dropping or padding the row loses data that no count
    // downstream can recover, so the converter must stop.
    let d = tmp("ragged");
    corpus(&d);
    write(
        &d.join("snapshot"),
        "AccountWithdrawAccount.csv",
        "fromId|toId|amount\n4614219293217785425|4619004367821865972\n",
    );
    let (log, _) = convert(&d);
    assert!(
        log.contains("REFUSING"),
        "a ragged row must be refused loudly; got:\n{log}"
    );
}

#[test]
fn timestamps_become_epoch_millis_not_formatted_text() {
    // The converter emitted the CSV's formatted text, reasoning that parsing
    // would silently pick a timezone the corpus never states. Defensible, and
    // wrong in its conclusion: the published reference compares `e.timestamp`
    // against INTEGER parameters and carries `9223372036854775807` (i64::MAX)
    // as its monotone-walk sentinel, so a string there is not a filter that can
    // work -- every TCR query returned zero rows rather than being merely
    // differently typed.
    //
    // The values are pinned against an INDEPENDENT calculation rather than the
    // converter's own output: `2020-01-10 06:22:20.222` UTC is 1578637340222,
    // computed with Python's datetime, and `2020-08-29 15:28:58.647` is
    // 1598714938647. A fixture derived from the code under test would only
    // prove the code agrees with itself.
    let d = tmp("millis");
    corpus(&d);
    let (_, out) = convert(&d);

    let nodes = lines(&out.join("nodes.jsonl"));
    assert!(
        nodes
            .iter()
            .any(|n| n.contains("\"createTime\":1578637340222")),
        "2020-01-10 06:22:20.222 UTC is 1578637340222 epoch ms.
{nodes:?}"
    );
    let rels = lines(&out.join("rels.jsonl"));
    assert!(
        rels.iter()
            .any(|r| r.contains("\"timestamp\":1598714938647")),
        "2020-08-29 15:28:58.647 UTC is 1598714938647 epoch ms.
{rels:?}"
    );
    // And NOTHING keeps the formatted shape: a single unconverted column would
    // fail exactly the query that reads it, which is the quiet case.
    assert!(
        !rels.iter().any(|r| r.contains("2020-08-29")),
        "no relationship may carry a formatted timestamp.
{rels:?}"
    );
}

#[test]
fn a_value_that_is_not_a_timestamp_is_left_alone() {
    // The guard on the conversion. `location` is free text and `mediumType` is
    // a word; a parser that accepted anything vaguely date-shaped would turn a
    // property into a number and lose it.
    let d = tmp("nonts");
    corpus(&d);
    let (_, out) = convert(&d);
    let rels = lines(&out.join("rels.jsonl"));
    assert!(
        rels.iter().any(|r| r.contains("\"location\":\"Vietnam\"")),
        "a non-timestamp string stays a string.
{rels:?}"
    );
}

#[test]
fn the_file_name_spelling_is_still_reachable_and_still_distinguishes_the_tails() {
    // `--type-style file` restores LDBC's galaxybase spelling. Kept reachable
    // because the two spellings are not interchangeable for every engine: an
    // engine storing one table per (type, src-label, dst-label) triple sees
    // `own` as an ALTERNATION over two tails, which the catalogue's
    // `rel_alternation` note is about. This asserts the flag actually changes
    // the output rather than being accepted and ignored.
    let d = tmp("typestyle");
    corpus(&d);
    let out = d.join("jsonl-file");
    let exe = env!("CARGO_BIN_EXE_finbench2jsonl");
    let o = std::process::Command::new(exe)
        .arg(&d)
        .arg(&out)
        .arg("--type-style")
        .arg("file")
        .output()
        .expect("run finbench2jsonl");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    let rels = lines(&out.join("rels.jsonl"));
    assert!(
        rels.iter()
            .any(|r| r.contains("\"t\":\"AccountTransferAccount\"")),
        "--type-style file must emit the galaxybase spelling.
{rels:?}
{log}"
    );
    assert!(
        !rels.iter().any(|r| r.contains("\"t\":\"transfer\"")),
        "and then NOT the verb, or the flag did nothing.
{rels:?}"
    );
}
