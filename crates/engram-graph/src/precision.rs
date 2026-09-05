//! §7 — precision locking: validating the PREDICATE, not the read set.
//!
//! # The hole this closes
//!
//! OCC validation here asks "did anyone touch a row I materialised?". That
//! cannot see a PHANTOM: a row committed after our snapshot that our MATCH
//! would have returned had it existed. We never read it, so it never entered
//! the read set, so nothing aborts. `docs/concurrency-direction.md` records
//! this under *Known limitations* — the guarantee is not serialisability.
//!
//! Neumann/Mühlbauer/Kemper (SIGMOD 2015) invert the question: iterate the rows
//! CHANGED since the snapshot and test each against the reader's predicates.
//! Two things follow, and the second is why this is scheduled rather than
//! admired:
//!
//! 1. The cost is O(delta × predicates) — **independent of how much was read**,
//!    which removes the last O(read set) term from the commit path.
//! 2. Phantoms are closed, so the guarantee goes UP.
//!
//! # Why it is tractable here
//!
//! Three pieces already existed. §6's commit window is a ts-ordered record of
//! every key committed since any given snapshot — literally the first half of
//! this validator, built for the read-set loop. The store's `ChangeGuard` seam
//! keeps the engine free of any notion of labels. And a node pattern is already
//! a restriction set: `NodePattern { labels, props }` is exactly "every one of
//! these labels AND every one of these property equalities".
//!
//! # Coverage is incremental, and that is sound
//!
//! A pattern this module can represent is checked. One it cannot — a correlated
//! property map, a relationship pattern, an inequality — is simply absent from
//! the guard, and validation for it stays exactly what it is today. Absent
//! coverage can only admit the anomaly the engine already admits, so partial
//! coverage never makes anything worse. What it must never do is *claim* to
//! have checked a predicate it did not, which is why `Restriction::extract`
//! returns `None` rather than an approximation.
//!
//! # Why it ships OFF
//!
//! It changes which statements abort. That is an isolation IMPROVEMENT and
//! still a behaviour change, so it ships behind `set_precision_locking`,
//! default off, and flips only after a full TCK pass and a soak on both arms.

use std::collections::BTreeMap;

use engram_cypher::stmt::NodePattern;
use engram_cypher::{Expr, Truth, Value};
use engram_observe::counted;
use engram_store::{ChangeGuard, PropertyId, Record, Store};

use crate::{Graph, P_LABELS, decode_label_set, decode_prop_opt};

/// One node pattern, resolved to tokens: the restriction a MATCH imposed.
///
/// Tokens rather than names because this is tested under the commit latch, once
/// per changed row: a name lookup there would put a map probe and a string
/// compare at the one serialisation point that cannot be parallelised.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Restriction {
    /// Label tokens the node must carry — ALL of them, as multi-label is an AND
    /// in MATCH.
    labels: Vec<u32>,
    /// Property equalities, by token. ALL must hold.
    props: Vec<(u32, Value)>,
}

impl Restriction {
    /// The restriction `pat` imposes, or `None` when this module cannot
    /// represent it exactly.
    ///
    /// `None` is the important half. Every reason to decline is a case where an
    /// approximation would be a LIE — either admitting rows the pattern would
    /// have rejected (which aborts commits that were fine, a correctness-safe
    /// but user-hostile failure) or rejecting rows it would have matched (which
    /// misses the phantom this exists to catch, and claims a guarantee it does
    /// not deliver). Declining leaves today's rule in place for that pattern,
    /// which is the one option that is honest either way.
    pub(crate) fn extract(
        graph: &Graph,
        pat: &NodePattern,
        params: &BTreeMap<String, Value>,
    ) -> Option<Restriction> {
        // A label never minted has no token, so nothing can carry it — but
        // "nothing can carry it" is a statement about NOW, and a concurrent
        // transaction may mint it and write a matching node. Declining is the
        // honest answer; a restriction over an absent token would silently
        // match nothing for ever.
        let mut labels = Vec::with_capacity(pat.labels.len());
        for l in &pat.labels {
            labels.push(graph.token_peek("lbl:", &graph.labels, l)?);
        }
        labels.sort_unstable();

        let mut props = Vec::new();
        if let Some(pv) = &pat.props {
            // A map LITERAL of row-independent values, or a parameter. Anything
            // that reads the incoming row is a different restriction per row
            // and cannot be one entry here.
            let entries = match pv {
                Expr::Map(entries) => {
                    let mut out = Vec::with_capacity(entries.len());
                    for (k, v) in entries {
                        out.push((k.clone(), literal_value(v, params)?));
                    }
                    out
                }
                Expr::Param(name) => match params.get(name)? {
                    Value::Map(m) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    _ => return None,
                },
                _ => return None,
            };
            for (name, v) in entries {
                // A NULL equality never matches under `eq3` (it answers
                // Unknown, not True), so a pattern carrying one matches
                // nothing and cannot be phantomed into. Representing it as a
                // restriction that matches nothing would be correct but
                // pointless; declining is simpler and equally sound.
                if matches!(v, Value::Null) {
                    return None;
                }
                props.push((graph.prop_token_peek(&name)?, v));
            }
            props.sort_by_key(|(t, _)| *t);
        }
        Some(Restriction { labels, props })
    }

    /// Whether a decoded node record satisfies this restriction.
    ///
    /// Mirrors `interp::node_satisfies` exactly — every label present, every
    /// property equal under `eq3` — because that function is the authority on
    /// what a pattern matches, and a validator that disagreed with it would
    /// abort on rows the MATCH would have skipped.
    fn matches(&self, rec: &Record) -> bool {
        let Ok(have) = decode_label_set(rec.get(P_LABELS)) else {
            // An undecodable record is not something to guess about. Treating
            // it as a match would abort a healthy commit on a corrupt
            // neighbour; treating it as a non-match keeps today's behaviour.
            return false;
        };
        for want in &self.labels {
            if !have.contains(want) {
                return false;
            }
        }
        for (token, want) in &self.props {
            let Some(tagged) = rec.get(PropertyId(*token)) else {
                return false; // absent property: `Null eq3 v` is never True
            };
            let Some(v) = decode_prop_opt(tagged) else {
                return false;
            };
            if v.eq3(want) != Truth::True {
                return false;
            }
        }
        true
    }
}

/// A value that does not depend on the incoming row, or `None`.
///
/// Deliberately a short whitelist rather than "anything `eval_expr` accepts
/// without a row": an expression that merely HAPPENS to be row-independent
/// today (a function call, an arithmetic fold) is one a later change can make
/// row-dependent, and the failure would be a restriction that silently stopped
/// describing its pattern.
fn literal_value(e: &Expr, params: &BTreeMap<String, Value>) -> Option<Value> {
    match e {
        Expr::Bool(b) => Some(Value::Bool(*b)),
        Expr::Int(i) => Some(Value::Int(*i)),
        Expr::Float(f) => Some(Value::Float(*f)),
        Expr::Str(s) => Some(Value::Str(s.clone())),
        Expr::Param(name) => params.get(name).cloned(),
        // `Expr::Null` is representable but pointless — see `extract`.
        _ => None,
    }
}

/// The [`ChangeGuard`] a transaction's restrictions become.
pub(crate) struct PredicateGuard {
    store: Store,
    /// The `nodes` prefix, and its encoding. A changed key outside the encoding
    /// is not a node record and no node restriction can speak about it; the
    /// prefix itself is what the re-read takes.
    nodes: engram_key::KeyPrefix,
    nodes_prefix: Vec<u8>,
    restrictions: Vec<Restriction>,
}

impl PredicateGuard {
    pub(crate) fn new(graph: &Graph, restrictions: Vec<Restriction>) -> PredicateGuard {
        let nodes = graph.nodes_prefix();
        let mut nodes_prefix = Vec::with_capacity(engram_key::PREFIX_LEN);
        nodes.encode_into(&mut nodes_prefix);
        PredicateGuard {
            store: graph.shared_store().clone(),
            nodes,
            nodes_prefix,
            restrictions,
        }
    }
}

impl ChangeGuard for PredicateGuard {
    fn conflicts(&self, key: &[u8], is_put: bool) -> bool {
        // A TOMBSTONE is a deletion. It cannot be a phantom — a phantom is a
        // row that APPEARED — and read-set validation already covers the case
        // where we read the row before it was deleted. Testing it would also
        // be meaningless: there is no record left to test.
        if !is_put {
            return false;
        }
        if !key.starts_with(&self.nodes_prefix) {
            return false;
        }
        let Some(body) = key.get(self.nodes_prefix.len()..) else {
            return false;
        };
        // THE RE-READ, which is the design decision the plan names explicitly:
        // do not widen the commit ring to carry values — it is a hot, latched
        // structure — and re-read the changed row instead. Changed rows are
        // recent by construction, so this lands in the tail or the newest
        // segment, and after §0.1 that lookup is O(1) per segment rather than a
        // rescan.
        let Some(bytes) = self.store.get_at(&self.nodes, body, u64::MAX) else {
            return false; // gone by the time we looked: not a row we can match
        };
        let Ok(rec) = Record::decode(&bytes) else {
            return false;
        };
        for r in &self.restrictions {
            if r.matches(&rec) {
                counted!("graph.a restriction matched a concurrently committed node");
                return true;
            }
        }
        false
    }
}
