//! The engine seam: one trait, four engines.
//!
//! # Why a trait rather than a second harness
//!
//! `stress.rs` speaks Bolt, so the mixed-workload measurement covers engram
//! and Neo4j and nothing else. That asymmetry is written into the project's
//! own notes — "the SNB write-mix (`stress`) profiles speak Bolt and are not
//! covered; this is the join workload only", in the Postgres comparator
//! section of `docs/leave-nothing-on-the-floor-plan.md`. The alternative to a
//! trait is another harness per engine, which is how the LSQB battery came to
//! exist three times.
//!
//! # What the trait must carry, and why each part is here
//!
//! A benchmark backend is not a database driver. It needs four things a driver
//! does not bother to separate:
//!
//! - **Row counts without rows.** The throughput path counts rows and discards
//!   them ([`Backend::run`]); the verification path keeps them
//!   ([`Backend::query`]). Measuring the second when you meant the first
//!   measures the harness's allocator.
//! - **Refusal apart from failure.** A constraint violation under
//!   `unique-create` is the profile WORKING — N−1 refusals per value is the
//!   expected answer — and a dropped socket is the server breaking. Collapsing
//!   them lets an engine that refuses everything look healthy. The
//!   classification is per-engine ([`OpError`]) because the evidence is:
//!   engram and Neo4j say it in a message, Postgres says it in a SQLSTATE.
//! - **Integrity reads.** The reconciliation needs a single integer
//!   ([`Backend::scalar`]) and a two-column pair ([`Backend::pair`]) out of a
//!   ONE-STATEMENT snapshot. Two statements are two instants, and a check that
//!   races its own workload cries wolf — which is worse than no check, because
//!   it teaches the reader to discount the one time it is right.
//! - **Transaction control**, so the seam exists before something needs it.
//!
//! # Autocommit is the only mode, and that is a decision
//!
//! `engram_bolt::client::Client` has no BEGIN/COMMIT: it is RUN + PULL, one
//! statement per unit. `PgClient` has explicit transaction control but
//! defaults to autocommit, which is the same unit. So [`TxMode::Autocommit`]
//! is what every measurement recorded so far was taken under, and it is what
//! the converged harness uses. [`Backend::begin`] exists so a future
//! multi-statement profile has a seam rather than a rewrite — and it refuses
//! by default rather than silently doing nothing, because a no-op `begin`
//! would let a profile believe it had a transaction it did not have.

use crate::catalogue::Dialect;

/// A result cell, normalised across wire formats.
///
/// Bolt hands back typed values; Postgres hands back text. Normalising HERE
/// rather than at each call site is what stops "is this an integer" from being
/// answered differently by two backends — the failure that makes a verifier
/// silently stop verifying.
#[derive(Clone, PartialEq, Debug)]
pub enum Cell {
    /// An integer.
    Int(i64),
    /// Anything else, as text.
    Text(String),
    /// SQL / Cypher NULL.
    Null,
}

/// The parameters a statement is BOUND with, by name.
///
/// Named rather than positional because that is what the catalogue declares —
/// `snb-bi.json`'s `parameters_note` is explicit that "a driver for this family
/// BINDS and does not render" — and because the two dialects place them
/// differently: Cypher's text already carries `$person1Id`, Umbra's SQL carries
/// psql's `:person1Id`, and the Postgres wire protocol wants `$1`. One named
/// map upstream, each backend doing its own placement, is the only arrangement
/// where the same parameter cannot mean two things.
///
/// **Binding rather than rendering is a correctness property, not a style.**
/// A rendered parameter is indistinguishable from the statement once it is on
/// the wire, so a date rendered as a bare string silently becomes a string
/// comparison against a temporal column and matches nothing — which is exactly
/// how bi12 and bi16 came to return empty results that read as working queries.
pub type Params = std::collections::BTreeMap<String, engram_cypher::Value>;

/// Substitute supplied `:name` parameters inside one string literal (quotes
/// included), as LDBC's drivers do. Only integer, float, boolean and string
/// values have an unambiguous text form; any other value leaves the
/// placeholder in place, so PostgreSQL still refuses the literal loudly.
fn substitute_in_literal(lit: &str, params: &Params) -> String {
    let b: Vec<char> = lit.chars().collect();
    let mut out = String::with_capacity(lit.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == ':'
            && b.get(i + 1) != Some(&':')
            && (i == 0 || b[i - 1] != ':')
            && matches!(b.get(i + 1), Some(ch) if ch.is_ascii_alphabetic() || *ch == '_')
        {
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == '_') {
                j += 1;
            }
            let name: String = b[i + 1..j].iter().collect();
            let text = match params.get(&name) {
                Some(engram_cypher::Value::Int(n)) => Some(n.to_string()),
                Some(engram_cypher::Value::Float(f)) => Some(f.to_string()),
                Some(engram_cypher::Value::Bool(x)) => Some(x.to_string()),
                Some(engram_cypher::Value::Str(s)) => Some(s.replace('\'', "''")),
                _ => None,
            };
            if let Some(t) = text {
                out.push_str(&t);
                i = j;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Rewrite psql's `:name` placeholders into the wire protocol's `$1..$n`, and
/// return the positional values in the order the rewritten SQL expects them.
///
/// The catalogue carries Umbra's SQL verbatim, and Umbra is driven by psql, so
/// its parameters are `:name`. The Postgres extended-query protocol only knows
/// `$1`. Something has to translate, and doing it here — once, with the same
/// scanner for every family — is what stops each lane inventing its own.
///
/// **Three things in SQL look like a placeholder and are not**, and a rewriter
/// that misses any of them corrupts the statement instead of parameterising it:
///
/// * `::int` — a cast. Two colons, never a parameter, and the second colon must
///   not then be read as the start of one.
/// * `':name'` inside a string literal, `":name"` inside a quoted identifier,
///   and `$tag$ ... $tag$` dollar-quoted bodies. Contents are data, not syntax
///   — with one exception, a string literal naming a SUPPLIED parameter, which
///   is substituted as text the way LDBC's drivers do (`substitute_in_literal`).
/// * `-- :name` and `/* :name */` — comments. LDBC's SQL is heavily commented,
///   and its comments DO mention the parameter names.
///
/// A repeated `:name` binds to the SAME `$k`, which is why the seen-map exists:
/// Postgres is happy to reference one parameter many times, and allocating a
/// fresh index per occurrence would silently need more values than the caller
/// has names.
fn sql_bind(sql: &str, params: &Params) -> Result<(String, Vec<String>, Vec<u32>), String> {
    let b: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len());
    let mut order: Vec<String> = Vec::new();
    let mut seen: std::collections::BTreeMap<String, usize> = Default::default();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            // ── literals and identifiers: copy through verbatim ──────────
            //
            // One exception, inside STRING literals only: a `:name` that names
            // a SUPPLIED parameter is substituted as text. LDBC's BI text for
            // bi17 writes its window as `':delta hour'::interval`, and LDBC's
            // own driver substitutes `:delta` wherever it appears; sent as-is,
            // PostgreSQL rejects the literal (`invalid input syntax for type
            // interval: ":delta hour"`, 2026-09-26). A name nobody supplied
            // stays data, which is what keeps `':notme'` a string.
            '\'' | '"' => {
                let quote = c;
                let mut lit = String::new();
                lit.push(c);
                i += 1;
                while i < b.len() {
                    lit.push(b[i]);
                    // A doubled quote is an escaped quote, not the end.
                    if b[i] == quote {
                        if b.get(i + 1) == Some(&quote) {
                            lit.push(quote);
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                if quote == '\'' {
                    out.push_str(&substitute_in_literal(&lit, params));
                } else {
                    out.push_str(&lit);
                }
            }
            // ── comments ────────────────────────────────────────────────
            '-' if b.get(i + 1) == Some(&'-') => {
                while i < b.len() && b[i] != '\n' {
                    out.push(b[i]);
                    i += 1;
                }
            }
            '/' if b.get(i + 1) == Some(&'*') => {
                out.push('/');
                out.push('*');
                i += 2;
                while i < b.len() {
                    if b[i] == '*' && b.get(i + 1) == Some(&'/') {
                        out.push('*');
                        out.push('/');
                        i += 2;
                        break;
                    }
                    out.push(b[i]);
                    i += 1;
                }
            }
            // ── a dollar-quoted body: $tag$ ... $tag$ ───────────────────
            //
            // Matched before the cast arm because the tag itself may be empty
            // (`$$ ... $$`). A `$` that does not open a valid tag — `$1`, say,
            // in SQL that is already partly positional — falls through to the
            // default arm and is copied, which is what we want.
            '$' => {
                let mut j = i + 1;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == '_') {
                    j += 1;
                }
                if b.get(j) == Some(&'$') {
                    let tag: String = b[i..=j].iter().collect();
                    out.push_str(&tag);
                    i = j + 1;
                    // Copy until the closing tag, which is the same bytes.
                    let tag_chars: Vec<char> = tag.chars().collect();
                    while i < b.len() {
                        if b[i] == '$' && b[i..].starts_with(tag_chars.as_slice()) {
                            out.push_str(&tag);
                            i += tag_chars.len();
                            break;
                        }
                        out.push(b[i]);
                        i += 1;
                    }
                } else {
                    out.push('$');
                    i += 1;
                }
            }
            // ── a cast, not a placeholder ───────────────────────────────
            ':' if b.get(i + 1) == Some(&':') => {
                out.push(':');
                out.push(':');
                i += 2;
            }
            // ── the real thing ──────────────────────────────────────────
            ':' if matches!(b.get(i + 1), Some(ch) if ch.is_ascii_alphabetic() || *ch == '_') => {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == '_') {
                    j += 1;
                }
                let name: String = b[start..j].iter().collect();
                if !params.contains_key(&name) {
                    return Err(format!(
                        "the statement binds `:{name}`, which is not among the \
                         parameters supplied ({})",
                        params
                            .keys()
                            .map(String::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                let idx = match seen.get(&name) {
                    Some(k) => *k,
                    None => {
                        order.push(name.clone());
                        let k = order.len();
                        seen.insert(name.clone(), k);
                        k
                    }
                };
                // `IN :list` is psql's spelling; the wire protocol has no such
                // form and answers `syntax error at or near "$3"`. The
                // equivalent is `= ANY($n)` over an array — measured on bi12,
                // whose SQL reads `Message.RootPostLanguage IN :languages`.
                //
                // Only rewritten when the VALUE is actually a list: `IN` with
                // a scalar is the caller's business and is left alone.
                let is_list = matches!(params[&name], engram_cypher::Value::List(_));
                let trimmed = out.trim_end();
                let in_form = is_list
                    && (trimmed.ends_with(" IN") || trimmed.ends_with(" in"))
                    && trimmed.len() >= 3;
                if in_form {
                    let keep = trimmed.len() - 2;
                    out.truncate(keep);
                    out.push_str(&format!("= ANY(${idx})"));
                } else {
                    out.push('$');
                    out.push_str(&idx.to_string());
                }
                i = j;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    let values = order
        .iter()
        .map(|n| pg_text(&params[n]))
        .collect::<Result<Vec<_>, _>>()?;
    // DECLARE each parameter's type rather than leaving it to inference.
    //
    // With no OIDs the server infers from context, and the inference is not
    // always the one meant: measured on the SNB BI SQL arm 2026-09-21, bi15
    // and bi20 bound a Person id, Postgres inferred `text`, and the statement
    // failed with `operator does not exist: text = bigint`. The VALUE was
    // right; only the declared type was missing.
    //
    // A 0 means "infer this one", which is the honest answer for a value whose
    // SQL type this layer cannot know.
    let oids = order
        .iter()
        .map(|n| match &params[n] {
            engram_cypher::Value::Int(_) => crate::pgwire::OID_INT8,
            engram_cypher::Value::Float(_) => crate::pgwire::OID_FLOAT8,
            engram_cypher::Value::Str(_) => crate::pgwire::OID_TEXT,
            // A list of strings reaches Postgres as a `text[]` literal and is
            // compared with `= ANY(...)`, so declare the array type rather
            // than leaving `{uz,tk}` to be inferred as a bare string.
            engram_cypher::Value::List(items)
                if items
                    .iter()
                    .all(|i| matches!(i, engram_cypher::Value::Str(_))) =>
            {
                crate::pgwire::OID_TEXT_ARRAY
            }
            _ => 0,
        })
        .collect();
    Ok((out, values, oids))
}

/// One parameter as Postgres text-format input.
///
/// Text format rather than binary because `encode_extended_query` sends zero
/// parameter type OIDs and lets the server infer each type from context — so
/// what matters is that the LITERAL is one Postgres can read for the column it
/// is compared against. A date must therefore arrive as `2012-09-16`, not as
/// the epoch-day integer it is held as internally; comparing an integer to a
/// date column is the silent-empty-result failure this whole lane exists to
/// stop.
fn pg_text(v: &engram_cypher::Value) -> Result<String, String> {
    use engram_cypher::Value as V;
    Ok(match v {
        V::Int(n) => n.to_string(),
        V::Float(f) => f.to_string(),
        V::Str(s) => s.clone(),
        V::Bool(b) => b.to_string(),
        V::Date(days) => days_to_iso(*days),
        V::DateTime {
            epoch_seconds,
            nanos,
            ..
        }
        | V::LocalDateTime {
            epoch_seconds,
            nanos,
        } => secs_to_iso(*epoch_seconds, *nanos),
        // A Postgres array literal. LDBC's bi12 binds a list of language codes.
        V::List(items) => {
            let inner = items
                .iter()
                .map(pg_text)
                .collect::<Result<Vec<_>, _>>()?
                .join(",");
            format!("{{{inner}}}")
        }
        other => {
            return Err(format!(
                "no Postgres text encoding for parameter value {other:?}"
            ));
        }
    })
}

/// Epoch-days to `YYYY-MM-DD`, by the civil-from-days algorithm.
fn days_to_iso(days: i64) -> String {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Epoch-seconds plus nanoseconds to `YYYY-MM-DD HH:MM:SS.mmm`.
///
/// `div_euclid`/`rem_euclid` rather than `/` and `%` so a pre-1970 timestamp
/// floors into the right day instead of truncating toward zero and landing a
/// day late with a negative time of day. SNB's corpus is all post-1970, which
/// is exactly why this would never have been noticed here.
fn secs_to_iso(epoch_seconds: i64, nanos: u32) -> String {
    let days = epoch_seconds.div_euclid(86_400);
    let rem = epoch_seconds.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem / 60) % 60, rem % 60);
    let milli = nanos / 1_000_000;
    format!("{} {h:02}:{m:02}:{s:02}.{milli:03}", days_to_iso(days))
}

impl Cell {
    /// The integer this cell holds, or `None`.
    ///
    /// Postgres returns `42` as the string `"42"`, so a text cell that parses
    /// as an integer IS one. This is not laxity: the alternative is that the
    /// same probe reads as an integer on one engine and as an unreadable row
    /// on another, and the second engine's reconciliation silently never runs.
    #[must_use]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Cell::Int(n) => Some(*n),
            Cell::Text(s) => s.trim().parse().ok(),
            Cell::Null => None,
        }
    }
}

/// What went wrong with one operation.
#[derive(Clone, Debug)]
pub enum OpError {
    /// A correct answer under load: a constraint violation, a budget refusal,
    /// a surfaced OCC conflict. The connection is fine and the engine
    /// published nothing.
    Refusal(String),
    /// The server broke, or the socket did. The connection is dropped and
    /// reopened, and the run FAILS on any of these.
    Transport(String),
}

impl OpError {
    /// Whether this is a refusal.
    #[must_use]
    pub fn is_refusal(&self) -> bool {
        matches!(self, OpError::Refusal(_))
    }

    /// The message, whichever kind it is.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            OpError::Refusal(m) | OpError::Transport(m) => m,
        }
    }
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpError::Refusal(m) => write!(f, "refusal: {m}"),
            OpError::Transport(m) => write!(f, "transport: {m}"),
        }
    }
}

/// The transaction unit a backend is running in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TxMode {
    /// One statement, one unit — what every recorded measurement was taken
    /// under, and the only mode both wire protocols share today.
    Autocommit,
    /// An explicit transaction is open.
    Explicit,
}

/// One engine, as the harness needs it.
pub trait Backend: Send {
    /// The engine's name as it appears in a report — `engram`, `neo4j`,
    /// `postgres`, `ladybug`.
    fn engine(&self) -> &str;

    /// The version string the server announced, or empty. Recorded in every
    /// result document: "which Neo4j" is a question a comparison table has to
    /// be able to answer three months later.
    fn version(&self) -> &str;

    /// Which statement language this backend renders from the catalogue.
    fn dialect(&self) -> Dialect;

    /// What the ENGINE answers about the two fairness knobs it is serving
    /// under — see [`crate::fairness`] for why a claim is not enough.
    ///
    /// The default is the honest one for an engine that cannot be asked:
    /// every figure `declared`, with the reason. A backend overrides it only
    /// where there is a real question to ask, and a backend that CANNOT ask
    /// must never manufacture the number it was passed — an "observation"
    /// that echoes the flag would make the check agree with itself.
    ///
    /// Called on a connection opened FOR the probe and then dropped, never on
    /// a measurement connection: a probe statement the engine rejects is
    /// classified as a transport error by [`BoltBackend`] and drops the
    /// client, so asking on a live session would trade a missing observation
    /// for a broken run.
    fn engine_fairness(&mut self) -> crate::fairness::EngineFairness {
        crate::fairness::EngineFairness::declared(&format!(
            "declared: {} answers no question about its serving configuration",
            self.engine()
        ))
    }

    /// Run a statement, counting rows and discarding them — the throughput
    /// path. The rows are still decoded off the wire, so the protocol is
    /// exercised; only the values are not retained.
    ///
    /// # Errors
    /// [`OpError::Refusal`] for a correct refusal, [`OpError::Transport`] for
    /// a broken connection.
    fn run(&mut self, stmt: &str) -> Result<u64, OpError>;

    /// Run a statement and keep the rows — the verification path.
    ///
    /// # Errors
    /// As [`Backend::run`].
    fn query(&mut self, stmt: &str) -> Result<Vec<Vec<Cell>>, OpError>;

    /// Run a statement with BOUND parameters and keep the rows.
    ///
    /// The three LDBC read batteries — SNB BI, SNB Interactive, FinBench — are
    /// parameterised, and every one of their published queries is defined by
    /// its substitution parameters as much as by its text. A lane that could
    /// only send a literal statement could not run them at all.
    ///
    /// The default forwards an EMPTY map to [`Backend::query`] and otherwise
    /// refuses, for the same reason [`Backend::begin`] refuses: a backend that
    /// cannot bind must say so, rather than let a caller believe a parameter
    /// was applied. Silently dropping a parameter would not fail — it would
    /// run a DIFFERENT query and report its time.
    ///
    /// # Errors
    /// As [`Backend::run`]; plus a refusal when this backend cannot bind.
    fn query_with(&mut self, stmt: &str, params: &Params) -> Result<Vec<Vec<Cell>>, OpError> {
        if params.is_empty() {
            return self.query(stmt);
        }
        Err(OpError::Transport(format!(
            "{} cannot bind parameters; this lane requires binding and will not \
             render them into the statement text",
            self.engine()
        )))
    }

    /// Reopen the connection after a transport error.
    ///
    /// # Errors
    /// [`OpError::Transport`] if the engine is still unreachable.
    fn reconnect(&mut self) -> Result<(), OpError>;

    /// Open an explicit transaction.
    ///
    /// # Errors
    /// The default refuses: a backend that has not implemented transaction
    /// control must say so rather than let a caller believe it has one.
    fn begin(&mut self) -> Result<(), OpError> {
        Err(OpError::Transport(format!(
            "{} has no explicit transaction control; every measurement so far \
             was taken in autocommit and this seam is unimplemented, not absent",
            self.engine()
        )))
    }

    /// Commit an explicit transaction.
    ///
    /// # Errors
    /// As [`Backend::begin`].
    fn commit(&mut self) -> Result<(), OpError> {
        self.begin()
    }

    /// Roll an explicit transaction back.
    ///
    /// # Errors
    /// As [`Backend::begin`].
    fn rollback(&mut self) -> Result<(), OpError> {
        self.begin()
    }

    /// The transaction unit currently in force.
    fn tx_mode(&self) -> TxMode {
        TxMode::Autocommit
    }

    // ── What the integrity reconciliation needs ─────────────────────────────

    /// A single integer out of a one-row, one-column result.
    ///
    /// Loud on every failure mode, deliberately: a verification that silently
    /// skips reads as a PASS, which is exactly the lie the hot-counter check
    /// exists to prevent.
    ///
    /// # Errors
    /// If the statement failed, or the result was not one integer.
    fn scalar(&mut self, stmt: &str) -> Result<i64, OpError> {
        let rows = self.query(stmt)?;
        match rows.as_slice() {
            [row] => match row.as_slice() {
                [c] => c.as_int().ok_or_else(|| {
                    OpError::Transport(format!("{stmt}: value {c:?} is not an integer"))
                }),
                other => Err(OpError::Transport(format!(
                    "{stmt}: expected one column, got {}",
                    other.len()
                ))),
            },
            other => Err(OpError::Transport(format!(
                "{stmt}: expected one row, got {}",
                other.len()
            ))),
        }
    }

    /// The `(bare, bound)` pair out of a one-row, two-column result.
    ///
    /// # Errors
    /// If the statement failed, or the result was not one two-integer row.
    fn pair(&mut self, stmt: &str) -> Result<(u64, u64), OpError> {
        let rows = self.query(stmt)?;
        match rows.as_slice() {
            [row] => match row.as_slice() {
                [a, b] => match (a.as_int(), b.as_int()) {
                    (Some(x), Some(y)) if x >= 0 && y >= 0 => Ok((x as u64, y as u64)),
                    _ => Err(OpError::Transport(format!(
                        "{stmt}: returned an unreadable row {a:?}, {b:?}"
                    ))),
                },
                other => Err(OpError::Transport(format!(
                    "{stmt}: expected two columns, got {}",
                    other.len()
                ))),
            },
            other => Err(OpError::Transport(format!(
                "{stmt}: expected one row, got {}",
                other.len()
            ))),
        }
    }
}

// ─── Bolt: engram and Neo4j ─────────────────────────────────────────────────

/// engram and Neo4j, over `engram_bolt::client::Client`.
///
/// One backend for two engines on purpose: they speak the same protocol and
/// the same dialect, so a difference between them is the engine. The server
/// agent string is the only thing that distinguishes them here, and it is read
/// from the wire rather than passed as a flag somebody has to remember.
pub struct BoltBackend {
    addr: String,
    client: Option<engram_bolt::client::Client>,
    engine: String,
    version: String,
    /// What the peer said it is serving under, off HELLO. `None` from a real
    /// Neo4j and from any engram built before `engram_bolt::serving` existed.
    serving: Option<engram_bolt::ServingHint>,
}

impl BoltBackend {
    /// Connect and identify the peer from its HELLO SUCCESS.
    ///
    /// # Errors
    /// [`OpError::Transport`] if the address cannot be reached.
    pub fn connect(addr: &str) -> Result<BoltBackend, OpError> {
        let client = engram_bolt::client::Client::connect(addr)
            .map_err(|e| OpError::Transport(format!("connect {addr}: {e}")))?;
        let agent = client.server_agent().to_string();
        // `Neo4j/5.26.0`, `engram/0.1.0` — product before the slash.
        let engine = match agent.split('/').next().unwrap_or("") {
            "" => "bolt-unknown".to_string(),
            p => p.to_ascii_lowercase(),
        };
        let serving = client.serving_hint();
        Ok(BoltBackend {
            addr: addr.to_string(),
            client: Some(client),
            engine,
            version: agent,
            serving,
        })
    }

    /// Classify a Bolt error.
    ///
    /// **This substring set is `stress.rs`'s, unchanged.** Widening it would
    /// move errors into the refusal bucket and change every recorded run's
    /// error count; narrowing it would fail runs that used to pass. It is
    /// transcribed rather than improved for exactly that reason.
    fn classify(msg: String) -> OpError {
        if msg.contains("budget")
            || msg.contains("refus")
            || msg.contains("already exists")
            || msg.contains("transaction conflict")
        {
            OpError::Refusal(msg)
        } else {
            OpError::Transport(msg)
        }
    }

    fn conn(&mut self) -> Result<&mut engram_bolt::client::Client, OpError> {
        self.client
            .as_mut()
            .ok_or_else(|| OpError::Transport("connection is closed".to_string()))
    }
}

/// Flatten one decoded Bolt row into cells.
///
/// `Client::query` yields one `Value` per ROW, and a row is a `List` of its
/// columns — the values are one level deeper than they look, and reading them
/// wrong is how a verifier silently stops verifying. A row that is NOT a list
/// is a single unwrapped column, which a future client that unwraps would
/// produce; both shapes are accepted.
fn bolt_row(v: &engram_cypher::Value) -> Vec<Cell> {
    use engram_cypher::Value;
    let cols: Vec<&Value> = match v {
        Value::List(cols) => cols.iter().collect(),
        one => vec![one],
    };
    cols.into_iter()
        .map(|c| match c {
            Value::Int(n) => Cell::Int(*n),
            Value::Null => Cell::Null,
            Value::Str(s) => Cell::Text(s.clone()),
            // A FLOAT MUST KEEP ITS VALUE. The Debug fallback below renders
            // one as `Float(0.123)`, which no consumer can parse back into a
            // number — so every float comparison silently compared two
            // unparseable strings and agreed with neither. Measured
            // 2026-09-21: the Graphalytics epsilon check scored PageRank and
            // LCC at 0/10 with a "worst" error of zero, which is what a
            // comparison looks like when BOTH sides fail to parse and every
            // row is skipped.
            //
            // `{f}` rather than `{f:?}` so the text is a bare number.
            // Infinities and NaN print as `inf`/`NaN`, which is what the
            // Graphalytics comparator expects for an unreachable vertex.
            Value::Float(f) => Cell::Text(format!("{f}")),
            Value::Bool(b) => Cell::Text(b.to_string()),
            // Structural values (lists, maps, nodes, relationships, paths)
            // have no scalar text and keep the Debug form deliberately: a
            // caller that wanted one of those wanted its shape.
            other => Cell::Text(format!("{other:?}")),
        })
        .collect()
}

impl Backend for BoltBackend {
    fn engine(&self) -> &str {
        &self.engine
    }

    fn version(&self) -> &str {
        &self.version
    }

    fn dialect(&self) -> Dialect {
        Dialect::Cypher
    }

    fn run(&mut self, stmt: &str) -> Result<u64, OpError> {
        let r = self.conn()?.run(stmt);
        match r {
            Ok(n) => Ok(n),
            Err(e) => {
                let err = Self::classify(e.to_string());
                if !err.is_refusal() {
                    // Drop it; the caller reconnects. A refusal leaves the
                    // session usable, which is the whole distinction.
                    self.client = None;
                }
                Err(err)
            }
        }
    }

    fn query_with(&mut self, stmt: &str, params: &Params) -> Result<Vec<Vec<Cell>>, OpError> {
        // Cypher's text already carries `$name`, so nothing is rewritten here:
        // the map goes onto the wire as the RUN message's parameter field and
        // the server does the placement. That is the whole point of binding —
        // the statement the engine plans is the catalogue's bytes, unaltered,
        // which is what makes the Neo4j and engram arms the same question.
        let r = self.conn()?.query_with(stmt, params.clone());
        match r {
            Ok(rows) => Ok(rows.iter().map(bolt_row).collect()),
            Err(e) => {
                let err = Self::classify(e.to_string());
                if !err.is_refusal() {
                    self.client = None;
                }
                Err(err)
            }
        }
    }

    fn query(&mut self, stmt: &str) -> Result<Vec<Vec<Cell>>, OpError> {
        let r = self.conn()?.query(stmt);
        match r {
            Ok(rows) => Ok(rows.iter().map(bolt_row).collect()),
            Err(e) => {
                let err = Self::classify(e.to_string());
                if !err.is_refusal() {
                    self.client = None;
                }
                Err(err)
            }
        }
    }

    fn reconnect(&mut self) -> Result<(), OpError> {
        let c = engram_bolt::client::Client::connect(&self.addr)
            .map_err(|e| OpError::Transport(format!("reconnect {}: {e}", self.addr)))?;
        self.version = c.server_agent().to_string();
        self.serving = c.serving_hint();
        self.client = Some(c);
        Ok(())
    }

    /// engram answers in HELLO; Neo4j answers a procedure; anything else on
    /// this wire is declared.
    ///
    /// The two engines share this backend because they share the protocol,
    /// and they are separated here by what the SERVER said it is — the same
    /// rule [`BoltBackend::connect`] already uses for the engine name, rather
    /// than a flag somebody has to remember to pass.
    fn engine_fairness(&mut self) -> crate::fairness::EngineFairness {
        use crate::fairness::{EngineFairness, Figure};
        match self.engine.as_str() {
            "engram" => match self.serving {
                None => EngineFairness::declared(
                    "declared: this engram server sends no serving hint — it predates \
                     `engram_bolt::serving`, so the stamp cannot be checked against it. \
                     Rebuild and redeploy the server binary to make it answerable.",
                ),
                Some(h) => {
                    let workers = h.workers.map_or_else(|| "?".to_string(), |w| w.to_string());
                    EngineFairness {
                        cache_budget_mb: match h.cache_budget_mb {
                            Some(mb) => Figure::observed(
                                mb,
                                format!("engram: HELLO serving hint, --paged-cache-mb {mb}"),
                            ),
                            // An OBSERVATION with no value, not a declaration:
                            // the server answered, and its answer was "there
                            // is no block cache in circuit". A `--data-dir` or
                            // in-memory store is fully resident and bounded
                            // only by the cgroup, so a `--cache-mb` stamped
                            // against one describes a knob that is not there.
                            None => Figure {
                                value: None,
                                provenance: crate::fairness::Provenance::Observed(
                                    "engram: HELLO serving hint says NO block cache is in \
                                     circuit (--data-dir or in-memory; --paged-cache-mb \
                                     applies only under --paged-dir), so the stamped cache \
                                     budget describes no knob on this server"
                                        .to_string(),
                                ),
                            },
                        },
                        thread_cap: match h.thread_cap {
                            Some(w) => Figure::observed(
                                w,
                                format!(
                                    "engram: HELLO serving hint, ENGRAM_QUERY_PARALLELISM \
                                     width {w} (--workers {workers} is the CONNECTION worker \
                                     count and is not this)"
                                ),
                            ),
                            None => Figure::declared(
                                "declared: this engram server sent a hint with no width",
                            ),
                        },
                    }
                }
            },
            "neo4j" => {
                let cache = match self.query(NEO4J_PAGECACHE_PROBE) {
                    Ok(rows) => match rows.first().and_then(|r| r.first()) {
                        Some(cell) => {
                            let raw = match cell {
                                Cell::Text(s) => s.clone(),
                                Cell::Int(n) => n.to_string(),
                                Cell::Null => "null".to_string(),
                            };
                            match crate::fairness::parse_size_mb(&raw) {
                                Some(mb) => Figure::observed(
                                    mb,
                                    format!(
                                        "neo4j: dbms.listConfig(server.memory.pagecache.size) \
                                         = `{raw}`"
                                    ),
                                ),
                                // The probe RAN and this could not read its
                                // answer. Recorded with the answer verbatim,
                                // so a parser that is wrong about this build's
                                // format says so instead of inventing a number.
                                None => Figure {
                                    value: None,
                                    provenance: crate::fairness::Provenance::Observed(format!(
                                        "neo4j: dbms.listConfig(server.memory.pagecache.size) \
                                         answered `{raw}`, which this build cannot parse as a \
                                         size — the figure is unchecked, not agreed"
                                    )),
                                },
                            }
                        }
                        None => Figure::declared(
                            "declared: dbms.listConfig returned no row for \
                             server.memory.pagecache.size",
                        ),
                    },
                    Err(e) => Figure::declared(format!(
                        "declared: dbms.listConfig is unavailable on this server ({e}) — it is \
                         admin-only, so a server with auth enabled will refuse it"
                    )),
                };
                EngineFairness {
                    cache_budget_mb: cache,
                    // NOT a knob Neo4j Community has. The parallel Cypher
                    // runtime is an Enterprise feature; Community executes one
                    // query on one thread, so the intra-query width is 1 and
                    // there is nothing to set it to. The stamp is therefore a
                    // statement about what the CGROUP permits, and this says
                    // so rather than inventing an agreement.
                    thread_cap: Figure::declared(
                        "declared: Neo4j Community has no intra-query parallelism setting — \
                         the parallel runtime is Enterprise-only and one query runs on one \
                         thread, so the stamped cap describes the cgroup's ceiling and not a \
                         width this engine was given",
                    ),
                }
            }
            other => crate::fairness::EngineFairness::declared(&format!(
                "declared: `{other}` on the Bolt wire answers no question about its serving \
                 configuration"
            )),
        }
    }
}

/// Neo4j's own answer for its page cache size.
///
/// `dbms.listConfig` is ADMIN-ONLY. The bench pod runs `NEO4J_AUTH=none`, where
/// every connection has full privileges, so it answers there; a server with
/// auth enabled refuses it and the figure degrades to `declared`, which is the
/// correct outcome and not a failure.
///
/// **This probe has never been run against a live Neo4j.** It was written
/// against 5.26's procedure signature while the Neo4j benchmark pod was not deployed —
/// the bench node was holding another workstream's pods — so the first sweep
/// to use it must READ the recorded source string rather than trust the
/// status. A build whose `value` column renders a size this crate cannot parse
/// records the raw answer and stays UNCHECKED; it cannot silently agree.
const NEO4J_PAGECACHE_PROBE: &str =
    "CALL dbms.listConfig('server.memory.pagecache.size') YIELD value RETURN value";

// ─── Postgres ───────────────────────────────────────────────────────────────

/// What the server says one setting is, or a word that says the read failed.
///
/// Never an empty string and never a plausible default: this value is written
/// into the version string of every result document, so a silent `""` would
/// read as "the setting is unset" and a fabricated `128MB` would read as a
/// measurement. `unreadable` reads as neither.
fn show(client: &mut crate::pgwire::PgClient, setting: &str) -> String {
    match client.query(&format!("SHOW {setting}")) {
        Ok(res) => match res.rows.first().and_then(|r| r.first()) {
            Some(Some(v)) => v.clone(),
            _ => "unreadable".to_string(),
        },
        Err(_) => "unreadable".to_string(),
    }
}

/// PostgreSQL, over the native v3 client in [`crate::pgwire`].
///
/// **Live-verified on 2026-09-09** against PostgreSQL 17.11 on the PostgreSQL benchmark
/// pod: the `synthetic` dataset seeds, every read shape and write op in the
/// ten headline profiles renders and runs, the churn reconciliation balances,
/// and it has been made to FAIL on purpose — see
/// `tests/a_live_postgres_runs_a_stress_level_through_pgbackend.rs` and §9 of
/// `docs/converged-harness.md`. Every `sql` catalogue entry is still
/// `unverified` in the sense the catalogue means: the ANSWERS have not been
/// cross-checked against the Cypher arm's, only the statements' execution.
///
/// # The connection outlives a SQL error, and that is the whole point
///
/// [`crate::pgwire`] draws a line no other backend here has to: a statement
/// that the server REJECTED leaves the connection re-synchronised and
/// reusable, and only broken framing kills it. This backend must respect that
/// line, because the first version did not — it dropped the client on every
/// error the classifier did not call a refusal, including a plain
/// `relation "stress" does not exist`. The control connection is never
/// reconnected inside a level, so one bad statement turned every later
/// integrity probe into `connection is closed`: five reconciliation failures
/// reported, one real cause, and the real cause was the FIRST message rather
/// than the loudest. The client is now dropped when, and only when,
/// [`crate::pgwire::PgClient::is_poisoned`] says framing broke.
pub struct PgBackend {
    addr: String,
    user: String,
    database: String,
    thread_cap: Option<u32>,
    client: Option<crate::pgwire::PgClient>,
    version: String,
    /// `SHOW shared_buffers`, verbatim, as this session's server answered it.
    shared_buffers: String,
    /// `SHOW max_parallel_workers_per_gather`, verbatim. The intra-query width
    /// is this PLUS ONE — see [`PgBackend::connect`].
    workers_per_gather: String,
    /// `SHOW max_parallel_workers`, verbatim — the POOL-wide ceiling.
    ///
    /// Read because `max_parallel_workers_per_gather` is a per-session GUC
    /// with no clamp against it: `SET ... = 7` succeeds and `SHOW` answers 7
    /// on a server whose pool holds 6, so reading the per-gather value alone
    /// would let this session's own `SET` verify itself. The width a query
    /// can actually reach is bounded by both.
    max_parallel_workers: String,
}

impl PgBackend {
    /// Connect to a database, applying `thread_cap` as this session's
    /// intra-query parallelism limit.
    ///
    /// # Why the cap is applied here rather than recorded and forgotten
    ///
    /// [`crate::plan::Fairness`] names `max_parallel_workers_per_gather` as
    /// Postgres's counterpart to engram's `--workers`, and the reporter
    /// REFUSES to build a comparison row out of two runs whose fairness blocks
    /// differ. A backend that stamped `thread_cap: 6` into the document while
    /// the session ran at the server's default would make that refusal
    /// worthless in the one direction it cannot detect: the blocks would
    /// agree, and the machines would not. So the cap is SET on every
    /// connection, and what the server actually reports is written into
    /// [`Backend::version`], which travels into every result document.
    ///
    /// `shared_buffers` is read and reported for the same reason and NOT set,
    /// because it cannot be: it is a postmaster-level setting, so
    /// `--cache-mb` is a claim about the server this connects to rather than
    /// an instruction to it. Reporting the observed value is what lets a
    /// reader see the difference instead of trusting the flag.
    ///
    /// # Errors
    /// [`OpError::Transport`] if the server is unreachable, refuses the
    /// startup exchange, or rejects the cap.
    pub fn connect(
        addr: &str,
        user: &str,
        database: &str,
        thread_cap: Option<u32>,
    ) -> Result<PgBackend, OpError> {
        let mut client = crate::pgwire::PgClient::connect(addr, user, database)
            .map_err(|e| OpError::Transport(format!("connect {addr}/{database}: {e}")))?;
        if let Some(cap) = thread_cap {
            // THE LEADER COUNTS, and it used to be counted twice.
            //
            // `max_parallel_workers_per_gather` is the number of ADDITIONAL
            // worker processes a Gather may start; the leader participates as
            // well (`parallel_leader_participation` is on by default, and is
            // `on` on the bench pod — read there, not assumed). So the width
            // one query may reach is `per_gather + 1`, and setting per_gather
            // to the fairness cap gave Postgres SEVEN processes for a query
            // stamped `thread_cap: 6` — one more than engram's `--workers 6`
            // and LadybugDB's `--threads 6` on the same 6-CPU quota.
            //
            // the PostgreSQL pod's manifest had this right in its own comment all along
            // ("parallel query capped at 6 processes (5 workers + leader =
            // engram's --workers 6)") and set `max_parallel_workers_per_gather
            // = 5`; the harness then overwrote it with 6 on every session.
            // A cap of 1 means the leader alone, which is the honest floor.
            let per_gather = cap.saturating_sub(1);
            client
                .execute(&format!(
                    "SET max_parallel_workers_per_gather = {per_gather}"
                ))
                .map_err(|e| {
                    OpError::Transport(format!(
                        "could not apply the fairness thread cap ({cap} = 1 leader + \
                         {per_gather} workers) to {addr}/{database}: {e} — the run would have \
                         stamped a cap the session did not have"
                    ))
                })?;
        }
        let announced = client.server_version().to_string();
        let buffers = show(&mut client, "shared_buffers");
        let gather = show(&mut client, "max_parallel_workers_per_gather");
        let pool = show(&mut client, "max_parallel_workers");
        let version = format!(
            "PostgreSQL/{announced} (shared_buffers={buffers}, max_parallel_workers_per_gather={gather})"
        );
        Ok(PgBackend {
            addr: addr.to_string(),
            user: user.to_string(),
            database: database.to_string(),
            thread_cap,
            client: Some(client),
            version,
            shared_buffers: buffers,
            workers_per_gather: gather,
            max_parallel_workers: pool,
        })
    }

    /// Classify an error and drop the connection only if framing broke.
    ///
    /// See the type docs: a SQL error is recoverable and the client says so.
    fn after_error(&mut self, e: &std::io::Error) -> OpError {
        let err = Self::classify(e);
        let dead = match &self.client {
            Some(c) => c.is_poisoned(),
            None => true,
        };
        if dead {
            self.client = None;
        }
        err
    }

    /// Classify a Postgres error by SQLSTATE.
    ///
    /// The SQLSTATE is the only field a program can act on, which is why
    /// `pgwire` keeps it rather than flattening the answer to a string. The
    /// mapping is the Bolt one's counterpart, class by class:
    ///
    /// | class | meaning | here |
    /// |---|---|---|
    /// | `23` | integrity constraint violation | refusal — `unique-create` expects N−1 of them |
    /// | `40` | transaction rollback (`40001` serialisation) | refusal — the engine published nothing and says retry |
    /// | `55P03` | lock not available | refusal |
    /// | `53` | insufficient resources | refusal — the budget refusal's analogue |
    /// | anything else | transport | the run fails on it |
    ///
    /// `57014` (statement cancelled) is deliberately NOT a refusal: a
    /// cancelled statement is a measurement that was cut short, and counting
    /// it as a correct answer under load would let a timeout look like a
    /// healthy refusal rate.
    fn classify(err: &std::io::Error) -> OpError {
        let Some(pg) = crate::pgwire::as_pg_error(err) else {
            return OpError::Transport(err.to_string());
        };
        let code = pg.code.as_str();
        let class = code.get(..2).unwrap_or("");
        if class == "23" || class == "40" || class == "53" || code == "55P03" {
            OpError::Refusal(format!("{pg}"))
        } else {
            OpError::Transport(format!("{pg}"))
        }
    }

    fn conn(&mut self) -> Result<&mut crate::pgwire::PgClient, OpError> {
        self.client
            .as_mut()
            .ok_or_else(|| OpError::Transport("connection is closed".to_string()))
    }
}

impl Backend for PgBackend {
    fn engine(&self) -> &str {
        "postgres"
    }

    fn version(&self) -> &str {
        &self.version
    }

    fn dialect(&self) -> Dialect {
        Dialect::Sql
    }

    /// PostgreSQL answers both halves, so both are OBSERVED.
    ///
    /// The cache budget is `shared_buffers`, which is postmaster-level and
    /// therefore genuinely a property of the server this session reached
    /// rather than of anything the harness did. The width is
    /// `1 + max_parallel_workers_per_gather`: the leader participates, and
    /// counting it is the difference between six processes and seven.
    ///
    /// `SET` having been applied by [`PgBackend::connect`] does not make this
    /// circular. The `SET` can be silently clamped — `max_parallel_workers`
    /// and `max_worker_processes` are both pool-wide ceilings a session cannot
    /// raise — so reading the value back is what proves the session got what
    /// the document says it got.
    fn engine_fairness(&mut self) -> crate::fairness::EngineFairness {
        use crate::fairness::{EngineFairness, Figure, Provenance, parse_size_mb};
        let raw_buffers = self.shared_buffers.clone();
        let cache = match parse_size_mb(&raw_buffers) {
            Some(mb) => Figure::observed(
                mb,
                format!("postgres: SHOW shared_buffers = `{raw_buffers}`"),
            ),
            None => Figure {
                value: None,
                provenance: Provenance::Observed(format!(
                    "postgres: SHOW shared_buffers answered `{raw_buffers}`, which this build \
                     cannot parse as a size — unchecked, not agreed"
                )),
            },
        };
        let raw_gather = self.workers_per_gather.clone();
        let raw_pool = self.max_parallel_workers.clone();
        let width = match (
            raw_gather.trim().parse::<u32>(),
            raw_pool.trim().parse::<u32>(),
        ) {
            // The NARROWER of the two ceilings, plus the leader. Taking the
            // per-gather value alone would let this session's own `SET`
            // verify itself: that GUC is per-session and is not clamped
            // against the pool, so `SET ... = 7` succeeds and `SHOW` answers
            // 7 on a server whose pool holds 6. Observed on the PostgreSQL benchmark pod.
            (Ok(g), Ok(pool)) => Figure::observed(
                g.min(pool).saturating_add(1),
                format!(
                    "postgres: 1 leader + min(max_parallel_workers_per_gather {raw_gather}, max_parallel_workers {raw_pool}) (parallel_leader_participation is on by default)"
                ),
            ),
            _ => Figure {
                value: None,
                provenance: Provenance::Observed(format!(
                    "postgres: SHOW max_parallel_workers_per_gather answered `{raw_gather}` and max_parallel_workers answered `{raw_pool}` - unchecked, not agreed"
                )),
            },
        };
        EngineFairness {
            cache_budget_mb: cache,
            thread_cap: width,
        }
    }

    /// # A measurement-fidelity gap, named rather than glossed
    ///
    /// The Bolt arm's `run` counts records as they arrive and decodes none of
    /// them; `PgClient::execute` builds the whole `PgResult` — a `String` per
    /// cell — and then reads the count off the tag. So a Postgres read shape
    /// pays an allocation per returned value that the same shape does not pay
    /// on engram or Neo4j. It is a difference in the HARNESS, not in the
    /// engine, and it is charged to the engine's number. Closing it needs a
    /// row-discarding path inside `pgwire`'s shared read loop; until that
    /// exists, the twenty-row shapes this workload issues make it small, and
    /// small is not zero.
    fn run(&mut self, stmt: &str) -> Result<u64, OpError> {
        // `execute` reports the tag's row count, which is what a write
        // produces; `rows.len()` after an INSERT is zero, and a harness that
        // measured write throughput from it would measure nothing. For a
        // SELECT the two agree.
        let r = self.conn()?.execute(stmt);
        match r {
            Ok(n) => Ok(n),
            Err(e) => Err(self.after_error(&e)),
        }
    }

    fn query(&mut self, stmt: &str) -> Result<Vec<Vec<Cell>>, OpError> {
        let r = self.conn()?.query(stmt);
        match r {
            Ok(res) => Ok(res
                .rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|v| match v {
                            None => Cell::Null,
                            Some(s) => Cell::Text(s.clone()),
                        })
                        .collect()
                })
                .collect()),
            Err(e) => Err(self.after_error(&e)),
        }
    }

    fn query_with(&mut self, stmt: &str, params: &Params) -> Result<Vec<Vec<Cell>>, OpError> {
        if params.is_empty() {
            return self.query(stmt);
        }
        // A rewrite failure is a REFUSAL, not a transport error: the statement
        // and the parameters disagree, the connection is fine, and calling it
        // transport would make the caller reconnect and try the same wrong
        // thing again.
        let (sql, values, oids) = sql_bind(stmt, params).map_err(OpError::Refusal)?;
        let bound: Vec<Option<&str>> = values.iter().map(|s| Some(s.as_str())).collect();
        let r = self.conn()?.query_params_typed(&sql, &bound, &oids);
        match r {
            Ok(res) => Ok(res
                .rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|v| match v {
                            None => Cell::Null,
                            Some(s) => Cell::Text(s.clone()),
                        })
                        .collect()
                })
                .collect()),
            Err(e) => Err(self.after_error(&e)),
        }
    }

    fn reconnect(&mut self) -> Result<(), OpError> {
        // Through `connect`, not `PgClient::connect`, so the reconnected
        // session carries the same fairness cap the original did. A reconnect
        // that quietly reverted to the server default would leave a level's
        // later half running at a different parallelism than its first.
        let fresh = PgBackend::connect(&self.addr, &self.user, &self.database, self.thread_cap)
            .map_err(|e| OpError::Transport(format!("reconnect {}: {e}", self.addr)))?;
        self.version = fresh.version;
        self.client = fresh.client;
        Ok(())
    }

    fn begin(&mut self) -> Result<(), OpError> {
        match self.conn()?.begin() {
            Ok(()) => Ok(()),
            Err(e) => Err(self.after_error(&e)),
        }
    }

    fn commit(&mut self) -> Result<(), OpError> {
        match self.conn()?.commit() {
            Ok(()) => Ok(()),
            Err(e) => Err(self.after_error(&e)),
        }
    }

    fn rollback(&mut self) -> Result<(), OpError> {
        match self.conn()?.rollback() {
            Ok(()) => Ok(()),
            Err(e) => Err(self.after_error(&e)),
        }
    }

    fn tx_mode(&self) -> TxMode {
        match &self.client {
            Some(c) if c.in_transaction() => TxMode::Explicit,
            _ => TxMode::Autocommit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_cell_that_parses_as_an_integer_is_one() {
        // Bolt says Int; Postgres says "42". A reconciliation that read the
        // first and not the second would silently never run on one engine.
        assert_eq!(Cell::Int(42).as_int(), Some(42));
        assert_eq!(Cell::Text("42".to_string()).as_int(), Some(42));
        assert_eq!(Cell::Text(" 42 ".to_string()).as_int(), Some(42));
        assert_eq!(Cell::Text("-1".to_string()).as_int(), Some(-1));
        assert_eq!(Cell::Text("x".to_string()).as_int(), None);
        assert_eq!(Cell::Null.as_int(), None);
    }

    #[test]
    fn the_bolt_classification_is_the_one_stress_shipped() {
        // Transcribed, not improved: widening this moves errors into the
        // refusal bucket and changes every recorded run's error count.
        for refusal in [
            "row budget exceeded",
            "the server refuses",
            "node already exists with that value",
            "transaction conflict, retry",
        ] {
            assert!(
                BoltBackend::classify(refusal.to_string()).is_refusal(),
                "{refusal} must classify as a refusal"
            );
        }
        for transport in ["connection reset by peer", "broken pipe", "eof"] {
            assert!(
                !BoltBackend::classify(transport.to_string()).is_refusal(),
                "{transport} must classify as transport"
            );
        }
    }

    #[test]
    fn a_bolt_row_is_its_columns_and_a_bare_value_is_one_column() {
        use engram_cypher::Value;
        assert_eq!(
            bolt_row(&Value::List((vec![Value::Int(1), Value::Null]).into())),
            vec![Cell::Int(1), Cell::Null]
        );
        assert_eq!(bolt_row(&Value::Int(7)), vec![Cell::Int(7)]);
    }
}

/// `:name` binding, the three lookalikes it must not touch, and the date
/// encodings — the pieces where a silent mistake produces a DIFFERENT query
/// rather than an error.
#[cfg(test)]
mod bind_tests {
    use super::{Params, days_to_iso, pg_text, secs_to_iso, sql_bind};
    use engram_cypher::Value as V;

    fn p(pairs: &[(&str, V)]) -> Params {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn a_float_keeps_its_value_through_the_cell_seam() {
        // THE BUG THIS PINS, measured 2026-09-21. `bolt_row`'s fallback arm
        // rendered a Float with `{:?}`, producing `Float(0.123)` — text no
        // consumer can parse back into a number. The Graphalytics epsilon
        // comparison then compared two unparseable strings, skipped every
        // row, and reported PageRank and LCC as 0/10 with a worst error of
        // ZERO: the signature of a check that never actually ran.
        use super::{Cell, bolt_row};
        use engram_cypher::Value;
        let row = bolt_row(&Value::List(std::sync::Arc::new(vec![
            Value::Float(0.123),
            Value::Float(f64::INFINITY),
            Value::Bool(true),
        ])));
        let text = |c: &Cell| match c {
            Cell::Text(s) => s.clone(),
            other => panic!("expected text, got {other:?}"),
        };
        let first = text(&row[0]);
        assert_eq!(
            first.parse::<f64>().ok(),
            Some(0.123),
            "a float must parse back, got `{first}`"
        );
        // An unreachable vertex under `graphalytics: true` arrives as an
        // infinity, and the comparator matches it as a class.
        assert_eq!(text(&row[1]), "inf");
        assert_eq!(text(&row[2]), "true");
    }

    #[test]
    fn a_named_placeholder_becomes_a_positional_one() {
        let (sql, vals, _oids) = sql_bind(
            "SELECT * FROM person WHERE id = :personId AND lang = :lang",
            &p(&[("personId", V::Int(933)), ("lang", V::Str("uz".into()))]),
        )
        .unwrap();
        // $1 is personId because it appears FIRST, not because it sorts first.
        assert_eq!(sql, "SELECT * FROM person WHERE id = $1 AND lang = $2");
        assert_eq!(vals, vec!["933".to_string(), "uz".to_string()]);
    }

    #[test]
    fn the_same_parameter_twice_binds_to_one_index() {
        let (sql, vals, _oids) = sql_bind(
            "SELECT :a, :b, :a",
            &p(&[("a", V::Int(1)), ("b", V::Int(2))]),
        )
        .unwrap();
        assert_eq!(sql, "SELECT $1, $2, $1");
        // Two names, two values — NOT three. Allocating a fresh index per
        // occurrence would demand a value this caller never had.
        assert_eq!(vals.len(), 2);
    }

    #[test]
    fn an_integer_parameter_declares_its_type_rather_than_leaving_it_inferred() {
        // THE DEFECT THIS PINS, measured on the SNB BI SQL arm 2026-09-21.
        // With no type OIDs Postgres infers each parameter from context, and
        // for bi15's and bi20's Person id it inferred `text` — the statement
        // failed with `operator does not exist: text = bigint`. The value was
        // correct; only the declared type was missing.
        let (_, vals, oids) = sql_bind(
            "SELECT * FROM person WHERE id = :pid AND name = :nm AND score > :sc",
            &p(&[
                ("pid", V::Int(15_393_162_799_074)),
                ("nm", V::Str("x".into())),
                ("sc", V::Float(1.5)),
            ]),
        )
        .unwrap();
        assert_eq!(vals.len(), 3);
        // int8, NOT int4: an LDBC id does not fit in 32 bits.
        assert_eq!(oids[0], crate::pgwire::OID_INT8);
        assert_eq!(oids[1], crate::pgwire::OID_TEXT);
        assert_eq!(oids[2], crate::pgwire::OID_FLOAT8);
    }

    #[test]
    fn an_in_list_becomes_any_because_the_wire_protocol_has_no_in_form() {
        // psql expands `IN :languages` into a literal list; the extended
        // protocol cannot, and answers `syntax error at or near "$3"`.
        // Measured on bi12, whose SQL reads
        // `Message.RootPostLanguage IN :languages`.
        let langs = V::List(std::sync::Arc::new(vec![
            V::Str("uz".into()),
            V::Str("tk".into()),
        ]));
        let (sql, vals, oids) = sql_bind(
            "SELECT * FROM m WHERE m.lang IN :languages",
            &p(&[("languages", langs)]),
        )
        .unwrap();
        assert!(sql.ends_with("m.lang = ANY($1)"), "{sql}");
        assert_eq!(vals[0], "{uz,tk}");
        assert_eq!(oids[0], crate::pgwire::OID_TEXT_ARRAY);
    }

    #[test]
    fn an_in_with_a_scalar_is_left_alone() {
        // Only a LIST is rewritten. `IN` against a scalar is the caller's
        // business and must not be quietly changed.
        let (sql, _, _) =
            sql_bind("SELECT * FROM m WHERE m.x IN :v", &p(&[("v", V::Int(3))])).unwrap();
        assert!(sql.ends_with("m.x IN $1"), "{sql}");
    }

    #[test]
    fn a_value_this_layer_cannot_type_is_left_to_inference() {
        // 0 means "infer this one". A list of STRINGS is typed `text[]`
        // (see the IN/ANY test), but a list this layer cannot describe — one
        // carrying mixed or non-string elements — is left to the server
        // rather than guessed into the wrong array type.
        let mixed = V::List(std::sync::Arc::new(vec![V::Str("uz".into()), V::Int(3)]));
        let (_, _, oids) = sql_bind("SELECT :xs", &p(&[("xs", mixed)])).unwrap();
        assert_eq!(oids[0], 0, "a list this layer cannot type is inferred");

        // And the typed case, stated here too so the pair is visible.
        let strs = V::List(std::sync::Arc::new(vec![V::Str("uz".into())]));
        let (_, _, oids) = sql_bind("SELECT :langs", &p(&[("langs", strs)])).unwrap();
        assert_eq!(oids[0], crate::pgwire::OID_TEXT_ARRAY);
    }

    #[test]
    fn a_cast_is_not_a_placeholder() {
        let (sql, vals, _oids) =
            sql_bind("SELECT x::int, :real", &p(&[("real", V::Int(1))])).unwrap();
        assert_eq!(sql, "SELECT x::int, $1");
        assert_eq!(vals.len(), 1);
    }

    #[test]
    fn a_placeholder_inside_a_literal_or_comment_is_data() {
        let params = p(&[("real", V::Int(1))]);
        // A string literal, a quoted identifier, a line comment and a block
        // comment. LDBC's SQL contains all four, and its comments DO name the
        // parameters.
        let (sql, vals, _oids) = sql_bind(
            "SELECT ':notme', \":alsonot\", :real -- :nope\n/* :neither */",
            &params,
        )
        .unwrap();
        assert_eq!(
            sql,
            "SELECT ':notme', \":alsonot\", $1 -- :nope\n/* :neither */"
        );
        assert_eq!(vals.len(), 1);
    }

    #[test]
    fn a_supplied_placeholder_inside_a_string_literal_is_substituted_as_ldbc_does() {
        // LDBC BI bi17: `':delta hour'::interval`. Sent verbatim, PostgreSQL
        // answered `invalid input syntax for type interval: ":delta hour"`.
        let params = p(&[("delta", V::Int(4)), ("real", V::Int(1))]);
        let (sql, vals, _oids) = sql_bind(
            "SELECT m.d + ':delta hour'::interval, ':notme', :real",
            &params,
        )
        .unwrap();
        assert_eq!(sql, "SELECT m.d + '4 hour'::interval, ':notme', $1");
        assert_eq!(vals.len(), 1);
        // a string value is escaped for its literal context
        let (sql, _, _) = sql_bind(
            "SELECT 'tag :t', :real",
            &p(&[("t", V::Str("o'neil".into())), ("real", V::Int(1))]),
        )
        .unwrap();
        assert_eq!(sql, "SELECT 'tag o''neil', $1");
        // a quoted IDENTIFIER is never substituted
        let (sql, _, _) = sql_bind("SELECT \":delta\", :real", &params).unwrap();
        assert_eq!(sql, "SELECT \":delta\", $1");
    }

    #[test]
    fn a_doubled_quote_does_not_end_the_literal() {
        // 'it''s :notme' is ONE literal. A scanner that ended at the second
        // quote would read `s :notme` as syntax and bind inside a string.
        let (sql, vals, _oids) =
            sql_bind("SELECT 'it''s :notme', :real", &p(&[("real", V::Int(1))])).unwrap();
        assert_eq!(sql, "SELECT 'it''s :notme', $1");
        assert_eq!(vals.len(), 1);
    }

    #[test]
    fn an_unsupplied_placeholder_is_refused_and_names_itself() {
        let e = sql_bind("SELECT :missing", &p(&[("other", V::Int(1))])).unwrap_err();
        assert!(e.contains("missing"), "{e}");
        assert!(e.contains("other"), "{e}");
    }

    #[test]
    fn a_date_binds_as_a_date_literal_not_an_integer() {
        // The bi12/bi16 failure shape: an epoch-day integer compared against a
        // date column matches nothing and reads as a working query.
        assert_eq!(days_to_iso(0), "1970-01-01");
        assert_eq!(days_to_iso(15_599), "2012-09-16");
        assert_eq!(days_to_iso(-1), "1969-12-31");
        assert_eq!(pg_text(&V::Date(15_599)).unwrap(), "2012-09-16");
    }

    #[test]
    fn a_pre_epoch_timestamp_floors_into_the_right_day() {
        // `/` truncates toward zero, so -1s would land on 1970-01-01 at a
        // negative time of day. The corpus is all post-1970, which is exactly
        // why this would never surface in a run.
        assert_eq!(secs_to_iso(0, 0), "1970-01-01 00:00:00.000");
        assert_eq!(secs_to_iso(-1, 0), "1969-12-31 23:59:59.000");
        assert_eq!(
            secs_to_iso(1_347_753_600, 250_000_000),
            "2012-09-16 00:00:00.250"
        );
    }

    #[test]
    fn a_list_binds_as_a_postgres_array_literal() {
        // bi12 binds a list of language codes.
        let v = V::List(std::sync::Arc::new(vec![
            V::Str("uz".into()),
            V::Str("tk".into()),
        ]));
        assert_eq!(pg_text(&v).unwrap(), "{uz,tk}");
    }

    #[test]
    fn a_dollar_quoted_body_is_data() {
        // $$ ... $$ and $tag$ ... $tag$ are string literals whose contents are
        // not syntax. A rewriter that bound inside one would corrupt a
        // function body.
        let params = p(&[("real", V::Int(1))]);
        let (sql, vals, _oids) =
            sql_bind("SELECT $$ :notme $$, $t$ :alsonot $t$, :real", &params).unwrap();
        assert_eq!(sql, "SELECT $$ :notme $$, $t$ :alsonot $t$, $1");
        assert_eq!(vals.len(), 1);
    }

    #[test]
    fn a_bare_dollar_is_copied_not_swallowed() {
        // `$1` does not open a dollar-quoted tag, so it must pass through
        // untouched rather than eat the rest of the statement.
        let (sql, _, _) = sql_bind("SELECT $1, :real", &p(&[("real", V::Int(1))])).unwrap();
        assert_eq!(sql, "SELECT $1, $1");
    }

    #[test]
    fn a_value_with_no_text_encoding_is_refused_rather_than_guessed() {
        let e = pg_text(&V::Null).unwrap_err();
        assert!(e.contains("no Postgres text encoding"), "{e}");
    }
}
