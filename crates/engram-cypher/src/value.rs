//! The Cypher value model, and the three-valued logic the corpus RELIES on.
//!
//! `null = 'x'` failing closed is load-bearing in the incumbent (the plan
//! names it), so null is not a bolted-on option type: [`Truth`] is the
//! three-valued domain, comparisons on incomparable types answer
//! [`Truth::Unknown`], and only the operators specified to see through null
//! (`IS NULL`, `coalesce`) do.

use std::collections::BTreeMap;

/// A Cypher value. Graph values (nodes, relationships, paths) arrive with the
/// clause layer; this is the scalar/composite core every expression needs.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// null.
    Null,
    /// A boolean.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A float.
    Float(f64),
    /// A string.
    Str(String),
    /// A list.
    List(Vec<Value>),
    /// A PATH: the alternating `[node, rel, node, …]` trail a path variable
    /// (`MATCH p = …`) binds. Structurally a list of nodes and relationships, but
    /// a DISTINCT type — openCypher compares a path to a non-path as incomparable
    /// (`null`), never element-wise like two lists, and `nodes(p)`/`length(p)`
    /// read it as a path. Kept separate from `List` for exactly that reason.
    Path(Vec<Value>),
    /// A map. BTreeMap so equality and rendering are order-independent —
    /// two maps with one content are one value.
    Map(BTreeMap<String, Value>),
    /// A node, materialised at bind time (labels and properties as of the
    /// read — the snapshot the driver would decode).
    Node {
        /// The node's id.
        id: u64,
        /// Its labels.
        labels: Vec<String>,
        /// Its properties.
        props: BTreeMap<String, Value>,
    },
    /// A calendar date: days since the epoch (proleptic Gregorian).
    Date(i64),
    /// Time of day with a fixed UTC offset.
    Time {
        /// Nanoseconds since midnight.
        nanos: i64,
        /// UTC offset in seconds.
        offset_seconds: i32,
    },
    /// Time of day, zoneless: nanoseconds since midnight.
    LocalTime(i64),
    /// An instant with a UTC offset (and possibly a CARRIED zone id — the
    /// zero-dep core has no tzdata, so a named zone is preserved, printed,
    /// and never silently resolved to a guessed offset).
    DateTime {
        /// Seconds since the epoch (UTC).
        epoch_seconds: i64,
        /// Sub-second nanoseconds.
        nanos: u32,
        /// UTC offset in seconds.
        offset_seconds: i32,
        /// The zone id, if the value carries one.
        zone: Option<String>,
    },
    /// A wall-clock datetime, zoneless.
    LocalDateTime {
        /// Seconds since the epoch, read as wall-clock.
        epoch_seconds: i64,
        /// Sub-second nanoseconds.
        nanos: u32,
    },
    /// A four-component duration — Bolt's shape exactly. A month is NOT a
    /// fixed number of days; collapsing components loses calendar arithmetic.
    Duration {
        /// Months.
        months: i64,
        /// Days.
        days: i64,
        /// Seconds.
        seconds: i64,
        /// Nanoseconds.
        nanos: i32,
    },
    /// A relationship, materialised at bind time.
    Rel {
        /// The relationship's id.
        id: u64,
        /// Source node id.
        src: u64,
        /// Destination node id.
        dst: u64,
        /// The type.
        rel_type: String,
        /// Its properties.
        props: BTreeMap<String, Value>,
    },
}

/// Three-valued truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Truth {
    /// Definitely true.
    True,
    /// Definitely false.
    False,
    /// Cannot be known — null's shadow. An `Unknown` in a filter fails
    /// CLOSED (the row does not pass), which is the property the corpus
    /// leans on.
    Unknown,
}

impl Truth {
    /// Three-valued AND: false dominates, then unknown.
    pub fn and(self, other: Truth) -> Truth {
        match (self, other) {
            (Truth::False, _) | (_, Truth::False) => Truth::False,
            (Truth::Unknown, _) | (_, Truth::Unknown) => Truth::Unknown,
            _ => Truth::True,
        }
    }

    /// Three-valued OR: true dominates, then unknown.
    pub fn or(self, other: Truth) -> Truth {
        match (self, other) {
            (Truth::True, _) | (_, Truth::True) => Truth::True,
            (Truth::Unknown, _) | (_, Truth::Unknown) => Truth::Unknown,
            _ => Truth::False,
        }
    }

    /// Three-valued XOR: any unknown poisons.
    pub fn xor(self, other: Truth) -> Truth {
        match (self, other) {
            (Truth::Unknown, _) | (_, Truth::Unknown) => Truth::Unknown,
            (a, b) => {
                if (a == Truth::True) != (b == Truth::True) {
                    Truth::True
                } else {
                    Truth::False
                }
            }
        }
    }

    /// Back to a value: `Unknown` is null, not false — a filter collapses it,
    /// an expression must not.
    pub fn to_value(self) -> Value {
        match self {
            Truth::True => Value::Bool(true),
            Truth::False => Value::Bool(false),
            Truth::Unknown => Value::Null,
        }
    }
}

impl Value {
    /// Equality in the three-valued domain.
    ///
    /// null against anything is Unknown; an int and a float compare
    /// numerically; values of incomparable TYPES are simply not equal (a
    /// definite false — openCypher's `1 = 'a'` is false, not null); lists and
    /// maps compare element-wise and any Unknown inside poisons.
    pub fn eq3(&self, other: &Value) -> Truth {
        use Value::*;
        match (self, other) {
            (Null, _) | (_, Null) => Truth::Unknown,
            (Bool(a), Bool(b)) => Truth::from_bool(a == b),
            (Int(a), Int(b)) => Truth::from_bool(a == b),
            (Float(a), Float(b)) => Truth::from_bool(a == b),
            (Int(a), Float(b)) | (Float(b), Int(a)) => Truth::from_bool((*a as f64) == *b),
            (Str(a), Str(b)) => Truth::from_bool(a == b),
            (List(a), List(b)) => {
                if a.len() != b.len() {
                    return Truth::False;
                }
                let mut acc = Truth::True;
                for (x, y) in a.iter().zip(b) {
                    acc = acc.and(x.eq3(y));
                    if acc == Truth::False {
                        return Truth::False;
                    }
                }
                acc
            }
            // Graph entities compare by IDENTITY — two reads of one node
            // are equal even if properties changed between them (Neo4j's
            // semantics, and the one MERGE dedup relies on).
            (Node { id: a, .. }, Node { id: b, .. }) => Truth::from_bool(a == b),
            (Date(a), Date(b)) => Truth::from_bool(a == b),
            (LocalTime(a), LocalTime(b)) => Truth::from_bool(a == b),
            (
                Time {
                    nanos: a,
                    offset_seconds: oa,
                },
                Time {
                    nanos: b,
                    offset_seconds: ob,
                },
            ) => Truth::from_bool(
                a - i64::from(*oa) * 1_000_000_000 == b - i64::from(*ob) * 1_000_000_000,
            ),
            // DateTimes compare by INSTANT — the offset is presentation.
            (
                DateTime {
                    epoch_seconds: sa,
                    nanos: na,
                    ..
                },
                DateTime {
                    epoch_seconds: sb,
                    nanos: nb,
                    ..
                },
            ) => Truth::from_bool((sa, na) == (sb, nb)),
            (
                LocalDateTime {
                    epoch_seconds: sa,
                    nanos: na,
                },
                LocalDateTime {
                    epoch_seconds: sb,
                    nanos: nb,
                },
            ) => Truth::from_bool((sa, na) == (sb, nb)),
            // Durations compare COMPONENTWISE: P1M is not P30D.
            (
                Duration {
                    months: ma,
                    days: da,
                    seconds: sa,
                    nanos: na,
                },
                Duration {
                    months: mb,
                    days: db,
                    seconds: sb,
                    nanos: nb,
                },
            ) => Truth::from_bool((ma, da, sa, na) == (mb, db, sb, nb)),
            (Rel { id: a, .. }, Rel { id: b, .. }) => Truth::from_bool(a == b),
            // Two PATHS are equal iff their trails match element-wise (nodes and
            // rels by identity). A path vs a non-path falls through to `False`
            // below — a path is a distinct type, never equal to a plain list.
            (Path(a), Path(b)) => {
                if a.len() != b.len() {
                    return Truth::False;
                }
                let mut acc = Truth::True;
                for (x, y) in a.iter().zip(b) {
                    acc = acc.and(x.eq3(y));
                    if acc == Truth::False {
                        return Truth::False;
                    }
                }
                acc
            }
            (Map(a), Map(b)) => {
                if a.len() != b.len() || !a.keys().eq(b.keys()) {
                    return Truth::False;
                }
                let mut acc = Truth::True;
                for (k, x) in a {
                    acc = acc.and(x.eq3(&b[k]));
                    if acc == Truth::False {
                        return Truth::False;
                    }
                }
                acc
            }
            _ => Truth::False,
        }
    }

    /// Ordering comparison (`<`): null → Unknown, numbers cross-compare,
    /// strings compare lexically, INCOMPARABLE TYPES → Unknown (openCypher:
    /// `1 < 'a'` is null, unlike `=`).
    pub fn lt3(&self, other: &Value) -> Truth {
        use Value::*;
        match (self, other) {
            (Null, _) | (_, Null) => Truth::Unknown,
            (Int(a), Int(b)) => Truth::from_bool(a < b),
            (Float(a), Float(b)) => Truth::from_bool(a < b),
            (Int(a), Float(b)) => Truth::from_bool((*a as f64) < *b),
            (Float(a), Int(b)) => Truth::from_bool(*a < (*b as f64)),
            (Str(a), Str(b)) => Truth::from_bool(a < b),
            (Bool(a), Bool(b)) => Truth::from_bool(!a & b),
            (Date(a), Date(b)) => Truth::from_bool(a < b),
            (LocalTime(a), LocalTime(b)) => Truth::from_bool(a < b),
            (
                Time {
                    nanos: a,
                    offset_seconds: oa,
                },
                Time {
                    nanos: b,
                    offset_seconds: ob,
                },
            ) => Truth::from_bool(
                a - i64::from(*oa) * 1_000_000_000 < b - i64::from(*ob) * 1_000_000_000,
            ),
            (
                DateTime {
                    epoch_seconds: sa,
                    nanos: na,
                    ..
                },
                DateTime {
                    epoch_seconds: sb,
                    nanos: nb,
                    ..
                },
            ) => Truth::from_bool((sa, na) < (sb, nb)),
            (
                LocalDateTime {
                    epoch_seconds: sa,
                    nanos: na,
                },
                LocalDateTime {
                    epoch_seconds: sb,
                    nanos: nb,
                },
            ) => Truth::from_bool((sa, na) < (sb, nb)),
            // Durations are NOT orderable (P1M vs P30D has no answer).
            (Duration { .. }, Duration { .. }) => Truth::Unknown,
            // Lists compare lexicographically under three-valued logic: walk the
            // common prefix; a definitive element `<`/`>` decides the whole, an
            // element that is `null`/incomparable makes the whole Unknown, and an
            // all-equal prefix defers to the shorter list being the lesser.
            (List(a), List(b)) => {
                for (x, y) in a.iter().zip(b.iter()) {
                    match (x.lt3(y), y.lt3(x)) {
                        (Truth::True, _) => return Truth::True,
                        (_, Truth::True) => return Truth::False,
                        (Truth::False, Truth::False) => continue,
                        _ => return Truth::Unknown,
                    }
                }
                Truth::from_bool(a.len() < b.len())
            }
            _ => Truth::Unknown,
        }
    }

    /// The value as a filter predicate: true passes, false and NULL do not,
    /// and a non-boolean is a type refusal handled by the caller.
    pub fn truth(&self) -> Option<Truth> {
        match self {
            Value::Null => Some(Truth::Unknown),
            Value::Bool(true) => Some(Truth::True),
            Value::Bool(false) => Some(Truth::False),
            _ => None,
        }
    }

    /// A short type name for error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Int(_) => "integer",
            Value::Float(_) => "float",
            Value::Str(_) => "string",
            Value::List(_) => "list",
            Value::Path(_) => "path",
            Value::Map(_) => "map",
            Value::Node { .. } => "node",
            Value::Rel { .. } => "relationship",
            Value::Date(_) => "date",
            Value::Time { .. } => "time",
            Value::LocalTime(_) => "localtime",
            Value::DateTime { .. } => "datetime",
            Value::LocalDateTime { .. } => "localdatetime",
            Value::Duration { .. } => "duration",
        }
    }
}

impl Truth {
    fn from_bool(b: bool) -> Truth {
        if b { Truth::True } else { Truth::False }
    }
}

/// Three-valued NOT: unknown stays unknown. The std trait so `.not()` and
/// `!t` are one operation and clippy cannot mistake it for a shadow.
impl std::ops::Not for Truth {
    type Output = Truth;

    fn not(self) -> Truth {
        match self {
            Truth::True => Truth::False,
            Truth::False => Truth::True,
            Truth::Unknown => Truth::Unknown,
        }
    }
}
