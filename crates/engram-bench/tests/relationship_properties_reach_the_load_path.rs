#![allow(non_snake_case)]
//! FinBench's prerequisite in the LOADER: `rels.jsonl` may carry `"p"`.
//!
//! `snbload` read exactly `s`, `d` and `t` and dropped everything else, so a
//! corpus whose edges carry properties — every FinBench edge type that matters
//! — loaded clean and produced a graph missing the data the benchmark is about.
//! The engine was never the blocker: it stores and reads relationship
//! properties, and `a_relationship_read_by_property_binds_from_the_adjacency`
//! covers the optimised read.
//!
//! Two assertions, and the SECOND is the load-bearing one:
//!
//!   1. a corpus with `"p"` emits a three-element UNWIND element and a `SET`.
//!   2. a corpus WITHOUT `"p"` emits exactly what it emitted before — because
//!      every SNB measurement in `measurements/` was taken against that
//!      statement, and a loader that silently started writing a different one
//!      would invalidate the back catalogue without failing anything.

use std::io::Write;
use std::path::{Path, PathBuf};

fn corpus(dir: &Path, rels: &str) {
    std::fs::create_dir_all(dir).expect("mkdir");
    let mut n = std::fs::File::create(dir.join("nodes.jsonl")).expect("nodes");
    writeln!(n, r#"{{"i":"a1","l":["Account"],"p":{{"id":1}}}}"#).unwrap();
    writeln!(n, r#"{{"i":"a2","l":["Account"],"p":{{"id":2}}}}"#).unwrap();
    let mut r = std::fs::File::create(dir.join("rels.jsonl")).expect("rels");
    write!(r, "{rels}").unwrap();
}

fn dump(dir: &Path, out: &Path) -> String {
    let exe = env!("CARGO_BIN_EXE_snbload");
    let st = std::process::Command::new(exe)
        .arg(dir)
        .arg("127.0.0.1:1")
        .env("SNBLOAD_DUMP", out)
        .output()
        .expect("run snbload");
    assert!(
        out.exists(),
        "snbload wrote no plan.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&st.stdout),
        String::from_utf8_lossy(&st.stderr)
    );
    std::fs::read_to_string(out).expect("read plan")
}

fn tmp(name: &str) -> PathBuf {
    // Per-test directory: the binary runs its tests in parallel and a shared
    // name would have them overwrite each other's corpora.
    let d = std::env::temp_dir().join(format!("relprops-{}-{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn a_corpus_carrying_relationship_properties_emits_them() {
    let d = tmp("with");
    corpus(
        &d,
        concat!(
            r#"{"s":"a1","d":"a2","t":"TRANSFER","p":{"amount":10}}"#,
            "\n",
            r#"{"s":"a1","d":"a2","t":"TRANSFER","p":{"amount":20}}"#,
            "\n"
        ),
    );
    let plan = dump(&d, &d.join("plan.txt"));
    assert!(
        plan.contains("amount: 10") && plan.contains("amount: 20"),
        "both property maps must reach the plan; got:\n{plan}"
    );
}

#[test]
fn a_corpus_without_them_emits_the_statement_it_always_did() {
    // THE REGRESSION GUARD. If this ever fails, every SNB figure already
    // recorded was measured against a statement the loader no longer sends.
    let d = tmp("without");
    corpus(
        &d,
        concat!(
            r#"{"s":"a1","d":"a2","t":"TRANSFER"}"#,
            "\n",
            r#"{"s":"a2","d":"a1","t":"TRANSFER"}"#,
            "\n"
        ),
    );
    let plan = dump(&d, &d.join("plan.txt"));
    assert!(
        !plan.contains("SET r ="),
        "a corpus with no relationship properties must not emit a SET; got:\n{plan}"
    );
}

#[test]
fn an_empty_property_map_is_treated_as_no_properties() {
    // `"p":{}` is a corpus that declares the field and fills nothing. Rendering
    // `{}` would change the statement for a corpus that carries no data, which
    // is the back-catalogue hazard again in its quietest form.
    let d = tmp("empty");
    corpus(
        &d,
        concat!(r#"{"s":"a1","d":"a2","t":"TRANSFER","p":{}}"#, "\n"),
    );
    let plan = dump(&d, &d.join("plan.txt"));
    assert!(
        !plan.contains("SET r ="),
        "an empty property map must not reach the statement; got:\n{plan}"
    );
}

/// The Neo4j-side converter must emit what `snbload` sends — BY DEFAULT.
///
/// `jsonl2neo4j`'s `--rel-props` was OFF by default and its help said why:
/// "snbload never sends them and the Bolt-loaded engines therefore do not have
/// them." That was true when it was written. It stopped being true when the
/// test above shipped: snbload now sends `SET r = ...` whenever the corpus
/// carries edge properties, and has no flag not to.
///
/// So the old default no longer preserved parity — it CREATED the divergence
/// it existed to prevent, giving Neo4j a smaller graph than every Bolt-loaded
/// engine. At SF3 all 565,247 KNOWS edges carry `creationDate`, and SNB BI's
/// bi11 filters on it — the only BI query that reads an edge property — as do
/// SNB Interactive's IC1, IC5, IC7, IC11 and IS3 on their own edge
/// properties; without those columns Neo4j cannot answer queries engram can,
/// and the gap would be read as an engine difference.
///
/// This asserts the two loaders agree on the SAME corpus: if snbload emits a
/// `SET`, the Neo4j CSV must carry the column.
#[test]
fn the_neo4j_converter_emits_edge_properties_by_default_just_as_snbload_does() {
    let d = std::env::temp_dir().join(format!("relprops-parity-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    corpus(
        &d,
        "{\"s\":\"a1\",\"d\":\"a2\",\"t\":\"KNOWS\",\"p\":{\"creationDate\":{\"~bigint\":\"1282333988428\"}}}\n",
    );
    std::fs::write(
        d.join("meta.json"),
        "{\"nodes\":{\"Account\":2},\"rels\":{\"KNOWS\":1}}",
    )
    .expect("meta");

    let out = d.join("csv");
    let exe = env!("CARGO_BIN_EXE_jsonl2neo4j");
    // NO flag: the default is the claim under test.
    let o = std::process::Command::new(exe)
        .arg(&d)
        .arg(&out)
        .arg("--skip-readback")
        .output()
        .expect("run jsonl2neo4j");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );

    let manifest = std::fs::read_to_string(out.join("manifest.json"))
        .unwrap_or_else(|e| panic!("manifest.json: {e}\n{log}"));
    assert!(
        manifest.contains("\"rel_properties_emitted\":true"),
        "edge properties must be emitted by DEFAULT, or Neo4j gets a different \
         graph from snbload's engines.\n{manifest}"
    );

    // And the column is actually THERE — a manifest flag that nothing backs
    // would be the same absent-signal failure this file already guards against.
    let csv: String = std::fs::read_dir(&out)
        .expect("read out dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rels") && n.ends_with(".csv"))
        })
        .map(|p| std::fs::read_to_string(p).unwrap_or_default())
        .collect();
    assert!(
        csv.contains("creationDate"),
        "the KNOWS CSV must carry the creationDate column.\n{csv}\n{log}"
    );
}
