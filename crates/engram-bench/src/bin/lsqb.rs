//! `lsqb <bolt-addr> [--json <out>] [--queries q1,q3] [--timeout-secs N]
//! [--expect <counts.json>]` — the LSQB cyclic-join lane.
//!
//! LSQB (the LDBC Labelled Subgraph Query Benchmark) is nine subgraph-COUNTING
//! queries over the SNB schema — cyclic joins, triangles, anti-joins — each
//! returning one number. This lane runs the nine official queries against a
//! corpus loaded by `snbload` from an `snbgen` export, records wall time and
//! the count, and writes a JSON report.
//!
//! # Adaptation to the loaded schema
//!
//! The `snbgen` corpus carries the FULL label/type vocabulary the official
//! queries use — `Message` as a supertype label on both `Post` and `Comment`,
//! `Country`/`City` as secondary labels beside `Place`, and every relationship
//! type (`CONTAINER_OF`, `HAS_MEMBER`, `REPLY_OF`, …) — so all nine queries
//! map 1:1 and the adapted text below is the official text, line-joined. The
//! per-query `divergence` field exists so that any future schema drift is
//! declared rather than silently absorbed; a query the schema cannot express
//! would be emitted with status `unmappable` and the reason, never dropped.
//! Counts are properties of the corpus (snbgen synthetic, not LDBC Datagen):
//! comparable across engines loading the SAME corpus, not against published
//! LSQB numbers.
//!
//! # Self-validation, fail-closed
//!
//! The columnar toggle is a Graph-level programmatic flag
//! (`Graph::set_columnar_scans`); there is no per-Bolt-session or per-statement
//! switch, so "run twice in differently-configured sessions" is not reachable
//! over the wire. The validation this lane CAN do, it does:
//!
//! - **Census first.** An empty corpus would answer 0 to everything and pass
//!   vacuously; a run against one fails before measuring anything.
//! - **Existence probes.** Each query's aggregate is re-derived as the same
//!   pattern with `RETURN 1 LIMIT 1`. A count of 0 where the probe found the
//!   pattern is `zero_on_populated` — a FAILURE, the "measures the index's
//!   negative path" trap. A 0 whose probe errored is `unverified_zero`, also a
//!   failure: an unverifiable zero does not pass. A positive count whose probe
//!   found nothing is `inconsistent` — the engine disagreeing with itself.
//! - **`--expect <json>`.** Reference counts (a flat `{"q1": 123, …}` map, or
//!   this tool's own `--json` output from another engine) are compared and any
//!   mismatch fails the run.
//! - **Timeouts are `timeout`,** never success. Each statement runs on its own
//!   connection in its own thread with a `recv_timeout` deadline; a timed-out
//!   worker is abandoned (the server may keep computing — expect residual load
//!   after a timeout, which is why every statement gets a FRESH connection).
//!
//! This binary needs real threads and a real clock, which the simulation
//! layer's `Runtime` deliberately does not provide — the same lint waiver
//! `snbconc` and `stress` carry, for the same reason.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use engram_bolt::client::Client;
use engram_cypher::Value;
use engram_cypher::json::{from_json, to_json};

// ─── The query table ────────────────────────────────────────────────────────

/// One LSQB query, adapted to the loaded schema.
struct QuerySpec {
    /// The official LSQB name, `q1`..`q9`.
    name: &'static str,
    /// The adapted statement; `None` if the loaded schema cannot express the
    /// official join shape (`unmappable` then says why).
    adapted: Option<&'static str>,
    /// Where the adapted text departs from the official query, the departure,
    /// stated; `None` means the text is the official text.
    divergence: Option<&'static str>,
    /// Required when `adapted` is `None`: why the query cannot be mapped.
    unmappable: Option<&'static str>,
}

/// All nine official LSQB queries. Text is the official cypher/q{n}.cypher
/// from ldbc/lsqb, line-joined; the snbgen schema needs no label or type
/// renames (see the module docs).
const QUERIES: &[QuerySpec] = &[
    QuerySpec {
        name: "q1",
        adapted: Some(
            "MATCH (:Country)<-[:IS_PART_OF]-(:City)<-[:IS_LOCATED_IN]-(:Person)\
             <-[:HAS_MEMBER]-(:Forum)-[:CONTAINER_OF]->(:Post)<-[:REPLY_OF]-(:Comment)\
             -[:HAS_TAG]->(:Tag)-[:HAS_TYPE]->(:TagClass) RETURN count(*) AS count",
        ),
        divergence: None,
        unmappable: None,
    },
    QuerySpec {
        name: "q2",
        adapted: Some(
            "MATCH (person1:Person)-[:KNOWS]-(person2:Person), \
             (person1)<-[:HAS_CREATOR]-(comment:Comment)-[:REPLY_OF]->(post:Post)\
             -[:HAS_CREATOR]->(person2) RETURN count(*) AS count",
        ),
        divergence: None,
        unmappable: None,
    },
    QuerySpec {
        name: "q3",
        adapted: Some(
            "MATCH (country:Country) \
             MATCH (person1:Person)-[:IS_LOCATED_IN]->(city1:City)-[:IS_PART_OF]->(country) \
             MATCH (person2:Person)-[:IS_LOCATED_IN]->(city2:City)-[:IS_PART_OF]->(country) \
             MATCH (person3:Person)-[:IS_LOCATED_IN]->(city3:City)-[:IS_PART_OF]->(country) \
             MATCH (person1)-[:KNOWS]-(person2)-[:KNOWS]-(person3)-[:KNOWS]-(person1) \
             RETURN count(*) AS count",
        ),
        divergence: None,
        unmappable: None,
    },
    QuerySpec {
        name: "q4",
        adapted: Some(
            "MATCH (:Tag)<-[:HAS_TAG]-(message:Message)-[:HAS_CREATOR]->(creator:Person), \
             (message)<-[:LIKES]-(liker:Person), \
             (message)<-[:REPLY_OF]-(comment:Comment) RETURN count(*) AS count",
        ),
        divergence: None,
        unmappable: None,
    },
    QuerySpec {
        name: "q5",
        adapted: Some(
            "MATCH (tag1:Tag)<-[:HAS_TAG]-(message:Message)<-[:REPLY_OF]-(comment:Comment)\
             -[:HAS_TAG]->(tag2:Tag) WHERE tag1 <> tag2 RETURN count(*) AS count",
        ),
        divergence: None,
        unmappable: None,
    },
    QuerySpec {
        name: "q6",
        adapted: Some(
            "MATCH (person1:Person)-[:KNOWS]-(person2:Person)-[:KNOWS]-(person3:Person)\
             -[:HAS_INTEREST]->(tag:Tag) WHERE person1 <> person3 RETURN count(*) AS count",
        ),
        divergence: None,
        unmappable: None,
    },
    QuerySpec {
        name: "q7",
        adapted: Some(
            "MATCH (:Tag)<-[:HAS_TAG]-(message:Message)-[:HAS_CREATOR]->(creator:Person) \
             OPTIONAL MATCH (message)<-[:LIKES]-(liker:Person) \
             OPTIONAL MATCH (message)<-[:REPLY_OF]-(comment:Comment) \
             RETURN count(*) AS count",
        ),
        divergence: None,
        unmappable: None,
    },
    QuerySpec {
        name: "q8",
        adapted: Some(
            "MATCH (tag1:Tag)<-[:HAS_TAG]-(message:Message)<-[:REPLY_OF]-(comment:Comment)\
             -[:HAS_TAG]->(tag2:Tag) \
             WHERE NOT (comment)-[:HAS_TAG]->(tag1) AND tag1 <> tag2 \
             RETURN count(*) AS count",
        ),
        divergence: None,
        unmappable: None,
    },
    QuerySpec {
        name: "q9",
        adapted: Some(
            "MATCH (person1:Person)-[:KNOWS]-(person2:Person)-[:KNOWS]-(person3:Person)\
             -[:HAS_INTEREST]->(tag:Tag) \
             WHERE NOT (person1)-[:KNOWS]-(person3) AND person1 <> person3 \
             RETURN count(*) AS count",
        ),
        divergence: None,
        unmappable: None,
    },
];

/// The census statements: an empty corpus answers 0 to every lane query and
/// would pass vacuously, so a run against one fails before measuring.
const CENSUS_NODES: &str = "MATCH (n) RETURN count(n) AS c";
const CENSUS_PERSONS: &str = "MATCH (p:Person) RETURN count(p) AS c";

/// Derive the existence probe from an adapted query: the same pattern (WHERE
/// clauses included — an anti-join query's zero is only provable with the NOT
/// applied) with the aggregate replaced by `RETURN 1 LIMIT 1`. `None` when the
/// query does not end in the count suffix — a table entry that broke the
/// contract, which the tests refuse.
fn probe_for(adapted: &str) -> Option<String> {
    adapted
        .strip_suffix("RETURN count(*) AS count")
        .map(|prefix| format!("{prefix}RETURN 1 LIMIT 1"))
}

// ─── Wire execution with a deadline ─────────────────────────────────────────

/// What one statement did on the wire.
enum Wire {
    /// Rows came back (first column of each), in the given wall milliseconds.
    Rows(Vec<Value>, f64),
    /// The server (or the socket) refused.
    Failed(String),
    /// The deadline passed first. The worker thread is abandoned; the server
    /// may still be computing.
    TimedOut,
}

/// Run one statement on a FRESH connection in its own thread, with a deadline.
/// A fresh connection per statement means an abandoned (timed-out) worker can
/// never poison a later statement's session.
fn run_wire(addr: &str, cypher: &str, timeout: Duration) -> Wire {
    let (tx, rx) = mpsc::channel();
    let addr = addr.to_string();
    let cypher = cypher.to_string();
    std::thread::spawn(move || {
        let outcome = (|| -> Result<(Vec<Value>, f64), String> {
            let mut c = Client::connect(&addr).map_err(|e| format!("connect: {e}"))?;
            let t0 = Instant::now();
            let rows = c.query(&cypher).map_err(|e| format!("query: {e}"))?;
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
///
/// A row from `Client::query` is the RECORD's one field decoded: the LIST of
/// column values (`Value::List([Int(n)])` for a count) — verified against a
/// live server, where a bare-`Int` assumption failed the census. A bare `Int`
/// is still accepted so a future client that unwraps single columns keeps
/// working.
fn extract_count(rows: &[Value]) -> Result<i64, String> {
    match rows {
        [Value::Int(n)] => Ok(*n),
        [Value::List(cols)] => match cols.as_slice() {
            [Value::Int(n)] => Ok(*n),
            other => Err(format!("query returned a non-integer count row: {other:?}")),
        },
        [] => Err("query returned no rows (count(*) must return exactly one)".to_string()),
        [other] => Err(format!("query returned a non-integer count: {other:?}")),
        more => Err(format!("query returned {} rows, expected 1", more.len())),
    }
}

// ─── Judgement ──────────────────────────────────────────────────────────────

/// What the existence probe established.
enum Probe {
    /// The pattern has at least one embedding.
    Exists,
    /// The pattern provably has none.
    Absent,
    /// The probe could not answer (error or timeout) — absence is UNPROVEN.
    Failed(String),
}

/// Per-query verdict.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Ok,
    Timeout,
    Error,
    ZeroOnPopulated,
    UnverifiedZero,
    Mismatch,
    Inconsistent,
    Unmappable,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Timeout => "timeout",
            Status::Error => "error",
            Status::ZeroOnPopulated => "zero_on_populated",
            Status::UnverifiedZero => "unverified_zero",
            Status::Mismatch => "mismatch",
            Status::Inconsistent => "inconsistent",
            Status::Unmappable => "unmappable",
        }
    }
    /// Whether this status keeps the run's PASS alive. `unmappable` is a
    /// declared corpus limitation, not a measurement failure — but a run in
    /// which NOTHING measured `ok` still fails (see `overall_pass`).
    fn passes(self) -> bool {
        matches!(self, Status::Ok | Status::Unmappable)
    }
}

/// Judge a measured count against the probe and any expected value.
/// Pure, and deliberately fail-closed: the only zeros that pass are ones whose
/// absence the probe PROVED.
fn judge(count: i64, probe: &Probe, expected: Option<i64>) -> (Status, Option<String>) {
    if let Some(e) = expected {
        if count != e {
            return (
                Status::Mismatch,
                Some(format!("expected {e} (from --expect), measured {count}")),
            );
        }
    }
    if count == 0 {
        return match probe {
            Probe::Exists => (
                Status::ZeroOnPopulated,
                Some("count is 0 but the existence probe found the pattern".to_string()),
            ),
            Probe::Absent => (
                Status::Ok,
                Some("pattern provably absent (existence probe returned no row)".to_string()),
            ),
            Probe::Failed(e) => (
                Status::UnverifiedZero,
                Some(format!(
                    "count is 0 and the existence probe could not prove absence: {e}"
                )),
            ),
        };
    }
    match probe {
        Probe::Absent => (
            Status::Inconsistent,
            Some(format!(
                "count is {count} but the existence probe found no row — the engine disagrees with itself"
            )),
        ),
        Probe::Failed(e) => (
            Status::Ok,
            Some(format!("count is non-zero; note: existence probe failed ({e})")),
        ),
        Probe::Exists => (Status::Ok, None),
    }
}

/// The whole-run verdict: every query passed AND at least one actually
/// measured `ok` — a run of nine `unmappable` entries compared nothing and
/// must not pass (the qcompare rule).
fn overall_pass(outcomes: &[QueryOutcome]) -> bool {
    outcomes.iter().all(|o| o.status.passes()) && outcomes.iter().any(|o| o.status == Status::Ok)
}

// ─── Outcomes and the JSON report ───────────────────────────────────────────

/// Everything the report records about one query.
struct QueryOutcome {
    name: &'static str,
    adapted: Option<&'static str>,
    divergence: Option<&'static str>,
    count: Option<i64>,
    millis: Option<f64>,
    status: Status,
    probe: String,
    detail: Option<String>,
    expected: Option<i64>,
}

fn opt_str(v: Option<&str>) -> Value {
    v.map(|s| Value::Str(s.to_string())).unwrap_or(Value::Null)
}
fn opt_int(v: Option<i64>) -> Value {
    v.map(Value::Int).unwrap_or(Value::Null)
}

fn outcome_value(o: &QueryOutcome) -> Value {
    let mut m = BTreeMap::new();
    m.insert("query".to_string(), Value::Str(o.name.to_string()));
    m.insert("adapted_cypher".to_string(), opt_str(o.adapted));
    m.insert("count".to_string(), opt_int(o.count));
    m.insert(
        "millis".to_string(),
        o.millis.map(Value::Float).unwrap_or(Value::Null),
    );
    m.insert(
        "status".to_string(),
        Value::Str(o.status.as_str().to_string()),
    );
    m.insert("divergence".to_string(), opt_str(o.divergence));
    m.insert("probe".to_string(), Value::Str(o.probe.clone()));
    m.insert("detail".to_string(), opt_str(o.detail.as_deref()));
    m.insert("expected".to_string(), opt_int(o.expected));
    Value::Map(m)
}

/// Facts about the run itself, carried into the report.
struct RunMeta {
    addr: String,
    timeout_secs: u64,
    census_nodes: Option<i64>,
    census_persons: Option<i64>,
    census_error: Option<String>,
    expect_used: bool,
}

/// Render the full report. Emission goes through the engine's own JSON writer
/// (`engram_cypher::json::to_json`), so escaping is the audited path, and the
/// tests round-trip hostile strings through `from_json` to hold it there.
fn render_json(meta: &RunMeta, outcomes: &[QueryOutcome], pass: bool) -> String {
    let mut m = BTreeMap::new();
    m.insert("tool".to_string(), Value::Str("lsqb".to_string()));
    m.insert("addr".to_string(), Value::Str(meta.addr.clone()));
    m.insert(
        "timeout_secs".to_string(),
        Value::Int(meta.timeout_secs as i64),
    );
    m.insert("census_nodes".to_string(), opt_int(meta.census_nodes));
    m.insert("census_persons".to_string(), opt_int(meta.census_persons));
    m.insert(
        "census_error".to_string(),
        opt_str(meta.census_error.as_deref()),
    );
    m.insert(
        "validation".to_string(),
        Value::Str(
            if meta.expect_used {
                "existence-probes + expected-counts"
            } else {
                "existence-probes"
            }
            .to_string(),
        ),
    );
    m.insert(
        "validation_note".to_string(),
        Value::Str(
            "columnar is a Graph-level flag (Graph::set_columnar_scans) with no per-session \
             switch, so dual-configured runs are not reachable over Bolt; zeros pass only \
             when an existence probe proves the pattern absent"
                .to_string(),
        ),
    );
    m.insert(
        "corpus_note".to_string(),
        Value::Str(
            "counts are properties of WHATEVER corpus is loaded (snbgen synthetic or \
             converted official Datagen — this tool cannot tell): comparable across \
             engines loading the same corpus, never against published LSQB results"
                .to_string(),
        ),
    );
    m.insert("pass".to_string(), Value::Bool(pass));
    m.insert(
        "queries".to_string(),
        Value::List(outcomes.iter().map(outcome_value).collect()),
    );
    to_json(&Value::Map(m))
}

// ─── Argument parsing ───────────────────────────────────────────────────────

/// Validate a `--queries` list against the table. Unknown names refuse (a
/// typo silently selecting nothing would pass vacuously); output is in TABLE
/// order regardless of argument order, so runs are comparable.
fn parse_queries_arg(arg: &str) -> Result<Vec<&'static str>, String> {
    let wanted: Vec<&str> = arg
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if wanted.is_empty() {
        return Err("--queries selected nothing".to_string());
    }
    for w in &wanted {
        if !QUERIES.iter().any(|q| q.name == *w) {
            let known: Vec<&str> = QUERIES.iter().map(|q| q.name).collect();
            return Err(format!("unknown query {w:?}; known: {}", known.join(",")));
        }
    }
    Ok(QUERIES
        .iter()
        .map(|q| q.name)
        .filter(|n| wanted.contains(n))
        .collect())
}

/// Parse an `--expect` document: either a flat `{"q1": 123, …}` map, or this
/// tool's own report (`{"queries":[{"query":…,"count":…}]}`) from another
/// engine. Unknown query names and non-integer counts refuse — a typo must not
/// silently expect nothing. Entries whose count is null (an earlier run's
/// timeout) carry no expectation and are skipped.
fn parse_expect(src: &str) -> Result<BTreeMap<String, i64>, String> {
    let v = from_json(src).map_err(|e| format!("--expect is not JSON: {e}"))?;
    let Value::Map(m) = v else {
        return Err("--expect must be a JSON object".to_string());
    };
    let mut out = BTreeMap::new();
    if let Some(Value::List(entries)) = m.get("queries") {
        for e in entries {
            let Value::Map(em) = e else {
                return Err("--expect: entries under \"queries\" must be objects".to_string());
            };
            let Some(Value::Str(name)) = em.get("query") else {
                return Err("--expect: an entry under \"queries\" has no \"query\" name".to_string());
            };
            if !QUERIES.iter().any(|q| q.name == name.as_str()) {
                return Err(format!("--expect names unknown query {name:?}"));
            }
            match em.get("count") {
                Some(Value::Int(n)) => {
                    out.insert(name.clone(), *n);
                }
                Some(Value::Null) | None => {} // unmeasured upstream — no expectation
                Some(other) => {
                    return Err(format!("--expect: {name} has a non-integer count {other:?}"));
                }
            }
        }
    } else {
        for (k, val) in &m {
            if !QUERIES.iter().any(|q| q.name == k.as_str()) {
                return Err(format!("--expect names unknown query {k:?}"));
            }
            match val {
                Value::Int(n) => {
                    out.insert(k.clone(), *n);
                }
                other => {
                    return Err(format!("--expect: {k} must be an integer, got {other:?}"));
                }
            }
        }
    }
    if out.is_empty() {
        return Err("--expect carries no usable counts".to_string());
    }
    Ok(out)
}

// ─── The run ────────────────────────────────────────────────────────────────

/// Probe, then measure, then judge one query.
fn run_one(
    addr: &str,
    spec: &QuerySpec,
    timeout: Duration,
    expected: Option<i64>,
) -> QueryOutcome {
    let Some(adapted) = spec.adapted else {
        return QueryOutcome {
            name: spec.name,
            adapted: None,
            divergence: spec.divergence,
            count: None,
            millis: None,
            status: Status::Unmappable,
            probe: "skipped".to_string(),
            detail: Some(
                spec.unmappable
                    .unwrap_or("no reason recorded (table defect)")
                    .to_string(),
            ),
            expected,
        };
    };

    // The existence probe FIRST, so a later zero is judged against evidence
    // gathered before the measured run, not after it.
    let probe_stmt = probe_for(adapted).expect("table entries end in the count suffix (tested)");
    // The probe's wall time travels in its label: a probe is a statement the
    // server ran, and one that takes 180 s (q3, v69–v71) or 69 s (q1 at SF3)
    // behind a 0.6 s count is the kind of number that hid for three days
    // when only the count's millis were reported.
    let (probe, probe_label) = match run_wire(addr, &probe_stmt, timeout) {
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

    match run_wire(addr, adapted, timeout) {
        Wire::TimedOut => QueryOutcome {
            name: spec.name,
            adapted: Some(adapted),
            divergence: spec.divergence,
            count: None,
            millis: Some(timeout.as_secs_f64() * 1000.0),
            status: Status::Timeout,
            probe: probe_label,
            detail: Some(format!(
                "no answer within {}s; the worker was abandoned and the server may still be computing",
                timeout.as_secs()
            )),
            expected,
        },
        Wire::Failed(e) => QueryOutcome {
            name: spec.name,
            adapted: Some(adapted),
            divergence: spec.divergence,
            count: None,
            millis: None,
            status: Status::Error,
            probe: probe_label,
            detail: Some(e),
            expected,
        },
        Wire::Rows(rows, ms) => match extract_count(&rows) {
            Err(e) => QueryOutcome {
                name: spec.name,
                adapted: Some(adapted),
                divergence: spec.divergence,
                count: None,
                millis: Some((ms * 1000.0).round() / 1000.0),
                status: Status::Error,
                probe: probe_label,
                detail: Some(e),
                expected,
            },
            Ok(count) => {
                let (status, detail) = judge(count, &probe, expected);
                QueryOutcome {
                    name: spec.name,
                    adapted: Some(adapted),
                    divergence: spec.divergence,
                    count: Some(count),
                    millis: Some((ms * 1000.0).round() / 1000.0),
                    status,
                    probe: probe_label,
                    detail,
                    expected,
                }
            }
        },
    }
}

fn census(addr: &str, stmt: &str, timeout: Duration) -> Result<i64, String> {
    match run_wire(addr, stmt, timeout) {
        Wire::Rows(rows, _) => extract_count(&rows),
        Wire::Failed(e) => Err(e),
        Wire::TimedOut => Err(format!("census timed out after {}s", timeout.as_secs())),
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: lsqb <bolt-addr> [--json <out>] [--queries q1,q3] [--timeout-secs N] [--expect <counts.json>]"
    );
    eprintln!("  runs the nine LSQB cyclic-join queries against a loaded SNB-schema corpus.");
    eprintln!("  --expect: a flat {{\"q1\": 123, ...}} map, or this tool's own --json report.");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut addr: Option<String> = None;
    let mut json_out: Option<String> = None;
    let mut selected: Option<Vec<&'static str>> = None;
    let mut timeout_secs: u64 = 120;
    let mut expect: Option<BTreeMap<String, i64>> = None;

    let mut i = 1;
    while i < args.len() {
        let take = |i: usize| -> &str {
            args.get(i + 1).map(String::as_str).unwrap_or_else(|| {
                eprintln!("[lsqb] {} needs a value", args[i]);
                usage()
            })
        };
        match args[i].as_str() {
            "--json" => {
                json_out = Some(take(i).to_string());
                i += 2;
            }
            "--queries" => {
                match parse_queries_arg(take(i)) {
                    Ok(v) => selected = Some(v),
                    Err(e) => {
                        eprintln!("[lsqb] {e}");
                        usage();
                    }
                }
                i += 2;
            }
            "--timeout-secs" => {
                match take(i).parse::<u64>() {
                    Ok(n) if n >= 1 => timeout_secs = n,
                    _ => {
                        eprintln!("[lsqb] --timeout-secs needs a positive integer");
                        usage();
                    }
                }
                i += 2;
            }
            "--expect" => {
                let path = take(i).to_string();
                let src = match std::fs::read_to_string(&path) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[lsqb] cannot read --expect {path}: {e}");
                        std::process::exit(2);
                    }
                };
                match parse_expect(&src) {
                    Ok(m) => expect = Some(m),
                    Err(e) => {
                        eprintln!("[lsqb] {e}");
                        std::process::exit(2);
                    }
                }
                i += 2;
            }
            flag if flag.starts_with("--") => {
                eprintln!("[lsqb] unknown flag {flag}");
                usage();
            }
            positional => {
                if addr.is_some() {
                    eprintln!("[lsqb] unexpected extra argument {positional:?}");
                    usage();
                }
                addr = Some(positional.to_string());
                i += 1;
            }
        }
    }
    let Some(addr) = addr else { usage() };
    let timeout = Duration::from_secs(timeout_secs);
    let specs: Vec<&QuerySpec> = match &selected {
        Some(names) => QUERIES.iter().filter(|q| names.contains(&q.name)).collect(),
        None => QUERIES.iter().collect(),
    };

    // ── Census: refuse the vacuous pass before measuring anything ──────────
    let mut meta = RunMeta {
        addr: addr.clone(),
        timeout_secs,
        census_nodes: None,
        census_persons: None,
        census_error: None,
        expect_used: expect.is_some(),
    };
    let census_verdict = census(&addr, CENSUS_NODES, timeout).and_then(|n| {
        meta.census_nodes = Some(n);
        census(&addr, CENSUS_PERSONS, timeout).map(|p| {
            meta.census_persons = Some(p);
            (n, p)
        })
    });
    match census_verdict {
        Err(e) => {
            meta.census_error = Some(e.clone());
            eprintln!("[lsqb] FAIL — census failed against {addr}: {e}");
            let doc = render_json(&meta, &[], false);
            if let Some(path) = &json_out {
                std::fs::write(path, &doc).unwrap_or_else(|e| {
                    eprintln!("[lsqb] cannot write {path}: {e}");
                });
            }
            println!("{doc}");
            std::process::exit(1);
        }
        Ok((n, p)) if n == 0 || p == 0 => {
            let e = format!("corpus is empty ({n} nodes, {p} persons) — nothing to measure");
            meta.census_error = Some(e.clone());
            eprintln!("[lsqb] FAIL — {e}");
            let doc = render_json(&meta, &[], false);
            if let Some(path) = &json_out {
                std::fs::write(path, &doc).unwrap_or_else(|e| {
                    eprintln!("[lsqb] cannot write {path}: {e}");
                });
            }
            println!("{doc}");
            std::process::exit(1);
        }
        Ok((n, p)) => {
            eprintln!(
                "[lsqb] corpus: {n} node(s), {p} person(s); {} quer{} selected; timeout {timeout_secs}s each",
                specs.len(),
                if specs.len() == 1 { "y" } else { "ies" }
            );
        }
    }

    // ── The lane ───────────────────────────────────────────────────────────
    let mut outcomes: Vec<QueryOutcome> = Vec::with_capacity(specs.len());
    for spec in &specs {
        let expected = expect
            .as_ref()
            .and_then(|m| m.get(spec.name))
            .copied();
        let o = run_one(&addr, spec, timeout, expected);
        eprintln!(
            "[lsqb] {:<3} {:<18} count={:<12} {:>10} ms  probe={}{}",
            o.name,
            o.status.as_str(),
            o.count.map(|c| c.to_string()).unwrap_or_else(|| "-".to_string()),
            o.millis.map(|m| format!("{m:.1}")).unwrap_or_else(|| "-".to_string()),
            o.probe,
            o.detail
                .as_deref()
                .map(|d| format!("  ({d})"))
                .unwrap_or_default(),
        );
        outcomes.push(o);
    }

    let pass = overall_pass(&outcomes);
    let doc = render_json(&meta, &outcomes, pass);
    if let Some(path) = &json_out {
        match std::fs::write(path, &doc) {
            Ok(()) => eprintln!("[lsqb] report written to {path}"),
            Err(e) => {
                eprintln!("[lsqb] cannot write {path}: {e}");
                println!("{doc}");
                std::process::exit(1);
            }
        }
    }
    println!("{doc}");
    if pass {
        eprintln!("[lsqb] PASS — every selected query answered and every zero was proven");
    } else {
        eprintln!("[lsqb] FAIL — see per-query statuses above; timings from this run are not quotable");
        std::process::exit(1);
    }
}

// ─── Tests: the pure parts ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_has_exactly_nine_uniquely_named_entries() {
        assert_eq!(QUERIES.len(), 9, "LSQB defines nine queries");
        let names: std::collections::BTreeSet<&str> = QUERIES.iter().map(|q| q.name).collect();
        assert_eq!(names.len(), 9, "duplicate query names in the table");
        for (i, q) in QUERIES.iter().enumerate() {
            assert_eq!(q.name, format!("q{}", i + 1), "table order must be q1..q9");
            // Exactly one of adapted/unmappable, never both, never neither.
            match (q.adapted, q.unmappable) {
                (Some(a), None) => {
                    assert!(
                        a.ends_with("RETURN count(*) AS count"),
                        "{}: adapted text must end in the count suffix (probe derivation)",
                        q.name
                    );
                }
                (None, Some(reason)) => assert!(!reason.is_empty()),
                other => panic!("{}: adapted/unmappable must be exclusive, got {other:?}", q.name),
            }
        }
    }

    /// Every adapted statement, every derived probe, and the census statements
    /// must parse with the ENGINE's parser — this is what catches the
    /// `NOT (x)-[:T]->(y)` pattern-predicate ambiguity without a server.
    /// Parsing is recursive, so the check runs on a thread with the parser's
    /// documented minimum stack, the way the hostile-input tests do.
    #[test]
    fn every_statement_parses_with_the_engine_parser() {
        std::thread::Builder::new()
            .stack_size(engram_cypher::MIN_PARSER_STACK_BYTES)
            .spawn(|| {
                let mut checked = 0usize;
                for stmt in [CENSUS_NODES, CENSUS_PERSONS] {
                    engram_cypher::parse_statement(stmt)
                        .unwrap_or_else(|e| panic!("census {stmt:?} does not parse: {e:?}"));
                    checked += 1;
                }
                for q in QUERIES {
                    let Some(adapted) = q.adapted else { continue };
                    engram_cypher::parse_statement(adapted)
                        .unwrap_or_else(|e| panic!("{} does not parse: {e:?}", q.name));
                    let probe = probe_for(adapted).expect("probe derivation");
                    engram_cypher::parse_statement(&probe)
                        .unwrap_or_else(|e| panic!("{} probe does not parse: {e:?}", q.name));
                    checked += 2;
                }
                // A loop that checked nothing passes every assertion it never
                // made; require the full complement.
                assert_eq!(checked, 2 + 9 * 2);
            })
            .expect("spawn parser-stack thread")
            .join()
            .expect("parse-validation thread panicked");
    }

    #[test]
    fn probe_derivation_replaces_only_the_final_aggregate() {
        for q in QUERIES {
            let Some(adapted) = q.adapted else { continue };
            let probe = probe_for(adapted).unwrap_or_else(|| panic!("{}: no probe", q.name));
            assert!(probe.ends_with("RETURN 1 LIMIT 1"), "{}: {probe}", q.name);
            assert!(!probe.contains("count("), "{}: aggregate survived: {probe}", q.name);
            // The pattern (everything before RETURN) is untouched.
            let pat = adapted.strip_suffix("RETURN count(*) AS count").unwrap();
            assert!(probe.starts_with(pat), "{}: pattern was altered", q.name);
        }
        assert_eq!(probe_for("MATCH (n) RETURN n"), None, "non-count text must refuse");
    }

    #[test]
    fn judge_is_fail_closed() {
        // The only zeros that pass are proven-absent zeros.
        assert_eq!(judge(0, &Probe::Exists, None).0, Status::ZeroOnPopulated);
        assert_eq!(judge(0, &Probe::Absent, None).0, Status::Ok);
        assert_eq!(
            judge(0, &Probe::Failed("x".into()), None).0,
            Status::UnverifiedZero
        );
        // A positive count against a probe that saw nothing is a divergence.
        assert_eq!(judge(5, &Probe::Absent, None).0, Status::Inconsistent);
        assert_eq!(judge(5, &Probe::Exists, None).0, Status::Ok);
        assert_eq!(judge(5, &Probe::Failed("x".into()), None).0, Status::Ok);
        // Expectation mismatches fail even when the probe is happy — and an
        // expect file that itself recorded a broken 0 does not launder one.
        assert_eq!(judge(5, &Probe::Exists, Some(6)).0, Status::Mismatch);
        assert_eq!(judge(5, &Probe::Exists, Some(5)).0, Status::Ok);
        assert_eq!(judge(0, &Probe::Exists, Some(0)).0, Status::ZeroOnPopulated);
    }

    #[test]
    fn overall_pass_requires_at_least_one_measured_ok() {
        let mk = |status: Status| QueryOutcome {
            name: "q1",
            adapted: Some("x"),
            divergence: None,
            count: None,
            millis: None,
            status,
            probe: "exists".to_string(),
            detail: None,
            expected: None,
        };
        assert!(overall_pass(&[mk(Status::Ok)]));
        assert!(overall_pass(&[mk(Status::Ok), mk(Status::Unmappable)]));
        assert!(!overall_pass(&[mk(Status::Unmappable)]), "nothing measured");
        assert!(!overall_pass(&[mk(Status::Ok), mk(Status::Timeout)]));
        assert!(!overall_pass(&[mk(Status::Ok), mk(Status::ZeroOnPopulated)]));
        assert!(!overall_pass(&[]), "an empty run compared nothing");
    }

    #[test]
    fn extract_count_takes_exactly_one_integer_row() {
        // The wire shape: one RECORD whose field list holds one Int column.
        assert_eq!(extract_count(&[Value::List(vec![Value::Int(7)])]), Ok(7));
        // The unwrapped shape stays accepted.
        assert_eq!(extract_count(&[Value::Int(7)]), Ok(7));
        assert!(extract_count(&[]).is_err());
        assert!(extract_count(&[Value::Int(1), Value::Int(2)]).is_err());
        assert!(extract_count(&[Value::Str("7".into())]).is_err());
        assert!(extract_count(&[Value::List(vec![])]).is_err());
        assert!(extract_count(&[Value::List(vec![Value::Int(1), Value::Int(2)])]).is_err());
        assert!(extract_count(&[Value::List(vec![Value::Str("7".into())])]).is_err());
    }

    /// Hostile strings must survive the writer: quotes, backslashes, newlines
    /// and raw control bytes in an error detail or query text, round-tripped
    /// through the same JSON family's parser.
    #[test]
    fn json_report_escapes_hostile_strings() {
        let hostile = "he said \"MATCH\\ (n)\"\nthen\tcontrol:\u{1}";
        let outcome = QueryOutcome {
            name: "q1",
            adapted: Some("MATCH (n) RETURN count(*) AS count"),
            divergence: None,
            count: Some(3),
            millis: Some(12.5),
            status: Status::Error,
            probe: "exists".to_string(),
            detail: Some(hostile.to_string()),
            expected: Some(3),
        };
        let meta = RunMeta {
            addr: "127.0.0.1:7687".to_string(),
            timeout_secs: 120,
            census_nodes: Some(10),
            census_persons: Some(2),
            census_error: None,
            expect_used: true,
        };
        let doc = render_json(&meta, &[outcome], false);
        let parsed = from_json(&doc).expect("report must be valid JSON");
        let Value::Map(m) = parsed else { panic!("report is not an object") };
        assert_eq!(m.get("pass"), Some(&Value::Bool(false)));
        assert_eq!(m.get("timeout_secs"), Some(&Value::Int(120)));
        let Some(Value::List(qs)) = m.get("queries") else { panic!("no queries array") };
        let Value::Map(q) = &qs[0] else { panic!("query entry is not an object") };
        assert_eq!(q.get("detail"), Some(&Value::Str(hostile.to_string())));
        assert_eq!(q.get("count"), Some(&Value::Int(3)));
        assert_eq!(q.get("millis"), Some(&Value::Float(12.5)));
        assert_eq!(q.get("status"), Some(&Value::Str("error".to_string())));
        assert_eq!(q.get("divergence"), Some(&Value::Null));
    }

    #[test]
    fn queries_arg_filters_in_table_order_and_refuses_unknowns() {
        assert_eq!(parse_queries_arg("q3,q1").unwrap(), vec!["q1", "q3"]);
        assert_eq!(parse_queries_arg("q9").unwrap(), vec!["q9"]);
        assert!(parse_queries_arg("q10").is_err());
        assert!(parse_queries_arg("Q1").is_err(), "names are case-sensitive");
        assert!(parse_queries_arg("").is_err(), "selecting nothing must refuse");
        assert!(parse_queries_arg(",,").is_err());
    }

    #[test]
    fn expect_parses_flat_maps_and_own_reports() {
        let flat = parse_expect(r#"{"q1": 10, "q5": 0}"#).unwrap();
        assert_eq!(flat.get("q1"), Some(&10));
        assert_eq!(flat.get("q5"), Some(&0));
        // The tool's own report shape: null counts (an upstream timeout) carry
        // no expectation; integer counts do.
        let own = parse_expect(
            r#"{"queries":[{"query":"q1","count":7},{"query":"q2","count":null}]}"#,
        )
        .unwrap();
        assert_eq!(own.get("q1"), Some(&7));
        assert!(!own.contains_key("q2"));
        // Fail-closed refusals: typos, wrong types, empty expectations.
        assert!(parse_expect(r#"{"qX": 1}"#).is_err());
        assert!(parse_expect(r#"{"q1": "ten"}"#).is_err());
        assert!(parse_expect(r#"{"queries":[{"query":"qX","count":1}]}"#).is_err());
        assert!(parse_expect(r#"{"queries":[{"query":"q2","count":null}]}"#).is_err());
        assert!(parse_expect("[]").is_err());
        assert!(parse_expect("not json").is_err());
    }
}
