//! The procedure catalogue — every `CALL` surface, as data.
//!
//! # The defect this crate exists to end
//!
//! A procedure used to be three things in three places: an arm of a
//! hard-coded `match` in the interpreter, a name in an allow-list in the front
//! end that decided whether the call could mutate, and a paragraph in the
//! book. Nothing tied them together, so they drifted. `db.info` sat in the
//! read-only allow-list with no implementation at all — it parsed, passed the
//! classification, and then refused at run time. Three separate copies of the
//! `YIELD`-binding loop diverged from one another. And a procedure's default
//! output columns existed only as a literal inside its own body, which is why
//! a standalone `CALL` could not name its own result.
//!
//! This crate is the single declaration. A [`ProcedureSignature`] states the
//! name, the arguments, **the default output columns**, and the mutation
//! class; `engram-cypher` reads it to classify and to check arity, and
//! `engram-graph` reads it to bind `YIELD` fields and to shape the result.
//! Neither owns it.
//!
//! # Why a sorted slice and not a map
//!
//! [`CATALOG`] is a `&'static [ProcedureSignature]` kept sorted by name and
//! searched by binary search. A `BTreeMap` would have to be built at first use
//! behind a `OnceLock`, which is interior mutability and a warm-up ordering
//! that the simulation lane would then have to reason about; a `HashMap` is a
//! denied type for iteration-order reasons that apply here too, since the
//! catalogue is enumerated to generate the reference documentation. A `const`
//! slice is visible at build time and costs nothing at run time.
//!
//! The sortedness the binary search assumes is not asserted at run time — it
//! is PROVEN BY A TEST, because a run-time assertion over a `const` is a cost
//! paid for ever to catch a mistake that can only be made once, at compile
//! time.

#![forbid(unsafe_code)]

/// What an argument or an output column holds, at the coarseness the front end
/// can actually check.
///
/// This is deliberately a SHAPE and not a type. `engram-cypher` owns the value
/// model but may not export it here — this crate must stay free of it so that
/// both the front end and the graph can depend on the catalogue without a
/// cycle — so the catalogue speaks in categories each side maps onto its own
/// representation. `Any` is the escape for a genuinely polymorphic position,
/// and it is not a synonym for "unchecked": an `Any` argument still counts for
/// arity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcType {
    /// Any value, including null. Counted for arity, unchecked for shape.
    Any,
    /// An integer.
    Int,
    /// A floating-point number.
    Float,
    /// A string.
    Str,
    /// A boolean.
    Bool,
    /// A list of any element type.
    List,
    /// A map, which is how an algorithm configuration is passed.
    Map,
    /// A node.
    Node,
    /// A relationship.
    Rel,
    /// A path.
    Path,
}

/// One declared argument of a procedure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcArg {
    /// The argument's name, for diagnostics. Cypher calls procedures
    /// positionally, so this never appears in a query.
    pub name: &'static str,
    /// The shape this position accepts.
    pub ty: ProcType,
    /// Whether the call may omit this argument.
    ///
    /// Optional arguments must be declared LAST; a required argument after an
    /// optional one makes arity ambiguous. The catalogue test proves it.
    pub optional: bool,
}

/// One column a procedure yields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcColumn {
    /// The column's name, as it appears in `YIELD` and as it becomes a result
    /// column when `YIELD` is omitted. Case-sensitive, and spelled the way a
    /// driver expects to see it.
    pub name: &'static str,
    /// The shape of the values in this column.
    pub ty: ProcType,
}

/// Whether a call may mutate, and what it may mutate.
///
/// This replaces the name-prefix allow-list the front end used to carry. The
/// distinction matters most for the graph algorithms, whose execution modes
/// share a name stem and differ precisely here: a `.stream` or `.stats` call
/// is [`Read`] and a `.mutate` or `.write` call is [`Write`]. Classifying by
/// mode rather than by name means no part of the engine parses a procedure
/// name to decide what it is allowed to do.
///
/// [`Read`]: ProcMode::Read
/// [`Write`]: ProcMode::Write
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcMode {
    /// Reads the graph. Admissible in a read transaction.
    Read,
    /// Mutates graph data.
    Write,
    /// Mutates the schema — indexes, constraints.
    SchemaWrite,
    /// Acts on the server rather than on the graph.
    ///
    /// Read-only with respect to the graph, and classified separately because
    /// a future authorization model gates it on a different privilege.
    Dbms,
}

impl ProcMode {
    /// Whether a call in this mode leaves the graph unchanged.
    #[must_use]
    pub fn is_read_only(self) -> bool {
        matches!(self, ProcMode::Read | ProcMode::Dbms)
    }
}

/// Everything the engine knows about one procedure, other than how to run it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcedureSignature {
    /// The lower-cased, dot-joined name, exactly as the parser produces it.
    ///
    /// The parser lower-cases and dot-joins every procedure name it reads, so
    /// this is the lookup key and it is what an implementation table must be
    /// keyed by. It is NOT what a user sees; [`display`] is.
    ///
    /// [`display`]: ProcedureSignature::display
    pub name: &'static str,
    /// The spelling a driver and an error message should use.
    pub display: &'static str,
    /// The declared arguments, in order.
    pub args: &'static [ProcArg],
    /// **The default output signature**: the columns, in order, that a
    /// `YIELD`-less call produces when it is the final clause of a query.
    ///
    /// NEVER EMPTY. A procedure with no columns to yield cannot end a query
    /// that returns anything, and declaring an empty output would make a
    /// standalone call silently produce nothing — which is the exact behaviour
    /// this catalogue was introduced to end. The catalogue test proves no
    /// entry is empty.
    pub outputs: &'static [ProcColumn],
    /// Whether the call may mutate, and what.
    pub mode: ProcMode,
    /// One line, used to generate the reference documentation.
    pub description: &'static str,
}

impl ProcedureSignature {
    /// The number of arguments that must be supplied.
    #[must_use]
    pub fn required_args(&self) -> usize {
        let mut n = 0;
        let mut i = 0;
        while i < self.args.len() {
            if !self.args[i].optional {
                n += 1;
            }
            i += 1;
        }
        n
    }

    /// Whether `n` arguments is an admissible count for this procedure.
    #[must_use]
    pub fn accepts_arity(&self, n: usize) -> bool {
        n >= self.required_args() && n <= self.args.len()
    }

    /// Whether this procedure declares an output column called `field`.
    #[must_use]
    pub fn yields(&self, field: &str) -> bool {
        self.outputs.iter().any(|c| c.name == field)
    }

    /// The declared output column names, comma-separated — the list an error
    /// message shows when a `YIELD` names something the procedure does not
    /// have.
    #[must_use]
    pub fn output_list(&self) -> String {
        let mut s = String::new();
        for (i, col) in self.outputs.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push_str(col.name);
        }
        s
    }

    /// How a call of this procedure should be spelled, for a diagnostic that
    /// wants to show the user the shape they got wrong.
    #[must_use]
    pub fn call_shape(&self) -> String {
        let mut s = String::new();
        s.push_str(self.display);
        s.push('(');
        for (i, arg) in self.args.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            if arg.optional {
                s.push('[');
                s.push_str(arg.name);
                s.push(']');
            } else {
                s.push_str(arg.name);
            }
        }
        s.push(')');
        s
    }
}

/// Shorthand for a required argument.
const fn a(name: &'static str, ty: ProcType) -> ProcArg {
    ProcArg {
        name,
        ty,
        optional: false,
    }
}

/// Shorthand for an optional argument.
const fn opt(name: &'static str, ty: ProcType) -> ProcArg {
    ProcArg {
        name,
        ty,
        optional: true,
    }
}

/// Shorthand for an output column.
const fn c(name: &'static str, ty: ProcType) -> ProcColumn {
    ProcColumn { name, ty }
}

/// Every procedure the engine implements, **sorted by name**.
///
/// Sortedness is what [`lookup`]'s binary search assumes, and it is proven by
/// the catalogue test. Adding an entry out of order fails that test rather
/// than silently making the entry unfindable — which is the failure a linear
/// scan would have hidden.
pub const CATALOG: &[ProcedureSignature] = &[
    ProcedureSignature {
        name: "db.awaitindexes",
        display: "db.awaitIndexes",
        args: &[opt("timeOutSeconds", ProcType::Int)],
        outputs: &[c("ok", ProcType::Bool)],
        mode: ProcMode::Read,
        description: "Returns true immediately. Index builds here are single-flight on the read \
                      path rather than background jobs, so there is nothing to await; the \
                      procedure exists because drivers call it on connect.",
    },
    ProcedureSignature {
        name: "db.index.fulltext.querynodes",
        display: "db.index.fulltext.queryNodes",
        args: &[
            a("indexName", ProcType::Str),
            a("queryString", ProcType::Str),
        ],
        outputs: &[c("node", ProcType::Node), c("score", ProcType::Float)],
        mode: ProcMode::Read,
        description: "Full-text search over a declared fulltext index, returning each matching \
                      node with its relevance score.",
    },
    ProcedureSignature {
        name: "db.index.vector.querynodes",
        display: "db.index.vector.queryNodes",
        args: &[
            a("indexName", ProcType::Str),
            a("numberOfNearestNeighbours", ProcType::Int),
            a("query", ProcType::List),
        ],
        outputs: &[c("node", ProcType::Node), c("score", ProcType::Float)],
        mode: ProcMode::Read,
        description: "Approximate nearest-neighbour search over a declared vector index, by \
                      cosine similarity.",
    },
    ProcedureSignature {
        name: "db.labels",
        display: "db.labels",
        args: &[],
        outputs: &[c("label", ProcType::Str)],
        mode: ProcMode::Read,
        description: "Every node label in use.",
    },
    ProcedureSignature {
        name: "db.propertykeys",
        display: "db.propertyKeys",
        args: &[],
        outputs: &[c("propertyKey", ProcType::Str)],
        mode: ProcMode::Read,
        description: "Every property key in use.",
    },
    ProcedureSignature {
        name: "db.relationshiptypes",
        display: "db.relationshipTypes",
        args: &[],
        outputs: &[c("relationshipType", ProcType::Str)],
        mode: ProcMode::Read,
        description: "Every relationship type in use.",
    },
    ProcedureSignature {
        name: "dbms.components",
        display: "dbms.components",
        args: &[],
        outputs: &[
            c("name", ProcType::Str),
            c("versions", ProcType::List),
            c("edition", ProcType::Str),
        ],
        mode: ProcMode::Dbms,
        description: "The server's name, version and edition, as a driver expects it on connect.",
    },
    ProcedureSignature {
        name: "engram.algo.betweenness.mutate",
        display: "engram.algo.betweenness.mutate",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("mutateKey", ProcType::Str), c("nodeCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int)],
        mode: ProcMode::Write,
        description: "Exact betweenness centrality by Brandes' algorithm: how many shortest paths run through each node. Cost is O(V x E), so a projection well inside the node and edge ceilings can still be refused against the all-pairs work ceiling. Publishes into the result cache under a key; touches no keyspace row.",
    },
    ProcedureSignature {
        name: "engram.algo.betweenness.stats",
        display: "engram.algo.betweenness.stats",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeCount", ProcType::Int), c("relationshipCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("distribution", ProcType::Map), c("outsideProjection", ProcType::Int)],
        mode: ProcMode::Read,
        description: "Exact betweenness centrality by Brandes' algorithm: how many shortest paths run through each node. Cost is O(V x E), so a projection well inside the node and edge ceilings can still be refused against the all-pairs work ceiling. One summary row.",
    },
    ProcedureSignature {
        name: "engram.algo.betweenness.stream",
        display: "engram.algo.betweenness.stream",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeId", ProcType::Int), c("node", ProcType::Node), c("score", ProcType::Float), c("asOf", ProcType::Int)],
        mode: ProcMode::Read,
        description: "Exact betweenness centrality by Brandes' algorithm: how many shortest paths run through each node. Cost is O(V x E), so a projection well inside the node and edge ceilings can still be refused against the all-pairs work ceiling. One row per node, ascending by node id.",
    },
    ProcedureSignature {
        name: "engram.algo.betweenness.write",
        display: "engram.algo.betweenness.write",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("writeProperty", ProcType::Str), c("nodesWritten", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("committedAt", ProcType::Int)],
        mode: ProcMode::Write,
        description: "Exact betweenness centrality by Brandes' algorithm: how many shortest paths run through each node. Cost is O(V x E), so a projection well inside the node and edge ceilings can still be refused against the all-pairs work ceiling. Persists as a node property, after the computation. The whole write-back is ONE atomic statement — `writeBatchSize` bounds the chunk the write loop works in, not a transaction boundary — so a failure leaves nothing behind rather than half a graph annotated.",
    },
    ProcedureSignature {
        name: "engram.algo.bfs.mutate",
        display: "engram.algo.bfs.mutate",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("mutateKey", ProcType::Str), c("nodeCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int)],
        mode: ProcMode::Write,
        description: "Unweighted hop distance from a source node. Publishes into the result cache under a key; touches no keyspace row.",
    },
    ProcedureSignature {
        name: "engram.algo.bfs.stats",
        display: "engram.algo.bfs.stats",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeCount", ProcType::Int), c("relationshipCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("distribution", ProcType::Map), c("outsideProjection", ProcType::Int)],
        mode: ProcMode::Read,
        description: "Unweighted hop distance from a source node. One summary row.",
    },
    ProcedureSignature {
        name: "engram.algo.bfs.stream",
        display: "engram.algo.bfs.stream",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeId", ProcType::Int), c("node", ProcType::Node), c("depth", ProcType::Int), c("asOf", ProcType::Int)],
        mode: ProcMode::Read,
        description: "Unweighted hop distance from a source node. One row per node, ascending by node id.",
    },
    ProcedureSignature {
        name: "engram.algo.bfs.write",
        display: "engram.algo.bfs.write",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("writeProperty", ProcType::Str), c("nodesWritten", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("committedAt", ProcType::Int)],
        mode: ProcMode::Write,
        description: "Unweighted hop distance from a source node. Persists as a node property, after the computation. The whole write-back is ONE atomic statement — `writeBatchSize` bounds the chunk the write loop works in, not a transaction boundary — so a failure leaves nothing behind rather than half a graph annotated.",
    },
    ProcedureSignature {
        name: "engram.algo.closeness.mutate",
        display: "engram.algo.closeness.mutate",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("mutateKey", ProcType::Str), c("nodeCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int)],
        mode: ProcMode::Write,
        description: "Closeness centrality in the Wasserman-Faust form: how near a node is to every node it can reach, scaled by the fraction of the graph it reaches so a small tight cluster does not outrank the whole graph. Cost is O(V x E), priced against the all-pairs work ceiling. Publishes into the result cache under a key; touches no keyspace row.",
    },
    ProcedureSignature {
        name: "engram.algo.closeness.stats",
        display: "engram.algo.closeness.stats",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeCount", ProcType::Int), c("relationshipCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("distribution", ProcType::Map), c("outsideProjection", ProcType::Int)],
        mode: ProcMode::Read,
        description: "Closeness centrality in the Wasserman-Faust form: how near a node is to every node it can reach, scaled by the fraction of the graph it reaches so a small tight cluster does not outrank the whole graph. Cost is O(V x E), priced against the all-pairs work ceiling. One summary row.",
    },
    ProcedureSignature {
        name: "engram.algo.closeness.stream",
        display: "engram.algo.closeness.stream",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeId", ProcType::Int), c("node", ProcType::Node), c("score", ProcType::Float), c("asOf", ProcType::Int)],
        mode: ProcMode::Read,
        description: "Closeness centrality in the Wasserman-Faust form: how near a node is to every node it can reach, scaled by the fraction of the graph it reaches so a small tight cluster does not outrank the whole graph. Cost is O(V x E), priced against the all-pairs work ceiling. One row per node, ascending by node id.",
    },
    ProcedureSignature {
        name: "engram.algo.closeness.write",
        display: "engram.algo.closeness.write",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("writeProperty", ProcType::Str), c("nodesWritten", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("committedAt", ProcType::Int)],
        mode: ProcMode::Write,
        description: "Closeness centrality in the Wasserman-Faust form: how near a node is to every node it can reach, scaled by the fraction of the graph it reaches so a small tight cluster does not outrank the whole graph. Cost is O(V x E), priced against the all-pairs work ceiling. Persists as a node property, after the computation. The whole write-back is ONE atomic statement — `writeBatchSize` bounds the chunk the write loop works in, not a transaction boundary — so a failure leaves nothing behind rather than half a graph annotated.",
    },
    ProcedureSignature {
        name: "engram.algo.degree.mutate",
        display: "engram.algo.degree.mutate",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("mutateKey", ProcType::Str), c("nodeCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int)],
        mode: ProcMode::Write,
        description: "Degree centrality, weighted when a relationship weight property is given. Publishes into the result cache under a key; touches no keyspace row.",
    },
    ProcedureSignature {
        name: "engram.algo.degree.stats",
        display: "engram.algo.degree.stats",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeCount", ProcType::Int), c("relationshipCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("distribution", ProcType::Map), c("outsideProjection", ProcType::Int)],
        mode: ProcMode::Read,
        description: "Degree centrality, weighted when a relationship weight property is given. One summary row.",
    },
    ProcedureSignature {
        name: "engram.algo.degree.stream",
        display: "engram.algo.degree.stream",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeId", ProcType::Int), c("node", ProcType::Node), c("degree", ProcType::Float), c("asOf", ProcType::Int)],
        mode: ProcMode::Read,
        description: "Degree centrality, weighted when a relationship weight property is given. One row per node, ascending by node id.",
    },
    ProcedureSignature {
        name: "engram.algo.degree.write",
        display: "engram.algo.degree.write",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("writeProperty", ProcType::Str), c("nodesWritten", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("committedAt", ProcType::Int)],
        mode: ProcMode::Write,
        description: "Degree centrality, weighted when a relationship weight property is given. Persists as a node property, after the computation. The whole write-back is ONE atomic statement — `writeBatchSize` bounds the chunk the write loop works in, not a transaction boundary — so a failure leaves nothing behind rather than half a graph annotated.",
    },
    ProcedureSignature {
        name: "engram.algo.kshortestpaths.stream",
        display: "engram.algo.kShortestPaths.stream",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("index", ProcType::Int), c("sourceNode", ProcType::Int), c("targetNode", ProcType::Int), c("totalCost", ProcType::Float), c("nodeIds", ProcType::List), c("asOf", ProcType::Int)],
        mode: ProcMode::Read,
        description: "The k shortest LOOPLESS routes between two nodes, by Yen's algorithm. Needs `sourceNode` and `targetNode`; `k` defaults to 1. Stream only — a route is not a per-node value, so there is nothing for the other three modes to write or summarise. Returns fewer than k rows when the graph holds fewer distinct routes.",
    },
    ProcedureSignature {
        name: "engram.algo.labelpropagation.mutate",
        display: "engram.algo.labelPropagation.mutate",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("mutateKey", ProcType::Str), c("nodeCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int)],
        mode: ProcMode::Write,
        description: "Communities by synchronous label propagation. Publishes into the result cache under a key; touches no keyspace row.",
    },
    ProcedureSignature {
        name: "engram.algo.labelpropagation.stats",
        display: "engram.algo.labelPropagation.stats",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeCount", ProcType::Int), c("relationshipCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("distribution", ProcType::Map), c("outsideProjection", ProcType::Int)],
        mode: ProcMode::Read,
        description: "Communities by synchronous label propagation. One summary row.",
    },
    ProcedureSignature {
        name: "engram.algo.labelpropagation.stream",
        display: "engram.algo.labelPropagation.stream",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeId", ProcType::Int), c("node", ProcType::Node), c("communityId", ProcType::Int), c("asOf", ProcType::Int)],
        mode: ProcMode::Read,
        description: "Communities by synchronous label propagation. One row per node, ascending by node id.",
    },
    ProcedureSignature {
        name: "engram.algo.labelpropagation.write",
        display: "engram.algo.labelPropagation.write",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("writeProperty", ProcType::Str), c("nodesWritten", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("committedAt", ProcType::Int)],
        mode: ProcMode::Write,
        description: "Communities by synchronous label propagation. Persists as a node property, after the computation. The whole write-back is ONE atomic statement — `writeBatchSize` bounds the chunk the write loop works in, not a transaction boundary — so a failure leaves nothing behind rather than half a graph annotated.",
    },
    ProcedureSignature {
        name: "engram.algo.localclusteringcoefficient.mutate",
        display: "engram.algo.localClusteringCoefficient.mutate",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("mutateKey", ProcType::Str), c("nodeCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int)],
        mode: ProcMode::Write,
        description: "The local clustering coefficient of each node. Publishes into the result cache under a key; touches no keyspace row.",
    },
    ProcedureSignature {
        name: "engram.algo.localclusteringcoefficient.stats",
        display: "engram.algo.localClusteringCoefficient.stats",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeCount", ProcType::Int), c("relationshipCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("distribution", ProcType::Map), c("outsideProjection", ProcType::Int)],
        mode: ProcMode::Read,
        description: "The local clustering coefficient of each node. One summary row.",
    },
    ProcedureSignature {
        name: "engram.algo.localclusteringcoefficient.stream",
        display: "engram.algo.localClusteringCoefficient.stream",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeId", ProcType::Int), c("node", ProcType::Node), c("coefficient", ProcType::Float), c("asOf", ProcType::Int)],
        mode: ProcMode::Read,
        description: "The local clustering coefficient of each node. One row per node, ascending by node id.",
    },
    ProcedureSignature {
        name: "engram.algo.localclusteringcoefficient.write",
        display: "engram.algo.localClusteringCoefficient.write",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("writeProperty", ProcType::Str), c("nodesWritten", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("committedAt", ProcType::Int)],
        mode: ProcMode::Write,
        description: "The local clustering coefficient of each node. Persists as a node property, after the computation. The whole write-back is ONE atomic statement — `writeBatchSize` bounds the chunk the write loop works in, not a transaction boundary — so a failure leaves nothing behind rather than half a graph annotated.",
    },
    ProcedureSignature {
        name: "engram.algo.louvain.mutate",
        display: "engram.algo.louvain.mutate",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("mutateKey", ProcType::Str), c("nodeCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int)],
        mode: ProcMode::Write,
        description: "Communities by Louvain modularity optimisation. Publishes into the result cache under a key; touches no keyspace row.",
    },
    ProcedureSignature {
        name: "engram.algo.louvain.stats",
        display: "engram.algo.louvain.stats",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeCount", ProcType::Int), c("relationshipCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("distribution", ProcType::Map), c("outsideProjection", ProcType::Int)],
        mode: ProcMode::Read,
        description: "Communities by Louvain modularity optimisation. One summary row.",
    },
    ProcedureSignature {
        name: "engram.algo.louvain.stream",
        display: "engram.algo.louvain.stream",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeId", ProcType::Int), c("node", ProcType::Node), c("communityId", ProcType::Int), c("asOf", ProcType::Int)],
        mode: ProcMode::Read,
        description: "Communities by Louvain modularity optimisation. One row per node, ascending by node id.",
    },
    ProcedureSignature {
        name: "engram.algo.louvain.write",
        display: "engram.algo.louvain.write",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("writeProperty", ProcType::Str), c("nodesWritten", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("committedAt", ProcType::Int)],
        mode: ProcMode::Write,
        description: "Communities by Louvain modularity optimisation. Persists as a node property, after the computation. The whole write-back is ONE atomic statement — `writeBatchSize` bounds the chunk the write loop works in, not a transaction boundary — so a failure leaves nothing behind rather than half a graph annotated.",
    },
    ProcedureSignature {
        name: "engram.algo.pagerank.mutate",
        display: "engram.algo.pageRank.mutate",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("mutateKey", ProcType::Str), c("nodeCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int)],
        mode: ProcMode::Write,
        description: "PageRank over a projection, by the power method. Publishes into the result cache under a key; touches no keyspace row.",
    },
    ProcedureSignature {
        name: "engram.algo.pagerank.stats",
        display: "engram.algo.pageRank.stats",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeCount", ProcType::Int), c("relationshipCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("distribution", ProcType::Map), c("outsideProjection", ProcType::Int)],
        mode: ProcMode::Read,
        description: "PageRank over a projection, by the power method. One summary row.",
    },
    ProcedureSignature {
        name: "engram.algo.pagerank.stream",
        display: "engram.algo.pageRank.stream",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeId", ProcType::Int), c("node", ProcType::Node), c("score", ProcType::Float), c("asOf", ProcType::Int)],
        mode: ProcMode::Read,
        description: "PageRank over a projection, by the power method. One row per node, ascending by node id.",
    },
    ProcedureSignature {
        name: "engram.algo.pagerank.write",
        display: "engram.algo.pageRank.write",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("writeProperty", ProcType::Str), c("nodesWritten", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("committedAt", ProcType::Int)],
        mode: ProcMode::Write,
        description: "PageRank over a projection, by the power method. Persists as a node property, after the computation. The whole write-back is ONE atomic statement — `writeBatchSize` bounds the chunk the write loop works in, not a transaction boundary — so a failure leaves nothing behind rather than half a graph annotated.",
    },
    ProcedureSignature {
        name: "engram.algo.project",
        display: "engram.algo.project",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("projection", ProcType::Str), c("nodeCount", ProcType::Int), c("relationshipCount", ProcType::Int), c("outsideProjection", ProcType::Int), c("asOf", ProcType::Int)],
        mode: ProcMode::Read,
        description: "Build an in-memory projection from ROWS: `{name, nodeLabels, edges, orientation}`, where `edges` is a list of `{source, target, weight}`. Writes nothing. Lives until its statement ends; pass the YIELDed `projection` handle to an algorithm as `projection:`. Vertices come from `nodeLabels`, so an isolated node is still in the graph; a missing or non-numeric weight is refused, never defaulted.",
    },
    ProcedureSignature {
        name: "engram.algo.result.drop",
        display: "engram.algo.result.drop",
        args: &[a("config", ProcType::Map)],
        outputs: &[c("mutateKey", ProcType::Str), c("dropped", ProcType::Bool)],
        mode: ProcMode::Write,
        description: "Remove a cached algorithm result.",
    },
    ProcedureSignature {
        name: "engram.algo.result.list",
        display: "engram.algo.result.list",
        args: &[],
        outputs: &[c("mutateKey", ProcType::Str), c("algorithm", ProcType::Str), c("asOf", ProcType::Int), c("nodeCount", ProcType::Int), c("bytes", ProcType::Int), c("stale", ProcType::Bool)],
        mode: ProcMode::Read,
        description: "Every cached algorithm result, with the snapshot it describes.",
    },
    ProcedureSignature {
        name: "engram.algo.result.stream",
        display: "engram.algo.result.stream",
        args: &[a("config", ProcType::Map)],
        outputs: &[c("nodeId", ProcType::Int), c("node", ProcType::Node), c("value", ProcType::Any), c("asOf", ProcType::Int)],
        mode: ProcMode::Read,
        description: "Stream a cached algorithm result by its key.",
    },
    ProcedureSignature {
        name: "engram.algo.scc.mutate",
        display: "engram.algo.scc.mutate",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("mutateKey", ProcType::Str), c("nodeCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int)],
        mode: ProcMode::Write,
        description: "Strongly connected components by Kosaraju's two-pass algorithm: which nodes can reach each other FOLLOWING the arrows, where weakly connected components ignores direction. Components are labelled by their smallest node id, as WCC labels them, so the two are comparable. Publishes into the result cache under a key; touches no keyspace row.",
    },
    ProcedureSignature {
        name: "engram.algo.scc.stats",
        display: "engram.algo.scc.stats",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeCount", ProcType::Int), c("relationshipCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("distribution", ProcType::Map), c("outsideProjection", ProcType::Int)],
        mode: ProcMode::Read,
        description: "Strongly connected components by Kosaraju's two-pass algorithm: which nodes can reach each other FOLLOWING the arrows, where weakly connected components ignores direction. Components are labelled by their smallest node id, as WCC labels them, so the two are comparable. One summary row.",
    },
    ProcedureSignature {
        name: "engram.algo.scc.stream",
        display: "engram.algo.scc.stream",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeId", ProcType::Int), c("node", ProcType::Node), c("componentId", ProcType::Int), c("asOf", ProcType::Int)],
        mode: ProcMode::Read,
        description: "Strongly connected components by Kosaraju's two-pass algorithm: which nodes can reach each other FOLLOWING the arrows, where weakly connected components ignores direction. Components are labelled by their smallest node id, as WCC labels them, so the two are comparable. One row per node, ascending by node id.",
    },
    ProcedureSignature {
        name: "engram.algo.scc.write",
        display: "engram.algo.scc.write",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("writeProperty", ProcType::Str), c("nodesWritten", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("committedAt", ProcType::Int)],
        mode: ProcMode::Write,
        description: "Strongly connected components by Kosaraju's two-pass algorithm: which nodes can reach each other FOLLOWING the arrows, where weakly connected components ignores direction. Components are labelled by their smallest node id, as WCC labels them, so the two are comparable. Persists as a node property, after the computation. The whole write-back is ONE atomic statement — `writeBatchSize` bounds the chunk the write loop works in, not a transaction boundary — so a failure leaves nothing behind rather than half a graph annotated.",
    },
    ProcedureSignature {
        name: "engram.algo.sssp.mutate",
        display: "engram.algo.sssp.mutate",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("mutateKey", ProcType::Str), c("nodeCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int)],
        mode: ProcMode::Write,
        description: "Weighted shortest distance from a source node, by Dijkstra. Publishes into the result cache under a key; touches no keyspace row.",
    },
    ProcedureSignature {
        name: "engram.algo.sssp.stats",
        display: "engram.algo.sssp.stats",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeCount", ProcType::Int), c("relationshipCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("distribution", ProcType::Map), c("outsideProjection", ProcType::Int)],
        mode: ProcMode::Read,
        description: "Weighted shortest distance from a source node, by Dijkstra. One summary row.",
    },
    ProcedureSignature {
        name: "engram.algo.sssp.stream",
        display: "engram.algo.sssp.stream",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeId", ProcType::Int), c("node", ProcType::Node), c("distance", ProcType::Float), c("asOf", ProcType::Int)],
        mode: ProcMode::Read,
        description: "Weighted shortest distance from a source node, by Dijkstra. One row per node, ascending by node id.",
    },
    ProcedureSignature {
        name: "engram.algo.sssp.write",
        display: "engram.algo.sssp.write",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("writeProperty", ProcType::Str), c("nodesWritten", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("committedAt", ProcType::Int)],
        mode: ProcMode::Write,
        description: "Weighted shortest distance from a source node, by Dijkstra. Persists as a node property, after the computation. The whole write-back is ONE atomic statement — `writeBatchSize` bounds the chunk the write loop works in, not a transaction boundary — so a failure leaves nothing behind rather than half a graph annotated.",
    },
    ProcedureSignature {
        name: "engram.algo.trianglecount.mutate",
        display: "engram.algo.triangleCount.mutate",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("mutateKey", ProcType::Str), c("nodeCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int)],
        mode: ProcMode::Write,
        description: "How many triangles each node takes part in. Publishes into the result cache under a key; touches no keyspace row.",
    },
    ProcedureSignature {
        name: "engram.algo.trianglecount.stats",
        display: "engram.algo.triangleCount.stats",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeCount", ProcType::Int), c("relationshipCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("distribution", ProcType::Map), c("outsideProjection", ProcType::Int)],
        mode: ProcMode::Read,
        description: "How many triangles each node takes part in. One summary row.",
    },
    ProcedureSignature {
        name: "engram.algo.trianglecount.stream",
        display: "engram.algo.triangleCount.stream",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeId", ProcType::Int), c("node", ProcType::Node), c("triangleCount", ProcType::Int), c("asOf", ProcType::Int)],
        mode: ProcMode::Read,
        description: "How many triangles each node takes part in. One row per node, ascending by node id.",
    },
    ProcedureSignature {
        name: "engram.algo.trianglecount.write",
        display: "engram.algo.triangleCount.write",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("writeProperty", ProcType::Str), c("nodesWritten", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("committedAt", ProcType::Int)],
        mode: ProcMode::Write,
        description: "How many triangles each node takes part in. Persists as a node property, after the computation. The whole write-back is ONE atomic statement — `writeBatchSize` bounds the chunk the write loop works in, not a transaction boundary — so a failure leaves nothing behind rather than half a graph annotated.",
    },
    ProcedureSignature {
        name: "engram.algo.wcc.mutate",
        display: "engram.algo.wcc.mutate",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("mutateKey", ProcType::Str), c("nodeCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int)],
        mode: ProcMode::Write,
        description: "Weakly connected components, labelled by each component's smallest node id. Publishes into the result cache under a key; touches no keyspace row.",
    },
    ProcedureSignature {
        name: "engram.algo.wcc.stats",
        display: "engram.algo.wcc.stats",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeCount", ProcType::Int), c("relationshipCount", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("distribution", ProcType::Map), c("outsideProjection", ProcType::Int)],
        mode: ProcMode::Read,
        description: "Weakly connected components, labelled by each component's smallest node id. One summary row.",
    },
    ProcedureSignature {
        name: "engram.algo.wcc.stream",
        display: "engram.algo.wcc.stream",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("nodeId", ProcType::Int), c("node", ProcType::Node), c("componentId", ProcType::Int), c("asOf", ProcType::Int)],
        mode: ProcMode::Read,
        description: "Weakly connected components, labelled by each component's smallest node id. One row per node, ascending by node id.",
    },
    ProcedureSignature {
        name: "engram.algo.wcc.write",
        display: "engram.algo.wcc.write",
        args: &[opt("config", ProcType::Map)],
        outputs: &[c("writeProperty", ProcType::Str), c("nodesWritten", ProcType::Int), c("iterations", ProcType::Int), c("converged", ProcType::Bool), c("asOf", ProcType::Int), c("committedAt", ProcType::Int)],
        mode: ProcMode::Write,
        description: "Weakly connected components, labelled by each component's smallest node id. Persists as a node property, after the computation. The whole write-back is ONE atomic statement — `writeBatchSize` bounds the chunk the write loop works in, not a transaction boundary — so a failure leaves nothing behind rather than half a graph annotated.",
    },
    ProcedureSignature {
        name: "engram.checkpoint",
        display: "engram.checkpoint",
        args: &[],
        outputs: &[
            c("spilled", ProcType::Int),
            c("segments", ProcType::Int),
            c("resident", ProcType::Int),
            c("tail", ProcType::Int),
        ],
        mode: ProcMode::Write,
        description: "Forces a checkpoint and reports what it moved.",
    },
];

/// The signature of `name`, if the engine implements it.
///
/// `name` must be the lower-cased, dot-joined spelling the parser produces.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static ProcedureSignature> {
    match CATALOG.binary_search_by(|s| s.name.cmp(name)) {
        Ok(i) => Some(&CATALOG[i]),
        Err(_) => None,
    }
}

/// Whether calling `name` leaves the graph unchanged.
///
/// **AN UNKNOWN NAME IS NOT READ-ONLY.** This fails closed: a procedure the
/// catalogue has never heard of is refused at run time anyway, and classifying
/// it as read-only in the meantime would admit it to a read transaction on the
/// strength of nothing at all.
#[must_use]
pub fn is_read_only(name: &str) -> bool {
    match lookup(name) {
        Some(s) => s.mode.is_read_only(),
        None => false,
    }
}

/// Every implemented procedure name, in catalogue order.
pub fn names() -> impl Iterator<Item = &'static str> {
    CATALOG.iter().map(|s| s.name)
}
