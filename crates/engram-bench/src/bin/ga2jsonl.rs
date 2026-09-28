//! `ga2jsonl` — an LDBC Graphalytics graph → the JSONL corpus contract.
//!
//! What the suite has published as "Graphalytics" so far is six algorithms run
//! over a friendship graph extracted from SNB SF3 and checked against LDBC's
//! reference values. That is a correctness result, not a Graphalytics result.
//! The benchmark has its OWN graphs, its own per-graph parameters, and a
//! published expected output for every algorithm on every graph — which is
//! what makes a run checkable by someone who did not produce it.
//!
//! This reads a dataset in that published form:
//!
//! ```text
//! <graph>.v            one vertex id per line
//! <graph>.e            `src dst [weight]`
//! <graph>.properties   directedness, and each algorithm's parameters
//! <graph>-BFS, -PR, …  the expected output, `vertexId value` per line
//! ```
//!
//! and emits `nodes.jsonl` / `rels.jsonl` / `meta.json` plus `params.json`,
//! which carries the algorithm parameters the `.properties` file fixes — the
//! BFS source vertex, PageRank's damping factor and iteration count, SSSP's
//! weight property. Running an algorithm with parameters of our own choosing
//! and comparing against LDBC's expected output would compare two different
//! computations, so the parameters travel with the graph.
//!
//! The vertex id is kept as a property (`vid`) as well as the corpus key, so a
//! result can be mapped back to the id the expected-output file uses. It is
//! not called `gid`: the loader reserves that name for the corpus id and
//! refuses the corpus rather than shadowing it.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: ga2jsonl <graph dir> <out dir> [--name <graph>]");
        eprintln!("  reads <graph>.v, <graph>.e and <graph>.properties");
        std::process::exit(2);
    }
    let dir = PathBuf::from(&args[1]);
    let out = PathBuf::from(&args[2]);
    let mut name = None;
    let mut i = 3;
    while i < args.len() {
        if args[i] == "--name" {
            name = args.get(i + 1).cloned();
            i += 2;
        } else {
            eprintln!("ga2jsonl: unknown argument `{}`", args[i]);
            std::process::exit(2);
        }
    }

    // the graph's name is whatever `.properties` file is there, unless told
    let name = match name {
        Some(n) => n,
        None => {
            let mut found = None;
            for e in fs::read_dir(&dir)? {
                let p = e?.path();
                if p.extension().and_then(|s| s.to_str()) == Some("properties") {
                    found = p.file_stem().and_then(|s| s.to_str()).map(String::from);
                    break;
                }
            }
            found.unwrap_or_else(|| {
                eprintln!("ga2jsonl: no .properties file in {}", dir.display());
                std::process::exit(2);
            })
        }
    };
    fs::create_dir_all(&out)?;

    // ── the properties file: directedness and per-algorithm parameters ─────
    let props = fs::read_to_string(dir.join(format!("{name}.properties")))?;
    let mut settings: BTreeMap<String, String> = BTreeMap::new();
    for line in props.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let prefix = format!("graph.{name}.");
        let Some(k) = k.strip_prefix(&prefix) else {
            continue;
        };
        settings.insert(k.to_string(), v.trim().to_string());
    }
    let directed = settings
        .get("directed")
        .map(|s| s == "true")
        .unwrap_or(true);
    let weighted = settings
        .get("edge-properties.names")
        .map(|s| s.contains("weight"))
        .unwrap_or(false);

    // ── vertices ───────────────────────────────────────────────────────────
    let mut nodes = BufWriter::with_capacity(1 << 20, fs::File::create(out.join("nodes.jsonl"))?);
    let mut n_nodes = 0u64;
    for line in BufReader::new(fs::File::open(dir.join(format!("{name}.v")))?).lines() {
        let line = line?;
        let id = line.trim();
        if id.is_empty() {
            continue;
        }
        // `vid`, not `gid`: the loader reserves `gid` for the corpus id itself
        // and refuses a property of that name rather than quietly shadowing it.
        writeln!(
            nodes,
            "{{\"i\":\"{id}\",\"l\":[\"Vertex\"],\"p\":{{\"vid\":{id}}}}}"
        )?;
        n_nodes += 1;
    }
    nodes.flush()?;

    // ── edges ──────────────────────────────────────────────────────────────
    //
    // An UNDIRECTED graph is written once in the file and must be traversable
    // both ways. The algorithms take an orientation, so the edge is emitted
    // once and the projection is asked for UNDIRECTED — writing it twice would
    // double every degree and quietly change every answer that counts edges.
    let mut rels = BufWriter::with_capacity(1 << 20, fs::File::create(out.join("rels.jsonl"))?);
    let mut n_rels = 0u64;
    for line in BufReader::new(fs::File::open(dir.join(format!("{name}.e")))?).lines() {
        let line = line?;
        let mut it = line.split_whitespace();
        let (Some(s), Some(d)) = (it.next(), it.next()) else {
            continue;
        };
        let w = it.next();
        match (weighted, w) {
            (true, Some(w)) => writeln!(
                rels,
                "{{\"s\":\"{s}\",\"d\":\"{d}\",\"t\":\"LINK\",\"p\":{{\"weight\":{w}}}}}"
            )?,
            _ => writeln!(
                rels,
                "{{\"s\":\"{s}\",\"d\":\"{d}\",\"t\":\"LINK\",\"p\":{{}}}}"
            )?,
        }
        n_rels += 1;
    }
    rels.flush()?;

    // ── what the run needs to know, travelling with the graph ──────────────
    let mut meta = BufWriter::new(fs::File::create(out.join("meta.json"))?);
    writeln!(
        meta,
        "{{\"family\":\"graphalytics\",\"graph\":\"{name}\",\"nodes\":{n_nodes},\"rels\":{n_rels},\
         \"rel_props\":{weighted},\"edge_ids_dense\":false,\"key\":\"gid\",\
         \"directed\":{directed},\"rel_type_counts\":{{\"LINK\":{n_rels}}}}}"
    )?;
    meta.flush()?;

    let mut params = BufWriter::new(fs::File::create(out.join("params.json"))?);
    write!(params, "{{\"graph\":\"{name}\",\"directed\":{directed}")?;
    for k in [
        "algorithms",
        "bfs.source-vertex",
        "cdlp.max-iterations",
        "pr.damping-factor",
        "pr.num-iterations",
        "sssp.weight-property",
        "sssp.source-vertex",
    ] {
        if let Some(v) = settings.get(k) {
            write!(params, ",\"{k}\":\"{v}\"")?;
        }
    }
    writeln!(params, "}}")?;
    params.flush()?;

    eprintln!(
        "[ga2jsonl] {name}: {n_nodes} vertices, {n_rels} edges, directed={directed}, \
         weighted={weighted}"
    );
    eprintln!(
        "[ga2jsonl] expected output present: {}",
        reference_files(&dir, &name).join(", ")
    );
    Ok(())
}

/// Which algorithms this graph ships an expected output for — the set a run
/// can actually be checked against.
fn reference_files(dir: &Path, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    for algo in ["BFS", "CDLP", "LCC", "PR", "SSSP", "WCC"] {
        if dir.join(format!("{name}-{algo}")).exists() {
            out.push(algo.to_string());
        }
    }
    if out.is_empty() {
        out.push("none".into());
    }
    out
}
