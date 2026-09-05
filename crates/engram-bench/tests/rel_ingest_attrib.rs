//! Relationship-INGEST attribution (scratch repro, local-only).
//!
//! The official SF1 measurement puts the paged relationship pass at 6,395
//! rels/s against Neo4j 5.26's ~9,800 — an indexed `MATCH`+`CREATE`, the most
//! load-bearing write shape a graph database has. This test takes that pass
//! apart in-process and says where the time goes, with counters rather than
//! guesses.
//!
//! # It replays the LOADER'S OWN PLAN
//!
//! `snbload --match-on <id|gid>` under `SNBLOAD_DUMP=<path>` writes the plan it
//! would send — its keying, its grouping, its group order, its index lifetimes
//! — instead of sending it. This driver walks that file. A second
//! implementation of the keying here would be a second thing to keep in step
//! with the loader, and the day it drifted the attribution would describe a
//! load nobody runs.
//!
//! Plan lines: `S\t<statement>` (node creates, index DDL, in order) and
//! `G\t<type>\t<srcLabel>\t<dstLabel>\t<prop>\t<elem>\t<elem>…`, one line per
//! (type, label-pair) group carrying every rendered `[s, d]` pair — so the
//! driver re-chunks at any batch size and a batch sweep measures the loader's
//! pairs, not a re-rendering of them.
//!
//! # The decomposition
//!
//! Five renderings of the same group, each a strict subset of the next, run as
//! whole passes over the whole corpus from a fresh store:
//!
//! | mode     | statement                                            | isolates |
//! |----------|------------------------------------------------------|----------|
//! | `unwind` | `UNWIND […] AS pair RETURN count(pair)`              | Bolt-free parse + literal-list build + UNWIND |
//! | `match1` | `… MATCH (a:SL {p: pair[0]}) RETURN count(*)`         | + ONE index seek per pair |
//! | `match`  | `… MATCH (a:SL{…}), (b:DL{…}) RETURN count(*)`        | + the SECOND seek |
//! | `full`   | `… CREATE (a)-[:T]->(b)`                             | + the create (the loader's real statement) |
//! | `api`    | `Graph::create_rel` directly, ids resolved up front  | the create with NO Cypher at all |
//!
//! Differences between adjacent modes attribute wall time; `api` is the floor
//! the statement path is measured against. `api` needs integer keys and
//! refuses a `gid` plan rather than quietly measuring something else.
//!
//! # Wall time is measured UNTRACED
//!
//! `counted!` is a thread-local borrow plus a `BTreeMap<String, u64>` lookup
//! keyed by string compare. Over a million relationships that is not free, so
//! a traced pass is NOT a timing of the untraced one. Timing runs untraced;
//! counters come from tracing the FIRST statement of each group (a per-group,
//! per-relationship operation table); commit-log growth is read exactly from
//! `Store::log_len()` across the whole pass, which costs nothing either way.
//!
//! GATED on `ENGRAM_RELATTRIB_PLAN`; a no-op without it. Knobs:
//!   ENGRAM_RELATTRIB_PLAN=<plan file from SNBLOAD_DUMP>
//!   ENGRAM_RELATTRIB_MODES=unwind,match1,match,full,api   (default: all five)
//!   ENGRAM_RELATTRIB_BATCH=500                            (pairs per statement)
//!   ENGRAM_RELATTRIB_BULK=1                               (bulk ingest, as --bulk-ingest)
//!   ENGRAM_RELATTRIB_LIMIT=<n>                            (stop after n rels per mode)
//!
//! Run with:
//!   SNBLOAD_DUMP=plan-id.txt snbload <corpus> 127.0.0.1:1 --match-on id
//!   ENGRAM_RELATTRIB_PLAN=plan-id.txt cargo test -p engram-bench --release \
//!     --test rel_ingest_attrib -- --nocapture --test-threads 1
//!
//! The wall clock is read here to MEASURE, the same waiver the stress harness
//! and `post_burst_rebuild_attrib.rs` carry; nothing decides on it.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::io::BufRead;
use std::time::Instant;

use engram_cypher::{Value, parse_any};
use engram_graph::{Graph, run_stmt};
use engram_key::{Namespace, Realm};
use engram_store::Store;

/// Counters that name work the relationship pass is made of: the store rows it
/// writes and reads, the seeks it makes, the derived structures it disturbs,
/// and the commit log it appends to.
const FAMILY: &[&str] = &[
    "store.gets",
    "store.puts",
    "store.unlogged puts",
    "store.deletes",
    "store.scans",
    "store.visitor scans",
    "store.block probes",
    "store.column scans",
    "store.column presence scans",
    "store.count scans",
    "store.block rows assembled",
    "store.projected gets",
    "store.txn commits",
    "store.txn conflicts",
    "store.cas commits",
    "store.group commits",
    "log.appended",
    "log.fsyncs",
    "index.builds",
    "index.range queries",
    "index.incremental updates",
    "index.overlay folds",
    "graph.range index builds",
    "graph.range index caught up",
    "graph.range index still current",
    "graph.range index cache hit",
    "graph.rels created",
    "graph.nodes created",
    "graph.tokens minted",
    "graph.nodes materialised in full",
    "graph.projected node materialisations",
    "graph.projected gets served from columns",
    "graph.stats rebuilt",
    "graph.publish stamp fenced below an in-flight writer",
    "graph.membership snapshots built",
    "graph.membership snapshots caught up",
    "graph.membership snapshots still current",
    "graph.adjacency tables built",
    "graph.adjacency tables repaired",
    "graph.count fast paths",
    "derived.change log touched",
    "derived.change log pruned",
    "derived.change log overflowed",
    "derived.snapshot published",
    "interp.statements run",
    "interp.bare count stages",
    "interp.dispatch run_pipeline",
    "interp.dispatch run_join",
    "interp.pipeline join runs",
    "cypher.statements parsed",
    "cypher.expressions parsed",
    "cypher.expressions evaluated",
    "paged.pread",
    "paged.block cache hit",
    "paged.block cache miss",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Unwind,
    Match1,
    Match,
    Full,
    Api,
}

impl Mode {
    fn parse(s: &str) -> Option<Mode> {
        Some(match s {
            "unwind" => Mode::Unwind,
            "match1" => Mode::Match1,
            "match" => Mode::Match,
            "full" => Mode::Full,
            "api" => Mode::Api,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Mode::Unwind => "unwind",
            Mode::Match1 => "match1",
            Mode::Match => "match",
            Mode::Full => "full",
            Mode::Api => "api",
        }
    }
}

/// One (type, src label, dst label) group and its rendered pairs.
struct Group {
    rel_type: String,
    src_label: String,
    dst_label: String,
    prop: String,
    /// Each element as the loader rendered it: `[0, 1]` or `['p:1', 'm:2']`.
    elems: Vec<String>,
}

/// One plan step, in the loader's order.
enum Step {
    Stmt(String),
    Group(Group),
}

fn read_plan(path: &str) -> Vec<Step> {
    let f = std::fs::File::open(path)
        .unwrap_or_else(|e| panic!("cannot open the plan {path}: {e}\n  produce it with SNBLOAD_DUMP=<path> snbload <corpus> 127.0.0.1:1 --match-on id"));
    let mut r = std::io::BufReader::with_capacity(1 << 20, f);
    let mut out = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        // `read_line` rather than `lines()`: one group line carries every pair
        // of its group and runs to megabytes.
        if r.read_line(&mut line).expect("read the plan") == 0 {
            break;
        }
        let line = line.trim_end_matches(['\n', '\r']);
        let Some((tag, rest)) = line.split_once('\t') else {
            continue;
        };
        match tag {
            "S" => out.push(Step::Stmt(rest.to_string())),
            "G" => {
                let mut it = rest.split('\t');
                let mut next = |what: &str| {
                    it.next()
                        .unwrap_or_else(|| panic!("plan group line has no {what}"))
                        .to_string()
                };
                let rel_type = next("type");
                let src_label = next("src label");
                let dst_label = next("dst label");
                let prop = next("property");
                out.push(Step::Group(Group {
                    rel_type,
                    src_label,
                    dst_label,
                    prop,
                    elems: it.map(str::to_string).collect(),
                }));
            }
            _ => panic!("unknown plan line tag {tag:?}"),
        }
    }
    out
}

fn run(g: &Graph, src: &str) -> engram_graph::QueryResult {
    let s = parse_any(src).unwrap_or_else(|e| panic!("parse `{}`: {e}", head(src)));
    run_stmt(g, &s, BTreeMap::new()).unwrap_or_else(|e| panic!("run `{}`: {e:?}", head(src)))
}

fn head(s: &str) -> String {
    s.chars().take(140).collect()
}

/// Render one statement for a chunk of a group, in `mode`.
fn render(g: &Group, mode: Mode, chunk: &[String]) -> String {
    let list = chunk.join(", ");
    let (t, sl, dl, p) = (&g.rel_type, &g.src_label, &g.dst_label, &g.prop);
    match mode {
        Mode::Unwind => format!("UNWIND [{list}] AS pair RETURN count(pair)"),
        Mode::Match1 => {
            format!("UNWIND [{list}] AS pair MATCH (a:{sl} {{{p}: pair[0]}}) RETURN count(a)")
        }
        Mode::Match => format!(
            "UNWIND [{list}] AS pair \
             MATCH (a:{sl} {{{p}: pair[0]}}), (b:{dl} {{{p}: pair[1]}}) RETURN count(a)"
        ),
        // The loader's statement, verbatim.
        Mode::Full => format!(
            "UNWIND [{list}] AS pair \
             MATCH (a:{sl} {{{p}: pair[0]}}), (b:{dl} {{{p}: pair[1]}}) \
             CREATE (a)-[:{t}]->(b)"
        ),
        Mode::Api => unreachable!("api mode does not render a statement"),
    }
}

/// The two halves of a rendered `[s, d]` element.
fn split_elem(e: &str) -> (&str, &str) {
    let inner = e
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or_else(|| panic!("plan element {e:?} is not `[s, d]`"));
    inner
        .split_once(", ")
        .unwrap_or_else(|| panic!("plan element {e:?} has no `, ` separator"))
}

/// `label` → (integer key → node id), for `api` mode. Built with ONE scan per
/// label, outside the timed create loop.
fn key_map(g: &Graph, label: &str, prop: &str) -> BTreeMap<i64, u64> {
    let res = run(g, &format!("MATCH (n:{label}) RETURN id(n), n.{prop}"));
    let mut out = BTreeMap::new();
    for row in &res.rows {
        let (Some(Value::Int(id)), Some(Value::Int(k))) = (row.first(), row.get(1)) else {
            panic!(
                "api mode needs an INTEGER key: :{label}.{prop} produced {:?} — \
                 a `gid` plan has string keys, so run api against a `--match-on id` plan",
                row.get(1)
            );
        };
        out.insert(*k, *id as u64);
    }
    out
}

fn tally(trace: &engram_observe::Trace) -> BTreeMap<&'static str, u64> {
    let mut t = BTreeMap::new();
    for k in FAMILY {
        if let Some(v) = trace.counters().get(*k) {
            if *v > 0 {
                t.insert(*k, *v);
            }
        }
    }
    t
}

struct Phase {
    mode: Mode,
    rel_secs: f64,
    setup_secs: f64,
    rels: u64,
    statements: u64,
    log_before: u64,
    log_after: u64,
    /// Per group: (label, rels in the traced statement, tallies).
    traced: Vec<(String, u64, BTreeMap<&'static str, u64>)>,
    /// Per group: (name, rels, seconds, src label size, dst label size) — the
    /// seek's cost against the label it seeks in, which is the one lever that
    /// separates seek from create inside the real write statement.
    per_group: Vec<(String, u64, f64, u64, u64)>,
}

fn one_phase(plan: &[Step], mode: Mode, batch: usize, bulk: bool, limit: u64) -> Phase {
    let g = Graph::new(Store::new(), Realm(1), Namespace(1));
    if bulk {
        g.set_bulk_ingest(true).expect("bulk ingest has no failure");
        g.set_serialisable_autocommit(false);
    }
    let store = g.shared_store();

    let mut setup = 0.0f64;
    let mut rel_secs = 0.0f64;
    let mut rels = 0u64;
    let mut statements = 0u64;
    let mut traced = Vec::new();
    let mut per_group = Vec::new();
    let mut maps: BTreeMap<String, BTreeMap<i64, u64>> = BTreeMap::new();
    let mut log_before = None;

    for step in plan {
        match step {
            Step::Stmt(s) => {
                let t = Instant::now();
                run(&g, s);
                setup += t.elapsed().as_secs_f64();
            }
            Step::Group(grp) => {
                if log_before.is_none() {
                    log_before = Some(store.log_len());
                }
                if limit > 0 && rels >= limit {
                    continue;
                }
                if mode == Mode::Api {
                    // Resolution is SETUP, not the create's cost: one scan per
                    // label, reused across every group naming it.
                    for l in [&grp.src_label, &grp.dst_label] {
                        if !maps.contains_key(l) {
                            let t = Instant::now();
                            let m = key_map(&g, l, &grp.prop);
                            setup += t.elapsed().as_secs_f64();
                            maps.insert(l.clone(), m);
                        }
                    }
                }
                let mut first = true;
                let g_t0 = rel_secs;
                let g_r0 = rels;
                for chunk in grp.elems.chunks(batch) {
                    if limit > 0 && rels >= limit {
                        break;
                    }
                    let n = chunk.len() as u64;
                    if mode == Mode::Api {
                        let sm = &maps[&grp.src_label];
                        let dm = &maps[&grp.dst_label];
                        let props: BTreeMap<String, Value> = BTreeMap::new();
                        // Parsing the element back to two integers is the
                        // driver's own work and is done BEFORE the clock.
                        let pairs: Vec<(u64, u64)> = chunk
                            .iter()
                            .map(|e| {
                                let (a, b) = split_elem(e);
                                let ka: i64 = a.parse().unwrap_or_else(|_| {
                                    panic!("api mode needs integer keys, got {a:?}")
                                });
                                let kb: i64 = b.parse().unwrap_or_else(|_| {
                                    panic!("api mode needs integer keys, got {b:?}")
                                });
                                (sm[&ka], dm[&kb])
                            })
                            .collect();
                        let t = Instant::now();
                        for (s, d) in &pairs {
                            g.create_rel(*s, &grp.rel_type, *d, &props)
                                .expect("create_rel");
                        }
                        rel_secs += t.elapsed().as_secs_f64();
                        if first {
                            let (_, tr) = engram_observe::with_trace(|| {
                                g.create_rel(pairs[0].0, &grp.rel_type, pairs[0].1, &props)
                                    .expect("traced create_rel")
                            });
                            traced.push((
                                format!("{} {}->{}", grp.rel_type, grp.src_label, grp.dst_label),
                                1,
                                tally(&tr),
                            ));
                        }
                    } else {
                        let stmt = render(grp, mode, chunk);
                        let t = Instant::now();
                        run(&g, &stmt);
                        rel_secs += t.elapsed().as_secs_f64();
                        if first {
                            // A traced repeat of a statement of the SAME shape,
                            // outside the timed total. For a writing mode this
                            // re-creates the chunk's edges — the tallies are what
                            // is wanted, and a duplicate edge costs the same.
                            let (_, tr) = engram_observe::with_trace(|| run(&g, &stmt));
                            traced.push((
                                format!("{} {}->{}", grp.rel_type, grp.src_label, grp.dst_label),
                                n,
                                tally(&tr),
                            ));
                        }
                    }
                    first = false;
                    rels += n;
                    statements += 1;
                }
                // Label sizes are read AFTER the group so a seek's cost can be
                // read against the label it actually seeks in.
                let size = |l: &str| {
                    match run(&g, &format!("MATCH (n:{l}) RETURN count(n)"))
                        .rows
                        .first()
                        .and_then(|r| r.first())
                    {
                        Some(Value::Int(n)) => *n as u64,
                        _ => 0,
                    }
                };
                per_group.push((
                    format!("{} {}->{}", grp.rel_type, grp.src_label, grp.dst_label),
                    rels - g_r0,
                    rel_secs - g_t0,
                    size(&grp.src_label),
                    size(&grp.dst_label),
                ));
            }
        }
    }
    let log_after = store.log_len();
    Phase {
        mode,
        rel_secs,
        setup_secs: setup,
        rels,
        statements,
        log_before: log_before.unwrap_or(0),
        log_after,
        traced,
        per_group,
    }
}

#[test]
fn rel_ingest_attribution() {
    let Ok(plan_path) = std::env::var("ENGRAM_RELATTRIB_PLAN") else {
        eprintln!("[relattrib] ENGRAM_RELATTRIB_PLAN unset — skipping");
        return;
    };
    let batch: usize = std::env::var("ENGRAM_RELATTRIB_BATCH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);
    let bulk = matches!(
        std::env::var("ENGRAM_RELATTRIB_BULK").as_deref(),
        Ok("1") | Ok("on") | Ok("true")
    );
    let limit: u64 = std::env::var("ENGRAM_RELATTRIB_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let modes: Vec<Mode> = std::env::var("ENGRAM_RELATTRIB_MODES")
        .unwrap_or_else(|_| "unwind,match1,match,full,api".to_string())
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| Mode::parse(s).unwrap_or_else(|| panic!("unknown mode {s:?}")))
        .collect();

    let t = Instant::now();
    let plan = read_plan(&plan_path);
    let groups = plan
        .iter()
        .filter(|s| matches!(s, Step::Group(_)))
        .count();
    let pairs: usize = plan
        .iter()
        .map(|s| match s {
            Step::Group(g) => g.elems.len(),
            Step::Stmt(_) => 0,
        })
        .sum();
    eprintln!(
        "[relattrib] plan {plan_path}: {} step(s), {groups} group(s), {pairs} pair(s), \
         read in {:.1}s | batch={batch} bulk={bulk} limit={limit}",
        plan.len(),
        t.elapsed().as_secs_f64()
    );

    let mut phases = Vec::new();
    for m in &modes {
        let p = one_phase(&plan, *m, batch, bulk, limit);
        eprintln!(
            "[relattrib] MODE {:<7} rels={} stmts={} REL-PASS {:.2}s = {:.0} rels/s \
             ({:.3} us/rel) | setup(nodes+ddl{}) {:.2}s | commit log {} -> {} (+{}, {:.2}/rel)",
            p.mode.name(),
            p.rels,
            p.statements,
            p.rel_secs,
            p.rels as f64 / p.rel_secs.max(1e-9),
            p.rel_secs * 1e6 / p.rels.max(1) as f64,
            if p.mode == Mode::Api { "+resolve" } else { "" },
            p.setup_secs,
            p.log_before,
            p.log_after,
            p.log_after.saturating_sub(p.log_before),
            p.log_after.saturating_sub(p.log_before) as f64 / p.rels.max(1) as f64,
        );
        phases.push(p);
    }

    for p in &phases {
        eprintln!(
            "[relattrib] --- {} per-GROUP cost (us/rel against the seeked label sizes)",
            p.mode.name()
        );
        for (name, rels, secs, sl, dl) in &p.per_group {
            eprintln!(
                "[relattrib]   {name:<36} rels={rels:<8} {:>9.3} us/rel  srcLabel={sl} dstLabel={dl}",
                secs * 1e6 / (*rels).max(1) as f64
            );
        }
    }

    // Per-group operation table, from the traced statement of each group.
    for p in &phases {
        eprintln!("[relattrib] --- {} per-relationship operations", p.mode.name());
        let mut totals: BTreeMap<&'static str, u64> = BTreeMap::new();
        let mut traced_rels = 0u64;
        for (name, n, t) in &p.traced {
            traced_rels += n;
            for (k, v) in t {
                *totals.entry(k).or_default() += v;
            }
            let compact: Vec<String> = t
                .iter()
                .map(|(k, v)| format!("{k}={:.2}", *v as f64 / *n as f64))
                .collect();
            eprintln!("[relattrib]   {name:<34} n={n:<4} {}", compact.join(" "));
        }
        eprintln!(
            "[relattrib]   == over {traced_rels} traced rel(s), per relationship:"
        );
        for (k, v) in &totals {
            eprintln!(
                "[relattrib]      {:>10.3}  {k}",
                *v as f64 / traced_rels.max(1) as f64
            );
        }
    }

    // The subtraction: adjacent modes differ by exactly one thing.
    let by = |m: Mode| phases.iter().find(|p| p.mode == m);
    eprintln!("[relattrib] --- attribution (whole-pass wall, untraced)");
    let order = [Mode::Unwind, Mode::Match1, Mode::Match, Mode::Full];
    let mut prev: Option<&Phase> = None;
    for m in order {
        let Some(p) = by(m) else { continue };
        let us = p.rel_secs * 1e6 / p.rels.max(1) as f64;
        match prev {
            Some(q) => {
                let qus = q.rel_secs * 1e6 / q.rels.max(1) as f64;
                eprintln!(
                    "[relattrib]   {:<7} {us:>8.3} us/rel   delta over {:<7} {:>+8.3} us/rel",
                    m.name(),
                    q.mode.name(),
                    us - qus
                );
            }
            None => eprintln!("[relattrib]   {:<7} {us:>8.3} us/rel   (floor)", m.name()),
        }
        prev = Some(p);
    }
    if let Some(p) = by(Mode::Api) {
        eprintln!(
            "[relattrib]   {:<7} {:>8.3} us/rel   (create_rel with no Cypher)",
            "api",
            p.rel_secs * 1e6 / p.rels.max(1) as f64
        );
    }
}
