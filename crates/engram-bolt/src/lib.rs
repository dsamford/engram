//! M7 — the wire: PackStream v2 and the sans-io Bolt server.

#![forbid(unsafe_code)]

pub mod client;
pub mod packstream;
pub mod server;

pub use packstream::{Decoder, Pack, PackError, decode_value, encode_value};
pub use server::{BoltServer, GraphResolver, MAX_MESSAGE_BYTES, TRACE_MARKER, WireError};

/// Production-visible counters (the `engram_log::FSYNCS` pattern): plain
/// global atomics, because the `counted!` trace is thread-local and only
/// tests install one — in the server binary those macros record nothing.
/// The server prints this surface periodically; tests may read it directly.
/// Monotonic, never read by engine logic.
pub mod counters {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Autocommit statements re-run after an OCC conflict (one per re-run).
    pub static AUTOCOMMIT_RERUNS: AtomicU64 = AtomicU64::new(0);
    /// Successful autocommit WRITE statements bucketed by the attempt that
    /// won: `[1, 2, 3–4, 5–8, 9+]`. The distribution, not just a total —
    /// a healthy hot key wins at 1–2; a retry storm lives in the top bucket.
    pub static WON_AT: [AtomicU64; 5] = {
        #[allow(clippy::declare_interior_mutable_const)]
        const Z: AtomicU64 = AtomicU64::new(0);
        [Z, Z, Z, Z, Z]
    };
    /// The most attempts any single statement has needed.
    pub static MAX_ATTEMPTS: AtomicU64 = AtomicU64::new(0);

    /// WHICH key class each OCC conflict named, bucketed:
    /// `[guard, unique-marker, adjacency, membership, entity-record, other]`.
    ///
    /// RC1 of `docs/write-concurrency-ceiling.md` measured 0.88 whole-statement
    /// re-executions per acknowledged write on `rel-hub` but could not say what
    /// they collided ON, and the two candidate fixes are chosen by that answer:
    /// a guard row (`'G'|node`, written by BOTH endpoints of every relationship
    /// create) is a FALSE sharing class that a finer conflict unit would remove,
    /// while a genuine hot-key write is a TRUE conflict that only ordering can
    /// help. Building either without this counter would be guessing.
    ///
    /// Classified from the key body's first byte, after the fixed 13-byte
    /// prefix; the body tags are the graph's own (`guard_row`, the `'U'`
    /// uniqueness marker).
    pub static CONFLICT_CLASS: [AtomicU64; 6] = {
        #[allow(clippy::declare_interior_mutable_const)]
        const Z: AtomicU64 = AtomicU64::new(0);
        [Z, Z, Z, Z, Z, Z]
    };

    /// Bucket one conflicting key by its class. `key` is a full logical key:
    /// realm(4) namespace(4) KIND(1) partition(4), then the body.
    ///
    /// The graph's index partition tags its row families in the body's first
    /// byte — `'G'` guard, `'U'` uniqueness marker, `'O'`/`'I'` adjacency,
    /// `'L'` membership (`guard_row`, `adjacency_row`, `membership_prefix` in
    /// engram-graph) — so those are read from the body. Everything else is
    /// read from the KIND byte, because a node or edge record's body is a
    /// bare id with no tag to read.
    pub fn record_conflict_class(key: &[u8]) {
        const KIND_AT: usize = 8;
        const PREFIX_LEN: usize = 13;
        let bucket = match key.get(PREFIX_LEN) {
            Some(b'G') => 0,
            Some(b'U') => 1,
            Some(b'O') | Some(b'I') => 2,
            Some(b'L') => 3,
            _ => match key.get(KIND_AT) {
                // Kind bytes, from engram-key's `Kind`: a record of an entity
                // rather than one of the index families above.
                Some(1) => 4, // NODE
                Some(2) => 4, // EDGE
                _ => 5,
            },
        };
        CONFLICT_CLASS[bucket].fetch_add(1, Ordering::Relaxed);
    }

    /// The conflict-class counts, in bucket order.
    pub fn conflict_classes() -> [u64; 6] {
        std::array::from_fn(|i| CONFLICT_CLASS[i].load(Ordering::Relaxed))
    }
    /// Statements that ESCALATED to the FIFO entity lock after a
    /// write-write conflict (W2.2).
    pub static ESCALATIONS: AtomicU64 = AtomicU64::new(0);
    /// Conflicts suffered WHILE escalated — the "escalated re-runs per
    /// success ≤ 1" acceptance gate reads this against ESCALATIONS.
    pub static ESCALATED_LOSSES: AtomicU64 = AtomicU64::new(0);

    /// Record a write statement's success after `reruns` conflict re-runs.
    pub fn record_win(reruns: u32) {
        let bucket = match reruns {
            0 => 0,
            1 => 1,
            2..=3 => 2,
            4..=7 => 3,
            _ => 4,
        };
        WON_AT[bucket].fetch_add(1, Ordering::Relaxed);
        MAX_ATTEMPTS.fetch_max(u64::from(reruns) + 1, Ordering::Relaxed);
    }
}

use engram_observe::{Canary, Gate, Registration, Subsystem};

/// The wire layer, as a registered subsystem.
pub struct BoltWire;

impl Subsystem for BoltWire {
    const NAME: &'static str = "bolt";

    fn register() -> Registration {
        Registration::new()
            // Reserved for the send boundary once results stream from a real
            // transport — the aspirational placement the pure layers use.
            .crash_point("bolt.before_success_sent")
            .sometimes("bolt.refused a non-bolt preamble")
            .sometimes("bolt.declined an unknown manifest version")
            .sometimes("bolt.refused an unoffered manifest pick")
            .sometimes("bolt.ignored a message while failed")
            .sometimes("bolt.explicit transaction opened")
            .sometimes("bolt.rolled back a transaction")
            .sometimes("bolt.sent a failure")
            .counter("bolt.sessions negotiated")
            .counter("bolt.sessions negotiated through the manifest")
            .counter("bolt.manifest offered")
            .counter("bolt.statements run")
            .counter("bolt.statements that grew the resident set")
            .counter("bolt.records streamed")
            .gate(
                Gate::new(
                    "the manifest is answered with the whole offer, and only an offered pick negotiates",
                    Canary::new("offer 4.4 in the manifest reply and assert a client picking 4.4 is refused; drop the varint wait and assert a split pick negotiates early"),
                ),
            )
            .gate(
                Gate::new(
                    "an unknown manifest version is passed over, never garbage",
                    Canary::new("treat major 0xFF minor 2 as v1 and assert the v2-only proposal list fails to negotiate"),
                ),
            )
            .gate(
                Gate::new(
                    "version ranges decode",
                    Canary::new("match proposals exactly and assert the range-only driver list fails to negotiate"),
                ),
            )
            .gate(
                Gate::new(
                    "the failure protocol IGNOREs until RESET",
                    Canary::new("keep answering after a failure and assert the post-failure RUN is not IGNORED"),
                ),
            )
            .gate(
                Gate::new(
                    "wire values decode identical to interpreter values",
                    Canary::new("swap two PackStream int widths and assert the round-trip goldens fail"),
                ),
            )
    }
}
