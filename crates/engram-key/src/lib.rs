//! **Artifact #1** — the frozen memcomparable key encoding.
//!
//! ```text
//! realm:u32 │ namespace:u32 │ KIND:u8 │ partition:u32 │ <body> │ !commit_ts:u64
//! ```
//!
//! # Why this file is the most expensive one to get wrong
//!
//! The plan ranks it first by cost of being wrong, above everything else in the
//! system: *"The key encoding **is** the on-disk format and is unfixable once
//! data exists."* Five separate requirements are consequences of this tuple
//! rather than features built on top of it — the namespace component IS the
//! overlay model, partition-before-id IS the contiguous range scan, the tenant
//! prefix IS the isolation boundary and the DEK derivation, and a `KIND` IS how
//! protected properties become physically unstorable in plaintext.
//!
//! Get it wrong and none of them are recoverable without rewriting every byte.
//!
//! # Memcomparable, and what that actually demands
//!
//! Byte-wise `memcmp` order must equal logical order, for every pair of keys.
//! That is a stronger claim than "we used big-endian", and it is where these
//! encodings usually break:
//!
//! - every fixed-width integer is big-endian, so the most significant byte is
//!   compared first;
//! - `commit_ts` is stored **inverted** (`!ts`), so a NEWER version sorts
//!   BEFORE an older one and a point read is the first row of a prefix scan
//!   rather than the last;
//! - a variable-length component is escaped, because otherwise `[1]` and
//!   `[1, 0]` compare in the wrong order relative to what follows them —
//!   the classic prefix ambiguity;
//! - a `fixed_bytes<N>` component is **deliberately NOT escaped** (FC-3),
//!   because a space-filling-curve cell id must keep its exact bit layout for
//!   Z-order/Hilbert locality to survive into the key.
//!
//! # The keyspace hygiene rule
//!
//! > A user property value must never appear in a sort-ordered key position.
//!
//! An LSM sorts by key, so a plaintext value in a key **is order-preserving
//! encryption** — sorting attack included — whether or not anyone called it
//! that. This is not a review rule here: [`Structural`] is a sealed trait, and
//! the only route from a value to key bytes is [`Structural::encode_into`].
//! A property value cannot implement a sealed trait from outside this crate, so
//! the mistake is unrepresentable rather than discouraged.

#![forbid(unsafe_code)]

use engram_observe::{Canary, Gate, Registration, Subsystem, counted, sometimes};

pub mod kind;
pub mod value;

pub use kind::{ESCAPE_KIND, Kind, KindBlock};

// ─── The hygiene rule, as a type ────────────────────────────────────────────

mod sealed {
    /// Sealed. Implemented only inside this crate, which is what stops a user
    /// property value from ever becoming a key component.
    pub trait Sealed {}
}

/// A value that is STRUCTURAL — low-entropy, enumerable anyway, and therefore
/// safe to sort on.
///
/// Realm, namespace, partition, kind, entity id, cell id. Every one of these is
/// either a tenant-scoped identifier or a derived locality value; none is
/// content a user typed. The trait is sealed so the set cannot be extended from
/// outside, which is the enforcement point for the hygiene rule above.
pub trait Structural: sealed::Sealed {
    /// Append this value's memcomparable bytes.
    fn encode_into(&self, out: &mut Vec<u8>);
}

/// A tenant. First in the key, so every scan is tenant-local by construction
/// and a DEK can be derived from the prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Realm(pub u32);

/// A namespace within a realm — the overlay model's physical prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Namespace(pub u32);

/// A partition. Placed BEFORE the entity id so one partition is a contiguous
/// range rather than a scattered set of point reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Partition(pub u32);

/// An entity id, fixed width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityId(pub u64);

/// An order-preserving cell id — FC-3.
///
/// Written UNESCAPED and fixed width, because a Z-order or Hilbert cell id
/// carries its locality in its exact bit layout. Escaping would insert bytes
/// that are not part of the curve, and the spatial ordering the whole
/// reservation exists for would not survive into the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellId<const N: usize>(pub [u8; N]);

impl sealed::Sealed for Realm {}
impl sealed::Sealed for Namespace {}
impl sealed::Sealed for Partition {}
impl sealed::Sealed for EntityId {}
impl<const N: usize> sealed::Sealed for CellId<N> {}

impl Structural for Realm {
    fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0.to_be_bytes());
    }
}
impl Structural for Namespace {
    fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0.to_be_bytes());
    }
}
impl Structural for Partition {
    fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0.to_be_bytes());
    }
}
impl Structural for EntityId {
    fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0.to_be_bytes());
    }
}
impl<const N: usize> Structural for CellId<N> {
    fn encode_into(&self, out: &mut Vec<u8>) {
        // Verbatim. See the type's doc comment — this is FC-3 and the absence
        // of escaping is the reservation, not an oversight.
        out.extend_from_slice(&self.0);
    }
}

// ─── Variable-length components ─────────────────────────────────────────────

/// Escape a variable-length component so that byte order still equals logical
/// order once something follows it.
///
/// Without this, `[1]` followed by a later component and `[1, 0]` followed by a
/// later component are indistinguishable — the shorter value's successor bytes
/// are compared against the longer value's own bytes, and the result depends on
/// data that has nothing to do with the comparison. `0x00` becomes `0x00 0xFF`
/// and the component is terminated by `0x00 0x01`, so a terminator can never
/// occur inside a payload and a prefix always sorts before a longer string.
///
/// Not exposed as a `Structural` component: nothing in the frozen v1 layout is
/// variable-length. It exists because a future KIND may need one, and inventing
/// the escaping later — after data exists — is exactly the unfixable class.
pub fn encode_var_bytes(payload: &[u8], out: &mut Vec<u8>) {
    for b in payload {
        out.push(*b);
        if *b == 0x00 {
            out.push(0xFF);
            sometimes!("key.escaped a 0x00 in a variable component", true);
        }
    }
    out.push(0x00);
    out.push(0x01);
}

/// Inverse of [`encode_var_bytes`]. Returns the payload and the bytes consumed.
pub fn decode_var_bytes(buf: &[u8]) -> Option<(Vec<u8>, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        let b = buf[i];
        if b != 0x00 {
            out.push(b);
            i += 1;
            continue;
        }
        // A 0x00 is either an escaped literal or the terminator.
        let next = *buf.get(i + 1)?;
        match next {
            0xFF => {
                out.push(0x00);
                i += 2;
            }
            0x01 => return Some((out, i + 2)),
            // Anything else is a corrupt component. Returning None rather than
            // guessing: a key decoded on a guess is a row attributed to the
            // wrong tenant.
            _ => return None,
        }
    }
    None
}

// ─── The key ────────────────────────────────────────────────────────────────

/// The fixed prefix every key carries, in frozen order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyPrefix {
    /// Tenant. First, so the prefix is the isolation boundary.
    pub realm: Realm,
    /// Namespace within the realm.
    pub namespace: Namespace,
    /// What this key IS. Dispatches body decoding — see FC-2.
    pub kind: Kind,
    /// Partition, before any entity id, so a partition scan is contiguous.
    pub partition: Partition,
}

/// Length of the encoded fixed prefix: 4 + 4 + 1 + 4.
pub const PREFIX_LEN: usize = 13;

/// Length of the encoded inverted commit timestamp.
pub const COMMIT_TS_LEN: usize = 8;

impl KeyPrefix {
    /// Encode the fixed prefix.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        self.realm.encode_into(out);
        self.namespace.encode_into(out);
        out.push(self.kind.byte());
        self.partition.encode_into(out);
    }

    /// Decode the fixed prefix. `None` if the buffer is too short.
    pub fn decode(buf: &[u8]) -> Option<(Self, usize)> {
        if buf.len() < PREFIX_LEN {
            return None;
        }
        let realm = Realm(u32::from_be_bytes(buf[0..4].try_into().ok()?));
        let namespace = Namespace(u32::from_be_bytes(buf[4..8].try_into().ok()?));
        let kind = Kind::from_byte(buf[8]);
        let partition = Partition(u32::from_be_bytes(buf[9..13].try_into().ok()?));
        Some((
            KeyPrefix {
                realm,
                namespace,
                kind,
                partition,
            },
            PREFIX_LEN,
        ))
    }
}

/// Encode `!commit_ts`, so a NEWER version sorts before an older one.
///
/// The inversion is what makes "the current value" the FIRST row of a prefix
/// scan rather than requiring a scan to the end of the version chain. It is
/// also the property the backward-clock-jump test in `engram-runtime` exists to
/// prove rather than assert: a clock that moves backwards must not be able to
/// mint a duplicate or out-of-order key.
pub fn encode_commit_ts(commit_ts: u64, out: &mut Vec<u8>) {
    out.extend_from_slice(&(!commit_ts).to_be_bytes());
}

/// Read back a `commit_ts` written by [`encode_commit_ts`].
pub fn decode_commit_ts(buf: &[u8]) -> Option<u64> {
    if buf.len() < COMMIT_TS_LEN {
        return None;
    }
    Some(!u64::from_be_bytes(buf[..COMMIT_TS_LEN].try_into().ok()?))
}

/// Build a complete key: prefix, KIND-specific body, inverted commit ts.
///
/// `body` is opaque here on purpose. This function must not learn what any
/// particular KIND's body looks like — that is FC-2, and it is what lets a new
/// KIND ship without touching the encoder.
pub fn encode_key(prefix: &KeyPrefix, body: &[u8], commit_ts: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(PREFIX_LEN + body.len() + COMMIT_TS_LEN);
    prefix.encode_into(&mut out);
    out.extend_from_slice(body);
    encode_commit_ts(commit_ts, &mut out);
    counted!("key.encoded");
    out
}

/// Split a key into prefix, body and commit ts without interpreting the body.
pub fn split_key(key: &[u8]) -> Option<(KeyPrefix, &[u8], u64)> {
    let Some((prefix, n)) = KeyPrefix::decode(key) else {
        counted!("key.decode rejected");
        return None;
    };
    if key.len() < n + COMMIT_TS_LEN {
        counted!("key.decode rejected");
        return None;
    }
    let body = &key[n..key.len() - COMMIT_TS_LEN];
    let ts = decode_commit_ts(&key[key.len() - COMMIT_TS_LEN..])?;
    Some((prefix, body, ts))
}

// ─── D3 registration ────────────────────────────────────────────────────────

/// The key encoder, as a registered subsystem.
pub struct KeyCodec;

impl Subsystem for KeyCodec {
    const NAME: &'static str = "key-codec";

    fn register() -> Registration {
        Registration::new()
            .crash_point("key.before_prefix_write")
            // The declared set matches what the code can actually fire. A
            // previous revision also declared "decoded an unknown KIND through
            // the escape value" — the escape path is a RESERVATION with no
            // implementation, so the event could never fire and the coverage
            // floor would have reported an eternal gap that reads as a missing
            // test rather than a missing feature. Declare it when the escape
            // decoder exists.
            .sometimes("key.escaped a 0x00 in a variable component")
            .counter("key.encoded")
            .counter("key.decode rejected")
            .gate(
                Gate::new(
                    "byte order equals logical order",
                    Canary::new(
                        "encode commit_ts without inverting it and assert newest still sorts first",
                    ),
                )
                .and_canary(Canary::new("write a u32 component little-endian"))
                .and_canary(Canary::new(
                    "drop the escaping from a variable-length component",
                )),
            )
            .gate(Gate::new(
                "no user property value reaches a key position",
                Canary::new("implement Structural for a property-value type outside this crate"),
            ))
            .gate(Gate::new(
                "the frozen layout does not move",
                Canary::new(
                    "reorder namespace and realm and assert the golden vectors still match",
                ),
            ))
    }
}
