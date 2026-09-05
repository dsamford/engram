//! The sans-io Bolt server: bytes in, bytes out, no socket in sight.
//!
//! The state machine owns everything the protocol REQUIRES and nothing the
//! network provides, so the DST can drive whole sessions deterministically
//! and a TCP adapter is a read/write loop in an out-of-envelope crate (risk
//! C12's boundary, applied to the wire).
//!
//! # The two handshake traps (from reading the driver's source, per plan)
//!
//! 1. The driver's FIRST proposal is `0xFF 0x01` (Manifest v1). It must be
//!    PARSED AND DECLINED — a server treating a 0xFF major as garbage fails
//!    the handshake outright.
//! 2. The driver sends NO plain single 5.x entry — its 5.x proposal is the
//!    RANGE 5.8→5.0. Exact-match negotiation fails against the real driver;
//!    range decoding is mandatory.

use std::collections::BTreeMap;

use engram_cypher::{Value, parse_any};
use engram_graph::{Graph, GraphTxn, QueryResult, RunError, run_stmt};
use engram_key::{Namespace, Realm};
use engram_observe::{counted, sometimes};

use crate::packstream::{Decoder, Pack, PackError, encode_struct, encode_value};

const MAGIC: [u8; 4] = [0x60, 0x60, 0xB0, 0x17];

// Requests.
const MSG_HELLO: u8 = 0x01;
const MSG_GOODBYE: u8 = 0x02;
const MSG_RESET: u8 = 0x0F;
const MSG_RUN: u8 = 0x10;

/// A statement whose execution grows the process's resident set by at least
/// this much is reported by name (see `run`). 32 MB is far above what any
/// point read or hop costs and far below the transient that killed the
/// 12Gi deployment. It was 256 MB: the v106 corpus run climbed the
/// mirror from 8.5 to 24.6 GB across eighty shapes and only ten statements
/// crossed that bar — the climb was statements below it, and the log could
/// not rank them.
const RSS_GROWTH_REPORT_BYTES: usize = 32 << 20;

/// A statement that BEGINS with this comment has its counters dumped exactly
/// as if `ENGRAM_TRACE_COUNTERS` were set — for that statement alone. The
/// lexer skips block comments, so the marker is invisible to the parser and
/// the statement plans and answers as it would without it. It exists because
/// the alternative to diagnosing ONE production shape was a rollout that
/// traced every statement the pod serves (and a second rollout to stop):
/// `/* engram:trace */ MATCH (t:ResearchTask {userId: $u})-[:PROPOSED_GRAPH_WRITE]->(p) …`
/// names, in the pod's log, the counters that statement recorded and the
/// microseconds it took. Nothing is returned to the client; the diagnosis is
/// the operator's, read from the log.
pub const TRACE_MARKER: &str = "/* engram:trace */";

/// The process's resident set in bytes from `/proc/self/statm` — Linux, where
/// the pod runs; `None` elsewhere or when the read fails, and then nothing is
/// reported rather than a guess.
fn rss_bytes() -> Option<usize> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: usize = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}
const MSG_BEGIN: u8 = 0x11;
const MSG_COMMIT: u8 = 0x12;
const MSG_ROLLBACK: u8 = 0x13;
const MSG_DISCARD: u8 = 0x2F;
const MSG_PULL: u8 = 0x3F;
const MSG_ROUTE: u8 = 0x66;
const MSG_LOGON: u8 = 0x6A;
const MSG_LOGOFF: u8 = 0x6B;
const MSG_TELEMETRY: u8 = 0x54;

// Responses.
const MSG_SUCCESS: u8 = 0x70;
const MSG_RECORD: u8 = 0x71;
const MSG_IGNORED: u8 = 0x7E;
const MSG_FAILURE: u8 = 0x7F;

/// A hard wire refusal — the connection is beyond protocol recovery.
#[derive(Debug)]
pub enum WireError {
    /// The preamble was not Bolt. Named specially when it looks like HTTP,
    /// because a misconfigured port should fail LEGIBLY.
    NotBolt {
        /// A human-readable reason.
        detail: String,
    },
    /// No proposed version is servable.
    NoCommonVersion,
    /// PackStream refused.
    Pack(PackError),
    /// A message arrived that no state accepts.
    Protocol(String),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::NotBolt { detail } => write!(f, "not a Bolt connection: {detail}"),
            WireError::NoCommonVersion => write!(f, "no common Bolt version"),
            WireError::Pack(e) => write!(f, "{e}"),
            WireError::Protocol(d) => write!(f, "protocol violation: {d}"),
        }
    }
}

impl std::error::Error for WireError {}

impl From<PackError> for WireError {
    fn from(e: PackError) -> Self {
        WireError::Pack(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Awaiting the 20 handshake bytes.
    Handshake,
    /// The manifest was offered and answered; awaiting the client's chosen
    /// version (4 bytes) and its capability selection (a varint).
    ManifestPending,
    /// Version chosen; awaiting HELLO.
    Negotiated,
    /// HELLO done; 5.1+ awaits LOGON before READY.
    AwaitingLogon,
    /// Open for business.
    Ready,
    /// A FAILURE was sent; everything except RESET (and GOODBYE) is IGNORED.
    Failed,
    /// GOODBYE received.
    Closed,
}

/// One streamed result awaiting PULL/DISCARD.
struct Stream {
    result: QueryResult,
    at: usize,
}

/// The server, one per connection.
/// Resolves a `(realm, namespace)` to the Graph that serves it. The server
/// holds ONE per coordinate over a shared store — federation is a routing
/// choice at HELLO, not a graph per connection. Per-namespace id spaces make
/// these graphs naturally isolated: a node id means nothing outside its own
/// `(realm, namespace)`, so two namespaces never collide.
// `Send + Sync` so one resolver (and the `Arc<Graph>` it hands out) can be shared
// across the server's worker threads — the D2 revision. The Graph is `Send + Sync`
// and its caches are internally latched, so many sessions on many threads share
// ONE graph safely.
pub type GraphResolver = dyn Fn(Realm, Namespace) -> std::sync::Arc<Graph> + Send + Sync;

/// The largest single Bolt message this server will assemble, and the largest
/// unterminated buffer it will hold.
///
/// Bolt bounds a CHUNK at 65535 bytes but places no limit on the total, so this
/// is a policy choice rather than a protocol constant. 64 MiB is far above any
/// legitimate message — the biggest a driver sends is a parameter map, and the
/// biggest this server sends is a RECORD batch it chunks itself — and far below
/// the point where one connection can exhaust a process.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// The Bolt session, one per connection. Bound to a graph coordinate.
pub struct BoltServer {
    /// The graph this session is bound to — the default coordinate until
    /// HELLO declares a namespace, then re-resolved.
    graph: std::sync::Arc<Graph>,
    resolver: std::sync::Arc<GraphResolver>,
    state: State,
    version: (u8, u8),
    inbox: Vec<u8>,
    streams: BTreeMap<i64, Stream>,
    next_qid: i64,
    in_explicit_tx: bool,
    /// See [`MAX_MESSAGE_BYTES`]; overridable per session by the adapter.
    max_message_bytes: usize,
    /// The explicit transaction opened by BEGIN, owned by THIS session and
    /// carried between messages (the worker multiplexes sessions, so it cannot
    /// live in a thread-local across messages). Installed only while a statement
    /// runs. `None` outside an explicit transaction — then statements autocommit.
    txn: Option<GraphTxn>,
    /// The `server` string returned in HELLO's SUCCESS metadata.
    server_agent: String,
    /// This connection's id, supplied by the adapter. See
    /// [`BoltServer::set_connection_id`].
    connection_id: u64,
    /// See [`BoltServer::set_trace_statements`].
    trace_statements: bool,
    /// See [`BoltServer::set_trace_counters`].
    trace_counters: bool,
    /// See [`BoltServer::set_trace_clock`].
    trace_clock: Option<std::sync::Arc<dyn Fn() -> i64 + Send + Sync>>,
    /// Whether the version was negotiated through the Manifest v1 exchange —
    /// then HELLO's SUCCESS carries `protocol_version`, which the spec
    /// reserves for exactly that case.
    manifest: bool,
}

/// The Bolt versions this server offers, in the ORDER it prefers them, as
/// manifest entries `(major, minor, range)` — `range` minors below `minor`
/// are covered too. 6.0 first: its wire deltas over 5.8 are the FAILURE
/// message's stability contract and two new structures (`Vector`,
/// `UnsupportedType`), both handled; Node and Relationship keep their 5.0
/// shape (integer id beside `element_id`) — the spec did not retire the
/// integer id in 6.0, whatever the plan's scoping guessed. 5.0 is the
/// floor: `element_id` arrived there and every structure this server
/// emits carries it.
const OFFERED: [(u8, u8, u8); 2] = [(6, 0, 0), (5, 8, 8)];

/// Whether `(major, minor)` is inside [`OFFERED`].
fn offered(major: u8, minor: u8) -> bool {
    OFFERED
        .iter()
        .any(|&(maj, min, range)| maj == major && minor <= min && minor >= min.saturating_sub(range))
}

/// The Manifest v1 request: `00 00 01 FF` — major `FF`, minor 1.
const MANIFEST_V1: [u8; 4] = [0x00, 0x00, 0x01, 0xFF];

/// Encode a Bolt handshake varint: 7 bits per byte, least significant
/// first, high bit = continuation.
fn push_varint(mut n: u64, out: &mut Vec<u8>) {
    loop {
        let b = (n & 0x7F) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// Decode a handshake varint from the front of `bytes`: `Some((value,
/// consumed))`, or `None` while the continuation bits say more is coming.
fn read_varint(bytes: &[u8]) -> Result<Option<(u64, usize)>, WireError> {
    let mut value: u64 = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if i >= 10 {
            return Err(WireError::Protocol("handshake varint longer than 10 bytes".into()));
        }
        value |= u64::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            return Ok(Some((value, i + 1)));
        }
    }
    Ok(None)
}

/// The GQLSTATUS a `Neo.*` code maps to, with its description and
/// classification — the 5.7 FAILURE fields. By the code's family, not
/// exhaustive: a syntax error is `42001` (GQL's "invalid syntax"); every
/// other failure is `50N42`, the general "unexpected error" status Neo4j
/// itself reports for a failure it has not classified more finely; the
/// classification comes from the code's second segment (`ClientError` /
/// `DatabaseError` / `TransientError`). A driver keys retry behaviour on
/// the classification and shows the status — neither is wrong here, and a
/// finer map is a table to fill as codes are met, not a design.
fn gql_of(code: &str) -> (&'static str, &'static str, &'static str) {
    let class = match code.split('.').nth(1) {
        Some("TransientError") => "TRANSIENT_ERROR",
        Some("DatabaseError") => "DATABASE_ERROR",
        _ => "CLIENT_ERROR",
    };
    if code.ends_with(".SyntaxError") {
        ("42001", "error: syntax error or access rule violation - invalid syntax", class)
    } else {
        ("50N42", "error: general processing exception - unexpected error", class)
    }
}

/// The default `server` agent string.
///
/// It used to be `"Neo4j/5.26.0 (engram)"` — a registered mark used as this
/// product's own wire identity, which the release review called the
/// highest-risk single artifact in the tree. Drivers negotiate on the Bolt
/// protocol version exchanged in the handshake, not on this string, so
/// answering honestly costs nothing a driver depends on.
///
/// It is nevertheless OVERRIDABLE ([`BoltServer::set_server_agent`]), because
/// "no driver reads it" is a claim about drivers we have tested and the
/// compatibility matrix that ships with 0.1 is deliberately an open one. An
/// operator who finds a client that does read it needs a lever, not a patch.
pub const DEFAULT_SERVER_AGENT: &str = concat!("engram/", env!("CARGO_PKG_VERSION"));

impl BoltServer {
    /// A server over a graph.
    pub fn new(graph: Graph) -> BoltServer {
        BoltServer::shared(std::sync::Arc::new(graph))
    }

    /// A session that ROUTES to a per-`(realm, namespace)` graph declared in
    /// HELLO (`namespace`/`realm` extras). Until HELLO, and for a client that
    /// declares nothing, it serves `default_realm`/`default_ns` — so an
    /// un-namespaced driver is unchanged. The resolver owns the one-graph-
    /// per-coordinate cache; this session only asks it.
    pub fn routed(
        resolver: std::sync::Arc<GraphResolver>,
        default_realm: Realm,
        default_ns: Namespace,
    ) -> BoltServer {
        let graph = resolver(default_realm, default_ns);
        BoltServer {
            graph,
            resolver,
            state: State::Handshake,
            version: (0, 0),
            inbox: Vec::new(),
            streams: BTreeMap::new(),
            next_qid: 0,
            in_explicit_tx: false,
            txn: None,
            max_message_bytes: MAX_MESSAGE_BYTES,
            server_agent: DEFAULT_SERVER_AGENT.to_string(),
            connection_id: 0,
            trace_statements: false,
            trace_counters: false,
            trace_clock: None,
            manifest: false,
        }
    }

    /// Override the assembled-message cap for this session.
    ///
    /// The default is [`MAX_MESSAGE_BYTES`]; the adapter sets it from operator
    /// configuration. It is a setter rather than a constructor parameter so the
    /// four existing constructors keep their signatures — a limit is a policy,
    /// and policies belong on the config path, not in every call site.
    pub fn set_max_message_bytes(&mut self, n: usize) {
        self.max_message_bytes = n;
    }

    /// Override the `server` agent string this session reports in HELLO.
    ///
    /// See [`DEFAULT_SERVER_AGENT`] for why the default no longer carries
    /// another vendor's mark, and why the override exists anyway.
    pub fn set_server_agent(&mut self, agent: impl Into<String>) {
        self.server_agent = agent.into();
    }

    /// Set this connection's id, reported to the driver as `connection_id`.
    ///
    /// Supplied by the ADAPTER, which is the layer that knows what a connection
    /// is. Taking it from a process-wide counter inside the protocol machine
    /// would put ambient mutable state in the one crate the simulation drives
    /// deterministically — the id would then depend on how many sessions a test
    /// happened to open before this one.
    pub fn set_connection_id(&mut self, id: u64) {
        self.connection_id = id;
    }

    /// Print every statement this session runs to stderr, as it is received —
    /// the adapter's diagnostic switch (`ENGRAM_TRACE_STATEMENTS`). Off by
    /// default; the last line before a stall names the statement that stalled.
    pub fn set_trace_statements(&mut self, on: bool) {
        self.trace_statements = on;
    }

    /// Print every counter a statement records, biggest first.
    ///
    /// The attribution the throughput number cannot give: `ic6-friend-tags` is
    /// 51% of read time at a ~100 ms median and the A/B that removed its label
    /// materialisation moved it 4%, which says the cost is somewhere this
    /// surface names and reasoning did not. Installing a trace makes every
    /// `counted!` in the engine a real map write, so this is a diagnostic and
    /// costs what a diagnostic costs — it is off unless
    /// `ENGRAM_TRACE_COUNTERS` is set.
    pub fn set_trace_counters(&mut self, on: bool) {
        self.trace_counters = on;
    }

    /// The clock the trace header's `(wall)` figure is read from, in
    /// MICROseconds from any fixed origin — supplied by the adapter, which is
    /// the layer allowed to read a real clock.
    ///
    /// Without it the header reads the graph's injected wall clock, which the
    /// adapter stamps ONCE per message batch: t0 and t1 inside one statement
    /// are then the same value and every statement printed `0 ms (wall)` —
    /// the rare non-zero figure was a concurrent connection's stamp landing
    /// mid-statement. The header looked like a statement timer and was not,
    /// so a `{status: "pending"}` count that cost the client 4.8 ms against
    /// a probe of three ids could not be split between the engine and
    /// everything around it. The simulation sets no clock and keeps the
    /// injected one, so a traced run there stays reproducible.
    pub fn set_trace_clock(&mut self, clock: std::sync::Arc<dyn Fn() -> i64 + Send + Sync>) {
        self.trace_clock = Some(clock);
    }

    /// Microseconds now, from the adapter's clock when it supplied one, else
    /// from the graph's injected millisecond wall clock.
    fn trace_now_us(&self, graph: &Graph) -> Option<i64> {
        match &self.trace_clock {
            Some(clock) => Some(clock()),
            None => graph.wall_ms().map(|ms| ms.saturating_mul(1000)),
        }
    }

    /// [`run_stmt`], with the statement's counters dumped when `trace` is on
    /// — [`BoltServer::set_trace_counters`] for every statement, or the
    /// [`TRACE_MARKER`] for this one. `query` is the statement's text, shown
    /// at the head of the dump so a log holding several traces reads.
    fn run_traced(
        &self,
        graph: &Graph,
        parsed: &engram_cypher::Stmt,
        params: BTreeMap<String, Value>,
        trace: bool,
        query: &str,
    ) -> Result<QueryResult, RunError> {
        if !trace {
            return run_stmt(graph, parsed, params);
        }
        // Time from the adapter's clock when it supplied one (see
        // `set_trace_clock`), else the graph's INJECTED wall clock — never a
        // direct `Instant` read here: the simulation owns time, and a clock
        // it cannot see makes a run unreproducible (clippy.toml disallows the
        // direct read for exactly that reason).
        let t0 = self.trace_now_us(graph);
        let (out, trace) = engram_observe::with_trace(|| run_stmt(graph, parsed, params));
        let elapsed_ms = match (t0, self.trace_now_us(graph)) {
            (Some(a), Some(b)) => b.saturating_sub(a) as f64 / 1000.0,
            _ => -1.0,
        };
        let mut rows: Vec<(u64, &str)> = trace
            .counters()
            .iter()
            .map(|(k, v)| (*v, k.as_str()))
            .collect();
        rows.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        let shown: String = query.chars().take(200).collect();
        eprintln!(
            "[trace-counters] conn {} — {} distinct, {:.3} ms (wall): {}",
            self.connection_id,
            rows.len(),
            elapsed_ms,
            shown.replace('\n', " ")
        );
        for (v, k) in rows.iter().take(60) {
            eprintln!("[trace-counters]   {v:>14}  {k}");
        }
        // THE EVENTS TOO, aggregated by (tag, name). A `sometimes!`/`always!`
        // records an event and no counter, so a site that fires per row is
        // INVISIBLE above while costing a `String` allocation per firing under
        // the trace. LSQB q2 took 35 s traced for the same 5.09M counted
        // operations that took its single-path spelling 3 s, and this is the
        // only instrument that can name the site doing that.
        let mut by_site: BTreeMap<(String, &str), u64> = BTreeMap::new();
        for e in trace.events() {
            *by_site
                .entry((format!("{:?}", e.tag), e.name.as_str()))
                .or_insert(0) += 1;
        }
        let mut sites: Vec<(u64, (String, &str))> =
            by_site.into_iter().map(|(k, v)| (v, k)).collect();
        sites.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        eprintln!(
            "[trace-events] conn {} — {} events, {} distinct sites",
            self.connection_id,
            trace.events().len(),
            sites.len()
        );
        for (v, (tag, name)) in sites.iter().take(20) {
            eprintln!("[trace-events]   {v:>14}  {tag:<16} {name}");
        }
        out
    }

    /// A session over a Graph SHARED with other sessions on the same
    /// thread. A server's label memberships, count store, vector indexes,
    /// degree tables and memo caches live in the Graph, not the store: a
    /// Graph per connection rebuilt every one of them per connection (18 s
    /// of membership rebuild on a new connection's first count, 115 s of
    /// HNSW on its first vector query, on the production export), and an
    /// index created on one connection did not exist on the next.
    pub fn shared(graph: std::sync::Arc<Graph>) -> BoltServer {
        let constant = std::sync::Arc::clone(&graph);
        BoltServer {
            graph,
            resolver: std::sync::Arc::new(move |_, _| std::sync::Arc::clone(&constant)),
            state: State::Handshake,
            version: (0, 0),
            inbox: Vec::new(),
            streams: BTreeMap::new(),
            next_qid: 0,
            in_explicit_tx: false,
            txn: None,
            max_message_bytes: MAX_MESSAGE_BYTES,
            server_agent: DEFAULT_SERVER_AGENT.to_string(),
            connection_id: 0,
            trace_statements: false,
            trace_counters: false,
            trace_clock: None,
            manifest: false,
        }
    }

    /// The negotiated (major, minor), once past the handshake.
    pub fn version(&self) -> (u8, u8) {
        self.version
    }

    /// Whether the connection is closed.
    pub fn closed(&self) -> bool {
        self.state == State::Closed
    }

    /// The graph this session runs against — the adapter injects the wall
    /// clock through it.
    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    /// Feed bytes from the peer; returns bytes to send back.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<u8>, WireError> {
        self.inbox.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            match self.state {
                State::Handshake => {
                    // A wrong magic is refusable at FOUR bytes — waiting for
                    // the full 20 would leave a misdirected HTTP client (or
                    // anything else) hanging instead of failing legibly.
                    if self.inbox.len() >= 4 && self.inbox[..4] != MAGIC {
                        return Err(self.refuse_preamble());
                    }
                    if self.inbox.len() < 20 {
                        return Ok(out);
                    }
                    let reply = self.handshake()?;
                    out.extend_from_slice(&reply);
                }
                State::ManifestPending => {
                    // The client's final part: its chosen version (4.0 format,
                    // no range) and the capability bits it selected. It may
                    // pipeline HELLO behind this, so consume exactly what the
                    // handshake owns and let the loop go on to messages.
                    if self.inbox.len() < 5 {
                        return Ok(out);
                    }
                    let Some((caps, used)) = read_varint(&self.inbox[4..])? else {
                        return Ok(out);
                    };
                    let (minor, major) = (self.inbox[2], self.inbox[3]);
                    self.inbox.drain(..4 + used);
                    if !offered(major, minor) {
                        sometimes!("bolt.refused an unoffered manifest pick", true);
                        self.state = State::Closed;
                        return Err(WireError::NoCommonVersion);
                    }
                    if caps != 0 {
                        // No capability was offered, so none can be selected.
                        self.state = State::Closed;
                        return Err(WireError::Protocol(format!(
                            "client selected capability bits {caps:#x} that were not offered"
                        )));
                    }
                    self.version = (major, minor);
                    self.manifest = true;
                    self.state = State::Negotiated;
                    counted!("bolt.sessions negotiated");
                    counted!("bolt.sessions negotiated through the manifest");
                }
                State::Closed => return Ok(out),
                _ => {
                    let Some(message) = self.next_message()? else {
                        return Ok(out);
                    };
                    if message.is_empty() {
                        continue; // a NOOP keep-alive chunk — tolerated.
                    }
                    let mut d = Decoder::new(&message);
                    let pack = d.decode()?;
                    let Pack::Struct { tag, fields } = pack else {
                        return Err(WireError::Protocol("a message must be a structure".into()));
                    };
                    self.handle(tag, fields, &mut out)?;
                }
            }
        }
    }

    fn refuse_preamble(&mut self) -> WireError {
        let head = &self.inbox[..4];
        let detail = if head.starts_with(b"GET ")
            || head.starts_with(b"POST")
            || head.starts_with(b"HTTP")
        {
            "an HTTP request arrived on the Bolt port".to_string()
        } else {
            format!("bad magic {head:02X?}")
        };
        sometimes!("bolt.refused a non-bolt preamble", true);
        self.state = State::Closed;
        WireError::NotBolt { detail }
    }

    fn handshake(&mut self) -> Result<Vec<u8>, WireError> {
        // Manifest v1 first, wherever the client put it: it hands the client
        // the WHOLE offer, so a client that speaks it always ends up on the
        // best common version rather than the first range that overlapped.
        // The reply is `00 00 01 FF`, a varint count, that many 4.3-format
        // entries (`00 range minor major`), and a varint capability mask —
        // zero: this server amends nothing. The exchange then waits for the
        // client's pick (`State::ManifestPending`).
        let proposals: Vec<[u8; 4]> = (0..4)
            .map(|i| self.inbox[4 + i * 4..8 + i * 4].try_into().expect("4"))
            .collect();
        if proposals.contains(&MANIFEST_V1) {
            self.inbox.drain(..20);
            let mut reply = MANIFEST_V1.to_vec();
            push_varint(OFFERED.len() as u64, &mut reply);
            for &(major, minor, range) in &OFFERED {
                reply.extend_from_slice(&[0, range, minor, major]);
            }
            push_varint(0, &mut reply);
            self.state = State::ManifestPending;
            counted!("bolt.manifest offered");
            return Ok(reply);
        }
        // Legacy: four proposals in preference order, each a 4.3-format range;
        // serve the highest version we speak that the first overlapping
        // range covers. A manifest minor other than 1 (major `FF`) is a
        // scheme we do not speak — parsed and passed over, never garbage.
        let mut chosen: Option<(u8, u8)> = None;
        for p in &proposals {
            let (range, minor, major) = (p[1], p[2], p[3]);
            if major == 0xFF {
                sometimes!("bolt.declined an unknown manifest version", true);
                continue;
            }
            let low = minor.saturating_sub(range);
            let pick = OFFERED
                .iter()
                .filter(|&&(maj, _, _)| maj == major)
                .map(|&(_, ours, _)| ours.min(minor))
                .find(|&pick| pick >= low);
            if let Some(pick) = pick {
                chosen = Some((major, pick));
                break;
            }
        }
        self.inbox.drain(..20);
        match chosen {
            Some((major, minor)) => {
                self.version = (major, minor);
                self.state = State::Negotiated;
                counted!("bolt.sessions negotiated");
                Ok(vec![0, 0, minor, major])
            }
            None => {
                self.state = State::Closed;
                // The protocol's refusal: four zero bytes, then close.
                Err(WireError::NoCommonVersion)
            }
        }
    }

    /// Extract one complete chunked message (an empty vec is a NOOP).
    ///
    /// Bolt bounds a CHUNK at 65535 bytes and says nothing about the total, so
    /// an unbounded assembler lets a client stream non-terminating chunks for
    /// ever. Two things then grow without limit: `payload`, and — because an
    /// incomplete message returns `Ok(None)` WITHOUT draining — `self.inbox`
    /// itself. One connection could exhaust the process, which is a denial of
    /// service against every other connection too.
    ///
    /// Both are now bounded by [`MAX_MESSAGE_BYTES`], and exceeding it is a
    /// terminal protocol error rather than a silent stall: a client that has
    /// sent 64 MiB without a terminator is not going to finish.
    fn next_message(&mut self) -> Result<Option<Vec<u8>>, WireError> {
        let mut at = 0usize;
        let mut payload = Vec::new();
        loop {
            let Some(header) = self.inbox.get(at..at + 2) else {
                // Incomplete: nothing is drained, so the buffer must be capped
                // here or a client that never terminates a message grows it
                // until the allocator gives up.
                if self.inbox.len() > self.max_message_bytes {
                    return Err(WireError::Protocol(format!(
                        "unterminated message larger than {} bytes",
                        self.max_message_bytes
                    )));
                }
                return Ok(None);
            };
            let size = u16::from_be_bytes(header.try_into().expect("2")) as usize;
            at += 2;
            if size == 0 {
                self.inbox.drain(..at);
                return Ok(Some(payload));
            }
            if payload.len() + size > self.max_message_bytes {
                sometimes!("bolt.refused an oversized message", true);
                return Err(WireError::Protocol(format!(
                    "message larger than {} bytes",
                    self.max_message_bytes
                )));
            }
            let Some(chunk) = self.inbox.get(at..at + size) else {
                if self.inbox.len() > self.max_message_bytes {
                    return Err(WireError::Protocol(format!(
                        "unterminated message larger than {} bytes",
                        self.max_message_bytes
                    )));
                }
                return Ok(None);
            };
            payload.extend_from_slice(chunk);
            at += size;
        }
    }

    fn handle(&mut self, tag: u8, fields: Vec<Pack>, out: &mut Vec<u8>) -> Result<(), WireError> {
        // GOODBYE is honoured in every state.
        if tag == MSG_GOODBYE {
            self.state = State::Closed;
            return Ok(());
        }
        // The failure protocol: after a FAILURE, everything except RESET is
        // answered IGNORED until the driver resets.
        if self.state == State::Failed && tag != MSG_RESET {
            sometimes!("bolt.ignored a message while failed", true);
            self.send(out, MSG_IGNORED, vec![])?;
            return Ok(());
        }
        match (self.state, tag) {
            (State::Negotiated, MSG_HELLO) => {
                // Federation routing: a `namespace` (and optional `realm`)
                // extra binds this session to that coordinate's graph. The
                // resolver returns the SAME graph for the same coordinate, so
                // two sessions on one namespace still share its caches.
                if let Some(Ok(Value::Map(extras))) = fields.first().map(|p| p.clone().into_value())
                {
                    let coord = |k: &str| -> Option<u32> {
                        match extras.get(k) {
                            Some(Value::Int(n)) if *n >= 0 && *n <= u32::MAX as i64 => {
                                Some(*n as u32)
                            }
                            _ => None,
                        }
                    };
                    if let Some(ns) = coord("namespace") {
                        let realm = coord("realm").unwrap_or(self.graph.realm().0);
                        self.graph = (self.resolver)(Realm(realm), Namespace(ns));
                        counted!("bolt.session routed to a namespace");
                    }
                }
                let mut meta = BTreeMap::new();
                meta.insert(
                    "server".to_string(),
                    Value::Str(self.server_agent.clone()),
                );
                // A UNIQUE id per connection.
                //
                // It was the constant `"bolt-0"`, which makes every connection
                // indistinguishable in a driver's logs, in a server log line,
                // and in any future `listConnections`. An identifier that is the
                // same for everyone is not an identifier; it is a label that
                // looks like one, and it silently defeats the first thing anyone
                // does when diagnosing a production incident.
                meta.insert(
                    "connection_id".to_string(),
                    Value::Str(format!("bolt-{}", self.connection_id)),
                );
                // 5.7+: the negotiated version, present ONLY when the
                // manifest negotiated it (the spec's condition, kept exactly:
                // a legacy client did not ask and is not told).
                if self.manifest {
                    meta.insert(
                        "protocol_version".to_string(),
                        Value::Str(format!("{}.{}", self.version.0, self.version.1)),
                    );
                }
                self.state = if self.version >= (5, 1) {
                    State::AwaitingLogon
                } else {
                    State::Ready
                };
                self.send(out, MSG_SUCCESS, vec![Pack::Value(Value::Map(meta))])
            }
            (State::AwaitingLogon, MSG_LOGON) => {
                // Credentials are accepted, not yet verified — the auth
                // backend is deliberately out of this slice. The FLOW is
                // enforced (RUN before LOGON refuses), which is the part the
                // driver depends on.
                self.state = State::Ready;
                self.send(
                    out,
                    MSG_SUCCESS,
                    vec![Pack::Value(Value::Map(BTreeMap::new()))],
                )
            }
            (State::Ready, MSG_LOGOFF) => {
                self.state = State::AwaitingLogon;
                self.send(
                    out,
                    MSG_SUCCESS,
                    vec![Pack::Value(Value::Map(BTreeMap::new()))],
                )
            }
            (State::Ready, MSG_RESET) | (State::Failed, MSG_RESET) => {
                self.state = State::Ready;
                self.streams.clear();
                self.in_explicit_tx = false;
                // RESET aborts any in-flight transaction — its buffered writes
                // are discarded (never published), and the next BEGIN starts
                // clean rather than tripping "a transaction is already open".
                if let Some(txn) = self.txn.take() {
                    self.graph.rollback_owned(txn);
                }
                self.send(
                    out,
                    MSG_SUCCESS,
                    vec![Pack::Value(Value::Map(BTreeMap::new()))],
                )
            }
            (State::Ready, MSG_RUN) => self.run(fields, out),
            (State::Ready, MSG_PULL) => self.pull(fields, out, true),
            (State::Ready, MSG_DISCARD) => self.pull(fields, out, false),
            (State::Ready, MSG_BEGIN) => {
                if self.txn.is_some() {
                    return self.fail(
                        out,
                        "Neo.ClientError.Transaction.TransactionStartFailed",
                        "a transaction is already open on this session",
                    );
                }
                // A real, session-owned transaction: statements RUN inside it,
                // COMMIT publishes its whole write-set atomically, and ROLLBACK
                // (or a dropped session) discards it.
                self.in_explicit_tx = true;
                self.txn = Some(self.graph.open_txn());
                sometimes!("bolt.explicit transaction opened", true);
                let mut meta = BTreeMap::new();
                self.home_db(&mut meta);
                self.send(out, MSG_SUCCESS, vec![Pack::Value(Value::Map(meta))])
            }
            (State::Ready, MSG_COMMIT) => {
                self.in_explicit_tx = false;
                if let Some(txn) = self.txn.take() {
                    if let Err(e) = self.graph.commit_owned(txn) {
                        // On an OCC conflict NOTHING published — the client
                        // retries the whole transaction. Reported by name.
                        sometimes!("bolt.commit failed", true);
                        return self.fail(
                            out,
                            "Neo.ClientError.Transaction.TransactionCommitFailed",
                            &format!("{e}"),
                        );
                    }
                }
                let mut meta = BTreeMap::new();
                meta.insert("bookmark".to_string(), Value::Str(self.bookmark()));
                self.send(out, MSG_SUCCESS, vec![Pack::Value(Value::Map(meta))])
            }
            (State::Ready, MSG_ROLLBACK) => {
                // Now genuinely supported: an explicit transaction's buffered
                // writes were never published, so discarding it undoes them.
                self.in_explicit_tx = false;
                if let Some(txn) = self.txn.take() {
                    self.graph.rollback_owned(txn);
                    sometimes!("bolt.rolled back a transaction", true);
                }
                self.send(
                    out,
                    MSG_SUCCESS,
                    vec![Pack::Value(Value::Map(BTreeMap::new()))],
                )
            }
            (State::Ready, MSG_ROUTE) => {
                // The single-server stub: self is router, reader and writer.
                let mut rt = BTreeMap::new();
                rt.insert("ttl".to_string(), Value::Int(300));
                rt.insert("db".to_string(), Value::Str("neo4j".to_string()));
                let server = |role: &str| {
                    let mut m = BTreeMap::new();
                    m.insert(
                        "addresses".to_string(),
                        Value::List(vec![Value::Str("localhost:7687".to_string())]),
                    );
                    m.insert("role".to_string(), Value::Str(role.to_string()));
                    Value::Map(m)
                };
                rt.insert(
                    "servers".to_string(),
                    Value::List(vec![server("ROUTE"), server("READ"), server("WRITE")]),
                );
                let mut meta = BTreeMap::new();
                meta.insert("rt".to_string(), Value::Map(rt));
                self.send(out, MSG_SUCCESS, vec![Pack::Value(Value::Map(meta))])
            }
            (State::Ready, MSG_TELEMETRY) => self.send(
                out,
                MSG_SUCCESS,
                vec![Pack::Value(Value::Map(BTreeMap::new()))],
            ),
            (state, tag) => Err(WireError::Protocol(format!(
                "message 0x{tag:02X} is not valid in {state:?}"
            ))),
        }
    }

    fn run(&mut self, fields: Vec<Pack>, out: &mut Vec<u8>) -> Result<(), WireError> {
        let mut it = fields.into_iter();
        let query = match it.next().map(|p| p.into_value()) {
            Some(Ok(Value::Str(s))) => s,
            _ => return Err(WireError::Protocol("RUN needs a query string".into())),
        };
        let params = match it.next().map(crate::packstream::decode_value) {
            None => BTreeMap::new(),
            Some(Ok(Value::Map(m))) => m,
            Some(Ok(_)) | Some(Err(_)) => {
                return Err(WireError::Protocol("RUN parameters must be a map".into()));
            }
        };
        counted!("bolt.statements run");
        if self.trace_statements {
            eprintln!("[bolt] conn {} RUN {}", self.connection_id, query);
        }
        // The process's resident set BEFORE this statement runs, so a
        // statement that grows the heap by more than `RSS_GROWTH_REPORT_BYTES`
        // names itself in the log. The server's 30 s memory line saw the
        // deployment go from 8.5 to 11.0 GB of RSS inside one tick and be
        // OOM-killed at 12Gi, and could not say which of the corpus's 371
        // statements did it. Two small file reads per statement on Linux;
        // nothing elsewhere.
        let rss_before = rss_bytes();
        let trace_this = self.trace_counters || query.trim_start().starts_with(TRACE_MARKER);
        let parsed = match parse_any(&query) {
            Ok(q) => q,
            Err(e) => {
                return self.fail(out, "Neo.ClientError.Statement.SyntaxError", &e.to_string());
            }
        };
        // Inside an explicit transaction, install the session's transaction for
        // exactly this statement's execution so its writes buffer into it (and
        // read-your-writes holds); otherwise the statement autocommits. The
        // transaction is carried back into the session for the next message.
        let graph = std::sync::Arc::clone(&self.graph);
        let run_result = if let Some(txn) = self.txn.take() {
            let (txn, r) = graph.with_txn(txn, || {
                self.run_traced(&graph, &parsed, params, trace_this, &query)
            });
            self.txn = Some(txn);
            r
        } else if graph.serialisable_autocommit_enabled() && parsed.may_write() {
            // SERIALISABLE AUTOCOMMIT — for a statement that CAN write. One
            // that cannot (decided from its syntax, `Stmt::may_write`) takes
            // the plain path below: it has nothing to buffer, nothing to
            // validate and nothing to re-run, and a transaction around it
            // would only record every read it makes. The read path therefore
            // costs exactly what it did before this existed.
            //
            // The statement runs inside a
            // transaction of its own: its reads are recorded, its writes are
            // buffered, and the commit validates that nothing it read moved
            // before it wrote. If something did — two sessions incrementing
            // one counter, both having read the old value — the loser's
            // commit aborts and the statement RE-RUNS on the new value,
            // instead of writing a stale result over a fresh one. Measured
            // before this existed: 8 clients × 200 `SET n.hits = n.hits + 1`
            // on one node landed 756 of 1,600.
            //
            // A statement that only reads never validates and never re-runs,
            // so the common path costs a transaction begin and a snapshot pin.
            // A conflict on a genuinely hot key is the ordinary case, not an
            // error: eight sessions incrementing one counter serialise, and
            // each loses several rounds before it wins. The bound is against
            // livelock, not a budget a normal statement should ever reach;
            // a yield between attempts lets the winner's publish land.
            const RETRIES: u32 = 4096;
            // Conflicts tolerated while HOLDING the locks before falling
            // back to plain OCC — a statement whose conflicts come from its
            // READ set must not camp on keys a lock cannot help.
            const ESCALATED_LOSS_BOUND: u32 = 32;
            let mut attempt = 0u32;
            let mut escalated_losses = 0u32;
            // The escalation lane (W2.2): the sorted union of write-set keys
            // the conflicts have named, and the FIFO guards held across
            // re-runs. Advisory — OCC validation stays the sole correctness
            // authority; the queue only ORDERS the losers so they stop
            // burning full re-executions against each other.
            let mut locked: Vec<Vec<u8>> = Vec::new();
            let mut guards: Vec<engram_store::LockGuard> = Vec::new();
            loop {
                let txn = graph.open_txn();
                let (txn, r) =
                    graph.with_txn(txn, || {
                        self.run_traced(&graph, &parsed, params.clone(), trace_this, &query)
                    });
                match r {
                    Ok(result) => match graph.commit_owned_reporting(txn) {
                        Ok(()) => {
                            crate::counters::record_win(attempt);
                            break Ok(result);
                        }
                        Err((engram_graph::GraphError::TxnConflict, info))
                            if attempt < RETRIES =>
                        {
                            attempt += 1;
                            crate::counters::AUTOCOMMIT_RERUNS
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            counted!("bolt.autocommit statements re-run on conflict");
                            // WHAT did it collide on? The two candidate fixes
                            // for the re-run rate are chosen by this answer,
                            // so record it before doing anything about it.
                            if let Some(k) = info.as_ref().and_then(|i| i.conflicting.first()) {
                                crate::counters::record_conflict_class(k);
                            }
                            if !guards.is_empty() {
                                escalated_losses += 1;
                                crate::counters::ESCALATED_LOSSES
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                if escalated_losses > ESCALATED_LOSS_BOUND {
                                    guards.clear();
                                    locked.clear();
                                }
                            }
                            // ESCALATE when the conflict is on a key this
                            // statement itself WRITES — only a write-write
                            // conflict can be queued behind.
                            let escalate = graph.conflict_escalation_enabled()
                                && escalated_losses <= ESCALATED_LOSS_BOUND
                                && info.as_ref().is_some_and(|i| {
                                    i.conflicting.iter().any(|k| i.write_set.contains(k))
                                });
                            if escalate {
                                let info = info.expect("checked above");
                                let mut union: std::collections::BTreeSet<Vec<u8>> =
                                    locked.drain(..).collect();
                                union.extend(info.write_set);
                                // Drop every held guard BEFORE re-acquiring:
                                // acquisition is then ascending-from-nothing
                                // in sorted key order, so no hold-and-wait
                                // cycle can form between statements.
                                guards.clear();
                                locked = union.into_iter().collect();
                                counted!("bolt.autocommit escalated to the entity lock");
                                crate::counters::ESCALATIONS
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                for k in &locked {
                                    guards.push(graph.lock_conflict_key(k.clone()));
                                }
                            } else if guards.is_empty() {
                                // Plain-OCC growing backoff: the winner still
                                // needs CPU to publish; a loser re-running
                                // instantly on every core can starve it
                                // under a container quota.
                                for _ in 0..=attempt.min(64) {
                                    std::thread::yield_now();
                                }
                            }
                            continue;
                        }
                        Err((e, _)) => break Err(engram_graph::RunError::Graph(e)),
                    },
                    Err(e) => {
                        graph.rollback_owned(txn);
                        break Err(e);
                    }
                }
            }
        } else {
            self.run_traced(&graph, &parsed, params, trace_this, &query)
        };
        if let (Some(before), Some(after)) = (rss_before, rss_bytes()) {
            if after.saturating_sub(before) >= RSS_GROWTH_REPORT_BYTES {
                counted!("bolt.statements that grew the resident set");
                let shown: String = query.chars().take(240).collect();
                eprintln!(
                    "[bolt] statement grew rss by {} MB ({} -> {} MB) on conn {}: {}",
                    (after - before) >> 20,
                    before >> 20,
                    after >> 20,
                    self.connection_id,
                    shown.replace('\n', " ")
                );
            }
        }
        match run_result {
            Ok(result) => {
                let qid = self.next_qid;
                self.next_qid += 1;
                let mut meta = BTreeMap::new();
                meta.insert(
                    "fields".to_string(),
                    Value::List(result.columns.iter().cloned().map(Value::Str).collect()),
                );
                meta.insert("qid".to_string(), Value::Int(qid));
                meta.insert("t_first".to_string(), Value::Int(0));
                self.home_db(&mut meta);
                self.streams.insert(qid, Stream { result, at: 0 });
                self.send(out, MSG_SUCCESS, vec![Pack::Value(Value::Map(meta))])
            }
            Err(e) => {
                let code = match &e {
                    RunError::Unsupported(_) => "Neo.ClientError.Statement.NotSupported",
                    RunError::Semantic(_) => "Neo.ClientError.Statement.SemanticError",
                    RunError::Eval(_) => "Neo.ClientError.Statement.ArgumentError",
                    RunError::Graph(_) => "Neo.ClientError.Statement.ExecutionFailed",
                    // The saturation marker is stage-internal control flow;
                    // one leaking here is an engine bug, reported as such.
                    RunError::Saturated => "Neo.DatabaseError.Statement.ExecutionFailed",
                };
                self.fail(out, code, &e.to_string())
            }
        }
    }

    fn pull(&mut self, fields: Vec<Pack>, out: &mut Vec<u8>, emit: bool) -> Result<(), WireError> {
        let extras = match fields.into_iter().next().map(|p| p.into_value()) {
            Some(Ok(Value::Map(m))) => m,
            _ => BTreeMap::new(),
        };
        let n = match extras.get("n") {
            Some(Value::Int(v)) => *v,
            _ => -1,
        };
        // qid -1 means "the most recent" — the implicit-stream case every
        // autocommit session uses; explicit qids are required from Bolt 4.0
        // for concurrent streams in one transaction.
        let qid = match extras.get("qid") {
            Some(Value::Int(v)) if *v >= 0 => *v,
            _ => match self.streams.keys().next_back() {
                Some(k) => *k,
                None => {
                    return self.fail(
                        out,
                        "Neo.ClientError.Statement.InvalidUsage",
                        "no open result stream",
                    );
                }
            },
        };
        let Some(stream) = self.streams.get_mut(&qid) else {
            return self.fail(
                out,
                "Neo.ClientError.Statement.InvalidUsage",
                &format!("no stream with qid {qid}"),
            );
        };
        let remaining = stream.result.rows.len() - stream.at;
        let take = if n < 0 {
            remaining
        } else {
            (n as usize).min(remaining)
        };
        if emit {
            for i in 0..take {
                let row = stream.result.rows[stream.at + i].clone();
                let mut field = Vec::new();
                crate::packstream::encode_size_public(row.len(), &mut field);
                for v in &row {
                    encode_value(v, &mut field)?;
                }
                Self::send_raw(out, MSG_RECORD, &field);
                counted!("bolt.records streamed");
            }
        }
        stream.at += take;
        let done = stream.at == stream.result.rows.len();
        let mut meta = BTreeMap::new();
        if done {
            self.streams.remove(&qid);
            meta.insert("bookmark".to_string(), Value::Str(self.bookmark()));
        } else {
            meta.insert("has_more".to_string(), Value::Bool(true));
        }
        self.send(out, MSG_SUCCESS, vec![Pack::Value(Value::Map(meta))])
    }

    /// The bookmark: monotonic from the store's commit clock, so replicas
    /// can one day wait on it. Free on one instance, load-bearing later.
    fn bookmark(&self) -> String {
        format!("eg:{}", self.graph.now_ts())
    }

    /// 5.8+: BEGIN and autocommit RUN report the resolved home database when
    /// the client named none. This server has one database, so the answer is
    /// constant; what a driver does with it (server-side routing's home-db
    /// cache) needs the key present, not the value interesting.
    fn home_db(&self, meta: &mut BTreeMap<String, Value>) {
        if self.version >= (5, 8) {
            meta.insert("db".to_string(), Value::Str("neo4j".to_string()));
        }
    }

    fn fail(&mut self, out: &mut Vec<u8>, code: &str, message: &str) -> Result<(), WireError> {
        sometimes!("bolt.sent a failure", true);
        let mut meta = BTreeMap::new();
        meta.insert("message".to_string(), Value::Str(message.to_string()));
        if self.version >= (5, 7) {
            // 5.7 renamed `code` to `neo4j_code` and added the GQL status
            // fields. `code` is sent beside it: a 5.7+ driver reads
            // `neo4j_code`, an older one reading `code` loses nothing, and a
            // key a driver ignores costs a dozen bytes.
            let (status, description, class) = gql_of(code);
            meta.insert("neo4j_code".to_string(), Value::Str(code.to_string()));
            meta.insert("gql_status".to_string(), Value::Str(status.to_string()));
            meta.insert("description".to_string(), Value::Str(description.to_string()));
            let mut record = BTreeMap::new();
            record.insert("OPERATION".to_string(), Value::Str(String::new()));
            record.insert("OPERATION_CODE".to_string(), Value::Str("0".to_string()));
            record.insert("CURRENT_SCHEMA".to_string(), Value::Str("/".to_string()));
            record.insert("_classification".to_string(), Value::Str(class.to_string()));
            meta.insert("diagnostic_record".to_string(), Value::Map(record));
        }
        meta.insert("code".to_string(), Value::Str(code.to_string()));
        self.state = State::Failed;
        self.send(out, MSG_FAILURE, vec![Pack::Value(Value::Map(meta))])
    }

    fn send(&self, out: &mut Vec<u8>, tag: u8, fields: Vec<Pack>) -> Result<(), WireError> {
        let mut payload = Vec::new();
        encode_struct(tag, &fields, &mut payload)?;
        Self::chunk(out, &payload);
        Ok(())
    }

    /// RECORD's field list is pre-encoded (a list header + values), so the
    /// structure is assembled by hand.
    fn send_raw(out: &mut Vec<u8>, tag: u8, encoded_field: &[u8]) {
        let mut payload = vec![0xB1, tag];
        payload.extend_from_slice(encoded_field);
        Self::chunk(out, &payload);
    }

    fn chunk(out: &mut Vec<u8>, payload: &[u8]) {
        for chunk in payload.chunks(0xFFFF) {
            out.extend_from_slice(&(chunk.len() as u16).to_be_bytes());
            out.extend_from_slice(chunk);
        }
        out.extend_from_slice(&[0, 0]);
    }
}
