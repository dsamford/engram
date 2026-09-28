//! What a server tells a client about the knobs a benchmark compares it on.
//!
//! # Why this is on the wire at all
//!
//! A benchmark result document carries a "fairness" block — the serving cache
//! budget and the intra-query width the two engines were held to — and a table
//! built from two documents whose blocks disagree is refused. That refusal has
//! always worked. What nothing could see was a block that AGREES with another
//! document while describing a server neither of them ran on, because the
//! numbers were typed on the client's command line and the server was started
//! somewhere else by someone else.
//!
//! It has now happened twice on this project. A Neo4j window stamped
//! `cache_budget_mb: 8192` on three batteries against a pod configured with 10
//! GiB of page cache; an engram dry run stamped `thread_cap: 6` against a
//! server started with `--workers 6` and no `ENGRAM_QUERY_PARALLELISM`, which
//! is a server with no intra-query parallelism at all. Neither is detectable
//! from the client side, and both compare cleanly against everything.
//!
//! So the server ANSWERS. A client that asks a server what it is serving under
//! cannot be told about a different server.
//!
//! # Why it is not in the `server` agent string
//!
//! The obvious place is [`crate::server::DEFAULT_SERVER_AGENT`], and it is the
//! wrong place. Drivers parse that string for a product and a version, and
//! `engram/0.1.0 (paged_cache_mb=8192)` is a version some of them will refuse
//! to parse. HELLO's SUCCESS metadata is an open map — Neo4j returns `server`,
//! `connection_id` and `hints` in it, and every driver ignores keys it does
//! not know — so an extra key costs nothing a driver depends on. The key is
//! absent entirely unless a server was configured to send one, so the default
//! HELLO is byte-for-byte what it was.

use std::collections::BTreeMap;

use engram_cypher::Value;

/// The HELLO SUCCESS metadata key this travels under.
///
/// Namespaced, because the map is shared with the protocol's own keys and a
/// bare `serving` would be a name a future Bolt revision could take.
pub const SERVING_KEY: &str = "engram_serving";

/// What a server is serving under, as far as a benchmark's fairness block is
/// concerned.
///
/// Both fields are `Option` and both `None` means something specific:
///
/// - `cache_budget_mb: None` — there is no serving cache to budget. An
///   in-memory or `--data-dir` engram store is entirely resident and has no
///   block cache, so a `--cache-mb` stamped against one describes a knob that
///   is not in circuit. That is a fact worth reporting, not an absence.
/// - `thread_cap: None` — the server declined to say. It is never a stand-in
///   for 1: "no intra-query parallelism installed" is the value `Some(1)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ServingHint {
    /// The serving cache budget in MiB, or `None` when the store has no
    /// cache to budget.
    pub cache_budget_mb: Option<u32>,
    /// The INTRA-QUERY parallelism width one statement may reach, or `None`
    /// when the server declined to say.
    ///
    /// This is the width the engine's morsel executor was installed at
    /// (`ENGRAM_QUERY_PARALLELISM`), and deliberately NOT the connection
    /// worker count (`--workers`). They are different knobs and only one of
    /// them is what a comparison against LadybugDB's `--threads` or
    /// Postgres's `max_parallel_workers_per_gather` means. The engram dry run
    /// that set `--workers 6` and stamped `thread_cap: 6` had a width of 1.
    pub thread_cap: Option<u32>,
    /// The connection worker count (`--workers`), carried so a reader can see
    /// BOTH numbers rather than having to know which one the check compared.
    pub workers: Option<u32>,
}

impl ServingHint {
    /// As HELLO metadata.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut m = BTreeMap::new();
        let put = |m: &mut BTreeMap<String, Value>, k: &str, v: Option<u32>| {
            m.insert(
                k.to_string(),
                v.map_or(Value::Null, |n| Value::Int(i64::from(n))),
            );
        };
        put(&mut m, "cache_budget_mb", self.cache_budget_mb);
        put(&mut m, "thread_cap", self.thread_cap);
        put(&mut m, "workers", self.workers);
        Value::Map(m)
    }

    /// Read one back off a HELLO SUCCESS metadata map.
    ///
    /// `None` when the peer sent no hint at all — a Neo4j, or an engram built
    /// before this existed, both of which are ordinary and neither of which is
    /// an error. A malformed or negative entry reads as absent rather than as
    /// a number, for the reason the whole mechanism exists: a fabricated
    /// figure is worse than a missing one.
    #[must_use]
    pub fn from_meta(meta: &BTreeMap<String, Value>) -> Option<ServingHint> {
        let Some(Value::Map(m)) = meta.get(SERVING_KEY) else {
            return None;
        };
        let get = |k: &str| -> Option<u32> {
            match m.get(k) {
                Some(Value::Int(n)) if *n >= 0 && *n <= i64::from(u32::MAX) => Some(*n as u32),
                _ => None,
            }
        };
        Some(ServingHint {
            cache_budget_mb: get("cache_budget_mb"),
            thread_cap: get("thread_cap"),
            workers: get("workers"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hint_survives_the_round_trip_and_an_absent_one_is_absent() {
        let h = ServingHint {
            cache_budget_mb: Some(8192),
            thread_cap: Some(6),
            workers: Some(6),
        };
        let mut meta = BTreeMap::new();
        meta.insert(SERVING_KEY.to_string(), h.to_value());
        assert_eq!(ServingHint::from_meta(&meta), Some(h));

        // A Neo4j's HELLO, or an engram from before this existed.
        let mut plain = BTreeMap::new();
        plain.insert("server".to_string(), Value::Str("Neo4j/5.26.0".into()));
        assert_eq!(ServingHint::from_meta(&plain), None);
    }

    #[test]
    fn a_null_field_is_absent_and_a_junk_field_is_absent_never_a_number() {
        let mut m = BTreeMap::new();
        m.insert("cache_budget_mb".to_string(), Value::Null);
        m.insert("thread_cap".to_string(), Value::Str("six".into()));
        m.insert("workers".to_string(), Value::Int(-1));
        let mut meta = BTreeMap::new();
        meta.insert(SERVING_KEY.to_string(), Value::Map(m));
        assert_eq!(
            ServingHint::from_meta(&meta),
            Some(ServingHint {
                cache_budget_mb: None,
                thread_cap: None,
                workers: None
            })
        );
    }
}
