//! The same client, against a real PostgreSQL.
//!
//! ```sh
//! # from a workstation, with a port-forward to the benchmark pod:
//! kubectl port-forward pod/<your-postgres-pod> 15432:5432
//! ENGRAM_PGWIRE_TEST_ADDR=127.0.0.1:15432 \
//!   cargo test -p engram-bench \
//!   --test a_live_postgres_answers_the_pgwire_client_or_the_skip_is_loud -- --nocapture
//! ```
//!
//! `ENGRAM_PGWIRE_TEST_USER` (default `postgres`) and `ENGRAM_PGWIRE_TEST_DB`
//! (default `postgres`) override the rest.
//!
//! # Why the skip is arranged the way it is
//!
//! A test that passes because it was skipped is worse than no test: it is a
//! green tick that certifies nothing, and it stays green through exactly the
//! change it was written to catch. Two things are done about that here, because
//! printing a message is not enough on its own — `cargo test` hides the output
//! of tests that pass.
//!
//! 1. **The skip is not the only coverage.** Every behaviour asserted below is
//!    also asserted against a scripted backend in
//!    `a_pgwire_error_leaves_the_connection_usable_but_a_broken_frame_does_not`,
//!    which needs no server and therefore always runs. What this file adds is
//!    the confirmation that a *real* PostgreSQL 17 agrees with the scripted
//!    one — valuable, and not the only thing standing between the codec and a
//!    regression.
//! 2. **The skip can be made fatal.** Set `ENGRAM_PGWIRE_TEST_REQUIRE_LIVE=1`
//!    and a missing address fails the test instead of skipping it. Anywhere the
//!    live run is *intended* — CI, a verification pass before a measurement —
//!    that variable turns "nobody set the address" from a silent pass into a
//!    failure. Without it there is no way to tell a suite that ran from a suite
//!    that evaporated.
//!
//! # What cannot be tested here, and is tested elsewhere
//!
//! The benchmark pod's `pg_hba.conf` is `trust` on every line, so this server
//! will never demand authentication and this file can never exercise the
//! refusal. That test lives with the scripted backend, which can demand MD5 and
//! SCRAM on command. Gating it on a live address would have meant it never ran
//! anywhere.
//!
//! # This suite is not a measurement
//!
//! Every statement below is trivial and every table is `TEMP`, so nothing
//! survives the connection and nothing competes for the pod's `bench_lock`.
//! Timing is deliberately not recorded: another workstream may be measuring,
//! and a number taken from a contended pod is worse than no number.

use engram_bench::pgwire::{PgClient, as_pg_error};

/// Open a connection, or explain — loudly — why the test is not running.
fn live_or_skip(what: &str) -> Option<PgClient> {
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
            "[pgwire-live] {what} could NOT run: ENGRAM_PGWIRE_TEST_REQUIRE_LIVE is set, which \
             says a live run was intended, but ENGRAM_PGWIRE_TEST_ADDR is unset. Failing rather \
             than skipping, because a skipped test is indistinguishable from a passing one."
        );
        eprintln!(
            "[pgwire-live] {what} SKIPPED — ENGRAM_PGWIRE_TEST_ADDR unset. The same behaviour is \
             covered against a scripted backend in \
             a_pgwire_error_leaves_the_connection_usable_but_a_broken_frame_does_not, which always \
             runs. To require the live run, set ENGRAM_PGWIRE_TEST_REQUIRE_LIVE=1."
        );
        return None;
    };
    let user = std::env::var("ENGRAM_PGWIRE_TEST_USER").unwrap_or_else(|_| "postgres".to_string());
    let db = std::env::var("ENGRAM_PGWIRE_TEST_DB").unwrap_or_else(|_| "postgres".to_string());
    let c = PgClient::connect(&addr, &user, &db).unwrap_or_else(|e| {
        panic!(
            "[pgwire-live] {what}: could not connect to {addr} as {user}/{db}: {e}. The address \
             was set, so this is a real failure and not a skip."
        )
    });
    Some(c)
}

#[test]
fn a_trust_server_completes_the_handshake_and_names_its_version() {
    let Some(c) = live_or_skip("handshake") else {
        return;
    };
    assert!(
        !c.server_version().is_empty(),
        "the server announced no server_version in ParameterStatus"
    );
    assert!(
        c.backend_pid() > 0,
        "BackendKeyData should carry a real pid, got {}",
        c.backend_pid()
    );
    assert_eq!(c.transaction_status(), b'I', "a fresh connection is idle");
    eprintln!(
        "[pgwire-live] connected: server_version={} backend_pid={}",
        c.server_version(),
        c.backend_pid()
    );
}

#[test]
fn a_simple_select_returns_its_column_its_row_and_its_tag() {
    let Some(mut c) = live_or_skip("SELECT 1") else {
        return;
    };
    let r = c.query("SELECT 1 AS n").expect("SELECT 1 must work");
    assert_eq!(r.columns, vec!["n"]);
    assert_eq!(r.rows, vec![vec![Some("1".to_string())]]);
    assert_eq!(r.tag, "SELECT 1");
    assert_eq!(r.affected_rows(), 1);
}

#[test]
fn a_real_error_response_leaves_the_connection_usable_for_the_next_query() {
    let Some(mut c) = live_or_skip("error recovery") else {
        return;
    };
    let err = c
        .query("SELECT * FROM a_table_that_does_not_exist_pgwire_test")
        .expect_err("the table does not exist");
    let pg = as_pg_error(&err).expect("a server error keeps its ErrorResponse");
    assert_eq!(
        pg.code, "42P01",
        "undefined_table, straight from the wire: {pg}"
    );
    assert!(!c.is_poisoned(), "reason: {:?}", c.poison_reason());

    // The guard: this must be the SECOND query's answer, not the first's.
    let ok = c.query("SELECT 'after' AS phase").expect("still usable");
    assert_eq!(ok.rows, vec![vec![Some("after".to_string())]]);

    // And again, so a one-off is ruled out.
    let err2 = c.query("SELECT 1/0").expect_err("division by zero");
    assert_eq!(
        as_pg_error(&err2).map(|e| e.code.as_str()),
        Some("22012"),
        "division_by_zero"
    );
    assert_eq!(
        c.query("SELECT 2 AS n").unwrap().rows,
        vec![vec![Some("2".to_string())]]
    );
}

#[test]
fn a_real_null_is_not_the_empty_string() {
    let Some(mut c) = live_or_skip("NULL vs empty string") else {
        return;
    };
    let r = c
        .query("SELECT NULL::text AS a, ''::text AS b, ' '::text AS c")
        .expect("the SELECT must work");
    assert_eq!(r.columns, vec!["a", "b", "c"]);
    assert_eq!(
        r.rows[0],
        vec![None, Some(String::new()), Some(" ".to_string())],
        "PostgreSQL sends -1 for NULL and 0 for the empty string; the client \
         must keep them apart all the way out"
    );
    assert!(r.rows[0][0].is_none());
    assert_eq!(r.rows[0][1].as_deref(), Some(""));
    assert_ne!(r.rows[0][0], r.rows[0][1]);
}

#[test]
fn a_multi_statement_simple_query_answers_with_its_last_result_set() {
    let Some(mut c) = live_or_skip("multi-statement") else {
        return;
    };
    // The documented behaviour, against a server that really does send two
    // result sets and one ReadyForQuery for one Query message.
    let last = c
        .query("SELECT 1 AS one; SELECT 2 AS two")
        .expect("both statements run");
    assert_eq!(last.columns, vec!["two"], "the LAST result set");
    assert_eq!(last.rows, vec![vec![Some("2".to_string())]]);

    let all = c
        .query_multi("SELECT 1 AS one; SELECT 2 AS two; SELECT 3 AS three")
        .expect("three statements");
    assert_eq!(all.len(), 3, "one result set per statement");
    assert_eq!(all[0].columns, vec!["one"]);
    assert_eq!(all[1].columns, vec!["two"]);
    assert_eq!(all[2].columns, vec!["three"]);

    // The prologue pattern this behaviour exists for: a SET in front of the
    // statement being measured must not swallow the measurement.
    let measured = c
        .query("SET work_mem='64MB'; SELECT 42 AS answer")
        .expect("a SET followed by a SELECT");
    assert_eq!(measured.columns, vec!["answer"]);
    assert_eq!(measured.rows, vec![vec![Some("42".to_string())]]);

    // A failure in the middle fails the whole call, and the server abandons
    // what follows.
    let err = c
        .query_multi("SELECT 1; SELECT * FROM still_not_a_table_pgwire; SELECT 3")
        .expect_err("statement two fails");
    assert_eq!(as_pg_error(&err).map(|e| e.code.as_str()), Some("42P01"));
    assert!(!c.is_poisoned());
    assert_eq!(
        c.query("SELECT 9 AS n").unwrap().rows,
        vec![vec![Some("9".to_string())]],
        "and the connection survives it"
    );
}

#[test]
fn an_extended_query_binds_text_parameters_and_a_none_is_sql_null() {
    let Some(mut c) = live_or_skip("extended query") else {
        return;
    };
    let r = c
        .query_params(
            "SELECT $1::text AS given, $2::text AS missing, $3::text AS empty",
            &[Some("hello"), None, Some("")],
        )
        .expect("Parse/Bind/Describe/Execute/Sync");
    assert_eq!(r.columns, vec!["given", "missing", "empty"]);
    assert_eq!(
        r.rows[0],
        vec![Some("hello".to_string()), None, Some(String::new())],
        "None binds as SQL NULL and comes back as NULL; Some(\"\") stays empty"
    );

    // Numbers arrive as text, which is the documented contract.
    let n = c
        .query_params(
            "SELECT ($1::int + $2::int) AS sum",
            &[Some("40"), Some("2")],
        )
        .unwrap();
    assert_eq!(n.rows, vec![vec![Some("42".to_string())]]);

    // An UNCONSTRAINED parameter is not an error: PostgreSQL 17 resolves it to
    // `text`. Asserted rather than assumed, because the module documents the
    // zero-declared-types choice and a wrong claim there would send someone
    // hunting for a bug that is really the server being helpful.
    let bare = c
        .query_params("SELECT $1", &[Some("x")])
        .expect("an unconstrained parameter resolves to text rather than failing");
    assert_eq!(bare.rows, vec![vec![Some("x".to_string())]]);

    // Errors on the extended path come in two flavours and BOTH must leave the
    // connection usable, because both are followed by a Sync the client has to
    // drain to. First: a failure at Parse, before any parameter is bound.
    let at_parse = c
        .query_params("SELECT * FROM nope_pgwire_extended", &[])
        .expect_err("the relation does not exist");
    assert_eq!(
        as_pg_error(&at_parse).map(|e| e.code.as_str()),
        Some("42P01"),
        "a Parse-time failure is a normal server error: {at_parse}"
    );
    assert!(!c.is_poisoned(), "reason: {:?}", c.poison_reason());

    // Second: a failure at Execute, after Parse and Bind both succeeded — the
    // case where the client has already consumed ParseComplete and BindComplete
    // and could most easily lose its place in the stream.
    let at_execute = c
        .query_params("SELECT $1::int AS n", &[Some("not-a-number")])
        .expect_err("'not-a-number' is not an int");
    assert_eq!(
        as_pg_error(&at_execute).map(|e| e.code.as_str()),
        Some("22P02"),
        "invalid_text_representation: {at_execute}"
    );
    assert!(!c.is_poisoned(), "reason: {:?}", c.poison_reason());

    assert_eq!(
        c.query_params("SELECT $1::text AS v", &[Some("still here")])
            .unwrap()
            .rows,
        vec![vec![Some("still here".to_string())]],
        "the connection survived both"
    );
}

#[test]
fn execute_reports_the_rows_a_write_touched() {
    let Some(mut c) = live_or_skip("execute row counts") else {
        return;
    };
    // TEMP: dropped when this connection closes, so the suite leaves nothing
    // behind in a database another workstream may be measuring.
    assert_eq!(
        c.execute("CREATE TEMP TABLE pgwire_probe (id int, note text)")
            .unwrap(),
        0,
        "CREATE TABLE carries no row count"
    );
    assert_eq!(
        c.execute("INSERT INTO pgwire_probe VALUES (1,'a'),(2,'b'),(3,NULL)")
            .unwrap(),
        3,
        "INSERT's tag is `INSERT <oid> <rows>` and the count is the LAST token"
    );
    assert_eq!(
        c.execute("UPDATE pgwire_probe SET note='z' WHERE id <= 2")
            .unwrap(),
        2,
        "UPDATE reports what it changed, though it returns no rows"
    );
    assert_eq!(
        c.execute("DELETE FROM pgwire_probe WHERE id = 3").unwrap(),
        1
    );

    let r = c
        .query("SELECT id, note FROM pgwire_probe ORDER BY id")
        .unwrap();
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.tag, "SELECT 2");
}

#[test]
fn a_transaction_opens_commits_and_rolls_back_and_the_status_byte_follows() {
    let Some(mut c) = live_or_skip("transactions") else {
        return;
    };
    c.execute("CREATE TEMP TABLE pgwire_tx (id int)").unwrap();

    c.begin().expect("BEGIN");
    assert_eq!(c.transaction_status(), b'T');
    c.execute("INSERT INTO pgwire_tx VALUES (1)").unwrap();
    c.commit().expect("COMMIT");
    assert_eq!(c.transaction_status(), b'I');
    assert_eq!(
        c.query("SELECT count(*) FROM pgwire_tx").unwrap().rows,
        vec![vec![Some("1".to_string())]],
        "the committed row is there"
    );

    c.begin().expect("BEGIN again");
    c.execute("INSERT INTO pgwire_tx VALUES (2)").unwrap();
    c.rollback().expect("ROLLBACK");
    assert_eq!(c.transaction_status(), b'I');
    assert_eq!(
        c.query("SELECT count(*) FROM pgwire_tx").unwrap().rows,
        vec![vec![Some("1".to_string())]],
        "the rolled-back row is not"
    );

    // A failed statement inside a transaction leaves status E, where every
    // further statement is refused until ROLLBACK — the recovery path a harness
    // must get right or every subsequent measurement is an error.
    c.begin().unwrap();
    let err = c
        .query("SELECT * FROM nope_pgwire_tx")
        .expect_err("no table");
    assert_eq!(as_pg_error(&err).map(|e| e.code.as_str()), Some("42P01"));
    assert_eq!(
        c.transaction_status(),
        b'E',
        "a failed transaction, not an idle connection"
    );
    let blocked = c
        .query("SELECT 1")
        .expect_err("Postgres refuses everything in an aborted transaction");
    assert_eq!(
        as_pg_error(&blocked).map(|e| e.code.as_str()),
        Some("25P02"),
        "in_failed_sql_transaction"
    );
    c.rollback().expect("ROLLBACK is the way out");
    assert_eq!(c.transaction_status(), b'I');
    assert_eq!(
        c.query("SELECT 1 AS n").unwrap().rows,
        vec![vec![Some("1".to_string())]],
        "and the connection is fully usable again"
    );
}

#[test]
fn a_wide_result_survives_the_read_buffer_boundary() {
    let Some(mut c) = live_or_skip("large result") else {
        return;
    };
    // The read buffer starts at 16 KiB, so a value larger than that exercises
    // the compact-and-grow path where a message spans several socket reads. A
    // bug there truncates a value or desynchronises framing, and both would
    // show up in a benchmark as a wrong answer rather than as an error.
    let r = c
        .query("SELECT repeat('x', 100000) AS big, length(repeat('x', 100000)) AS n")
        .expect("a 100 KB value");
    assert_eq!(r.rows[0][0].as_deref().map(str::len), Some(100_000));
    assert_eq!(r.rows[0][1].as_deref(), Some("100000"));

    // Many rows, so the buffer is refilled repeatedly rather than grown once.
    let many = c
        .query("SELECT i, repeat('y', 200) AS pad FROM generate_series(1, 5000) AS i")
        .expect("5000 rows");
    assert_eq!(many.rows.len(), 5000);
    assert_eq!(many.tag, "SELECT 5000");
    assert_eq!(many.affected_rows(), 5000);
    assert_eq!(many.rows[4999][0].as_deref(), Some("5000"));
}
