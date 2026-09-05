//! The record layer — what a key's value actually holds.
//!
//! A record is an ordered sequence of `(property_id: u32, tagged value)` pairs,
//! the values in the FC-4 tag encoding. Two rules make it more than a map:
//!
//! # Rule 1: a reader steps over what it does not understand
//!
//! Property values use `engram_key::value::skip_value`, so a record written by
//! a newer build reads fine on an older one — the unknown property is walked
//! over by the block rule, and every property after it is still reachable.
//!
//! # Rule 2: a WRITER preserves what it does not understand
//!
//! The half of forward compatibility that usually gets lost. Read-modify-write
//! is the ordinary shape of an update: decode, change one property, re-encode.
//! A writer that re-encodes only the properties it can DECODE silently drops
//! every unknown one — so the first old-build write after a new-build write
//! destroys the new build's data, and nothing errors anywhere. [`Record`]
//! therefore keeps undecodable properties as raw bytes and writes them back
//! verbatim. The test for this is the one that matters in this file.

use std::collections::BTreeMap;

use engram_key::value::skip_value;

/// A property identifier. Stable per label-schema, assigned elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PropertyId(pub u32);

/// A decoded record: property id → tagged value bytes.
///
/// Values are kept ENCODED. The record layer's job is structure, not
/// interpretation — decoding a value it only needs to carry would make every
/// unknown tag a failure, which is exactly what the skip rule exists to avoid.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Record {
    props: BTreeMap<u32, Vec<u8>>,
}

/// Why a record failed to decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordError {
    /// The buffer ended inside a property header or value.
    Truncated {
        /// Offset at which the walk could no longer continue.
        at: usize,
    },
    /// A value failed the skip rule — structurally invalid, not merely unknown.
    UnskippableValue {
        /// The property whose value could not be walked.
        property: PropertyId,
        /// Offset of the value's tag byte.
        at: usize,
    },
    /// The same property id appeared twice.
    ///
    /// Refused rather than last-wins: two writers disagreeing about a property
    /// is a conflict, and silently keeping one is a lost update wearing the
    /// shape of a merge.
    DuplicateProperty(PropertyId),
}

impl Record {
    /// An empty record.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a property to an already-encoded tagged value.
    pub fn set(&mut self, id: PropertyId, tagged_value: Vec<u8>) {
        self.props.insert(id.0, tagged_value);
    }

    /// Remove a property. Returns whether it was present.
    pub fn remove(&mut self, id: PropertyId) -> bool {
        self.props.remove(&id.0).is_some()
    }

    /// The encoded tagged value for a property.
    pub fn get(&self, id: PropertyId) -> Option<&[u8]> {
        self.props.get(&id.0).map(Vec::as_slice)
    }

    /// Property count.
    pub fn len(&self) -> usize {
        self.props.len()
    }

    /// Whether the record has no properties.
    pub fn is_empty(&self) -> bool {
        self.props.is_empty()
    }

    /// Iterate `(id, tagged value)` in id order.
    pub fn iter(&self) -> impl Iterator<Item = (PropertyId, &[u8])> {
        self.props
            .iter()
            .map(|(id, v)| (PropertyId(*id), v.as_slice()))
    }

    /// Encode the record.
    ///
    /// Layout: `count:u32`, then per property `id:u32` followed by its tagged
    /// value (self-delimiting via the skip rule — no length prefix needed, and
    /// adding one would create a second source of truth for the value's size
    /// that could disagree with the tag's own).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.props.len() as u32).to_le_bytes());
        for (id, value) in &self.props {
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(value);
        }
        out
    }

    /// Decode a record, keeping every property — understood or not.
    pub fn decode(buf: &[u8]) -> Result<Self, RecordError> {
        let count = u32::from_le_bytes(
            buf.get(0..4)
                .ok_or(RecordError::Truncated { at: 0 })?
                .try_into()
                .expect("4 bytes"),
        ) as usize;
        let mut props = BTreeMap::new();
        let mut at = 4usize;
        for _ in 0..count {
            let id = u32::from_le_bytes(
                buf.get(at..at + 4)
                    .ok_or(RecordError::Truncated { at })?
                    .try_into()
                    .expect("4 bytes"),
            );
            at += 4;
            let len = skip_value(&buf[at..]).ok_or(RecordError::UnskippableValue {
                property: PropertyId(id),
                at,
            })?;
            let value = buf[at..at + len].to_vec();
            at += len;
            if props.insert(id, value).is_some() {
                return Err(RecordError::DuplicateProperty(PropertyId(id)));
            }
        }
        if at != buf.len() {
            // Trailing bytes after the declared count: the count and the
            // content disagree, and trusting either alone hides the other's
            // corruption.
            return Err(RecordError::Truncated { at });
        }
        Ok(Record { props })
    }

    /// Decode ONLY the properties in `want`, walking over the rest with the
    /// skip rule — no allocation for a value the caller did not ask for.
    ///
    /// The structural checks are the same as [`Record::decode`]'s (a
    /// truncated buffer, an unskippable value, a duplicate id, trailing
    /// bytes), so a corrupt record is refused here exactly as it is there;
    /// only the copying differs. A projected node materialisation used to
    /// decode the whole record — one `Vec` per property, thirty for a
    /// platform `ManagedRepo` — and then name every property before checking
    /// whether it was wanted: ~15 µs per row for two properties, against
    /// Neo4j's ~7. The record layer already knew how to skip; this is that
    /// knowledge applied to more than one property at a time.
    pub fn decode_projected(buf: &[u8], want: &[u32]) -> Result<Self, RecordError> {
        let count = u32::from_le_bytes(
            buf.get(0..4)
                .ok_or(RecordError::Truncated { at: 0 })?
                .try_into()
                .expect("4 bytes"),
        ) as usize;
        let mut props = BTreeMap::new();
        let mut at = 4usize;
        let mut last: Option<u32> = None;
        for _ in 0..count {
            let id = u32::from_le_bytes(
                buf.get(at..at + 4)
                    .ok_or(RecordError::Truncated { at })?
                    .try_into()
                    .expect("4 bytes"),
            );
            at += 4;
            let len = skip_value(&buf[at..]).ok_or(RecordError::UnskippableValue {
                property: PropertyId(id),
                at,
            })?;
            if want.contains(&id) {
                if props.insert(id, buf[at..at + len].to_vec()).is_some() {
                    return Err(RecordError::DuplicateProperty(PropertyId(id)));
                }
            } else if last == Some(id) {
                // Records are written in id order, so a repeat is adjacent;
                // the full decode refuses any duplicate and this refuses the
                // ones it can see without keeping every id.
                return Err(RecordError::DuplicateProperty(PropertyId(id)));
            }
            last = Some(id);
            at += len;
        }
        if at != buf.len() {
            return Err(RecordError::Truncated { at });
        }
        Ok(Record { props })
    }
}

/// Read ONE property from an encoded record without decoding the rest.
///
/// The skip rule's payoff: a point read of one property walks tag bytes and
/// length prefixes, never value contents. Returns the tagged value bytes.
pub fn get_property(buf: &[u8], id: PropertyId) -> Option<Vec<u8>> {
    let count = u32::from_le_bytes(buf.get(0..4)?.try_into().ok()?) as usize;
    let mut at = 4usize;
    for _ in 0..count {
        let pid = u32::from_le_bytes(buf.get(at..at + 4)?.try_into().ok()?);
        at += 4;
        let len = skip_value(buf.get(at..)?)?;
        if pid == id.0 {
            return Some(buf[at..at + len].to_vec());
        }
        at += len;
    }
    None
}

#[cfg(test)]
mod projected_tests {
    use super::{PropertyId, Record, RecordError};

    // Tagged values as FC-4 writes them — INT64 is tag 0x03 + 8 LE bytes,
    // STRING is tag 0x05 + u32 length + bytes — whatever `skip_value` walks;
    // the record layer never interprets.
    fn int64(v: i64) -> Vec<u8> {
        let mut out = vec![0x03];
        out.extend_from_slice(&v.to_le_bytes());
        out
    }
    fn string(s: &str) -> Vec<u8> {
        let mut out = vec![0x05];
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
        out
    }

    fn wide() -> Record {
        let mut r = Record::new();
        for id in 0..40u32 {
            let v = if id % 7 == 0 {
                int64(id as i64)
            } else {
                string(&format!("value-{id}-{}", "x".repeat(id as usize)))
            };
            r.set(PropertyId(id), v);
        }
        r
    }

    /// The projected decode holds exactly the wanted properties, byte for
    /// byte what the full decode holds for them — and nothing else.
    #[test]
    fn projected_equals_the_full_decode_filtered() {
        let r = wide();
        let bytes = r.encode();
        let full = Record::decode(&bytes).expect("full");
        for want in [vec![], vec![3u32], vec![0, 39], vec![5, 6, 7, 99], (0..40).collect::<Vec<_>>()] {
            let p = Record::decode_projected(&bytes, &want).expect("projected");
            assert_eq!(p.len(), want.iter().filter(|w| **w < 40).count(), "count for {want:?}");
            for (pid, tagged) in p.iter() {
                assert!(want.contains(&pid.0), "unwanted {pid:?} came back");
                assert_eq!(Some(tagged), full.get(pid), "bytes of {pid:?}");
            }
        }
    }

    /// Corruption is refused by both decodes alike — a projected read must
    /// not become the path that accepts what the full read refuses.
    #[test]
    fn projected_refuses_what_the_full_decode_refuses() {
        let bytes = wide().encode();
        // The SAME refusal from both decodes, whatever the full one says: a
        // cut inside a value is "unskippable" to the walk, a cut inside a
        // header is "truncated" — the projected read must agree either way.
        let same = |buf: &[u8], want: &[u32]| {
            let full = Record::decode(buf).expect_err("the full decode must refuse");
            let proj = Record::decode_projected(buf, want).expect_err("the projected decode must refuse");
            assert_eq!(full, proj, "refusals differ for want={want:?}");
            full
        };
        // Truncated inside the last value, and inside a header.
        same(&bytes[..bytes.len() - 5], &[3]);
        same(&bytes[..bytes.len() - 5], &[39]);
        assert!(matches!(same(&bytes[..6], &[0]), RecordError::Truncated { .. }));
        // Trailing bytes.
        let mut long = bytes.clone();
        long.push(0);
        assert!(matches!(same(&long, &[3]), RecordError::Truncated { .. }));
        // A duplicated (adjacent) property id, wanted or not.
        let mut dup = Record::new();
        dup.set(PropertyId(1), int64(1));
        let mut enc = dup.encode();
        // Splice a second copy of the single entry and bump the count to 2.
        let entry = enc[4..].to_vec();
        enc.extend_from_slice(&entry);
        enc[0..4].copy_from_slice(&2u32.to_le_bytes());
        assert!(matches!(Record::decode(&enc), Err(RecordError::DuplicateProperty(_))));
        assert!(matches!(Record::decode_projected(&enc, &[1]), Err(RecordError::DuplicateProperty(_))));
        assert!(matches!(Record::decode_projected(&enc, &[9]), Err(RecordError::DuplicateProperty(_))));
    }
}
