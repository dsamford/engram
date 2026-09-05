//! FC-1 and FC-2 — the KIND registry, its reserved blocks, and its escape.
//!
//! # Why blocks rather than a flat enum
//!
//! A flat list of KINDs allocates the next free number to whoever asks first.
//! Two consequences, both unfixable once data exists: a protected KIND ends up
//! interleaved with unprotected ones, so "is this protected?" becomes a lookup
//! table that can drift out of step with the storage gate; and an extension
//! KIND added downstream collides with a core one added upstream.
//!
//! Blocks make both structural. `Kind::is_protected` is a RANGE CHECK, so the
//! storage layer's refusal to accept a plaintext put cannot fall out of step
//! with the registry — there is nothing to keep in step.
//!
//! # The escape value
//!
//! One byte gives 256 KINDs, and the encoding is frozen. [`ESCAPE_KIND`] is the
//! reservation that stops that being a permanent ceiling: it means *the real
//! kind is a u16 at the start of the body*. Nothing uses it yet, and the point
//! is that nothing has to — the byte is spent now so it is available later,
//! which is the only time it can be spent at all.
//!
//! # FC-2: dispatch, not a match arm in the decoder
//!
//! The decoder in `lib.rs` never learns what a body looks like; it returns the
//! body as bytes. A KIND's decoder is registered against it. That is what lets
//! a new KIND ship without a change to the key codec — and the key codec is the
//! frozen artifact, so "without a change" is the whole requirement.

use std::collections::BTreeMap;

/// Reserved blocks of the one-byte KIND space.
///
/// The boundaries are frozen with the encoding. Widening a block later would
/// reclassify KINDs that already exist on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum KindBlock {
    /// `0x00` — never valid. A zeroed byte is the most likely corruption, and
    /// it must not decode as a real kind.
    Invalid,
    /// `0x01..=0x3F` — core graph structure.
    Core,
    /// `0x40..=0x7F` — reserved for future core use.
    ReservedCore,
    /// `0x80..=0xBF` — PROTECTED. The storage layer refuses a plaintext put to
    /// any KIND in this block, unconditionally and without consulting the
    /// planner.
    Protected,
    /// `0xC0..=0xFE` — reserved for extensions.
    Extension,
    /// `0xFF` — the escape. The real kind is a `u16` at the head of the body.
    Escape,
}

/// The escape byte. See the module header.
pub const ESCAPE_KIND: u8 = 0xFF;

/// A key's KIND discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Kind(u8);

impl Kind {
    // ── Core block ──────────────────────────────────────────────────────
    /// A node record.
    pub const NODE: Kind = Kind(0x01);
    /// An edge record.
    pub const EDGE: Kind = Kind(0x02);
    /// A property record.
    pub const PROPERTY: Kind = Kind(0x03);
    /// An adjacency entry.
    pub const ADJACENCY: Kind = Kind(0x04);
    /// A range-index entry (FC-11: its payload lives in the VALUE, never here).
    pub const INDEX_ENTRY: Kind = Kind(0x05);
    /// An index definition record (FC-9: open-ended body).
    pub const INDEX_DEF: Kind = Kind(0x06);
    /// A KV entry — R5's namespace.
    pub const KV: Kind = Kind(0x07);
    /// A blob-manifest entry — R9's engine-enforced half: the MVCC row that
    /// owns a blob's refcount, locator, verification state and key pointer.
    pub const BLOB_MANIFEST: Kind = Kind(0x08);
    /// A blob tombstone — the unlink queue. Appended in the same operation
    /// that drops the last reference; dequeued only on confirmed unlink.
    pub const BLOB_TOMBSTONE: Kind = Kind(0x09);
    /// A T1 engine blob — key-value-separated bytes in the store's own MVCC
    /// domain (4 KiB – 1 MiB; the CRDT-snapshot tier).
    pub const BLOB_T1: Kind = Kind(0x0A);

    // ── Protected block ─────────────────────────────────────────────────
    /// A protected property. Plaintext puts to this KIND are refused.
    pub const PROTECTED_PROPERTY: Kind = Kind(0x80);
    /// A protected index entry.
    pub const PROTECTED_INDEX_ENTRY: Kind = Kind(0x81);

    /// Wrap a raw byte. Any byte is representable — a decoder must be able to
    /// name what it found, including a kind this build does not know.
    pub const fn from_byte(b: u8) -> Self {
        Kind(b)
    }

    /// The raw byte.
    pub const fn byte(self) -> u8 {
        self.0
    }

    /// Which reserved block this KIND falls in.
    pub const fn block(self) -> KindBlock {
        match self.0 {
            0x00 => KindBlock::Invalid,
            0x01..=0x3F => KindBlock::Core,
            0x40..=0x7F => KindBlock::ReservedCore,
            0x80..=0xBF => KindBlock::Protected,
            0xC0..=0xFE => KindBlock::Extension,
            ESCAPE_KIND => KindBlock::Escape,
        }
    }

    /// Whether the storage layer must refuse a plaintext put.
    ///
    /// A RANGE CHECK, deliberately, not a lookup in a list of known protected
    /// kinds. A list can fall behind the registry; a range cannot. And the
    /// failure mode of falling behind is a protected property silently stored
    /// in plaintext — the plan calls a silent downgrade from encrypted to
    /// plaintext search "this codebase's dominant defect class in its worst
    /// possible form".
    ///
    /// Note it answers for kinds this build has never heard of: an unknown byte
    /// in the protected block is protected. Defaulting an unrecognised kind to
    /// UNprotected would make forward-compatibility and a plaintext leak the
    /// same event.
    pub const fn is_protected(self) -> bool {
        matches!(self.block(), KindBlock::Protected)
    }

    /// Whether this byte can legally appear as a KIND in a key.
    pub const fn is_valid(self) -> bool {
        !matches!(self.block(), KindBlock::Invalid)
    }
}

// ─── FC-2: KIND-dispatched body decoding ────────────────────────────────────

/// What a KIND's body decodes to, in terms the key codec does not interpret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedBody {
    /// Human-readable rendering, for traces and diagnostics.
    pub summary: String,
    /// Bytes consumed. A decoder that does not consume the whole body is a
    /// decoder that is wrong about the format, not one that is being lenient.
    pub consumed: usize,
}

/// A body decoder for one KIND.
pub type BodyDecoder = fn(&[u8]) -> Option<DecodedBody>;

/// The dispatch table.
///
/// The point of FC-2: adding a KIND registers a decoder here and touches
/// nothing in the frozen codec. A `match` in the decoder would make every new
/// KIND an edit to artifact #1, which is precisely the file that must stop
/// changing.
#[derive(Debug, Default)]
pub struct KindRegistry {
    decoders: BTreeMap<u8, BodyDecoder>,
}

impl KindRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a decoder for one KIND.
    ///
    /// Refuses to overwrite: two decoders for one KIND is a collision between
    /// two features, and the one that loses would decode as the one that won —
    /// producing plausible rows attributed to the wrong thing.
    pub fn register(&mut self, kind: Kind, decoder: BodyDecoder) -> Result<(), KindError> {
        if !kind.is_valid() {
            return Err(KindError::Invalid(kind.byte()));
        }
        if self.decoders.contains_key(&kind.byte()) {
            return Err(KindError::AlreadyRegistered(kind.byte()));
        }
        self.decoders.insert(kind.byte(), decoder);
        Ok(())
    }

    /// Decode a body for `kind`, or report that nothing is registered.
    ///
    /// `Unregistered` is a distinct outcome from a decode FAILURE. A build that
    /// has never heard of a KIND and a body that is corrupt are different
    /// facts: the first is a forward-compatibility event and the row should be
    /// passed through untouched, the second is data loss. Collapsing them would
    /// make an older reader report corruption for every key a newer writer
    /// produced.
    pub fn decode(&self, kind: Kind, body: &[u8]) -> BodyOutcome {
        match self.decoders.get(&kind.byte()) {
            None => BodyOutcome::Unregistered,
            Some(d) => match d(body) {
                Some(v) if v.consumed == body.len() => BodyOutcome::Decoded(v),
                Some(v) => BodyOutcome::Trailing {
                    consumed: v.consumed,
                    len: body.len(),
                },
                None => BodyOutcome::Corrupt,
            },
        }
    }

    /// How many decoders are registered.
    pub fn len(&self) -> usize {
        self.decoders.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.decoders.is_empty()
    }
}

/// The result of asking the registry to decode a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyOutcome {
    /// Decoded, consuming the whole body.
    Decoded(DecodedBody),
    /// No decoder for this KIND in this build — forward compatibility, not an error.
    Unregistered,
    /// A decoder claimed fewer bytes than the body holds.
    Trailing {
        /// Bytes the decoder consumed.
        consumed: usize,
        /// Bytes the body actually held.
        len: usize,
    },
    /// The decoder rejected the bytes.
    Corrupt,
}

/// Registry errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KindError {
    /// A KIND byte that may not appear in a key.
    Invalid(u8),
    /// A decoder is already registered for this KIND.
    AlreadyRegistered(u8),
}

impl std::fmt::Display for KindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KindError::Invalid(b) => write!(f, "KIND 0x{b:02x} is not valid in a key"),
            KindError::AlreadyRegistered(b) => {
                write!(f, "a decoder is already registered for KIND 0x{b:02x}")
            }
        }
    }
}

impl std::error::Error for KindError {}
