//! A native PostgreSQL v3 wire-protocol client, so the harness can drive the
//! RDBMS comparator the same way it drives its own engine.
//!
//! The comparator ran through `psql` until now, and that is not a client — it
//! is a process. Three consequences, each of which corrupts the one number the
//! harness exists to produce:
//!
//! 1. **A process cannot be a concurrent client.** The concurrency ceiling
//!    measurement drives K clients at once and asks how throughput grows with
//!    K. Through `psql` that is K forks, K connection handshakes and K pipes
//!    per operation, so the curve measures the operating system's process
//!    scheduler well before it measures the database's lock contention. There
//!    is no value of K at which the answer is about Postgres.
//! 2. **Timing includes the wrong things.** `\timing` covers the round trip as
//!    `psql` sees it, and the harness's own timing additionally covers fork,
//!    exec, dynamic linking and connection setup — tens of milliseconds of
//!    constant overhead added to queries whose interesting range starts around
//!    one millisecond.
//! 3. **Errors arrive as text on a stream.** A failed statement becomes a line
//!    on stderr that the caller may or may not be reading. A benchmark that
//!    swallows an error reports a fast, wrong number, which is worse than
//!    reporting nothing — it is a result that looks like a win.
//!
//! So this speaks the protocol directly, over `std` only. No external crate:
//! the workspace's one-`cc`-invocation purity rule and the `c-deps` gate both
//! hold, for the same reason `engram_bolt`'s client exists rather than a real
//! Neo4j driver.
//!
//! # Why there is no authentication code here
//!
//! The benchmark pod's `pg_hba.conf` is `trust` on **every** line, including
//! the catch-all `host all all all trust`. Under `trust` the server answers
//! `StartupMessage` with `AuthenticationOk` and nothing else — no password
//! round trip, no MD5, no SCRAM. Implementing SCRAM would mean HMAC-SHA-256,
//! PBKDF2 and a channel-binding decision, for a code path that provably never
//! executes against the only server this client is pointed at.
//!
//! What is implemented instead is the failure: if the server ever answers with
//! any authentication request other than `AuthenticationOk`, [`PgClient::connect`]
//! returns an error that **names the method by its protocol name** and stops.
//! That is the important half. A client that silently ignored the request would
//! sit waiting for a `ReadyForQuery` the server will never send, and a hung
//! benchmark client is indistinguishable from a slow database — the harness
//! would report a collapse and the collapse would be this file.
//!
//! # The poisoned-connection guard
//!
//! This is the design decision the rest of the module is arranged around.
//!
//! A Postgres connection is a single byte stream with no message boundaries
//! above the framing layer, so there are exactly two ways an operation can end
//! and they must never be confused:
//!
//! * **The server reported a SQL error.** `ErrorResponse` arrives, then the
//!   server discards input until `Sync` and sends `ReadyForQuery`. The
//!   connection is *fine*. It is reusable the moment `ReadyForQuery` has been
//!   consumed — and the client must consume it, because a `ReadyForQuery` left
//!   in the socket becomes the first message the *next* query reads, which
//!   makes that query return the previous one's result. In a concurrency
//!   harness that manifests as a throughput collapse with plausible-looking
//!   numbers, and it would be attributed to the database.
//! * **The framing itself broke.** A short read, an unexpected message type, a
//!   length field that does not agree with the payload. Now the client no
//!   longer knows where the next message starts, and every subsequent parse is
//!   arbitrary. There is no recovery from this that does not involve a new
//!   connection.
//!
//! So the first case drains to `ReadyForQuery` and returns `Err` on a *usable*
//! client; the second sets [`PgClient::is_poisoned`] and every later call fails
//! immediately with a named error rather than returning fiction. The whole
//! point is that a wrong number is never produced: after any failure the client
//! is either provably re-synchronised or provably dead, and it says which.
//!
//! # Nagle
//!
//! `TCP_NODELAY` is set. The client writes one request and then blocks reading
//! the response, which is precisely the traffic pattern Nagle's algorithm
//! delays: without it the measurement acquires a ~40 ms floor on some paths and
//! the harness reports the delayed-ACK interaction as database latency.

use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

// ─── Framing constants ──────────────────────────────────────────────────────

/// Protocol version 3.0, as the major/minor pair the `StartupMessage` carries
/// (`3 << 16 | 0`). This is the only version any supported server speaks; a
/// server that dislikes it answers `ErrorResponse` and closes.
const PROTOCOL_VERSION_3: i32 = 196_608;

/// The largest single backend message this client will allocate for.
///
/// A length field is attacker- or corruption-controlled in the sense that
/// matters here: if framing has drifted, four arbitrary bytes become a length,
/// and a client that trusts it reserves that many bytes. Postgres's own limit
/// on a field value is 1 GiB, so nothing legitimate exceeds this, and a bogus
/// length now produces a named framing error instead of an allocation failure
/// that reads like the machine running out of memory.
const MAX_MESSAGE_BYTES: usize = 1 << 30;

/// How long [`PgClient::connect`] will wait for the TCP handshake.
///
/// Deliberately present, and deliberately generous. A benchmark that blocks in
/// `connect` forever looks exactly like a database that has stopped answering,
/// and the harness would record the wrong cause. Ten seconds to a pod-local
/// address is already pathological, and this bounds only the TCP handshake —
/// query time is unbounded on purpose, because an LSQB statement is allowed to
/// run for twenty minutes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The most notices retained for one operation before recording stops.
///
/// Notices are kept (a dropped `WARNING: there is already a transaction in
/// progress` is real signal a harness should not lose) but a single statement
/// can raise unboundedly many, and a load generator must not grow a buffer per
/// operation forever.
const MAX_RETAINED_NOTICES: usize = 64;

// ─── Errors ─────────────────────────────────────────────────────────────────

/// A structured `ErrorResponse` or `NoticeResponse` from the server.
///
/// This exists because the alternative — flattening the server's answer into a
/// string — loses the SQLSTATE, and the SQLSTATE is the only field a program
/// can act on. `40001` (serialisation failure) is a retry, `57014` (statement
/// cancelled by timeout) is a measurement that must be excluded rather than
/// counted as slow, and `42P01` (undefined table) means the harness is pointed
/// at the wrong database and every subsequent number is meaningless. Those are
/// three different outcomes that all render as "an error occurred".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PgError {
    /// `ERROR`, `FATAL`, `PANIC`, `WARNING`, `NOTICE`, … Taken from the
    /// non-localised `V` field when the server sends one (9.6 and later) and
    /// from the localised `S` field otherwise, so it is stable across the
    /// server's `lc_messages` setting.
    pub severity: String,
    /// The five-character SQLSTATE (`23505`, `42P01`, `57014`). Empty only if
    /// the server omitted the field, which is a protocol violation.
    pub code: String,
    /// The primary human-readable message. Always present in a well-formed
    /// `ErrorResponse`.
    pub message: String,
    /// Optional secondary detail.
    pub detail: Option<String>,
    /// Optional suggestion; advice rather than fact, per the protocol.
    pub hint: Option<String>,
    /// Character offset into the original statement, 1-based, when the error is
    /// positional.
    pub position: Option<String>,
    /// The protocol's `W` ("Where") field: the call-stack-like context in which
    /// the error occurred.
    pub context: Option<String>,
    /// The server source routine that raised it — useful when a message is
    /// ambiguous and the SQLSTATE is generic.
    pub routine: Option<String>,
}

impl fmt::Display for PgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sev = if self.severity.is_empty() {
            "ERROR"
        } else {
            &self.severity
        };
        let code = if self.code.is_empty() {
            "-----"
        } else {
            &self.code
        };
        let msg = &self.message;
        write!(f, "postgres {sev} [{code}]: {msg}")?;
        if let Some(d) = &self.detail {
            write!(f, " | detail: {d}")?;
        }
        if let Some(h) = &self.hint {
            write!(f, " | hint: {h}")?;
        }
        if let Some(p) = &self.position {
            write!(f, " | position: {p}")?;
        }
        Ok(())
    }
}

impl std::error::Error for PgError {}

/// Recover the structured [`PgError`] from an error this module returned, if
/// that error was a server-reported SQL failure rather than an I/O or framing
/// one.
///
/// The public API returns [`std::io::Result`] because the caller is doing I/O
/// and one error type beats two. That erases the SQLSTATE unless there is a way
/// back, which is this: an operation that failed because the *server* said so
/// carries the whole `ErrorResponse` inside, and an operation that failed
/// because the *socket* did returns `None` here. Distinguishing those two is
/// how a harness decides whether to retry, exclude the sample, or stop.
#[must_use]
pub fn as_pg_error(err: &std::io::Error) -> Option<&PgError> {
    err.get_ref()?.downcast_ref::<PgError>()
}

/// Build the module's non-server error: framing, I/O misuse, or a protocol
/// case this client does not implement. Distinguished from a [`PgError`] by
/// carrying no SQLSTATE, which is exactly what [`as_pg_error`] keys on.
fn protocol_error(what: impl Into<String>) -> std::io::Error {
    let what = what.into();
    std::io::Error::other(format!("pgwire: {what}"))
}

// ─── Byte-level readers over a message payload ──────────────────────────────
//
// Every one of these is bounds-checked and returns an error rather than
// panicking. A benchmark client parses whatever the socket produced, and the
// framing may already have drifted by the time a payload reaches here — an
// index panic would take down a harness thread and be reported as a crash of
// the thing being measured.

fn take<'a>(payload: &'a [u8], at: &mut usize, n: usize) -> std::io::Result<&'a [u8]> {
    let end = at
        .checked_add(n)
        .ok_or_else(|| protocol_error("a message field length overflowed the payload offset"))?;
    let slice = payload
        .get(*at..end)
        .ok_or_else(|| protocol_error("a message ended in the middle of a field"))?;
    *at = end;
    Ok(slice)
}

fn read_i16(payload: &[u8], at: &mut usize) -> std::io::Result<i16> {
    let b = take(payload, at, 2)?;
    Ok(i16::from_be_bytes([b[0], b[1]]))
}

fn read_i32(payload: &[u8], at: &mut usize) -> std::io::Result<i32> {
    let b = take(payload, at, 4)?;
    Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Read a NUL-terminated field, returning the bytes without the terminator.
fn read_cstr_bytes<'a>(payload: &'a [u8], at: &mut usize) -> std::io::Result<&'a [u8]> {
    let start = *at;
    let rest = payload
        .get(start..)
        .ok_or_else(|| protocol_error("a string field started past the end of its message"))?;
    let len = rest
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| protocol_error("a string field was not NUL-terminated"))?;
    *at = start + len + 1;
    Ok(&rest[..len])
}

/// The lossy half of the UTF-8 split, used for **error and notice text only**.
///
/// The strict half is in [`parse_data_row`], and the asymmetry is deliberate.
/// A value that is not valid UTF-8 is a result the harness must not silently
/// accept, so it fails. An *error message* that is not valid UTF-8 must still
/// be delivered, because failing to report an error in order to complain about
/// its encoding loses the only diagnostic there was.
fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The value of a `ParameterStatus` payload, if it announces `want`.
///
/// A malformed one answers `None` rather than an error on purpose:
/// `ParameterStatus` is unsolicited — the server sends it whenever a GUC
/// changes — so refusing a query because a status message the caller never
/// asked for was odd would fail the wrong operation.
fn parameter_status_value(payload: &[u8], want: &[u8]) -> Option<String> {
    let mut at = 0usize;
    let name = read_cstr_bytes(payload, &mut at).ok()?;
    if name != want {
        return None;
    }
    let value = read_cstr_bytes(payload, &mut at).ok()?;
    Some(lossy(value))
}

// ─── Decoders (public so they can be tested without a server) ───────────────
//
// These are `pub` for one reason, and it is the reason this module has tests at
// all: the interesting parsing cases — a NULL that is not an empty string, an
// `ErrorResponse` field list, a `CommandComplete` tag with an OID in it — are
// reachable from a byte slice and need no PostgreSQL anywhere. A test that can
// only run when a database happens to be reachable is a test that gets skipped,
// and a skipped test looks exactly like a passing one.

/// Parse an `ErrorResponse` / `NoticeResponse` payload (the bytes after the
/// type byte and length) into a [`PgError`].
///
/// Infallible by construction. An unterminated or truncated field list yields
/// whatever was recovered before the payload ran out rather than an error,
/// because the caller is already on a failure path: returning "could not parse
/// the error" in place of the error is how a diagnosis gets lost.
#[must_use]
pub fn parse_error_response(payload: &[u8]) -> PgError {
    let mut out = PgError::default();
    // The localised `S` field is a fallback for the non-localised `V`; if both
    // arrive, `V` wins regardless of order.
    let mut have_nonlocalised_severity = false;
    let mut at = 0usize;
    while let Some(&field) = payload.get(at) {
        at += 1;
        if field == 0 {
            break;
        }
        let Ok(raw) = read_cstr_bytes(payload, &mut at) else {
            break;
        };
        let value = lossy(raw);
        match field {
            b'V' => {
                out.severity = value;
                have_nonlocalised_severity = true;
            }
            b'S' if !have_nonlocalised_severity => out.severity = value,
            b'C' => out.code = value,
            b'M' => out.message = value,
            b'D' => out.detail = Some(value),
            b'H' => out.hint = Some(value),
            b'P' => out.position = Some(value),
            b'W' => out.context = Some(value),
            b'R' => out.routine = Some(value),
            // Everything else (schema, table, column, datatype, constraint,
            // file, line, internal position/query) is deliberately dropped: it
            // is either redundant with `message` or only meaningful to someone
            // reading the server's source.
            _ => {}
        }
    }
    out
}

/// Parse a `RowDescription` payload into the column names, in order.
///
/// Type OIDs, table OIDs, attribute numbers, type modifiers and format codes
/// are read (so the payload is validated end to end) and discarded. Everything
/// this client asks for comes back in text format, so a type OID would only be
/// used to re-derive a type the caller already knows from its own SQL.
pub fn parse_row_description(payload: &[u8]) -> std::io::Result<Vec<String>> {
    let mut at = 0usize;
    let n = read_i16(payload, &mut at)?;
    if n < 0 {
        return Err(protocol_error(
            "RowDescription declared a negative field count",
        ));
    }
    let n = n as usize;
    let mut cols = Vec::with_capacity(n);
    for i in 0..n {
        let name = read_cstr_bytes(payload, &mut at)?;
        let name = std::str::from_utf8(name).map_err(|e| {
            protocol_error(format!(
                "column {i} has a name that is not valid UTF-8: {e}"
            ))
        })?;
        cols.push(name.to_string());
        // tableOid i32, columnAttr i16, typeOid i32, typeLen i16, typeMod i32,
        // formatCode i16 — 18 bytes, read as a block because none is used.
        take(payload, &mut at, 18)?;
    }
    if at != payload.len() {
        return Err(protocol_error(
            "RowDescription had bytes left over after its declared fields",
        ));
    }
    Ok(cols)
}

/// Parse a `DataRow` payload into text values, where `None` is SQL NULL.
///
/// **NULL is a length of `-1`, and an empty string is a length of `0`.** They
/// are different bytes on the wire and they stay different here. Collapsing
/// them is the classic wire-client bug and it is silent: a `LEFT JOIN` that
/// found no match and a column that genuinely holds `''` become the same value,
/// so a correctness comparison between two engines passes while the engines
/// disagree.
///
/// Values must be valid UTF-8, and this is the **strict** half of the module's
/// UTF-8 split: a value that cannot be decoded is refused rather than repaired
/// with replacement characters, because a repaired value is a wrong answer that
/// two engines could "agree" on. Error and notice *text* is decoded leniently
/// instead — failing to report an error in order to complain about its encoding
/// throws away the only diagnostic there was.
pub fn parse_data_row(payload: &[u8]) -> std::io::Result<Vec<Option<String>>> {
    let mut at = 0usize;
    let n = read_i16(payload, &mut at)?;
    if n < 0 {
        return Err(protocol_error("DataRow declared a negative column count"));
    }
    let n = n as usize;
    let mut row = Vec::with_capacity(n);
    for i in 0..n {
        let len = read_i32(payload, &mut at)?;
        if len == -1 {
            row.push(None);
            continue;
        }
        if len < 0 {
            return Err(protocol_error(format!(
                "column {i} declared length {len}, which is neither a size nor the NULL sentinel -1"
            )));
        }
        let bytes = take(payload, &mut at, len as usize)?;
        let text = std::str::from_utf8(bytes)
            .map_err(|e| protocol_error(format!("column {i} is not valid UTF-8 text: {e}")))?;
        row.push(Some(text.to_string()));
    }
    if at != payload.len() {
        return Err(protocol_error(
            "DataRow had bytes left over after its declared columns",
        ));
    }
    Ok(row)
}

/// Parse a `CommandComplete` payload into its tag (`SELECT 1`, `INSERT 0 1`).
pub fn parse_command_complete(payload: &[u8]) -> std::io::Result<String> {
    let mut at = 0usize;
    let tag = read_cstr_bytes(payload, &mut at)?;
    let tag = std::str::from_utf8(tag)
        .map_err(|e| protocol_error(format!("CommandComplete tag is not valid UTF-8: {e}")))?;
    Ok(tag.to_string())
}

/// The row count a `CommandComplete` tag reports, or `0` when it reports none.
///
/// The rule is "the last whitespace-separated token, if it parses as a number",
/// which is not a shortcut — it is what the protocol's tag grammar reduces to.
/// `INSERT` is the only tag with two numbers (`INSERT <oid> <rows>`) and the
/// count is the second, so taking the last token is correct there and correct
/// for the single-number tags (`SELECT`, `UPDATE`, `DELETE`, `MOVE`, `FETCH`,
/// `COPY`). Tags with no count at all (`BEGIN`, `COMMIT`, `SET`, `CREATE
/// TABLE`) end in a word, which does not parse, and yield `0`.
#[must_use]
pub fn row_count_from_tag(tag: &str) -> u64 {
    tag.rsplit(' ')
        .next()
        .and_then(|t| t.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Parse an `Authentication*` payload, returning the request code.
///
/// `0` is `AuthenticationOk` and is the only code this client can proceed past;
/// see [`auth_method_name`] for what the rest are called.
pub fn parse_authentication(payload: &[u8]) -> std::io::Result<i32> {
    let mut at = 0usize;
    read_i32(payload, &mut at)
}

/// The protocol name of an authentication request code.
///
/// Exists so the refusal names what was asked for. "the server demanded
/// authentication" sends someone to read `pg_hba.conf` with no idea what they
/// are looking for; "the server demanded SASL/SCRAM authentication" says which
/// line changed and what implementing it would cost.
#[must_use]
pub fn auth_method_name(code: i32) -> &'static str {
    match code {
        0 => "AuthenticationOk",
        2 => "Kerberos V5",
        3 => "cleartext password",
        5 => "MD5 password",
        6 => "SCM credential",
        7 => "GSSAPI",
        8 => "GSSAPI continuation",
        9 => "SSPI",
        10 => "SASL/SCRAM",
        11 => "SASL/SCRAM continuation",
        12 => "SASL/SCRAM final",
        _ => "an unrecognised method",
    }
}

// ─── Encoders (public for the same reason) ──────────────────────────────────

/// Append one framed frontend message: the type byte, a big-endian `i32` length
/// that **counts itself but not the type byte**, then the body.
///
/// That off-by-four is the protocol's, not a mistake here, and it is the single
/// most common way a hand-written client desynchronises — so it lives in one
/// function that every message goes through rather than at each call site.
pub fn frame(out: &mut Vec<u8>, msg_type: u8, body: &[u8]) -> std::io::Result<()> {
    let len = u32::try_from(body.len())
        .ok()
        .and_then(|n| n.checked_add(4))
        .ok_or_else(|| protocol_error("a frontend message body exceeded the 4 GiB length field"))?;
    out.push(msg_type);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    Ok(())
}

/// Encode a `StartupMessage`.
///
/// The one message in the protocol with **no type byte** — it is length,
/// version, then NUL-terminated key/value pairs terminated by a bare NUL. A
/// client that frames it like every other message sends a leading byte the
/// server reads as part of the length and then hangs waiting for gigabytes.
///
/// A NUL inside `user` or `database` is refused rather than encoded, because
/// the encoding would silently truncate the name at the NUL and the server
/// would open a connection to a *different* database than the caller asked for
/// — a benchmark measuring the wrong corpus and reporting it as the right one.
pub fn encode_startup(user: &str, database: &str) -> std::io::Result<Vec<u8>> {
    for (label, value) in [("user", user), ("database", database)] {
        if value.as_bytes().contains(&0) {
            return Err(protocol_error(format!(
                "the {label} parameter contains a NUL byte, which the wire format cannot carry"
            )));
        }
    }
    let mut body = Vec::with_capacity(64 + user.len() + database.len());
    body.extend_from_slice(&PROTOCOL_VERSION_3.to_be_bytes());
    for (k, v) in [("user", user), ("database", database)] {
        body.extend_from_slice(k.as_bytes());
        body.push(0);
        body.extend_from_slice(v.as_bytes());
        body.push(0);
    }
    body.push(0);
    let len = u32::try_from(body.len())
        .ok()
        .and_then(|n| n.checked_add(4))
        .ok_or_else(|| protocol_error("the startup parameters exceeded the length field"))?;
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Encode a Simple Query (`Q`) message.
pub fn encode_simple_query(sql: &str) -> std::io::Result<Vec<u8>> {
    if sql.as_bytes().contains(&0) {
        return Err(protocol_error(
            "the statement contains a NUL byte, which would truncate it on the wire",
        ));
    }
    let mut body = Vec::with_capacity(sql.len() + 1);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let mut out = Vec::with_capacity(body.len() + 5);
    frame(&mut out, b'Q', &body)?;
    Ok(out)
}

/// Encode one extended-query round trip: Parse, Bind, Describe, Execute, Sync,
/// as a **single buffer written in one `write_all`**.
///
/// Emitted together on purpose. The five messages are one logical request and
/// the server does not answer until `Sync`, so splitting them into five writes
/// buys five syscalls and — with `TCP_NODELAY` set, which it is — five packets,
/// for no change in semantics.
///
/// Three parameter choices are load-bearing and are made here rather than being
/// exposed:
///
/// * **Zero parameter type OIDs in `Parse`.** The server infers each parameter
///   type from where it appears in the statement, which is what a caller
///   passing `&[Option<&str>]` means. Verified against PostgreSQL 17.11 rather
///   than assumed: an *unconstrained* parameter resolves to `text` rather than
///   failing, so a bare `SELECT $1` succeeds and answers with the text that was
///   bound. Where inference genuinely cannot settle a type — an ambiguous
///   operator over two parameters, say — the server says so, and that error
///   arrives here as a normal [`PgError`] rather than being swallowed. Callers
///   who need a specific type should cast in the SQL (`$1::int`), which is also
///   what makes the statement's plan stable across calls.
/// * **Zero format codes, for both parameters and results.** Zero means "all
///   default", and the default is text. [`PgResult`] is text-format by
///   contract, so binary results would have to be converted back.
/// * **`max_rows = 0` in `Execute`, meaning unlimited.** With a row limit the
///   server answers `PortalSuspended` instead of `CommandComplete` and the
///   client must re-`Execute` the portal to continue. A partial result silently
///   returned as a whole one is a wrong count, so the limit is simply never
///   used.
pub fn encode_extended_query(sql: &str, params: &[Option<&str>]) -> std::io::Result<Vec<u8>> {
    encode_extended_query_typed(sql, params, &[])
}

/// PostgreSQL's OID for `bigint`. Declared for every id and integer
/// parameter: LDBC ids do not fit in an `int4`, and leaving the type to
/// inference is what produced `operator does not exist: text = bigint`.
pub const OID_INT8: u32 = 20;
/// PostgreSQL's OID for `double precision`.
pub const OID_FLOAT8: u32 = 701;
/// PostgreSQL's OID for `text`.
pub const OID_TEXT: u32 = 25;
/// PostgreSQL's OID for `text[]`, for a list compared with `= ANY(...)`.
pub const OID_TEXT_ARRAY: u32 = 1009;
/// PostgreSQL's OID for `timestamp without time zone`.
pub const OID_TIMESTAMP: u32 = 1114;

/// As [`encode_extended_query`], with explicit parameter type OIDs.
///
/// # Why declaring the type is not optional for an id
///
/// With zero OIDs the server infers each parameter from its context, and the
/// inference is not always the one the caller meant. Measured on the SNB BI
/// SQL arm 2026-09-21: `bi15` and `bi20` bind a Person id, Postgres inferred
/// `text`, and the statement failed with
/// `operator does not exist: text = bigint`. The value was right and the type
/// was not.
///
/// An OID of 0 keeps the old behaviour for that parameter — "infer this one" —
/// so a caller declares only what it actually knows.
pub fn encode_extended_query_typed(
    sql: &str,
    params: &[Option<&str>],
    oids: &[u32],
) -> std::io::Result<Vec<u8>> {
    if sql.as_bytes().contains(&0) {
        return Err(protocol_error(
            "the statement contains a NUL byte, which would truncate it on the wire",
        ));
    }
    let n_params = i16::try_from(params.len())
        .map_err(|_| protocol_error("more than 32767 bind parameters were supplied"))?;

    let mut out = Vec::with_capacity(sql.len() + 64);

    // Parse: unnamed statement, the SQL, and the parameter type OIDs.
    let mut body = Vec::with_capacity(sql.len() + 8);
    body.push(0); // unnamed prepared statement
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    // Parameter type OIDs: none, or one per parameter. A 0 in the list means
    // "infer this one", so a partial declaration is expressible.
    if oids.is_empty() {
        body.extend_from_slice(&0i16.to_be_bytes());
    } else {
        let n = i16::try_from(oids.len())
            .map_err(|_| protocol_error("more than 32767 parameter type OIDs"))?;
        body.extend_from_slice(&n.to_be_bytes());
        for o in oids {
            body.extend_from_slice(&o.to_be_bytes());
        }
    }
    frame(&mut out, b'P', &body)?;

    // Bind: unnamed portal, unnamed statement, default (text) formats.
    body.clear();
    body.push(0); // unnamed portal
    body.push(0); // unnamed prepared statement
    body.extend_from_slice(&0i16.to_be_bytes()); // parameter format codes: all default
    body.extend_from_slice(&n_params.to_be_bytes());
    for p in params {
        match p {
            None => body.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(v) => {
                let len = i32::try_from(v.len())
                    .map_err(|_| protocol_error("a bind parameter exceeded 2 GiB"))?;
                body.extend_from_slice(&len.to_be_bytes());
                body.extend_from_slice(v.as_bytes());
            }
        }
    }
    body.extend_from_slice(&0i16.to_be_bytes()); // result format codes: all default
    frame(&mut out, b'B', &body)?;

    // Describe the portal, so a RowDescription arrives and the result carries
    // column names even when it carries no rows.
    body.clear();
    body.push(b'P');
    body.push(0);
    frame(&mut out, b'D', &body)?;

    // Execute the unnamed portal with no row limit.
    body.clear();
    body.push(0);
    body.extend_from_slice(&0i32.to_be_bytes());
    frame(&mut out, b'E', &body)?;

    frame(&mut out, b'S', &[])?;
    Ok(out)
}

// ─── Results ────────────────────────────────────────────────────────────────

/// One result set: the columns, the rows, and the tag that closed it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PgResult {
    /// Column names in select-list order. Empty for a statement that returned
    /// no row set at all (`SET`, `CREATE TABLE`, `BEGIN`).
    pub columns: Vec<String>,
    /// Text-format values; `None` is SQL NULL.
    pub rows: Vec<Vec<Option<String>>>,
    /// The `CommandComplete` tag, e.g. `"SELECT 1"` or `"INSERT 0 1"`. Empty
    /// only for the empty statement, which the server closes with
    /// `EmptyQueryResponse` and no tag at all.
    pub tag: String,
}

impl PgResult {
    /// The row count the server reported in [`PgResult::tag`], via
    /// [`row_count_from_tag`].
    ///
    /// Not the same thing as `rows.len()`, and the difference is the point:
    /// after an `UPDATE` the tag says how many rows changed while `rows` is
    /// empty, and after a `SELECT` with no `RETURNING` they agree. A harness
    /// that measured write throughput from `rows.len()` would measure zero.
    #[must_use]
    pub fn affected_rows(&self) -> u64 {
        row_count_from_tag(&self.tag)
    }
}

// ─── The client ─────────────────────────────────────────────────────────────

/// A synchronous PostgreSQL v3 connection, held at `ReadyForQuery`.
///
/// One statement at a time and no pipelining, matching `engram_bolt::client`:
/// the concurrency harness models load with many *clients*, so a client that
/// pipelined internally would measure something no real workload does.
///
/// `Send` but not `Sync` — one connection per thread is the model, and the
/// compile-time assertion below keeps the `Send` half true.
pub struct PgClient {
    stream: TcpStream,
    /// Bytes received from the socket but not yet consumed live in
    /// `buf[pos..fill]`. Present so a result of N `DataRow`s costs O(bytes)
    /// reads rather than 2N `read` syscalls.
    buf: Vec<u8>,
    pos: usize,
    fill: usize,
    /// `Some(reason)` once framing has broken. See the module docs: this is the
    /// difference between "the server refused the statement" and "this client
    /// no longer knows where the next message begins".
    poisoned: Option<String>,
    /// The last `ReadyForQuery` status byte: `I` idle, `T` in a transaction,
    /// `E` in a failed transaction.
    tx_status: u8,
    server_version: String,
    backend_pid: i32,
    notices: Vec<PgError>,
}

impl fmt::Debug for PgClient {
    /// Hand-written rather than derived, because the derive would dump the read
    /// buffer — sixteen kilobytes of mostly-stale bytes — into every assertion
    /// message that mentions a client. What a reader needs when a test fails is
    /// which server, which transaction state, and whether the connection is
    /// still alive.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgClient")
            .field("peer", &self.stream.peer_addr().ok())
            .field("server_version", &self.server_version)
            .field("backend_pid", &self.backend_pid)
            .field("tx_status", &(self.tx_status as char))
            .field("poisoned", &self.poisoned)
            .finish()
    }
}

// A harness that drives K clients from K threads needs `PgClient: Send`, and
// discovering otherwise at the call site produces an error about a closure
// rather than about this type. Asserted here so the break is reported where the
// cause is.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<PgClient>();
};

impl PgClient {
    /// Connect, send the `StartupMessage`, and read through to the first
    /// `ReadyForQuery`.
    ///
    /// `addr` is anything `ToSocketAddrs` accepts, e.g. `"10.42.0.116:5432"`.
    /// Returns an error naming the method if the server requests any
    /// authentication (see the module docs for why none is implemented).
    pub fn connect(addr: &str, user: &str, database: &str) -> std::io::Result<Self> {
        let mut last_err = None;
        let mut stream = None;
        for sa in addr.to_socket_addrs()? {
            match TcpStream::connect_timeout(&sa, CONNECT_TIMEOUT) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        let stream = match stream {
            Some(s) => s,
            None => {
                return Err(last_err.unwrap_or_else(|| {
                    protocol_error(format!("{addr} resolved to no socket address"))
                }));
            }
        };
        // Before anything else: a request/response client that blocks on the
        // read after each write is exactly what Nagle delays.
        stream.set_nodelay(true)?;

        let mut c = PgClient {
            stream,
            buf: vec![0u8; 16 * 1024],
            pos: 0,
            fill: 0,
            poisoned: None,
            tx_status: b'I',
            server_version: String::new(),
            backend_pid: 0,
            notices: Vec::new(),
        };
        let startup = encode_startup(user, database)?;
        c.write_all(&startup)?;

        let mut payload = Vec::new();
        loop {
            let msg_type = c.read_message(&mut payload)?;
            match msg_type {
                b'R' => {
                    let code = parse_authentication(&payload)?;
                    if code != 0 {
                        let name = auth_method_name(code);
                        // Poison rather than merely erroring: the handshake is
                        // incomplete, so there is no re-synchronisation point.
                        return c.poison(format!(
                            "the server demanded {name} authentication (code {code}); this client \
                             implements none, because the benchmark server's pg_hba.conf is trust \
                             on every line — check whether that changed"
                        ));
                    }
                }
                b'S' => {
                    if let Some(v) = parameter_status_value(&payload, b"server_version") {
                        c.server_version = v;
                    }
                }
                b'K' => {
                    let mut at = 0usize;
                    c.backend_pid = read_i32(&payload, &mut at)?;
                    // The cancellation secret is read past but not kept: this
                    // client never issues a CancelRequest, and storing a
                    // credential it cannot use invites someone to assume it can.
                    let _secret = read_i32(&payload, &mut at)?;
                }
                b'N' => c.record_notice(&payload),
                b'E' => {
                    let e = parse_error_response(&payload);
                    // Startup errors close the connection; there is no
                    // ReadyForQuery coming and nothing to drain.
                    c.poisoned = Some(format!("the server refused the connection: {e}"));
                    return Err(std::io::Error::other(e));
                }
                b'Z' => {
                    c.tx_status = *payload
                        .first()
                        .ok_or_else(|| protocol_error("ReadyForQuery carried no status byte"))?;
                    return Ok(c);
                }
                other => {
                    return c.poison(format!(
                        "unexpected message type {:?} during startup",
                        other as char
                    ));
                }
            }
        }
    }

    /// The server's `server_version` as announced in `ParameterStatus`, or
    /// empty if it announced none.
    ///
    /// Recorded for the same reason `engram_bolt::client::Client` records the
    /// server agent: a comparison run has to be able to state what it measured
    /// against, and a version taken from a wiki page is a version nobody
    /// checked.
    #[must_use]
    pub fn server_version(&self) -> &str {
        &self.server_version
    }

    /// The backend process id the server reported, useful for correlating a
    /// client with `pg_stat_activity` while a run is in flight.
    #[must_use]
    pub fn backend_pid(&self) -> i32 {
        self.backend_pid
    }

    /// The last `ReadyForQuery` status byte: `b'I'` idle, `b'T'` in a
    /// transaction, `b'E'` in a failed transaction that only `ROLLBACK` will
    /// leave.
    #[must_use]
    pub fn transaction_status(&self) -> u8 {
        self.tx_status
    }

    /// Whether the connection is inside a transaction, failed or not.
    #[must_use]
    pub fn in_transaction(&self) -> bool {
        self.tx_status == b'T' || self.tx_status == b'E'
    }

    /// Whether framing has broken and this connection can no longer be used.
    ///
    /// A SQL error does **not** poison — see the module docs. If this is true
    /// the only remedy is a new [`PgClient::connect`].
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.is_some()
    }

    /// Why the connection was poisoned, if it was.
    #[must_use]
    pub fn poison_reason(&self) -> Option<&str> {
        self.poisoned.as_deref()
    }

    /// Notices (`NoticeResponse`) the server raised during the most recent
    /// operation. Cleared at the start of each operation and capped, so this is
    /// "what the last statement said", not a log.
    #[must_use]
    pub fn notices(&self) -> &[PgError] {
        &self.notices
    }

    /// Set (or clear, with `None`) the socket read and write timeouts.
    ///
    /// Off by default and deliberately so: an LSQB statement is allowed twenty
    /// minutes, and a timeout that fires mid-result poisons the connection —
    /// correctly, since the remaining messages are still in the socket, but a
    /// harness that set one carelessly would report the timeout as a database
    /// failure.
    pub fn set_timeout(&mut self, dur: Option<Duration>) -> std::io::Result<()> {
        self.stream.set_read_timeout(dur)?;
        self.stream.set_write_timeout(dur)
    }

    /// Run one or more statements with the Simple Query protocol (`Q`) and
    /// return **every** result set, in order.
    ///
    /// This is the primitive; [`PgClient::query`] is the one-result convenience
    /// over it. If any statement in the batch fails, the whole call fails —
    /// the server abandons the rest of the batch, so there is no partial
    /// success to report and no result from a later statement that could be
    /// mistaken for the failed one's.
    pub fn query_multi(&mut self, sql: &str) -> std::io::Result<Vec<PgResult>> {
        self.check_usable()?;
        self.notices.clear();
        let msg = encode_simple_query(sql)?;
        self.write_all(&msg)?;
        self.read_until_ready()
    }

    /// Run `sql` with the Simple Query protocol and return **the last** result
    /// set it produced.
    ///
    /// # Which result set, and why the last
    ///
    /// One `Q` may carry several statements and therefore produce several
    /// result sets. This returns the last, because the alternative loses data
    /// in the direction that matters: the idiomatic prologue is
    /// `SET work_mem = '256MB'; SELECT …`, and taking the *first* result would
    /// return the `SET`'s empty answer and silently discard the measurement.
    /// Taking the last returns the `SET` only when the `SET` was the whole
    /// statement, which is what the caller asked for either way.
    ///
    /// Nothing is discarded silently in the case that matters — a failure
    /// anywhere in the batch is an error here — and nothing is discarded at all
    /// if the caller uses [`PgClient::query_multi`], which is the same work
    /// with every result set returned. If the number of result sets is
    /// load-bearing for the caller, that is the method to call; this one is
    /// documented to answer with one.
    pub fn query(&mut self, sql: &str) -> std::io::Result<PgResult> {
        let mut all = self.query_multi(sql)?;
        all.pop().ok_or_else(|| {
            protocol_error("the server completed the statement without producing a result set")
        })
    }

    /// Run `sql` with the Extended Query protocol (Parse/Bind/Describe/Execute/
    /// Sync), binding `params` as text-format values where `None` is SQL NULL.
    ///
    /// Exactly one result set, because exactly one portal is executed — the
    /// multi-statement ambiguity [`PgClient::query`] has to resolve cannot
    /// arise, since the extended protocol refuses a multi-statement string
    /// outright.
    ///
    /// A failure here leaves the connection **usable**: `Sync` is always sent,
    /// so the server always answers with `ReadyForQuery`, and this drains to it
    /// before returning the error. That is the guard the module docs describe,
    /// and it is why the error path here is the same code as the success path.
    pub fn query_params(
        &mut self,
        sql: &str,
        params: &[Option<&str>],
    ) -> std::io::Result<PgResult> {
        self.query_params_typed(sql, params, &[])
    }

    /// As [`PgClient::query_params`], declaring each parameter's type OID.
    ///
    /// # Errors
    /// As [`PgClient::query_params`].
    pub fn query_params_typed(
        &mut self,
        sql: &str,
        params: &[Option<&str>],
        oids: &[u32],
    ) -> std::io::Result<PgResult> {
        self.check_usable()?;
        self.notices.clear();
        let msg = encode_extended_query_typed(sql, params, oids)?;
        self.write_all(&msg)?;
        let mut all = self.read_until_ready()?;
        all.pop().ok_or_else(|| {
            protocol_error("the server completed the statement without producing a result set")
        })
    }

    /// Run `sql` and return the row count from its `CommandComplete` tag.
    ///
    /// The count comes from the tag, not from `rows.len()`, so an `UPDATE` or
    /// `DELETE` reports what it changed. Rows are still read and discarded if
    /// the statement produced any — this is for statements that do not.
    pub fn execute(&mut self, sql: &str) -> std::io::Result<u64> {
        Ok(self.query(sql)?.affected_rows())
    }

    /// `BEGIN`, refusing to nest.
    ///
    /// Postgres answers a nested `BEGIN` with a *warning* and leaves the
    /// transaction exactly as it was, so a harness that lost track of its own
    /// nesting would keep running and measure autocommit while believing it was
    /// measuring transactions. Refusing here makes the bookkeeping error an
    /// error.
    pub fn begin(&mut self) -> std::io::Result<()> {
        if self.in_transaction() {
            return Err(protocol_error(
                "BEGIN while already in a transaction; Postgres would warn and ignore it, which \
                 would leave the caller measuring something other than what it thinks",
            ));
        }
        self.execute("BEGIN")?;
        self.expect_status(b'T', "BEGIN")
    }

    /// `COMMIT`.
    ///
    /// Refused outside a transaction for the same reason [`PgClient::begin`]
    /// refuses to nest. Permitted from a *failed* transaction (`E`), where the
    /// server turns it into a rollback — that is Postgres's documented
    /// behaviour and the caller finds out through the resulting `I` status.
    pub fn commit(&mut self) -> std::io::Result<()> {
        if !self.in_transaction() {
            return Err(protocol_error(
                "COMMIT with no transaction in progress; Postgres would warn and ignore it",
            ));
        }
        self.execute("COMMIT")?;
        self.expect_status(b'I', "COMMIT")
    }

    /// `ROLLBACK`. The correct way out of a failed transaction (`E` status).
    pub fn rollback(&mut self) -> std::io::Result<()> {
        if !self.in_transaction() {
            return Err(protocol_error(
                "ROLLBACK with no transaction in progress; Postgres would warn and ignore it",
            ));
        }
        self.execute("ROLLBACK")?;
        self.expect_status(b'I', "ROLLBACK")
    }

    // ── internals ───────────────────────────────────────────────────────────

    fn expect_status(&mut self, want: u8, what: &str) -> std::io::Result<()> {
        if self.tx_status == want {
            return Ok(());
        }
        let got = self.tx_status as char;
        let want = want as char;
        Err(protocol_error(format!(
            "{what} left the connection in transaction status {got:?}, expected {want:?}"
        )))
    }

    fn record_notice(&mut self, payload: &[u8]) {
        if self.notices.len() < MAX_RETAINED_NOTICES {
            self.notices.push(parse_error_response(payload));
        }
    }

    fn check_usable(&self) -> std::io::Result<()> {
        match &self.poisoned {
            None => Ok(()),
            Some(why) => Err(protocol_error(format!(
                "this connection is poisoned and cannot be reused: {why}"
            ))),
        }
    }

    /// Record the reason framing broke and return it as an error, in one step,
    /// so no caller can set one without the other.
    fn poison<T>(&mut self, why: String) -> std::io::Result<T> {
        if self.poisoned.is_none() {
            self.poisoned = Some(why.clone());
        }
        Err(protocol_error(format!("connection poisoned: {why}")))
    }

    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        // A partial or failed write leaves the server's input stream holding
        // half a message, which it will interpret as the start of the next one.
        // There is no recovery, so this poisons.
        if let Err(e) = self.stream.write_all(bytes) {
            let reason = format!("a request was only partly written: {e}");
            if self.poisoned.is_none() {
                self.poisoned = Some(reason);
            }
            return Err(e);
        }
        Ok(())
    }

    /// Read one backend message into `out` (payload only) and return its type
    /// byte. Any failure poisons: a message read that did not complete leaves
    /// the byte stream at an unknown offset.
    fn read_message(&mut self, out: &mut Vec<u8>) -> std::io::Result<u8> {
        match self.read_message_inner(out) {
            Ok(t) => Ok(t),
            Err(e) => {
                if self.poisoned.is_none() {
                    self.poisoned = Some(format!("a message could not be read whole: {e}"));
                }
                Err(e)
            }
        }
    }

    fn read_message_inner(&mut self, out: &mut Vec<u8>) -> std::io::Result<u8> {
        self.fill_at_least(5)?;
        let msg_type = self.buf[self.pos];
        let len = i32::from_be_bytes([
            self.buf[self.pos + 1],
            self.buf[self.pos + 2],
            self.buf[self.pos + 3],
            self.buf[self.pos + 4],
        ]);
        // The length counts itself, so anything below 4 is not a length.
        if len < 4 {
            return Err(protocol_error(format!(
                "message {:?} declared length {len}, which is below the 4-byte minimum",
                msg_type as char
            )));
        }
        let body = len as usize - 4;
        if body > MAX_MESSAGE_BYTES {
            return Err(protocol_error(format!(
                "message {:?} declared a {body}-byte body, above the {MAX_MESSAGE_BYTES}-byte cap; \
                 framing has almost certainly drifted",
                msg_type as char
            )));
        }
        self.pos += 5;
        self.fill_at_least(body)?;
        out.clear();
        out.extend_from_slice(&self.buf[self.pos..self.pos + body]);
        self.pos += body;
        Ok(msg_type)
    }

    /// Ensure at least `n` unconsumed bytes are buffered, reading from the
    /// socket as needed.
    fn fill_at_least(&mut self, n: usize) -> std::io::Result<()> {
        if self.fill - self.pos >= n {
            return Ok(());
        }
        if self.pos > 0 {
            self.buf.copy_within(self.pos..self.fill, 0);
            self.fill -= self.pos;
            self.pos = 0;
        }
        if self.buf.len() < n {
            // Grow to what is needed; a large message is legitimate (a wide row
            // or a long error) and the buffer stays that size afterwards, which
            // is fine for a client whose lifetime is one benchmark run.
            self.buf.resize(n.max(16 * 1024), 0);
        }
        while self.fill < n {
            let got = self.stream.read(&mut self.buf[self.fill..])?;
            if got == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "pgwire: the server closed the connection mid-message",
                ));
            }
            self.fill += got;
        }
        Ok(())
    }

    /// The one read loop both query paths share.
    ///
    /// Shared on purpose. The rule that makes the poisoned-connection guard
    /// work — *always* drain to `ReadyForQuery`, including after an error — is
    /// easy to state and easy to forget at one of two call sites, so there is
    /// one call site. An `ErrorResponse` is remembered and the loop keeps
    /// reading; the error is returned only once `Z` has been consumed and the
    /// connection is provably back in sync.
    fn read_until_ready(&mut self) -> std::io::Result<Vec<PgResult>> {
        let mut results = Vec::new();
        let mut columns: Vec<String> = Vec::new();
        let mut rows: Vec<Vec<Option<String>>> = Vec::new();
        let mut first_error: Option<PgError> = None;
        let mut local_error: Option<std::io::Error> = None;
        let mut payload = Vec::new();

        loop {
            let msg_type = self.read_message(&mut payload)?;
            match msg_type {
                // ── result-set assembly ──────────────────────────────────────
                b'T' => match parse_row_description(&payload) {
                    Ok(c) => {
                        columns = c;
                        rows.clear();
                    }
                    Err(e) => return self.poison(format!("bad RowDescription: {e}")),
                },
                b'D' => match parse_data_row(&payload) {
                    Ok(r) => rows.push(r),
                    Err(e) => return self.poison(format!("bad DataRow: {e}")),
                },
                b'C' => {
                    let tag = match parse_command_complete(&payload) {
                        Ok(t) => t,
                        Err(e) => return self.poison(format!("bad CommandComplete: {e}")),
                    };
                    results.push(PgResult {
                        columns: std::mem::take(&mut columns),
                        rows: std::mem::take(&mut rows),
                        tag,
                    });
                }
                // The empty statement: no tag, no rows. Recorded as a result so
                // `query("")` answers rather than reporting "no result set",
                // which would read as a client bug.
                b'I' => {
                    columns.clear();
                    rows.clear();
                    results.push(PgResult::default());
                }

                // ── extended-query bookkeeping ───────────────────────────────
                // ParseComplete, BindComplete, CloseComplete, NoData,
                // ParameterDescription. Nothing to extract: the portal Describe
                // gives the columns and the rest is acknowledgement.
                b'1' | b'2' | b'3' | b'n' | b't' => {}
                b's' => {
                    // PortalSuspended. Unreachable while Execute sends
                    // max_rows = 0, so reaching it means the encoder changed and
                    // the result in hand is a PREFIX of the real one. Returning
                    // it would be a silently short answer, which is the exact
                    // class of wrong number this module exists to prevent.
                    local_error.get_or_insert_with(|| {
                        protocol_error(
                            "the server suspended the portal, so this result is only a prefix; \
                             Execute must request unlimited rows (max_rows = 0)",
                        )
                    });
                }

                // ── session state ────────────────────────────────────────────
                b'S' => {
                    // ParameterStatus can arrive at any time (a SET changes it).
                    if let Some(v) = parameter_status_value(&payload, b"server_version") {
                        self.server_version = v;
                    }
                }
                b'K' => {}
                b'N' => self.record_notice(&payload),
                // NotificationResponse from LISTEN/NOTIFY. Nothing in the
                // harness listens; dropped rather than treated as a framing
                // error, because an unrelated session can cause one.
                b'A' => {}

                // ── the failure that does NOT poison ─────────────────────────
                b'E' => {
                    let e = parse_error_response(&payload);
                    first_error.get_or_insert(e);
                    // Deliberately no early return. The server will send
                    // ReadyForQuery; leaving it in the socket is what poisons a
                    // connection, so keep reading.
                }

                // ── COPY: not supported, but must not hang ───────────────────
                b'G' => {
                    // CopyInResponse. The server is now waiting for CopyData,
                    // and a client that simply returned would hang the *next*
                    // operation forever. CopyFail is the protocol's own escape:
                    // the server answers ErrorResponse then ReadyForQuery, so
                    // the loop below recovers normally.
                    let mut msg = Vec::new();
                    let mut body = Vec::new();
                    body.extend_from_slice(
                        b"pgwire: this client does not implement COPY FROM STDIN",
                    );
                    body.push(0);
                    frame(&mut msg, b'f', &body)?;
                    self.write_all(&msg)?;
                }
                // CopyOutResponse / CopyData / CopyDone. Drained and discarded:
                // the rows exist but this client has no representation for them,
                // so reporting an empty result would be a wrong answer. The
                // error is raised once, here.
                b'H' => {
                    local_error.get_or_insert_with(|| {
                        protocol_error(
                            "the statement started COPY TO STDOUT, which this client does not \
                             decode; use a SELECT so the rows arrive as DataRow messages",
                        )
                    });
                }
                b'd' | b'c' => {}
                b'W' => {
                    return self.poison(
                        "the server started COPY BOTH (streaming replication), which this client \
                         cannot speak and cannot escape"
                            .to_string(),
                    );
                }

                // ── done ─────────────────────────────────────────────────────
                b'Z' => {
                    self.tx_status = *payload
                        .first()
                        .ok_or_else(|| protocol_error("ReadyForQuery carried no status byte"))?;
                    break;
                }

                other => {
                    return self.poison(format!(
                        "unexpected message type {:?}; this client does not implement it and \
                         cannot know how much of the stream it owns",
                        other as char
                    ));
                }
            }
        }

        // Server-reported failure wins over a client-side one: it is the cause,
        // and the client-side complaint is usually a consequence.
        if let Some(e) = first_error {
            return Err(std::io::Error::other(e));
        }
        if let Some(e) = local_error {
            return Err(e);
        }
        Ok(results)
    }
}

impl Drop for PgClient {
    /// Send `Terminate` so the server frees the backend now rather than when it
    /// eventually notices the socket is gone.
    ///
    /// Best effort — a failure here has nowhere to go and nothing depends on
    /// it — but it matters at scale: a concurrency sweep that opens and drops
    /// hundreds of connections while backends linger hits `max_connections` and
    /// reports the refusal as a database limit.
    fn drop(&mut self) {
        if self.poisoned.is_some() {
            return;
        }
        let mut msg = Vec::new();
        if frame(&mut msg, b'X', &[]).is_ok() {
            let _ = self.stream.write_all(&msg);
        }
    }
}
