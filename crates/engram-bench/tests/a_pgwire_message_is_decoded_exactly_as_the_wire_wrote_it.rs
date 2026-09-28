//! The PostgreSQL v3 codec, tested against byte slices and nothing else.
//!
//! Every test in this file runs in a plain `cargo test`. That is the whole
//! point of the file: the interesting cases in a wire client — a NULL that is
//! not an empty string, an `ErrorResponse` field list, a `CommandComplete` tag
//! with an OID wedged into the middle of it — are decisions about bytes, and a
//! decision about bytes does not need a database to check. The companion suites
//! (`a_pgwire_error_leaves_the_connection_usable_but_a_broken_frame_does_not`
//! and `a_live_postgres_answers_the_pgwire_client_or_the_skip_is_loud`) cover
//! the connection's behaviour; this one covers whether the bytes were read
//! right.
//!
//! The failure mode being defended against is the one where a client is
//! "tested" by a suite that only runs when a server happens to be reachable, so
//! the green tick on a developer laptop means the tests were skipped rather
//! than that the codec is correct.

use engram_bench::pgwire::{
    PgError, auth_method_name, encode_extended_query, encode_simple_query, encode_startup, frame,
    parse_authentication, parse_command_complete, parse_data_row, parse_error_response,
    parse_row_description, row_count_from_tag,
};

// ─── helpers that build backend payloads (the bytes AFTER type + length) ────

fn cstr(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

/// A `DataRow` payload: `i16` column count, then per column an `i32` length
/// (`-1` for NULL) and that many bytes.
fn data_row_payload(vals: &[Option<&str>]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&(vals.len() as i16).to_be_bytes());
    for v in vals {
        match v {
            None => p.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(s) => {
                p.extend_from_slice(&(s.len() as i32).to_be_bytes());
                p.extend_from_slice(s.as_bytes());
            }
        }
    }
    p
}

/// A `RowDescription` payload: `i16` field count, then per field a name and
/// the eighteen bytes of type metadata this client reads past.
fn row_description_payload(names: &[&str]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&(names.len() as i16).to_be_bytes());
    for n in names {
        cstr(&mut p, n);
        p.extend_from_slice(&0i32.to_be_bytes()); // table OID
        p.extend_from_slice(&0i16.to_be_bytes()); // column attribute number
        p.extend_from_slice(&25i32.to_be_bytes()); // type OID (text)
        p.extend_from_slice(&(-1i16).to_be_bytes()); // type length
        p.extend_from_slice(&(-1i32).to_be_bytes()); // type modifier
        p.extend_from_slice(&0i16.to_be_bytes()); // format code (text)
    }
    p
}

/// An `ErrorResponse` / `NoticeResponse` payload: `(field code, value)` pairs
/// terminated by a zero byte.
fn error_payload(fields: &[(u8, &str)]) -> Vec<u8> {
    let mut p = Vec::new();
    for (code, value) in fields {
        p.push(*code);
        cstr(&mut p, value);
    }
    p.push(0);
    p
}

// ─── NULL is not the empty string ───────────────────────────────────────────

#[test]
fn a_null_column_and_an_empty_string_column_decode_differently() {
    let payload = data_row_payload(&[None, Some(""), Some("x")]);
    let row = parse_data_row(&payload).expect("a well-formed DataRow must parse");

    assert_eq!(
        row,
        vec![None, Some(String::new()), Some("x".to_string())],
        "NULL is length -1 and the empty string is length 0; they are different \
         bytes on the wire and must stay different values"
    );
    assert!(row[0].is_none(), "column 0 was NULL");
    assert_eq!(
        row[1].as_deref(),
        Some(""),
        "column 1 was the empty string, NOT NULL — collapsing these makes a \
         LEFT JOIN miss indistinguishable from a genuinely empty value"
    );
    assert_ne!(row[0], row[1], "the two must never compare equal");
}

#[test]
fn a_data_row_with_no_columns_is_a_row_not_an_absence() {
    let payload = data_row_payload(&[]);
    let row = parse_data_row(&payload).expect("a zero-column DataRow is legal");
    assert!(row.is_empty(), "no columns");
}

#[test]
fn a_data_row_that_ends_early_is_an_error_and_not_a_panic() {
    // Declares two columns; supplies one and then stops mid-length.
    let mut payload = data_row_payload(&[Some("a"), Some("b")]);
    payload.truncate(payload.len() - 3);
    let err = parse_data_row(&payload).expect_err("a truncated DataRow must be refused");
    assert!(
        err.to_string().contains("pgwire"),
        "the error names this module: {err}"
    );
}

#[test]
fn a_data_row_with_bytes_left_over_is_refused() {
    // Trailing bytes mean the length field and the payload disagree, which is
    // the first observable symptom of framing drift. Accepting the row would
    // hide it until a later message parsed as garbage.
    let mut payload = data_row_payload(&[Some("a")]);
    payload.push(0xFF);
    let err = parse_data_row(&payload).expect_err("trailing bytes must be refused");
    assert!(
        err.to_string().contains("left over"),
        "the error says what was wrong: {err}"
    );
}

#[test]
fn a_column_length_that_is_neither_a_size_nor_the_null_sentinel_is_refused() {
    let mut payload = Vec::new();
    payload.extend_from_slice(&1i16.to_be_bytes());
    payload.extend_from_slice(&(-7i32).to_be_bytes());
    let err = parse_data_row(&payload).expect_err("-7 is not -1 and not a length");
    assert!(
        err.to_string().contains("-7"),
        "the error quotes the bad length: {err}"
    );
}

#[test]
fn a_column_that_is_not_utf8_is_refused_rather_than_quietly_replaced() {
    // The strict half of the UTF-8 split: a value that cannot be decoded is a
    // result the harness must not accept, because a lossy replacement character
    // would make two engines "agree" on a value neither produced.
    let mut payload = Vec::new();
    payload.extend_from_slice(&1i16.to_be_bytes());
    payload.extend_from_slice(&2i32.to_be_bytes());
    payload.extend_from_slice(&[0xFF, 0xFE]);
    let err = parse_data_row(&payload).expect_err("invalid UTF-8 must be refused");
    assert!(
        err.to_string().contains("UTF-8"),
        "the error says why: {err}"
    );
}

// ─── ErrorResponse ──────────────────────────────────────────────────────────

#[test]
fn an_error_response_keeps_its_sqlstate_and_its_message() {
    let payload = error_payload(&[
        (b'S', "ERROR"),
        (b'V', "ERROR"),
        (b'C', "42P01"),
        (b'M', "relation \"nope\" does not exist"),
        (b'P', "15"),
        (b'F', "parse_relation.c"),
        (b'L', "1392"),
        (b'R', "parserOpenTable"),
    ]);
    let e = parse_error_response(&payload);

    assert_eq!(
        e.code, "42P01",
        "the SQLSTATE is the field a program acts on"
    );
    assert_eq!(e.message, "relation \"nope\" does not exist");
    assert_eq!(e.severity, "ERROR");
    assert_eq!(e.position.as_deref(), Some("15"));
    assert_eq!(e.routine.as_deref(), Some("parserOpenTable"));
    assert!(
        e.to_string().contains("42P01") && e.to_string().contains("does not exist"),
        "Display carries both the code and the message: {e}"
    );
}

#[test]
fn the_non_localised_severity_wins_over_the_localised_one_in_either_order() {
    // `S` is translated per the server's lc_messages; `V` is not. A harness
    // that matched on severity would otherwise behave differently on a server
    // configured in another language.
    let s_first = parse_error_response(&error_payload(&[
        (b'S', "FEHLER"),
        (b'V', "ERROR"),
        (b'C', "23505"),
        (b'M', "duplicate key"),
    ]));
    let v_first = parse_error_response(&error_payload(&[
        (b'V', "ERROR"),
        (b'S', "FEHLER"),
        (b'C', "23505"),
        (b'M', "duplicate key"),
    ]));
    assert_eq!(s_first.severity, "ERROR");
    assert_eq!(v_first.severity, "ERROR");
    assert_eq!(s_first, v_first, "field order must not change the result");
}

#[test]
fn a_server_that_sends_only_the_localised_severity_still_gets_one() {
    let e = parse_error_response(&error_payload(&[
        (b'S', "WARNING"),
        (b'C', "25P01"),
        (b'M', "there is no transaction in progress"),
    ]));
    assert_eq!(e.severity, "WARNING");
}

#[test]
fn detail_and_hint_are_optional_and_absent_means_none() {
    let bare = parse_error_response(&error_payload(&[(b'C', "42601"), (b'M', "syntax error")]));
    assert_eq!(bare.detail, None);
    assert_eq!(bare.hint, None);
    assert_eq!(bare.context, None);

    let full = parse_error_response(&error_payload(&[
        (b'C', "23505"),
        (b'M', "duplicate key value violates unique constraint"),
        (b'D', "Key (id)=(1) already exists."),
        (b'H', "try a different id"),
        (b'W', "PL/pgSQL function f() line 3"),
    ]));
    assert_eq!(full.detail.as_deref(), Some("Key (id)=(1) already exists."));
    assert_eq!(full.hint.as_deref(), Some("try a different id"));
    assert_eq!(
        full.context.as_deref(),
        Some("PL/pgSQL function f() line 3")
    );
}

#[test]
fn a_truncated_error_response_still_yields_what_it_carried() {
    // Deliberate: the caller is already on a failure path. Returning "the error
    // could not be parsed" in place of the error is how a diagnosis is lost.
    let mut payload = error_payload(&[(b'C', "57014"), (b'M', "statement timeout")]);
    payload.truncate(payload.len() - 4); // eat the message's tail and terminator
    let e = parse_error_response(&payload);
    assert_eq!(
        e.code, "57014",
        "the SQLSTATE survived even though the payload did not"
    );
}

#[test]
fn an_empty_error_response_is_a_default_not_a_panic() {
    assert_eq!(parse_error_response(&[]), PgError::default());
    assert_eq!(parse_error_response(&[0]), PgError::default());
}

// ─── CommandComplete tags ───────────────────────────────────────────────────

#[test]
fn a_command_complete_tag_is_read_up_to_its_terminator() {
    let mut payload = Vec::new();
    cstr(&mut payload, "SELECT 42");
    assert_eq!(parse_command_complete(&payload).unwrap(), "SELECT 42");
}

#[test]
fn an_unterminated_command_complete_tag_is_refused() {
    let err = parse_command_complete(b"SELECT 42").expect_err("no NUL terminator");
    assert!(
        err.to_string().contains("NUL-terminated"),
        "the error says what was missing: {err}"
    );
}

#[test]
fn a_row_count_comes_from_the_last_token_so_insert_reports_rows_not_its_oid() {
    // `INSERT` is the only tag with two numbers, and the OID comes FIRST. A
    // parser that took the first number would report 0 for every insert into a
    // table without OIDs, i.e. every table since PostgreSQL 12.
    assert_eq!(row_count_from_tag("INSERT 0 1"), 1);
    assert_eq!(row_count_from_tag("INSERT 0 5000"), 5000);
    assert_eq!(row_count_from_tag("INSERT 16384 3"), 3);

    assert_eq!(row_count_from_tag("SELECT 0"), 0);
    assert_eq!(row_count_from_tag("SELECT 1"), 1);
    assert_eq!(row_count_from_tag("UPDATE 7"), 7);
    assert_eq!(row_count_from_tag("DELETE 12"), 12);
    assert_eq!(row_count_from_tag("MOVE 3"), 3);
    assert_eq!(row_count_from_tag("FETCH 9"), 9);
    assert_eq!(row_count_from_tag("COPY 100"), 100);
}

#[test]
fn a_tag_with_no_count_reports_zero_rather_than_guessing() {
    for tag in [
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
        "SET",
        "CREATE TABLE",
        "SHOW",
        "",
    ] {
        assert_eq!(
            row_count_from_tag(tag),
            0,
            "{tag:?} carries no row count and must report 0"
        );
    }
}

// ─── RowDescription ─────────────────────────────────────────────────────────

#[test]
fn a_row_description_yields_its_column_names_in_select_list_order() {
    let payload = row_description_payload(&["id", "name", "?column?"]);
    let cols = parse_row_description(&payload).unwrap();
    assert_eq!(cols, vec!["id", "name", "?column?"]);
}

#[test]
fn a_row_description_with_no_columns_parses_to_no_columns() {
    let cols = parse_row_description(&row_description_payload(&[])).unwrap();
    assert!(cols.is_empty());
}

#[test]
fn a_row_description_that_ends_inside_its_metadata_is_refused() {
    let mut payload = row_description_payload(&["id"]);
    payload.truncate(payload.len() - 5);
    let err = parse_row_description(&payload).expect_err("truncated metadata must be refused");
    assert!(err.to_string().contains("pgwire"), "{err}");
}

// ─── Authentication ─────────────────────────────────────────────────────────

#[test]
fn an_authentication_message_yields_its_code_and_every_code_has_a_name() {
    assert_eq!(parse_authentication(&0i32.to_be_bytes()).unwrap(), 0);
    assert_eq!(parse_authentication(&5i32.to_be_bytes()).unwrap(), 5);
    assert_eq!(parse_authentication(&10i32.to_be_bytes()).unwrap(), 10);

    assert_eq!(auth_method_name(0), "AuthenticationOk");
    assert_eq!(auth_method_name(3), "cleartext password");
    assert_eq!(auth_method_name(5), "MD5 password");
    assert_eq!(auth_method_name(10), "SASL/SCRAM");
    assert_eq!(
        auth_method_name(999),
        "an unrecognised method",
        "an unknown code still produces a sentence, so the refusal reads"
    );
}

#[test]
fn a_short_authentication_message_is_an_error_not_a_panic() {
    assert!(parse_authentication(&[0, 0]).is_err());
    assert!(parse_authentication(&[]).is_err());
}

// ─── Encoders ───────────────────────────────────────────────────────────────

#[test]
fn a_framed_message_has_a_length_that_counts_itself_but_not_the_type_byte() {
    let mut out = Vec::new();
    frame(&mut out, b'Q', b"hi").unwrap();
    assert_eq!(
        out,
        vec![b'Q', 0, 0, 0, 6, b'h', b'i'],
        "2 body bytes + 4 for the length field = 6; the type byte is not counted"
    );

    let mut empty = Vec::new();
    frame(&mut empty, b'S', &[]).unwrap();
    assert_eq!(
        empty,
        vec![b'S', 0, 0, 0, 4],
        "a bodiless message still declares 4"
    );
}

#[test]
fn a_startup_message_carries_no_type_byte_and_declares_protocol_three() {
    let msg = encode_startup("alice", "bench").unwrap();

    let declared = i32::from_be_bytes([msg[0], msg[1], msg[2], msg[3]]);
    assert_eq!(
        declared as usize,
        msg.len(),
        "the startup length counts the WHOLE message including itself — this is \
         the one message with no type byte, and a client that frames it like the \
         others sends a leading byte the server folds into the length"
    );

    let version = i32::from_be_bytes([msg[4], msg[5], msg[6], msg[7]]);
    assert_eq!(version, 196_608, "protocol 3.0 is (3 << 16) | 0");

    assert_eq!(
        &msg[8..],
        b"user\0alice\0database\0bench\0\0",
        "NUL-terminated pairs, then a bare NUL to end the list"
    );
}

#[test]
fn a_nul_in_a_startup_parameter_is_refused_rather_than_silently_truncating() {
    // Encoding it would connect to a DIFFERENT database than the caller named,
    // and the benchmark would report the wrong corpus as the right one.
    let err = encode_startup("alice", "bench\0evil").expect_err("a NUL must be refused");
    assert!(
        err.to_string().contains("database") && err.to_string().contains("NUL"),
        "the error names the parameter: {err}"
    );
    assert!(encode_startup("bob\0x", "bench").is_err());
    assert!(encode_startup("bob", "bench").is_ok());
}

#[test]
fn a_simple_query_is_one_q_message_holding_a_terminated_statement() {
    let msg = encode_simple_query("SELECT 1").unwrap();
    assert_eq!(msg[0], b'Q');
    let len = i32::from_be_bytes([msg[1], msg[2], msg[3], msg[4]]);
    assert_eq!(len as usize, msg.len() - 1, "the type byte is not counted");
    assert_eq!(&msg[5..], b"SELECT 1\0");

    assert!(
        encode_simple_query("SELECT 1\0DROP TABLE t").is_err(),
        "a NUL would truncate the statement on the wire"
    );
}

/// Walk a frontend buffer, returning `(type byte, payload)` for each message.
/// Doubles as a check that the buffer is self-consistently framed: a wrong
/// length here shows up as a wrong type byte on the next iteration.
fn split_frontend(buf: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < buf.len() {
        let t = buf[at];
        let len = i32::from_be_bytes([buf[at + 1], buf[at + 2], buf[at + 3], buf[at + 4]]) as usize;
        assert!(len >= 4, "message {:?} declared length {len}", t as char);
        let body = buf[at + 5..at + 1 + len].to_vec();
        out.push((t, body));
        at += 1 + len;
    }
    assert_eq!(
        at,
        buf.len(),
        "the buffer ended exactly on a message boundary"
    );
    out
}

#[test]
fn an_extended_query_is_parse_bind_describe_execute_sync_in_one_buffer() {
    let msg = encode_extended_query("SELECT $1::text, $2::text", &[Some("hello"), None]).unwrap();
    let parts = split_frontend(&msg);

    let types: Vec<char> = parts.iter().map(|(t, _)| *t as char).collect();
    assert_eq!(
        types,
        vec!['P', 'B', 'D', 'E', 'S'],
        "all five go out together: the server answers nothing until Sync, so \
         splitting them buys syscalls and packets and changes no semantics"
    );

    // Parse: unnamed statement, the SQL, then zero parameter type OIDs so the
    // server infers them.
    let parse = &parts[0].1;
    assert_eq!(parse[0], 0, "unnamed prepared statement");
    assert_eq!(&parse[1..parse.len() - 3], b"SELECT $1::text, $2::text");
    assert_eq!(
        i16::from_be_bytes([parse[parse.len() - 2], parse[parse.len() - 1]]),
        0,
        "zero declared parameter types; the server infers from context"
    );

    // Bind: unnamed portal + unnamed statement, no format codes (all text),
    // then the values with -1 standing for NULL.
    let bind = &parts[1].1;
    let mut at = 0usize;
    assert_eq!(bind[at], 0, "unnamed portal");
    at += 1;
    assert_eq!(bind[at], 0, "unnamed statement");
    at += 1;
    assert_eq!(
        i16::from_be_bytes([bind[at], bind[at + 1]]),
        0,
        "zero parameter format codes means all default, and the default is text"
    );
    at += 2;
    assert_eq!(
        i16::from_be_bytes([bind[at], bind[at + 1]]),
        2,
        "two values"
    );
    at += 2;
    let l0 = i32::from_be_bytes([bind[at], bind[at + 1], bind[at + 2], bind[at + 3]]);
    at += 4;
    assert_eq!(l0, 5);
    assert_eq!(&bind[at..at + 5], b"hello");
    at += 5;
    let l1 = i32::from_be_bytes([bind[at], bind[at + 1], bind[at + 2], bind[at + 3]]);
    at += 4;
    assert_eq!(
        l1, -1,
        "None binds as SQL NULL, which is length -1 — not as an empty string"
    );
    assert_eq!(
        i16::from_be_bytes([bind[at], bind[at + 1]]),
        0,
        "zero result format codes: everything comes back as text"
    );

    // Describe the PORTAL, so column names arrive even for an empty result.
    assert_eq!(parts[2].1, vec![b'P', 0]);

    // Execute the unnamed portal with no row limit.
    let exec = &parts[3].1;
    assert_eq!(exec[0], 0, "unnamed portal");
    assert_eq!(
        i32::from_be_bytes([exec[1], exec[2], exec[3], exec[4]]),
        0,
        "max_rows = 0 means unlimited; any other value invites PortalSuspended \
         and a result that is silently a prefix"
    );

    assert!(parts[4].1.is_empty(), "Sync has no body");
}

#[test]
fn an_extended_query_with_no_parameters_still_binds_zero_of_them() {
    let msg = encode_extended_query("SELECT 1", &[]).unwrap();
    let parts = split_frontend(&msg);
    assert_eq!(parts.len(), 5);
    let bind = &parts[1].1;
    assert_eq!(
        i16::from_be_bytes([bind[4], bind[5]]),
        0,
        "zero bound values"
    );
}

#[test]
fn an_empty_string_parameter_binds_as_length_zero_not_as_null() {
    let msg = encode_extended_query("SELECT $1::text", &[Some("")]).unwrap();
    let parts = split_frontend(&msg);
    let bind = &parts[1].1;
    // portal(1) + stmt(1) + formats(2) + count(2) = 6 bytes before the length.
    let len = i32::from_be_bytes([bind[6], bind[7], bind[8], bind[9]]);
    assert_eq!(
        len, 0,
        "the empty string is length 0; NULL is length -1; the encoder must not \
         conflate them any more than the decoder does"
    );
}
