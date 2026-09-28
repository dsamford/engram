#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
//! The LDBC Graphalytics lane: six kernels, measured the way the benchmark
//! defines measurement.
//!
//! # Why this exists when a shell script already ran the kernels
//!
//! `docs/bench/graphalytics-all-algorithms.sh` runs all six and diffs them
//! against the published reference output, which establishes CORRECTNESS. It
//! establishes nothing about performance, and it cannot, because Graphalytics
//! does not define performance as "how long one run took":
//!
//! * **Three repetitions per algorithm-dataset job** (spec §3.2.1), ranked on
//!   the **arithmetic mean** (§4.2.1).
//! * **`EVPS = (V + E) / mean(Tp)`** and NEVER the mean of per-run EVPS
//!   (§4.2.1 states this in both directions). Averaging rates flatters the
//!   fast run.
//! * **Tl and Tp are separate, and Tl is excluded from Tp** (§2.5.3): Tp is
//!   the algorithm alone, not loading or partitioning.
//! * **Per-scale timeouts** (§3.2.1 Table 3.1): S 900 s, M 1800 s, L 3600 s,
//!   XL 7200 s, 2XL+ 10800 s.
//!
//! The script reports one whole-second `date` difference per kernel. That is
//! not any of the above, and a number taken from it is not a Graphalytics
//! number.
//!
//! # The trap this runner is built around
//!
//! §2.5.3: when a job hits its timeout it is terminated "and the time-out
//! duration is reported as the performance metrics instead", and §3.3
//! classifies that run **TIM**. So a job that did not finish contributes a
//! FINITE metric — and a flattering one, because the true time was longer.
//! Fold three runs of which one breached into a mean and the mean is lower
//! than any honest measurement of that job.
//!
//! **So the breach is carried per run, all the way into the report row.** A
//! job with any breached repetition is reported as `tim` and its mean is
//! marked `lower_bound`, never as a performance result. That is the same
//! failure shape as an absent scan read as a clean one: an absence read as a
//! value.
//!
//! # Conformance is ON
//!
//! Every kernel is invoked with `graphalytics: true`, which is what makes
//! BFS return `9223372036854775807` for an unreachable vertex, SSSP an
//! infinity, CDLP count in- and out-neighbours separately, PageRank run
//! exactly `maxIterations`, and LCC keep direction in its triangle test. The
//! shipped defaults are deliberately different and are the right answer for a
//! query; they are not what the benchmark validates.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use engram_bench::backend::{Backend, BoltBackend, Cell};

/// One repetition's outcome.
#[derive(Debug, Clone)]
struct Rep {
    /// Wall milliseconds for the algorithm alone.
    tp_ms: f64,
    /// Whether this repetition hit the per-scale ceiling.
    breached: bool,
    /// Rows the kernel produced.
    rows: usize,
}

/// What a comparison concluded, as NUMBERS plus a human line.
///
/// Separate from the printed text on purpose: a status decided by searching
/// that text for "DIFFERS" is a status that passes whenever the wording
/// changes, and it passed `0/10` once already.
#[derive(Debug, Clone)]
struct Verdict {
    /// Did every compared vertex agree, with none missing?
    agrees: bool,
    /// The line a reader sees.
    detail: String,
}

/// How a kernel's output is compared with the reference.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Match {
    /// Every vertex's value identical. BFS, CDLP.
    Exact,
    /// `|r - s| <= 1e-4 * |r|`, and where `r == 0` ONLY an exact zero passes.
    /// PageRank, SSSP, LCC.
    Epsilon,
    /// A two-way label mapping is allowed; only the partition must agree. WCC.
    Equivalence,
}

/// One kernel, as the spec names it and as engram exposes it.
struct Kernel {
    /// The reference file's suffix: `<graph>-BFS`.
    reference: &'static str,
    /// The `engram.algo.*` procedure.
    procedure: &'static str,
    /// The `YIELD` field carrying the value.
    yields: &'static str,
    /// How the output is judged.
    mode: Match,
}

const KERNELS: &[Kernel] = &[
    Kernel {
        reference: "BFS",
        procedure: "bfs",
        yields: "depth",
        mode: Match::Exact,
    },
    Kernel {
        reference: "WCC",
        procedure: "wcc",
        yields: "componentId",
        mode: Match::Equivalence,
    },
    Kernel {
        reference: "PR",
        procedure: "pagerank",
        yields: "score",
        mode: Match::Epsilon,
    },
    Kernel {
        reference: "SSSP",
        procedure: "sssp",
        yields: "distance",
        mode: Match::Epsilon,
    },
    Kernel {
        reference: "LCC",
        procedure: "localclusteringcoefficient",
        yields: "coefficient",
        mode: Match::Epsilon,
    },
    Kernel {
        reference: "CDLP",
        procedure: "labelpropagation",
        yields: "communityId",
        mode: Match::Exact,
    },
];

/// The same call in `stats` mode: `.stream(` becomes `.stats(`, and
/// everything from the first ` YIELD ` becomes `YIELD nodeCount RETURN
/// nodeCount`. No configuration map here contains " YIELD ".
fn stats_statement(stream: &str) -> String {
    let s = stream.replacen(".stream(", ".stats(", 1);
    match s.find(" YIELD ") {
        Some(at) => format!("{} YIELD nodeCount RETURN nodeCount", &s[..at]),
        None => s,
    }
}

/// A stream's rows as `vertex id -> value text`.
fn values_of(rows: &[Vec<Cell>]) -> BTreeMap<u64, String> {
    let mut m = BTreeMap::new();
    for row in rows {
        if let (Some(a), Some(b)) = (row.first(), row.get(1)) {
            if let Some(vid) = a.as_int() {
                m.insert(u64::try_from(vid).unwrap_or(0), cell_text(b));
            }
        }
    }
    m
}

/// `--kernels BFS,LCC`: the jobs to run, by their reference names, any case.
/// `None` runs all six. An unknown name is refused rather than dropped: a
/// filter that silently matched nothing would report a clean run of nothing.
///
/// It exists for the chain that restarts the server after a TIM. The ceiling
/// bounds this CLIENT; a breached kernel keeps computing inside the server,
/// under every repetition and every kernel issued after it, so the only clean
/// measurement after a breach is on a fresh server, one kernel per invocation.
fn kernel_filter(arg: Option<&str>) -> Result<Option<Vec<String>>, String> {
    let Some(arg) = arg else {
        return Ok(None);
    };
    let mut out = Vec::new();
    for k in arg.split(',').map(str::trim).filter(|k| !k.is_empty()) {
        let k = k.to_ascii_uppercase();
        if !KERNELS.iter().any(|x| x.reference == k) {
            return Err(format!(
                "--kernels: no kernel `{k}`; the six are {}",
                KERNELS.iter().map(|x| x.reference).collect::<Vec<_>>().join(", ")
            ));
        }
        out.push(k);
    }
    if out.is_empty() {
        return Err("--kernels names no kernel".to_string());
    }
    Ok(Some(out))
}

/// The spec's per-scale ceiling, in seconds (§3.2.1, Table 3.1).
///
/// Keyed on the LDBC scale LETTER rather than on a vertex count, because that
/// is what the benchmark assigns a graph and what a reader can check against
/// the table. An unknown letter takes the SMALLEST ceiling rather than the
/// largest: a run that overruns an unexpectedly tight bound is visible, where
/// one given three hours by default is a silent stall.
fn ceiling_secs(scale: &str) -> u64 {
    match scale.to_ascii_uppercase().as_str() {
        "S" => 900,
        "M" => 1800,
        "L" => 3600,
        "XL" => 7200,
        "2XL" | "3XL" | "4XL" | "5XL" => 10800,
        _ => 900,
    }
}

/// Read a `vertexId value` reference file.
fn read_reference(path: &Path) -> Result<BTreeMap<u64, String>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(v), Some(val)) = (it.next(), it.next()) else {
            continue;
        };
        let Ok(v) = v.parse::<u64>() else { continue };
        out.insert(v, val.to_string());
    }
    if out.is_empty() {
        return Err(format!("{} holds no vertex rows", path.display()));
    }
    Ok(out)
}

/// Is this token one of the infinity spellings either side may use?
fn is_inf(s: &str) -> bool {
    let t = s.trim().trim_start_matches(['+', '-']);
    t.eq_ignore_ascii_case("inf") || t.eq_ignore_ascii_case("infinity")
}

/// Compare one kernel's output with the reference, in the mode the spec
/// assigns that kernel.
///
/// # The epsilon rule is RELATIVE, and it collapses at zero
///
/// §2.4: values match iff `|r - s| <= eps*|r|` with `eps = 0.0001`, where `r`
/// is the REFERENCE. Where `r == 0` the bound is zero and **only an exact zero
/// passes**. For LCC that is not an edge case — it is every vertex of degree 0
/// or 1, the most numerous rows in the file — so zero-reference rows are
/// counted and reported separately rather than folded into a total.
fn compare(mode: Match, reference: &BTreeMap<u64, String>, got: &BTreeMap<u64, String>) -> Verdict {
    let mut n = 0usize;
    let mut ok = 0usize;
    let mut missing = 0usize;
    let mut worst = 0.0f64;
    let mut worst_at = String::from("-");
    let mut zero_ref = 0usize;
    let mut zero_bad = 0usize;
    // Equivalence needs a two-way mapping, built as it goes.
    let mut fwd: BTreeMap<String, String> = BTreeMap::new();
    let mut rev: BTreeMap<String, String> = BTreeMap::new();
    let mut partition_ok = true;

    for (v, r) in reference {
        n += 1;
        let Some(s) = got.get(v) else {
            missing += 1;
            continue;
        };
        match mode {
            Match::Exact => {
                if s == r {
                    ok += 1;
                }
            }
            Match::Equivalence => {
                let f = fwd.entry(r.clone()).or_insert_with(|| s.clone());
                let b = rev.entry(s.clone()).or_insert_with(|| r.clone());
                if f == s && b == r {
                    ok += 1;
                } else {
                    partition_ok = false;
                }
            }
            Match::Epsilon => {
                // Infinity is a class on both sides: `infinity` and `inf` are
                // the same answer, and `+0` on either token is 0 in a naive
                // parse — which would score an unreachable vertex answered as
                // distance zero as a MATCH.
                if is_inf(r) || is_inf(s) {
                    if is_inf(r) && is_inf(s) {
                        ok += 1;
                    }
                    continue;
                }
                let (Ok(rv), Ok(sv)) = (r.parse::<f64>(), s.parse::<f64>()) else {
                    continue;
                };
                let d = (sv - rv).abs();
                let a = rv.abs();
                if a == 0.0 {
                    zero_ref += 1;
                    if d == 0.0 {
                        ok += 1;
                    } else {
                        zero_bad += 1;
                    }
                    continue;
                }
                if d <= 1e-4 * a {
                    ok += 1;
                }
                let rel = d / a;
                if rel > worst {
                    worst = rel;
                    worst_at = v.to_string();
                }
            }
        }
    }
    let miss = if missing > 0 {
        format!(", {missing} NOT returned")
    } else {
        String::new()
    };
    let detail = match mode {
        Match::Exact => format!("{ok}/{n} exact{miss}"),
        Match::Equivalence => format!(
            "partition {} over {n} vertices, {} component(s){miss}",
            if partition_ok { "matches" } else { "DIFFERS" },
            fwd.len()
        ),
        Match::Epsilon => format!(
            "{ok}/{n} within 1e-4 relative, worst {worst:.3e} at {worst_at}, \
             {zero_ref} zero-ref{}{miss}",
            if zero_bad > 0 {
                format!(" ({zero_bad} NOT exactly zero)")
            } else {
                " all exact".to_string()
            }
        ),
    };
    // The verdict is computed from COUNTS, never sniffed out of the text
    // above. A first cut of this runner decided `ok` by searching that text
    // for "DIFFERS" and "NOT returned", and so reported
    // `0/10 within 1e-4 relative` as a PASS -- the validator agreeing with
    // itself about nothing, which is the failure this lane exists to refuse.
    let agrees = match mode {
        Match::Equivalence => partition_ok && missing == 0,
        _ => ok == n && missing == 0 && n > 0,
    };
    Verdict { agrees, detail }
}

fn cell_text(c: &Cell) -> String {
    match c {
        Cell::Int(n) => n.to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Null => "null".into(),
    }
}

/// Run one statement with a deadline on a FRESH connection, so an abandoned
/// worker cannot poison the next repetition's session.
fn timed(addr: &str, stmt: &str, ceiling: Duration) -> (Option<Vec<Vec<Cell>>>, f64, bool) {
    let (tx, rx) = std::sync::mpsc::channel();
    let (a, s) = (addr.to_string(), stmt.to_string());
    std::thread::spawn(move || {
        let r = (|| -> Result<(Vec<Vec<Cell>>, f64), String> {
            let mut b = BoltBackend::connect(&a).map_err(|e| e.to_string())?;
            let t0 = Instant::now();
            let rows = b.query(&s).map_err(|e| e.to_string())?;
            Ok((rows, t0.elapsed().as_secs_f64() * 1000.0))
        })();
        let _ = tx.send(r);
    });
    match rx.recv_timeout(ceiling) {
        Ok(Ok((rows, ms))) => (Some(rows), ms, false),
        Ok(Err(_)) => (None, 0.0, false),
        // §2.5.3: the ceiling itself becomes the metric, and the run is TIM.
        Err(_) => (None, ceiling.as_secs_f64() * 1000.0, true),
    }
}

/// `params.json`, as `ga2jsonl` writes it into its OUTPUT directory.
fn read_params_json(dir: &Path) -> Option<BTreeMap<String, String>> {
    let t = std::fs::read_to_string(dir.join("params.json")).ok()?;
    let engram_cypher::Value::Map(m) = engram_cypher::json::from_json(&t).ok()? else {
        return None;
    };
    Some(
        m.into_iter()
            .map(|(k, val)| {
                let text = match val {
                    engram_cypher::Value::Str(x) => x,
                    engram_cypher::Value::Int(x) => x.to_string(),
                    engram_cypher::Value::Float(x) => x.to_string(),
                    engram_cypher::Value::Bool(x) => x.to_string(),
                    other => format!("{other:?}"),
                };
                (k, text)
            })
            .collect(),
    )
}

/// LDBC's own `<graph>.properties`, which always sits beside the graph.
///
/// Every key is prefixed `graph.<name>.`; the prefix is stripped so both
/// sources yield the same key set (`bfs.source-vertex`, `pr.damping-factor`,
/// `sssp.weight-property`, `directed`).
fn read_properties(dir: &Path, name: &str) -> Option<BTreeMap<String, String>> {
    let t = std::fs::read_to_string(dir.join(format!("{name}.properties"))).ok()?;
    let prefix = format!("graph.{name}.");
    let mut m = BTreeMap::new();
    for line in t.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let Some(k) = k.strip_prefix(&prefix) else {
            continue;
        };
        m.insert(k.to_string(), v.trim().to_string());
    }
    (!m.is_empty()).then_some(m)
}

fn flag<'a>(args: &'a [String], k: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == k)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

#[allow(clippy::too_many_lines)]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: graphalytics <bolt-addr> <graph dir> [--name G] [--scale S|M|L|XL|2XL]
                    [--reps 3] [--json OUT]

Measures the six LDBC Graphalytics kernels against a corpus ALREADY LOADED at
<bolt-addr>, and validates each against the published reference output beside
the graph.

The protocol is the benchmark's, not a stopwatch: three repetitions per job,
ranked on the ARITHMETIC MEAN; EVPS = (V+E)/mean(Tp) and never the mean of
per-run rates; Tl reported separately and EXCLUDED from Tp; the per-scale
ceiling from Table 3.1. A repetition that breaches the ceiling contributes the
CEILING as its metric and marks the whole job TIM -- it is never folded into a
mean that then reads as a performance result.

Every kernel runs with `graphalytics: true`. The shipped `engram.algo.*`
defaults are deliberately different and are the better answer for a query; they
are not what this benchmark validates."
        );
        std::process::exit(2);
    }
    let addr = args[1].clone();
    let dir = PathBuf::from(&args[2]);
    let reps: usize = flag(&args, "--reps")
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    let scale = flag(&args, "--scale").unwrap_or("S").to_string();
    let ceiling = Duration::from_secs(ceiling_secs(&scale));
    // `--tp stats` (the default) times each repetition as the procedure's
    // `stats` mode: the kernel runs whole and returns one summary row. The
    // spec's Tp is processing time EXCLUDING output writing (§2.5.3), and a
    // stream of every vertex's value through Cypher and Bolt is output:
    // ~9 us a row on cit-Patents, so ~144 s of datagen-7_8-zf's 16.5M rows
    // would have been read as kernel time. The answer is still validated, from
    // ONE stream run after the repetitions, timed and ceiling-bounded apart.
    // `--tp stream` keeps the old measurement (the stream IS the repetition).
    let tp_stats = match flag(&args, "--tp").unwrap_or("stats") {
        "stats" => true,
        "stream" => false,
        other => {
            eprintln!("[graphalytics] --tp takes stats or stream, got `{other}`");
            std::process::exit(2);
        }
    };
    let only = match kernel_filter(flag(&args, "--kernels")) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[graphalytics] {e}");
            std::process::exit(2);
        }
    };

    // The graph's name decides every reference file's path.
    let name = match flag(&args, "--name") {
        Some(n) => n.to_string(),
        None => match std::fs::read_dir(&dir) {
            Ok(rd) => rd
                .filter_map(Result::ok)
                .find_map(|e| {
                    let p = e.path();
                    (p.extension()?.to_str()? == "properties")
                        .then(|| p.file_stem()?.to_str().map(str::to_string))?
                })
                .unwrap_or_else(|| {
                    eprintln!(
                        "[graphalytics] no .properties in {}; pass --name",
                        dir.display()
                    );
                    std::process::exit(2);
                }),
            Err(e) => {
                eprintln!("[graphalytics] {}: {e}", dir.display());
                std::process::exit(2);
            }
        },
    };

    let mut be = match BoltBackend::connect(&addr) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[graphalytics] cannot reach {addr}: {e}");
            std::process::exit(1);
        }
    };
    let engine = be.engine().to_string();
    let version = be.version().to_string();
    eprintln!("[graphalytics] {engine} {version} against {addr}, graph {name}, scale {scale}");
    eprintln!(
        "[graphalytics] ceiling {} s per repetition, {reps} repetition(s) per job",
        ceiling.as_secs()
    );

    // ── Tl: the load, measured and then EXCLUDED from every Tp ───────────
    //
    // The corpus is already resident, so what is timed here is the census that
    // proves it is — not a load. Reported as `tl_ms` with that stated, rather
    // than omitted, because a document with no Tl reads as a load that took no
    // time.
    let t0 = Instant::now();
    let (v, e) = {
        let n = be
            .query("MATCH (n:Vertex) RETURN count(n) AS n")
            .ok()
            .and_then(|r| r.first().and_then(|x| x.first()).and_then(Cell::as_int))
            .unwrap_or(0);
        let m = be
            .query("MATCH ()-[r:LINK]->() RETURN count(r) AS n")
            .ok()
            .and_then(|r| r.first().and_then(|x| x.first()).and_then(Cell::as_int))
            .unwrap_or(0);
        (n, m)
    };
    let tl_ms = t0.elapsed().as_secs_f64() * 1000.0;
    if v <= 0 {
        eprintln!("[graphalytics] the corpus holds no :Vertex — nothing to measure");
        std::process::exit(1);
    }
    eprintln!("[graphalytics] {v} vertices, {e} edges (census {tl_ms:.0} ms, EXCLUDED from Tp)");

    // The graph's own parameters, as `ga2jsonl` wrote them.
    // Read with the engine's own JSON reader. A first cut split the text on
    // quotes and took every fourth field, which mis-pairs the moment one value
    // is UNQUOTED: `"directed":true` shifted every later key by one, so
    // `sssp.weight-property` came back empty and SSSP was skipped as "no
    // weight property declared" on a graph that declares one.
    // Prefer `params.json` when the caller points at a CONVERSION output dir,
    // and fall back to LDBC's own `<graph>.properties` in the graph dir.
    //
    // The two live in different places, and the first cut read only the former
    // from the latter's directory — so every parameter came back empty, BFS
    // was issued as `{vid: }` and failed outright, and SSSP was skipped as
    // "no weight property declared" on a graph that declares one. The
    // `.properties` file is LDBC's own and is always beside the graph, so it
    // is the one that makes `<graph dir>` a sufficient argument.
    let params: BTreeMap<String, String> = read_params_json(&dir)
        .or_else(|| read_properties(&dir, &name))
        .unwrap_or_else(|| {
            eprintln!(
                "[graphalytics] no params.json in {} and no {name}.properties — \
                 every kernel parameter would be empty, so refusing rather than \
                 issuing statements with holes in them",
                dir.display()
            );
            std::process::exit(2);
        });
    let directed = params.get("directed").is_some_and(|d| d == "true");
    let orientation = if directed { "NATURAL" } else { "UNDIRECTED" };
    let bfs_src = params.get("bfs.source-vertex").cloned().unwrap_or_default();
    let sssp_src = params
        .get("sssp.source-vertex")
        .cloned()
        .unwrap_or_default();
    let weight = params
        .get("sssp.weight-property")
        .cloned()
        .unwrap_or_default();
    let damping = params
        .get("pr.damping-factor")
        .cloned()
        .unwrap_or_else(|| "0.85".into());
    let pr_iters = params
        .get("pr.num-iterations")
        .cloned()
        .unwrap_or_else(|| "20".into());
    let cdlp_iters = params
        .get("cdlp.max-iterations")
        .cloned()
        .unwrap_or_else(|| "10".into());

    let mut rows = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    for k in KERNELS {
        if only.as_ref().is_some_and(|o| !o.iter().any(|x| x == k.reference)) {
            continue;
        }
        let ref_path = dir.join(format!("{name}-{}", k.reference));
        if !ref_path.exists() {
            eprintln!(
                "[graphalytics] {:<5} no reference output — skipped",
                k.reference
            );
            continue;
        }
        let reference = match read_reference(&ref_path) {
            Ok(r) => r,
            Err(err) => {
                failures.push(format!("{}: {err}", k.reference));
                continue;
            }
        };

        // `graphalytics: true` on EVERY call. Without it these measure the
        // shipped default semantics, which is a different computation.
        let base = format!(
            "nodeLabels: ['Vertex'], relationshipTypes: ['LINK'], \
             orientation: '{orientation}', graphalytics: true"
        );
        let stmt = match k.procedure {
            "bfs" => format!(
                "MATCH (s:Vertex {{vid: {bfs_src}}}) \
                 CALL engram.algo.bfs.stream({{{base}, sourceNode: id(s)}}) \
                 YIELD node, {} RETURN node.vid AS v, {} AS x",
                k.yields, k.yields
            ),
            "sssp" => {
                if weight.is_empty() || sssp_src.is_empty() {
                    eprintln!("[graphalytics] SSSP  no weight property declared — skipped");
                    continue;
                }
                format!(
                    "MATCH (s:Vertex {{vid: {sssp_src}}}) \
                     CALL engram.algo.sssp.stream({{{base}, sourceNode: id(s), \
                     relationshipWeightProperty: '{weight}'}}) \
                     YIELD node, {} RETURN node.vid AS v, {} AS x",
                    k.yields, k.yields
                )
            }
            "pagerank" => format!(
                "CALL engram.algo.pagerank.stream({{{base}, dampingFactor: {damping}, \
                 maxIterations: {pr_iters}}}) YIELD node, {} RETURN node.vid AS v, {} AS x",
                k.yields, k.yields
            ),
            "labelpropagation" => format!(
                "CALL engram.algo.labelpropagation.stream({{{base}, maxIterations: {cdlp_iters}}}) \
                 YIELD node, communityId MATCH (c:Vertex) WHERE id(c) = communityId \
                 RETURN node.vid AS v, c.vid AS x"
            ),
            other => format!(
                "CALL engram.algo.{other}.stream({{{base}}}) YIELD node, {} \
                 RETURN node.vid AS v, {} AS x",
                k.yields, k.yields
            ),
        };

        // ── The repetitions ──────────────────────────────────────────────
        let timed_stmt = if tp_stats {
            stats_statement(&stmt)
        } else {
            stmt.clone()
        };

        // ── The warm-up: the kernel's projection built OUTSIDE Tp ────────
        //
        // engram builds the in-memory projection a kernel runs on (the CSR over
        // `:Vertex`/`LINK`) lazily, at the first algorithm call that asks for
        // it, and keeps it for later calls on the same adjacency. That build is
        // the spec's preprocessing, part of Tl and not of Tp (§2.5.3). This
        // runner left it inside the first repetition: on datagen-7_5-fb BFS read
        // 142,756 ms, then 5,735 and 5,627, a mean of 51,373 ms that was the
        // build's and not the kernel's. WCC, run next on the projection BFS had
        // built, read 5.0-5.4 s on every repetition. One call of the kernel's
        // own `stats` statement now precedes the repetitions. It builds exactly
        // the projection the repetitions use, whatever the kernel keys it on,
        // and it is timed and reported as `warmup_ms`, EXCLUDED from Tp. It
        // holds one run of the kernel besides the build, so Tl is at most
        // `warmup_ms - mean(Tp)`. Its bound is four ceilings: it is not a
        // measured repetition, and a build that outran even that is a TIM.
        let (_, warmup_ms, warmup_breached) =
            timed(&addr, &stats_statement(&stmt), ceiling.saturating_mul(4));
        eprintln!(
            "[graphalytics] {:<5} warm-up {:>9.0} ms (the projection build and one run, EXCLUDED from Tp){}",
            k.reference,
            warmup_ms,
            if warmup_breached { " TIM (four ceilings)" } else { "" }
        );
        if warmup_breached {
            let validation = "not validated: the warm-up breached four ceilings";
            failures.push(format!("{}: tim — {validation}", k.reference));
            rows.push(format!(
                "    {{\"kernel\": \"{}\", \"status\": \"tim\", \"reps\": 0, \"tp_mode\": \"{}\", \
                 \"warmup_ms\": {warmup_ms:.3}, \"tp_ms_mean\": null, \"tp_ms_each\": [], \
                 \"validation_ms\": 0.000, \"tim\": true, \"mean_is_lower_bound\": true, \
                 \"evps\": null, \"rows\": 0, \"match_mode\": \"{:?}\", \"validation\": \"{validation}\"}}",
                k.reference,
                if tp_stats { "stats" } else { "stream" },
                k.mode,
            ));
            eprintln!("[graphalytics] {:<5} tim  {validation}", k.reference);
            continue;
        }

        let mut samples: Vec<Rep> = Vec::new();
        let mut last: Option<BTreeMap<u64, String>> = None;
        for r in 0..reps {
            let (out, ms, breached) = timed(&addr, &timed_stmt, ceiling);
            let rows_n = out.as_ref().map_or(0, Vec::len);
            if let (false, Some(o)) = (tp_stats, out) {
                last = Some(values_of(&o));
            }
            eprintln!(
                "[graphalytics] {:<5} rep {}/{reps} {:>9.0} ms {}",
                k.reference,
                r + 1,
                ms,
                if breached { "TIM (ceiling)" } else { "" }
            );
            samples.push(Rep {
                tp_ms: ms,
                breached,
                rows: rows_n,
            });
            // The job is TIM whatever the rest would do, and the breached
            // statement is still running in the server: another repetition
            // would only pile a second copy on top of it.
            if breached {
                eprintln!(
                    "[graphalytics] {:<5} the remaining {} repetition(s) are not issued: \
                     the job is TIM, and the breached run is still computing in the server",
                    k.reference,
                    reps - r - 1
                );
                break;
            }
        }

        // ── The validating stream, when the repetitions did not stream ───
        let mut validation_ms = 0.0;
        let mut stream_rows = samples.last().map_or(0, |s| s.rows);
        let mut validation_breach = false;
        if tp_stats && !samples.iter().any(|s| s.breached) {
            let (out, ms, breached) = timed(&addr, &stmt, ceiling);
            validation_ms = ms;
            validation_breach = breached;
            stream_rows = out.as_ref().map_or(0, Vec::len);
            if let Some(o) = out {
                last = Some(values_of(&o));
            }
            eprintln!(
                "[graphalytics] {:<5} validating stream {:>9.0} ms, {} row(s){}",
                k.reference,
                ms,
                stream_rows,
                if breached { " TIM (ceiling)" } else { "" }
            );
        }

        // ── The metric ───────────────────────────────────────────────────
        //
        // Mean of Tp, then the rate — NEVER the mean of per-run rates. §4.2.1
        // says so in both directions, and the two differ whenever the runs do.
        let mean_tp = samples.iter().map(|s| s.tp_ms).sum::<f64>() / samples.len() as f64;
        let any_breach = samples.iter().any(|s| s.breached) || validation_breach;
        let evps = if mean_tp > 0.0 {
            (v + e) as f64 / (mean_tp / 1000.0)
        } else {
            0.0
        };

        let verdict = if any_breach {
            None
        } else {
            last.as_ref().map(|got| compare(k.mode, &reference, got))
        };
        let validation = if any_breach {
            "not validated: a repetition breached the ceiling".to_string()
        } else {
            match &verdict {
                Some(v) => v.detail.clone(),
                None => "no output returned".to_string(),
            }
        };
        let status = if any_breach {
            "tim"
        } else {
            match &verdict {
                None => "error",
                Some(v) if v.agrees => "ok",
                Some(_) => "mismatch",
            }
        };
        if status != "ok" {
            failures.push(format!("{}: {status} — {validation}", k.reference));
        }
        eprintln!(
            "[graphalytics] {:<5} mean {:>9.0} ms  {:>12.0} EVPS  {status}  {validation}",
            k.reference, mean_tp, evps
        );

        rows.push(format!(
            "    {{\"kernel\": \"{}\", \"status\": \"{status}\", \
             \"reps\": {}, \"tp_mode\": \"{}\", \"warmup_ms\": {warmup_ms:.3}, \
             \"tp_ms_mean\": {:.3}, \"tp_ms_each\": [{}], \
             \"validation_ms\": {:.3}, \
             \"tim\": {any_breach}, \"mean_is_lower_bound\": {any_breach}, \
             \"evps\": {evps:.1}, \"rows\": {}, \"match_mode\": \"{:?}\", \
             \"validation\": \"{}\"}}",
            k.reference,
            samples.len(),
            if tp_stats { "stats" } else { "stream" },
            mean_tp,
            samples
                .iter()
                .map(|s| format!("{:.3}", s.tp_ms))
                .collect::<Vec<_>>()
                .join(", "),
            validation_ms,
            stream_rows,
            k.mode,
            validation.replace('"', "'")
        ));
    }

    let doc = format!(
        "{{\n  \"workload\": \"graphalytics\",\n  \"engine\": \"{engine}\",\n  \
         \"engine_version\": \"{version}\",\n  \"graph\": \"{name}\",\n  \
         \"scale\": \"ga-{name}\",\n  \"ldbc_scale_letter\": \"{scale}\",\n  \
         \"vertices\": {v},\n  \"edges\": {e},\n  \"tl_ms\": {tl_ms:.3},\n  \
         \"tl_note\": \"census of an already-resident corpus, EXCLUDED from every Tp per spec 2.5.3\",\n  \
         \"ceiling_secs\": {},\n  \"repetitions\": {reps},\n  \
         \"graphalytics_semantics\": true,\n  \"kernels\": [\n{}\n  ],\n  \
         \"failures\": [{}]\n}}\n",
        ceiling.as_secs(),
        rows.join(",\n"),
        failures
            .iter()
            .map(|f| format!("\"{}\"", f.replace('"', "'")))
            .collect::<Vec<_>>()
            .join(", ")
    );

    match flag(&args, "--json") {
        Some(p) => {
            if let Err(err) = std::fs::write(p, &doc) {
                eprintln!("[graphalytics] cannot write {p}: {err}");
                std::process::exit(1);
            }
            eprintln!("[graphalytics] report written to {p}");
        }
        None => print!("{doc}"),
    }

    if failures.is_empty() {
        eprintln!("\nPASS — {} kernel(s), every one validated", rows.len());
    } else {
        eprintln!("\nFAIL");
        for f in &failures {
            eprintln!("   - {f}");
        }
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{Match, ceiling_secs, compare, is_inf};
    use std::collections::BTreeMap;

    fn m(pairs: &[(u64, &str)]) -> BTreeMap<u64, String> {
        pairs.iter().map(|(k, v)| (*k, (*v).to_string())).collect()
    }

    #[test]
    fn the_graphs_own_params_file_parses_into_every_key() {
        // `ga2jsonl` writes this shape. A first cut read it by splitting on
        // quotes and taking every fourth field, which mis-pairs the moment a
        // value is UNQUOTED — `"directed":true` shifted every later key by
        // one, so `sssp.weight-property` came back EMPTY and SSSP was skipped
        // as "no weight property declared" on a graph that declares one, and
        // `bfs.source-vertex` came back empty so BFS was issued as
        // `{vid: }` and failed outright.
        let text = r#"{"graph":"example-directed","directed":true,"algorithms":"bfs, cdlp",
          "bfs.source-vertex":"1","pr.damping-factor":"0.85",
          "sssp.weight-property":"weight","sssp.source-vertex":"1"}"#;
        let v = engram_cypher::json::from_json(text).expect("params.json must parse");
        let engram_cypher::Value::Map(m) = v else {
            panic!("params.json is an object");
        };
        assert_eq!(
            m.get("sssp.weight-property"),
            Some(&engram_cypher::Value::Str("weight".into())),
            "the weight property must survive an unquoted neighbour"
        );
        assert_eq!(
            m.get("bfs.source-vertex"),
            Some(&engram_cypher::Value::Str("1".into()))
        );
        assert_eq!(m.get("directed"), Some(&engram_cypher::Value::Bool(true)));
    }

    #[test]
    fn a_total_mismatch_is_never_reported_as_agreement() {
        // THE BUG THIS PINS, measured 2026-09-21. The first cut of this runner
        // decided its status by searching the printed line for "DIFFERS" and
        // "NOT returned". `0/10 within 1e-4 relative` contains neither, so
        // PageRank and LCC were reported `ok` having matched NOTHING — the
        // validator agreeing with itself about a comparison that failed
        // completely.
        //
        // Agreement is now counted, not read.
        let r = m(&[(1, "1.0"), (2, "2.0")]);
        let nothing_matches = compare(Match::Epsilon, &r, &m(&[(1, "9.0"), (2, "9.0")]));
        assert!(!nothing_matches.agrees, "{nothing_matches:?}");
        assert!(
            nothing_matches.detail.contains("0/2"),
            "{nothing_matches:?}"
        );

        // And the same for an exact-match kernel.
        let ex = compare(Match::Exact, &r, &m(&[(1, "9"), (2, "9")]));
        assert!(!ex.agrees, "{ex:?}");

        // An EMPTY comparison is not agreement either: a kernel that returned
        // nothing against a reference that lists nothing must not pass.
        let empty = compare(Match::Exact, &BTreeMap::new(), &BTreeMap::new());
        assert!(
            !empty.agrees,
            "an empty comparison proves nothing: {empty:?}"
        );
    }

    #[test]
    fn the_per_scale_ceilings_are_the_specs_table() {
        assert_eq!(ceiling_secs("S"), 900);
        assert_eq!(ceiling_secs("M"), 1800);
        assert_eq!(ceiling_secs("L"), 3600);
        assert_eq!(ceiling_secs("XL"), 7200);
        assert_eq!(ceiling_secs("2XL"), 10800);
        // An unknown letter takes the SMALLEST, so an overrun is visible
        // rather than sitting silently under a three-hour default.
        assert_eq!(ceiling_secs("banana"), 900);
    }

    #[test]
    fn a_zero_reference_value_admits_only_an_exact_zero() {
        // The spec's bound is |r - s| <= 1e-4*|r|. At r = 0 it collapses, and
        // for LCC that is every vertex of degree 0 or 1 — the most numerous
        // rows in the file.
        let r = m(&[(1, "0.0"), (2, "0.5")]);
        let exact = compare(Match::Epsilon, &r, &m(&[(1, "0.0"), (2, "0.5")]));
        assert!(exact.agrees, "{exact:?}");
        assert!(exact.detail.contains("2/2"), "{exact:?}");
        assert!(exact.detail.contains("all exact"), "{exact:?}");

        let near = compare(Match::Epsilon, &r, &m(&[(1, "0.000000001"), (2, "0.5")]));
        assert!(!near.agrees, "a near-zero must NOT pass: {near:?}");
        assert!(near.detail.contains("NOT exactly zero"), "{near:?}");
    }

    #[test]
    fn the_epsilon_is_relative_one_in_ten_thousand() {
        let r = m(&[(1, "1000.0")]);
        // 0.05 is 5e-5 relative — inside the bound.
        assert!(compare(Match::Epsilon, &r, &m(&[(1, "1000.05")])).agrees);
        // 0.5 is 5e-4 relative — outside it.
        assert!(!compare(Match::Epsilon, &r, &m(&[(1, "1000.5")])).agrees);
    }

    #[test]
    fn an_unreachable_vertex_answered_as_zero_is_not_a_match() {
        // THE TRAP. `infinity` and `0` both parse to 0.0 in a naive reader, so
        // a distance-zero answer for an unreachable vertex would score as a
        // match and SSSP would validate against nothing.
        assert!(is_inf("infinity") && is_inf("inf") && is_inf("INF"));
        assert!(!is_inf("0") && !is_inf("0.0"));
        let r = m(&[(1, "infinity"), (2, "2.5")]);
        let good = compare(Match::Epsilon, &r, &m(&[(1, "inf"), (2, "2.5")]));
        assert!(good.agrees, "inf and infinity are one answer: {good:?}");
        let bad = compare(Match::Epsilon, &r, &m(&[(1, "0"), (2, "2.5")]));
        assert!(!bad.agrees, "zero must not match infinity: {bad:?}");
    }

    #[test]
    fn wcc_is_judged_on_the_partition_not_the_labels() {
        // Component ids are arbitrary; only the partition they induce matters.
        let r = m(&[(1, "0"), (2, "0"), (3, "7")]);
        let relabelled = compare(Match::Equivalence, &r, &m(&[(1, "5"), (2, "5"), (3, "9")]));
        assert!(relabelled.agrees, "{relabelled:?}");
        // But merging two reference components is a real difference.
        let merged = compare(Match::Equivalence, &r, &m(&[(1, "5"), (2, "5"), (3, "5")]));
        assert!(!merged.agrees, "{merged:?}");
    }

    #[test]
    fn a_vertex_the_kernel_never_returned_is_counted_not_ignored() {
        let r = m(&[(1, "1"), (2, "2")]);
        let out = compare(Match::Exact, &r, &m(&[(1, "1")]));
        assert!(!out.agrees, "{out:?}");
        assert!(out.detail.contains("NOT returned"), "{out:?}");
    }
}

/// The catalogue and this binary must describe the SAME six kernels.
///
/// A catalogue nothing checks against the code is decoration: it would keep
/// stamping a digest while the lane ran something else, which is precisely the
/// claim the digest exists to make checkable. This is the check.
#[cfg(test)]
mod catalogue_agrees_with_the_kernels_tests {
    use super::{KERNELS, Match};
    use engram_cypher::Value;
    use engram_cypher::json::from_json;

    fn catalogue_kernels() -> std::collections::BTreeMap<String, Value> {
        let fam = engram_bench::catalogue::family("graphalytics")
            .expect("the graphalytics family is compiled in");
        let doc = match from_json(fam.source) {
            Ok(Value::Map(m)) => m,
            other => panic!("the graphalytics catalogue is not an object: {other:?}"),
        };
        match doc.get("kernels") {
            Some(Value::Map(m)) => m.clone(),
            other => panic!("`kernels` is not an object: {other:?}"),
        }
    }

    fn field(k: &Value, name: &str) -> String {
        match k {
            Value::Map(m) => match m.get(name) {
                Some(Value::Str(s)) => s.clone(),
                other => panic!("{name} is not a string: {other:?}"),
            },
            other => panic!("kernel is not an object: {other:?}"),
        }
    }

    #[test]
    fn the_catalogue_declares_exactly_the_kernels_this_binary_runs() {
        let cat = catalogue_kernels();
        let mut declared: Vec<&str> = cat.keys().map(String::as_str).collect();
        declared.sort_unstable();
        let mut running: Vec<&str> = KERNELS.iter().map(|k| k.reference).collect();
        running.sort_unstable();
        assert_eq!(
            declared, running,
            "the catalogue and the KERNELS table disagree about which kernels exist"
        );
    }

    #[test]
    fn every_kernel_agrees_on_its_procedure_yield_and_match_mode() {
        let cat = catalogue_kernels();
        for k in KERNELS {
            let entry = cat
                .get(k.reference)
                .unwrap_or_else(|| panic!("{} is not in the catalogue", k.reference));
            assert_eq!(
                field(entry, "procedure"),
                format!("engram.algo.{}", k.procedure),
                "{} runs a different procedure than the catalogue declares",
                k.reference
            );
            assert_eq!(
                field(entry, "yields"),
                k.yields,
                "{} reads a different YIELD field than the catalogue declares",
                k.reference
            );
            // The match MODE decides whether a run PASSES, so a drift here is
            // the one that would silently change a verdict.
            let want = match k.mode {
                Match::Exact => "exact",
                Match::Epsilon => "epsilon",
                Match::Equivalence => "equivalence",
            };
            assert_eq!(
                field(entry, "match"),
                want,
                "{} is judged by a different rule than the catalogue declares",
                k.reference
            );
        }
    }
}

#[cfg(test)]
mod stats_statement_tests {
    use super::stats_statement;

    #[test]
    fn a_stream_call_becomes_its_stats_call() {
        assert_eq!(
            stats_statement(
                "MATCH (s:Vertex {vid: 7}) CALL engram.algo.bfs.stream({nodeLabels: ['Vertex'], \
                 sourceNode: id(s)}) YIELD node, depth RETURN node.vid AS v, depth AS x"
            ),
            "MATCH (s:Vertex {vid: 7}) CALL engram.algo.bfs.stats({nodeLabels: ['Vertex'], \
             sourceNode: id(s)}) YIELD nodeCount RETURN nodeCount"
        );
        assert_eq!(
            stats_statement(
                "CALL engram.algo.labelpropagation.stream({maxIterations: 10}) YIELD node, \
                 communityId MATCH (c:Vertex) WHERE id(c) = communityId RETURN node.vid AS v, c.vid AS x"
            ),
            "CALL engram.algo.labelpropagation.stats({maxIterations: 10}) YIELD nodeCount RETURN nodeCount"
        );
    }
}

#[cfg(test)]
mod kernel_filter_tests {
    use super::kernel_filter;

    #[test]
    fn no_flag_runs_every_kernel() {
        assert_eq!(kernel_filter(None), Ok(None));
    }

    #[test]
    fn names_are_matched_in_any_case_and_order() {
        assert_eq!(
            kernel_filter(Some("lcc, BFS")),
            Ok(Some(vec!["LCC".to_string(), "BFS".to_string()]))
        );
    }

    #[test]
    fn an_unknown_name_is_refused_not_dropped() {
        assert!(kernel_filter(Some("BFS,TRIANGLES")).unwrap_err().contains("TRIANGLES"));
        assert!(kernel_filter(Some(" , ")).is_err());
    }
}
