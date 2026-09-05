//! Marker keys for UNIQUE / NODE KEY constraints (W1.2 of
//! `docs/scale-and-integrity-plan.md`).
//!
//! A constrained write puts a MARKER ROW at a key derived deterministically
//! from `(constraint, value tuple)`, so two transactions writing the same
//! value collide write-write at OCC validation — the phantom the population
//! walk could never see becomes an ordinary conflict. The marker's value is
//! the owner's entity id; a hit is VERIFIED against the live owner before it
//! refuses (see `enforce_constraints`), so a stale marker or a digest
//! collision degrades to a spurious re-check or refusal for that one pair,
//! never to silent corruption.
//!
//! # Canonical tuple encoding
//!
//! The digest is over a canonical byte encoding whose equality matches
//! [`Value::eq3`] over the storable property domain:
//!
//! - numerics normalise: an integral, exactly-representable float encodes as
//!   its integer (`1` and `1.0` are one value; `-0.0` is `0`); `NaN` is
//!   never `eq3`-equal to anything, so a NaN-bearing tuple writes NO marker
//!   (it cannot violate uniqueness);
//! - `Time` encodes offset-normalised, `DateTime` by instant (the offset is
//!   presentation), `Duration` componentwise — exactly `eq3`'s rules;
//! - lists encode recursively; a `null` inside a list makes the tuple's
//!   equality `Unknown` for ever, so it too writes no marker.
//!
//! **Documented divergence:** `eq3` compares `Int` to `Float` through an
//! `as f64` cast, which is not injective above 2^53 — `Int(2^53)` and
//! `Int(2^53 + 1)` are both `eq3`-equal to `Float(9007199254740992.0)` but
//! not to each other, so NO total encoding can match `eq3` there (`eq3` is
//! not transitive). The canonical encoding is exact for integers up to
//! ±2^53 and treats larger integers by their exact value; this canonical
//! equality IS the constraint's definition of equality, property-tested in
//! `tests/constraint_markers.rs`.
//!
//! **The digest is unkeyed, deliberately.** The values it digests sit in
//! PLAINTEXT records in the same partition — an attacker with raw store
//! bytes reads the properties themselves before dictionary-testing digests.
//! A keyed digest without a key-management story would be theatre; when
//! protected kinds grow constrained labels, the marker rows seal with them.

use engram_cypher::Value;

/// The index-partition row family for markers ('L', 'O', 'I' and 'G' are
/// the neighbours).
pub(crate) const MARKER_TAG: u8 = b'U';

/// The 8-byte identity of a constraint, from its NAME — the schema row key,
/// stable across the constraint's life and freed by a drop.
pub(crate) fn constraint_digest(name: &str) -> [u8; 8] {
    let mut h = blake3::Hasher::new();
    h.update(b"con:");
    h.update(name.as_bytes());
    let d = h.finalize();
    d.as_bytes()[..8].try_into().expect("8")
}

/// The full marker row body: `'U' ‖ constraint digest (8) ‖ tuple digest
/// (16)` — 25 bytes, no user value bytes in the key.
pub(crate) fn marker_body(con: &[u8; 8], canonical_tuple: &[u8]) -> Vec<u8> {
    let mut h = blake3::Hasher::new();
    h.update(b"tuple:");
    h.update(canonical_tuple);
    let d = h.finalize();
    let mut v = Vec::with_capacity(25);
    v.push(MARKER_TAG);
    v.extend_from_slice(con);
    v.extend_from_slice(&d.as_bytes()[..16]);
    v
}

/// The canonical encoding of one complete tuple, or `None` when the tuple
/// can never be `eq3`-equal to anything (NaN, a null inside a list) or
/// holds a value the encoding does not cover (entities, maps — not storable
/// as properties; the caller falls back to the population walk, so an
/// uncovered value NARROWS the fast path, never the enforcement).
pub(crate) fn canonical_tuple(values: &[&Value]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(values.len() * 12);
    for v in values {
        encode_value(v, &mut out)?;
    }
    Some(out)
}

fn encode_value(v: &Value, out: &mut Vec<u8>) -> Option<()> {
    match v {
        // A null tuple COMPONENT is exempted upstream (Neo4j's rule); a null
        // reaching here sits inside a list, where it poisons equality to
        // Unknown for ever — nothing to enforce, no marker.
        Value::Null => return None,
        Value::Bool(b) => {
            out.push(0x01);
            out.push(u8::from(*b));
        }
        Value::Int(i) => {
            out.push(0x02);
            out.extend_from_slice(&i.to_be_bytes());
        }
        Value::Float(f) => {
            if f.is_nan() {
                return None; // never eq3-equal to anything
            }
            // Integral and exactly representable → the INTEGER encoding,
            // so `1` and `1.0` (and `-0.0` and `0`) digest identically.
            if f.is_finite() && *f == f.trunc() && *f >= i64::MIN as f64 && *f <= i64::MAX as f64 {
                let i = *f as i64;
                if i as f64 == *f {
                    out.push(0x02);
                    out.extend_from_slice(&i.to_be_bytes());
                    return Some(());
                }
            }
            out.push(0x03);
            out.extend_from_slice(&f.to_bits().to_be_bytes());
        }
        Value::Str(s) => {
            out.push(0x04);
            out.extend_from_slice(&(s.len() as u32).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Value::Date(d) => {
            out.push(0x05);
            out.extend_from_slice(&d.to_be_bytes());
        }
        Value::LocalTime(n) => {
            out.push(0x06);
            out.extend_from_slice(&n.to_be_bytes());
        }
        Value::Time {
            nanos,
            offset_seconds,
        } => {
            // Offset-normalised, exactly as eq3 compares.
            let utc = nanos - i64::from(*offset_seconds) * 1_000_000_000;
            out.push(0x07);
            out.extend_from_slice(&utc.to_be_bytes());
        }
        Value::DateTime {
            epoch_seconds,
            nanos,
            ..
        } => {
            // By INSTANT — offset and zone are presentation.
            out.push(0x08);
            out.extend_from_slice(&epoch_seconds.to_be_bytes());
            out.extend_from_slice(&nanos.to_be_bytes());
        }
        Value::LocalDateTime {
            epoch_seconds,
            nanos,
        } => {
            out.push(0x09);
            out.extend_from_slice(&epoch_seconds.to_be_bytes());
            out.extend_from_slice(&nanos.to_be_bytes());
        }
        Value::Duration {
            months,
            days,
            seconds,
            nanos,
        } => {
            // Componentwise — P1M is not P30D, exactly as eq3 compares.
            out.push(0x0A);
            out.extend_from_slice(&months.to_be_bytes());
            out.extend_from_slice(&days.to_be_bytes());
            out.extend_from_slice(&seconds.to_be_bytes());
            out.extend_from_slice(&nanos.to_be_bytes());
        }
        Value::List(items) => {
            out.push(0x0B);
            out.extend_from_slice(&(items.len() as u32).to_be_bytes());
            for item in items {
                encode_value(item, out)?;
            }
        }
        // Not storable as property values; the caller falls back to the
        // walk, which enforces whatever equality eq3 gives these.
        Value::Path(_) | Value::Map(_) | Value::Node { .. } | Value::Rel { .. } => return None,
    }
    Some(())
}
