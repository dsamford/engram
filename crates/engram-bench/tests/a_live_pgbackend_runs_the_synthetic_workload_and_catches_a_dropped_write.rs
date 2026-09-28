//! `PgBackend` against a real PostgreSQL, including the integrity guard
//! failing on purpose.
//!
//! ```sh
//! kubectl port-forward pod/<your-postgres-pod> 15432:5432
//! ENGRAM_PGWIRE_TEST_ADDR=127.0.0.1:15432 \
//!   cargo test -p engram-bench \
//!   --test a_live_pgbackend_runs_the_synthetic_workload_and_catches_a_dropped_write \
//!   -- --nocapture
//! ```
//!
//! # Why this file exists beside the pgwire one
//!
//! `a_live_postgres_answers_the_pgwire_client_or_the_skip_is_loud` proves the
//! CODEC: that the frames this client writes are the frames PostgreSQL reads.
//! It says nothing about the layer the harness actually calls. `PgBackend`
//! compiled against that codec for a week without issuing a statement, and
//! three of the things wrong with it were invisible from either side alone:
//! the catalogue's SQL seed bound a placeholder nothing supplied, the SQL arm
//! had no schema step at all, and a plain SQL error threw away a connection
//! the codec had explicitly re-synchronised.
//!
//! # The guard, and why it is exercised by breaking it
//!
//! The churn reconciliation is the check that stands between a throughput
//! number and lost data: acked creates minus acked deletes must equal the
//! survivors a FRESH query finds. A guard nobody has watched fail is not known
//! to be a guard — it is a green tick whose meaning is untested — so
//! [`the_churn_ledger_balances_and_then_fails_when_a_row_is_taken_underneath_it`]
//! makes it fail twice, in the two ways it can: one acked row removed behind
//! the ledger's back, and the whole table truncated underneath the probe.
//!
//! # This suite is not a measurement
//!
//! Every table is TEMPORARY and dies with the connection, every statement is
//! trivial, and nothing is timed. Another workstream may be measuring; a
//! number taken from a contended pod is worse than no number, and this file
//! takes none.

use std::collections::BTreeMap;

use engram_bench::backend::{Backend, Cell, PgBackend};
use engram_bench::catalogue::{Catalogue, Dialect, render};
use engram_bench::plan::{LEVEL_STRIDE, scope_params};
use engram_bench::workload::{Param, Reconciliation, reconcile};

const NONCE: u64 = 77;
const CID: u64 = 0;

/// Open a `PgBackend` on a private temporary schema, or explain — loudly —
/// why the test is not running.
///
/// The skip discipline is the one
/// `a_live_postgres_answers_the_pgwire_client_or_the_skip_is_loud` established
/// and for the same reason: `cargo test` hides a passing test's output, so a
/// suite that evaporated looks exactly like a suite that ran.
/// `ENGRAM_PGWIRE_TEST_REQUIRE_LIVE=1` turns the skip into a failure.
fn live_or_skip(what: &str) -> Option<PgBackend> {
    let addr = std::env::var("ENGRAM_PGWIRE_TEST_ADDR")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let Some(addr) = addr else {
        let strict = matches!(
            std::env::var("ENGRAM_PGWIRE_TEST_REQUIRE_LIVE").as_deref(),
            Ok("1") | Ok("true") | Ok("yes")
        );
        assert!(
            !strict,
            "[pgbackend-live] {what} could NOT run: ENGRAM_PGWIRE_TEST_REQUIRE_LIVE says a live \
             run was intended and ENGRAM_PGWIRE_TEST_ADDR is unset. Failing rather than skipping."
        );
        eprintln!(
            "[pgbackend-live] {what} SKIPPED — ENGRAM_PGWIRE_TEST_ADDR unset. Nothing else covers \
             PgBackend against a live server: the scripted-backend suites cover the CODEC, not \
             this layer. Set ENGRAM_PGWIRE_TEST_REQUIRE_LIVE=1 to make this a failure."
        );
        return None;
    };
    let user = std::env::var("ENGRAM_PGWIRE_TEST_USER").unwrap_or_else(|_| "postgres".to_string());
    let db = std::env::var("ENGRAM_PGWIRE_TEST_DB").unwrap_or_else(|_| "postgres".to_string());
    let mut b = PgBackend::connect(&addr, &user, &db, Some(6)).unwrap_or_else(|e| {
        panic!("[pgbackend-live] {what}: could not connect to {addr} as {user}/{db}: {e}")
    });
    isolate(&mut b, what);
    Some(b)
}

/// Put this session's unqualified names in its own temporary schema.
///
/// The bootstrap table is not ceremony. `pg_temp` in `search_path` is IGNORED
/// for object creation until the session's temporary schema exists, so a
/// `SET search_path TO pg_temp` issued first would silently leave `CREATE
/// TABLE stress` landing in `public` — a test that quietly created permanent
/// tables in a shared database, which is the opposite of isolated. Creating
/// one temp table forces the schema into existence, and the assertion below
/// proves the redirection took rather than assuming it.
fn isolate(b: &mut PgBackend, what: &str) {
    b.run("CREATE TEMP TABLE harness_temp_bootstrap (x int)")
        .unwrap_or_else(|e| panic!("[pgbackend-live] {what}: temp bootstrap: {e}"));
    b.run("SET search_path TO pg_temp, public")
        .unwrap_or_else(|e| panic!("[pgbackend-live] {what}: search_path: {e}"));
    let n = b
        .scalar(
            "SELECT count(*) FROM pg_class WHERE relname = 'harness_temp_bootstrap' \
             AND relnamespace = pg_my_temp_schema()",
        )
        .unwrap_or_else(|e| panic!("[pgbackend-live] {what}: temp-schema probe: {e}"));
    assert_eq!(
        n, 1,
        "[pgbackend-live] {what}: this session is NOT isolated to its temporary schema, so \
         everything below would write permanent tables into a shared database"
    );
}

/// Build the synthetic dataset's schema from the catalogue's own text.
///
/// The catalogue's, not a copy: if `sql_schema` and the statements that read
/// those tables ever disagree, this test must break, and it cannot break on a
/// disagreement it holds both halves of.
fn build_schema(b: &mut PgBackend, cat: &Catalogue) {
    for stmt in cat
        .dataset_list("synthetic", "sql_schema")
        .expect("synthetic.sql_schema")
    {
        b.run(&stmt)
            .unwrap_or_else(|e| panic!("schema statement failed ({stmt}): {e}"));
    }
    let landed = b
        .scalar(
            "SELECT count(*) FROM pg_class WHERE relname IN \
             ('stress','link','slink','stressw','uniq','churn','churn_rel','churn_anchor') \
             AND relnamespace = pg_my_temp_schema()",
        )
        .expect("schema placement probe");
    assert_eq!(
        landed, 8,
        "the synthetic schema did not land in the temporary schema; refusing to run the rest \
         of this file against a shared database"
    );
}

fn p(pairs: &[(&str, u64)]) -> BTreeMap<String, Param> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), Param::Uint(*v)))
        .collect()
}

/// Render one catalogue write op with bound parameters, exactly as the harness
/// does.
fn write_stmt(cat: &Catalogue, op: &str, params: &BTreeMap<String, Param>) -> String {
    let entry = cat
        .write_op(op, "synthetic", Dialect::Sql)
        .unwrap_or_else(|e| panic!("catalogue write_op {op}: {e}"));
    let owned: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (k.clone(), v.render()))
        .collect();
    let refs: Vec<(&str, &str)> = owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    render(&entry.text, &refs)
}

/// Render one integrity probe with the level window bound.
fn probe_stmt(cat: &Catalogue, name: &str, level: usize, extra: &[(&str, u64)]) -> String {
    let lo = level as u64 * LEVEL_STRIDE;
    let hi = lo + LEVEL_STRIDE;
    let entry = cat
        .integrity_probe(name, Dialect::Sql)
        .unwrap_or_else(|e| panic!("catalogue integrity_probe {name}: {e}"));
    let mut owned: Vec<(String, String)> = vec![
        ("lo".to_string(), lo.to_string()),
        ("hi".to_string(), hi.to_string()),
        ("nonce".to_string(), NONCE.to_string()),
    ];
    owned.extend(extra.iter().map(|(k, v)| ((*k).to_string(), v.to_string())));
    let refs: Vec<(&str, &str)> = owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    render(&entry.text, &refs)
}

// ─── The connection contract ────────────────────────────────────────────────

#[test]
fn a_pgbackend_names_postgres_and_states_the_fairness_its_session_actually_has() {
    let Some(b) = live_or_skip("identity") else {
        return;
    };
    assert_eq!(b.engine(), "postgres");
    assert_eq!(b.dialect(), Dialect::Sql);
    let v = b.version();
    assert!(
        v.starts_with("PostgreSQL/"),
        "the version travels into every result document and has to name the product: {v}"
    );
    // The thread cap is the half a document could lie about: the reporter
    // refuses a comparison across differing fairness blocks, so a block that
    // agrees while the sessions differ is the one mismatch it cannot see.
    assert!(
        v.contains("max_parallel_workers_per_gather=6"),
        "connect() asked for a 6-core intra-query cap; the version string must report what the \
         session actually got, and it reported: {v}"
    );
    assert!(
        v.contains("shared_buffers="),
        "`--cache-mb` is a claim about the server, not an instruction to it, so the observed \
         value has to travel with the number: {v}"
    );
    assert!(
        !v.contains("unreadable"),
        "a setting came back unreadable, which is the honest word for it and still a finding: {v}"
    );
    eprintln!("[pgbackend-live] version stamp: {v}");
}

#[test]
fn a_sql_error_leaves_the_backend_usable_and_a_later_statement_still_answers() {
    // The regression this file was written around. The first PgBackend dropped
    // its client on every non-refusal error, including `relation does not
    // exist` — which pgwire had already re-synchronised. The control
    // connection is never reconnected inside a level, so ONE bad statement
    // turned every later integrity probe into `connection is closed`: five
    // failures reported, one real cause, and the real cause buried first.
    let Some(mut b) = live_or_skip("error recovery") else {
        return;
    };
    let err = b
        .run("SELECT * FROM a_table_that_does_not_exist")
        .expect_err("a missing relation must be an error");
    assert!(
        !err.is_refusal(),
        "42P01 is not a correct answer under load; it is the run's problem: {err}"
    );
    assert_eq!(
        b.scalar("SELECT 1")
            .expect("the connection must survive a SQL error"),
        1,
        "the backend threw away a connection PostgreSQL had already re-synchronised"
    );
    // Five in a row, because the failure mode was a CASCADE: the first error
    // was survivable and every later probe reported the corpse.
    for i in 0..5 {
        let _ = b.run("SELECT * FROM a_table_that_does_not_exist");
        assert_eq!(
            b.scalar("SELECT 1")
                .unwrap_or_else(|e| panic!("usable after error {i}: {e}")),
            1
        );
    }
}

#[test]
fn a_unique_violation_is_a_refusal_and_everything_else_is_the_runs_problem() {
    let Some(mut b) = live_or_skip("classification") else {
        return;
    };
    let cat = Catalogue::load().expect("catalogue");
    build_schema(&mut b, &cat);

    let first = write_stmt(&cat, "unique_create", &p(&[("u", 1)]));
    assert_eq!(b.run(&first).expect("the first writer wins"), 1);
    let clash = b
        .run(&first)
        .expect_err("the second writer must be refused");
    assert!(
        clash.is_refusal(),
        "`unique-create` expects N-1 refusals per value; classifying 23505 as a transport error \
         would fail every run of the profile that is working: {clash}"
    );
    assert!(
        clash.message().contains("23505"),
        "the SQLSTATE is the only field a program can act on and must survive into the \
         message: {clash}"
    );
    // A syntax error is not the workload succeeding.
    let bad = b.run("SELECT FROM WHERE").expect_err("nonsense must fail");
    assert!(
        !bad.is_refusal(),
        "a syntax error classified as a refusal: {bad}"
    );
}

#[test]
fn run_reports_the_tag_and_query_keeps_the_rows_that_run_discards() {
    let Some(mut b) = live_or_skip("row counts") else {
        return;
    };
    let cat = Catalogue::load().expect("catalogue");
    build_schema(&mut b, &cat);
    b.run("INSERT INTO stress (k, b, pad) SELECT i, i % 16, 'x' FROM generate_series(0, 9) AS i")
        .expect("seed");

    // An INSERT returns no rows and changes ten; `rows.len()` would be zero
    // and a harness that measured write throughput from it would measure
    // nothing.
    assert_eq!(
        b.run("UPDATE stress SET hits = 1 WHERE k < 4")
            .expect("update"),
        4
    );
    assert_eq!(b.run("SELECT k FROM stress").expect("select"), 10);
    let rows = b
        .query("SELECT k, pad FROM stress WHERE k = 3")
        .expect("query");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 2);
    // Postgres answers in text; a reconciliation that read Bolt's Int and not
    // this would silently never run on this engine.
    assert_eq!(rows[0][0], Cell::Text("3".to_string()));
    assert_eq!(rows[0][0].as_int(), Some(3));
    assert_eq!(
        b.scalar("SELECT coalesce(hits, 0) FROM stress WHERE k = 0")
            .expect("scalar"),
        1
    );
    assert_eq!(
        b.pair("SELECT count(*), count(k) FROM stress")
            .expect("pair"),
        (10, 10)
    );
    // A NULL is not an integer, and `scalar` must say so rather than answer 0.
    assert!(
        b.scalar("SELECT NULL::bigint").is_err(),
        "a NULL read as 0 is a verification that silently stops verifying"
    );
}

// ─── The guard ──────────────────────────────────────────────────────────────

/// Drive a deterministic churn ledger and reconcile it, then break it twice.
///
/// Deterministic on purpose: no threads, no clock, no plan. The point is not
/// that churn works under load — the harness's own run shows that — but that
/// the reconciliation reaches the right verdict through THIS backend, and that
/// its right verdict includes `Mismatch`.
#[test]
fn the_churn_ledger_balances_and_then_fails_when_a_row_is_taken_underneath_it() {
    let Some(mut b) = live_or_skip("churn reconciliation") else {
        return;
    };
    let cat = Catalogue::load().expect("catalogue");
    build_schema(&mut b, &cat);
    for stmt in cat
        .fixture("delete-churn", Dialect::Sql, "setup")
        .expect("delete-churn setup")
    {
        let s = render(
            &stmt,
            &[
                ("lo", "0"),
                ("hi", &LEVEL_STRIDE.to_string()),
                ("nonce", &NONCE.to_string()),
            ],
        );
        b.run(&s).unwrap_or_else(|e| panic!("setup ({s}): {e}"));
    }
    b.run(&write_stmt(
        &cat,
        "churn_anchor",
        &p(&[("cid", CID), ("nonce", NONCE)]),
    ))
    .expect("anchor");

    // Twenty creates, four deletes: sixteen survivors, the same shape the
    // profile's CHURN_FLOOR produces, with none of its timing.
    let (mut creates, mut deletes) = (0u64, 0u64);
    for id in 0..20u64 {
        let s = write_stmt(
            &cat,
            "churn_create",
            &p(&[("cid", CID), ("nonce", NONCE), ("id", id)]),
        );
        let n = b
            .run(&s)
            .unwrap_or_else(|e| panic!("churn_create {id}: {e}"));
        assert_eq!(
            n, 1,
            "an acked create that wrote {n} rows is not an acked create"
        );
        creates += 1;
    }
    for id in 0..4u64 {
        let s = write_stmt(
            &cat,
            "churn_delete",
            &p(&[("cid", CID), ("nonce", NONCE), ("id", id)]),
        );
        b.run(&s)
            .unwrap_or_else(|e| panic!("churn_delete {id}: {e}"));
        deletes += 1;
    }

    let total = probe_stmt(&cat, "churn-survivors-total", 0, &[]);
    let worker = probe_stmt(&cat, "churn-survivors-worker", 0, &[("cid", CID)]);
    let survivors = b.run(&total).expect("survivors probe");
    assert_eq!(
        reconcile(creates, deletes, survivors),
        Reconciliation::Balanced(16),
        "the clean ledger must balance before a broken one means anything"
    );
    assert_eq!(b.run(&worker).expect("worker probe"), 16);
    assert_eq!(
        b.run(&probe_stmt(&cat, "churn-duplicates", 0, &[]))
            .expect("duplicate probe"),
        0
    );
    // The rel half: one anchor rel per survivor, and every CHURN edge binds
    // both endpoints.
    assert_eq!(
        b.run(&probe_stmt(&cat, "churn-anchor-rels", 0, &[]))
            .expect("anchor rels"),
        16
    );
    let bare = b
        .run(&probe_stmt(&cat, "churn-rel-bare", 0, &[]))
        .expect("bare");
    let bound = b
        .run(&probe_stmt(&cat, "churn-rel-bound", 0, &[]))
        .expect("bound");
    assert_eq!(
        bare, bound,
        "{bare} CHURN edge(s) but only {bound} bind both endpoints"
    );

    // ── Fault one: one acked row removed behind the ledger's back ──────────
    //
    // The ledger still says sixteen. The corpus holds fifteen. This is a lost
    // write in the only form the reconciliation can see it, and if it reads
    // Balanced here the guard is decoration.
    b.run(&format!(
        "DELETE FROM churn WHERE id = 7 AND nonce = {NONCE}"
    ))
    .expect("take one row");
    let after = b.run(&total).expect("survivors probe");
    assert_eq!(
        reconcile(creates, deletes, after),
        Reconciliation::Mismatch {
            expected: 16,
            measured: 15
        },
        "ONE acked create was removed underneath the probe and the reconciliation still \
         balanced: the check cannot see a lost write"
    );
    assert_eq!(
        reconcile(creates, deletes, b.run(&worker).expect("worker probe")),
        Reconciliation::Mismatch {
            expected: 16,
            measured: 15
        },
        "the per-worker probe agreed with a ledger it should have contradicted"
    );

    // ── Fault two: the table truncated underneath the probe ────────────────
    b.run("TRUNCATE churn").expect("truncate");
    assert_eq!(
        reconcile(creates, deletes, b.run(&total).expect("survivors probe")),
        Reconciliation::Mismatch {
            expected: 16,
            measured: 0
        },
        "every acked create vanished and the reconciliation reported no loss"
    );
    eprintln!(
        "[pgbackend-live] churn guard: balanced at 16, then Mismatch at 15 and at 0 — the \
         guard has been observed to fail"
    );
}

#[test]
fn a_level_scoped_write_lands_only_where_that_levels_probe_looks() {
    // The failure this arithmetic prevents, reproduced in miniature: a write
    // whose id was not moved by the stride lands in the PREVIOUS level's
    // window, so the level's own probe finds nothing and reports total loss —
    // while the plan, the acks and the rate all look perfectly healthy.
    let Some(mut b) = live_or_skip("level scoping") else {
        return;
    };
    let cat = Catalogue::load().expect("catalogue");
    build_schema(&mut b, &cat);
    b.run(&write_stmt(
        &cat,
        "churn_anchor",
        &p(&[("cid", CID), ("nonce", NONCE)]),
    ))
    .expect("anchor");

    let bound = p(&[("cid", CID), ("nonce", NONCE), ("id", 3)]);
    let at_level_0 = scope_params("churn_create", &bound, 0);
    let at_level_1 = scope_params("churn_create", &bound, 1);
    assert_eq!(
        at_level_1.get("id"),
        Some(&Param::Uint(3 + LEVEL_STRIDE)),
        "the stride never reached the binding"
    );
    b.run(&write_stmt(&cat, "churn_create", &at_level_0))
        .expect("level 0 create");
    b.run(&write_stmt(&cat, "churn_create", &at_level_1))
        .expect("level 1 create");

    let seen_at = |b: &mut PgBackend, level: usize| -> u64 {
        b.run(&probe_stmt(&cat, "churn-survivors-total", level, &[]))
            .expect("survivors probe")
    };
    assert_eq!(
        seen_at(&mut b, 0),
        1,
        "level 0's window holds its own write"
    );
    assert_eq!(
        seen_at(&mut b, 1),
        1,
        "level 1's window holds its own write"
    );

    // Now the bug: a third create that FORGOT the stride while claiming to be
    // level 1's. It lands in level 0's window, so level 1's probe still says
    // one — the level acked two writes and can account for one.
    let forgot = scope_params(
        "churn_create",
        &p(&[("cid", CID), ("nonce", NONCE), ("id", 4)]),
        0,
    );
    b.run(&write_stmt(&cat, "churn_create", &forgot))
        .expect("unscoped create");
    assert_eq!(
        seen_at(&mut b, 1),
        1,
        "an unscoped write became visible to level 1's probe, which would make the loss this \
         test demonstrates invisible"
    );
    assert_eq!(
        seen_at(&mut b, 0),
        2,
        "the unscoped write did not land in level 0's window either, so this test is \
         demonstrating nothing"
    );
    // Stated as the reconciliation would state it: two acked creates at level
    // 1, one survivor. That is the shape of the report a dropped stride
    // produces, and it is a FAILURE rather than a plausible rate.
    assert_eq!(
        reconcile(2, 0, seen_at(&mut b, 1)),
        Reconciliation::Mismatch {
            expected: 2,
            measured: 1
        }
    );
}

#[test]
fn a_reconnect_restores_the_session_and_the_cap_it_was_opened_with() {
    // A reconnect that reverted to the server's default parallelism would
    // leave a level's second half running on a different machine than its
    // first, and nothing in the document would say so.
    let Some(mut b) = live_or_skip("reconnect") else {
        return;
    };
    let before = b.version().to_string();
    b.reconnect().expect("reconnect");
    assert_eq!(
        b.version(),
        before,
        "the reconnected session reports different settings than the original"
    );
    assert_eq!(b.scalar("SELECT 42").expect("usable after reconnect"), 42);
    // The temp schema is gone with the old connection — which is the honest
    // consequence of a reconnect and worth asserting so nobody builds a probe
    // that assumes otherwise.
    assert!(
        b.run("SELECT * FROM harness_temp_bootstrap").is_err(),
        "a temporary table survived a reconnect, which would make every isolated test in this \
         file leak into the next"
    );
}
