//! The poisoned-connection guard, driven by a scripted backend on a loopback
//! socket.
//!
//! These are the four behaviours the client exists to get right, and every one
//! of them runs in a plain `cargo test` with no PostgreSQL anywhere. That is
//! deliberate and it is not a convenience:
//!
//! * **Two of them cannot be tested against the real server at all.** The
//!   benchmark pod is `trust` on every `pg_hba.conf` line, so it will never
//!   demand authentication, and it will never emit a message type this client
//!   does not implement. Gating those tests on a live address would mean they
//!   never run anywhere — a suite that is green because it was skipped.
//! * **The other two would be gated behind a database that is not on the dev
//!   box.** `ErrorResponse` recovery and NULL-versus-empty-string are the exact
//!   cases a wire client gets wrong, and a test that only runs when someone
//!   remembers to set an environment variable is a test that runs on the day it
//!   is written and never again.
//!
//! The live suite
//! (`a_live_postgres_answers_the_pgwire_client_or_the_skip_is_loud`) re-checks
//! the two that a real server *can* express, against a real server. This file
//! is what makes its skip survivable.
//!
//! A scripted backend also buys precision the real server cannot: it can send a
//! `ReadyForQuery` and then a garbage message type on demand, so the guard is
//! tested by tripping it rather than by hoping.
//!
//! This test needs real threads — the scripted backend runs on one while the
//! client blocks on the other, which is what a socket conversation is — and a
//! real wall clock for the timeouts that keep a broken script from hanging the
//! suite. The simulation layer's `Runtime` deliberately provides neither, hence
//! the waiver, the same one `stress` and `snbconc` carry and for the same
//! reason.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;
use std::time::Duration;

use engram_bench::pgwire::{PgClient, as_pg_error};

/// Bounds every socket read in this file. A scripted backend that stopped early
/// would otherwise hang the suite forever, and "the tests never finished" is a
/// worse diagnostic than "the client waited five seconds for a message that
/// never came".
const TEST_TIMEOUT: Duration = Duration::from_secs(5);

// ─── building backend messages ──────────────────────────────────────────────

fn msg(msg_type: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![msg_type];
    out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(body);
    out
}

fn cstr(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

fn auth_ok() -> Vec<u8> {
    msg(b'R', &0i32.to_be_bytes())
}

/// `AuthenticationMD5Password` — code 5 plus a four-byte salt.
fn auth_md5() -> Vec<u8> {
    let mut body = 5i32.to_be_bytes().to_vec();
    body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    msg(b'R', &body)
}

/// `AuthenticationSASL` — code 10 plus the mechanism list.
fn auth_sasl() -> Vec<u8> {
    let mut body = 10i32.to_be_bytes().to_vec();
    cstr(&mut body, "SCRAM-SHA-256");
    body.push(0);
    msg(b'R', &body)
}

fn ready(status: u8) -> Vec<u8> {
    msg(b'Z', &[status])
}

fn param_status(name: &str, value: &str) -> Vec<u8> {
    let mut body = Vec::new();
    cstr(&mut body, name);
    cstr(&mut body, value);
    msg(b'S', &body)
}

fn backend_key(pid: i32) -> Vec<u8> {
    let mut body = pid.to_be_bytes().to_vec();
    body.extend_from_slice(&0x1234_5678i32.to_be_bytes());
    msg(b'K', &body)
}

fn row_description(names: &[&str]) -> Vec<u8> {
    let mut body = (names.len() as i16).to_be_bytes().to_vec();
    for n in names {
        cstr(&mut body, n);
        body.extend_from_slice(&0i32.to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&25i32.to_be_bytes());
        body.extend_from_slice(&(-1i16).to_be_bytes());
        body.extend_from_slice(&(-1i32).to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
    }
    msg(b'T', &body)
}

fn data_row(vals: &[Option<&str>]) -> Vec<u8> {
    let mut body = (vals.len() as i16).to_be_bytes().to_vec();
    for v in vals {
        match v {
            None => body.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(s) => {
                body.extend_from_slice(&(s.len() as i32).to_be_bytes());
                body.extend_from_slice(s.as_bytes());
            }
        }
    }
    msg(b'D', &body)
}

fn command_complete(tag: &str) -> Vec<u8> {
    let mut body = Vec::new();
    cstr(&mut body, tag);
    msg(b'C', &body)
}

fn error_response(sqlstate: &str, message: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(b'S');
    cstr(&mut body, "ERROR");
    body.push(b'V');
    cstr(&mut body, "ERROR");
    body.push(b'C');
    cstr(&mut body, sqlstate);
    body.push(b'M');
    cstr(&mut body, message);
    body.push(0);
    msg(b'E', &body)
}

fn notice_response(message: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(b'S');
    cstr(&mut body, "WARNING");
    body.push(b'C');
    cstr(&mut body, "25P01");
    body.push(b'M');
    cstr(&mut body, message);
    body.push(0);
    msg(b'N', &body)
}

/// The startup burst a `trust` server sends: AuthenticationOk, a couple of
/// parameters, the backend key, then idle.
fn handshake() -> Vec<u8> {
    let mut v = auth_ok();
    v.extend_from_slice(&param_status("server_version", "17.11 (scripted)"));
    v.extend_from_slice(&param_status("client_encoding", "UTF8"));
    v.extend_from_slice(&backend_key(4242));
    v.extend_from_slice(&ready(b'I'));
    v
}

fn concat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.iter().flat_map(|p| p.iter().copied()).collect()
}

// ─── the scripted backend ───────────────────────────────────────────────────

/// A one-connection PostgreSQL impersonator.
///
/// It reads the `StartupMessage`, replies with `script[0]`, and thereafter
/// replies with the next script entry each time the client completes a request
/// — where "completes" means a `Query` or a `Sync`, which is precisely when a
/// real server starts answering. Parsing the frontend framing rather than
/// simply reading whatever arrives is what makes it correct for the extended
/// protocol, where five messages go out in one `write_all`.
struct ScriptedBackend {
    addr: String,
    thread: Option<JoinHandle<()>>,
}

impl ScriptedBackend {
    /// Run the script and then hold the socket open, the way a real server
    /// waits for the next statement.
    fn start(script: Vec<Vec<u8>>) -> ScriptedBackend {
        ScriptedBackend::start_with(script, true)
    }

    /// Run the script and then close the socket, the way a server that crashed
    /// or was killed mid-result does.
    fn start_and_hang_up(script: Vec<Vec<u8>>) -> ScriptedBackend {
        ScriptedBackend::start_with(script, false)
    }

    fn start_with(script: Vec<Vec<u8>>, linger: bool) -> ScriptedBackend {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let addr = listener
            .local_addr()
            .expect("read back the bound port")
            .to_string();
        let thread = std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            sock.set_nodelay(true).ok();
            sock.set_read_timeout(Some(TEST_TIMEOUT)).ok();
            sock.set_write_timeout(Some(TEST_TIMEOUT)).ok();
            if read_startup(&mut sock).is_err() {
                return;
            }
            let mut script = script.into_iter();
            if let Some(first) = script.next() {
                if sock.write_all(&first).is_err() {
                    return;
                }
            }
            for reply in script {
                if !wait_for_request(&mut sock) {
                    return;
                }
                if sock.write_all(&reply).is_err() {
                    return;
                }
            }
            if linger {
                // Stay open so the client's own Drop-time Terminate has
                // somewhere to go; without this the client would see a reset
                // instead. Dropping `sock` at the end of the closure is the
                // hang-up the other mode wants.
                let mut sink = [0u8; 256];
                let _ = sock.read(&mut sink);
            }
        });
        ScriptedBackend {
            addr,
            thread: Some(thread),
        }
    }

    fn connect(&self) -> std::io::Result<PgClient> {
        let mut c = PgClient::connect(&self.addr, "bench", "bench")?;
        c.set_timeout(Some(TEST_TIMEOUT))?;
        Ok(c)
    }
}

impl Drop for ScriptedBackend {
    fn drop(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The `StartupMessage` is the one message with no type byte: length first.
fn read_startup(sock: &mut TcpStream) -> std::io::Result<()> {
    let mut len4 = [0u8; 4];
    sock.read_exact(&mut len4)?;
    let len = i32::from_be_bytes(len4);
    assert!(
        (8..=4096).contains(&len),
        "a StartupMessage declared length {len}; a client that framed it with a \
         type byte would land here"
    );
    let mut body = vec![0u8; len as usize - 4];
    sock.read_exact(&mut body)?;
    let version = i32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    assert_eq!(version, 196_608, "protocol 3.0");
    Ok(())
}

/// Consume frontend messages until one completes a request. Returns false on a
/// closed socket or a `Terminate`.
fn wait_for_request(sock: &mut TcpStream) -> bool {
    loop {
        let mut hdr = [0u8; 5];
        if sock.read_exact(&mut hdr).is_err() {
            return false;
        }
        let len = i32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]);
        if len < 4 {
            return false;
        }
        let mut body = vec![0u8; len as usize - 4];
        if !body.is_empty() && sock.read_exact(&mut body).is_err() {
            return false;
        }
        match hdr[0] {
            // A Query or a Sync is where a real server starts answering.
            b'Q' | b'S' => return true,
            b'X' => return false,
            _ => {}
        }
    }
}

// ─── 1. an ErrorResponse leaves the client usable ───────────────────────────

#[test]
fn a_sql_error_leaves_the_connection_usable_for_the_very_next_query() {
    // The guard, stated as bytes: statement one fails, statement two must still
    // get statement two's answer. If the client left the ErrorResponse's
    // trailing ReadyForQuery in the socket, the second query would read it
    // first and return the FIRST query's result — a wrong number that looks
    // entirely plausible, which is the failure this whole module is arranged
    // to prevent.
    let backend = ScriptedBackend::start(vec![
        handshake(),
        concat(&[
            error_response("42P01", "relation \"nope\" does not exist"),
            ready(b'I'),
        ]),
        concat(&[
            row_description(&["n"]),
            data_row(&[Some("1")]),
            command_complete("SELECT 1"),
            ready(b'I'),
        ]),
    ]);
    let mut c = backend
        .connect()
        .expect("the scripted trust handshake succeeds");

    let err = c
        .query("SELECT * FROM nope")
        .expect_err("the server refused this statement");
    let pg = as_pg_error(&err).expect("a server-reported failure keeps its ErrorResponse");
    assert_eq!(
        pg.code, "42P01",
        "the SQLSTATE survives the trip through io::Error"
    );
    assert!(pg.message.contains("does not exist"), "{pg}");

    assert!(
        !c.is_poisoned(),
        "a SQL error is not a framing error; the connection is re-synchronised \
         and must remain usable: {:?}",
        c.poison_reason()
    );

    let ok = c.query("SELECT 1 AS n").expect("the next query works");
    assert_eq!(ok.tag, "SELECT 1", "this is the SECOND query's tag");
    assert_eq!(ok.columns, vec!["n"]);
    assert_eq!(ok.rows, vec![vec![Some("1".to_string())]]);
}

#[test]
fn an_extended_query_error_also_leaves_the_connection_usable() {
    // The extended path has its own way to poison: after an error the server
    // discards input until Sync, so a client that returned early would leave
    // both the ErrorResponse's tail and the ReadyForQuery unread.
    let backend = ScriptedBackend::start(vec![
        handshake(),
        concat(&[
            error_response("42883", "operator does not exist"),
            ready(b'I'),
        ]),
        concat(&[
            msg(b'1', &[]),
            msg(b'2', &[]),
            row_description(&["v"]),
            data_row(&[Some("ok")]),
            command_complete("SELECT 1"),
            ready(b'I'),
        ]),
    ]);
    let mut c = backend.connect().unwrap();

    let err = c
        .query_params("SELECT $1 + 1", &[Some("x")])
        .expect_err("the server refused this bind");
    assert_eq!(as_pg_error(&err).map(|e| e.code.as_str()), Some("42883"));
    assert!(
        !c.is_poisoned(),
        "Sync guarantees a ReadyForQuery to drain to"
    );

    let ok = c
        .query_params("SELECT $1::text AS v", &[Some("ok")])
        .unwrap();
    assert_eq!(ok.rows, vec![vec![Some("ok".to_string())]]);
}

#[test]
fn a_broken_frame_poisons_the_connection_and_every_later_call_says_so() {
    // The other half of the guard. A message type this client does not
    // implement means it cannot know how many bytes belong to it, so there is
    // no re-synchronisation point and continuing would return fiction.
    let backend = ScriptedBackend::start(vec![
        handshake(),
        concat(&[msg(b'\x7f', b"who knows"), ready(b'I')]),
    ]);
    let mut c = backend.connect().unwrap();

    let err = c
        .query("SELECT 1")
        .expect_err("an unknown message type is fatal");
    assert!(
        err.to_string().contains("poisoned"),
        "the first error already says the connection is gone: {err}"
    );
    assert!(
        as_pg_error(&err).is_none(),
        "this is a framing failure, not a server-reported one — as_pg_error is \
         how a caller tells 'retry the statement' from 'reconnect'"
    );
    assert!(c.is_poisoned());
    assert!(
        c.poison_reason()
            .unwrap_or_default()
            .contains("unexpected message type"),
        "the reason names the cause: {:?}",
        c.poison_reason()
    );

    // And it stays refused, immediately, rather than reading more garbage.
    let again = c.query("SELECT 1").expect_err("a poisoned client is done");
    assert!(again.to_string().contains("poisoned"), "{again}");
    let params = c
        .query_params("SELECT 1", &[])
        .expect_err("every entry point checks");
    assert!(params.to_string().contains("poisoned"), "{params}");
}

#[test]
fn a_server_that_hangs_up_mid_result_poisons_rather_than_returning_a_short_answer() {
    // Two rows announced, one delivered, then EOF. Returning the one row would
    // be a silently short count.
    let backend = ScriptedBackend::start_and_hang_up(vec![
        handshake(),
        concat(&[row_description(&["n"]), data_row(&[Some("1")])]),
    ]);
    let mut c = backend.connect().unwrap();
    let err = c
        .query("SELECT n FROM t")
        .expect_err("the stream ended early");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::UnexpectedEof,
        "the error names the truncation rather than blaming the statement: {err}"
    );
    assert!(
        c.is_poisoned(),
        "there is no recovery from a truncated stream"
    );
}

// ─── 2. NULL is distinguished from the empty string ─────────────────────────

#[test]
fn a_returned_null_is_not_the_empty_string() {
    let backend = ScriptedBackend::start(vec![
        handshake(),
        concat(&[
            row_description(&["nul", "empty", "text"]),
            data_row(&[None, Some(""), Some("x")]),
            command_complete("SELECT 1"),
            ready(b'I'),
        ]),
    ]);
    let mut c = backend.connect().unwrap();
    let r = c.query("SELECT NULL, '', 'x'").unwrap();

    assert_eq!(r.columns, vec!["nul", "empty", "text"]);
    assert_eq!(r.rows.len(), 1);
    assert_eq!(
        r.rows[0],
        vec![None, Some(String::new()), Some("x".to_string())]
    );
    assert!(r.rows[0][0].is_none(), "SQL NULL is None");
    assert_eq!(
        r.rows[0][1].as_deref(),
        Some(""),
        "the empty string is Some(\"\") and must not become None"
    );
    assert_ne!(
        r.rows[0][0], r.rows[0][1],
        "these are different values and stay different all the way out"
    );
}

// ─── 3. a multi-statement simple query behaves as documented ────────────────

#[test]
fn a_multi_statement_simple_query_answers_with_its_last_result_set() {
    // Documented behaviour, asserted rather than assumed: `query` returns the
    // LAST result set, so the `SET …; SELECT …` prologue pattern returns the
    // SELECT rather than silently discarding it.
    let backend = ScriptedBackend::start(vec![
        handshake(),
        concat(&[
            command_complete("SET"),
            row_description(&["two"]),
            data_row(&[Some("2")]),
            command_complete("SELECT 1"),
            ready(b'I'),
        ]),
    ]);
    let mut c = backend.connect().unwrap();
    let r = c.query("SET work_mem='256MB'; SELECT 2 AS two").unwrap();

    assert_eq!(
        r.tag, "SELECT 1",
        "the LAST result set — taking the first would return the SET's empty \
         answer and throw away the measurement"
    );
    assert_eq!(r.columns, vec!["two"]);
    assert_eq!(r.rows, vec![vec![Some("2".to_string())]]);
}

#[test]
fn a_multi_statement_simple_query_can_return_every_result_set_instead() {
    // Nothing is lost, only defaulted: query_multi is the primitive and returns
    // all of them, which is what makes `query`'s choice safe to document rather
    // than something a caller has to work around.
    let backend = ScriptedBackend::start(vec![
        handshake(),
        concat(&[
            row_description(&["one"]),
            data_row(&[Some("1")]),
            command_complete("SELECT 1"),
            row_description(&["two"]),
            data_row(&[Some("2")]),
            data_row(&[Some("3")]),
            command_complete("SELECT 2"),
            ready(b'I'),
        ]),
    ]);
    let mut c = backend.connect().unwrap();
    let all = c.query_multi("SELECT 1 AS one; SELECT * FROM two").unwrap();

    assert_eq!(all.len(), 2, "one result set per statement, in order");
    assert_eq!(all[0].columns, vec!["one"]);
    assert_eq!(all[0].tag, "SELECT 1");
    assert_eq!(all[0].rows.len(), 1);
    assert_eq!(all[1].columns, vec!["two"]);
    assert_eq!(all[1].tag, "SELECT 2");
    assert_eq!(all[1].rows.len(), 2);
    assert_eq!(
        all[1].affected_rows(),
        2,
        "the count comes from the tag, not from rows.len()"
    );
}

#[test]
fn a_failure_anywhere_in_a_batch_fails_the_whole_call() {
    // The server abandons the rest of the batch after an error, so there is no
    // partial success — and crucially no later statement's result that could be
    // mistaken for the failed one's.
    let backend = ScriptedBackend::start(vec![
        handshake(),
        concat(&[
            row_description(&["one"]),
            data_row(&[Some("1")]),
            command_complete("SELECT 1"),
            error_response("23505", "duplicate key value violates unique constraint"),
            ready(b'I'),
        ]),
    ]);
    let mut c = backend.connect().unwrap();
    let err = c
        .query_multi("SELECT 1 AS one; INSERT INTO t VALUES (1)")
        .expect_err("statement two failed, so the call failed");
    assert_eq!(as_pg_error(&err).map(|e| e.code.as_str()), Some("23505"));
    assert!(
        !c.is_poisoned(),
        "the ReadyForQuery after the error was still drained"
    );
}

#[test]
fn an_update_reports_the_rows_it_changed_and_not_the_rows_it_returned() {
    let backend = ScriptedBackend::start(vec![
        handshake(),
        concat(&[command_complete("UPDATE 17"), ready(b'I')]),
    ]);
    let mut c = backend.connect().unwrap();
    let n = c.execute("UPDATE t SET x = 1").unwrap();
    assert_eq!(
        n, 17,
        "execute reads the tag; a harness measuring write throughput from \
         rows.len() would measure zero"
    );
}

// ─── 4. a server that demands authentication is refused, loudly ─────────────

#[test]
fn a_server_that_demands_md5_authentication_is_refused_by_name() {
    // Not a hang and not a panic. A client that ignored the request would wait
    // for a ReadyForQuery the server will never send, and a hung benchmark
    // client is indistinguishable from a database that has stopped answering.
    let backend = ScriptedBackend::start(vec![auth_md5()]);
    let err = PgClient::connect(&backend.addr, "bench", "bench")
        .expect_err("this client implements no authentication");
    let text = err.to_string();
    assert!(
        text.contains("MD5 password"),
        "the refusal names the method that was demanded: {text}"
    );
    assert!(
        text.contains("trust"),
        "and points at pg_hba.conf, which is the thing that must have changed: \
         {text}"
    );
}

#[test]
fn a_server_that_demands_scram_authentication_is_refused_by_name() {
    let backend = ScriptedBackend::start(vec![auth_sasl()]);
    let err =
        PgClient::connect(&backend.addr, "bench", "bench").expect_err("SCRAM is not implemented");
    assert!(
        err.to_string().contains("SASL/SCRAM"),
        "the refusal names SCRAM specifically, so the cost of implementing it \
         is a known quantity rather than a mystery: {err}"
    );
}

#[test]
fn a_server_that_refuses_the_connection_outright_reports_its_sqlstate() {
    // FATAL during startup: the server closes without a ReadyForQuery, so there
    // is nothing to drain and the error is all there is.
    let backend = ScriptedBackend::start(vec![error_response(
        "3D000",
        "database \"nope\" does not exist",
    )]);
    let err = PgClient::connect(&backend.addr, "bench", "nope")
        .expect_err("the server refused the startup");
    let pg = as_pg_error(&err).expect("a startup refusal is still a server error");
    assert_eq!(pg.code, "3D000");
}

// ─── session bookkeeping ────────────────────────────────────────────────────

#[test]
fn the_handshake_records_what_the_server_said_about_itself() {
    let backend = ScriptedBackend::start(vec![handshake()]);
    let c = backend.connect().unwrap();
    assert_eq!(
        c.server_version(),
        "17.11 (scripted)",
        "a comparison run has to be able to state what it measured against"
    );
    assert_eq!(c.backend_pid(), 4242);
    assert_eq!(c.transaction_status(), b'I');
    assert!(!c.in_transaction());
}

#[test]
fn transaction_status_follows_the_servers_ready_for_query_byte() {
    let backend = ScriptedBackend::start(vec![
        handshake(),
        concat(&[command_complete("BEGIN"), ready(b'T')]),
        concat(&[
            error_response("42601", "syntax error"),
            ready(b'E'), // a failed transaction: only ROLLBACK leaves it
        ]),
        concat(&[command_complete("ROLLBACK"), ready(b'I')]),
    ]);
    let mut c = backend.connect().unwrap();

    c.begin().expect("BEGIN reaches transaction status T");
    assert_eq!(c.transaction_status(), b'T');
    assert!(c.in_transaction());

    let _ = c.query("oops").expect_err("the statement failed");
    assert_eq!(
        c.transaction_status(),
        b'E',
        "a failed transaction is still a transaction"
    );
    assert!(c.in_transaction(), "so COMMIT/ROLLBACK are still permitted");

    c.rollback().expect("ROLLBACK is the way out of E");
    assert_eq!(c.transaction_status(), b'I');
}

#[test]
fn a_nested_begin_is_refused_instead_of_being_warned_away_by_the_server() {
    // Postgres answers a nested BEGIN with a warning and changes nothing, so a
    // harness that lost track of its nesting would keep running and measure
    // autocommit while believing it measured transactions.
    let backend = ScriptedBackend::start(vec![
        handshake(),
        concat(&[command_complete("BEGIN"), ready(b'T')]),
    ]);
    let mut c = backend.connect().unwrap();
    c.begin().unwrap();
    let err = c.begin().expect_err("nesting is a bookkeeping error");
    assert!(
        err.to_string().contains("already in a transaction"),
        "{err}"
    );
}

#[test]
fn commit_and_rollback_outside_a_transaction_are_refused() {
    let backend = ScriptedBackend::start(vec![handshake()]);
    let mut c = backend.connect().unwrap();
    assert!(c.commit().is_err(), "COMMIT with nothing open");
    assert!(c.rollback().is_err(), "ROLLBACK with nothing open");
    assert!(!c.is_poisoned(), "refusing locally touches no bytes");
}

#[test]
fn a_notice_is_kept_rather_than_dropped_on_the_floor() {
    let backend = ScriptedBackend::start(vec![
        handshake(),
        concat(&[
            notice_response("there is already a transaction in progress"),
            command_complete("BEGIN"),
            ready(b'T'),
        ]),
    ]);
    let mut c = backend.connect().unwrap();
    c.query("BEGIN").unwrap();
    assert_eq!(c.notices().len(), 1);
    assert!(
        c.notices()[0].message.contains("already a transaction"),
        "a dropped WARNING is real signal lost: {:?}",
        c.notices()[0]
    );
    assert_eq!(c.notices()[0].severity, "WARNING");
}

#[test]
fn an_empty_statement_answers_with_an_empty_result_and_not_an_error() {
    let backend = ScriptedBackend::start(vec![handshake(), concat(&[msg(b'I', &[]), ready(b'I')])]);
    let mut c = backend.connect().unwrap();
    let r = c
        .query("")
        .expect("EmptyQueryResponse is an answer, not a fault");
    assert!(r.columns.is_empty());
    assert!(r.rows.is_empty());
    assert_eq!(r.tag, "", "the empty statement has no CommandComplete tag");
}

#[test]
fn a_copy_out_is_reported_rather_than_returned_as_an_empty_result() {
    // The rows exist but this client has no representation for them. Answering
    // with zero rows would be a wrong count; the error names the remedy.
    let mut copy_out = 0u8.to_be_bytes().to_vec(); // overall format: text
    copy_out.extend_from_slice(&1i16.to_be_bytes()); // one column
    copy_out.extend_from_slice(&0i16.to_be_bytes()); // text
    let backend = ScriptedBackend::start(vec![
        handshake(),
        concat(&[
            msg(b'H', &copy_out),
            msg(b'd', b"1\n"),
            msg(b'c', &[]),
            command_complete("COPY 1"),
            ready(b'I'),
        ]),
    ]);
    let mut c = backend.connect().unwrap();
    let err = c
        .query("COPY t TO STDOUT")
        .expect_err("COPY TO STDOUT is not decoded");
    assert!(
        err.to_string().contains("COPY TO STDOUT"),
        "the error names what happened and what to do instead: {err}"
    );
    assert!(
        !c.is_poisoned(),
        "the CopyData/CopyDone were drained, so the connection survives"
    );
}
