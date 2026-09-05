//! PackStream v2 — the codec, and the thirteen structures the driver
//! actually decodes (tags verified from neo4j-driver 5.28.3's source, per
//! the plan's census).
//!
//! Values encode from and decode to [`engram_cypher::Value`]; graph entities
//! carry the Bolt-5 `element_id` STRING alongside the legacy integer id,
//! because `elementId()` is the corpus's primary identity (65 sites) and the
//! Bolt 4.x Node structure's lack of it is why 5.0 is the FLOOR.

use std::collections::BTreeMap;

use engram_cypher::Value;

/// Why a decode refused. Positioned, and never a guess: a truncated stream
/// is `Truncated`, an unknown marker names the byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackError {
    /// More bytes were needed.
    Truncated {
        /// Offset at which the stream ended early.
        at: usize,
    },
    /// A marker byte this decoder does not know.
    UnknownMarker {
        /// The byte.
        marker: u8,
        /// Offset.
        at: usize,
    },
    /// A structure with an unexpected tag or field count.
    BadStructure {
        /// The tag.
        tag: u8,
        /// What was wrong.
        detail: String,
    },
    /// Nesting deeper than [`MAX_DEPTH`].
    ///
    /// A separate variant rather than a `BadStructure`, because this one is
    /// reached by hostile input rather than by a malformed message, and an
    /// operator reading a log needs to tell "a driver sent something odd" from
    /// "someone is trying to overflow the stack".
    TooDeep {
        /// The limit that was exceeded.
        limit: u32,
        /// Offset at which the limit was hit.
        at: usize,
    },
    /// A string that was not UTF-8.
    BadUtf8 {
        /// Offset.
        at: usize,
    },
    /// A value that cannot be REPRESENTED on the wire (should be unreachable
    /// from the engine's own values; kept for the encoder's totality).
    Unrepresentable(String),
}

impl std::fmt::Display for PackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackError::Truncated { at } => write!(f, "stream truncated at byte {at}"),
            PackError::UnknownMarker { marker, at } => {
                write!(f, "unknown PackStream marker 0x{marker:02X} at byte {at}")
            }
            PackError::BadStructure { tag, detail } => {
                write!(f, "bad structure 0x{tag:02X}: {detail}")
            }
            PackError::TooDeep { limit, at } => {
                write!(f, "nesting deeper than {limit} at byte {at}")
            }
            PackError::BadUtf8 { at } => write!(f, "invalid UTF-8 at byte {at}"),
            PackError::Unrepresentable(d) => write!(f, "unrepresentable on the wire: {d}"),
        }
    }
}

impl std::error::Error for PackError {}

// ─── Structure tags (verified against the installed driver) ────────────────

/// `Node` — 4 fields: id, labels, properties, element_id.
pub const SIG_NODE: u8 = 0x4E;
/// `Relationship` — 8 fields.
pub const SIG_RELATIONSHIP: u8 = 0x52;
/// `UnboundRelationship` — 4 fields.
pub const SIG_UNBOUND_REL: u8 = 0x72;
/// `Path` — 3 fields.
pub const SIG_PATH: u8 = 0x50;
/// `Date` — days since epoch.
pub const SIG_DATE: u8 = 0x44;
/// `Time` — nanos + tz offset seconds.
pub const SIG_TIME: u8 = 0x54;
/// `LocalTime`.
pub const SIG_LOCAL_TIME: u8 = 0x74;
/// `DateTime` with zone offset.
pub const SIG_DATETIME_OFFSET: u8 = 0x49;
/// `DateTime` with zone id.
pub const SIG_DATETIME_ZONE_ID: u8 = 0x69;
/// `LocalDateTime`.
pub const SIG_LOCAL_DATETIME: u8 = 0x64;
/// `Duration`.
pub const SIG_DURATION: u8 = 0x45;
/// `Point2D` (FC-7: must round-trip).
pub const SIG_POINT_2D: u8 = 0x58;
/// `Point3D`.
pub const SIG_POINT_3D: u8 = 0x59;
/// `Vector` (Bolt 6.0): `(type_marker, data)` — a homogeneous numeric vector
/// whose elements are packed big-endian in `data` at the width `type_marker`
/// names (`C8`/`C9`/`CA`/`CB` int8..int64, `C6` float32, `C1` float64; the
/// marker is not repeated in `data`). Decoded into a plain list of numbers:
/// the engine has no vector-typed VALUE, and a list is what every vector
/// function and index here already consumes.
pub const SIG_VECTOR: u8 = 0x56;
/// `UnsupportedType` (Bolt 6.0): `(name, minimum_protocol_major,
/// minimum_protocol_minor, extra{message})` — what a server sends in place
/// of a value the negotiated version cannot carry. Named here so a client
/// of this crate can recognise it; this server never has cause to emit it
/// (every value it holds has a 5.0 encoding).
pub const SIG_UNSUPPORTED_TYPE: u8 = 0x3F;

/// A decoded PackStream item: a value, or a structure (message or graph
/// type) with its tag and fields.
#[derive(Debug, Clone, PartialEq)]
pub enum Pack {
    /// A plain value.
    Value(Value),
    /// A byte array (markers `CC`/`CD`/`CE`). Kept apart from [`Value`]
    /// because the engine has no bytes type: as a parameter it becomes a
    /// list of the byte values, and inside a [`SIG_VECTOR`] structure it is
    /// the packed element data.
    Bytes(Vec<u8>),
    /// A structure.
    Struct {
        /// The signature byte.
        tag: u8,
        /// The fields, in order.
        fields: Vec<Pack>,
    },
}

impl Pack {
    /// The value inside, or a refusal naming the structure.
    pub fn into_value(self) -> Result<Value, PackError> {
        match self {
            Pack::Value(v) => Ok(v),
            Pack::Bytes(b) => Ok(Value::List(b.into_iter().map(|x| Value::Int(i64::from(x))).collect())),
            Pack::Struct { tag, .. } => Err(PackError::BadStructure {
                tag,
                detail: "expected a plain value".into(),
            }),
        }
    }
}

// ─── Encoding ───────────────────────────────────────────────────────────────

/// Encode one value.
pub fn encode_value(v: &Value, out: &mut Vec<u8>) -> Result<(), PackError> {
    match v {
        Value::Null => out.push(0xC0),
        Value::Bool(false) => out.push(0xC2),
        Value::Bool(true) => out.push(0xC3),
        Value::Int(i) => encode_int(*i, out),
        Value::Float(f) => {
            out.push(0xC1);
            out.extend_from_slice(&f.to_be_bytes());
        }
        Value::Str(s) => encode_string(s, out),
        Value::List(items) => {
            encode_size(items.len(), 0x90, 0xD4, out);
            for item in items {
                encode_value(item, out)?;
            }
        }
        // A PATH is packed as its `[node, rel, node, …]` trail list for now — a
        // faithful, non-crashing shape. The dedicated Bolt Path struct (SIG_PATH:
        // unique-node list + unbound-rel list + index sequence) is a follow-up;
        // no current caller round-trips a path through Bolt.
        Value::Path(items) => {
            encode_size(items.len(), 0x90, 0xD4, out);
            for item in items {
                encode_value(item, out)?;
            }
        }
        Value::Map(entries) => {
            encode_size(entries.len(), 0xA0, 0xD8, out);
            for (k, val) in entries {
                encode_string(k, out);
                encode_value(val, out)?;
            }
        }
        Value::Node { id, labels, props } => {
            let iid = i64::try_from(*id)
                .map_err(|_| PackError::Unrepresentable("node id past i64".into()))?;
            encode_struct_header(SIG_NODE, 4, out);
            encode_int(iid, out);
            encode_size(labels.len(), 0x90, 0xD4, out);
            for l in labels {
                encode_string(l, out);
            }
            encode_value(&Value::Map(props.clone()), out)?;
            encode_string(&format!("n:{id}"), out);
        }
        Value::Date(days) => {
            encode_struct_header(SIG_DATE, 1, out);
            encode_int(*days, out);
        }
        Value::Time {
            nanos,
            offset_seconds,
        } => {
            encode_struct_header(SIG_TIME, 2, out);
            encode_int(*nanos, out);
            encode_int(i64::from(*offset_seconds), out);
        }
        Value::LocalTime(nanos) => {
            encode_struct_header(SIG_LOCAL_TIME, 1, out);
            encode_int(*nanos, out);
        }
        Value::DateTime {
            epoch_seconds,
            nanos,
            offset_seconds,
            zone,
        } => match zone {
            None => {
                encode_struct_header(SIG_DATETIME_OFFSET, 3, out);
                encode_int(*epoch_seconds, out);
                encode_int(i64::from(*nanos), out);
                encode_int(i64::from(*offset_seconds), out);
            }
            Some(z) => {
                encode_struct_header(SIG_DATETIME_ZONE_ID, 3, out);
                encode_int(*epoch_seconds, out);
                encode_int(i64::from(*nanos), out);
                encode_string(z, out);
            }
        },
        Value::LocalDateTime {
            epoch_seconds,
            nanos,
        } => {
            encode_struct_header(SIG_LOCAL_DATETIME, 2, out);
            encode_int(*epoch_seconds, out);
            encode_int(i64::from(*nanos), out);
        }
        Value::Duration {
            months,
            days,
            seconds,
            nanos,
        } => {
            encode_struct_header(SIG_DURATION, 4, out);
            encode_int(*months, out);
            encode_int(*days, out);
            encode_int(*seconds, out);
            encode_int(i64::from(*nanos), out);
        }
        Value::Rel {
            id,
            src,
            dst,
            rel_type,
            props,
        } => {
            let as_i = |v: u64, what: &str| {
                i64::try_from(v).map_err(|_| PackError::Unrepresentable(format!("{what} past i64")))
            };
            encode_struct_header(SIG_RELATIONSHIP, 8, out);
            encode_int(as_i(*id, "rel id")?, out);
            encode_int(as_i(*src, "src id")?, out);
            encode_int(as_i(*dst, "dst id")?, out);
            encode_string(rel_type, out);
            encode_value(&Value::Map(props.clone()), out)?;
            encode_string(&format!("r:{id}"), out);
            encode_string(&format!("n:{src}"), out);
            encode_string(&format!("n:{dst}"), out);
        }
    }
    Ok(())
}

/// Encode a structure with pre-encoded fields.
pub fn encode_struct(tag: u8, fields: &[Pack], out: &mut Vec<u8>) -> Result<(), PackError> {
    encode_struct_header(tag, fields.len(), out);
    for f in fields {
        match f {
            Pack::Value(v) => encode_value(v, out)?,
            Pack::Bytes(b) => encode_bytes(b, out),
            Pack::Struct { tag, fields } => encode_struct(*tag, fields, out)?,
        }
    }
    Ok(())
}

fn encode_struct_header(tag: u8, len: usize, out: &mut Vec<u8>) {
    debug_assert!(len <= 15, "no Bolt structure has more than 15 fields");
    out.push(0xB0 | (len as u8));
    out.push(tag);
}

fn encode_int(i: i64, out: &mut Vec<u8>) {
    match i {
        -16..=127 => out.push(i as u8),
        -128..=-17 => {
            out.push(0xC8);
            out.push(i as u8);
        }
        -32_768..=32_767 => {
            out.push(0xC9);
            out.extend_from_slice(&(i as i16).to_be_bytes());
        }
        -2_147_483_648..=2_147_483_647 => {
            out.push(0xCA);
            out.extend_from_slice(&(i as i32).to_be_bytes());
        }
        _ => {
            out.push(0xCB);
            out.extend_from_slice(&i.to_be_bytes());
        }
    }
}

/// A byte array: `CC`/`CD`/`CE` with an 8/16/32-bit length (no tiny form).
fn encode_bytes(b: &[u8], out: &mut Vec<u8>) {
    match b.len() {
        n if n <= 0xFF => {
            out.push(0xCC);
            out.push(n as u8);
        }
        n if n <= 0xFFFF => {
            out.push(0xCD);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            out.push(0xCE);
            out.extend_from_slice(&(n as u32).to_be_bytes());
        }
    }
    out.extend_from_slice(b);
}

fn encode_string(s: &str, out: &mut Vec<u8>) {
    let b = s.as_bytes();
    encode_size(b.len(), 0x80, 0xD0, out);
    out.extend_from_slice(b);
}

/// Size headers share a shape: tiny marker for < 16, then 8/16/32-bit forms.
fn encode_size(len: usize, tiny_base: u8, sized_base: u8, out: &mut Vec<u8>) {
    if len < 16 {
        out.push(tiny_base | (len as u8));
    } else if len <= 0xFF {
        out.push(sized_base);
        out.push(len as u8);
    } else if len <= 0xFFFF {
        out.push(sized_base + 1);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(sized_base + 2);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    }
}

/// Encode a LIST size header — RECORD's row list is assembled by hand in
/// the server (values pre-encoded), so the header is exposed.
pub fn encode_size_public(len: usize, out: &mut Vec<u8>) {
    encode_size(len, 0x90, 0xD4, out);
}

// ─── Decoding ───────────────────────────────────────────────────────────────

/// The deepest container nesting this decoder will build.
///
/// PackStream costs ONE BYTE per level — `0x91` is a one-element list, `0xB1` a
/// one-field structure — so an unbounded decoder turns a 64 KiB message into
/// 64k stack frames. A Rust stack overflow is not a catchable panic: it is
/// `SIGSEGV`/`abort()`, which takes the whole process and every other session
/// with it. Pre-authentication, from one packet.
///
/// 64 is far above anything a driver produces (a RECORD carrying a path of
/// nodes-with-properties is ~5) and far below the ~10k frames a 2 MiB stack
/// holds, so it separates "hostile" from "unusual" with room on both sides.
///
/// The limit is enforced DURING decode, before the nested value is built —
/// checking a finished `Pack` would be too late twice over: the recursion that
/// built it has already run, and dropping it recurses again.
pub const MAX_DEPTH: u32 = 64;

/// A positioned decoder over a byte slice.
pub struct Decoder<'a> {
    buf: &'a [u8],
    at: usize,
    /// Containers currently open. See [`MAX_DEPTH`].
    depth: u32,
}

impl<'a> Decoder<'a> {
    /// Over `buf`.
    pub fn new(buf: &'a [u8]) -> Decoder<'a> {
        Decoder {
            buf,
            at: 0,
            depth: 0,
        }
    }

    /// Run `f` one level deeper, refusing past [`MAX_DEPTH`].
    ///
    /// Every recursive entry point goes through here, so a new container kind
    /// cannot be added that silently bypasses the limit — the alternative,
    /// incrementing a counter by hand at each call site, is the shape that
    /// grows a hole the first time someone adds a variant.
    fn nested<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, PackError>,
    ) -> Result<T, PackError> {
        if self.depth >= MAX_DEPTH {
            return Err(PackError::TooDeep {
                limit: MAX_DEPTH,
                at: self.at,
            });
        }
        self.depth += 1;
        let r = f(self);
        self.depth -= 1;
        r
    }

    /// Bytes consumed so far.
    pub fn consumed(&self) -> usize {
        self.at
    }

    /// Whether the input is fully consumed.
    pub fn done(&self) -> bool {
        self.at == self.buf.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], PackError> {
        let s = self
            .buf
            .get(self.at..self.at + n)
            .ok_or(PackError::Truncated { at: self.buf.len() })?;
        self.at += n;
        Ok(s)
    }

    fn byte(&mut self) -> Result<u8, PackError> {
        Ok(self.take(1)?[0])
    }

    /// Decode one item.
    pub fn decode(&mut self) -> Result<Pack, PackError> {
        let at = self.at;
        let marker = self.byte()?;
        Ok(match marker {
            0xC0 => Pack::Value(Value::Null),
            0xC2 => Pack::Value(Value::Bool(false)),
            0xC3 => Pack::Value(Value::Bool(true)),
            0x00..=0x7F => Pack::Value(Value::Int(i64::from(marker))),
            0xF0..=0xFF => Pack::Value(Value::Int(i64::from(marker as i8))),
            0xC8 => Pack::Value(Value::Int(i64::from(self.byte()? as i8))),
            0xC9 => {
                let b = self.take(2)?;
                Pack::Value(Value::Int(i64::from(i16::from_be_bytes(
                    b.try_into().expect("2"),
                ))))
            }
            0xCA => {
                let b = self.take(4)?;
                Pack::Value(Value::Int(i64::from(i32::from_be_bytes(
                    b.try_into().expect("4"),
                ))))
            }
            0xCB => {
                let b = self.take(8)?;
                Pack::Value(Value::Int(i64::from_be_bytes(b.try_into().expect("8"))))
            }
            0xC1 => {
                let b = self.take(8)?;
                Pack::Value(Value::Float(f64::from_be_bytes(b.try_into().expect("8"))))
            }
            0xCC => {
                let n = self.byte()? as usize;
                Pack::Bytes(self.take(n)?.to_vec())
            }
            0xCD => {
                let n = u16::from_be_bytes(self.take(2)?.try_into().expect("2")) as usize;
                Pack::Bytes(self.take(n)?.to_vec())
            }
            0xCE => {
                let n = u32::from_be_bytes(self.take(4)?.try_into().expect("4")) as usize;
                Pack::Bytes(self.take(n)?.to_vec())
            }
            0x80..=0x8F => self.string_value((marker & 0x0F) as usize, at)?,
            0xD0 => {
                let n = self.byte()? as usize;
                self.string_value(n, at)?
            }
            0xD1 => {
                let n = u16::from_be_bytes(self.take(2)?.try_into().expect("2")) as usize;
                self.string_value(n, at)?
            }
            0xD2 => {
                let n = u32::from_be_bytes(self.take(4)?.try_into().expect("4")) as usize;
                self.string_value(n, at)?
            }
            0x90..=0x9F => self.list((marker & 0x0F) as usize)?,
            0xD4 => {
                let n = self.byte()? as usize;
                self.list(n)?
            }
            0xD5 => {
                let n = u16::from_be_bytes(self.take(2)?.try_into().expect("2")) as usize;
                self.list(n)?
            }
            0xD6 => {
                let n = u32::from_be_bytes(self.take(4)?.try_into().expect("4")) as usize;
                self.list(n)?
            }
            0xA0..=0xAF => self.map((marker & 0x0F) as usize)?,
            0xD8 => {
                let n = self.byte()? as usize;
                self.map(n)?
            }
            0xD9 => {
                let n = u16::from_be_bytes(self.take(2)?.try_into().expect("2")) as usize;
                self.map(n)?
            }
            0xDA => {
                let n = u32::from_be_bytes(self.take(4)?.try_into().expect("4")) as usize;
                self.map(n)?
            }
            0xB0..=0xBF => {
                let n = (marker & 0x0F) as usize;
                let tag = self.byte()?;
                self.nested(|d| {
                    let mut fields = Vec::with_capacity(n);
                    for _ in 0..n {
                        fields.push(d.decode()?);
                    }
                    Ok(Pack::Struct { tag, fields })
                })?
            }
            other => return Err(PackError::UnknownMarker { marker: other, at }),
        })
    }

    fn string_value(&mut self, n: usize, at: usize) -> Result<Pack, PackError> {
        let bytes = self.take(n)?;
        let s = std::str::from_utf8(bytes).map_err(|_| PackError::BadUtf8 { at })?;
        Ok(Pack::Value(Value::Str(s.to_string())))
    }

    fn list(&mut self, n: usize) -> Result<Pack, PackError> {
        self.nested(|d| {
            let mut items = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                // Entity structures nest inside record rows and lists — a list
                // element decodes through the STRUCTURE mapping, not just the
                // scalar path.
                items.push(decode_value(d.decode()?)?);
            }
            Ok(Pack::Value(Value::List(items)))
        })
    }

    fn map(&mut self, n: usize) -> Result<Pack, PackError> {
        self.nested(|d| {
            let mut entries = BTreeMap::new();
            for _ in 0..n {
                let k = match d.decode()?.into_value()? {
                    Value::Str(s) => s,
                    other => {
                        return Err(PackError::Unrepresentable(format!(
                            "a map key must be a string, got {}",
                            other.type_name()
                        )));
                    }
                };
                entries.insert(k, decode_value(d.decode()?)?);
            }
            Ok(Pack::Value(Value::Map(entries)))
        })
    }
}

/// Decode a full graph value — structures for Node/Relationship/Path decode
/// into [`Value`]; other structures refuse by tag (a DRIVER decodes those; a
/// server receiving one in parameters refuses).
pub fn decode_value(pack: Pack) -> Result<Value, PackError> {
    match pack {
        Pack::Value(v) => Ok(v),
        Pack::Bytes(b) => Pack::Bytes(b).into_value(),
        Pack::Struct {
            tag: SIG_VECTOR,
            fields,
        } => {
            let [marker, data] = take_fields::<2>(SIG_VECTOR, fields)?;
            // The marker travels either as the integer value of the byte or as
            // a one-byte array; both spellings exist among drivers.
            let marker = match marker {
                Pack::Value(Value::Int(m)) if (0..=255).contains(&m) => m as u8,
                Pack::Bytes(b) if b.len() == 1 => b[0],
                _ => {
                    return Err(PackError::BadStructure {
                        tag: SIG_VECTOR,
                        detail: "type_marker".into(),
                    })
                }
            };
            let Pack::Bytes(data) = data else {
                return Err(PackError::BadStructure {
                    tag: SIG_VECTOR,
                    detail: "data must be a byte array".into(),
                });
            };
            let width = match marker {
                0xC8 => 1,
                0xC9 => 2,
                0xCA => 4,
                0xCB => 8,
                0xC6 => 4,
                0xC1 => 8,
                other => {
                    return Err(PackError::BadStructure {
                        tag: SIG_VECTOR,
                        detail: format!("unknown element type marker {other:#04X}"),
                    })
                }
            };
            if data.len() % width != 0 {
                return Err(PackError::BadStructure {
                    tag: SIG_VECTOR,
                    detail: format!(
                        "{} data byte(s) is not a whole number of {width}-byte elements",
                        data.len()
                    ),
                });
            }
            let elements = data
                .chunks_exact(width)
                .map(|c| match marker {
                    0xC8 => Value::Int(i64::from(c[0] as i8)),
                    0xC9 => Value::Int(i64::from(i16::from_be_bytes([c[0], c[1]]))),
                    0xCA => Value::Int(i64::from(i32::from_be_bytes([c[0], c[1], c[2], c[3]]))),
                    0xCB => Value::Int(i64::from_be_bytes(c.try_into().expect("8"))),
                    0xC6 => Value::Float(f64::from(f32::from_be_bytes([c[0], c[1], c[2], c[3]]))),
                    _ => Value::Float(f64::from_be_bytes(c.try_into().expect("8"))),
                })
                .collect();
            Ok(Value::List(elements))
        }
        Pack::Struct {
            tag: SIG_NODE,
            fields,
        } => {
            let [id, labels, props, _eid] = take_fields::<4>(SIG_NODE, fields)?;
            let Value::Int(id) = decode_value(id)? else {
                return Err(PackError::BadStructure {
                    tag: SIG_NODE,
                    detail: "id".into(),
                });
            };
            let Value::List(ls) = decode_value(labels)? else {
                return Err(PackError::BadStructure {
                    tag: SIG_NODE,
                    detail: "labels".into(),
                });
            };
            let Value::Map(props) = decode_value(props)? else {
                return Err(PackError::BadStructure {
                    tag: SIG_NODE,
                    detail: "props".into(),
                });
            };
            let mut labels = Vec::with_capacity(ls.len());
            for l in ls {
                match l {
                    Value::Str(s) => labels.push(s),
                    _ => {
                        return Err(PackError::BadStructure {
                            tag: SIG_NODE,
                            detail: "label type".into(),
                        });
                    }
                }
            }
            Ok(Value::Node {
                id: id as u64,
                labels,
                props,
            })
        }
        Pack::Struct {
            tag: SIG_RELATIONSHIP,
            fields,
        } => {
            let [id, src, dst, typ, props, _eid, _seid, _deid] =
                take_fields::<8>(SIG_RELATIONSHIP, fields)?;
            let as_int = |p: Pack, what: &str| match decode_value(p) {
                Ok(Value::Int(v)) => Ok(v as u64),
                _ => Err(PackError::BadStructure {
                    tag: SIG_RELATIONSHIP,
                    detail: what.to_string(),
                }),
            };
            let (id, src, dst) = (as_int(id, "id")?, as_int(src, "src")?, as_int(dst, "dst")?);
            let Value::Str(rel_type) = decode_value(typ)? else {
                return Err(PackError::BadStructure {
                    tag: SIG_RELATIONSHIP,
                    detail: "type".into(),
                });
            };
            let Value::Map(props) = decode_value(props)? else {
                return Err(PackError::BadStructure {
                    tag: SIG_RELATIONSHIP,
                    detail: "props".into(),
                });
            };
            Ok(Value::Rel {
                id,
                src,
                dst,
                rel_type,
                props,
            })
        }
        Pack::Struct {
            tag: SIG_DATE,
            fields,
        } => {
            let [days] = take_fields::<1>(SIG_DATE, fields)?;
            Ok(Value::Date(field_int(days, SIG_DATE, "days")?))
        }
        Pack::Struct {
            tag: SIG_TIME,
            fields,
        } => {
            let [nanos, offset] = take_fields::<2>(SIG_TIME, fields)?;
            Ok(Value::Time {
                nanos: field_int(nanos, SIG_TIME, "nanos")?,
                offset_seconds: field_int(offset, SIG_TIME, "offset")? as i32,
            })
        }
        Pack::Struct {
            tag: SIG_LOCAL_TIME,
            fields,
        } => {
            let [nanos] = take_fields::<1>(SIG_LOCAL_TIME, fields)?;
            Ok(Value::LocalTime(field_int(nanos, SIG_LOCAL_TIME, "nanos")?))
        }
        Pack::Struct {
            tag: SIG_DATETIME_OFFSET,
            fields,
        } => {
            let [secs, nanos, offset] = take_fields::<3>(SIG_DATETIME_OFFSET, fields)?;
            Ok(Value::DateTime {
                epoch_seconds: field_int(secs, SIG_DATETIME_OFFSET, "seconds")?,
                nanos: field_int(nanos, SIG_DATETIME_OFFSET, "nanos")? as u32,
                offset_seconds: field_int(offset, SIG_DATETIME_OFFSET, "offset")? as i32,
                zone: None,
            })
        }
        Pack::Struct {
            tag: SIG_DATETIME_ZONE_ID,
            fields,
        } => {
            let [secs, nanos, zone] = take_fields::<3>(SIG_DATETIME_ZONE_ID, fields)?;
            let Value::Str(z) = decode_value(zone)? else {
                return Err(PackError::BadStructure {
                    tag: SIG_DATETIME_ZONE_ID,
                    detail: "zone id".into(),
                });
            };
            Ok(Value::DateTime {
                epoch_seconds: field_int(secs, SIG_DATETIME_ZONE_ID, "seconds")?,
                nanos: field_int(nanos, SIG_DATETIME_ZONE_ID, "nanos")? as u32,
                offset_seconds: 0,
                zone: Some(z),
            })
        }
        Pack::Struct {
            tag: SIG_LOCAL_DATETIME,
            fields,
        } => {
            let [secs, nanos] = take_fields::<2>(SIG_LOCAL_DATETIME, fields)?;
            Ok(Value::LocalDateTime {
                epoch_seconds: field_int(secs, SIG_LOCAL_DATETIME, "seconds")?,
                nanos: field_int(nanos, SIG_LOCAL_DATETIME, "nanos")? as u32,
            })
        }
        Pack::Struct {
            tag: SIG_DURATION,
            fields,
        } => {
            let [months, days, seconds, nanos] = take_fields::<4>(SIG_DURATION, fields)?;
            Ok(Value::Duration {
                months: field_int(months, SIG_DURATION, "months")?,
                days: field_int(days, SIG_DURATION, "days")?,
                seconds: field_int(seconds, SIG_DURATION, "seconds")?,
                nanos: field_int(nanos, SIG_DURATION, "nanos")? as i32,
            })
        }
        Pack::Struct { tag, .. } => Err(PackError::BadStructure {
            tag,
            detail: "no value mapping for this structure".into(),
        }),
    }
}

fn field_int(p: Pack, tag: u8, what: &str) -> Result<i64, PackError> {
    match decode_value(p) {
        Ok(Value::Int(v)) => Ok(v),
        _ => Err(PackError::BadStructure {
            tag,
            detail: what.to_string(),
        }),
    }
}

fn take_fields<const N: usize>(tag: u8, fields: Vec<Pack>) -> Result<[Pack; N], PackError> {
    let n = fields.len();
    fields.try_into().map_err(|_| PackError::BadStructure {
        tag,
        detail: format!("expected {N} fields, got {n}"),
    })
}
