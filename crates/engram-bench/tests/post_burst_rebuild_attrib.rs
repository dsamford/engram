//! Post-burst derived-structure REBUILD attribution (scratch repro, local-only).
//!
//! The pod's SF1 paged sweep (`measurements/official-sf1-paged-2026-08-29.md`)
//! shows the `contention` profile at 1 client doing 12 reads + 11 writes in
//! ~25 s with ONE ~25 s operation, in both arms, right after the two
//! `write-only` levels (~42k `CREATE (m:Message:Comment)-[:HAS_CREATOR]->(p)`
//! statements with no read in between). This test reproduces the sequence
//! in-process on an `snbgen` corpus and attributes the stall with
//! `engram_observe::with_trace` counter tallies plus per-statement wall time:
//! which statement pays, what it rebuilt, and how the cost moves with the
//! corpus and the burst size.
//!
//! GATED on `ENGRAM_BURST_ATTRIB_DIR` (an `snbgen` export dir); a no-op
//! without it. Knobs:
//!   ENGRAM_BURST_ATTRIB_N=<burst statements>        (default 5000)
//!   ENGRAM_BURST_ATTRIB_PAGED=<block cache MB>       (paged store, like the pod)
//!   ENGRAM_BURST_ATTRIB_SET_FIRST=1                  (hot SET before any read)
//!   ENGRAM_BURST_ATTRIB_CHURN_THREADS=1,8            (delete-churn 1-vs-N census)
//!   ENGRAM_BURST_ATTRIB_LEVERS=0                     (the three post-burst levers OFF:
//!                                                     fixed repair cap, serial fold,
//!                                                     plain rebuild walk — the BEFORE arm)
//!   ENGRAM_BURST_ATTRIB_MAINT=<N>                    (the server's maintenance cadence,
//!                                                     in-process: `refresh_stale_derived`
//!                                                     every N burst writes and once at
//!                                                     the burst's end — the tick)
//! Run with:
//!   cargo run -p engram-bench --bin snbgen --release -- <dir> 1000 1
//!   ENGRAM_BURST_ATTRIB_DIR=<dir> cargo test -p engram-bench --release \
//!     --test post_burst_rebuild_attrib -- --nocapture --test-threads 1
//!
//! Threads and the wall clock are read here to MEASURE (the same waiver the
//! stress harness and `mixed_workload_counters.rs` carry); nothing decides on
//! them.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::time::Instant;

use engram_cypher::{Value, parse_any};
use engram_graph::{Graph, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// Counters that name a derived-structure BUILD, REPAIR or CATCH-UP, or the
/// store work such a rebuild is made of.
const FAMILY: &[&str] = &[
    "graph.adjacency tables built",
    "graph.adjacency tables built in one pass",
    "graph.adjacency tables repaired",
    "graph.adjacency table overlay folded",
    "graph.adjacency tables reused",
    "graph.adjacency tables built by another worker",
    "graph.membership snapshots built",
    "graph.membership snapshots caught up",
    "graph.membership snapshots still current",
    "graph.membership snapshots current",
    "derived.members view folded",
    "derived.members view caught up",
    "derived.members view materialised",
    "derived.change log overflowed",
    "derived.snapshot published",
    "graph.range index builds",
    "graph.range index caught up",
    "graph.range index still current",
    "graph.range index cache hit",
    "index.incremental updates",
    "index.overlay folds",
    "graph.stats rebuilt",
    "store.visitor scans",
    "store.scans",
    "store.block probes",
    "store.gets",
    "paged.pread",
    "paged.block cache hit",
    "paged.block cache miss",
    "store.txn conflicts",
    "store.cas commits",
];

fn run(g: &Graph, src: &str) -> engram_graph::QueryResult {
    let s = parse_any(src).unwrap_or_else(|e| panic!("parse `{src}`: {e}"));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{src}`: {e:?}"))
}

/// One statement, traced and timed. Returns (ms, rows, family tallies).
fn traced(g: &Graph, src: &str) -> (f64, usize, BTreeMap<&'static str, u64>) {
    let t = Instant::now();
    let (res, trace) = engram_observe::with_trace(|| run(g, src));
    let ms = t.elapsed().as_secs_f64() * 1e3;
    let mut tallies = BTreeMap::new();
    for k in FAMILY {
        if let Some(v) = trace.counters().get(*k) {
            tallies.insert(*k, *v);
        }
    }
    (ms, res.rows.len(), tallies)
}

fn report(tag: &str, name: &str, src: &str, ms: f64, rows: usize, t: &BTreeMap<&str, u64>) {
    eprintln!("[{tag}] {name:<26} {ms:>10.2} ms  rows={rows:<5} {src}");
    for (k, v) in t {
        eprintln!("[{tag}]     {v:>9}  {k}");
    }
}

fn first_int(r: &engram_graph::QueryResult) -> i64 {
    match r.rows.first().and_then(|r| r.first()) {
        Some(Value::Int(n)) => *n,
        other => panic!("expected one Int, got {other:?}"),
    }
}

/// The SNB read shapes of `stress.rs::render_read`, verbatim.
fn shape(name: &str, key: u64, space: u64) -> String {
    match name {
        "is1-profile" => format!(
            "MATCH (p:Person {{id: {key}}}) \
             RETURN p.firstName, p.lastName, p.birthday, p.locationIP, p.browserUsed"
        ),
        "is3-friends" => format!(
            "MATCH (p:Person {{id: {key}}})-[:KNOWS]-(f:Person) RETURN f.id, f.firstName LIMIT 25"
        ),
        "ic-foaf" => format!(
            "MATCH (p:Person {{id: {key}}})-[:KNOWS]-()-[:KNOWS]-(f:Person) \
             RETURN count(DISTINCT f) AS c"
        ),
        "is5-by-creator" => format!(
            "MATCH (m:Message)-[:HAS_CREATOR]->(p:Person {{id: {key}}}) RETURN m.id LIMIT 25"
        ),
        "is5-anchored" => format!(
            "MATCH (p:Person {{id: {key}}})<-[:HAS_CREATOR]-(m:Message) RETURN m.id LIMIT 25"
        ),
        "ic6-friend-tags" => format!(
            "MATCH (p:Person {{id: {key}}})-[:KNOWS]-(f:Person)<-[:HAS_CREATOR]-(m:Message) \
             MATCH (m)-[:HAS_TAG]->(t:Tag) RETURN t.name, count(*) AS c ORDER BY c DESC LIMIT 10"
        ),
        "knows-var-length" => format!(
            "MATCH (p:Person {{id: {key}}})-[:KNOWS*1..2]-(f:Person) RETURN count(DISTINCT f) AS c"
        ),
        "is7-replies" => format!(
            "MATCH (m:Message {{id: {}}})<-[:REPLY_OF]-(c:Comment) RETURN c.id LIMIT 25",
            key.wrapping_mul(7) % space.max(1)
        ),
        "agg-by-city" => "MATCH (p:Person)-[:IS_LOCATED_IN]->(c:City) \
             RETURN c.name, count(p) AS n ORDER BY n DESC LIMIT 10"
            .to_string(),
        // NOT a harness shape: the OUT side of HAS_CREATOR, whose changed
        // set is the burst's NEW message nodes (always > ADJ_REPAIR_MAX for a
        // 5k burst, whatever the person count).
        "has-creator-out" => format!(
            "MATCH (m:Message {{id: {key}}})-[:HAS_CREATOR]->(p:Person) RETURN p.id"
        ),
        other => unreachable!("unknown shape {other}"),
    }
}

/// `stress.rs::render_write(Snb, Uniform, Node, cid=0, seq, space, _)`, with
/// the message id offset so it never collides with a corpus message id.
fn burst_write(seq: u64, space: u64) -> String {
    let author = seq % space.max(1);
    format!(
        "MATCH (p:Person {{id: {author}}}) \
         CREATE (m:Message:Comment {{id: {}, creationDate: {}, content: 'stress', length: 6}})\
         -[:HAS_CREATOR]->(p)",
        (1u64 << 40) | seq,
        1_400_000_000_000i64 + seq as i64
    )
}

fn levers_off() -> bool {
    matches!(
        std::env::var("ENGRAM_BURST_ATTRIB_LEVERS").as_deref(),
        Ok("0") | Ok("off") | Ok("false")
    )
}

/// The in-process stand-in for the server's maintenance thread: one
/// `refresh_stale_derived`, timed and traced, reported under `[maint]`.
/// Returns the report and the wall time — the cost the reader did NOT pay.
fn maintenance_refresh(g: &Graph, what: &str) -> (engram_graph::RefreshReport, f64) {
    let t = Instant::now();
    let (report, trace) = engram_observe::with_trace(|| g.refresh_stale_derived());
    let ms = t.elapsed().as_secs_f64() * 1e3;
    eprintln!("[maint] refresh ({what}) {ms:>10.2} ms  {report:?}");
    let mut rows: Vec<(&String, u64)> = trace.counters().iter().map(|(k, v)| (k, *v)).collect();
    rows.sort_by_key(|(_, v)| std::cmp::Reverse(*v));
    for (k, v) in rows.iter().filter(|(k, _)| FAMILY.contains(&k.as_str()) || k.contains("maintenance") || k.contains("declined")) {
        eprintln!("[maint]     {v:>9}  {k}");
    }
    (report, ms)
}

const HOT_SET: &str = "MATCH (p:Person {id: 0}) SET p.hits = coalesce(p.hits, 0) + 1";
const HOT_PROBE: &str = "MATCH (p:Person {id: 0}) RETURN coalesce(p.hits, 0)";

/// Load the corpus the way the server does: bulk load, seal, then either
/// compact (resident, `portserve`) or `into_paged` (the pod's serving mode),
/// and serve from a FRESH graph over the shared store (the resolver's graph,
/// not the loading one).
fn load(dir: &str) -> (Graph, u64) {
    let loader = Graph::new(Store::new(), Realm(1), Namespace(1));
    let t = Instant::now();
    let stats = engram_bench::load_export(&loader, std::path::Path::new(dir));
    let store = loader.shared_store();
    drop(loader);
    store.seal();
    let mode = match std::env::var("ENGRAM_BURST_ATTRIB_PAGED") {
        Ok(mb) => {
            let mb: usize = mb.parse().expect("ENGRAM_BURST_ATTRIB_PAGED = cache MB");
            let pdir = std::path::Path::new(dir).join("paged-attrib");
            let _ = std::fs::remove_dir_all(&pdir);
            std::fs::create_dir_all(&pdir).expect("mkdir paged dir");
            let _cache = store.into_paged(&pdir, mb << 20).expect("into_paged");
            format!("PAGED (cache {mb} MB, {} segments)", store.segment_count())
        }
        Err(_) => {
            store.compact();
            "RESIDENT (sealed+compacted)".to_string()
        }
    };
    eprintln!(
        "[burst-attrib] loaded {} nodes, {} rels from {dir} in {:.1} s — {mode}",
        stats.nodes,
        stats.rels,
        t.elapsed().as_secs_f64()
    );
    let g = Graph::new(store, Realm(1), Namespace(1));
    // The BEFORE arm: every post-burst lever off, so one binary measures
    // both sides. Applied before the warm, which walks the span too.
    if levers_off() {
        g.set_adj_cost_repair(false);
        g.set_members_batch_fold(false);
        g.set_scan_resistant_rebuild(false);
        eprintln!("[burst-attrib] LEVERS OFF: fixed repair cap, serial fold, plain rebuild walk");
    }
    // The harness's fixture requirement: the two indexes and their forced
    // builds, timed as themselves (stress.rs `Dataset::indexes/index_probes`).
    for ddl in [
        "CREATE INDEX snb_person_id IF NOT EXISTS FOR (n:Person) ON (n.id)",
        "CREATE INDEX snb_message_id IF NOT EXISTS FOR (n:Message) ON (n.id)",
    ] {
        run(&g, ddl);
    }
    for probe in ["MATCH (p:Person {id: 1}) RETURN p.id", "MATCH (m:Message {id: 1}) RETURN m.id"] {
        let (ms, _, t) = traced(&g, probe);
        eprintln!("[burst-attrib] index build (first seek) {ms:.0} ms  {probe}  {t:?}");
    }
    // The server warms at boot (`ServerConfig::warm_caches`, default true):
    // every (side, type) table published in one pass.
    let t = Instant::now();
    let w = g.warm();
    eprintln!(
        "[burst-attrib] warmed in {} ms: {} nodes, {} out-edges, {} in-edges, {} tables",
        t.elapsed().as_millis(),
        w.nodes,
        w.out_edges,
        w.in_edges,
        w.tables
    );
    let persons = first_int(&run(&g, "MATCH (p:Person) RETURN count(*) AS c")) as u64;
    eprintln!("[burst-attrib] persons = {persons} (the key space)");
    (g, persons)
}

#[test]
fn which_statement_pays_after_a_write_only_burst() {
    let Ok(dir) = std::env::var("ENGRAM_BURST_ATTRIB_DIR") else {
        eprintln!("[burst-attrib] ENGRAM_BURST_ATTRIB_DIR unset — skipping (see the header)");
        return;
    };
    let n: u64 = std::env::var("ENGRAM_BURST_ATTRIB_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5_000);
    let set_first = std::env::var_os("ENGRAM_BURST_ATTRIB_SET_FIRST").is_some();
    let (g, persons) = load(&dir);

    // Client 0's first twelve reads under the sweep's seed (424242), replayed
    // from stress.rs's SplitMix64: is1 ×8, is3 ×2, is7 ×1, then is5-by-creator
    // (key 821) as the 12th — the operation in flight when the level stopped.
    let pre_reads: Vec<(&str, String)> = vec![
        ("is1-profile", shape("is1-profile", 8615 % persons, persons)),
        ("is3-friends", shape("is3-friends", 4282 % persons, persons)),
        ("is7-replies", shape("is7-replies", 486 % persons, persons)),
    ];
    let payer_name = "is5-by-creator";
    let payer = shape(payer_name, 821 % persons, persons);
    let after: Vec<(&str, String)> = vec![
        ("is5-anchored", shape("is5-anchored", 821 % persons, persons)),
        ("ic6-friend-tags", shape("ic6-friend-tags", 821 % persons, persons)),
        ("has-creator-out", shape("has-creator-out", 5, persons)),
        ("agg-by-city", shape("agg-by-city", 0, persons)),
        ("ic-foaf", shape("ic-foaf", 821 % persons, persons)),
    ];

    // ── Steady state: every shape three times, the third traced ──────────
    eprintln!("\n[steady] every shape ×3 warm, third run traced");
    let mut steady: BTreeMap<&str, f64> = BTreeMap::new();
    for (name, src) in pre_reads.iter().chain(std::iter::once(&(payer_name, payer.clone()))).chain(after.iter()) {
        run(&g, src);
        run(&g, src);
        let (ms, rows, t) = traced(&g, src);
        report("steady", name, src, ms, rows, &t);
        steady.insert(name, ms);
    }
    run(&g, HOT_SET);
    let (ms, rows, t) = traced(&g, HOT_SET);
    report("steady", "hot-set", HOT_SET, ms, rows, &t);
    steady.insert("hot-set", ms);
    let (ms, rows, t) = traced(&g, HOT_PROBE);
    report("steady", "hot-probe", HOT_PROBE, ms, rows, &t);

    // ── The burst: N write-only statements, no read between them ─────────
    // With ENGRAM_BURST_ATTRIB_MAINT=<M>, the server's maintenance cadence
    // runs between chunks of M writes and once after the last — never
    // inside a read — and its cost is reported apart from the burst's.
    let maint: u64 = std::env::var("ENGRAM_BURST_ATTRIB_MAINT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    eprintln!("\n[burst] {n} × `MATCH (p:Person {{id: seq % {persons}}}) CREATE (m:Message:Comment)-[:HAS_CREATOR]->(p)`");
    if maint > 0 {
        eprintln!("[burst] maintenance refresh every {maint} writes and at the end (ENGRAM_BURST_ATTRIB_MAINT)");
    }
    let t0 = Instant::now();
    let mut trace = engram_observe::Trace::default();
    let mut burst_ms = 0f64;
    let mut maint_ms = 0f64;
    let mut maint_reports: Vec<engram_graph::RefreshReport> = Vec::new();
    let chunk = if maint > 0 { maint } else { n };
    let mut seq = 0u64;
    while seq < n {
        let end = (seq + chunk).min(n);
        let tc = Instant::now();
        let ((), tr) = engram_observe::with_trace(|| {
            for s in seq..end {
                run(&g, &burst_write(s, persons));
            }
        });
        burst_ms += tc.elapsed().as_secs_f64() * 1e3;
        for (k, v) in tr.counters() {
            trace.count(k, *v);
        }
        seq = end;
        if maint > 0 {
            let what = if seq < n { format!("after {seq} writes") } else { "the tick".to_string() };
            let (r, ms) = maintenance_refresh(&g, &what);
            maint_ms += ms;
            maint_reports.push(r);
        }
    }
    let wall_ms = t0.elapsed().as_secs_f64() * 1e3;
    eprintln!(
        "[burst] {n} writes in {burst_ms:.0} ms ({:.0}/s); distinct dst persons = {}, distinct src messages = {n}",
        n as f64 * 1e3 / burst_ms,
        n.min(persons)
    );
    if maint > 0 {
        eprintln!(
            "[burst] maintenance: {} refresh(es), {maint_ms:.0} ms off the read path (burst+maintenance wall {wall_ms:.0} ms)",
            maint_reports.len()
        );
    }
    let mut rows: Vec<(&String, u64)> = trace.counters().iter().map(|(k, v)| (k, *v)).collect();
    rows.sort_by_key(|(_, v)| std::cmp::Reverse(*v));
    for (k, v) in rows.iter().filter(|(k, _)| FAMILY.contains(&k.as_str())) {
        eprintln!("[burst]     {v:>9}  [{:>7.2}/w]  {k}", *v as f64 / n as f64);
    }
    let burst_built = trace.counters().get("graph.adjacency tables built").copied().unwrap_or(0);
    let burst_repaired = trace.counters().get("graph.adjacency tables repaired").copied().unwrap_or(0);

    // ── After the burst: the level's sequence, each statement traced ─────
    eprintln!("\n[post] the contention level's sequence after the burst (each statement traced)");
    let mut post: BTreeMap<&str, (f64, BTreeMap<&str, u64>)> = BTreeMap::new();
    if set_first {
        let (ms, rows, t) = traced(&g, HOT_SET);
        report("post", "hot-set (FIRST)", HOT_SET, ms, rows, &t);
        post.insert("hot-set-first", (ms, t));
    }
    let (ms, rows, t) = traced(&g, HOT_PROBE);
    report("post", "hot-probe (level start)", HOT_PROBE, ms, rows, &t);
    post.insert("hot-probe", (ms, t));
    for (name, src) in &pre_reads {
        let (ms, rows, t) = traced(&g, src);
        report("post", name, src, ms, rows, &t);
        post.insert(name, (ms, t));
    }
    let (ms, rows, t) = traced(&g, HOT_SET);
    report("post", "hot-set", HOT_SET, ms, rows, &t);
    post.insert("hot-set", (ms, t));
    let (ms, rows, t) = traced(&g, &payer);
    report("post", "is5-by-creator (12th read)", &payer, ms, rows, &t);
    post.insert("is5-by-creator", (ms, t));
    let (ms2, rows, t2) = traced(&g, &payer);
    report("post", "is5-by-creator (again)", &payer, ms2, rows, &t2);
    post.insert("is5-by-creator-again", (ms2, t2));
    for (name, src) in &after {
        let (ms, rows, t) = traced(&g, src);
        report("post", name, src, ms, rows, &t);
        post.insert(name, (ms, t));
    }

    // ── The attribution, as ratios ───────────────────────────────────────
    eprintln!("\n[ratio] first-post-burst / steady (ms)");
    for (name, (ms, _)) in &post {
        if let Some(s) = steady.get(name.trim_end_matches("-again")) {
            eprintln!("[ratio] {name:<26} {ms:>10.2} / {s:>8.2} = {:>8.1}x", ms / s.max(1e-6));
        }
    }
    let get = |name: &str, k: &str| post.get(name).and_then(|(_, t)| t.get(k)).copied().unwrap_or(0);
    let maint_adj: usize = maint_reports.iter().map(|r| r.adjacency_repaired + r.adjacency_rebuilt).sum();
    let maint_members: usize = maint_reports.iter().map(|r| r.members_caught_up + r.members_rebuilt).sum();
    eprintln!(
        "\n[verdict] levers={} maintenance: adjacency={maint_adj} members={maint_members} ({maint_ms:.0} ms) | \
         burst: built={burst_built} repaired={burst_repaired} | \
         hot-set: built={} repaired={} | hot-probe: built={} repaired={} | \
         is1: built={} repaired={} | is5-by-creator: built={} repaired={} folded={} \
         members-built={} members-caught-up={} members-folded={} | \
         has-creator-out: built={} repaired={}",
        if levers_off() { "OFF" } else { "on" },
        get("hot-set", "graph.adjacency tables built"),
        get("hot-set", "graph.adjacency tables repaired"),
        get("hot-probe", "graph.adjacency tables built"),
        get("hot-probe", "graph.adjacency tables repaired"),
        get("is1-profile", "graph.adjacency tables built"),
        get("is1-profile", "graph.adjacency tables repaired"),
        get("is5-by-creator", "graph.adjacency tables built"),
        get("is5-by-creator", "graph.adjacency tables repaired"),
        get("is5-by-creator", "graph.adjacency table overlay folded"),
        get("is5-by-creator", "graph.membership snapshots built"),
        get("is5-by-creator", "graph.membership snapshots caught up"),
        get("is5-by-creator", "derived.members view folded"),
        get("has-creator-out", "graph.adjacency tables built"),
        get("has-creator-out", "graph.adjacency tables repaired"),
    );

    // Structural facts that hold by construction; the numbers above are the
    // finding and are read, not frozen.
    assert_eq!(burst_built, 0, "a write never rebuilds an adjacency table");
    assert_eq!(
        get("hot-set", "graph.adjacency tables built") + get("hot-set", "graph.adjacency tables repaired"),
        0,
        "the hot SET touches no adjacency-derived structure"
    );
    let is5_work = get("is5-by-creator", "graph.adjacency tables built")
        + get("is5-by-creator", "graph.adjacency tables repaired");
    if maint > 0 {
        assert_eq!(
            is5_work, 0,
            "with the maintenance cadence the first HAS_CREATOR read must find its table current"
        );
        assert!(maint_adj >= 1, "the maintenance refresh must have brought an adjacency table current");
    } else {
        assert!(
            is5_work >= 1,
            "the first HAS_CREATOR read after the burst must rebuild or repair its table"
        );
    }
    assert_eq!(
        get("is5-by-creator-again", "graph.adjacency tables built")
            + get("is5-by-creator-again", "graph.adjacency tables repaired"),
        0,
        "the second run must reuse the table the first published"
    );
}

/// Delete-churn census: what ONE churn op records and rebuilds, single-thread
/// (the mechanism), then the same loop on 1 and N threads over the shared
/// graph (does the engine alone scale negatively, or only the server?).
#[test]
fn delete_churn_census_and_thread_scaling() {
    let Ok(dir) = std::env::var("ENGRAM_BURST_ATTRIB_DIR") else {
        eprintln!("[churn] ENGRAM_BURST_ATTRIB_DIR unset — skipping (see the header)");
        return;
    };
    let threads: Vec<usize> = std::env::var("ENGRAM_BURST_ATTRIB_CHURN_THREADS")
        .ok()
        .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![1, 8]);
    let (g, _persons) = load(&dir);
    for ddl in [
        "CREATE INDEX churn_id IF NOT EXISTS FOR (n:Churn) ON (n.id)",
        "CREATE INDEX churn_anchor_cid IF NOT EXISTS FOR (n:ChurnAnchor) ON (n.cid)",
    ] {
        run(&g, ddl);
    }
    // stress.rs's churn plan: odd seq deletes the oldest live id once 16 are
    // live, even seq creates `seq` wired to the worker's anchor.
    fn churn_ops(cid: u64, nonce: u64, ops: u64) -> Vec<(bool, String)> {
        let mut live = std::collections::VecDeque::new();
        let mut out = Vec::with_capacity(ops as usize);
        let base = cid << 40;
        for i in 0..ops {
            let seq = base + i;
            if seq % 2 == 1 && live.len() >= 16 {
                let id: u64 = live.pop_front().expect("floor");
                out.push((true, format!("MATCH (n:Churn {{id: {id}, nonce: {nonce}}}) DETACH DELETE n")));
            } else {
                live.push_back(seq);
                out.push((
                    false,
                    format!(
                        "MATCH (a:ChurnAnchor {{cid: {cid}, nonce: {nonce}}}) \
                         CREATE (a)-[:CHURN]->(:Churn {{id: {seq}, cid: {cid}, nonce: {nonce}}})"
                    ),
                ));
            }
        }
        out
    }

    // ── Single-thread census: per-op tallies, creates vs deletes apart ──
    run(&g, "CREATE (:ChurnAnchor {cid: 0, nonce: 1})");
    let ops = churn_ops(0, 1, 2_000);
    // warm the two churn indexes' first seeks and the anchor's table gate
    run(&g, &ops[0].1);
    let mut create_t = engram_observe::Trace::default();
    let mut delete_t = engram_observe::Trace::default();
    let (mut cn, mut dn) = (0u64, 0u64);
    let (mut cms, mut dms) = (0f64, 0f64);
    for (is_delete, src) in &ops[1..] {
        let t = Instant::now();
        let ((), tr) = engram_observe::with_trace(|| {
            run(&g, src);
        });
        let ms = t.elapsed().as_secs_f64() * 1e3;
        let (acc, n, tot) = if *is_delete {
            (&mut delete_t, &mut dn, &mut dms)
        } else {
            (&mut create_t, &mut cn, &mut cms)
        };
        *n += 1;
        *tot += ms;
        for (k, v) in tr.counters() {
            acc.count(k, *v);
        }
    }
    for (what, n, tot, tr) in [("create", cn, cms, &create_t), ("DETACH DELETE", dn, dms, &delete_t)] {
        eprintln!("\n[churn] {what}: {n} ops, {:.3} ms/op — counters per op:", tot / n.max(1) as f64);
        let mut rows: Vec<(&String, u64)> = tr.counters().iter().map(|(k, v)| (k, *v)).collect();
        rows.sort_by_key(|(_, v)| std::cmp::Reverse(*v));
        for (k, v) in rows.iter().take(40) {
            eprintln!("[churn]     {v:>9}  [{:>8.3}/op]  {k}", *v as f64 / n.max(1) as f64);
        }
    }

    // ── 1 vs N threads over the SHARED graph, per-worker anchors ─────────
    // `ENGRAM_BURST_ATTRIB_CHURN_PAD=<n>` pre-creates n live :Churn nodes so
    // a 1-thread level scans a label as large as an N-thread level's, which
    // separates the label-scan growth from thread contention.
    let pad: u64 = std::env::var("ENGRAM_BURST_ATTRIB_CHURN_PAD")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if pad > 0 {
        run(
            &g,
            &format!("UNWIND range(0, {}) AS i CREATE (:Churn {{id: 900000000 + i, cid: 99, nonce: 999}})", pad - 1),
        );
        let live = first_int(&run(&g, "MATCH (n:Churn) RETURN count(*) AS c"));
        eprintln!("[churn-threads] padded the :Churn label to {live} live nodes before the levels");
    }
    let per_thread: u64 = 3_000;
    for (level, &k) in threads.iter().enumerate() {
        let nonce = 100 + level as u64;
        for cid in 0..k as u64 {
            run(&g, &format!("CREATE (:ChurnAnchor {{cid: {cid}, nonce: {nonce}}})"));
        }
        let t0 = Instant::now();
        let mut merged: BTreeMap<String, u64> = BTreeMap::new();
        std::thread::scope(|s| {
            let hs: Vec<_> = (0..k as u64)
                .map(|cid| {
                    let g = &g;
                    s.spawn(move || {
                        let ops = churn_ops(cid, nonce, per_thread);
                        let ((), tr) = engram_observe::with_trace(|| {
                            for (_, src) in &ops {
                                run(g, src);
                            }
                        });
                        tr
                    })
                })
                .collect();
            for h in hs {
                let tr = h.join().expect("worker");
                for (kk, v) in tr.counters() {
                    *merged.entry(kk.clone()).or_insert(0) += v;
                }
            }
        });
        let secs = t0.elapsed().as_secs_f64();
        let total = per_thread * k as u64;
        eprintln!(
            "\n[churn-threads] {k} thread(s): {total} ops in {secs:.2} s = {:.0} ops/s ({:.0} ops/s/thread)",
            total as f64 / secs,
            per_thread as f64 / secs
        );
        let mut rows: Vec<(&String, u64)> = merged.iter().map(|(k, v)| (k, *v)).collect();
        rows.sort_by_key(|(_, v)| std::cmp::Reverse(*v));
        for (kk, v) in rows.iter().filter(|(kk, _)| FAMILY.contains(&kk.as_str()) || kk.contains("conflict")) {
            eprintln!("[churn-threads]     {v:>9}  [{:>8.3}/op]  {kk}", *v as f64 / total as f64);
        }
        let survivors = first_int(&run(&g, &format!("MATCH (n:Churn {{nonce: {nonce}}}) RETURN count(*) AS c")));
        eprintln!("[churn-threads] survivors with nonce {nonce}: {survivors}");
    }
    // What the churn left behind for the NEXT `id` seek: the churn statements
    // never seek (label-scan plan), so nobody consumed the `id` property log
    // while every create/delete recorded into it.
    let (ms, rows, t) = traced(&g, "MATCH (p:Person {id: 1}) RETURN p.id");
    report("churn-after", "Person.id seek", "MATCH (p:Person {id: 1}) RETURN p.id", ms, rows, &t);
}
