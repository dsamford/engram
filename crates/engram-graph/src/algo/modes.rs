//! The four execution modes, over one computation and one cache.

use engram_cypher::Value;

use super::graph::ProjectionKey;

/// What a caller wants done with the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Return one row per node.
    Stream,
    /// Return one summary row.
    Stats,
    /// Publish into the result cache under a user-chosen key.
    ///
    /// The composition mechanism, equivalent to GDS's `mutate`: an algorithm
    /// reads a previous result by name without anything touching durable
    /// state.
    Mutate,
    /// Persist as a node property.
    ///
    /// **Through the ORDINARY write path, in a separate short transaction,
    /// after the computation has finished.** See `algo::modes::write_back`.
    Write,
}

/// The per-node values an algorithm produced.
#[derive(Debug, Clone, PartialEq)]
pub enum AlgoValues {
    /// A score per node.
    Float(Vec<f64>),
    /// A component or community id per node.
    Id(Vec<u64>),
    /// A count per node.
    Count(Vec<u64>),
    /// A distance per node, absent where unreachable.
    Distance(Vec<Option<f64>>),
    /// A depth per node, absent where unreachable.
    Depth(Vec<Option<u32>>),
}

impl AlgoValues {
    /// How many nodes carry a value.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            AlgoValues::Float(v) => v.len(),
            AlgoValues::Id(v) | AlgoValues::Count(v) => v.len(),
            AlgoValues::Distance(v) => v.len(),
            AlgoValues::Depth(v) => v.len(),
        }
    }

    /// Whether there are no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// One node's value as a Cypher value, under LDBC Graphalytics'
    /// unreachable sentinels.
    ///
    /// BFS writes `9223372036854775807` and SSSP the literal `Infinity` — two
    /// DIFFERENT sentinels, confirmed against LDBC's own `bfs/dir-output` and
    /// `sssp/dir-output`, where vertex 9 is unreachable in both. `at` answers
    /// `Null` for the same rows, which is the better answer for a query and
    /// the wrong one for the benchmark; see `AlgoConfig::graphalytics`.
    #[must_use]
    pub fn at_conformant(&self, i: usize) -> Value {
        match self {
            AlgoValues::Distance(v) => match v.get(i) {
                Some(Some(d)) => Value::Float(*d),
                _ => Value::Float(f64::INFINITY),
            },
            AlgoValues::Depth(v) => match v.get(i) {
                Some(Some(d)) => Value::Int(i64::from(*d)),
                _ => Value::Int(i64::MAX),
            },
            other => other.at(i),
        }
    }

    /// One node's value as a Cypher value, or `Null` where it has none.
    #[must_use]
    pub fn at(&self, i: usize) -> Value {
        match self {
            AlgoValues::Float(v) => v.get(i).map_or(Value::Null, |x| Value::Float(*x)),
            AlgoValues::Id(v) | AlgoValues::Count(v) => {
                v.get(i).map_or(Value::Null, |x| Value::Int(*x as i64))
            }
            AlgoValues::Distance(v) => match v.get(i) {
                Some(Some(d)) => Value::Float(*d),
                _ => Value::Null,
            },
            AlgoValues::Depth(v) => match v.get(i) {
                Some(Some(d)) => Value::Int(i64::from(*d)),
                _ => Value::Null,
            },
        }
    }

    /// The values as floats, for a distribution summary.
    #[must_use]
    pub fn as_floats(&self) -> Vec<f64> {
        match self {
            AlgoValues::Float(v) => v.clone(),
            AlgoValues::Id(v) | AlgoValues::Count(v) => v.iter().map(|x| *x as f64).collect(),
            // UNREACHABLE VERTICES ARE DROPPED, not counted. A distribution
            // summary over a graph with an unreachable component therefore
            // reports a mean over the reachable part and says nothing about
            // the rest. Left as it is because a summary of infinities is not
            // meaningful either — recorded so the omission is a choice on the
            // record rather than an accident nobody noticed.
            AlgoValues::Distance(v) => v.iter().filter_map(|x| *x).collect(),
            AlgoValues::Depth(v) => v.iter().filter_map(|x| x.map(f64::from)).collect(),
        }
    }
}

/// One algorithm run's output.
#[derive(Debug, Clone)]
pub struct AlgoResult {
    /// The algorithm's name.
    pub algorithm: String,
    /// The node ids, ascending — the dense numbering's inverse.
    pub ids: Vec<u64>,
    /// The values, parallel to `ids`.
    pub values: AlgoValues,
    /// **The snapshot the values describe.**
    ///
    /// Reported in every mode, including `stream`, where it is constant per
    /// row. That redundancy is deliberate: the alternative is a user joining a
    /// stream result to a stats result to discover its vintage.
    pub as_of: u64,
    /// How many iterations ran.
    pub iterations: u32,
    /// Whether the run asked for LDBC Graphalytics semantics.
    ///
    /// Carried on the RESULT rather than read from the config at render time:
    /// the renderer (`algo_stream_rows`) does not see the config, and a flag
    /// that describes how values were computed belongs with the values.
    pub graphalytics: bool,
    /// Whether the algorithm converged, or hit its cap.
    pub converged: bool,
    /// Edges whose other end fell outside the projection.
    pub outside: u64,
    /// Extra per-algorithm summary values (modularity, community count).
    pub extra: Vec<(String, Value)>,
}

impl AlgoResult {
    /// How many bytes this result holds, for the cache budget.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.ids.len() * 8
            + match &self.values {
                AlgoValues::Float(v) => v.len() * 8,
                AlgoValues::Id(v) | AlgoValues::Count(v) => v.len() * 8,
                AlgoValues::Distance(v) => v.len() * 16,
                AlgoValues::Depth(v) => v.len() * 8,
            }
    }
}

/// A parsed algorithm configuration.
#[derive(Debug, Clone)]
pub struct AlgoConfig {
    /// Which slice of the graph to run over.
    pub projection: ProjectionKey,
    /// The mode.
    pub mode: Mode,
    /// PageRank's damping.
    pub damping: f64,
    /// The convergence threshold.
    pub tolerance: f64,
    /// The iteration cap.
    pub max_iterations: u32,
    /// Louvain's resolution.
    pub resolution: f64,
    /// The source node for a traversal.
    pub source: Option<i64>,
    /// `targetNode` — the route's far end, for the path procedures.
    pub target: Option<i64>,
    /// `k` — how many routes a k-shortest-paths call asks for.
    pub k: usize,
    /// `concurrency` — an UPPER BOUND on the executor width this run may use.
    ///
    /// It can only ever narrow. The engine never spawns a thread, so the
    /// width available is whatever executor the server installed, and a config
    /// key cannot conjure workers that do not exist. Asking for more than the
    /// server has is honoured as "all of it" rather than refused, which is
    /// what a GDS-shaped call expects; asking for less genuinely gives less.
    ///
    /// It was accepted and IGNORED for a release — a key in the accepted list
    /// that changed nothing, in a parser whose own doc says a silently ignored
    /// key is a wrong answer that looks right. It was right about that.
    pub concurrency: Option<usize>,
    /// The cache key for `mutate`, or the key to read.
    pub mutate_key: Option<String>,
    /// The property `write` sets.
    pub write_property: Option<String>,
    /// How many nodes per write transaction.
    pub write_batch_size: usize,
    /// **LDBC Graphalytics semantics**, off by default.
    ///
    /// `engram.algo.*` is a shipped product surface, and five of its six
    /// Graphalytics kernels diverge from the published specification in ways
    /// that are defensible as engine behaviour but wrong as conformance:
    ///
    /// | kernel | default | `graphalytics: true` |
    /// |---|---|---|
    /// | BFS | unreachable is `null` | `9223372036854775807` |
    /// | SSSP | unreachable is `null` | `Infinity` |
    /// | CDLP | in/out neighbours deduplicated | counted separately |
    /// | PageRank | stops at `tolerance` | exactly `maxIterations` rounds |
    /// | LCC | triangle test symmetrised | direction kept in the test |
    ///
    /// Gated rather than switched because both readings are legitimate: a
    /// `null` unreachable distance is the better answer for a query, and
    /// `9223372036854775807` is the one the benchmark validates. Changing the
    /// default would silently alter answers for every existing caller, and
    /// leaving no way to conform would make the benchmark unrunnable. So the
    /// caller says which they want, and the conformance tests pass `true`.
    pub graphalytics: bool,
    /// `projection` — the handle of a graph `engram.algo.project` built from
    /// rows in this statement. Exclusive with `nodeLabels`,
    /// `relationshipTypes`, `relationshipWeightProperty` and `orientation`:
    /// the projection already fixed all four.
    pub named: Option<String>,
}

impl Default for AlgoConfig {
    fn default() -> Self {
        AlgoConfig {
            projection: ProjectionKey {
                labels: Vec::new(),
                types: Vec::new(),
                dir: crate::Dir::Out,
                weight: None,
            },
            mode: Mode::Stream,
            damping: 0.85,
            tolerance: 1e-7,
            max_iterations: 20,
            resolution: 1.0,
            source: None,
            target: None,
            k: 1,
            concurrency: None,
            mutate_key: None,
            write_property: None,
            write_batch_size: 10_000,
            graphalytics: false,
            named: None,
        }
    }
}

/// Percentiles of a value distribution, for a `stats` row.
#[must_use]
pub fn distribution(values: &[f64]) -> Vec<(String, Value)> {
    if values.is_empty() {
        return Vec::new();
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let at = |p: f64| -> f64 {
        let i = ((v.len() as f64 - 1.0) * p).round() as usize;
        v[i.min(v.len() - 1)]
    };
    let sum: f64 = v.iter().sum();
    vec![
        ("min".to_string(), Value::Float(v[0])),
        ("max".to_string(), Value::Float(v[v.len() - 1])),
        ("mean".to_string(), Value::Float(sum / v.len() as f64)),
        ("p50".to_string(), Value::Float(at(0.50))),
        ("p75".to_string(), Value::Float(at(0.75))),
        ("p90".to_string(), Value::Float(at(0.90))),
        ("p99".to_string(), Value::Float(at(0.99))),
    ]
}
