//! The `engram.algo.*` procedure surface.
//!
//! # The namespace, and why not `gds.*`
//!
//! `gds.*` names promise a graph catalogue — `gds.graph.project`, named
//! graphs, `gds.graph.drop` — which this engine deliberately does not have: a
//! projection here is built from a config map and thrown away. One built from
//! ROWS by `engram.algo.project` is thrown away too, when the statement that
//! built it ends — it has a name only so the same statement can read it back,
//! and nothing outlives the statement. Squatting the
//! namespace while diverging on semantics is worse than a distinct name, so
//! these live under `engram.*`, which is already reserved and already occupied
//! by `engram.checkpoint`.

use std::collections::BTreeMap;

use engram_cypher::Value;
use engram_observe::{counted, sometimes};

use super::cache::CACHE_BYTES;
use super::modes::{AlgoConfig, AlgoResult, Mode, distribution};
use super::run::{Algorithm, mode_of};
use super::{ProjectionKey, Refusal, kernels};
use crate::scoped_exec::ScopedExec;
use crate::{Dir, Graph};

/// The config keys an algorithm accepts.
const KEYS: &[&str] = &[
    "nodeLabels",
    "relationshipTypes",
    "orientation",
    "relationshipWeightProperty",
    "maxIterations",
    "tolerance",
    "dampingFactor",
    "resolution",
    "sourceNode",
    "targetNode",
    "k",
    "mutateKey",
    "writeProperty",
    "writeBatchSize",
    "concurrency",
    "graphalytics",
    "projection",
];

/// Parse a configuration map.
///
/// **AN UNKNOWN KEY IS AN ERROR, WITH A SUGGESTION.** A misspelled `tolerance`
/// that is silently ignored is a wrong answer that looks right — the algorithm
/// runs to a different convergence than the one asked for and says nothing.
/// This is "refuse rather than guess" applied to configuration.
pub(crate) fn parse_config(v: Option<&Value>) -> Result<AlgoConfig, String> {
    let mut cfg = AlgoConfig::default();
    let Some(Value::Map(m)) = v else {
        if v.is_none() || matches!(v, Some(Value::Null)) {
            return Ok(cfg);
        }
        return Err("the configuration must be a map".into());
    };
    for k in m.keys() {
        if !KEYS.contains(&k.as_str()) {
            let hint = nearest(k);
            return Err(match hint {
                Some(h) => format!("unknown config key `{k}`; did you mean `{h}`?"),
                None => format!(
                    "unknown config key `{k}`; accepted keys are {}",
                    KEYS.join(", ")
                ),
            });
        }
    }
    let strs = |key: &str| -> Result<Vec<String>, String> {
        match m.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::List(items)) => items
                .iter()
                .map(|i| match i {
                    Value::Str(s) => Ok(s.clone()),
                    other => Err(format!("`{key}` must be a list of strings, got {other:?}")),
                })
                .collect(),
            Some(Value::Str(s)) => Ok(vec![s.clone()]),
            Some(other) => Err(format!("`{key}` must be a list of strings, got {other:?}")),
        }
    };
    let num = |key: &str| -> Result<Option<f64>, String> {
        match m.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Int(i)) => Ok(Some(*i as f64)),
            Some(Value::Float(f)) => Ok(Some(*f)),
            Some(other) => Err(format!("`{key}` must be a number, got {other:?}")),
        }
    };
    let text = |key: &str| -> Result<Option<String>, String> {
        match m.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Str(s)) => Ok(Some(s.clone())),
            Some(other) => Err(format!("`{key}` must be a string, got {other:?}")),
        }
    };

    cfg.projection = ProjectionKey {
        labels: strs("nodeLabels")?,
        types: strs("relationshipTypes")?,
        dir: match text("orientation")?.as_deref() {
            None | Some("NATURAL") => Dir::Out,
            Some("REVERSE") => Dir::In,
            Some("UNDIRECTED") => Dir::Both,
            Some(other) => {
                return Err(format!(
                    "`orientation` must be NATURAL, REVERSE or UNDIRECTED, got `{other}`"
                ));
            }
        },
        weight: text("relationshipWeightProperty")?,
    };
    // A ROW-BUILT projection already fixed its vertices, edges, weights and
    // orientation; naming it beside any of those would be two answers to one
    // question, and choosing one silently would be a wrong answer that looks
    // right.
    cfg.named = text("projection")?;
    if cfg.named.is_some()
        && (!cfg.projection.labels.is_empty()
            || !cfg.projection.types.is_empty()
            || cfg.projection.weight.is_some()
            || m.contains_key("orientation"))
    {
        return Err(
            "`projection` names a graph engram.algo.project built, which already fixed its \
             nodes, edges, weights and orientation; it cannot be combined with `nodeLabels`, \
             `relationshipTypes`, `relationshipWeightProperty` or `orientation`"
                .into(),
        );
    }
    // LDBC Graphalytics semantics — see `AlgoConfig::graphalytics`.
    if let Some(Value::Bool(b)) = m.get("graphalytics") {
        cfg.graphalytics = *b;
    }
    if let Some(n) = num("maxIterations")? {
        cfg.max_iterations = n.max(1.0) as u32;
    }
    if let Some(n) = num("tolerance")? {
        cfg.tolerance = n;
    }
    if let Some(n) = num("dampingFactor")? {
        if !(0.0..1.0).contains(&n) {
            return Err(format!("`dampingFactor` must be in [0, 1), got {n}"));
        }
        cfg.damping = n;
    }
    if let Some(n) = num("resolution")? {
        cfg.resolution = n;
    }
    if let Some(n) = num("sourceNode")? {
        cfg.source = Some(n as i64);
    }
    if let Some(n) = num("targetNode")? {
        cfg.target = Some(n as i64);
    }
    if let Some(n) = num("k")? {
        if n < 1.0 {
            return Err(format!("`k` must be at least 1, got {n}"));
        }
        cfg.k = n as usize;
    }
    if let Some(n) = num("writeBatchSize")? {
        cfg.write_batch_size = (n.max(1.0)) as usize;
    }
    if let Some(n) = num("concurrency")? {
        if n < 1.0 {
            return Err(format!("`concurrency` must be at least 1, got {n}"));
        }
        cfg.concurrency = Some(n as usize);
    }
    cfg.mutate_key = text("mutateKey")?;
    cfg.write_property = text("writeProperty")?;
    Ok(cfg)
}

/// The accepted key closest to `k`, by edit distance.
fn nearest(k: &str) -> Option<&'static str> {
    let mut best: Option<(usize, &'static str)> = None;
    for cand in KEYS {
        let d = edit_distance(k, cand);
        if d <= 3 && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, cand));
        }
    }
    best.map(|(_, c)| c)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let sub = prev[j - 1] + usize::from(a[i - 1] != b[j - 1]);
            cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Split `engram.algo.<algorithm>.<mode>` into its parts.
#[must_use]
pub(crate) fn split_name(name: &str) -> Option<(Algorithm, Mode)> {
    let rest = name.strip_prefix("engram.algo.")?;
    let (alg, mode) = rest.rsplit_once('.')?;
    let mode = mode_of(mode)?;
    let alg = match alg {
        "pagerank" => Algorithm::PageRank,
        "wcc" => Algorithm::Wcc,
        "degree" => Algorithm::Degree,
        "bfs" => Algorithm::Bfs,
        "sssp" => Algorithm::Sssp,
        "trianglecount" => Algorithm::TriangleCount,
        "localclusteringcoefficient" => Algorithm::LocalClustering,
        "labelpropagation" => Algorithm::LabelPropagation,
        "louvain" => Algorithm::Louvain,
        "betweenness" => Algorithm::Betweenness,
        "scc" => Algorithm::Scc,
        "closeness" => Algorithm::Closeness,
        _ => return None,
    };
    Some((alg, mode))
}

/// One produced row, as `(field, value)` pairs.
pub(crate) type Row = Vec<(&'static str, Value)>;

impl Graph {
    /// Run an algorithm procedure and produce its rows.
    pub(crate) fn algo_procedure(
        &self,
        name: &str,
        config: Option<&Value>,
        // Whether the statement's `YIELD` actually names the `node` column.
        // A stream row carried the whole node whether or not anyone asked for
        // it — see [`Graph::algo_stream_rows`].
        wants_node: bool,
    ) -> Result<Vec<Row>, String> {
        if name == "engram.algo.result.list" {
            return Ok(self.algo_result_list());
        }
        if name == "engram.algo.project" {
            return self.algo_project(config);
        }
        let cfg = parse_config(config)?;
        if name == "engram.algo.result.drop" {
            let key = cfg
                .mutate_key
                .ok_or("`engram.algo.result.drop` needs a `mutateKey`")?;
            let dropped = self.algo_cache.borrow_mut().drop_key(&key);
            return Ok(vec![vec![
                ("mutateKey", Value::Str(key)),
                ("dropped", Value::Bool(dropped)),
            ]]);
        }
        if name == "engram.algo.kshortestpaths.stream" {
            return self.algo_k_shortest_paths(&cfg);
        }
        if name == "engram.algo.result.stream" {
            let key = cfg
                .mutate_key
                .ok_or("`engram.algo.result.stream` needs a `mutateKey`")?;
            let Some(result) = self.algo_cache.borrow().get(&key) else {
                // AN ERROR, NOT AN EMPTY ANSWER. A result that was evicted for
                // budget, or never published, must not read as "the algorithm
                // found nothing" — those are different facts and only one of
                // them is about the graph.
                return Err(format!(
                    "no cached result under `{key}`. It was never published, or it was \
                     evicted under the {CACHE_BYTES}-byte cache budget; raise \
                     ENGRAM_ALGO_CACHE_BYTES or recompute it."
                ));
            };
            counted!("algo.result served from the cache");
            return Ok(self.algo_stream_rows(&result, "value", wants_node));
        }

        let Some((alg, mode)) = split_name(name) else {
            return Err(format!("`{name}` is not an algorithm procedure"));
        };
        // The morsel seam, reached from the ONLY surface a user has. It was
        // hard-wired to `serial()` for a revision: the fixpoint driver's
        // parallel lane was correct, tested and unreachable, which makes it
        // dead code however green its tests are. A lever that production
        // cannot pull measures nothing, and so does a lane production cannot
        // enter.
        //
        // Gated on an INSTALLED executor and on the lever, but deliberately
        // NOT on `in_txn` as `expand`'s dispatch is: the morsel body here
        // calls `VertexProgram::pull`, which reads the already-materialised
        // CSR and nothing else — no store, no overlay, no thread-local. The
        // transaction hazard that gate exists for cannot arise, and copying
        // the condition anyway would suggest a risk that had been considered
        // and found real.
        let serial = super::run::serial();
        let installed = self.exec();
        let parallel = self.algo_parallel_enabled();
        // `concurrency` NARROWS the installed width and can never widen it:
        // the engine spawns nothing, so the workers available are whatever the
        // server installed. A capped view is the honest way to honour the key
        // without promising threads that do not exist.
        let capped;
        let exec: &dyn ScopedExec = match installed.as_deref() {
            Some(e) if parallel => {
                counted!("algo.fixpoint parallel");
                match cfg.concurrency {
                    Some(c) if c < e.width() => {
                        counted!("algo.concurrency narrowed the executor");
                        capped = Capped {
                            inner: e,
                            width: c.max(1),
                        };
                        &capped
                    }
                    _ => e,
                }
            }
            _ => &serial,
        };
        let result = self.algo_run(alg, &cfg, exec).map_err(|e| e.to_string())?;

        match mode {
            Mode::Stream => Ok(self.algo_stream_rows(&result, alg.value_column(), wants_node)),
            Mode::Stats => {
                let mut row: Row = vec![
                    ("nodeCount", Value::Int(result.ids.len() as i64)),
                    (
                        "relationshipCount",
                        Value::Int(self.algo_edge_count(&cfg.projection)),
                    ),
                    ("iterations", Value::Int(i64::from(result.iterations))),
                    ("converged", Value::Bool(result.converged)),
                    ("asOf", Value::Int(result.as_of as i64)),
                    (
                        "distribution",
                        Value::Map(
                            distribution(&result.values.as_floats())
                                .into_iter()
                                .chain(result.extra.iter().cloned())
                                .collect::<BTreeMap<String, Value>>(),
                        ),
                    ),
                    ("outsideProjection", Value::Int(result.outside as i64)),
                ];
                row.truncate(7);
                Ok(vec![row])
            }
            Mode::Mutate => {
                let key = cfg
                    .mutate_key
                    .clone()
                    .ok_or("`mutate` needs a `mutateKey` in its configuration")?;
                let n = result.ids.len() as i64;
                let (iters, conv, as_of) = (result.iterations, result.converged, result.as_of);
                self.algo_cache.borrow_mut().publish(
                    &key,
                    std::sync::Arc::new(result),
                    self.algo_cache_bytes(),
                )?;
                Ok(vec![vec![
                    ("mutateKey", Value::Str(key)),
                    ("nodeCount", Value::Int(n)),
                    ("iterations", Value::Int(i64::from(iters))),
                    ("converged", Value::Bool(conv)),
                    ("asOf", Value::Int(as_of as i64)),
                ]])
            }
            Mode::Write => {
                let prop = cfg
                    .write_property
                    .clone()
                    .ok_or("`write` needs a `writeProperty` in its configuration")?;
                let (written, committed) = self
                    .algo_write_back(&result, &prop, cfg.write_batch_size)
                    .map_err(|e| e.to_string())?;
                Ok(vec![vec![
                    ("writeProperty", Value::Str(prop)),
                    ("nodesWritten", Value::Int(written as i64)),
                    ("iterations", Value::Int(i64::from(result.iterations))),
                    ("converged", Value::Bool(result.converged)),
                    // BOTH STAMPS. The values describe `asOf`; they landed at
                    // `committedAt`. GDS reports one summary and never says
                    // that the scores describe a graph that no longer exists.
                    ("asOf", Value::Int(result.as_of as i64)),
                    ("committedAt", Value::Int(committed as i64)),
                ]])
            }
        }
    }

    /// One row per vertex: its id, the node itself, the algorithm's value,
    /// and the vintage.
    ///
    /// THE NODE IS READ ONLY IF THE STATEMENT ASKS FOR IT. This materialised
    /// every vertex's record unconditionally — a store get and a full decode
    /// per vertex — even for `YIELD depth RETURN count(*)`, which never looks
    /// at a node. On the SF3 friendship graph (24,328 people) that is 24,328
    /// gets the query did not ask for, and on a cold cache they are 343,686
    /// block reads against 326,771 evictions: 35 s for a BFS whose kernel is
    /// milliseconds over 1.13M edges. Warm it measured 0 s, doing exactly the
    /// same work — which is why this hid: the reads are only expensive when
    /// the blocks are not already resident.
    ///
    /// An empty `YIELD` binds every declared column, so a bare `CALL` still
    /// gets its nodes; this skips the read only where the field is genuinely
    /// unbound.
    fn algo_stream_rows(
        &self,
        result: &AlgoResult,
        column: &'static str,
        wants_node: bool,
    ) -> Vec<Row> {
        if !wants_node {
            counted!("algo.stream skipped the node column nobody yielded");
        }
        let mut out = Vec::with_capacity(result.ids.len());
        for (i, id) in result.ids.iter().enumerate() {
            let node = if wants_node {
                self.node(*id).ok().flatten().unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            out.push(vec![
                ("nodeId", Value::Int(*id as i64)),
                ("node", node),
                (
                    column,
                    if result.graphalytics {
                        result.values.at_conformant(i)
                    } else {
                        result.values.at(i)
                    },
                ),
                // Constant per row, and reported anyway: the alternative is a
                // user joining a stream result to a stats result to learn its
                // vintage.
                ("asOf", Value::Int(result.as_of as i64)),
            ]);
        }
        out
    }

    fn algo_result_list(&self) -> Vec<Row> {
        let now = self.store.now_ts();
        self.algo_cache
            .borrow()
            .list()
            .map(|(k, c)| {
                let stale = c.result.as_of < now;
                if stale {
                    sometimes!("algo.a cached result was served stale", true);
                }
                vec![
                    ("mutateKey", Value::Str(k.clone())),
                    ("algorithm", Value::Str(c.result.algorithm.clone())),
                    ("asOf", Value::Int(c.result.as_of as i64)),
                    ("nodeCount", Value::Int(c.result.ids.len() as i64)),
                    ("bytes", Value::Int(c.bytes as i64)),
                    // REPORTED, NEVER ACTED ON. A cached result is a
                    // measurement of a past graph; deleting it because the
                    // graph moved would be destroying evidence rather than
                    // maintaining a cache.
                    ("stale", Value::Bool(stale)),
                ]
            })
            .collect()
    }

    /// `engram.algo.project` — build an in-memory projection from rows.
    ///
    /// SNB BI bi15 weights each KNOWS pair by a computed interaction score.
    /// With only stored weights to read, it had to CREATE a weighted
    /// relationship per pair inside its timed statement — at SF10 that is a
    /// join inside a writing statement, the shape that reached 109 GB for one
    /// slice of bi20's precomputation. LDBC's own bi15 writes nothing: GDS
    /// builds the graph in memory. This is that, scoped to the statement.
    fn algo_project(&self, config: Option<&Value>) -> Result<Vec<Row>, String> {
        const KEYS: &[&str] = &["name", "nodeLabels", "edges", "orientation"];
        let Some(Value::Map(m)) = config else {
            return Err("`engram.algo.project` needs a configuration map: \
                 {name, nodeLabels, edges, orientation}"
                .into());
        };
        for k in m.keys() {
            if !KEYS.contains(&k.as_str()) {
                return Err(format!(
                    "`engram.algo.project` does not accept `{k}`; it takes `name`, \
                     `nodeLabels`, `edges` and `orientation`"
                ));
            }
        }
        let name = match m.get("name") {
            Some(Value::Str(s)) if !s.is_empty() && !s.contains('#') => s.clone(),
            Some(Value::Str(_)) => {
                return Err("`name` must be non-empty and must not contain `#`, which \
                     separates the name from its statement in the handle"
                    .into());
            }
            _ => return Err("`engram.algo.project` needs a string `name`".into()),
        };
        let labels: Vec<String> = match m.get("nodeLabels") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Str(s)) => vec![s.clone()],
            Some(Value::List(l)) => l
                .iter()
                .map(|v| match v {
                    Value::Str(s) => Ok(s.clone()),
                    other => Err(format!("`nodeLabels` must be strings, got {other:?}")),
                })
                .collect::<Result<_, _>>()?,
            Some(other) => {
                return Err(format!("`nodeLabels` must be a list of strings, got {other:?}"));
            }
        };
        let dir = match m.get("orientation") {
            None | Some(Value::Null) => Dir::Out,
            Some(Value::Str(s)) => match s.as_str() {
                "NATURAL" => Dir::Out,
                "REVERSE" => Dir::In,
                "UNDIRECTED" => Dir::Both,
                other => {
                    return Err(format!(
                        "`orientation` must be NATURAL, REVERSE or UNDIRECTED, got `{other}`"
                    ));
                }
            },
            Some(other) => return Err(format!("`orientation` must be a string, got {other:?}")),
        };
        let Some(Value::List(list)) = m.get("edges") else {
            return Err("`engram.algo.project` needs `edges`: a list of \
                 {source, target, weight}, typically from collect()"
                .into());
        };
        let node_id = |v: Option<&Value>, what: &str, i: usize| -> Result<u64, String> {
            match v {
                Some(Value::Int(n)) if *n >= 0 => Ok(*n as u64),
                Some(Value::Node { id, .. }) => Ok(*id),
                other => Err(format!(
                    "edge {i}: `{what}` must be a node or a node id, got {other:?}"
                )),
            }
        };
        let mut edges = Vec::with_capacity(list.len());
        for (i, e) in list.iter().enumerate() {
            let Value::Map(em) = e else {
                return Err(format!("edge {i} must be a map {{source, target, weight}}, got {e:?}"));
            };
            let s = node_id(em.get("source"), "source", i)?;
            let t = node_id(em.get("target"), "target", i)?;
            // REFUSED, NEVER DEFAULTED. A stored projection counts a missing
            // weight and uses 1.0; a projection whose every weight the caller
            // computed has no business guessing one.
            let w = match em.get("weight") {
                Some(Value::Float(f)) if f.is_finite() => *f,
                Some(Value::Int(n)) => *n as f64,
                other => {
                    return Err(format!(
                        "edge {i}: `weight` must be a finite number, got {other:?}. A \
                         projection's weight is never defaulted."
                    ));
                }
            };
            edges.push((s, t, w));
        }
        let generation = crate::interp::statement_gen();
        if generation == 0 {
            return Err("`engram.algo.project` must run on its statement's own thread: call \
                 it once, after an aggregation such as collect(), not per row of a \
                 parallel stage"
                .into());
        }
        let g = self.algo_graph_from_edges(&labels, dir, &edges)?;
        let (n, rels, outside, as_of) = (g.len(), g.edge_count(), g.outside, g.as_of);
        let bytes = super::graph::row_projection_bytes(n as u64, rels as u64);
        let handle = super::graph::register_named_projection(
            generation,
            &name,
            g,
            bytes,
            self.algo_byte_ceiling(),
        )?;
        Ok(vec![vec![
            ("projection", Value::Str(handle)),
            ("nodeCount", Value::Int(n as i64)),
            ("relationshipCount", Value::Int(rels as i64)),
            ("outsideProjection", Value::Int(outside as i64)),
            ("asOf", Value::Int(as_of as i64)),
        ]])
    }

    fn algo_edge_count(&self, key: &ProjectionKey) -> i64 {
        self.algo_graph(key).map_or(0, |g| g.edge_count() as i64)
    }
}

impl Graph {
    /// `engram.algo.kshortestpaths.stream` — Yen's `k` shortest loopless
    /// routes between two nodes.
    ///
    /// # Why this is not one of the four modes
    ///
    /// Every other algorithm here answers with one value PER NODE, which is
    /// what makes `stream`, `stats`, `mutate` and `write` all sensible over
    /// one computation. This answers with ROUTES: there is nothing to write
    /// onto a node, no distribution to summarise, and a `mutate` cache keyed
    /// by node would have nowhere to put them. GDS reaches the same
    /// conclusion — `gds.shortestPath.yens` has a stream mode and no others —
    /// and forcing the shape would have produced two modes that error and one
    /// that lies.
    pub(crate) fn algo_k_shortest_paths(&self, cfg: &AlgoConfig) -> Result<Vec<Row>, String> {
        let (Some(src_id), Some(dst_id)) = (cfg.source, cfg.target) else {
            return Err(
                "`engram.algo.kshortestpaths.stream` needs both `sourceNode` and `targetNode`"
                    .into(),
            );
        };
        let full = "engram.algo.kshortestpaths".to_string();
        // k = 1 IS ONE DIJKSTRA. Yen's spur searches — a Dijkstra per node of
        // each accepted route — exist only to find routes 2..k; the first is
        // the plain shortest path, `E + V log V`, the same single-source shape
        // as SSSP. Pricing it `k x V x E` refused SNB BI's bi19 at SF3
        // (27.5e9) and bi19/bi20 at SF10 (254.5e9 / 808.9e9 "work") for a
        // computation that is one pass over the projection.
        let all_pairs = cfg.k > 1;
        let named = match &cfg.named {
            Some(h) => Some(super::graph::named_projection(h)?),
            None => None,
        };
        let (nodes, edges) = match &named {
            // priced when it was built
            Some(g) => (g.len() as u64, g.edge_count() as u64),
            None => self
                .algo_price(&full, &cfg.projection, 32, all_pairs)
                .map_err(|r| r.to_string())?,
        };
        // From k = 2, Yen runs a Dijkstra per node of each accepted route, so
        // its cost is `k x V x (E + V log V)` — the same all-pairs shape as
        // betweenness and priced against the same ceiling, scaled by `k`.
        let work = if all_pairs {
            nodes
                .saturating_mul(edges)
                .saturating_mul(cfg.k.max(1) as u64)
        } else {
            0
        };
        if work > self.algo_work_ceiling() {
            counted!("algo.refused for all-pairs work");
            return Err(Refusal {
                algorithm: full,
                what: "k x node x edge work",
                measured: work,
                ceiling: self.algo_work_ceiling(),
                lever: "ENGRAM_ALGO_WORK_CEILING",
            }
            .to_string());
        }

        let g = match named {
            Some(g) => super::graph::AlgoProjection::new(g),
            None => self
                .algo_graph(&cfg.projection)
                .map_err(|e| format!("{e:?}"))?,
        };
        // An endpoint outside the projection is a REFUSAL and not an empty
        // answer: "no route" and "you named a node this projection does not
        // contain" are different facts, and returning nothing for both leaves
        // a caller unable to tell a typo from a disconnection.
        let offset_of = |id: i64| -> Result<u32, String> {
            let id = u64::try_from(id).map_err(|_| format!("node id {id} is negative"))?;
            g.ids
                .binary_search(&id)
                .map(|i| i as u32)
                .map_err(|_| format!("node {id} is not in this projection"))
        };
        let src = offset_of(src_id)?;
        let dst = offset_of(dst_id)?;

        let routes = kernels::yen_k_shortest(&g, src, dst, cfg.k.max(1));
        let as_of = g.as_of;
        Ok(routes
            .into_iter()
            .enumerate()
            .map(|(i, r)| {
                let ids: Vec<Value> = r
                    .nodes
                    .iter()
                    .map(|o| Value::Int(g.id_of(*o) as i64))
                    .collect();
                vec![
                    ("index", Value::Int(i as i64)),
                    ("sourceNode", Value::Int(src_id)),
                    ("targetNode", Value::Int(dst_id)),
                    ("totalCost", Value::Float(r.cost)),
                    ("nodeIds", Value::List((ids).into())),
                    ("asOf", Value::Int(as_of as i64)),
                ]
            })
            .collect())
    }
}

/// An executor narrowed to at most `width` workers.
///
/// Wrapping rather than reconfiguring, because the installed executor is
/// shared by every session on the worker and a run's `concurrency` must not
/// change what anybody else gets. `for_each` is forwarded untouched: the
/// operator sizes its morsels from `width()`, so narrowing the reported width
/// is the whole of the mechanism.
struct Capped<'a> {
    inner: &'a dyn ScopedExec,
    width: usize,
}

impl ScopedExec for Capped<'_> {
    fn width(&self) -> usize {
        self.width
    }

    fn for_each(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        self.inner.for_each(n, f);
    }
}
