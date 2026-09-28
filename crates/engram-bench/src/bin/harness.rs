//! The converged harness — four engines, two workloads, one implementation.
//!
//! ```text
//! harness catalogue --dump <path>
//! harness plan --profile <p> --clients <n> --ops <m> --out <plan.jsonl>
//!              [--seed N] [--dataset synthetic|snb|snb-platform] [--keys N] [--nonce N]
//! harness lsqb <addr> --rig RIG --thread-cap N --cache-mb N
//!              [--engine bolt|pg] [--pg-user U --pg-db D]
//!              [--corpus sf1] [--queries q1,q3] [--timeout-secs N] [--json OUT]
//! harness stress <addr> <profile|all> <clients-csv> <seconds> --writes single|multi
//!              --rig RIG --thread-cap N --cache-mb N
//!              [--engine bolt|pg] [--plan PLAN] [--seed N] [--keys N]
//!              [--dataset D] [--json OUT]
//! harness report <result.json> [<result.json> …]
//! ```
//!
//! # Two lanes, and `--rig`
//!
//! Numbers are taken on two machines that are not comparable in either
//! direction — a pod under a 6-CPU quota on a shared node, and a dedicated
//! bench node with no quota — and both lanes are permanent. So `--rig` is
//! REQUIRED on both workloads, with no default and no inference, and
//! [`engram_bench::report::compare`] refuses a table whose documents disagree
//! about it. `harness report` exits non-zero on that refusal, because a
//! refusal that exits 0 gets redirected into a file and pasted.
//!
//! # `--thread-cap` and `--cache-mb`, and why they lost their defaults
//!
//! They are CLAIMS about a server this harness did not start, and they used to
//! default to 6 and 8192. A defaulted claim is not a missing field: it is a
//! plausible field describing a server that is not running, and the reporter
//! compares it for equality against another document's equally plausible one.
//! The 2026-09-08/09 Neo4j window passed `--cache-mb 10240` on its two stress
//! arms and nothing on its three LSQB batteries, so three documents stamped
//! 8192 against a pod configured with 10 GiB of page cache and every check
//! passed. Both flags are now REQUIRED on both workloads, and the value is
//! CHECKED against the engine wherever the engine will answer — see
//! [`engram_bench::fairness`].
//!
//! # What converged, and what did not
//!
//! LSQB and stress share EVERYTHING except where their operations come from:
//! connection handling, timing, percentiles, result checking, the
//! NOT-QUOTABLE refusal, the JSON document, the reporter. LSQB's op source is
//! a fixed nine-entry battery; stress's is a seeded generator or an emitted
//! plan. That is the whole difference, and it lives behind
//! [`engram_bench::plan::OpSource`] and this file's two `run_*` functions.
//!
//! They keep two things apart, deliberately:
//!
//! - **Sequence.** LSQB runs one statement at a time on a FRESH connection
//!   with a deadline, because a timed-out worker must never poison a later
//!   statement's session and the server may keep computing after one is
//!   abandoned. Stress holds one connection per client for the whole level,
//!   because reconnect cost inside a throughput window is not the engine's.
//! - **Verdict.** LSQB judges a COUNT against an existence probe and an
//!   expected value; stress judges a RATE against integrity reconciliation and
//!   the quotability rules. Merging those would produce a verdict that means
//!   neither.
//!
//! # `stress.rs` is still here, and that is deliberate
//!
//! This binary is a NEW baseline for the stress workload wherever level
//! scoping moves a value the statement interpolates — see
//! [`engram_bench::plan::LEVEL_STRIDE`]. `stress.rs` stays in the tree to
//! reproduce the recorded one, and
//! `tests/a_converged_plan_replays_the_stress_op_sequence.rs` holds the two
//! generators to the same ops.
//!
//! This binary needs real threads and a real clock, which the simulation
//! layer's `Runtime` deliberately does not provide — the same lint waiver
//! `stress`, `snbconc` and `lsqb` carry, for the same reason.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use engram_bench::backend::{Backend, BoltBackend, OpError, Params, PgBackend};
use engram_bench::catalogue::{Catalogue, Dialect, Status, has_unbound, render};
use engram_bench::fairness::{EngineFairness, FairnessCheck, FairnessStatus};
use engram_bench::plan::{
    ASSUMED_PEAK_OPS_PER_CLIENT_SEC, Fairness, LEVEL_STRIDE, LoadedPlan, OpSource, PlanOp,
    emit_plan, load_plan, required_ops_per_client, undersized_because,
};
use engram_bench::report::{
    FLOOR_STALL, KNOWN_RIGS, LevelResult, MIN_JUDGED_BUCKETS, QueryResult, Rig, RigCheck,
    RigStatus, RunReport, TREND_COLLAPSE, Workload, max_inflight, table,
};
use engram_bench::workload::{
    ChurnSet, Dataset, LevelSpec, PROFILES, Param, Profile, Reconciliation, bind_churn_anchor,
    profile, reconcile,
};

// ─── Small helpers ──────────────────────────────────────────────────────────

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Bindings as the catalogue renderer wants them.
fn bindings(params: &BTreeMap<String, Param>) -> Vec<(String, String)> {
    params
        .iter()
        .map(|(k, v)| (k.clone(), v.render()))
        .collect()
}

fn borrow(pairs: &[(String, String)]) -> Vec<(&str, &str)> {
    pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect()
}

/// Turn a plan op into the statement one dialect sends.
///
/// Returns `Err` when the catalogue declares the shape unsupported in this
/// dialect — which is an ANSWER, not a failure: the run records the reason and
/// refuses to quote the level rather than substituting something adjacent.
/// `--id-base N` (stress only): added to the `id` a `node_create` mints.
///
/// The generator mints `(cid << 40) | seq`, and the level stride adds
/// `level_index << 48`. So client 0 of every profile's FIRST level mints 0, 1,
/// 2, … — ids the SNB corpus already holds, because its messages are numbered
/// densely from 0. An engine that enforces the message key (PostgreSQL) refused
/// every one of those writes: write-only at 1 client acked 0 and refused 53,286
/// in the 2026-09-26 smoke run. The graph engines enforce nothing on
/// `Message.id`, so they accepted them and silently duplicated corpus ids. A
/// base above every corpus id (2^62) fixes both. It defaults to 0 so that every
/// run taken before it, and the generators' op-sequence agreement, are
/// unchanged; the catalogue is frozen and is not touched.
static ID_BASE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

fn id_base() -> u64 {
    ID_BASE.get().copied().unwrap_or(0)
}

fn render_op(
    cat: &Catalogue,
    dialect: Dialect,
    family: &str,
    op: &PlanOp,
) -> Result<String, String> {
    let mut pairs = bindings(op.params());
    if let PlanOp::Write { op: name, params, .. } = op {
        if name == "node_create" && id_base() != 0 {
            if let Some(Param::Uint(v)) = params.get("id") {
                let based = v.wrapping_add(id_base()).to_string();
                for pair in &mut pairs {
                    if pair.0 == "id" {
                        pair.1.clone_from(&based);
                    }
                }
            }
        }
    }
    let refs = borrow(&pairs);
    let entry = match op {
        PlanOp::Read { shape, .. } => cat
            .read_shape(shape, dialect)
            .map_err(|e| format!("{shape}: {e}"))?,
        PlanOp::Write { op: name, .. } => cat
            .write_op(name, family, dialect)
            .map_err(|e| format!("{name}: {e}"))?,
    };
    if let Status::Unsupported(reason) = &entry.status {
        return Err(format!(
            "{} is unsupported in {}: {reason}",
            op.shape().or(op.op()).unwrap_or("?"),
            dialect.key()
        ));
    }
    let rendered = render(&entry.text, &refs);
    if has_unbound(&rendered) {
        // A `${name}` that reached the wire is a binding the plan did not
        // carry. The engine's syntax error is a much worse way to find that
        // out than this is.
        return Err(format!(
            "{}: rendered statement still carries an unbound placeholder: {rendered}",
            op.shape().or(op.op()).unwrap_or("?")
        ));
    }
    Ok(rendered)
}

/// The latency past which a statement is logged individually, with its unix-ms
/// start, so the stalled seconds of a level can be attributed to WHICH
/// statements stalled — writes (a lock) or reads (the read path) — and to which
/// shape. `HARNESS_SLOW_MS` overrides the default of 250.
///
/// Set to `0` it logs EVERY statement, which is what the behaviour-preservation
/// check reads: `stress.rs`'s `STRESS_SLOW_MS=0` produces the same line, so the
/// two op streams can be diffed rather than argued about.
fn slow_ms() -> u64 {
    static SLOW: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *SLOW.get_or_init(|| {
        std::env::var("HARNESS_SLOW_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(250)
    })
}

/// The four dataset-level keys one dialect uses: schema, seed, attach, census.
///
/// Grouped rather than derived at four call sites because they must agree. The
/// census used to be read from `cypher_census` unconditionally and then skipped
/// on `sql`, which is how the SQL arm ended up with no re-seed guard at all —
/// a guard that exists for one dialect and is stepped around for another is
/// harder to notice than one that is simply absent.
fn dataset_keys(dialect: Dialect) -> (&'static str, &'static str, &'static str, &'static str) {
    match dialect {
        Dialect::Sql => ("sql_schema", "sql_seed", "sql_attach", "sql_census"),
        Dialect::CypherLadybug => (
            "cypher_ladybug_schema",
            "cypher_ladybug_seed",
            "cypher_ladybug_attach",
            "cypher_ladybug_census",
        ),
        // `cypher_engram` differs from `cypher` only in the few QUERY texts
        // where no single Cypher serves both Bolt engines. The corpus is the
        // same corpus and is loaded the same way, so schema, seed, attach and
        // census are deliberately shared: forking them would let the two
        // dialects measure different databases.
        Dialect::Cypher | Dialect::CypherEngram => (
            "cypher_schema",
            "cypher_seed",
            "cypher_attach",
            "cypher_census",
        ),
    }
}

/// Refuse a corpus statement that still carries a `${name}` the caller never
/// bound.
///
/// The same rule [`render_op`] applies to workload statements, applied to the
/// setup ones, and it is here because the setup path is where it was missing.
/// `sql_seed` bound `${keys_minus_1}`, which nothing supplies; the harness
/// issued the template forty times with those literal characters still in it
/// and PostgreSQL answered `syntax error at or near "$"` — an error that names
/// a character and not the cause. A placeholder that reaches the wire is a
/// binding somebody forgot, and the harness knows that before the engine does.
fn reject_unbound(what: &str, stmt: &str) -> Result<(), String> {
    if has_unbound(stmt) {
        return Err(format!(
            "{what} still carries an unbound `${{name}}` placeholder and was not sent: {stmt}"
        ));
    }
    Ok(())
}

/// Seed the synthetic corpus, or ATTACH to a corpus that is already loaded.
///
/// The synthetic dataset builds its own world so the harness runs anywhere,
/// including in CI — which is the only way a stress test becomes a regression
/// gate rather than an occasional ritual, and the only way a four-engine
/// concurrency comparison needs no corpus at all. The SNB dataset attaches,
/// because loading half a million nodes one statement at a time would measure
/// the loader.
///
/// # Why the schema step is on the SEEDING branch only
///
/// A graph engine needs no DDL to accept `CREATE (:Stress {k: 1})`; a
/// relational one does, so the synthetic dataset that "runs anywhere" ran
/// nowhere on the SQL arm until this executed `sql_schema`. The step is
/// deliberately unreachable from the ATTACH branch: `snb.sql_schema` is a
/// specified-but-unbuilt relational schema owned by the corpus loader, and a
/// harness that created half of it on the way past would leave a corpus that
/// answers queries and holds no rows.
///
/// Returns the key space to use.
fn seed_or_attach(
    cat: &Catalogue,
    dialect: Dialect,
    dataset: Dataset,
    keys: u64,
    control: &mut Box<dyn Backend>,
) -> Result<u64, String> {
    let (schema_key, seed_key, attach_key, census_key) = dataset_keys(dialect);
    let name = dataset.family().name();
    if let Some(attach) = cat
        .dataset_str(name, attach_key)
        .map_err(|e| e.to_string())?
    {
        let rows = control
            .run(&attach)
            .map_err(|e| format!("could not probe the {name} corpus: {e}"))?;
        if rows == 0 {
            return Err(format!(
                "the target holds no corpus — the `{name}` dataset ATTACHES to an \
                 already-loaded one (start it with `portserve <corpus dir> <addr>`)"
            ));
        }
        eprintln!("[harness] attached to a {name} corpus: {rows} person(s); --keys set from it");
        return Ok(rows);
    }
    let templates = cat
        .dataset_list(name, seed_key)
        .map_err(|e| e.to_string())?;
    if templates.is_empty() {
        return Ok(keys);
    }
    // The schema, before the census — the census reads a table the schema
    // creates, and on a fresh database a census that ran first would report
    // "relation does not exist" and be indistinguishable from an empty corpus.
    for stmt in cat
        .dataset_list(name, schema_key)
        .map_err(|e| e.to_string())?
    {
        reject_unbound(&format!("{name}.{schema_key}"), &stmt)?;
        control
            .run(&stmt)
            .map_err(|e| format!("could not create the {name} schema ({stmt}): {e}"))?;
    }
    // Census next: a corpus this harness has already seeded must not be
    // seeded twice — a second pass would double the population and every
    // recorded level after it would be measuring a different corpus.
    if let Some(census) = cat
        .dataset_str(name, census_key)
        .map_err(|e| e.to_string())?
    {
        if let Ok(n) = control.scalar(&census) {
            if n as u64 >= keys {
                eprintln!("[harness] {name} corpus already holds {n} node(s) — not re-seeding");
                return Ok(keys);
            }
        }
    }
    eprintln!("[harness] seeding {keys} nodes");
    let t0 = Instant::now();
    const BATCH: u64 = 500;
    for template in &templates {
        let mut made = 0u64;
        while made < keys {
            let n = BATCH.min(keys - made);
            let pairs = [
                ("lo".to_string(), made.to_string()),
                ("hi".to_string(), (made + n - 1).to_string()),
                ("keys".to_string(), keys.to_string()),
            ];
            let stmt = render(template, &borrow(&pairs));
            reject_unbound(&format!("{name}.{seed_key}"), &stmt)?;
            control
                .run(&stmt)
                .map_err(|e| format!("seed failed at {made}: {e}"))?;
            made += n;
        }
    }
    // Whatever the engine needs done ONCE after a load, attributed to itself
    // rather than to the first query that happens to draw it. For PostgreSQL
    // that is `ANALYZE`: a planner with no statistics is not the PostgreSQL
    // anybody means to compare against, and autovacuum's first pass would land
    // at an unpredictable point inside a measured level.
    let post_key = format!("{seed_key}_post");
    for stmt in cat
        .dataset_list(name, &post_key)
        .map_err(|e| e.to_string())?
    {
        reject_unbound(&format!("{name}.{post_key}"), &stmt)?;
        control
            .run(&stmt)
            .map_err(|e| format!("post-seed step failed ({stmt}): {e}"))?;
        eprintln!("[harness] post-seed: {stmt}");
    }
    eprintln!("[harness] seeded in {:.1}s", t0.elapsed().as_secs_f64());
    Ok(keys)
}

/// Everything a result document DECLARES about the conditions of a run, and
/// what checking each declaration against reality came to.
///
/// One struct because they travel together and are wrong together: a rig is
/// which machine, a fairness block is how the engine on it was configured,
/// and each has a companion saying whether anything could confirm it. Passing
/// them as four parameters is four chances to hand `run_lsqb` a check that
/// belongs to a different declaration.
struct Stamps {
    rig: Rig,
    rig_check: RigCheck,
    fairness: Fairness,
    fairness_check: FairnessCheck,
}

/// How a backend is built, once per client thread.
///
/// `Clone` because every place that opens a connection off the main thread — a
/// client thread, an LSQB worker — needs its own copy, and three hand-written
/// match arms that rebuilt the struct field by field is three places to forget
/// a field. One was already forgotten: `thread_cap` arrived and the LSQB
/// spawn arm would have kept the old shape.
#[derive(Clone)]
enum Target {
    Bolt(String),
    Pg {
        addr: String,
        user: String,
        database: String,
        /// The fairness thread cap, applied to every session this opens. See
        /// [`PgBackend::connect`] — a cap recorded in the document and not
        /// applied to the server is the one fairness mismatch the reporter's
        /// refusal cannot see.
        thread_cap: u32,
    },
}

impl Target {
    fn open(&self) -> Result<Box<dyn Backend>, OpError> {
        match self {
            Target::Bolt(addr) => Ok(Box::new(BoltBackend::connect(addr)?)),
            Target::Pg {
                addr,
                user,
                database,
                thread_cap,
            } => Ok(Box::new(PgBackend::connect(
                addr,
                user,
                database,
                Some(*thread_cap),
            )?)),
        }
    }
    fn addr(&self) -> &str {
        match self {
            Target::Bolt(a) => a,
            Target::Pg { addr, .. } => addr,
        }
    }
}

// ─── The LSQB lane ──────────────────────────────────────────────────────────

/// What one statement did on the wire.
enum Wire {
    Rows(Vec<Vec<engram_bench::backend::Cell>>, f64),
    Failed(String),
    TimedOut,
}

/// Run one statement on a FRESH connection in its own thread, with a deadline.
/// A fresh connection per statement means an abandoned (timed-out) worker can
/// never poison a later statement's session.
fn run_wire(target: &Target, stmt: &str, timeout: Duration) -> Wire {
    run_wire_with(target, stmt, &Params::new(), timeout)
}

/// As [`run_wire`], with BOUND parameters.
///
/// The three LDBC read batteries are parameterised, and a parameter is part of
/// the question: `bi12` with `languages ['en','de']` against a corpus carrying
/// `uz`/`tk`/`ar` is not a slow `bi12`, it is a different query that matches
/// nothing. So the parameters travel to the engine on the wire beside the
/// catalogue's unaltered bytes, and never into them.
fn run_wire_with(target: &Target, stmt: &str, params: &Params, timeout: Duration) -> Wire {
    let (tx, rx) = std::sync::mpsc::channel();
    let stmt = stmt.to_string();
    let params = params.clone();
    let spawn = target.clone();
    std::thread::spawn(move || {
        let outcome = (|| -> Result<(Vec<Vec<engram_bench::backend::Cell>>, f64), String> {
            let mut b = spawn.open().map_err(|e| format!("connect: {e}"))?;
            let t0 = Instant::now();
            let rows = b
                .query_with(&stmt, &params)
                .map_err(|e| format!("query: {e}"))?;
            Ok((rows, t0.elapsed().as_secs_f64() * 1000.0))
        })();
        // The receiver is gone iff the deadline already passed; nothing to do.
        let _ = tx.send(outcome);
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok((rows, ms))) => Wire::Rows(rows, ms),
        Ok(Err(e)) => Wire::Failed(e),
        Err(_) => Wire::TimedOut,
    }
}

/// Extract the single COUNT an LSQB query returns. Anything but exactly one
/// single-integer-column row is an error, stated — a count query that returns
/// two rows is not a count query.
fn extract_count(rows: &[Vec<engram_bench::backend::Cell>]) -> Result<i64, String> {
    match rows {
        [row] => match row.as_slice() {
            [c] => c
                .as_int()
                .ok_or_else(|| format!("query returned a non-integer count: {c:?}")),
            other => Err(format!(
                "query returned {} columns, expected 1",
                other.len()
            )),
        },
        [] => Err("query returned no rows (count(*) must return exactly one)".to_string()),
        more => Err(format!("query returned {} rows, expected 1", more.len())),
    }
}

/// What the existence probe established.
enum Probe {
    Exists,
    Absent,
    Failed(String),
}

/// Judge a measured count against the probe and any expected value.
///
/// Pure, and deliberately fail-closed — `lsqb.rs`'s rule, unchanged: the only
/// zeros that pass are ones whose absence the probe PROVED.
fn judge(count: i64, probe: &Probe, expected: Option<i64>) -> (&'static str, Option<String>) {
    if let Some(e) = expected {
        if count != e {
            return (
                "mismatch",
                Some(format!("expected {e} (catalogue), measured {count}")),
            );
        }
    }
    if count == 0 {
        return match probe {
            Probe::Exists => (
                "zero_on_populated",
                Some("count is 0 but the existence probe found the pattern".to_string()),
            ),
            Probe::Absent => (
                "ok",
                Some("pattern provably absent (existence probe returned no row)".to_string()),
            ),
            Probe::Failed(e) => (
                "unverified_zero",
                Some(format!(
                    "count is 0 and the existence probe could not prove absence: {e}"
                )),
            ),
        };
    }
    match probe {
        Probe::Absent => (
            "inconsistent",
            Some(format!(
                "count is {count} but the existence probe found no row — the engine \
                 disagrees with itself"
            )),
        ),
        Probe::Failed(e) => (
            "ok",
            Some(format!(
                "count is non-zero; note: existence probe failed ({e})"
            )),
        ),
        Probe::Exists => ("ok", None),
    }
}

/// Derive the existence probe from a Cypher count query: the same pattern,
/// WHERE clauses included, with the aggregate replaced by `RETURN 1 LIMIT 1`.
/// An anti-join query's zero is only provable with the NOT applied.
fn cypher_probe(text: &str) -> Option<String> {
    text.strip_suffix("RETURN count(*) AS count")
        .map(|prefix| format!("{prefix}RETURN 1 LIMIT 1"))
}

/// The battery, against one engine.
///
/// The corpus is NOT a separate parameter: it is `rig.scale`, the same string
/// the expected counts are keyed on. Passing both would let a document claim
/// `sf1` in its rig while checking its counts against `sf10`, which is a
/// disagreement no reader could see and no check would catch.
/// One parameterised LDBC read battery: SNB BI, SNB Interactive, or FinBench.
///
/// # Why this is not `run_lsqb` with a different catalogue path
///
/// LSQB's nine queries take no parameters and each returns a single `count(*)`,
/// so `run_lsqb` judges a scalar against a published oracle. These three
/// batteries do neither: every query is parameterised, and every one returns a
/// PROJECTION — an ordered, limited row set whose shape the catalogue declares.
/// The measured quantity is therefore the row count and the wall time, and the
/// oracle is the other dialect's answer rather than a published integer.
///
/// # An empty result is not a pass
///
/// The one judgement this lane exists to make. `bi12` ran with
/// `languages ['en','de']` against a corpus carrying `uz`/`tk`/`ar` and matched
/// nothing; `bi16` ran on dates its tag had no messages for, twice. Both
/// returned a well-formed empty answer in a fraction of the time the real query
/// takes, and both were written into a comparison table as results.
///
/// So zero rows is `empty`, never `ok`, and the detail says the parameters may
/// be the cause. A query that legitimately returns nothing is then a deliberate
/// judgement someone records, not a default.
#[allow(clippy::too_many_arguments)]
fn run_family(
    target: &Target,
    dialect: Dialect,
    family: &'static engram_bench::catalogue::Family,
    stamps: Stamps,
    selected: Option<Vec<String>>,
    supplied: &BTreeMap<String, BTreeMap<String, String>>,
    variant_filter: Option<String>,
    temporal_encoding: engram_bench::params::TemporalEncoding,
    continue_after_timeout: bool,
    // `--show-rows N`: print each answer's first N rows. A row COUNT is what
    // the document keeps, and two engines can agree on a count while
    // disagreeing on every value -- the value-level check a catalogue entry
    // needs before it may say `verified` reads these.
    show_rows: usize,
) -> RunReport {
    let Stamps {
        rig,
        rig_check,
        fairness,
        fairness_check,
    } = stamps;
    let timeout = Duration::from_secs(fairness.seconds);
    let mut out = Vec::new();
    let mut failures = Vec::new();
    // Set by the FIRST timeout and never cleared. See `timed_out` below.
    let mut abandoned: Option<String> = None;

    let (engine, version) = match target.open() {
        Ok(b) => (b.engine().to_string(), b.version().to_string()),
        Err(e) => {
            failures.push(format!("cannot reach {}: {e}", target.addr()));
            ("unreachable".to_string(), String::new())
        }
    };
    let ident = (engine, version, dialect, target.addr().to_string());

    let cat = match family.load() {
        Ok(c) => c,
        Err(e) => {
            failures.push(format!("catalogue {}: {e}", family.name));
            return finish_family(
                family,
                ident,
                rig,
                rig_check,
                fairness,
                fairness_check,
                out,
                failures,
            );
        }
    };
    let path = family.queries_path;
    let names = match cat.query_names(path) {
        Ok(n) => n,
        Err(e) => {
            failures.push(e.to_string());
            Vec::new()
        }
    };

    // Census: refuse the vacuous pass BEFORE measuring anything. An empty
    // corpus answers nothing to every query, which would read as twenty fast
    // `empty` rows rather than as an unloaded database.
    let census = match run_wire(target, census_stmt(dialect), timeout) {
        Wire::Rows(rows, _) => extract_count(&rows).unwrap_or(0),
        Wire::Failed(e) => {
            failures.push(format!("census failed against {}: {e}", target.addr()));
            0
        }
        Wire::TimedOut => {
            failures.push(format!("census timed out after {}s", timeout.as_secs()));
            0
        }
    };
    if census <= 0 {
        failures.push(format!(
            "corpus is empty ({census} node(s)) -- nothing to measure"
        ));
    } else {
        eprintln!("[harness] corpus: {census} node(s)");
    }

    for name in names.iter().filter(|_| census > 0) {
        if let Some(sel) = &selected {
            if !sel.contains(name) {
                continue;
            }
        }
        let entry = match cat.query(path, name, dialect) {
            Ok(e) => e,
            Err(e) => {
                failures.push(e.to_string());
                continue;
            }
        };
        let variants = match cat.variants(path, name) {
            Ok(v) => v,
            Err(e) => {
                failures.push(e.to_string());
                continue;
            }
        };
        if let Status::Unsupported(reason) = &entry.status {
            // PRINTED, not merely recorded. A declared gap that scrolls past
            // in silence reads as a query the battery forgot, and the reader
            // then goes looking for a run that never was.
            eprintln!("[harness] {name:<10} {:>10} {:>12}  unmappable", "-", "-");
            out.push(QueryResult {
                query: name.clone(),
                statement: None,
                count: None,
                millis: None,
                status: "unmappable".into(),
                probe: "skipped".into(),
                detail: Some(reason.clone()),
                expected: None,
                catalogue_status: entry.status.as_str().into(),
            });
            continue;
        }
        // Each curated variant is its own question and gets its own row.
        let chosen: Vec<_> = variants
            .iter()
            .filter(|(label, _)| match &variant_filter {
                Some(f) => f == label,
                None => true,
            })
            .collect();
        if chosen.is_empty() {
            out.push(QueryResult {
                query: name.clone(),
                statement: None,
                count: None,
                millis: None,
                status: "error".into(),
                probe: "skipped".into(),
                detail: Some(format!(
                    "no variant matched `{}`; this query declares {}",
                    variant_filter.clone().unwrap_or_default(),
                    variants
                        .iter()
                        .map(|(l, _)| l.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                expected: None,
                catalogue_status: entry.status.as_str().into(),
            });
            continue;
        }
        for (label, specs) in chosen {
            // ── A CEILING BOUNDS THE CLIENT, NOT THE SERVER ──────────────
            //
            // `run_wire_with` abandons its worker on a deadline; the server
            // keeps computing. Everything measured after that shares the
            // machine with a query nobody is waiting for, and the numbers are
            // not of the thing they name.
            //
            // MEASURED, not inferred. On SF3 2026-09-21: bi7 answered in
            // 184 ms in one run and hit the 300 s ceiling in the next, and
            // bi8a/bi8b went from 54 s each to the ceiling — the only
            // difference being that bi6 had timed out just before them and was
            // still running. Node load stayed at 7.46 with no client attached.
            //
            // `measurements/pod/README.md` rule 7 already says to restart the
            // server after every overrun. This lane does not own the server,
            // so it stops instead of quietly producing contaminated rows.
            if let Some(first) = &abandoned {
                if !continue_after_timeout {
                    out.push(QueryResult {
                        query: key_of(name, label, variants.len()),
                        statement: None,
                        count: None,
                        millis: None,
                        status: "abandoned-upstream".into(),
                        probe: "skipped".into(),
                        detail: Some(format!(
                            "not measured: `{first}` hit the ceiling and its worker was                              abandoned, so the server may still be computing it. A number                              taken now is not a number for this query. Restart the server                              and re-run from here, or pass --continue-after-timeout to                              measure anyway and have every later row recorded as                              contaminated."
                        )),
                        expected: None,
                        catalogue_status: entry.status.as_str().into(),
                    });
                    continue;
                }
            }
            let key = if variants.len() > 1 {
                engram_bench::catalogue::variant_key(name, label)
            } else {
                name.clone()
            };
            // The parameter file is keyed by variant where one exists, and by
            // the bare query name otherwise.
            let vals = supplied
                .get(&key)
                .or_else(|| supplied.get(name))
                .cloned()
                .unwrap_or_default();
            // The PostgreSQL arm loaded raw Datagen CSV and carries LDBC's
            // own ids; engram and the Neo4j arm share the corpus's dense ones.
            // Same question, two identifier spaces -- so the parameter file
            // carries both and the DIALECT chooses, rather than the query text
            // being forked per engine.
            let ids = if dialect == Dialect::Sql {
                engram_bench::params::IdSpace::Ldbc
            } else {
                engram_bench::params::IdSpace::Dense
            };
            let bound = match engram_bench::params::bind(&key, specs, &vals, temporal_encoding, ids)
            {
                Ok(b) => b,
                Err(e) => {
                    // NOT a skip. A battery that silently drops the queries it
                    // could not parameterise reports on a shrinking subset and
                    // calls it a pass.
                    out.push(QueryResult {
                        query: key.clone(),
                        statement: None,
                        count: None,
                        millis: None,
                        status: "unparameterised".into(),
                        probe: "skipped".into(),
                        detail: Some(e),
                        expected: None,
                        catalogue_status: entry.status.as_str().into(),
                    });
                    continue;
                }
            };
            // BINDING is the mechanism, and the rest of this lane assumes it:
            // the engine plans the catalogue's own bytes and the parameters
            // travel beside them on the wire.
            //
            // `snb-interactive` is the exception, and a real one. Its Cypher
            // was transcribed from an upstream that SUBSTITUTES -- it carries
            // `${personId}`, and `'${firstName}'` with the quotes already in
            // the template -- where `snb-bi`'s carries native `$tagA`. A lane
            // that could only bind would report all 21 of its queries as
            // unrunnable, which is the harness's limitation reported as the
            // catalogue's.
            //
            // So a template that demands substitution gets it, the document
            // SAYS which of the two happened, and the values pass through the
            // same coercion either way -- so a DATE against an
            // epoch-millisecond corpus substitutes as the integer the column
            // actually holds, not as a date string that would match nothing.
            let renders = has_unbound(&entry.text);
            let (stmt, wire_params) = if renders {
                let pairs: Vec<(&str, String)> = bound
                    .iter()
                    .map(|(k, v)| (k.as_str(), engram_bench::params::render_text(v)))
                    .collect();
                let refs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
                (render(&entry.text, &refs), Params::new())
            } else {
                (entry.text.clone(), bound.clone())
            };
            if has_unbound(&stmt) {
                out.push(QueryResult {
                    query: key.clone(),
                    statement: Some(stmt.clone()),
                    count: None,
                    millis: None,
                    status: "unparameterised".into(),
                    probe: "skipped".into(),
                    detail: Some(
                        "a `${...}` placeholder survived substitution: the catalogue's text                          names a parameter its own `parameters` block does not declare"
                            .into(),
                    ),
                    expected: None,
                    catalogue_status: entry.status.as_str().into(),
                });
                continue;
            }
            let bound_label = format!(
                "{} {}",
                if renders { "rendered" } else { "bound" },
                bound
                    .iter()
                    .map(|(k, v)| format!("{k}={v:?}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            let r = match run_wire_with(target, &stmt, &wire_params, timeout) {
                Wire::TimedOut => QueryResult {
                    query: key.clone(),
                    statement: Some(stmt.clone()),
                    count: None,
                    millis: Some(timeout.as_secs_f64() * 1000.0),
                    status: "timeout".into(),
                    probe: bound_label,
                    detail: Some(format!(
                        "no answer within {}s; the worker was abandoned and the server may \
                         still be computing -- restart it before the next statement",
                        timeout.as_secs()
                    )),
                    expected: None,
                    catalogue_status: entry.status.as_str().into(),
                },
                Wire::Failed(e) => {
                    // A temporal function applied to an integer is not a
                    // defect in the query -- it is the CORPUS's typing. Say so
                    // once, plainly, instead of leaking five different engine
                    // type errors that each look like a separate bug.
                    let (status, detail) = classify_failure(e, temporal_encoding);
                    QueryResult {
                        query: key.clone(),
                        statement: Some(stmt.clone()),
                        count: None,
                        millis: None,
                        status,
                        probe: bound_label,
                        detail: Some(detail),
                        expected: None,
                        catalogue_status: entry.status.as_str().into(),
                    }
                }
                Wire::Rows(rows, ms) => {
                    for (i, row) in rows.iter().take(show_rows).enumerate() {
                        eprintln!("[harness]   {key} row {}: {row:?}", i + 1);
                    }
                    let n = i64::try_from(rows.len()).unwrap_or(i64::MAX);
                    let (status, detail) = if n == 0 {
                        (
                            "empty",
                            Some(
                                "zero rows. An empty result is indistinguishable from a working \
                                 query, and is usually a parameter that matches nothing in this \
                                 corpus -- check the bound values against it before quoting this \
                                 as a timing."
                                    .to_string(),
                            ),
                        )
                    } else {
                        ("ok", None)
                    };
                    QueryResult {
                        query: key.clone(),
                        statement: Some(stmt.clone()),
                        count: Some(n),
                        millis: Some((ms * 1000.0).round() / 1000.0),
                        status: status.into(),
                        probe: bound_label,
                        detail,
                        expected: None,
                        catalogue_status: entry.status.as_str().into(),
                    }
                }
            };
            let mut r = r;
            // Latch the FIRST timeout. Later rows are either refused above or,
            // with --continue-after-timeout, taken and marked.
            if r.status == "timeout" && abandoned.is_none() {
                abandoned = Some(r.query.clone());
                eprintln!(
                    "[harness] {} hit the {}s ceiling. Its worker was abandoned and the                      SERVER MAY STILL BE COMPUTING IT -- every later measurement would                      share the machine with it.{}",
                    r.query,
                    timeout.as_secs(),
                    if continue_after_timeout {
                        " Continuing anyway; later rows are recorded as contaminated."
                    } else {
                        " Stopping here. Restart the server and re-run from this query."
                    }
                );
            } else if abandoned.is_some() {
                // Taken under --continue-after-timeout: the number exists, and
                // it is not a number for this query alone.
                r.status = format!("{}-contaminated", r.status);
                let first = abandoned.clone().unwrap_or_default();
                r.detail = Some(match r.detail.take() {
                    Some(d) => format!("measured while `{first}` was still running: {d}"),
                    None => format!(
                        "measured while `{first}` was still running on the server, so this                          figure includes contention from a query nobody was waiting for"
                    ),
                });
            }
            eprintln!(
                "[harness] {:<10} {:>10} {:>12}  {}",
                r.query,
                r.millis.map_or("-".into(), |m| format!("{m:.0}ms")),
                r.count.map_or("-".into(), |c| format!("{c} row(s)")),
                r.status
            );
            out.push(r);
        }
    }
    finish_family(
        family,
        ident,
        rig,
        rig_check,
        fairness,
        fairness_check,
        out,
        failures,
    )
}

/// Read a parameter file: `{"<query>": {"<parameter>": "<text value>"}}`.
///
/// Values stay TEXT here and are coerced by [`engram_bench::params::coerce`]
/// against the type the catalogue declares. That ordering is deliberate: JSON
/// has no date type, so a file that carried typed values would have to guess
/// which strings are dates -- which is the guess that turns a DATE into a
/// string comparison against a temporal column and matches nothing.
///
/// A JSON number is accepted and stringified, so an id may be written `933`
/// rather than `"933"`.
///
/// # Errors
/// If the file cannot be read, is not a JSON object of objects, or holds a
/// value that is not a string, number or array of those.
type ParamFile = (
    BTreeMap<String, BTreeMap<String, String>>,
    engram_bench::params::TemporalEncoding,
);

fn read_params_file(path: &str) -> Result<ParamFile, String> {
    use engram_cypher::Value;
    let src = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let v = engram_cypher::json::from_json(&src).map_err(|e| format!("{e:?}"))?;
    let Value::Map(top) = v else {
        return Err("the file is not a JSON object".into());
    };
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    // Keys beginning `_` are the file's own metadata, not a query.
    let mut encoding = engram_bench::params::TemporalEncoding::Typed;
    if let Some(Value::Str(e)) = top.get("_temporal_encoding") {
        encoding = engram_bench::params::TemporalEncoding::parse(e).ok_or_else(|| {
            format!("`_temporal_encoding` is `{e}`, which is not `typed` or `epoch_millis`")
        })?;
    }
    for (query, qv) in top {
        if query.starts_with('_') {
            continue;
        }
        let Value::Map(pm) = qv else {
            return Err(format!("`{query}` is not an object of parameters"));
        };
        let mut inner = BTreeMap::new();
        for (name, pv) in pm {
            let text = match &pv {
                Value::Str(s) => s.clone(),
                Value::Int(n) => n.to_string(),
                Value::Float(f) => f.to_string(),
                Value::Bool(b) => b.to_string(),
                // An array is joined with LDBC's own separator, so a file may
                // write a STRING[] either way round.
                Value::List(items) => items
                    .iter()
                    .map(|i| match i {
                        Value::Str(s) => Ok(s.clone()),
                        Value::Int(n) => Ok(n.to_string()),
                        Value::Float(f) => Ok(f.to_string()),
                        other => Err(format!(
                            "{query}.{name}: a list element is not a string or number ({other:?})"
                        )),
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .join(";"),
                other => {
                    return Err(format!(
                        "{query}.{name}: not a string, number or array ({other:?})"
                    ));
                }
            };
            inner.insert(name, text);
        }
        out.insert(query, inner);
    }
    Ok((out, encoding))
}

/// Turn an engine failure into a status a reader can act on.
///
/// Two failure families here are NOT defects in the query, and reporting them
/// as a bare `error` makes each look like a separate bug to chase:
///
/// * **Corpus typing.** The ordinary SNB load stores `creationDate` as an
///   epoch-millisecond integer, and LDBC's Cypher calls `date()`, reads
///   `.year`, and adds a `duration` to it. Five of the twenty BI queries fail
///   this way and NONE of them can run on such a corpus whatever their
///   parameters are. The fix is a corpus, not a parameter:
///   `docs/bench/snb-datetime-corpus-build.sh`, and `ldbc-coverage-plan.md`
///   0.3.0 records why the two SNB families need different typings.
///
/// * **A procedure the engine does not implement.** LDBC's Neo4j texts reach
///   for APOC and GDS. That is a capability gap, declared, and it is the same
///   class as bi15/19/20's `unsupported` status -- not a run that went wrong.
fn classify_failure(e: String, enc: engram_bench::params::TemporalEncoding) -> (String, String) {
    let lower = e.to_ascii_lowercase();
    // The query wants TYPED temporals and the corpus stores integers.
    let wants_typed = lower.contains("takes a string or map, got integer")
        || lower.contains("got integer vs duration")
        || (lower.contains("type error in property access") && lower.contains("got integer"));
    if wants_typed && enc == engram_bench::params::TemporalEncoding::EpochMillis {
        return (
            "corpus-typing".into(),
            format!(
                "this query applies a temporal function to `creationDate`, which THIS corpus                  stores as an epoch-millisecond integer. No parameter can fix it: the query                  needs a corpus loaded with typed temporals                  (docs/bench/snb-datetime-corpus-build.sh; ldbc-coverage-plan.md 0.3.0).                  Engine said: {e}"
            ),
        );
    }
    // THE MIRROR CASE, and it is not hypothetical. `snb-interactive`'s Cypher
    // was transcribed for an epoch-millisecond corpus: IC10 builds
    // `datetime({epochMillis: friend.birthday})` and IC7 subtracts one
    // timestamp from another and divides by 1000. Against a TYPED corpus both
    // are type errors.
    //
    // So the two SNB families want OPPOSITE corpus typings, which is what
    // ldbc-coverage-plan.md 0.3.0 says and what this pair measured.
    let wants_millis = lower.contains("epochmillis must be an integer")
        || lower.contains("got datetime vs datetime")
        || (lower.contains("type error in sub") && lower.contains("datetime"));
    if wants_millis && enc == engram_bench::params::TemporalEncoding::Typed {
        return (
            "corpus-typing".into(),
            format!(
                "this query does ARITHMETIC on a temporal -- subtracting two timestamps, or                  building one from `epochMillis` -- which needs a corpus storing them as                  epoch-millisecond integers. THIS corpus stores typed temporals. No parameter                  can fix it, and the fix is not the same corpus SNB BI needs: the two                  families want opposite typings (ldbc-coverage-plan.md 0.3.0).                  Engine said: {e}"
            ),
        );
    }
    if lower.contains("not supported yet: procedure") {
        return (
            "unimplemented-procedure".into(),
            format!(
                "the catalogue's text calls a procedure this engine does not implement. That is                  a declared capability gap of the same kind as bi15/19/20's GDS dependency, not                  a failed measurement. Engine said: {e}"
            ),
        );
    }
    ("error".into(), e)
}

/// The key one curated variant is recorded under, or the bare query name when
/// there is only one. Shared by the measuring path and the abandoned-upstream
/// path so a skipped row carries the same name a measured one would have.
fn key_of(name: &str, label: &str, variant_count: usize) -> String {
    if variant_count > 1 {
        engram_bench::catalogue::variant_key(name, label)
    } else {
        name.to_string()
    }
}

/// The node census, per dialect. One statement, one integer.
fn census_stmt(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::Sql => "SELECT count(*) AS n FROM person",
        _ => "MATCH (n) RETURN count(n) AS n",
    }
}

/// Assemble a family battery's document.
///
/// Split out of [`run_family`] only because the early-return paths need it too:
/// a catalogue that will not load still produces a document that says so,
/// rather than no document at all.
#[allow(clippy::too_many_arguments)]
fn finish_family(
    family: &'static engram_bench::catalogue::Family,
    ident: (String, String, Dialect, String),
    rig: Rig,
    rig_check: RigCheck,
    fairness: Fairness,
    fairness_check: FairnessCheck,
    out: Vec<QueryResult>,
    mut failures: Vec<String>,
) -> RunReport {
    // A status that is neither ok nor unmappable fails the run -- and that
    // deliberately includes `empty` and `unparameterised`. Both are the shapes
    // that previously passed as results.
    for q in &out {
        if q.status != "ok" && q.status != "unmappable" {
            failures.push(format!("{}: {}", q.query, q.status));
        }
    }
    let workload = match family.name {
        "snb-bi" => Workload::SnbBi,
        "snb-interactive" => Workload::SnbInteractive,
        _ => Workload::Finbench,
    };
    let (engine, engine_version, dialect, addr) = ident;
    RunReport {
        workload,
        engine,
        engine_version,
        dialect: dialect.key().to_string(),
        addr,
        dataset: "snb".into(),
        corpus: rig.scale.clone(),
        seed: 0,
        keys: 0,
        catalogue_digest: family.digest(),
        plan_sha256: None,
        plan_emitter: None,
        op_source: "battery".into(),
        writes_mode: "n/a".into(),
        rig,
        rig_check,
        fairness,
        fairness_check,
        levels: Vec::new(),
        queries: out,
        integrity: Vec::new(),
        failures,
    }
}

fn run_lsqb(
    target: &Target,
    dialect: Dialect,
    cat: &Catalogue,
    stamps: Stamps,
    selected: Option<Vec<String>>,
) -> RunReport {
    let Stamps {
        rig,
        rig_check,
        fairness,
        fairness_check,
    } = stamps;
    // Derived rather than passed alongside: `fairness.seconds` IS this
    // timeout, and two parameters carrying one number is two chances for the
    // number the document records and the number the run enforced to differ.
    let timeout = Duration::from_secs(fairness.seconds);
    let scale = rig.scale.clone();
    let corpus: &str = &scale;
    let mut out = Vec::new();
    let mut failures = Vec::new();
    let names = match cat.lsqb_names() {
        Ok(n) => n,
        Err(e) => {
            failures.push(e.to_string());
            Vec::new()
        }
    };
    let (engine, version) = match target.open() {
        Ok(b) => (b.engine().to_string(), b.version().to_string()),
        Err(e) => {
            failures.push(format!("cannot reach {}: {e}", target.addr()));
            ("unreachable".to_string(), String::new())
        }
    };
    // ── Census: refuse the vacuous pass BEFORE measuring anything ──────────
    //
    // An empty corpus answers 0 to every query and would pass every check that
    // does not look. This runs first, and a failure here abandons the run
    // rather than producing nine well-formed zeros.
    let census_ok = match cat.lsqb_census(dialect) {
        Err(e) => {
            failures.push(format!("census: {e}"));
            false
        }
        Ok((nodes_stmt, persons_stmt)) => {
            let read = |stmt: &str| -> Result<i64, String> {
                match run_wire(target, stmt, timeout) {
                    Wire::Rows(rows, _) => extract_count(&rows),
                    Wire::Failed(e) => Err(e),
                    Wire::TimedOut => Err(format!("census timed out after {}s", timeout.as_secs())),
                }
            };
            match (read(&nodes_stmt), read(&persons_stmt)) {
                (Ok(n), Ok(p)) if n > 0 && p > 0 => {
                    eprintln!("[harness] corpus: {n} node(s), {p} person(s)");
                    true
                }
                (Ok(n), Ok(p)) => {
                    failures.push(format!(
                        "corpus is empty ({n} node(s), {p} person(s)) — nothing to measure"
                    ));
                    false
                }
                (Err(e), _) | (_, Err(e)) => {
                    failures.push(format!("census failed against {}: {e}", target.addr()));
                    false
                }
            }
        }
    };
    for name in names.iter().filter(|_| census_ok) {
        if let Some(sel) = &selected {
            if !sel.contains(name) {
                continue;
            }
        }
        let entry = match cat.lsqb(name, dialect) {
            Ok(e) => e,
            Err(e) => {
                failures.push(e.to_string());
                continue;
            }
        };
        if let Status::Unsupported(reason) = &entry.status {
            // Declared, never dropped. `unmappable` keeps the run's PASS
            // alive; a run in which NOTHING measured `ok` still fails.
            out.push(QueryResult {
                query: name.clone(),
                statement: None,
                count: None,
                millis: None,
                status: "unmappable".into(),
                probe: "skipped".into(),
                detail: Some(reason.clone()),
                expected: cat.lsqb_expected(name, corpus).ok().flatten(),
                catalogue_status: entry.status.as_str().into(),
            });
            continue;
        }
        let (count_stmt, probe_stmt) = match dialect {
            Dialect::Sql => (
                cat.sql_count(&entry.text).unwrap_or_default(),
                cat.sql_probe(&entry.text).unwrap_or_default(),
            ),
            _ => (
                entry.text.clone(),
                cypher_probe(&entry.text).unwrap_or_default(),
            ),
        };
        // The probe FIRST, so a later zero is judged against evidence gathered
        // before the measured run. Its wall time travels in its label: a probe
        // that takes 180 s behind a 0.6 s count is the kind of number that hid
        // for three days when only the count's millis were reported.
        let (probe, probe_label) = match run_wire(target, &probe_stmt, timeout) {
            Wire::Rows(rows, ms) if rows.is_empty() => (Probe::Absent, format!("absent {ms:.0}ms")),
            Wire::Rows(_, ms) => (Probe::Exists, format!("exists {ms:.0}ms")),
            Wire::Failed(e) => {
                let label = format!("failed: {e}");
                (Probe::Failed(e), label)
            }
            Wire::TimedOut => (
                Probe::Failed("probe timed out".to_string()),
                "failed: timeout".to_string(),
            ),
        };
        let expected = cat.lsqb_expected(name, corpus).ok().flatten();
        let r = match run_wire(target, &count_stmt, timeout) {
            Wire::TimedOut => QueryResult {
                query: name.clone(),
                statement: Some(count_stmt.clone()),
                count: None,
                millis: Some(timeout.as_secs_f64() * 1000.0),
                status: "timeout".into(),
                probe: probe_label,
                detail: Some(format!(
                    "no answer within {}s; the worker was abandoned and the server may \
                     still be computing",
                    timeout.as_secs()
                )),
                expected,
                catalogue_status: entry.status.as_str().into(),
            },
            Wire::Failed(e) => QueryResult {
                query: name.clone(),
                statement: Some(count_stmt.clone()),
                count: None,
                millis: None,
                status: "error".into(),
                probe: probe_label,
                detail: Some(e),
                expected,
                catalogue_status: entry.status.as_str().into(),
            },
            Wire::Rows(rows, ms) => match extract_count(&rows) {
                Err(e) => QueryResult {
                    query: name.clone(),
                    statement: Some(count_stmt.clone()),
                    count: None,
                    millis: Some((ms * 1000.0).round() / 1000.0),
                    status: "error".into(),
                    probe: probe_label,
                    detail: Some(e),
                    expected,
                    catalogue_status: entry.status.as_str().into(),
                },
                Ok(count) => {
                    let (status, detail) = judge(count, &probe, expected);
                    QueryResult {
                        query: name.clone(),
                        statement: Some(count_stmt.clone()),
                        count: Some(count),
                        millis: Some((ms * 1000.0).round() / 1000.0),
                        status: status.into(),
                        probe: probe_label,
                        detail,
                        expected,
                        catalogue_status: entry.status.as_str().into(),
                    }
                }
            },
        };
        eprintln!(
            "[harness] {:<3} {:<18} count={:<14} {:>10} ms  probe={}{}",
            r.query,
            r.status,
            r.count.map(|c| c.to_string()).unwrap_or_else(|| "-".into()),
            r.millis
                .map(|m| format!("{m:.1}"))
                .unwrap_or_else(|| "-".into()),
            r.probe,
            r.detail
                .as_deref()
                .map(|d| format!("  ({d})"))
                .unwrap_or_default(),
        );
        out.push(r);
    }
    // A status that is neither ok nor unmappable fails the run.
    for q in &out {
        if q.status != "ok" && q.status != "unmappable" {
            failures.push(format!("{}: {}", q.query, q.status));
        }
    }
    RunReport {
        workload: Workload::Lsqb,
        engine,
        engine_version: version,
        dialect: dialect.key().to_string(),
        addr: target.addr().to_string(),
        dataset: "snb".into(),
        corpus: corpus.to_string(),
        seed: 0,
        keys: 0,
        catalogue_digest: engram_bench::catalogue::digest(),
        plan_sha256: None,
        plan_emitter: None,
        op_source: "battery".into(),
        // LSQB is read-only, so the writer count cannot have changed it; the
        // field is stamped anyway because a row without it is uninterpretable
        // and "n/a" is more honest than an absent key.
        writes_mode: "n/a".into(),
        // The rig has NO "n/a". Read-only or not, the battery still ran on a
        // machine, and the cross-engine table this workload exists to produce
        // is precisely the table a lane blend would corrupt.
        rig,
        rig_check,
        fairness,
        fairness_check,
        levels: Vec::new(),
        queries: out,
        integrity: Vec::new(),
        failures,
    }
}

// ─── The stress lane ────────────────────────────────────────────────────────

/// What one client thread brought back.
#[derive(Default)]
struct Samples {
    reads: Vec<u64>,
    writes: Vec<u64>,
    /// `(start, end)` in microseconds since the level's release, for
    /// [`max_inflight`].
    spans: Vec<(u64, u64)>,
    errors: u64,
    refusals: u64,
    refusal_kinds: BTreeMap<String, u64>,
    per_shape: BTreeMap<String, Vec<u64>>,
    /// Acked churn creates and deletes, per the plan's op names.
    churn_creates: u64,
    churn_deletes: u64,
    /// When this client's replayed plan ran out, microseconds since the level
    /// was released. `None` means it did not.
    ///
    /// A bool says a plan was too small. The MOMENT says how much too small,
    /// which is the difference between "re-emit with a larger plan" and
    /// "re-emit with `--ops 50000`".
    exhausted_us: Option<u64>,
    /// A statement the catalogue could not render in this dialect. Fatal for
    /// the level: a workload with a shape removed is not the workload.
    unrenderable: Option<String>,
}

/// Normalise a refusal message into a histogram bucket.
///
/// The buckets are the ones the four engines actually produce. Anything
/// unrecognised lands in `other` WITH its message truncated, rather than being
/// dropped: an unknown refusal is a finding, and a histogram that silently
/// discarded it would be the bare count this exists to replace.
fn refusal_kind(msg: &str) -> String {
    let m = msg.to_ascii_lowercase();
    if m.contains("already exists") || m.contains("uniqueness") || m.contains("duplicate") {
        "duplicate-key".into()
    } else if m.contains("transaction conflict") || m.contains("write-write") {
        "write-conflict".into()
    } else if m.contains("cannot start a new write transaction") {
        "single-writer".into()
    } else if m.contains("budget") {
        "budget".into()
    } else if m.contains("serial") {
        "serialisation".into()
    } else {
        let head: String = msg.chars().take(48).collect();
        format!("other:{head}")
    }
}

#[allow(clippy::too_many_arguments)]
fn run_level(
    target: &Target,
    dialect: Dialect,
    cat: &Catalogue,
    spec: LevelSpec,
    prof: &Profile,
    clients: usize,
    seconds: u64,
    level_index: usize,
    plan: Option<&LoadedPlan>,
    writes_mode: &str,
    control: &mut Box<dyn Backend>,
    integrity: &mut Vec<String>,
) -> LevelResult {
    let family = spec.dataset.family().name();
    let lo = level_index as u64 * LEVEL_STRIDE;
    let hi = lo.saturating_add(LEVEL_STRIDE);

    // ── Per-level setup: a clean slate for THIS level's value range ────────
    let group = match prof.write_kind {
        engram_bench::workload::WriteKind::UniqueCreate => Some("unique-create"),
        engram_bench::workload::WriteKind::DeleteChurn => Some("delete-churn"),
        _ => None,
    };
    if let Some(g) = group {
        let pairs = [
            ("lo".to_string(), lo.to_string()),
            ("hi".to_string(), hi.to_string()),
            ("nonce".to_string(), spec.nonce.to_string()),
        ];
        for stmt in cat.fixture(g, dialect, "setup").unwrap_or_default() {
            let s = render(&stmt, &borrow(&pairs));
            if let Err(e) = control.run(&s) {
                integrity.push(format!(
                    "{} @ {clients}: level setup failed ({s}): {e}",
                    prof.name
                ));
            }
        }
    }
    // Hot-locality levels are self-verifying: every acked hot write must
    // appear in the counter, or the run is measuring loss. A verification that
    // CANNOT run fails the run — fail closed.
    let hot_probe = match spec.dataset.family() {
        Dataset::Synthetic => "hot-counter-synthetic",
        _ => "hot-counter-snb",
    };
    let hot_before = if prof.write_locality == engram_bench::workload::Locality::Hot {
        match cat
            .integrity_probe(hot_probe, dialect)
            .map_err(|e| e.to_string())
            .and_then(|e| control.scalar(&e.text).map_err(|e| e.to_string()))
        {
            Ok(n) => Some(n),
            Err(e) => {
                integrity.push(format!(
                    "{} @ {clients} clients: the hot-counter baseline read failed ({e}) — \
                     loss verification could not run",
                    prof.name
                ));
                None
            }
        }
    } else {
        None
    };

    // ── Release the clients ────────────────────────────────────────────────
    let stop = Arc::new(AtomicBool::new(false));
    let ticker = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::with_capacity(clients);
    // The level's identity for the slow-statement log, printed once so a
    // reader of the log can bracket the level without the JSON.
    let level_tag: Arc<String> = Arc::new(format!("{}@{clients}", prof.name));
    let started_unix_ms = unix_ms();
    eprintln!(
        "[harness] level {}@{clients} started t={started_unix_ms} (level_index {level_index}, \
         id offset {lo})",
        prof.name
    );
    let t_zero = Instant::now();
    for cid in 0..clients {
        let stop = Arc::clone(&stop);
        let ticker = Arc::clone(&ticker);
        let level_tag = Arc::clone(&level_tag);
        let prof = *prof;
        let ops: Result<Option<Vec<PlanOp>>, String> = match plan {
            Some(p) => p.stream_for(cid, level_index).map(Some),
            None => Ok(None),
        };
        let target_c = target.clone();
        // The rendered statement for every op is produced on the worker, from
        // the catalogue it was handed. Rendering is not measured — the timer
        // brackets the wire call alone.
        let cat_src = engram_bench::catalogue::SOURCE;
        let family = family.to_string();
        handles.push(std::thread::spawn(move || {
            let mut s = Samples::default();
            let cat = match Catalogue::parse(cat_src) {
                Ok(c) => c,
                Err(e) => {
                    s.unrenderable = Some(e.to_string());
                    return s;
                }
            };
            let ops = match ops {
                Ok(o) => o,
                Err(e) => {
                    s.unrenderable = Some(e);
                    return s;
                }
            };
            let mut src = match ops {
                Some(ops) => OpSource::Replay { ops, at: 0, cid },
                None => OpSource::Live {
                    source: Box::new(engram_bench::workload::ClientOps::new(spec, &prof, cid)),
                    churn: Box::new(ChurnSet::default()),
                    cid,
                    nonce: spec.nonce,
                    level_index,
                    at: 0,
                },
            };
            let mut conn = match target_c.open() {
                Ok(b) => Some(b),
                Err(_) => {
                    s.errors += 1;
                    None
                }
            };
            // The per-worker churn anchor, made ONCE: a retried CREATE could
            // mint two anchors and double every later create.
            if prof.write_kind == engram_bench::workload::WriteKind::DeleteChurn {
                let (op, params) = bind_churn_anchor(cid, spec.nonce);
                let pairs = bindings(&params);
                if let Ok(e) = cat.write_op(op, &family, dialect) {
                    if e.status.runnable() {
                        let stmt = render(&e.text, &borrow(&pairs));
                        match conn.as_mut() {
                            Some(c) => {
                                if c.run(&stmt).is_err() {
                                    s.errors += 1;
                                    conn = None;
                                }
                            }
                            None => s.errors += 1,
                        }
                    }
                }
            }
            while !stop.load(Ordering::Relaxed) {
                let op = match src.next_op() {
                    Ok(op) => op,
                    Err(_) => {
                        s.exhausted_us = Some(t_zero.elapsed().as_micros() as u64);
                        break;
                    }
                };
                let Some(c) = conn.as_mut() else {
                    conn = target_c.open().ok();
                    s.errors += 1;
                    continue;
                };
                let stmt = match render_op(&cat, dialect, &family, &op) {
                    Ok(st) => st,
                    Err(e) => {
                        s.unrenderable = Some(e);
                        break;
                    }
                };
                let is_write = op.is_write();
                let shape = op.shape().map(str::to_string);
                let opname = op.op().map(str::to_string);
                let t0 = t_zero.elapsed().as_micros() as u64;
                let issued_ms = unix_ms();
                let t = Instant::now();
                let outcome = c.run(&stmt);
                let us = t.elapsed().as_micros() as u64;
                if us / 1000 >= slow_ms() {
                    // The same line `stress.rs` emits under `STRESS_SLOW_MS`,
                    // so the two op streams diff directly.
                    eprintln!(
                        "[slow] t={issued_ms} {}ms {level_tag} c{cid} {} {}",
                        us / 1000,
                        if is_write { "write" } else { "read" },
                        shape.as_deref().unwrap_or("-"),
                    );
                }
                match outcome {
                    Ok(_) => {
                        s.spans.push((t0, t0 + us));
                        if is_write {
                            s.writes.push(us);
                            match opname.as_deref() {
                                Some("churn_create") => s.churn_creates += 1,
                                Some("churn_delete") => s.churn_deletes += 1,
                                _ => {}
                            }
                        } else {
                            s.reads.push(us);
                            if let Some(n) = shape {
                                s.per_shape.entry(n).or_default().push(us);
                            }
                        }
                        ticker.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        // A REFUSAL is a correct answer under load and is
                        // counted apart from a transport error — collapsing
                        // them would let a server that refuses everything look
                        // healthy.
                        if e.is_refusal() {
                            s.refusals += 1;
                            *s.refusal_kinds
                                .entry(refusal_kind(e.message()))
                                .or_default() += 1;
                        } else {
                            s.errors += 1;
                            conn = None;
                        }
                    }
                }
            }
            s
        }));
    }

    // Sample throughput each second so collapse and drift are visible.
    let start = Instant::now();
    let mut per_sec = Vec::with_capacity(seconds as usize);
    let mut last = 0u64;
    for _ in 0..seconds {
        std::thread::sleep(Duration::from_secs(1));
        let now = ticker.load(Ordering::Relaxed);
        per_sec.push(now - last);
        last = now;
    }
    stop.store(true, Ordering::Relaxed);

    let mut agg = Samples::default();
    let mut ledgers: Vec<(usize, u64, u64)> = Vec::new();
    let mut exhausted: Vec<usize> = Vec::new();
    let mut exhausted_us: Vec<u64> = Vec::new();
    let mut joined = 0usize;
    for (wid, h) in handles.into_iter().enumerate() {
        if let Ok(s) = h.join() {
            joined += 1;
            if let Some(why) = &s.unrenderable {
                integrity.push(format!(
                    "{} @ {clients} clients: client {wid} could not render an operation \
                     in {}: {why}",
                    prof.name,
                    dialect.key()
                ));
            }
            if let Some(us) = s.exhausted_us {
                exhausted.push(wid);
                exhausted_us.push(us);
            }
            ledgers.push((wid, s.churn_creates, s.churn_deletes));
            agg.reads.extend(s.reads);
            agg.writes.extend(s.writes);
            agg.spans.extend(s.spans);
            agg.errors += s.errors;
            agg.refusals += s.refusals;
            agg.churn_creates += s.churn_creates;
            agg.churn_deletes += s.churn_deletes;
            for (k, v) in s.refusal_kinds {
                *agg.refusal_kinds.entry(k).or_default() += v;
            }
            for (k, v) in s.per_shape {
                agg.per_shape.entry(k).or_default().extend(v);
            }
        }
    }
    if joined != clients {
        integrity.push(format!(
            "{} @ {clients} clients: only {joined} of {clients} client(s) reported — \
             the level's totals are short and its rate is not a rate",
            prof.name
        ));
    }
    let secs = start.elapsed().as_secs_f64();
    agg.reads.sort_unstable();
    agg.writes.sort_unstable();
    let res = LevelResult {
        profile: prof.name.to_string(),
        clients,
        secs,
        r_ops: agg.reads.len(),
        w_ops: agg.writes.len(),
        r: agg.reads,
        w: agg.writes,
        errors: agg.errors,
        refusals: agg.refusals,
        per_sec,
        started_unix_ms,
        per_shape: agg.per_shape,
        plan_exhausted: exhausted,
        plan_exhausted_us: exhausted_us,
        plan_ops_per_client: plan.map(|p| p.ops_per_client),
        refusal_kinds: agg.refusal_kinds,
        max_inflight: max_inflight(&agg.spans),
        writes_mode: writes_mode.to_string(),
    };

    // ── Integrity, after every client thread has joined ────────────────────
    let probe_pairs = [
        ("lo".to_string(), lo.to_string()),
        ("hi".to_string(), hi.to_string()),
        ("nonce".to_string(), spec.nonce.to_string()),
    ];
    let render_probe = |name: &str, extra: &[(String, String)]| -> Option<String> {
        let mut pairs = probe_pairs.to_vec();
        pairs.extend_from_slice(extra);
        cat.integrity_probe(name, dialect)
            .ok()
            .filter(|e| e.status.runnable())
            .map(|e| render(&e.text, &borrow(&pairs)))
    };

    if prof.write_kind == engram_bench::workload::WriteKind::UniqueCreate {
        if let Some(q) = render_probe("uniq-duplicates", &[]) {
            match control.run(&q) {
                Ok(0) => eprintln!(
                    "        unique-integrity check: {} acked winner(s), {} refusal(s), \
                     zero duplicates",
                    res.w_ops, res.refusals
                ),
                Ok(d) => integrity.push(format!(
                    "{} @ {clients} clients: {d} DUPLICATE unique value(s) committed",
                    prof.name
                )),
                Err(e) => integrity.push(format!(
                    "{} @ {clients} clients: unique-integrity probe failed: {e}",
                    prof.name
                )),
            }
        }
    }
    if matches!(
        prof.write_kind,
        engram_bench::workload::WriteKind::RelSpread | engram_bench::workload::WriteKind::RelHub
    ) {
        let name = match spec.dataset.family() {
            Dataset::Synthetic => "rel-endpoints-synthetic",
            _ => "rel-endpoints-snb",
        };
        match cat.integrity_probe(name, dialect) {
            Ok(e) if e.status.runnable() => match control.pair(&e.text) {
                // ONE STATEMENT, so ONE SNAPSHOT: two queries are two
                // instants, and anything landing between them shows as a
                // divergence with the WRONG SIGN. A verifier that cries wolf
                // is worse than no verifier.
                Ok((x, y)) if x == y => {
                    eprintln!("        rel-integrity check: {x} edge(s), all endpoints bind");
                }
                Ok((x, y)) => integrity.push(format!(
                    "{} @ {clients} clients: {x} edge(s) but only {y} bind both endpoints \
                     — DANGLING EDGES",
                    prof.name
                )),
                Err(e) => integrity.push(format!(
                    "{} @ {clients} clients: rel-integrity probe failed: {e}",
                    prof.name
                )),
            },
            Ok(e) => eprintln!(
                "        rel-integrity check: not applicable in {} — {}",
                dialect.key(),
                match &e.status {
                    Status::Unsupported(r) => r.as_str(),
                    _ => "",
                }
            ),
            Err(e) => integrity.push(format!("{} @ {clients} clients: {e}", prof.name)),
        }
    }
    if prof.write_kind == engram_bench::workload::WriteKind::DeleteChurn {
        // FAIL CLOSED on zero work: reconcile(0,0,0) balances, so a level that
        // never acked a single churn create would sail through every probe and
        // print a PASS over nothing.
        if agg.churn_creates == 0 {
            integrity.push(format!(
                "{} @ {clients} clients: ZERO acked churn create(s) — the level did no \
                 verifiable churn work; refusing the vacuous pass",
                prof.name
            ));
        } else if agg.churn_deletes == 0
            && agg.churn_creates >= (clients * 2 * engram_bench::workload::CHURN_FLOOR) as u64
        {
            integrity.push(format!(
                "{} @ {clients} clients: {} acked create(s) but ZERO acked delete(s) — \
                 the delete path never engaged",
                prof.name, agg.churn_creates
            ));
        }
        let mut worker_mismatch = false;
        for (wid, cr, de) in &ledgers {
            let Some(q) =
                render_probe("churn-survivors-worker", &[("cid".into(), wid.to_string())])
            else {
                continue;
            };
            match control.run(&q) {
                Ok(survivors) => {
                    if let Reconciliation::Mismatch { expected, measured } =
                        reconcile(*cr, *de, survivors)
                    {
                        worker_mismatch = true;
                        if res.errors > 0 {
                            // A transport error after a server-side commit
                            // loses the ack, not the write — unattributable,
                            // and the transport errors already fail the run.
                            eprintln!(
                                "        churn worker {wid}: expected {expected} survivor(s), \
                                 measured {measured} — MISMATCH (unattributable: transport \
                                 errors ate acks)"
                            );
                        } else {
                            integrity.push(format!(
                                "{} @ {clients} clients: churn worker {wid} acked {cr} \
                                 create(s), {de} delete(s), but {measured} node(s) survive \
                                 (expected {expected}) — CHURN LOSS",
                                prof.name
                            ));
                        }
                    }
                }
                Err(e) => integrity.push(format!(
                    "{} @ {clients} clients: churn reconciliation probe for worker {wid} \
                     failed ({e}) — reconciliation could not run",
                    prof.name
                )),
            }
        }
        // The total is a FRESH query, not a sum of the per-worker probes — a
        // node minted with a wrong or missing cid hides from every per-worker
        // count and shows up only here.
        let mut total_survivors: Option<u64> = None;
        if let Some(q) = render_probe("churn-survivors-total", &[]) {
            match control.run(&q) {
                Ok(survivors) => {
                    total_survivors = Some(survivors);
                    match reconcile(agg.churn_creates, agg.churn_deletes, survivors) {
                        Reconciliation::Balanced(n) => {
                            if !worker_mismatch {
                                eprintln!(
                                    "        churn-integrity check: {} created, {} deleted, \
                                     {n} survivor(s) — every worker reconciles",
                                    agg.churn_creates, agg.churn_deletes
                                );
                            }
                        }
                        Reconciliation::Mismatch { expected, measured } => {
                            if res.errors > 0 {
                                eprintln!(
                                    "        churn total: expected {expected} survivor(s), \
                                     measured {measured} — MISMATCH (unattributable: transport \
                                     errors ate acks)"
                                );
                            } else {
                                integrity.push(format!(
                                    "{} @ {clients} clients: {} acked create(s) minus {} acked \
                                     delete(s), but {measured} node(s) survive (expected \
                                     {expected}) — CHURN LOSS",
                                    prof.name, agg.churn_creates, agg.churn_deletes
                                ));
                            }
                        }
                    }
                }
                Err(e) => integrity.push(format!(
                    "{} @ {clients} clients: the total churn reconciliation probe failed \
                     ({e}) — reconciliation could not run",
                    prof.name
                )),
            }
        }
        // Rel cleanup: DETACH DELETE must have taken each victim's anchor rel
        // with it — this level's anchors hold exactly one rel per survivor —
        // and every CHURN rel corpus-wide must bind both endpoints (the W1.1
        // dangling class, on the churn type the generic rel probe cannot see).
        let three = (
            render_probe("churn-anchor-rels", &[]),
            render_probe("churn-rel-bare", &[]),
            render_probe("churn-rel-bound", &[]),
        );
        if let (Some(qa), Some(qbare), Some(qbound)) = three {
            match (control.run(&qa), control.run(&qbare), control.run(&qbound)) {
                (Ok(anch), Ok(x), Ok(y)) => {
                    if x != y {
                        integrity.push(format!(
                            "{} @ {clients} clients: {x} CHURN edge(s) but only {y} bind both \
                             endpoints — DANGLING EDGES",
                            prof.name
                        ));
                    }
                    match total_survivors {
                        Some(surv) if anch != surv => integrity.push(format!(
                            "{} @ {clients} clients: {surv} churn survivor(s) but {anch} \
                             anchor rel(s) — deletes left rels behind, or took extra ones",
                            prof.name
                        )),
                        Some(surv) if x == y => eprintln!(
                            "        churn-rel check: {anch} anchor rel(s) == {surv} \
                             survivor(s), all CHURN endpoints bind"
                        ),
                        // `None` already failed the run in the reconciliation.
                        _ => {}
                    }
                }
                (a, b, c) => integrity.push(format!(
                    "{} @ {clients} clients: churn rel probe failed ({a:?} / {b:?} / {c:?})",
                    prof.name
                )),
            }
        }
        if let Some(q) = render_probe("churn-duplicates", &[]) {
            match control.run(&q) {
                Ok(0) => {}
                Ok(d) => integrity.push(format!(
                    "{} @ {clients} clients: {d} DUPLICATE churn id(s) committed",
                    prof.name
                )),
                Err(e) => integrity.push(format!(
                    "{} @ {clients} clients: churn duplicate probe failed: {e}",
                    prof.name
                )),
            }
        }
    }
    if let Some(before) = hot_before {
        match cat
            .integrity_probe(hot_probe, dialect)
            .map_err(|e| e.to_string())
            .and_then(|e| control.scalar(&e.text).map_err(|e| e.to_string()))
        {
            Ok(after) => {
                let delta = after - before;
                let acked = res.w_ops as i64;
                if delta == acked {
                    eprintln!(
                        "        hot-counter check: acked {acked}, counter moved {delta} — \
                         every acked write landed"
                    );
                } else if res.errors > 0 {
                    eprintln!(
                        "        hot-counter check: acked {acked}, counter moved {delta} — \
                         MISMATCH (unattributable: transport errors ate acks)"
                    );
                } else {
                    integrity.push(format!(
                        "{} @ {clients} clients: {acked} acked hot write(s) but the counter \
                         moved {delta} — LOST UPDATES",
                        prof.name
                    ));
                }
            }
            Err(e) => integrity.push(format!(
                "{} @ {clients} clients: the hot-counter FINAL read failed ({e}) — \
                 loss verification could not run",
                prof.name
            )),
        }
    }
    res
}

// ─── Argument parsing and dispatch ──────────────────────────────────────────

/// Print every family this binary was compiled with, and its digest.
///
/// The digest is printed beside the name because it is the only thing that
/// makes the list checkable: two binaries can both say `snb-bi` and hold
/// different statements, and the name alone cannot tell you which one produced
/// a result document.
fn list_families() {
    eprintln!("families compiled into this binary:");
    for f in engram_bench::catalogue::FAMILIES {
        let n = f
            .load()
            .ok()
            .and_then(|c| c.query_names(f.queries_path).ok())
            .map_or("unreadable".to_string(), |q| format!("{} queries", q.len()));
        let lane = if f.name == "lsqb-stress" {
            "runnable: `harness lsqb` / `harness stress`"
        } else {
            "carried only: renderable, NO execution lane in this binary"
        };
        eprintln!("  {:<16} {:016x}  {n:<12} {lane}", f.name, f.digest());
    }
}

fn usage() -> ! {
    eprintln!(
        "usage:
  harness catalogue --dump <path> [--family NAME]
  harness family --list
  harness family <name> --list
  harness family <name> --query Q --dialect cypher|ladybug|sql
               [--param k=v]... [--allow-unbound]
  harness plan --profile <p> --clients <n> --seconds <s> --out <plan.jsonl>
               [--ops m] [--rate ops/s/client] [--seed N]
               [--dataset synthetic|snb|snb-platform] [--keys N] [--nonce N]
  harness snb-bi|snb-interactive|finbench <addr> --params FILE
               --rig RIG --thread-cap N --cache-mb N
               [--engine bolt|pg] [--pg-user U] [--pg-db D]
               [--corpus sf3] [--queries bi1,bi4] [--variant 16a]
               [--dialect cypher|cypher_engram|cypher_ladybug|sql]
               [--timeout-secs N] [--continue-after-timeout]
               [--show-rows N] [--allow-fairness-mismatch] [--json OUT]
  harness lsqb <addr> --rig RIG --thread-cap N --cache-mb N
               [--engine bolt|pg] [--pg-user U] [--pg-db D]
               [--corpus sf1] [--queries q1,q3] [--timeout-secs N]
               [--allow-fairness-mismatch] [--json OUT]
  harness stress <addr> <profile|all> <clients-csv> <seconds> --writes single|multi
               --rig RIG --thread-cap N --cache-mb N
               [--engine bolt|pg] [--pg-user U] [--pg-db D] [--plan PLAN]
               [--seed N] [--keys N] [--dataset D]
               [--plan-rate ops/s/client] [--continue-after-plan-exhaustion]
               [--allow-rig-mismatch] [--allow-fairness-mismatch]
               [--allow-short-levels] [--json OUT]
  harness report <result.json> [<result.json> ...]
               [--baseline BASE.json [--max-regression PCT]
                [--min-regression-ms MS] [--reproduce]]
               (--reproduce: every result named is a repetition of ONE
               candidate, and a key regresses only when all of them regress
               it; --min-regression-ms: a millisecond key must also be slower
               by more than MS)

families: every family now has an EXECUTION lane. `harness family` still
  selects and renders statements WITHOUT connecting to an engine, which is the
  right tool for inspecting text; `harness snb-bi|snb-interactive|finbench`
  connects and measures.

  The three read batteries BIND their parameters and never render them, so the
  engine plans the catalogue's own bytes. `--params` is REQUIRED for exactly
  that reason: a parameter is part of the question, and this lane will not
  invent one. `bi12` once ran `languages ['en','de']` against a corpus carrying
  uz/tk/ar, matched zero rows, and was recorded as a 160 s timing; `bi16` did
  the same on dates its tag had no messages for. Zero rows is therefore
  reported as `empty` and FAILS the run -- it is never `ok`.

  A CEILING BOUNDS THE CLIENT, NOT THE SERVER. When a query overruns, its
  worker is abandoned and the server keeps computing it, so everything measured
  afterwards shares the machine with a query nobody is waiting for. Measured on
  SF3 2026-09-21: bi7 answered in 184 ms in one run and hit the 300 s ceiling in
  the next, and bi8a/bi8b went from 54 s each to the ceiling, purely because bi6
  had overrun just before them. The lane therefore STOPS at the first timeout;
  --continue-after-timeout measures anyway and records every later row as
  `<status>-contaminated`.

plan sizing: a plan must hold `rate x seconds` ops per client or the fastest
  arm runs out mid-level and EVERY level of the sweep is refused. `--seconds`
  is required so the emitter can do that arithmetic; `--ops` defaults to it and
  is REFUSED when it is below it. The default rate is

fairness (--thread-cap and --cache-mb are REQUIRED on BOTH workloads, with no
  defaults, and are CHECKED against the engine wherever it will answer:
  PostgreSQL always (SHOW shared_buffers; 1 leader + max_parallel_workers_per_gather);
  engram from a server new enough to send a HELLO serving hint; Neo4j through
  dbms.listConfig. Where the engine answers nothing the figure is recorded as
  declared-not-observed, which is NOT a pass. A CONTRADICTED stamp stops the run;
  --allow-fairness-mismatch takes the measurement and records it, and the
  reporter then refuses the row.)

rigs (--rig is REQUIRED, and is CHECKED against the machine at run time;
      the reporter REFUSES a table across two of them):"
    );
    eprintln!("   {ASSUMED_PEAK_OPS_PER_CLIENT_SEC} ops/s per client.\n");
    for r in KNOWN_RIGS {
        eprintln!(
            "   {:<16} {} cores{}, {} MiB — {}",
            r.name,
            r.node_cores,
            match r.cpu_quota_cores {
                Some(q) => format!(", {q}-core quota"),
                None => ", no quota".to_string(),
            },
            r.mem_limit_mb,
            r.what
        );
    }
    eprintln!(
        "   {:<16} anything outside this estate, every field spelled out",
        "name:type:c:q:m"
    );
    eprintln!(
        "
profiles:"
    );
    for p in PROFILES {
        eprintln!(
            "   {:<22} {:>3}% writes {} — {}",
            p.name,
            p.write_pct,
            if p.diagnostic {
                "[diagnostic]"
            } else {
                "            "
            },
            p.what
        );
    }
    std::process::exit(2);
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn num(args: &[String], name: &str, dflt: u64) -> u64 {
    flag(args, name)
        .and_then(|s| s.parse().ok())
        .unwrap_or(dflt)
}

/// Resolve the REQUIRED `--rig`, or refuse to run.
///
/// No default, and deliberately no inference. The harness could read
/// `/sys/fs/cgroup/cpu.max` and `nproc` and be right most of the time, and
/// "most of the time" is worthless here: a rig it guessed wrong is worse than
/// no rig at all, because a wrong stamp COMPARES. The operator knows which
/// lane they are on; the harness makes them say it.
///
/// This is `--writes`'s friction one level up. `--writes` refuses to guess
/// which workload ran; `--rig` refuses to guess which machine ran it.
///
/// # And then it is CHECKED
///
/// Refusing to guess is not the same as being right. A declaration nothing
/// verifies cannot be wrong, and the dry run put it plainly: *"I stamped two
/// different pods identically and nothing objected."* So the declared rig is
/// compared against what the process can read about itself — see
/// [`engram_bench::report::ObservedMachine`] — and a CONTRADICTION stops the
/// run before it starts, rather than producing a labelled result nobody can
/// use. `--allow-rig-mismatch` takes the measurement anyway and stamps the
/// disagreement into the document, where the reporter refuses it.
///
/// A machine that says nothing about itself (a workstation, Windows) degrades
/// to `unobservable`. That is a loud line on stderr and a word in the
/// document, NOT a pass: the stamp is exactly as trusted as it always was, and
/// now says so.
fn require_rig(args: &[String], scale: &str) -> (Rig, RigCheck) {
    let Some(spec) = flag(args, "--rig") else {
        eprintln!(
            "[harness] --rig is REQUIRED and has no default: this project runs TWO \
             measurement lanes side by side and keeps both, so a number that cannot \
             say which one produced it is uninterpretable — and the moment it sits \
             next to a number from the other lane, somebody reads a ratio off the \
             pair. The reporter refuses a table across rigs; a run refuses to be \
             taken without one."
        );
        eprintln!("[harness] known rigs:");
        for r in KNOWN_RIGS {
            eprintln!("   {:<16} {} ({} cores)", r.name, r.what, r.node_cores);
        }
        eprintln!(
            "[harness] a machine outside this estate is described inline, every field \
             spelled out: --rig name:node_type:cores:quota|none:mem_mb"
        );
        usage();
    };
    let rig = match Rig::from_spec(spec, scale) {
        Ok(r) => {
            // Echoed before the run, not after. A mistyped inline rig is cheap
            // to notice now and expensive to notice at the end of a sweep,
            // when the only remedy is to take the measurement again.
            eprintln!("[harness] rig: {}", r.describe());
            r
        }
        Err(e) => {
            eprintln!("[harness] --rig: {e}");
            usage();
        }
    };
    let check = RigCheck::observe(&rig);
    eprintln!("[harness] {}", check.describe());
    if check.status == RigStatus::Mismatch {
        let allowed = args.iter().any(|a| a == "--allow-rig-mismatch");
        for d in &check.disagreements {
            eprintln!("[harness]   - {d}");
        }
        if !allowed {
            eprintln!(
                "[harness] REFUSING to run: a WRONG rig stamp is worse than an absent one, \
                 because it compares. Two runs that agree on a label neither earned build a \
                 clean table of two different machines, and nothing in the numbers says \
                 otherwise. Fix `--rig` (or describe this machine inline as \
                 name:node_type:cores:quota|none:mem_mb) and run again.\n\
                 [harness] If the observation itself is wrong — a nested cgroup, a sysfs the \
                 container cannot see — `--allow-rig-mismatch` takes the measurement anyway \
                 and records the disagreement in the document. The run will FAIL and the \
                 reporter will refuse the row: that is the point, not an oversight."
            );
            std::process::exit(2);
        }
        eprintln!(
            "[harness] --allow-rig-mismatch: proceeding. The disagreement is recorded in the \
             document, the run will FAIL, and `harness report` will refuse to build a table \
             from it."
        );
    }
    (rig, check)
}

/// One REQUIRED fairness number, with no default and no inference.
///
/// # Why the default had to go
///
/// `--cache-mb` defaulted to 8192 and `--thread-cap` to 6, and a default is
/// exactly as dangerous as the thing it describes. These two numbers are
/// CLAIMS about a server started somewhere else, so an omitted flag does not
/// produce a missing field — it produces a *plausible* field describing a
/// server that is not running, and every check downstream compares that field
/// for equality against another document's equally plausible one.
///
/// It is not hypothetical and it is not old. The 2026-09-08/09 Neo4j window
/// passed `--cache-mb 10240` on its two stress arms and nothing on its three
/// LSQB batteries; those three documents stamped 8192 against a pod running
/// with a 10 GiB page cache, and nothing objected. The defect surfaced days
/// later, and only because the reporter refused a table built across the two
/// numbers — the guard worked, the stamping did not.
///
/// So this is `--rig`'s friction, one level down. `--rig` refuses to guess
/// which machine ran a measurement; this refuses to guess how the engine on it
/// was configured.
fn require_num(args: &[String], name: &str, what: &str, example: &str) -> u32 {
    let Some(raw) = flag(args, name) else {
        eprintln!(
            "[harness] {name} is REQUIRED and has no default: it is {what}, which is a CLAIM \
             about a server this harness did not start. A defaulted claim is not a missing \
             field — it is a plausible field describing a server that is not running, and \
             the reporter compares it for equality against another document's equally \
             plausible one. That is not a story about what could happen: three Neo4j LSQB \
             batteries stamped `cache_budget_mb: 8192` against a pod configured with 10 GiB \
             of page cache, because this flag had a default."
        );
        eprintln!("[harness] e.g. {name} {example}");
        eprintln!(
            "[harness] the value is CHECKED against the engine wherever the engine will \
             answer (PostgreSQL always; engram from a server new enough to send a serving \
             hint; Neo4j through dbms.listConfig), and recorded as declared-not-observed \
             where it will not."
        );
        usage();
    };
    match raw.parse::<u32>() {
        Ok(n) if n > 0 => n,
        _ => {
            eprintln!("[harness] {name} `{raw}` is not a positive whole number");
            usage();
        }
    }
}

/// Ask the ENGINE what it is serving under, and hold the stamp to the answer.
///
/// The probe runs on its OWN connection, opened for this and dropped
/// immediately. That is not tidiness: [`BoltBackend`](engram_bench::backend)
/// classifies a statement the server rejects as a transport error and drops
/// its client, so asking a Neo4j procedure on a measurement session would
/// trade a missing observation for a broken run — and the engram case asks
/// nothing at all, because the answer arrived in HELLO.
///
/// An engine that cannot be reached is `declared`, not a failure: the run is
/// about to fail on its own, loudly, and reporting a connection error as a
/// fairness verdict would name the wrong thing.
fn check_fairness(target: &Target, fairness: &Fairness, args: &[String]) -> FairnessCheck {
    let observed = match target.open() {
        Ok(mut b) => b.engine_fairness(),
        Err(e) => EngineFairness::declared(&format!(
            "declared: could not open a probe connection to {} ({e})",
            target.addr()
        )),
    };
    let check = FairnessCheck::of(fairness, observed);
    eprintln!("[harness] {}", check.describe());
    if check.status == FairnessStatus::Mismatch {
        for d in &check.disagreements {
            eprintln!("[harness]   - {d}");
        }
        if !args.iter().any(|a| a == "--allow-fairness-mismatch") {
            eprintln!(
                "[harness] REFUSING to run: the engine is not configured the way this run \
                 says it is. A wrong fairness stamp COMPARES — two documents that agree on \
                 a cache budget neither engine was given build a clean table of two \
                 differently configured servers. Either configure the engine to the stamp \
                 or stamp what the engine is configured with, and run again.\n\
                 [harness] If the OBSERVATION is wrong — a size format this build cannot \
                 read, a knob that means something else on this engine — \
                 `--allow-fairness-mismatch` takes the measurement anyway and records the \
                 disagreement in the document. The run will FAIL and the reporter will \
                 refuse the row: that is the point, not an oversight."
            );
            std::process::exit(2);
        }
        eprintln!(
            "[harness] --allow-fairness-mismatch: proceeding. The disagreement is recorded \
             in the document, the run will FAIL, and `harness report` will refuse to build a \
             table from it."
        );
    }
    check
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
    }
    let cat = match Catalogue::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[harness] the compiled-in catalogue is broken: {e}");
            std::process::exit(1);
        }
    };
    match args[1].as_str() {
        "catalogue" => {
            let Some(path) = flag(&args, "--dump") else {
                usage()
            };
            // Defaulting to the frozen family rather than requiring --family
            // keeps every existing caller -- the Python executors and the pod
            // scripts -- dumping exactly the bytes they dumped yesterday.
            let want = flag(&args, "--family").unwrap_or("lsqb-stress");
            let Some(fam) = engram_bench::catalogue::family(want) else {
                eprintln!("[harness] this binary carries no family `{want}`");
                list_families();
                std::process::exit(2);
            };
            // The EXACT bytes this binary was compiled with, so the Python
            // executor cannot be handed a different catalogue by a stale file
            // on a pod.
            match std::fs::write(path, fam.source) {
                Ok(()) => eprintln!(
                    "[harness] catalogue {} {:016x} written to {path}",
                    fam.name,
                    fam.digest()
                ),
                Err(e) => {
                    eprintln!("[harness] cannot write {path}: {e}");
                    std::process::exit(1);
                }
            }
        }
        "family" => {
            if args.len() < 3 || args[2] == "--list" {
                list_families();
                return;
            }
            let want = args[2].clone();
            let Some(fam) = engram_bench::catalogue::family(&want) else {
                eprintln!("[harness] this binary carries no family `{want}`");
                list_families();
                std::process::exit(2);
            };
            let fcat = match fam.load() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[harness] family `{}` is broken: {e}", fam.name);
                    std::process::exit(1);
                }
            };
            let names = match fcat.query_names(fam.queries_path) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("[harness] family `{}`: {e}", fam.name);
                    std::process::exit(1);
                }
            };
            let Some(query) = flag(&args, "--query") else {
                println!(
                    "{} {:016x} -- {} queries",
                    fam.name,
                    fam.digest(),
                    names.len()
                );
                for n in &names {
                    println!("  {n}");
                }
                return;
            };
            let dname = flag(&args, "--dialect").unwrap_or("cypher");
            let Some(dialect) = Dialect::parse(dname) else {
                eprintln!("[harness] unknown dialect `{dname}`");
                std::process::exit(2);
            };
            let entry = match fcat.query(fam.queries_path, query, dialect) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("[harness] {e}");
                    std::process::exit(1);
                }
            };
            if let engram_bench::catalogue::Status::Unsupported(reason) = &entry.status {
                // An exit code rather than an empty stdout. A caller piping
                // this into a driver must be able to tell "this dialect cannot
                // express the query, and here is who decided that" from "the
                // query produced no text", and those two look identical on
                // stdout.
                eprintln!(
                    "[harness] {}.{query} [{}] is UNSUPPORTED: {reason}",
                    fam.name,
                    dialect.key()
                );
                std::process::exit(3);
            }
            let mut params: Vec<(String, String)> = Vec::new();
            let mut i = 0;
            while i + 1 < args.len() {
                if args[i] == "--param" {
                    match args[i + 1].split_once('=') {
                        Some((k, v)) => params.push((k.to_string(), v.to_string())),
                        None => {
                            eprintln!("[harness] --param wants k=v, got `{}`", args[i + 1]);
                            std::process::exit(2);
                        }
                    }
                }
                i += 1;
            }
            let borrowed: Vec<(&str, &str)> = params
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let rendered = engram_bench::catalogue::render(&entry.text, &borrowed);
            // A statement still holding `${...}` is not a statement. Sending
            // one to an engine gets a parse error at best; at worst it parses,
            // because the placeholder sat inside a string literal, and answers
            // a question about the literal text. The refusal is the default
            // and `--allow-unbound` is the way to look at a template on
            // purpose.
            if engram_bench::catalogue::has_unbound(&rendered)
                && !args.iter().any(|a| a == "--allow-unbound")
            {
                eprintln!(
                    "[harness] {}.{query} [{}] still has unbound placeholders after {} \
                     --param binding(s). Bind them, or pass --allow-unbound to print the \
                     template as a template.",
                    fam.name,
                    dialect.key(),
                    params.len()
                );
                eprintln!("{rendered}");
                std::process::exit(4);
            }
            eprintln!(
                "[harness] {} {:016x} :: {query} [{}] status={}",
                fam.name,
                fam.digest(),
                dialect.key(),
                entry.status.as_str()
            );
            println!("{rendered}");
        }
        "plan" => {
            let Some(name) = flag(&args, "--profile") else {
                usage()
            };
            let Some(out) = flag(&args, "--out") else {
                usage()
            };
            let Some(prof) = profile(name) else {
                eprintln!("[harness] unknown profile `{name}`");
                usage();
            };
            let dataset = flag(&args, "--dataset")
                .map_or(Some(Dataset::Synthetic), Dataset::parse)
                .unwrap_or_else(|| {
                    eprintln!("[harness] unknown dataset");
                    usage()
                });
            let spec = LevelSpec {
                seed: num(&args, "--seed", 424_242),
                dataset,
                keys: num(&args, "--keys", 20_000).max(1),
                nonce: num(&args, "--nonce", 1),
            };
            let clients = num(&args, "--clients", 32) as usize;
            // `--seconds` is REQUIRED and has no default, and this is the
            // whole fix for the sizing defect. A plan's length only means
            // something against a level DURATION; without one the emitter
            // cannot tell a sufficient `--ops` from a useless one, and the
            // 200,000 that used to be the default was a number, not an answer.
            // The dry run emitted 4,000 and lost the sweep, and nothing in the
            // command line was in a position to object.
            let seconds = flag(&args, "--seconds")
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|v| *v > 0);
            let Some(seconds) = seconds else {
                eprintln!(
                    "[harness] --seconds is REQUIRED and has no default: it is the level \
                     duration this plan will be replayed at, and it is the only thing that \
                     makes `--ops` checkable. A plan shorter than `rate x seconds` drains \
                     mid-level, the level is correctly refused as NOT QUOTABLE, and a \
                     ten-hour sweep produces a complete set of refused rows — which is \
                     exactly what the 2026-09-09 dry run did with a 4,000-op plan and \
                     20-second levels."
                );
                eprintln!(
                    "[harness] pass the same number you will pass to `harness stress \
                     <addr> <profile> <clients> <seconds>`."
                );
                usage();
            };
            let rate = num(&args, "--rate", ASSUMED_PEAK_OPS_PER_CLIENT_SEC).max(1);
            let sized = required_ops_per_client(seconds, rate);
            let ops = match flag(&args, "--ops") {
                None => {
                    eprintln!(
                        "[harness] --ops not given: sizing for a {seconds}s level at {rate} \
                         ops/s per client -> {sized} ops per client"
                    );
                    sized
                }
                Some(v) => match v.parse::<usize>() {
                    Ok(n) if n > 0 => n,
                    _ => {
                        eprintln!("[harness] --ops must be a positive whole number");
                        usage();
                    }
                },
            };
            // The refusal, before any bytes are written. It names the `--ops`
            // that WOULD work, because "emit a longer plan" is the advice that
            // produced the second undersized plan.
            if let Some(why) = undersized_because(ops, seconds, rate) {
                eprintln!("[harness] REFUSING to emit an undersized plan: {why}");
                std::process::exit(2);
            }
            match emit_plan(std::path::Path::new(out), spec, prof, clients, ops) {
                Ok(p) => {
                    let bytes = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
                    eprintln!(
                        "[harness] wrote {out}: {clients} client stream(s) x {ops} ops, \
                         profile={name}, dataset={}, seed={}, {:.1} MB, sha256={}",
                        dataset.name(),
                        spec.seed,
                        bytes as f64 / 1e6,
                        p.sha256
                    );
                    eprintln!(
                        "[harness] sized for {seconds}s levels at up to {rate} ops/s per \
                         client; a faster engine than that will still exhaust it, and the \
                         run will say so with the `--ops` it needed"
                    );
                    eprintln!(
                        "[harness] a sweep may run client levels up to {clients}; a higher \
                         level REFUSES rather than wrapping"
                    );
                }
                Err(e) => {
                    eprintln!("[harness] cannot write {out}: {e}");
                    std::process::exit(1);
                }
            }
        }
        "snb-bi" | "snb-interactive" | "finbench" => {
            if args.len() < 3 {
                usage();
            }
            let fam_name = args[1].clone();
            let Some(family) = engram_bench::catalogue::family(&fam_name) else {
                eprintln!("[harness] this binary carries no family `{fam_name}`");
                std::process::exit(2);
            };
            let addr = args[2].clone();
            // ── Parameters ──────────────────────────────────────────────
            //
            // REQUIRED, and this is the whole point of the lane. Every query
            // in these three families is parameterised, and a run without a
            // parameter file would bind nothing and report twenty
            // `unparameterised` rows. Saying so here, once, is clearer than
            // twenty rows saying it separately.
            let Some(pf) = flag(&args, "--params") else {
                eprintln!(
                    "[harness] {fam_name} needs --params FILE.

Every query in this family is parameterised, and a parameter is part of the
question: `bi12` with languages ['en','de'] against a corpus carrying uz/tk/ar
matched ZERO rows and was recorded as a 160 s timing. This lane will not guess
a parameter and will not run without one.

Derive a file for this corpus with:
  snbparams <addr> --family {fam_name} --out params.json

The file is {{\"<query>\": {{\"<parameter>\": \"<text value>\"}}}}, and each value is
coerced to the type the CATALOGUE declares for it -- not to the type it looks
like."
                );
                std::process::exit(2);
            };
            let (supplied, temporal_encoding) = match read_params_file(pf) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("[harness] --params {pf}: {e}");
                    std::process::exit(2);
                }
            };
            let thread_cap = require_num(
                &args,
                "--thread-cap",
                "the intra-query parallelism the engine was given",
                "6 (this project's pod-lane width, against a 6-CPU quota)",
            );
            let cache_mb = require_num(
                &args,
                "--cache-mb",
                "the serving cache budget the engine was given, in MiB",
                "8192 (this project's pod-lane budget)",
            );
            let target = build_target(&args, &addr, thread_cap);
            // `--dialect` overrides the engine default, and exists for the
            // handful of queries where no single Cypher serves both Bolt
            // engines: bi15/19/20 reach a weighted shortest path through GDS
            // on Neo4j and through `engram.algo.kshortestpaths` on engram, so
            // the shared `cypher` entry is `unsupported` and each engine's
            // text is parked under its own key. A run driven this way stamps
            // `cypher_engram` in its document, so a reader can see the row was
            // not produced from the shared text.
            let dialect = match flag(&args, "--dialect") {
                Some(d) => match Dialect::parse(d) {
                    Some(d) => d,
                    None => {
                        eprintln!(
                            "[harness] --dialect takes cypher, cypher_engram, cypher_ladybug                              or sql, got `{d}`"
                        );
                        std::process::exit(2);
                    }
                },
                None => match flag(&args, "--engine").unwrap_or("bolt") {
                    "pg" | "postgres" => Dialect::Sql,
                    "ladybug" => Dialect::CypherLadybug,
                    _ => Dialect::Cypher,
                },
            };
            let corpus = flag(&args, "--corpus").unwrap_or("sf1").to_string();
            let (rig, rig_check) = require_rig(&args, &corpus);
            let selected = flag(&args, "--queries").map(|s| {
                s.split(',')
                    .map(|q| q.trim().to_string())
                    .collect::<Vec<_>>()
            });
            let variant_filter = flag(&args, "--variant").map(str::to_string);
            // The ceiling. `lsqb` defaults to 120 s; these batteries are
            // analytical and the pod lane has been running them at 300 s.
            let timeout = Duration::from_secs(num(&args, "--timeout-secs", 300));
            let fairness = Fairness {
                thread_cap,
                cache_budget_mb: cache_mb,
                clients: 1,
                seconds: timeout.as_secs(),
            };
            let fairness_check = check_fairness(&target, &fairness, &args);
            let report = run_family(
                &target,
                dialect,
                family,
                Stamps {
                    rig,
                    rig_check,
                    fairness,
                    fairness_check,
                },
                selected,
                &supplied,
                variant_filter,
                temporal_encoding,
                args.iter().any(|a| a == "--continue-after-timeout"),
                num(&args, "--show-rows", 0) as usize,
            );
            finish(report, flag(&args, "--json"));
        }
        "lsqb" => {
            if args.len() < 3 {
                usage();
            }
            let addr = args[2].clone();
            // The SAME resolution the fairness block records, so the number
            // the document stamps and the number the session runs under
            // cannot drift apart — and REQUIRED on this lane too. It was not,
            // and that is the whole defect: the Neo4j window's stress arms
            // passed a cache budget and its three batteries did not.
            let thread_cap = require_num(
                &args,
                "--thread-cap",
                "the intra-query parallelism the engine was given",
                "6 (this project's pod-lane width, against a 6-CPU quota)",
            );
            let cache_mb = require_num(
                &args,
                "--cache-mb",
                "the serving cache budget the engine was given, in MiB",
                "8192 (this project's pod-lane budget)",
            );
            let target = build_target(&args, &addr, thread_cap);
            let dialect = match flag(&args, "--engine").unwrap_or("bolt") {
                "pg" | "postgres" => Dialect::Sql,
                "ladybug" => Dialect::CypherLadybug,
                _ => Dialect::Cypher,
            };
            let corpus = flag(&args, "--corpus").unwrap_or("sf1").to_string();
            // The corpus IS the scale for the battery — it is the key the
            // expected counts are looked up under — so it is handed to the rig
            // and read back out of it, rather than travelling twice.
            let (rig, rig_check) = require_rig(&args, &corpus);
            let selected = flag(&args, "--queries").map(|s| {
                s.split(',')
                    .map(|q| q.trim().to_string())
                    .collect::<Vec<_>>()
            });
            let timeout = Duration::from_secs(num(&args, "--timeout-secs", 120));
            let fairness = Fairness {
                thread_cap,
                cache_budget_mb: cache_mb,
                clients: 1,
                seconds: timeout.as_secs(),
            };
            let fairness_check = check_fairness(&target, &fairness, &args);
            let report = run_lsqb(
                &target,
                dialect,
                &cat,
                Stamps {
                    rig,
                    rig_check,
                    fairness,
                    fairness_check,
                },
                selected,
            );
            finish(report, flag(&args, "--json"));
        }
        "stress" => {
            if args.len() < 6 {
                usage();
            }
            let addr = args[2].clone();
            let want = args[3].clone();
            let levels: Vec<usize> = args[4]
                .split(',')
                .filter_map(|s| s.trim().parse().ok())
                .filter(|n: &usize| *n > 0)
                .collect();
            let seconds: u64 = args[5].parse().unwrap_or_else(|_| {
                eprintln!("[harness] seconds must be a number");
                usage()
            });
            // A level shorter than MIN_JUDGED_BUCKETS seconds produces fewer
            // one-second buckets than `trend` and `floor` need, so the DEGRADED
            // and STALLED checks CANNOT fire on it — and both statistics fall
            // back to 1.0, the value of a perfectly steady level. Such a level
            // used to print a clean row, and the clean row was the absence of a
            // check. Refusing here is the cheap failure: before the corpus is
            // touched, rather than after the sweep.
            //
            // The escape exists because the short level has a real use — the
            // `read-only 1 3` attach probe that proved the `snb` seam — and
            // deleting it would be worse than the defect. It does NOT buy a
            // clean row: every level it produces is refused as
            // `too_short_to_judge` in the document, which is the honest state
            // of a smoke probe.
            let allow_short = args.iter().any(|a| a == "--allow-short-levels");
            if (seconds as usize) < MIN_JUDGED_BUCKETS && !allow_short {
                eprintln!(
                    "[harness] --seconds {seconds} is below the {MIN_JUDGED_BUCKETS} a level needs \
                     before it can be judged: `trend` splits the one-second buckets in half \
                     and `floor` takes a 10th percentile over them, and with {seconds} \
                     bucket(s) both return 1.0 — the value of a PERFECTLY STEADY level. The \
                     DEGRADED and STALLED checks would not fire, whatever the engine did, \
                     and the level would print CLEAN."
                );
                eprintln!(
                    "[harness] run levels of at least {MIN_JUDGED_BUCKETS}s, or pass \
                     --allow-short-levels for a smoke probe — which runs, and marks every \
                     level NOT QUOTABLE (too_short_to_judge) rather than letting one pass \
                     unchecked."
                );
                usage();
            }
            // `--writes` is REQUIRED and has no default, which is deliberate
            // friction: LadybugDB's two modes produce categorically different
            // results from one plan, and defaulting to either would silently
            // pick one. Bolt and Postgres are multi-writer by construction,
            // and must still say so, because the field is what makes a row
            // interpretable.
            let Some(writes_mode) = flag(&args, "--writes") else {
                eprintln!(
                    "[harness] --writes single|multi is REQUIRED: the same plan produces \
                     categorically different results in the two modes, so a row without \
                     the stamp is uninterpretable"
                );
                usage();
            };
            if writes_mode != "single" && writes_mode != "multi" {
                eprintln!("[harness] --writes must be `single` or `multi`");
                usage();
            }
            if let Some(b) = flag(&args, "--id-base") {
                let Ok(base) = b.parse::<u64>() else {
                    eprintln!("[harness] --id-base must be an unsigned integer");
                    usage();
                };
                let _ = ID_BASE.set(base);
                eprintln!(
                    "[harness] node_create ids offset by {base} (--id-base): no stress write \
                     reuses an id the corpus holds"
                );
            }
            // One resolution of each fairness knob, REQUIRED, used both to
            // configure the session and to stamp the document.
            let thread_cap = require_num(
                &args,
                "--thread-cap",
                "the intra-query parallelism the engine was given",
                "6 (this project's pod-lane width, against a 6-CPU quota)",
            );
            let cache_mb = require_num(
                &args,
                "--cache-mb",
                "the serving cache budget the engine was given, in MiB",
                "8192 (this project's pod-lane budget)",
            );
            let target = build_target(&args, &addr, thread_cap);
            let dialect = match flag(&args, "--engine").unwrap_or("bolt") {
                "pg" | "postgres" => Dialect::Sql,
                _ => Dialect::Cypher,
            };
            let dataset = flag(&args, "--dataset")
                .map_or(Some(Dataset::Synthetic), Dataset::parse)
                .unwrap_or_else(|| {
                    eprintln!("[harness] unknown dataset");
                    usage()
                });
            let plan = flag(&args, "--plan").map(|p| match load_plan(std::path::Path::new(p)) {
                Ok(pl) => pl,
                Err(e) => {
                    eprintln!("[harness] {e}");
                    std::process::exit(1);
                }
            });
            let spec = LevelSpec {
                seed: plan
                    .as_ref()
                    .map_or(num(&args, "--seed", 424_242), |p| p.seed),
                dataset: plan
                    .as_ref()
                    .and_then(|p| Dataset::parse(&p.dataset))
                    .unwrap_or(dataset),
                keys: plan
                    .as_ref()
                    .map_or(num(&args, "--keys", 20_000), |p| p.keys)
                    .max(1),
                nonce: plan.as_ref().map_or(num(&args, "--nonce", 1), |p| p.nonce),
            };
            // The scale, derived from what the run actually loaded rather than
            // from a flag that may be absent. The generated corpus HAS a scale
            // — its key count — and two synthetic runs at different key counts
            // are no more comparable than SF1 against SF10, so it is spelled
            // out rather than left as the `-` placeholder the document used to
            // carry. An SNB run that will not name its corpus is refused: a
            // rig that cannot say sf1 from sf10 is the blend in miniature.
            let scale = match flag(&args, "--corpus") {
                Some(c) => c.to_string(),
                None if spec.dataset.family() == Dataset::Synthetic => {
                    format!("synthetic-keys-{}", spec.keys)
                }
                None => {
                    eprintln!(
                        "[harness] --corpus is REQUIRED for the `{}` dataset: the rig \
                         records the scale a number was taken at, and `sf1` next to \
                         `sf10` in one table is the same error as two machines in one \
                         table",
                        spec.dataset.name()
                    );
                    usage();
                }
            };
            let (rig, rig_check) = require_rig(&args, &scale);
            // Asked BEFORE the sweep, for the reason the rig is: a stamp that
            // contradicts the engine is cheap to notice now and costs the
            // whole run to notice afterwards, when the only remedy is to take
            // the measurement again.
            let fairness = Fairness {
                thread_cap,
                cache_budget_mb: cache_mb,
                clients: levels.iter().copied().max().unwrap_or(1),
                seconds,
            };
            let fairness_check = check_fairness(&target, &fairness, &args);
            let profiles: Vec<&Profile> = if want == "all" {
                PROFILES.iter().filter(|p| !p.diagnostic).collect()
            } else {
                match profile(&want) {
                    Some(p) => vec![p],
                    None => {
                        eprintln!("[harness] unknown profile `{want}`");
                        usage();
                    }
                }
            };
            if let Some(p) = &plan {
                if profiles.len() != 1 || profiles[0].name != p.profile {
                    eprintln!(
                        "[harness] the plan was emitted for profile `{}`; a plan replays ONE \
                         profile, so run it by name rather than with `all`",
                        p.profile
                    );
                    std::process::exit(2);
                }
                if let Some(max) = levels.iter().max() {
                    if *max > p.clients {
                        eprintln!(
                            "[harness] the plan holds {} client stream(s) and this sweep runs \
                             up to {max}: re-emit with --clients >= {max} rather than \
                             wrapping (two clients replaying one stream is not this workload)",
                            p.clients
                        );
                        std::process::exit(2);
                    }
                }
                // The same arithmetic the emitter refuses on, asked again
                // on the replay side — because a plan can be emitted correctly
                // for 5-second levels and then replayed into 20-second ones,
                // and the file itself carries no level duration to check
                // against. This is the gate that stands between a wrong `--ops`
                // and ten hours of NOT QUOTABLE.
                let plan_rate = num(&args, "--plan-rate", ASSUMED_PEAK_OPS_PER_CLIENT_SEC).max(1);
                if let Some(why) = undersized_because(p.ops_per_client, seconds, plan_rate) {
                    eprintln!("[harness] REFUSING to replay an undersized plan: {why}");
                    eprintln!(
                        "[harness] this is refused BEFORE the sweep rather than after it: \
                         every level would drain early, every level would be correctly \
                         refused, and the whole run would be a complete set of rows nobody \
                         can quote. If this engine really is slower than {plan_rate} ops/s \
                         per client, say so with --plan-rate and the requirement drops."
                    );
                    std::process::exit(2);
                }
                eprintln!(
                    "[harness] replaying {} ({} clients x {} ops, emitter={}, sha256={}) \
                     — sized for {seconds}s levels at up to {plan_rate} ops/s per client",
                    p.profile, p.clients, p.ops_per_client, p.emitter, p.sha256
                );
                if p.emitter != "engram-bench/harness" {
                    eprintln!(
                        "[harness] NOTE: this plan was written by `{}`, not by this harness. \
                         A comparison built on it is a comparison on a self-generated plan.",
                        p.emitter
                    );
                }
            }
            let mut control = match target.open() {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[harness] cannot reach {addr}: {e}");
                    std::process::exit(1);
                }
            };
            let engine = control.engine().to_string();
            let version = control.version().to_string();
            let mut integrity: Vec<String> = Vec::new();
            // Fixtures: the indexes the read shapes need, and the forced first
            // seek so an index BUILD is timed as itself rather than charged to
            // whichever operation drew it. Before the corpus, so the synthetic
            // index is built incrementally by the seeding writes exactly as
            // `stress.rs` builds it.
            for stmt in cat
                .fixture(spec.dataset.fixture_group(), dialect, "indexes")
                .unwrap_or_default()
            {
                if let Err(e) = control.run(&stmt) {
                    eprintln!("[harness] could not create an index ({stmt}): {e}");
                    std::process::exit(1);
                }
            }
            for probe in cat
                .fixture(spec.dataset.fixture_group(), dialect, "probes")
                .unwrap_or_default()
            {
                let t = Instant::now();
                if let Err(e) = control.run(&probe) {
                    eprintln!("[harness] index warm probe failed ({probe}): {e}");
                    std::process::exit(1);
                }
                eprintln!(
                    "[harness] index build (first seek): {:.0} ms  {probe}",
                    t.elapsed().as_secs_f64() * 1e3
                );
            }
            // Seed or attach, AFTER the indexes, as `stress.rs` does: the
            // synthetic index is created before its data so it is built
            // incrementally by the seeding writes, which is why the synthetic
            // fixture declares no index probe.
            let mut spec = spec;
            match seed_or_attach(&cat, dialect, spec.dataset, spec.keys, &mut control) {
                Ok(k) => spec.keys = k,
                Err(e) => {
                    eprintln!("[harness] {e}");
                    std::process::exit(1);
                }
            }
            if let Some(p) = &plan {
                // A plan bound its keys at emit time. Replaying it against a
                // corpus of a different size would issue lookups outside the
                // key space — a run whose reads mostly return nothing measures
                // the index's negative path and reports it as throughput.
                if p.keys != spec.keys {
                    eprintln!(
                        "[harness] the plan was emitted for a key space of {} and this corpus \
                         holds {} — re-emit rather than replaying keys the corpus does not have",
                        p.keys, spec.keys
                    );
                    std::process::exit(2);
                }
            }
            let mut levels_out = Vec::new();
            // A plan that runs out has told us everything it is going to tell
            // us. Running the other nineteen levels to learn the same thing
            // nineteen more times is what turns a sizing mistake into a
            // ten-hour one, so the sweep STOPS at the first exhausted level
            // and reports the `--ops` that would have covered it.
            let continue_after_exhaustion =
                args.iter().any(|a| a == "--continue-after-plan-exhaustion");
            let mut aborted: Option<String> = None;
            'sweep: for prof in &profiles {
                println!(
                    "\n=== profile: {} ({}% writes) — {}",
                    prof.name, prof.write_pct, prof.what
                );
                println!(
                    "{:>7} {:>10} {:>10} {:>9} {:>9} {:>9} {:>9} {:>7} {:>8} {:>7} {:>7} {:>9}",
                    "clients",
                    "ops/s",
                    "r_ops/s",
                    "p50(ms)",
                    "p95(ms)",
                    "p99(ms)",
                    "max(ms)",
                    "errors",
                    "refusals",
                    "trend",
                    "floor",
                    "inflight"
                );
                for (level_index, &k) in levels.iter().enumerate() {
                    let res = run_level(
                        &target,
                        dialect,
                        &cat,
                        spec,
                        prof,
                        k,
                        seconds,
                        level_index,
                        plan.as_ref(),
                        writes_mode,
                        &mut control,
                        &mut integrity,
                    );
                    let all = res.all_latencies();
                    println!(
                        "{:>7} {:>10.0} {:>10.0} {:>9.2} {:>9.2} {:>9.2} {:>9.2} {:>7} {:>8} \
                         {:>7.2} {:>7.2} {:>9}",
                        res.clients,
                        res.rps(),
                        res.r_ops as f64 / res.secs,
                        engram_bench::report::pct(&all, 0.50) as f64 / 1000.0,
                        engram_bench::report::pct(&all, 0.95) as f64 / 1000.0,
                        engram_bench::report::pct(&all, 0.99) as f64 / 1000.0,
                        all.last().copied().unwrap_or(0) as f64 / 1000.0,
                        res.errors,
                        res.refusals,
                        res.trend(),
                        res.floor(),
                        res.max_inflight,
                    );
                    if let Some(why) = res.not_quotable_because(all.last().copied().unwrap_or(0)) {
                        println!("        NOT QUOTABLE — {why}");
                    }
                    let drained = !res.plan_exhausted.is_empty();
                    let sufficient = res.sufficient_plan_ops();
                    levels_out.push(res);
                    if drained && !continue_after_exhaustion {
                        let cure = sufficient.map_or_else(
                            || "a larger --ops".to_string(),
                            |n| format!("`--ops {n}`"),
                        );
                        let msg = format!(
                            "the sweep STOPPED at {}@{k}: the plan ran out inside the \
                             level, so nothing here measures the engine. Re-emit with \
                             {cure} and run again. Every remaining level would fail the \
                             same way, so running them costs hours and learns nothing \
                             (--continue-after-plan-exhaustion runs them anyway).",
                            prof.name
                        );
                        println!("        {msg}");
                        eprintln!("[harness] {msg}");
                        aborted = Some(msg);
                        break 'sweep;
                    }
                }
            }
            let mut failures = Vec::new();
            if let Some(msg) = aborted {
                failures.push(msg);
            }
            for r in &levels_out {
                // A level whose plan ran out stopped issuing work partway
                // through the window, so its second half is empty and its
                // 10th-percentile second is zero — by CONSTRUCTION, not
                // because the engine did anything. Reporting those as
                // "throughput DEGRADED" and "throughput STALLED" manufactures
                // two findings out of one operator error, and a reader
                // triaging the run would go looking for a compaction cliff
                // that is not there. The exhaustion refusal below still fires;
                // it is the only true statement available about this level.
                let drained = !r.plan_exhausted.is_empty();
                if r.errors > 0 {
                    failures.push(format!(
                        "{} @ {} clients: {} transport error(s) — the server dropped \
                         connections",
                        r.profile, r.clients, r.errors
                    ));
                }
                if !drained && r.judged() && r.trend() < TREND_COLLAPSE {
                    failures.push(format!(
                        "{} @ {} clients: throughput DEGRADED over the run (second half was \
                         {:.0}% of the first)",
                        r.profile,
                        r.clients,
                        r.trend() * 100.0
                    ));
                }
                if !drained && r.judged() && r.floor() < FLOOR_STALL {
                    failures.push(format!(
                        "{} @ {} clients: throughput STALLED within the level \
                         (10th-percentile second was {:.0}% of the median)",
                        r.profile,
                        r.clients,
                        r.floor() * 100.0
                    ));
                }
                // THE OTHER DIRECTION. The DEGRADED check above distrusts a
                // collapsing level and nothing looked upward, so a K=1 level at
                // trend 1.65 — the second half at 165% of the first, all of it
                // warm-up — passed clean. The counterpart is
                // `NotQuotable::WarmUpRamp` rather than a third line here, and
                // deliberately: DEGRADED is a claim about the ENGINE, which a
                // reader can weigh; a warm-up ramp is a claim that the MEAN IS
                // NOT A RATE, which is the quotability rule set's job and has
                // to reach the JSON, the comparison table and the LadybugDB arm
                // — none of which read this loop. It arrives below, through
                // `not_quotable_because`.
                //
                // Between TREND_WARMUP_WARN and TREND_WARMUP_REFUSE it is a
                // WARNING and the row is still quotable: some warm-up is
                // expected on the first level of a run, and refusing it would
                // delete the K=1 row every scaling ratio is divided by. The
                // warning is printed rather than swallowed, because a reader
                // told nothing assumes nothing happened.
                if let Some(note) = r.warm_up_warning() {
                    println!(
                        "        WARM-UP — {} @ {} clients: {note}",
                        r.profile, r.clients
                    );
                }
                let max = r.all_latencies().last().copied().unwrap_or(0);
                if let Some(why) = r.not_quotable_because(max) {
                    failures.push(format!(
                        "{} @ {} clients: NOT QUOTABLE ({:.2} ops/s must not be compared) — \
                         {why}",
                        r.profile,
                        r.clients,
                        r.rps()
                    ));
                }
            }
            let report = RunReport {
                workload: Workload::Stress,
                engine,
                engine_version: version,
                dialect: dialect.key().to_string(),
                addr: addr.clone(),
                dataset: spec.dataset.name().to_string(),
                // The same string the rig carries, so the document cannot say
                // one scale in one field and another in the other.
                corpus: scale.clone(),
                seed: spec.seed,
                keys: spec.keys,
                catalogue_digest: engram_bench::catalogue::digest(),
                plan_sha256: plan.as_ref().map(|p| p.sha256.clone()),
                plan_emitter: plan.as_ref().map(|p| p.emitter.clone()),
                op_source: if plan.is_some() { "plan" } else { "live" }.to_string(),
                writes_mode: writes_mode.to_string(),
                rig,
                rig_check,
                fairness,
                fairness_check,
                levels: levels_out,
                queries: Vec::new(),
                integrity,
                failures,
            };
            finish(report, flag(&args, "--json"));
        }
        "report" => {
            // A FLAG'S VALUE IS NOT A RESULT DOCUMENT. `--baseline b.json` was
            // swept into this list by the old `!a.starts_with("--")` filter,
            // because a path does not start with `--`. The baseline then
            // loaded as a second result AND, since the candidate was taken as
            // the LAST document loaded, the gate compared the baseline against
            // itself and reported no regression -- while nine unit tests over
            // `regressions()` passed, because the defect was entirely in the
            // wiring. Found by running it end to end against a real document
            // with a deliberately 4x-faster baseline, which it waved through.
            let paths: Vec<&String> = positional_paths(&args[2..]);
            // Two documents build a TABLE; one document plus `--baseline` is a
            // REGRESSION check and is complete on its own.
            let gating = flag(&args, "--baseline").is_some();
            if paths.is_empty() || (paths.len() < 2 && !gating) {
                eprintln!(
                    "[harness] report needs at least two result documents,                      or one with --baseline"
                );
                std::process::exit(2);
            }
            let mut loaded = Vec::new();
            for p in &paths {
                match std::fs::read_to_string(p)
                    .map_err(|e| e.to_string())
                    .and_then(|s| engram_bench::report::parse(&s))
                {
                    Ok(r) => loaded.push(r),
                    Err(e) => {
                        eprintln!("[harness] {p}: {e}");
                        std::process::exit(1);
                    }
                }
            }
            let refs: Vec<&engram_bench::report::Comparable> = loaded.iter().collect();
            // ONE document is a gate, not a table: the table of a single run
            // is a refusal ("needs at least two runs"), and printed above a
            // PASSING verdict it made every gate log open with `REFUSED`.
            if refs.len() >= 2 {
                print!("{}", table(&refs));
            }
            // A refusal must be a FAILED command, not a line at the top of a
            // file. `harness report … > table.txt` is how these get quoted,
            // and a `REFUSED —` line that exits 0 is a footnote — which is
            // exactly the shape this project keeps retracting results over.
            // The table is still printed, because the reason has to be
            // readable; the exit code is what stops it being pasted from a
            // script that checked `$?`.
            // The REGRESSION gate, when a baseline is named. Separate from
            // `compare`, which asks whether two documents MAY be compared at
            // all; this asks whether the newer one got worse. A scheduled lane
            // needs both, and conflating them would let a refused pair report
            // "no regressions".
            if let Some(base_path) = flag(&args, "--baseline") {
                let tolerance = flag(&args, "--max-regression")
                    .and_then(|t| t.parse::<f64>().ok())
                    .unwrap_or(10.0)
                    / 100.0;
                // A floor that failed to parse is refused, not read as 0: the
                // gate would still run, stricter than asked, and its log would
                // not say why every small query suddenly failed it.
                let floor_ms = match flag(&args, "--min-regression-ms") {
                    None => 0.0,
                    Some(t) => match t.parse::<f64>() {
                        Ok(v) if v >= 0.0 => v,
                        _ => {
                            eprintln!(
                                "[harness] --min-regression-ms takes milliseconds, got `{t}`"
                            );
                            std::process::exit(1);
                        }
                    },
                };
                let baseline = match std::fs::read_to_string(base_path)
                    .map_err(|e| e.to_string())
                    .and_then(|t| engram_bench::report::parse(&t))
                {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("[harness] {base_path}: {e}");
                        std::process::exit(1);
                    }
                };
                // Comparability FIRST. Two documents from different rigs can
                // differ by any amount for reasons that are not a regression,
                // and reporting one would be a number nobody can act on.
                // The CANDIDATE is the first document named, never "the last
                // one loaded" -- that was how the baseline ended up being
                // compared with itself. With `--reproduce` every document
                // named is a REPETITION of one candidate, and a key is a
                // regression only when all of them regress it (see
                // `regressions_in_every`): never inferred from the count of
                // documents, because two DIFFERENT binaries' runs named for a
                // table would then hide a regression in either.
                let reproduce = args.iter().any(|a| a == "--reproduce");
                let candidates: Vec<&engram_bench::report::Comparable> = if reproduce {
                    loaded.iter().collect()
                } else {
                    vec![&loaded[0]]
                };
                for (i, &candidate) in candidates.iter().enumerate() {
                    // The REASON is printed here: the table above covers only
                    // the candidates, so with one candidate its REFUSED line
                    // is about the table, never about the baseline pair it
                    // seemed to name.
                    if let Err(why) = engram_bench::report::compare(&[&baseline, candidate]) {
                        eprintln!("[harness] REFUSED — {}: {why}", paths[i]);
                        eprintln!(
                            "[harness] the baseline and the candidate are NOT comparable; \
                             no regression verdict was reached."
                        );
                        std::process::exit(3);
                    }
                    // The same key under different BOUND PARAMETERS is a
                    // different question: no verdict, for any key, until the
                    // files agree.
                    let moved = engram_bench::report::parameter_mismatches(&baseline, candidate);
                    if !moved.is_empty() {
                        eprintln!(
                            "[harness] the baseline and {} BOUND DIFFERENT PARAMETERS for {} key(s) \
                             -- not the same questions; no regression verdict was reached:",
                            paths[i],
                            moved.len()
                        );
                        for (key, was, now) in &moved {
                            eprintln!("[harness]   {key}: baseline {was}; candidate {now}");
                        }
                        std::process::exit(3);
                    }
                }
                let regs = engram_bench::report::regressions_in_every(
                    &baseline,
                    &candidates,
                    tolerance,
                    floor_ms,
                );
                if !regs.is_empty() {
                    eprintln!(
                        "[harness] {} REGRESSION(S) against {base_path}:",
                        regs.len()
                    );
                    for r in &regs {
                        eprintln!("[harness]   {}: {}", r.key, r.why);
                    }
                    std::process::exit(4);
                }
                println!("[harness] no regression against {base_path}");
                // With ONE document the regression check IS the whole job.
                // Falling through built a table from a single run, which
                // `compare` refuses -- a clean gate then exited 3 and read as
                // a failure.
                if paths.len() == 1 {
                    return;
                }
            }
            if engram_bench::report::compare(&refs).is_err() {
                // The reason is already on stdout, in the table's own first
                // line, where anyone reading the output will see it. This says
                // nothing new on purpose — repeating the prose would just make
                // the refusal easier to skim past.
                eprintln!("[harness] no table was built; see the REFUSED line above");
                std::process::exit(3);
            }
        }
        _ => usage(),
    }
}

/// The thread cap is HANDED IN rather than re-read from the arguments.
///
/// It used to be read here a second time, from the same flag with the same
/// default — which is one flag, two readers, and exactly the shape that lets
/// the number a document RECORDS drift from the number a session RUNS under.
/// One resolution, passed down.
fn build_target(args: &[String], addr: &str, thread_cap: u32) -> Target {
    match flag(args, "--engine").unwrap_or("bolt") {
        "pg" | "postgres" => Target::Pg {
            addr: addr.to_string(),
            user: flag(args, "--pg-user").unwrap_or("postgres").to_string(),
            database: flag(args, "--pg-db").unwrap_or("postgres").to_string(),
            thread_cap,
        },
        _ => Target::Bolt(addr.to_string()),
    }
}

fn finish(report: RunReport, json_out: Option<&str>) -> ! {
    let doc = report.render();
    if let Some(path) = json_out {
        match std::fs::write(path, &doc) {
            Ok(()) => eprintln!("[harness] report written to {path}"),
            Err(e) => eprintln!("[harness] cannot write {path}: {e}"),
        }
    } else {
        println!("{doc}");
    }
    println!();
    if report.pass() {
        println!(
            "PASS — {} level(s), {} quer(y|ies): no integrity finding, nothing unquotable",
            report.levels.len(),
            report.queries.len()
        );
        std::process::exit(0);
    }
    println!("FAIL");
    for b in report.integrity.iter().chain(report.failures.iter()) {
        println!("   - {b}");
    }
    std::process::exit(1);
}

/// The positional documents in `args`, with flags AND THEIR VALUES removed.
///
/// A flag's value does not start with `--`, so a filter of
/// `!a.starts_with("--")` keeps it — and `--baseline b.json` then loaded
/// `b.json` as a second RESULT document. With the candidate taken as the last
/// document loaded, the gate compared the baseline against itself and reported
/// no regression. Nine unit tests over `regressions()` passed throughout,
/// because the defect was entirely in the wiring; it was found by running the
/// gate end to end against a deliberately 4x-faster baseline and watching it
/// wave the run through.
fn positional_paths(args: &[String]) -> Vec<&String> {
    /// Flags that consume the argument after them.
    const VALUED: [&str; 3] = ["--baseline", "--max-regression", "--min-regression-ms"];
    let mut out = Vec::new();
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if a.starts_with("--") {
            skip = VALUED.contains(&a.as_str());
            continue;
        }
        out.push(a);
    }
    out
}

#[cfg(test)]
#[allow(non_snake_case)]
mod positional_path_tests {
    use super::positional_paths;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn a_flags_VALUE_is_not_a_result_document() {
        let args = v(&["run.json", "--baseline", "base.json"]);
        assert_eq!(positional_paths(&args), vec!["run.json"]);
    }

    #[test]
    fn the_tolerance_value_is_not_a_result_document_either() {
        let args = v(&[
            "run.json",
            "--baseline",
            "base.json",
            "--max-regression",
            "25",
        ]);
        assert_eq!(positional_paths(&args), vec!["run.json"]);
    }

    #[test]
    fn the_floor_value_is_not_a_result_document_either() {
        let args = v(&[
            "run1.json",
            "run2.json",
            "--baseline",
            "base.json",
            "--min-regression-ms",
            "5",
            "--reproduce",
        ]);
        assert_eq!(positional_paths(&args), vec!["run1.json", "run2.json"]);
    }

    #[test]
    fn two_documents_still_build_a_table() {
        let args = v(&["a.json", "b.json"]);
        assert_eq!(positional_paths(&args), vec!["a.json", "b.json"]);
    }

    #[test]
    fn a_valueless_flag_does_not_swallow_the_next_document() {
        // `--json` style flags are not in VALUED here; a bare flag must not
        // eat the path after it.
        let args = v(&["a.json", "--some-switch", "b.json"]);
        assert_eq!(positional_paths(&args), vec!["a.json", "b.json"]);
    }
}
