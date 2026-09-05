//! **FC-4/5/6** — the property-value tag registry.
//!
//! The plan stars FC-4 as the highest-value reservation on the R13 list, and
//! the reason is the skip rule: *"a skip-unknown rule that actually holds."*
//!
//! # Why skipping is the whole feature
//!
//! A record holds many properties. When an older reader meets a value written
//! by a newer writer, it must skip PAST that value to reach the ones it does
//! understand. If it cannot — if skipping requires knowing the type — then
//! every new value type is a format break for every deployed reader, and the
//! registry's reserved blocks are decorative.
//!
//! So the rule here is structural: **every tag's payload length is computable
//! from the tag byte plus, for variable types, a length prefix that is always
//! in the same place.** A reader never needs to understand a value to step over
//! it. [`skip_value`] does exactly that, and its tests include tags this build
//! has never assigned.
//!
//! # FC-6: POINT and VECTOR are different tags, permanently
//!
//! Both are fixed-width numeric arrays; a `Point2D` and a 2-dim vector have the
//! SAME bytes. Sharing a tag would make them indistinguishable on disk — and
//! since one is a coordinate that feeds a space-filling curve and the other is
//! an embedding that feeds a similarity index, misreading one as the other
//! produces plausible numbers in every downstream computation. That is
//! unrecoverable precisely because nothing errors.
//!
//! # This is NOT the key encoding
//!
//! Values live in record payloads, which are ciphertext under the tenant DEK.
//! Nothing here is memcomparable and nothing here may be placed in a key — the
//! sealed `Structural` trait in `lib.rs` enforces that from the other side.
//! Values use little-endian, deliberately breaking symmetry with the key
//! encoding: an encoder that "helpfully" reuses key components for values (or
//! the reverse) fails golden tests immediately rather than working by accident
//! until the first multi-byte value.

use std::collections::BTreeMap;

// ─── The tag space ──────────────────────────────────────────────────────────

/// Reserved blocks of the one-byte tag space.
///
/// Frozen with the format, like [`crate::KindBlock`]. The parallel structure is
/// deliberate — one mental model for both registries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TagBlock {
    /// `0x00` — never a valid tag; a zeroed byte must not decode as a value.
    Invalid,
    /// `0x01..=0x3F` — core scalar and container types.
    Core,
    /// `0x40..=0x6F` — reserved for future core use.
    ReservedCore,
    /// `0x70..=0x9F` — spatial and other fixed-layout structures (POINT lives
    /// here, away from VECTOR by construction).
    Spatial,
    /// `0xA0..=0xCF` — vectors and other array-of-numeric payloads.
    Vector,
    /// `0xD0..=0xFE` — extensions.
    Extension,
    /// `0xFF` — escape: the real tag is a u16 that follows, then a u32 length.
    Escape,
}

/// The escape tag. Its payload is self-describing: `u16` real tag, `u32`
/// length, then that many bytes — so even the escape is skippable by a reader
/// that has no idea what the real tag means.
pub const ESCAPE_TAG: u8 = 0xFF;

/// A property value's type tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tag(u8);

impl Tag {
    // ── Core scalars ────────────────────────────────────────────────────
    /// Explicit null. Distinct from an ABSENT property: Neo4j's `properties(n)`
    /// omits null columns and this codebase has been bitten by that collapse
    /// repeatedly — the distinction is load-bearing and it starts on disk.
    pub const NULL: Tag = Tag(0x01);
    /// Boolean, 1 byte.
    pub const BOOL: Tag = Tag(0x02);
    /// Signed 64-bit integer, little-endian.
    pub const INT64: Tag = Tag(0x03);
    /// IEEE-754 double, little-endian.
    pub const FLOAT64: Tag = Tag(0x04);
    /// UTF-8 string, u32 length prefix.
    pub const STRING: Tag = Tag(0x05);
    /// Raw bytes, u32 length prefix.
    pub const BYTES: Tag = Tag(0x06);
    /// Homogeneous list: u32 count, element tag, then packed payloads.
    pub const LIST: Tag = Tag(0x07);
    /// A list of VARIABLE-WIDTH elements (temporals): length-prefixed (the skip
    /// rule steps over it by its u32 length), then a u32 count and each element
    /// as its own tagged, length-framed encoding. Packed lists cannot hold
    /// variable-width elements, so this envelope carries them instead.
    pub const LIST_TEMPORAL: Tag = Tag(0x08);
    // ── Temporal (Bolt-facing; payloads mirror what PackStream needs) ───
    /// Days since the epoch, i64.
    pub const DATE: Tag = Tag(0x10);
    /// Nanoseconds since midnight, i64, plus a UTC-offset i32 in seconds.
    pub const TIME: Tag = Tag(0x11);
    /// Nanoseconds since midnight, i64, no zone.
    pub const LOCAL_TIME: Tag = Tag(0x12);
    /// Epoch seconds i64 + nanos u32 + offset seconds i32.
    pub const DATETIME_OFFSET: Tag = Tag(0x13);
    /// Epoch seconds i64 + nanos u32 + zone id as a u32-length string.
    pub const DATETIME_ZONE_ID: Tag = Tag(0x14);
    /// Epoch seconds i64 + nanos u32, no zone.
    pub const LOCAL_DATETIME: Tag = Tag(0x15);
    /// Months i64 + days i64 + seconds i64 + nanos i32 — Bolt's shape exactly;
    /// a Duration is NOT a number of seconds and collapsing it loses calendar
    /// arithmetic (a month is not a fixed length).
    pub const DURATION: Tag = Tag(0x16);
    // ── Spatial block (FC-5) ────────────────────────────────────────────
    /// FC-5: `srid:u32 │ x:f64 │ y:f64` — 20 bytes, exactly.
    pub const POINT_2D: Tag = Tag(0x70);
    /// FC-5: `srid:u32 │ x:f64 │ y:f64 │ z:f64` — 28 bytes, exactly.
    pub const POINT_3D: Tag = Tag(0x71);
    // ── Vector block (FC-6: a DIFFERENT block from POINT) ───────────────
    //
    // The u32 prefix on every vector tag is the BYTE length, never the
    // dimension — the skip rule demands it, because `skip_value` steps over
    // length-prefixed payloads by that u32 without knowing the element width.
    // A dim prefix here once desynchronised every record walk containing a
    // vector: `skip_value` read dim as bytes, landed mid-payload, and
    // `get_property` reported the property absent. The dimension is DERIVED:
    // len/4 for f32, len/8 for f64, len for i8, with len % width a refusal.
    /// f32 vector: u32 BYTE length (= 4 × dim), then the little-endian f32s.
    pub const VECTOR_F32: Tag = Tag(0xA0);
    /// f64 vector: u32 BYTE length (= 8 × dim), then the little-endian f64s.
    pub const VECTOR_F64: Tag = Tag(0xA1);
    /// int8 vector (the X2 storage form): u32 BYTE length (= dim), then the i8s.
    pub const VECTOR_I8: Tag = Tag(0xA2);

    /// Wrap a raw byte.
    pub const fn from_byte(b: u8) -> Self {
        Tag(b)
    }

    /// The raw byte.
    pub const fn byte(self) -> u8 {
        self.0
    }

    /// Which block this tag falls in.
    pub const fn block(self) -> TagBlock {
        match self.0 {
            0x00 => TagBlock::Invalid,
            0x01..=0x3F => TagBlock::Core,
            0x40..=0x6F => TagBlock::ReservedCore,
            0x70..=0x9F => TagBlock::Spatial,
            0xA0..=0xCF => TagBlock::Vector,
            0xD0..=0xFE => TagBlock::Extension,
            ESCAPE_TAG => TagBlock::Escape,
        }
    }

    /// Whether this byte may appear as a tag at all.
    pub const fn is_valid(self) -> bool {
        !matches!(self.block(), TagBlock::Invalid)
    }
}

// ─── The skip rule ──────────────────────────────────────────────────────────

/// How a tag's payload length is determined — the property FC-4 turns on.
///
/// This is a closed, structural classification: every tag ever assigned MUST
/// fall into one of these three shapes, and the shape is derivable from the
/// TAG BYTE ALONE for the fixed cases. That is what lets [`skip_value`] step
/// over a value it does not understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadShape {
    /// Exactly `N` bytes follow the tag.
    Fixed(usize),
    /// A u32 length prefix follows the tag, then that many bytes.
    LengthPrefixed,
    /// LIST: u32 count + element tag, then count packed element payloads.
    List,
    /// ESCAPE: u16 real tag + u32 length, then that many bytes.
    Escaped,
}

/// The payload shape for a tag, INCLUDING tags this build has never assigned.
///
/// Unknown tags resolve by BLOCK: the reservation is not just of numbers but of
/// shapes. An unknown Core/Spatial/Extension tag is length-prefixed; an unknown
/// Vector tag is length-prefixed. That rule is what makes "skip-unknown
/// actually holds" true rather than aspirational — a reader can skip a value
/// whose tag was assigned years after it shipped, because the BLOCK told it
/// where the length lives.
pub fn payload_shape(tag: Tag) -> Option<PayloadShape> {
    // Known fixed-width tags first: their exact lengths are frozen (FC-5).
    let fixed = match tag {
        Tag::NULL => Some(0),
        Tag::BOOL => Some(1),
        Tag::INT64 | Tag::FLOAT64 => Some(8),
        Tag::DATE => Some(8),
        Tag::TIME => Some(12),
        Tag::LOCAL_TIME => Some(8),
        Tag::DATETIME_OFFSET => Some(16),
        Tag::LOCAL_DATETIME => Some(12),
        Tag::DURATION => Some(28),
        Tag::POINT_2D => Some(20),
        Tag::POINT_3D => Some(28),
        _ => None,
    };
    if let Some(n) = fixed {
        return Some(PayloadShape::Fixed(n));
    }
    match tag {
        Tag::LIST => Some(PayloadShape::List),
        t if t.byte() == ESCAPE_TAG => Some(PayloadShape::Escaped),
        t if !t.is_valid() => None,
        // EVERY other tag — known length-prefixed ones (STRING, BYTES,
        // DATETIME_ZONE_ID, the vectors) and every UNKNOWN tag in a valid
        // block — carries a u32 length prefix. New fixed-width tags are
        // therefore only assignable to bytes listed above BEFORE any reader
        // ships; after that, a new type must be length-prefixed. That
        // constraint is the price of the skip rule and it is accepted here,
        // in writing, rather than discovered.
        _ => Some(PayloadShape::LengthPrefixed),
    }
}

/// Step over one value, returning the total bytes it occupies (tag included).
///
/// Works for tags this build has never seen — that is its purpose. Returns
/// `None` only for a truncated or structurally invalid buffer, and refuses
/// rather than guessing: a mis-skipped value desynchronises every property
/// after it, which reads as corruption arbitrarily far from the cause.
pub fn skip_value(buf: &[u8]) -> Option<usize> {
    let tag = Tag::from_byte(*buf.first()?);
    match payload_shape(tag)? {
        PayloadShape::Fixed(n) => {
            if buf.len() < 1 + n {
                return None;
            }
            Some(1 + n)
        }
        PayloadShape::LengthPrefixed => {
            let len = read_u32(buf.get(1..5)?)? as usize;
            if buf.len() < 5 + len {
                return None;
            }
            Some(5 + len)
        }
        PayloadShape::List => {
            let count = read_u32(buf.get(1..5)?)?;
            let elem = Tag::from_byte(*buf.get(5)?);
            // Elements are PACKED (no per-element tag), so the element payload
            // size must be derivable from the element tag alone.
            let per = match payload_shape(elem)? {
                PayloadShape::Fixed(n) => n,
                // A list of variable-length elements writes each element as a
                // full tagged value instead; that list uses an element tag of
                // ESCAPE_TAG minus nothing — it is simply not packed. Packed
                // lists of variable elements are unrepresentable on purpose.
                _ => return None,
            };
            // In u64, so overflow is IMPOSSIBLE on every target rather than
            // guarded on some: count is at most u32::MAX and per at most 28,
            // so the product tops out near 1.2e11 — far under u64::MAX, but
            // PAST u32::MAX, which is exactly where a 32-bit usize would have
            // wrapped a huge declared count into a small "valid" total and
            // skipped on a guess. A canary proved checked arithmetic here was
            // untestable on a 64-bit host (the wrap is unreachable), so the
            // property is made structural instead of tested.
            let total = 6u64 + (count as u64) * (per as u64);
            if (buf.len() as u64) < total {
                return None;
            }
            Some(total as usize)
        }
        PayloadShape::Escaped => {
            // u16 real tag + u32 len.
            let len = read_u32(buf.get(3..7)?)? as usize;
            let total = 7usize.checked_add(len)?;
            if buf.len() < total {
                return None;
            }
            Some(total)
        }
    }
}

fn read_u32(b: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(b.try_into().ok()?))
}

// ─── FC-5: POINT, written down exactly ──────────────────────────────────────

/// A 2-D point: an SRID and two coordinates. 20 payload bytes, frozen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point2D {
    /// Spatial reference system id (e.g. 4326 for WGS-84, 7203 cartesian).
    pub srid: u32,
    /// X, or longitude under a geographic SRID.
    pub x: f64,
    /// Y, or latitude.
    pub y: f64,
}

/// A 3-D point. 28 payload bytes, frozen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point3D {
    /// Spatial reference system id.
    pub srid: u32,
    /// X.
    pub x: f64,
    /// Y.
    pub y: f64,
    /// Z, or height.
    pub z: f64,
}

impl Point2D {
    /// Encode as a tagged value.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(21);
        out.push(Tag::POINT_2D.byte());
        out.extend_from_slice(&self.srid.to_le_bytes());
        out.extend_from_slice(&self.x.to_le_bytes());
        out.extend_from_slice(&self.y.to_le_bytes());
        out
    }

    /// Decode from a tagged value.
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() != 21 || buf[0] != Tag::POINT_2D.byte() {
            return None;
        }
        Some(Point2D {
            srid: u32::from_le_bytes(buf[1..5].try_into().ok()?),
            x: f64::from_le_bytes(buf[5..13].try_into().ok()?),
            y: f64::from_le_bytes(buf[13..21].try_into().ok()?),
        })
    }
}

impl Point3D {
    /// Encode as a tagged value.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(29);
        out.push(Tag::POINT_3D.byte());
        out.extend_from_slice(&self.srid.to_le_bytes());
        out.extend_from_slice(&self.x.to_le_bytes());
        out.extend_from_slice(&self.y.to_le_bytes());
        out.extend_from_slice(&self.z.to_le_bytes());
        out
    }

    /// Decode from a tagged value.
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() != 29 || buf[0] != Tag::POINT_3D.byte() {
            return None;
        }
        Some(Point3D {
            srid: u32::from_le_bytes(buf[1..5].try_into().ok()?),
            x: f64::from_le_bytes(buf[5..13].try_into().ok()?),
            y: f64::from_le_bytes(buf[13..21].try_into().ok()?),
            z: f64::from_le_bytes(buf[21..29].try_into().ok()?),
        })
    }
}

// ─── FC-10: the histogram uses THESE tags ───────────────────────────────────

/// A per-tag count, for the constraint validator's type histogram.
///
/// FC-10's requirement is only that the histogram and the value encoding share
/// ONE tag vocabulary — two vocabularies drift, and a histogram keyed on its
/// own notion of "type" reports a clean column while the encoder writes tags
/// the validator has never heard of. Keyed by the raw byte so unknown tags are
/// COUNTED rather than folded into an "other" bucket that hides growth.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TagHistogram {
    counts: BTreeMap<u8, u64>,
}

impl TagHistogram {
    /// An empty histogram.
    pub fn new() -> Self {
        Self::default()
    }

    /// Count one value by its tag byte.
    pub fn record(&mut self, tag: Tag) {
        *self.counts.entry(tag.byte()).or_insert(0) += 1;
    }

    /// The count for one tag.
    pub fn count(&self, tag: Tag) -> u64 {
        self.counts.get(&tag.byte()).copied().unwrap_or(0)
    }

    /// Every (tag, count), sorted by tag byte.
    pub fn entries(&self) -> impl Iterator<Item = (Tag, u64)> + '_ {
        self.counts.iter().map(|(b, c)| (Tag::from_byte(*b), *c))
    }

    /// Total values recorded.
    pub fn total(&self) -> u64 {
        self.counts.values().sum()
    }
}
