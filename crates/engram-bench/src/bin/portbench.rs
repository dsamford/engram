//! The full-database port benchmark — the engine loaded with an ENTIRE
//! graph export, running the same statements the incumbent ran, timed
//! on the same hardware.
//!
//! Inputs (a directory):
//!   - `nodes.jsonl`  — one node per line: `{"i": id, "l": [labels], "p": props}`
//!   - `rels.jsonl`   — one rel per line: `{"s": src, "d": dst, "t": type, "p": props}`
//!   - `meta.json`    — `{captured_at, nodes, rels}`
//!   - `statements.json` — the benchmark set, each with the flags the
//!     decoded-values run defined
//!   - optionally `neo4j-results.json` — the incumbent's captured rows and
//!     SERVER-side timings for the same statements; when present, rows are
//!     compared with the shared canonicalisation rules and the report
//!     carries both engines' numbers side by side.
//!
//! Timing discipline: per statement, one untimed correctness run (rows
//! captured up to the same 5,000-row cap the incumbent capture used), then
//! N timed runs (default 5) — median reported, all samples kept. A first
//! run over 10s reduces N to 2 rather than silently skipping the query: a
//! benchmark that drops its slowest members reports a faster engine than
//! exists. Engram is timed IN-PROCESS; the incumbent's numbers are the
//! driver's server-side `resultAvailableAfter + resultConsumedAfter`, so
//! transport is excluded on BOTH sides.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

// musl's mallocng degrades catastrophically under this load's allocation
// churn (measured: ~2.5 ms per 116-byte node create at a 3 GB heap, a
// ~100× collapse from the start of the run). Match the PRODUCTION allocator
// (`engram-server`, `portserve`): mimalloc, so a benchmark measures the same
// heap the deployed server runs on — the columnar hash-join paths (IC5's
// chainB expansion) are allocation-heavy, and mimalloc's thread-caching
// roughly HALVES their latency vs dlmalloc (IC5: 191 ms dlmalloc → ~92 ms
// mimalloc). On every non-musl target this declaration compiles away.
#[cfg(target_env = "musl")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::collections::BTreeMap;
use std::time::Instant;

use engram_bench::{
    canon_engram, canon_incumbent, get_bool, get_list, get_str, tie_at_limit_boundary,
};
use engram_cypher::{Value, json, parse_any};
use engram_graph::{Graph, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

const ROW_CAP: usize = 5_000;

/// Resident set in MB, from /proc — 0 where /proc is absent. The two pod
/// OOM kills before this existed left NOTHING to diagnose; a peak that
/// prints as it grows is the difference between a fix and a guess.
fn rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| {
                l.split_whitespace()
                    .nth(1)
                    .and_then(|kb| kb.parse::<u64>().ok())
            })
        })
        .map_or(0, |kb| kb / 1024)
}

/// Append one statement's result to the incremental JSONL (flushed — a
/// death must not lose it) and to the in-memory aggregate.
fn append_result(
    jsonl: &mut std::fs::File,
    results: &mut Vec<Value>,
    rep: BTreeMap<String, Value>,
) {
    use std::io::Write;
    let v = Value::Map(rep);
    let line = json::to_json(&v);
    let _ = jsonl.write_all(line.as_bytes());
    let _ = jsonl.write_all(b"\n");
    let _ = jsonl.flush();
    results.push(v);
}

fn main() {
    let dir = std::path::PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "port".to_string()),
    );
    let out_path = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "port-report.json".to_string());
    let iters: u32 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    // Statements per invocation. The allocator keeps its high-water mark, so
    // one process running EVERY statement accumulates the worst peaks of all
    // of them; a bounded chunk per process, resumed by the next invocation,
    // pays a world reload instead — measured as the cheaper side.
    let max_new: usize = std::env::args()
        .nth(4)
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);

    // ── Load the world ──────────────────────────────────────────────────
    // ENGRAM_PORT_OPEN_PAGED=<dir>:<cache_bytes>: the M3 bigger-than-RAM proof —
    // OPEN a previously-persisted paged store (bounded open: only per-segment
    // footer+index anchors) and run the corpus WITHOUT ever loading the graph
    // resident. Steady-state RSS is bounded by the cache + anchors, not the
    // graph. (Persist first with a normal ENGRAM_PORT_PAGED run.)
    let open_paged: Option<(String, usize)> = std::env::var("ENGRAM_PORT_OPEN_PAGED")
        .ok()
        .and_then(|spec| {
            let (d, c) = spec.rsplit_once(':')?;
            Some((d.to_string(), c.parse().ok()?))
        });
    let graph = if let Some((segdir, cache_bytes)) = &open_paged {
        let (store, cache) =
            engram_store::Store::open_paged_dir(std::path::Path::new(segdir), *cache_bytes)
                .expect("open_paged_dir");
        std::mem::forget(cache); // keep the shared cache alive for the whole run
        eprintln!(
            "[port] OPENED PAGED from {segdir} with {} MB cache — NO resident load (rss {} MB)",
            cache_bytes / 1024 / 1024,
            rss_mb()
        );
        Graph::new(store, Realm(1), Namespace(1))
    } else {
        Graph::new(Store::new(), Realm(1), Namespace(1))
    };
    if std::env::var("ENGRAM_PORT_NO_LATE").is_ok() {
        graph.set_late_projection(false);
    }
    if std::env::var("ENGRAM_PORT_NO_SEEK").is_ok() {
        graph.set_property_seek(false);
        eprintln!("[port] property seek DISABLED by env");
    }
    if std::env::var("ENGRAM_PORT_NO_FRONTIER").is_ok() {
        graph.set_frontier_expand(false);
        eprintln!("[port] frontier-BFS var-length DISABLED by env");
    }
    if let Ok(v) = std::env::var("ENGRAM_PORT_DEGREE_TABLE_AFTER") {
        if let Ok(n) = v.parse::<u64>() {
            graph.set_degree_table_after(n);
            eprintln!("[port] degree_table_after set to {n} by env");
        }
    }
    if let Ok(v) = std::env::var("ENGRAM_PORT_COLUMN_BUDGET_FACTOR") {
        if let Ok(n) = v.parse::<usize>() {
            graph.set_columnar_column_budget_factor(n);
            eprintln!("[port] columnar_column_budget_factor set to {n} by env");
        }
    }
    if std::env::var("ENGRAM_PORT_NO_COLUMNAR").is_ok() {
        graph.set_columnar_scans(false);
        eprintln!("[port] columnar scans DISABLED by env");
    }
    if std::env::var("ENGRAM_PORT_NO_DEGREE").is_ok() {
        graph.set_degree_aggregate(false);
        eprintln!("[port] degree short-circuit DISABLED by env");
    }
    if std::env::var("ENGRAM_PORT_NO_IC2").is_ok() {
        graph.set_ic2_ordered(false);
        eprintln!("[port] IC2 ordered merge DISABLED by env");
    }
    if std::env::var("ENGRAM_PORT_NO_IC11").is_ok() {
        graph.set_ic11_semijoin(false);
        eprintln!("[port] IC11 semijoin DISABLED by env");
    }
    if std::env::var("ENGRAM_PORT_NO_BI7").is_ok() {
        graph.set_bi7_rollup(false);
        eprintln!("[port] BI7 rollup DISABLED by env");
    }
    if std::env::var("ENGRAM_PORT_NO_IC3").is_ok() {
        graph.set_ic3_datewindow(false);
        eprintln!("[port] IC3 datewindow DISABLED by env");
    }
    if std::env::var("ENGRAM_PORT_NO_MS_BATCH").is_ok() {
        graph.set_multistage_topk_batch(false);
        eprintln!("[port] multi-stage top-k batching DISABLED by env");
    }
    // In OPEN-PAGED mode the world is already on disk — skip loading, sealing,
    // compacting and converting; the store is paged from the start.
    let (n_nodes, n_rels, load_ms, blocks, block_rows) = if open_paged.is_some() {
        eprintln!(
            "[port] open-paged: {} sealed segments on disk, ready (rss {} MB)",
            graph.shared_store().segment_count(),
            rss_mb()
        );
        (0u64, 0u64, 0u128, 0usize, 0usize)
    } else {
        let stats = engram_bench::load_export(&graph, &dir);
        eprintln!(
            "[port] world loaded: {} nodes, {} rels ({} dangling, {} unloadable props) in {} ms",
            stats.nodes, stats.rels, stats.dangling, stats.unloadable, stats.load_ms
        );
        eprintln!("[port] load complete (rss {} MB)", rss_mb());
        // Compact once so the head/tail layout is in play — the store a real
        // deployment would be reading. ENGRAM_PORT_NO_COMPACT=1 skips it.
        let (blocks, block_rows) = if std::env::var("ENGRAM_PORT_NO_COMPACT").is_ok() {
            eprintln!("[port] compaction SKIPPED by env — row form throughout");
            graph.shared_store().seal();
            (0, 0)
        } else {
            let compact_started = Instant::now();
            graph.shared_store().seal();
            graph.shared_store().compact();
            let cs = graph.shared_store().columnar_stats();
            eprintln!(
                "[port] compacted in {} ms: {} column blocks holding {} rows (rss {} MB)",
                compact_started.elapsed().as_millis(),
                cs.0,
                cs.1,
                rss_mb()
            );
            cs
        };
        (stats.nodes, stats.rels, stats.load_ms, blocks, block_rows)
    };

    // ENGRAM_PORT_PAGED=<cache_bytes>: convert the loaded store to PAGED
    // (Track B) — every sealed segment written to disk and read block-by-block
    // through a bounded cache. Run with a small cache to force fault-in at
    // scale; the DIVERGED verdicts must match a resident run (paged == resident
    // over the whole corpus, the M1 gate). Queries run through a FRESH graph so
    // its adjacency/member caches start cold and reads actually hit disk.
    let graph = if let Ok(v) = std::env::var("ENGRAM_PORT_PAGED") {
        let cache_bytes: usize = v.parse().unwrap_or(64 * 1024 * 1024);
        let paged_dir = std::path::Path::new("/work/engram-paged-seg");
        let _ = std::fs::remove_dir_all(paged_dir);
        std::fs::create_dir_all(paged_dir).expect("mkdir paged");
        let started = Instant::now();
        let cache = graph
            .shared_store()
            .into_paged(paged_dir, cache_bytes)
            .expect("into_paged");
        std::mem::forget(cache); // keep the shared cache alive for the whole run
        // ENGRAM_PORT_PERSIST_IDX: write index-at-seal sidecars beside the segments,
        // so a later ENGRAM_PORT_OPEN_PAGED run SERVES the property index from disk
        // instead of rebuilding it (the index-at-seal at-scale check).
        if std::env::var("ENGRAM_PORT_PERSIST_IDX").is_ok() {
            let props = ["creationDate", "id"];
            let n = graph
                .persist_indexes(paged_dir, &props)
                .expect("persist_indexes");
            eprintln!("[port] persisted {n} index sidecar(s) to {paged_dir:?}");
        }
        let fresh = Graph::new(graph.shared_store(), Realm(1), Namespace(1));
        eprintln!(
            "[port] PAGED: on-disk segments, {} MB cache, converted in {} ms (rss {} MB)",
            cache_bytes / 1024 / 1024,
            started.elapsed().as_millis(),
            rss_mb()
        );
        fresh
    } else {
        graph
    };

    // ── The incumbent's capture, if present ─────────────────────────────
    let neo4j: BTreeMap<String, BTreeMap<String, Value>> =
        std::fs::read_to_string(dir.join("neo4j-results.json"))
            .ok()
            .and_then(|raw| json::from_json(&raw).ok())
            .map(|v| match v {
                Value::List(items) => items
                    .into_iter()
                    .filter_map(|st| match st {
                        Value::Map(m) => Some((
                            format!("{}:{}", get_str(&m, "file"), get_str(&m, "line")),
                            m,
                        )),
                        _ => None,
                    })
                    .collect(),
                _ => BTreeMap::new(),
            })
            .unwrap_or_default();
    eprintln!("[port] incumbent capture: {} statements", neo4j.len());
    // A statement that would materialise the whole cross-product of two big
    // labels must REFUSE, not feed the OOM killer — the first full run died
    // exactly there, silently, taking every completed timing with it.
    let row_budget: usize = std::env::var("ENGRAM_PORT_ROW_BUDGET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000_000);
    eprintln!("[port] row budget: {row_budget}");
    graph.set_row_budget(Some(row_budget));

    // ── The statements ──────────────────────────────────────────────────
    // ENGRAM_PORT_BISECT=1: instead of the benchmark, time the census
    // statement's sub-shapes against the loaded production store, so a
    // wall that the scaled repro cannot reproduce is attributed HERE.
    // ENGRAM_PORT_BISECT_SQL="stmt;;stmt;;…": bisect THOSE statements
    // instead (each timed cold and warm, with its engine counters), so a
    // production-only cost is split where it occurs.
    if let Ok(list) = std::env::var("ENGRAM_PORT_BISECT_SQL") {
        for src in list.split(";;").map(str::trim).filter(|s| !s.is_empty()) {
            let q = match engram_cypher::parse_statement(src) {
                Ok(q) => q,
                Err(e) => {
                    eprintln!("[bisect] parse error for `{src}`: {e:?}");
                    continue;
                }
            };
            for pass in ["cold", "warm"] {
                let t = Instant::now();
                let (r, trace) = engram_observe::with_trace(|| {
                    engram_graph::run_query(&graph, &q, BTreeMap::new())
                });
                let elapsed = t.elapsed();
                match r {
                    Ok(r) => eprintln!(
                        "[bisect] {pass} {elapsed:?} rows {} :: {src}
    counters {:?}
    events {:?}",
                        r.rows.len(),
                        trace
                            .counters()
                            .iter()
                            .filter(|(k, _)| k.starts_with("graph.")
                                || k.starts_with("interp.")
                                || k.starts_with("store."))
                            .collect::<Vec<_>>(),
                        trace
                            .sometimes_hit()
                            .iter()
                            .filter(|k| k.starts_with("interp.") || k.starts_with("graph."))
                            .collect::<Vec<_>>()
                    ),
                    Err(e) => eprintln!("[bisect] {pass} {elapsed:?} ERROR {e:?} :: {src}"),
                }
            }
        }
        return;
    }
    if std::env::var("ENGRAM_PORT_BISECT").is_ok() {
        for (name, src) in [
            (
                "scan-only: MATCH (n) RETURN count(n)",
                "MATCH (n) RETURN count(n)",
            ),
            (
                "scan+probe (no sort): sum of degrees",
                "MATCH (n) WITH n, count { (n)--() } AS d RETURN sum(d)",
            ),
            (
                "scan+probe+sort",
                "MATCH (n) WITH n, count { (n)--() } AS d WITH d ORDER BY d RETURN count(d)",
            ),
            (
                "scan+probe+sort+collect (full census)",
                "MATCH (n) WITH n, count { (n)--() } AS d WITH d ORDER BY d WITH collect(d) AS ds                  RETURN ds[toInteger(size(ds) * 0.50)] AS p50, ds[size(ds) - 1] AS max",
            ),
            (
                "probe only via label-less seed, no WITH: count of degrees>0",
                "MATCH (n) WHERE count { (n)--() } > 0 RETURN count(n)",
            ),
        ] {
            let q = engram_cypher::parse_statement(src).expect("parse");
            let t = Instant::now();
            let (r, trace) =
                engram_observe::with_trace(|| engram_graph::run_query(&graph, &q, BTreeMap::new()));
            let r = r.expect("run");
            eprintln!(
                "[bisect] {name}: {:?}  rows {:?}  counters {:?}",
                t.elapsed(),
                r.rows.first(),
                trace
                    .counters()
                    .iter()
                    .filter(|(k, _)| k.starts_with("graph.") || k.starts_with("interp."))
                    .collect::<Vec<_>>()
            );
        }
        return;
    }
    let stmts_raw = std::fs::read_to_string(dir.join("statements.json")).expect("statements.json");
    let Ok(Value::List(stmts)) = json::from_json(&stmts_raw) else {
        panic!("statements.json is not a list");
    };

    // Incremental results: one JSON line per completed statement, appended
    // as it lands, so a death preserves every timing before it — and a
    // relaunch RESUMES past them instead of repaying the whole run.
    let jsonl_path = dir.join("port-results.jsonl");
    let mut results: Vec<Value> = Vec::new();
    let mut done_keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    if let Ok(prior) = std::fs::read_to_string(&jsonl_path) {
        for line in prior.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(v) = json::from_json(line) {
                if let Value::Map(m) = &v {
                    done_keys.insert(get_str(m, "key"));
                }
                results.push(v);
            }
        }
        if !results.is_empty() {
            eprintln!(
                "[port] resuming past {} completed statements",
                results.len()
            );
        }
    }
    let mut jsonl = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&jsonl_path)
        .expect("open results jsonl");
    let total_stmts = stmts.len();
    let mut stmt_no = 0usize;
    let mut fresh_done = 0usize;
    for st in &stmts {
        let Value::Map(st) = st else { continue };
        let text = get_str(st, "text");
        let key = format!("{}:{}", get_str(st, "file"), get_str(st, "line"));
        stmt_no += 1;
        if done_keys.contains(&key) {
            continue; // resumed from a prior run
        }
        if fresh_done >= max_new {
            eprintln!("[port] chunk limit {max_new} reached — exiting for a fresh heap");
            std::process::exit(3); // deliberate: the loop re-invokes to resume
        }
        fresh_done += 1;
        eprintln!(
            "[port] stmt {stmt_no}/{total_stmts} (rss {} MB): {key}",
            rss_mb()
        );
        let mut rep: BTreeMap<String, Value> = BTreeMap::new();
        rep.insert("key".into(), Value::Str(key.clone()));

        let stmt = match parse_any(&text) {
            Ok(s) => s,
            Err(e) => {
                rep.insert("error".into(), Value::Str(format!("parse: {e}")));
                append_result(&mut jsonl, &mut results, rep);
                continue;
            }
        };
        // Correctness run (untimed), rows capped exactly like the capture.
        let first = match run_stmt(&graph, &stmt, BTreeMap::new()) {
            Ok(r) => r,
            Err(e) => {
                rep.insert("error".into(), Value::Str(format!("run: {e:?}")));
                append_result(&mut jsonl, &mut results, rep);
                continue;
            }
        };
        let truncated = first.rows.len() > ROW_CAP;
        rep.insert("rows".into(), Value::Int(first.rows.len() as i64));
        rep.insert("truncated".into(), Value::Bool(truncated));

        // Compare against the incumbent when its capture holds this key.
        if let Some(inc) = neo4j.get(&key) {
            let verdict = if inc.contains_key("error") || truncated || get_bool(inc, "truncated") {
                "not_comparable"
            } else {
                let inc_rows = get_list(inc, "rows");
                if get_bool(st, "order_dependent") || get_bool(st, "id_space") {
                    if inc_rows.len() == first.rows.len() {
                        "count_match"
                    } else {
                        "diverged_count"
                    }
                } else {
                    // Canonicalise each COLUMN of a row (which sorts nested
                    // collect() lists — an order-unspecified aggregate — so those
                    // compare as multisets) but NEVER sort the row's COLUMNS: both
                    // engines emit columns in RETURN order, so the comparison must
                    // preserve it. Passing a whole incumbent row to
                    // `canon_incumbent` treated the row itself as a list and sorted
                    // its columns, while the engram side (per-column map, below)
                    // did not — an ASYMMETRY that false-flagged every projection
                    // whose RETURN order was not already value-sorted (13 of 16 on
                    // SF1). Build both sides the SAME way: map the per-value canon
                    // over the columns, wrap in a List, leave column order intact.
                    let mut a: Vec<(String, Value)> = inc_rows
                        .iter()
                        .map(|r| {
                            let v = match r {
                                Value::List(cols) => {
                                    Value::List(cols.iter().map(canon_incumbent).collect())
                                }
                                other => canon_incumbent(other),
                            };
                            (json::to_json(&v), v)
                        })
                        .collect();
                    let mut b: Vec<(String, Value)> = first
                        .rows
                        .iter()
                        .map(|row| {
                            let v = Value::List(row.iter().map(canon_engram).collect());
                            (json::to_json(&v), v)
                        })
                        .collect();
                    a.sort_by(|x, y| x.0.cmp(&y.0));
                    b.sort_by(|x, y| x.0.cmp(&y.0));
                    if a.iter().map(|(s, _)| s).eq(b.iter().map(|(s, _)| s)) {
                        "identical"
                    } else {
                        let only_a: Vec<&Value> = a
                            .iter()
                            .filter(|(s, _)| !b.iter().any(|(t, _)| t == s))
                            .map(|(_, v)| v)
                            .collect();
                        let only_b: Vec<&Value> = b
                            .iter()
                            .filter(|(s, _)| !a.iter().any(|(t, _)| t == s))
                            .map(|(_, v)| v)
                            .collect();
                        if tie_at_limit_boundary(&text, &first.columns, &only_a, &only_b) {
                            "tie_boundary"
                        } else {
                            rep.insert(
                                "divergence".into(),
                                Value::Str(format!(
                                    "{} vs {} rows; only-neo4j {:?}; only-engram {:?}",
                                    a.len(),
                                    b.len(),
                                    only_a
                                        .iter()
                                        .take(1)
                                        .map(|v| json::to_json(v)
                                            .chars()
                                            .take(200)
                                            .collect::<String>())
                                        .collect::<Vec<_>>(),
                                    only_b
                                        .iter()
                                        .take(1)
                                        .map(|v| json::to_json(v)
                                            .chars()
                                            .take(200)
                                            .collect::<String>())
                                        .collect::<Vec<_>>(),
                                )),
                            );
                            "diverged"
                        }
                    }
                }
            };
            rep.insert("verdict".into(), Value::Str(verdict.to_string()));
            if let Some(ms) = inc.get("server_ms_median") {
                rep.insert("neo4j_server_ms".into(), ms.clone());
            }
        }

        // Timed runs. A >10s first run reduces the sample count, never to 0.
        let probe = Instant::now();
        let _ = run_stmt(&graph, &stmt, BTreeMap::new()).expect("timed run");
        let probe_ns = probe.elapsed().as_nanos() as i64;
        let n_runs = if probe_ns > 10_000_000_000 {
            1
        } else {
            iters.max(1) - 1
        };
        let mut samples: Vec<i64> = vec![probe_ns];
        for _ in 0..n_runs {
            let t = Instant::now();
            let _ = run_stmt(&graph, &stmt, BTreeMap::new()).expect("timed run");
            samples.push(t.elapsed().as_nanos() as i64);
        }
        samples.sort_unstable();
        let median = samples[samples.len() / 2];
        rep.insert("engram_ns_median".into(), Value::Int(median));
        rep.insert(
            "engram_ms_median".into(),
            Value::Float(median as f64 / 1_000_000.0),
        );
        rep.insert(
            "samples_ns".into(),
            Value::List(samples.into_iter().map(Value::Int).collect()),
        );
        append_result(&mut jsonl, &mut results, rep);
    }

    let count_verdict = |v: &str| -> usize {
        results
            .iter()
            .filter(|r| matches!(r, Value::Map(m) if get_str(m, "verdict") == v))
            .count()
    };
    let identical = count_verdict("identical");
    let count_match = count_verdict("count_match");
    let tie = count_verdict("tie_boundary");
    let diverged = count_verdict("diverged") + count_verdict("diverged_count");
    let skipped_cmp = count_verdict("not_comparable");
    let count_err_kind = |prefix: &str| -> usize {
        results
            .iter()
            .filter(|r| matches!(r, Value::Map(m) if get_str(m, "error").starts_with(prefix)))
            .count()
    };
    let parse_errors = count_err_kind("parse:");
    let run_errors = count_err_kind("run:");
    println!("== port benchmark ==");
    println!(
        "world: {n_nodes} nodes, {n_rels} rels, load {load_ms} ms, {blocks} blocks / {block_rows} rows"
    );
    println!("statements: {}", results.len());
    println!("  identical:      {identical}");
    println!("  count-match:    {count_match}");
    println!("  tie-boundary:   {tie}");
    println!("  DIVERGED:       {diverged}");
    println!("  not comparable: {skipped_cmp}");
    println!("  parse errors:   {parse_errors}");
    println!("  run errors:     {run_errors}");

    let mut doc = BTreeMap::new();
    doc.insert("nodes".to_string(), Value::Int(n_nodes as i64));
    doc.insert("rels".to_string(), Value::Int(n_rels as i64));
    doc.insert("load_ms".to_string(), Value::Int(load_ms as i64));
    doc.insert("blocks".to_string(), Value::Int(blocks as i64));
    doc.insert("block_rows".to_string(), Value::Int(block_rows as i64));
    doc.insert("identical".to_string(), Value::Int(identical as i64));
    doc.insert("count_match".to_string(), Value::Int(count_match as i64));
    doc.insert("tie_boundary".to_string(), Value::Int(tie as i64));
    doc.insert("diverged".to_string(), Value::Int(diverged as i64));
    doc.insert("not_comparable".to_string(), Value::Int(skipped_cmp as i64));
    doc.insert("parse_errors".to_string(), Value::Int(parse_errors as i64));
    doc.insert("run_errors".to_string(), Value::Int(run_errors as i64));
    doc.insert("results".to_string(), Value::List(results));
    std::fs::write(&out_path, json::to_json(&Value::Map(doc))).expect("write report");
    eprintln!("[port] report written to {out_path}");
}
