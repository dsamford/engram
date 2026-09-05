//! Layer-4 columnar rewrite — the incremental-strategy STEP 1 of the Tier-0
//! architecture (`docs/engine-redesign.md`, "Unified columnar execution" /
//! "REFINED to Layer-4-first"): a GENERAL, COMPOSABLE columnar pipeline over
//! the core read chain — SINGLE- AND MULTI-hop, and SINGLE- AND MULTI-PATH (a
//! chain expressed as several paths, or a branch re-rooted at an already-bound
//! var) — the composable replacement for the whole-shape recognizers in
//! `vectorized.rs`.
//!
//! Where each `try_vectorized_*` recognizer matches ONE query shape end to end,
//! this builds `scan -> expand* -> [filter] -> project` from small operators
//! over a `DataChunk` (columnar id columns, one per bound var, with a live-row
//! selection). It reuses the SAME primitives the recognizers do — `eval_column`
//! (column-at-a-time expr eval), `load_side_columns` (batched aligned property
//! reads) — and materialises through the SHARED `project_rows_tail`
//! (byte-identical ORDER BY/NULLS/DESC/stability + late-materialise) — so its
//! output is identical to the per-tuple `run_streaming` path, row-for-row AND in
//! order, or it DECLINES (`Ok(None)`) and that path answers.
//!
//! THE LOAD-BEARING FACT (canaried): `run_streaming` emits a fixed hop's
//! neighbours in REVERSE `adjacent_slim` order (the LIFO pop order of
//! `expand_var_length` from a bound start) and seeds a label scan ASCENDING
//! (`members_all`). Chaining N `expand`s reproduces this EXACTLY — including a
//! MULTI-hop path's NESTED reverse-adjacency order (seed ascending; for each
//! seed row its hop-1 neighbours reversed; for each of those its hop-2
//! neighbours reversed; …) — because `expand` walks the prior chunk's rows in
//! order and emits each row's neighbours reversed. A MULTI-PATH pattern nests
//! the SAME way: a later path's expand sources from an already-bound var and
//! walks the existing rows in order (outer) × that var's reverse-adjacency
//! (inner), so `(a)-[:T1]->(b), (a)-[:T2]->(c)` emits per-a: per-b-reversed:
//! per-c-reversed, exactly as `stream_pattern` nests one path's matches inside
//! the previous. A stable sort's ties (and a production-order LIMIT cut) depend
//! on it. An UNDIRECTED fixed hop is the SAME machinery through `Dir::Both`:
//! `adjacent_slim(src, Both)` is OUT neighbours then IN neighbours (an IN-side
//! self-loop deduped there), and both paths reverse it identically — the
//! pipeline via `adj.iter().rev()`, `run_streaming` via the LIFO pop — so the
//! neighbour order is byte-identical with no per-direction special-casing.
//! RELATIONSHIP ISOMORPHISM (a walk may not reuse a relationship) is
//! enforced WITHIN a path exactly as `expand_var_length`'s `used` set does, and
//! RE-SEEDED per path (each path's `handle_start` starts `Partial.used` empty),
//! so a relationship used by an earlier path never forbids a later path's walk —
//! self-loops, back-traversals and cross-path reuse would otherwise diverge.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use engram_cypher::ast::Expr;
use engram_cypher::bindings::VarMap;
use engram_cypher::eval::{Scope, eval_with};
use engram_cypher::stmt::{
    Clause, NodePattern, PathPattern, Pattern, ProjItem, Projection, RelDir, RelPattern,
    SingleQuery,
};
use engram_cypher::value::{Truth, Value};
use engram_observe::counted;

use crate::interp::{
    AggItem, AggSite, NULL_ID, SiteAcc, VarKind, agg_key_of, budget_check, cmp_order_keys,
    column_name, conjuncts_of, contains_opaque, distinct_vars_of_proj, eval_count,
    expr_has_aggregate, extract_aggregates, free_vars_of, plan_agg_projection, project_rows_tail,
    prop_eq_index, rel_of, run_agg_return, run_agg_with,
};
use crate::vectorized::{Side, eval_column, key_side, load_rel_columns, load_side_columns};
use crate::{Dir, Graph, GraphError, QueryResult, RunError};

/// Sentinel var names for `classify_key`'s `key_side` probe — a real Cypher
/// identifier can never contain a NUL, so these never collide with a bound var.
const SENTINEL_A: &str = "\0__pipeline_sentinel_a";
const SENTINEL_B: &str = "\0__pipeline_sentinel_b";

/// A reserved "column name" (NUL-prefixed, never a real property) under which a
/// bound NODE variable's own IDENTITY is loaded as a column of id-only
/// `Value::Node`s — so `key_side`/`eval_column` can vectorise a node-identity
/// comparison (`country = countryX`, `country IN [countryX, countryY]`, a CASE
/// over `country = countryX`). `eq3` compares graph entities by id ONLY, so an
/// id-only light node compares equal to a fully-materialised one of the same id.
/// Only NODE vars carry it (a Rel var's identity is not synthesised here, so a
/// rel-identity pred declines to the interp).
pub(crate) const NODE_IDENTITY_KEY: &str = "\0__node_identity";

// ─── COUNT-FOLD levers (pipeline-local) ──────────────────────────────────────
//
// Every other operator lever lives on `Graph` (`set_degree_aggregate`, …). These
// two are PIPELINE-LOCAL thread-locals because `lib.rs` is being edited
// concurrently by another workstream (the derived-structure race fix) and this
// file may not touch it; the lever is consulted at PLAN time (`plan_count_fold`)
// so a flipped lever changes the very next statement, exactly as a `Graph` cell
// would. Thread-local (not global) so parallel test threads never see each
// other's flips — the engine itself is single-threaded per shard.
thread_local! {
    /// The COUNT FOLD (operator A of `docs/lsqb-completeness-plan.md`): ON by
    /// default. OFF = every hop materialises, the honest differential twin.
    static COUNT_FOLD: Cell<bool> = const { Cell::new(true) };
    /// The per-level MEMO inside the fold: ON by default. OFF = every level is
    /// re-enumerated per parent node — same count, so a differential test proves
    /// the memo is a pure cache.
    static COUNT_FOLD_MEMO: Cell<bool> = const { Cell::new(true) };
    /// The COUNT-ONLY JOIN REORDER (operator C): ON by default. OFF = the
    /// pattern is planned in SOURCE order, the honest differential twin (the
    /// rewrite is unobservable, so ON and OFF must agree row-for-row).
    static COUNT_ONLY_REORDER: Cell<bool> = const { Cell::new(true) };
    /// The PEAK-AWARE ORDERING SEARCH inside that reorder: ON by default. OFF =
    /// the greedy alone, which is the arm that prices the search. Both orderings
    /// answer the same `count(*)`, so ON and OFF must agree row-for-row.
    static ORDER_PEAK_SEARCH: Cell<bool> = const { Cell::new(true) };
}

/// The honest test forcing for the COUNT FOLD: with it off, a `count(*)`-only
/// aggregate materialises every hop and reduces one row per walk instead of
/// multiplying per-row walk counts — same groups, same counts, so a differential
/// proves the fold byte-identical. Pipeline-local (see the thread-local above).
pub fn set_count_fold(enabled: bool) {
    COUNT_FOLD.with(|c| c.set(enabled));
}

/// Whether the COUNT FOLD is on (this thread).
pub fn count_fold_enabled() -> bool {
    COUNT_FOLD.with(Cell::get)
}

/// The honest test forcing for the fold's per-level MEMO: with it off, every
/// folded level is re-enumerated per parent — identical counts, so a
/// differential proves the memo is a pure cache.
pub fn set_count_fold_memo(enabled: bool) {
    COUNT_FOLD_MEMO.with(|c| c.set(enabled));
}

/// Whether the fold's per-level memo is on (this thread).
pub fn count_fold_memo_enabled() -> bool {
    COUNT_FOLD_MEMO.with(Cell::get)
}

/// The honest test forcing for the COUNT-ONLY JOIN REORDER: with it off the
/// pattern is planned exactly as written. The rewrite only ever applies to a
/// one-row `count(*)`-only statement, so ON and OFF must produce the SAME row.
pub fn set_count_only_reorder(enabled: bool) {
    COUNT_ONLY_REORDER.with(|c| c.set(enabled));
}

/// Whether the count-only join reorder is on (this thread).
pub fn count_only_reorder_enabled() -> bool {
    COUNT_ONLY_REORDER.with(Cell::get)
}

/// The honest test forcing for the PEAK-AWARE ORDERING SEARCH: with it off the
/// reorder keeps its greedy, which scores the immediate step and treats any
/// both-ends-bound path as a free close. The two orderings compute the same
/// `count(*)`, so ON and OFF must agree row-for-row — what differs is how many
/// intermediate rows were built to get there.
pub fn set_order_peak_search(enabled: bool) {
    ORDER_PEAK_SEARCH.with(|c| c.set(enabled));
}

/// Whether the peak-aware ordering search is on (this thread).
pub fn order_peak_search_enabled() -> bool {
    ORDER_PEAK_SEARCH.with(Cell::get)
}

/// Load a bound variable's property columns from the record family its KIND
/// selects — `ColumnFamily::Nodes` for a Node var, `ColumnFamily::Rels` for a
/// Rel var — so `r.since` on a relationship variable reads the relationship
/// store, not the node store. `Ok(None)` = a span over the column budget
/// (decline), exactly as the node-only loaders return.
pub(crate) fn load_var_columns(
    graph: &Graph,
    kind: VarKind,
    distinct: &[u64],
    props: &BTreeSet<String>,
) -> Result<Option<BTreeMap<String, Vec<Value>>>, RunError> {
    // The NODE_IDENTITY_KEY is not a store property — strip it, load the real
    // properties, then synthesise the identity column (id-only nodes) aligned to
    // `distinct`. A Rel var declines identity (not synthesised) → the pred falls
    // to the interp.
    let wants_identity = props.contains(NODE_IDENTITY_KEY);
    let loaded = if wants_identity {
        let real: BTreeSet<String> = props
            .iter()
            .filter(|p| p.as_str() != NODE_IDENTITY_KEY)
            .cloned()
            .collect();
        match kind {
            VarKind::Node => load_side_columns(graph, distinct, &real),
            VarKind::Rel => load_rel_columns(graph, distinct, &real),
        }
    } else {
        match kind {
            VarKind::Node => load_side_columns(graph, distinct, props),
            VarKind::Rel => load_rel_columns(graph, distinct, props),
        }
    }?;
    let Some(mut cols) = loaded else {
        return Ok(None);
    };
    if wants_identity {
        match kind {
            VarKind::Node => {
                let id_col: Vec<Value> = distinct
                    .iter()
                    .map(|&id| Value::Node {
                        id,
                        labels: Vec::new(),
                        props: BTreeMap::new(),
                    })
                    .collect();
                cols.insert(NODE_IDENTITY_KEY.to_string(), id_col);
            }
            // A rel var's identity is not synthesised — leave the column absent so
            // `eval_column`'s Var arm returns None and the pred declines.
            VarKind::Rel => {}
        }
    }
    Ok(Some(cols))
}

/// [`load_var_columns`] for a var bound under known pattern LABELS: a NODE
/// var under ONE label reads its properties from the label's column — the
/// property-column cache when it already holds them, else a whole-label walk
/// that is KEPT when the distinct live ids are at least an eighth of the
/// label (`batch::label_value_columns`) — aligned to `distinct`, instead of
/// a record read per distinct id on every statement. Anything else (a rel
/// var, several labels, a small population over a cold label) loads as
/// before. The values are the same decode the record read produces, so the
/// column is byte-identical; the identity column is synthesised as
/// [`load_var_columns`] does.
pub(crate) fn load_var_columns_labelled(
    graph: &Graph,
    kind: VarKind,
    distinct: &[u64],
    props: &BTreeSet<String>,
    labels: Option<&[String]>,
    params: &BTreeMap<String, Value>,
) -> Result<Option<BTreeMap<String, Vec<Value>>>, RunError> {
    // A var under several labels (`lore:Repo:ManagedRepo`) is a member of
    // every one of them, so its columns read through the SMALLEST — the
    // cheapest whole-label walk that still holds every live id.
    let label = labels
        .filter(|l| !l.is_empty())
        .and_then(|l| l.iter().min_by_key(|x| graph.count_label_nodes(x)))
        .map(String::as_str);
    if let (VarKind::Node, Some(label)) = (kind, label) {
        if graph.columnar_scans_enabled() && !distinct.is_empty() {
            let real: Vec<String> = props
                .iter()
                .filter(|p| p.as_str() != NODE_IDENTITY_KEY)
                .cloned()
                .collect();
            let cached = !real.is_empty()
                && real
                    .iter()
                    .all(|p| graph.prop_column(label, p, false).is_some());
            let label_n = graph.count_label_nodes(label);
            if !real.is_empty() && (cached || whole_label_worth(distinct.len(), label_n)) {
                if let Some(cols) = crate::batch::label_value_columns(graph, label, &real, params)? {
                    counted!("interp.pipeline bound-var columns read from the label column");
                    let mut out: BTreeMap<String, Vec<Value>> = BTreeMap::new();
                    for (j, p) in real.iter().enumerate() {
                        out.insert(p.clone(), crate::vectorized::align(distinct, &cols[j]));
                    }
                    if props.contains(NODE_IDENTITY_KEY) {
                        let id_col: Vec<Value> = distinct
                            .iter()
                            .map(|&id| Value::Node {
                                id,
                                labels: Vec::new(),
                                props: BTreeMap::new(),
                            })
                            .collect();
                        out.insert(NODE_IDENTITY_KEY.to_string(), id_col);
                    }
                    return Ok(Some(out));
                }
            }
        }
    }
    load_var_columns(graph, kind, distinct, props)
}

/// Whether a column over a bound var's `distinct_n` live ids is worth
/// reading over its WHOLE label (`label_n` members) so the property-column
/// cache keeps it: a small label always (one gather of at most a few
/// thousand records, then none), a larger one when the population is at
/// least an eighth of it. Otherwise the population gathers its own ids as
/// before — a 25-row page over a 44k-member label must not read the label.
fn whole_label_worth(distinct_n: usize, label_n: u64) -> bool {
    label_n <= WHOLE_LABEL_ALWAYS || (distinct_n as u64).saturating_mul(8) >= label_n
}

/// The label size up to which a bound var's column is always read whole
/// (and kept) rather than gathered per statement.
const WHOLE_LABEL_ALWAYS: u64 = 4096;

/// The pattern labels an aggregate plan binds each var under, by var index:
/// the seed's, or a new-var hop's end labels; `None` for a close hop's
/// target, a rel var, or an unlabelled pattern node. What the reduce and the
/// group-key gather read their columns through (fix 33).
fn agg_var_labels(plan: &AggPlan) -> Vec<Option<Vec<String>>> {
    plan.vars
        .iter()
        .map(|v| {
            if *v == plan.a_var {
                return (!plan.a_labels.is_empty()).then(|| plan.a_labels.clone());
            }
            plan.hops
                .iter()
                .find(|h| h.tgt.is_none() && h.var == *v && !h.labels.is_empty())
                .map(|h| h.labels.clone())
        })
        .collect()
}

/// The pattern labels a core plan binds `var` under: the seed's, or a
/// new-var hop's end labels; `None` for a close hop's target, a rel var, or
/// an unlabelled pattern node.
fn core_var_labels<'a>(plan: &'a CorePlan, var: &str) -> Option<&'a [String]> {
    if plan.a_var == var {
        return (!plan.a_labels.is_empty()).then_some(plan.a_labels.as_slice());
    }
    plan.hops
        .iter()
        .find(|h| h.tgt.is_none() && h.var == var && !h.labels.is_empty())
        .map(|h| h.labels.as_slice())
}

// ─── DataChunk ───────────────────────────────────────────────────────────────

/// A single materialized batch of bindings: one id column per bound variable
/// (all equal length = row count) plus the live-row selection that filters
/// shrink. One chunk materializes the whole result — no multi-chunk batching.
/// Property columns are loaded per-DISTINCT at the filter/project operators
/// (`load_side_columns`), never carried per row. `used_rels[r]` records the
/// relationship ids traversed to reach row `r` WITHIN THE CURRENT PATH — the
/// per-row `used` set that enforces relationship isomorphism across a MULTI-hop
/// expand chain. It stays empty on a single hop (reuse is impossible), and it is
/// RESET at every path boundary of a multi-path pattern: `run_streaming`
/// re-seeds `Partial.used` per path (each path's `handle_start` starts fresh),
/// so a relationship used by an earlier path never forbids a later path's walk.
pub(crate) struct DataChunk {
    /// Bound variables, in binding order — NODE variables and, where a hop binds
    /// a relationship variable `(a)-[r:T]->(b)`, its RELATIONSHIP variable too.
    vars: Vec<String>,
    /// Per-var KIND, parallel to `vars`: a Node var's id column holds node ids
    /// (materialised via `node_of`, properties from `ColumnFamily::Nodes`); a Rel
    /// var's id column holds relationship ids (materialised via `rel_of`,
    /// properties from `ColumnFamily::Rels`). Every site that turns a var id into
    /// a `Value` or loads its property column routes on this.
    var_kinds: Vec<VarKind>,
    /// One id column per var; `ids[v][r]` is var `v`'s node/rel id in row `r`.
    /// All columns share the row count.
    ids: Vec<Vec<u64>>,
    /// Live row indices after filters. Starts `0..row_count`; a filter shrinks
    /// it; id columns are never copied.
    selection: Vec<usize>,
    /// Per-row relationship ids used so far (aligned to the row index, NOT the
    /// selection). Empty when isomorphism tracking is off (single hop).
    used_rels: Vec<Vec<u64>>,
    /// OPTIONAL left-join OUTER-ROW PROVENANCE (aligned to the row index, NOT the
    /// selection): `prov[r]` is the index — into the outer chunk's LIVE rows, in
    /// order — of the outer row this row descends from. `expand`/`semijoin` copy
    /// it forward in lockstep with the id columns, so after running the optional
    /// steps over the WHOLE outer chunk in one pass, every surviving row still
    /// knows its outer row and the merge can interleave null-fills in `exec_match`
    /// order. EMPTY on every non-OPTIONAL chunk — the seed leaves it empty and
    /// `expand`/`semijoin` carry it only when it is non-empty, so the census /
    /// core-chain callers are byte-identical and pay only an `is_empty` check.
    prov: Vec<usize>,
    /// COUNT-FOLD WEIGHTS (aligned to the row index, NOT the selection): how many
    /// walks of the FOLDED subtrees each row stands for — the multiplicity a
    /// `count(*)` site adds instead of 1 (`fold_row_weighted`). EMPTY = every row
    /// weighs 1, which is every chunk that never met a folded hop, so the
    /// existing operators pay only an `is_empty` check. Non-empty only after
    /// `fold_tail` ran; `expand`/`semijoin` then copy a row's weight to each
    /// row it produces (a materialised hop multiplies rows, the fold multiplies
    /// weights — the product is the same count).
    weights: Vec<u64>,
}

impl DataChunk {
    /// SCAN/SEED: the initial one-var chunk from a label scan's ascending ids.
    fn seed(var: &str, ids: Vec<u64>) -> Self {
        let n = ids.len();
        DataChunk {
            vars: vec![var.to_string()],
            // The scan start is always a NODE (a label scan seeds node ids).
            var_kinds: vec![VarKind::Node],
            ids: vec![ids],
            // Empty per-row `used` sets — no relationship traversed yet. Empty
            // `Vec`s carry no heap allocation, so seeding a large label is cheap
            // whether or not the chain ends up tracking isomorphism.
            used_rels: vec![Vec::new(); n],
            // No provenance by default — only the OPTIONAL runner seeds it.
            prov: Vec::new(),
            selection: (0..n).collect(),
            // Every row weighs 1 until a fold runs.
            weights: Vec::new(),
        }
    }

    /// A FOLDED hop's placeholder: append `var` as a column of `NULL_ID` so the
    /// binding-order layout (`hop.src` / `KeyVal::Node(vi)` / `row_ids`) stays
    /// intact while the var itself is never materialised. The fold guarantees
    /// nothing reads the column (a folded var is never a key, a site argument,
    /// a WHERE var or an ORDER BY key — `plan_count_fold`), exactly as the degree
    /// short-circuit's `vec![NULL_ID; vars.len()]` group templates rely on.
    /// `kind` is the kind the MATERIALISED operator would have recorded: an
    /// expand's end var is a Node, a hop's bound relationship variable is a Rel.
    /// One placeholder per column the materialised operator appends — a folded
    /// hop that appended fewer would shift every later var's index.
    fn null_extend(&mut self, var: &str, kind: VarKind) {
        let n = self.row_count();
        self.ids.push(vec![NULL_ID; n]);
        self.vars.push(var.to_string());
        self.var_kinds.push(kind);
    }

    /// A chunk from explicit per-var id columns (all the same length) — used to
    /// hand a small set of already-chosen rows (the index-ordered top-k winners)
    /// to the SHARED projection tail. No relationships traversed (empty `used`),
    /// no provenance; every row live, in the given order.
    fn from_columns(vars: Vec<String>, var_kinds: Vec<VarKind>, ids: Vec<Vec<u64>>) -> Self {
        let n = ids.first().map_or(0, Vec::len);
        DataChunk {
            vars,
            var_kinds,
            used_rels: vec![Vec::new(); n],
            prov: Vec::new(),
            selection: (0..n).collect(),
            ids,
            weights: Vec::new(),
        }
    }

    /// A standalone DRIVING sub-chunk over live rows `[start, end)` — the same
    /// vars/kinds, its own compacted id columns, and an EMPTY `used_rels`/`prov`
    /// (a fresh stage-2 seed, exactly what `project_carried` yields, just narrowed
    /// to a batch). Only valid on a chunk with no OPTIONAL provenance (`prov`
    /// empty) — the caller checks. Preserves live-row ORDER, so expanding batches
    /// in sequence reproduces the single-pass production order.
    fn driving_slice(&self, start: usize, end: usize) -> DataChunk {
        let rows = &self.selection[start..end];
        let ids: Vec<Vec<u64>> = self
            .ids
            .iter()
            .map(|col| rows.iter().map(|&r| col[r]).collect())
            .collect();
        let n = rows.len();
        // A fold weight is a per-row fact, so the slice keeps its rows' weights
        // (compacted like the id columns); an unweighted chunk stays unweighted.
        let weights: Vec<u64> = if self.weights.is_empty() {
            Vec::new()
        } else {
            rows.iter().map(|&r| self.weights[r]).collect()
        };
        DataChunk {
            vars: self.vars.clone(),
            var_kinds: self.var_kinds.clone(),
            ids,
            selection: (0..n).collect(),
            used_rels: vec![Vec::new(); n],
            prov: Vec::new(),
            weights,
        }
    }

    /// The number of rows (columns' shared length), live or not.
    fn row_count(&self) -> usize {
        self.ids.first().map_or(0, Vec::len)
    }

    /// The number of live rows.
    fn live(&self) -> usize {
        self.selection.len()
    }

    /// The position of `var` in the binding order.
    fn var_index(&self, var: &str) -> usize {
        self.vars
            .iter()
            .position(|v| v == var)
            .expect("var is bound in this chunk")
    }

    /// EXPAND: append `new_var` by expanding the bound variable at `src_vi` over
    /// a fixed directed hop. For a SINGLE-path chain `src_vi` is the last bound
    /// var; for a MULTI-path pattern it is ANY already-bound var (a chain
    /// continued from `b`, or a branch re-rooted at `a`). For each LIVE source
    /// row, its neighbours are emitted IN REVERSE `adjacent_slim` order (the LIFO
    /// order the per-tuple path pops) as new rows that copy the source row's ids
    /// and append the neighbour id. The end-label filter (`b_members`) drops a
    /// non-member neighbour. When `track_rels` is set (this hop's PATH has more
    /// than one hop), a neighbour reached over a relationship already used in this
    /// row's CURRENT-PATH walk is dropped (relationship isomorphism), and the
    /// traversed relationship is recorded for the next hop. `reset_rels` marks the
    /// FIRST hop of a later path (k>=2): the isomorphism base is empty regardless
    /// of what earlier paths recorded, mirroring `run_streaming`'s per-path
    /// `Partial.used` re-seed. Row order = source-row order x reverse-adjacency
    /// (the source-row order already encodes every earlier path's nesting, so
    /// appending the new var innermost reproduces `run_streaming`'s exact nested
    /// production order). Consumes `self` (the row count changes) and returns the
    /// widened chunk. This is the ONLY per-edge work.
    #[allow(clippy::too_many_arguments)]
    fn expand(
        self,
        graph: &Graph,
        src_vi: usize,
        new_var: &str,
        rel_var: Option<&str>,
        dir: Dir,
        tokens: &Option<Vec<u32>>,
        b_members: Option<&crate::MembersView>,
        track_rels: bool,
        reset_rels: bool,
    ) -> Result<DataChunk, RunError> {
        // Morsel-parallel path (opt-in lever, default OFF). Split the driving
        // rows into morsels, expand each independently through the graph's
        // installed `ScopedExec`, and concatenate the partials IN ORDER —
        // byte-identical to the serial loop below, which the A/B differential
        // proves. The gates, each load-bearing:
        //   - the lever (off by default — the digest and benchmarks run serial);
        //   - an INSTALLED executor (the engine never spawns; absent → serial,
        //     which is also the sim lane's path);
        //   - NO ACTIVE TRANSACTION on this thread, not merely no buffered
        //     writes: the read-your-writes overlays and the OCC read-set are
        //     thread-local, so a worker thread would silently read committed
        //     state and record nothing;
        //   - enough driving rows for the split to beat its own overhead;
        //   - NO fold weights on the driving rows: the morsel body does not
        //     carry the weight column (a weighted chunk only ever arises in the
        //     count-only aggregate, which is small by then), so it stays serial.
        if graph.parallel_expand_enabled()
            && self.selection.len() >= graph.parallel_min_rows()
            && !graph.in_txn()
            && self.weights.is_empty()
        {
            if let Some(exec) = graph.exec() {
                return self.expand_parallel(
                    graph, &*exec, src_vi, new_var, rel_var, dir, tokens, b_members, track_rels,
                    reset_rels,
                );
            }
        }
        let mut out_ids: Vec<Vec<u64>> = (0..self.ids.len()).map(|_| Vec::new()).collect();
        let mut new_col: Vec<u64> = Vec::new();
        // When the hop binds a relationship variable, its id (`e.rel`) is
        // appended as an ADDITIONAL column, in lockstep with the peer node
        // column. Binding order is `new_var` (the node) THEN `rel_var` — the
        // recognizer continues the next hop from the NODE's index, not this one.
        let mut rel_col: Option<Vec<u64>> = rel_var.map(|_| Vec::new());
        let mut out_used: Vec<Vec<u64>> = Vec::new();
        // Carry OPTIONAL provenance only when present (the OPTIONAL runner seeds
        // it); the census / core-chain callers leave `prov` empty and pay only
        // this flag test, keeping their output byte-identical.
        let carry_prov = !self.prov.is_empty();
        let mut out_prov: Vec<usize> = Vec::new();
        // Carry fold weights only when present (see `weights`): each produced
        // row inherits its source row's weight.
        let carry_w = !self.weights.is_empty();
        let mut out_w: Vec<u64> = Vec::new();
        // Label-filter probes, accumulated in a register and flushed ONCE — a
        // relaxed atomic per edge would cost more than the probe it counts.
        let mut probes = 0u64;
        // The isomorphism base for a row's walk: empty at a path boundary
        // (`reset_rels`) or when this hop's path is single-hop; else the rels
        // this row's CURRENT path has already traversed.
        let base_of = |r: usize| -> &[u64] {
            if track_rels && !reset_rels {
                self.used_rels.get(r).map_or(&[][..], Vec::as_slice)
            } else {
                &[]
            }
        };
        // One produced edge of row `r`; answers the rows produced so far (the
        // budget check reads it between edges).
        let mut take = |r: usize, base: &[u64], e: &crate::SlimAdj| -> usize {
            let peer = e.peer;
            if let Some(m) = b_members {
                probes += 1;
                if !graph.members_contains(m, peer) {
                    return new_col.len();
                }
            }
            if track_rels && base.contains(&e.rel) {
                return new_col.len(); // relationship isomorphism — this walk already used it
            }
            for (vi, col) in out_ids.iter_mut().enumerate() {
                col.push(self.ids[vi][r]);
            }
            new_col.push(peer);
            if let Some(rc) = rel_col.as_mut() {
                rc.push(e.rel);
            }
            if track_rels {
                let mut used = base.to_vec();
                used.push(e.rel);
                out_used.push(used);
            }
            if carry_prov {
                out_prov.push(self.prov[r]);
            }
            if carry_w {
                out_w.push(self.weights[r]);
            }
            new_col.len()
        };
        // Fix 39: a DIRECTED hop borrows its adjacency table ONCE for every
        // driving row (`Graph::with_hop_table`) and walks each row's CSR
        // slice in place, back-to-front — the per-row accessor's visit order
        // exactly, without its per-visit epoch / gate / overlay / memo
        // bookkeeping (~400 ns a row: the REPLIED_TO count from a 38k-email
        // seed spent 14–18 ms on it for 0 edges, Neo4j 1.6–1.9). An
        // undirected hop (two sides, a self-loop deduped between them), an
        // id past the table range, a writing transaction or a declined
        // table keep the per-row accessor, byte-identical.
        let hop_tag: Option<u8> = match dir {
            Dir::Out => Some(b'O'),
            Dir::In => Some(b'I'),
            Dir::Both => None,
        };
        let mut borrowed = false;
        if let Some(tag) = hop_tag {
            let in_range = self
                .selection
                .iter()
                .all(|&r| self.ids[src_vi][r] <= crate::DEGREE_TABLE_MAX_ID);
            if in_range {
                borrowed = graph.with_hop_table(tag, tokens, self.selection.len(), |tbl| {
                    let Some(t) = tbl else {
                        return Ok::<bool, RunError>(false);
                    };
                    counted!("interp.pipeline hop borrowed its adjacency table once");
                    let mut produced = 0usize;
                    for &r in &self.selection {
                        let src = self.ids[src_vi][r];
                        let base = base_of(r);
                        for e in t.slice(src).iter().rev() {
                            produced = take(r, base, e);
                        }
                        budget_check(graph, produced)?;
                    }
                    Ok(true)
                })?;
            }
        }
        if !borrowed {
            for &r in &self.selection {
                let src = self.ids[src_vi][r];
                let base = base_of(r);
                // Zero-copy reverse adjacency: this hop pops neighbours LIFO, so the
                // accessor feeds them straight from the cached CSR slice iterated
                // back-to-front — byte-identical to `adjacent_slim(..).iter().rev()`
                // but WITHOUT the per-node Vec alloc+copy (profiled ~3.8 ns/edge).
                let mut produced = 0usize;
                graph.adjacent_slim_rev_for_each(src, dir, tokens, |e| {
                    produced = take(r, base, e);
                });
                budget_check(graph, produced)?;
            }
        }
        crate::counters::MEMBERS_PROBES.fetch_add(probes, std::sync::atomic::Ordering::Relaxed);
        out_ids.push(new_col);
        let mut vars = self.vars;
        let mut var_kinds = self.var_kinds;
        vars.push(new_var.to_string());
        var_kinds.push(VarKind::Node);
        if let (Some(rname), Some(rc)) = (rel_var, rel_col) {
            out_ids.push(rc);
            vars.push(rname.to_string());
            var_kinds.push(VarKind::Rel);
        }
        let n = out_ids[0].len();
        Ok(DataChunk {
            vars,
            var_kinds,
            ids: out_ids,
            selection: (0..n).collect(),
            used_rels: out_used,
            prov: out_prov,
            weights: out_w,
        })
    }

    /// The morsel-parallel body of [`DataChunk::expand`]. Each worker expands a
    /// contiguous slice of the driving rows via [`expand_row_slice`] — pure over
    /// the graph's thread-safe read path — and the partials are concatenated in
    /// slice order, so the output is byte-identical to the serial expansion. The
    /// row budget is checked once on the combined size (identical pass/fail: both
    /// paths fail iff the TOTAL output exceeds the budget).
    #[allow(clippy::too_many_arguments)]
    fn expand_parallel(
        self,
        graph: &Graph,
        exec: &dyn crate::scoped_exec::ScopedExec,
        src_vi: usize,
        new_var: &str,
        rel_var: Option<&str>,
        dir: Dir,
        tokens: &Option<Vec<u32>>,
        b_members: Option<&crate::MembersView>,
        track_rels: bool,
        reset_rels: bool,
    ) -> Result<DataChunk, RunError> {
        let DataChunk {
            mut vars,
            mut var_kinds,
            ids,
            selection,
            used_rels,
            prov,
            weights,
        } = self;
        // The gate above admits only an UNWEIGHTED chunk to this path.
        debug_assert!(weights.is_empty(), "the parallel expand never carries fold weights");
        let carry_prov = !prov.is_empty();
        let want_rel_col = rel_var.is_some();
        let ncols = ids.len();
        counted!("interp.expand parallel");
        // Warm the adjacency table(s) once, serially, so the workers read them
        // lock-free rather than each rebuilding on a concurrent miss.
        graph.warm_adjacency(dir, tokens);
        let workers = exec.width().min(selection.len()).max(1);
        let per = selection.len().div_ceil(workers);
        let morsels: Vec<&[usize]> = selection.chunks(per).collect();
        // Shared, read-only inputs the workers borrow; one SLOT per morsel
        // for the partials, so no return-value plumbing crosses the seam and
        // the merge below reads them back IN MORSEL ORDER.
        let ids_ref: &[Vec<u64>] = &ids;
        let used_ref: &[Vec<u64>] = &used_rels;
        let prov_ref: &[usize] = &prov;
        let slots: Vec<std::sync::Mutex<Option<ExpandCols>>> =
            morsels.iter().map(|_| std::sync::Mutex::new(None)).collect();
        // The workers' shared row-budget account (see `expand_row_slice`): the
        // serial loop refuses while producing; the workers must too, or the
        // refusal arrives only after the memory it exists to prevent is spent.
        let budget = graph.row_budget().unwrap_or(usize::MAX);
        let produced = std::sync::atomic::AtomicUsize::new(0);
        let over = std::sync::atomic::AtomicBool::new(false);
        exec.for_each(morsels.len(), &|i| {
            let part = expand_row_slice(
                graph,
                ids_ref,
                used_ref,
                prov_ref,
                morsels[i],
                src_vi,
                dir,
                tokens,
                b_members,
                track_rels,
                reset_rels,
                carry_prov,
                want_rel_col,
                &produced,
                &over,
                budget,
            );
            *slots[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(part);
        });
        if over.load(std::sync::atomic::Ordering::Relaxed) {
            // The combined output passed the budget while still being produced.
            // Refuse with the SAME error the serial path raises, without ever
            // merging the partials.
            counted!("interp.expand parallel over budget");
            budget_check(graph, produced.load(std::sync::atomic::Ordering::Relaxed))?;
        }
        let parts: Vec<ExpandCols> = slots
            .into_iter()
            .map(|m| {
                m.into_inner()
                    .unwrap_or_else(|e| e.into_inner())
                    .expect("every morsel ran — ScopedExec::for_each returns only when all have")
            })
            .collect();
        // Concatenate IN ORDER — this is what makes the result byte-identical.
        let mut out_ids: Vec<Vec<u64>> = (0..ncols).map(|_| Vec::new()).collect();
        let mut new_col: Vec<u64> = Vec::new();
        let mut rel_col: Option<Vec<u64>> = if want_rel_col { Some(Vec::new()) } else { None };
        let mut out_used: Vec<Vec<u64>> = Vec::new();
        let mut out_prov: Vec<usize> = Vec::new();
        for (p_ids, p_new, p_rel, p_used, p_prov) in parts {
            for (vi, col) in p_ids.into_iter().enumerate() {
                out_ids[vi].extend(col);
            }
            new_col.extend(p_new);
            if let (Some(rc), Some(pr)) = (rel_col.as_mut(), p_rel) {
                rc.extend(pr);
            }
            out_used.extend(p_used);
            out_prov.extend(p_prov);
        }
        budget_check(graph, new_col.len())?;
        out_ids.push(new_col);
        vars.push(new_var.to_string());
        var_kinds.push(VarKind::Node);
        if let (Some(rname), Some(rc)) = (rel_var, rel_col) {
            out_ids.push(rc);
            vars.push(rname.to_string());
            var_kinds.push(VarKind::Rel);
        }
        let n = out_ids[0].len();
        Ok(DataChunk {
            vars,
            var_kinds,
            ids: out_ids,
            selection: (0..n).collect(),
            used_rels: out_used,
            prov: out_prov,
            weights: Vec::new(),
        })
    }

    /// SEMIJOIN: the CYCLE / connecting-path step — a hop whose END var is
    /// ALREADY BOUND (at `tgt_vi`), so it appends NO column. For each LIVE
    /// source row, `src_vi`'s neighbours are walked in the SAME REVERSE
    /// `adjacent_slim` order `expand` uses, and an edge is KEPT iff its peer
    /// equals THIS row's bound target id (`self.ids[tgt_vi][r]`) — closing the
    /// path onto the bound var — AND, when `track_rels` is set, its relationship
    /// is not already used in this row's CURRENT-PATH walk (relationship
    /// isomorphism). One output row is EMITTED PER MATCHING EDGE, copying every
    /// existing id column UNCHANGED (the target column included): several edges
    /// connecting the pair MULTIPLY the row exactly as `run_streaming`'s
    /// `expand_var_length` does (each edge is a distinct stack entry that
    /// completes at depth 1 under the `target_ok` check). A source row with NO
    /// closing edge is DROPPED — the non-OPTIONAL semantics (OPTIONAL is 4b2).
    /// `reset_rels` re-seeds the isomorphism base empty at a later path's first
    /// hop, mirroring `run_streaming`'s per-path `Partial.used`. Row order =
    /// source-row order × reverse-adjacency, byte-identical to the nested
    /// production order; the semijoin is a path's FINAL hop, so it never
    /// advances the binding order. Consumes `self` (the row count changes).
    /// (Because the closing edges of one source row all carry the SAME copied
    /// ids and no rel column, they are byte-identical rows — the reverse is
    /// retained for structural fidelity with `expand`, but only the row COUNT
    /// it produces is observable downstream.)
    #[allow(clippy::too_many_arguments)]
    fn semijoin(
        self,
        graph: &Graph,
        src_vi: usize,
        tgt_vi: usize,
        rel_var: Option<&str>,
        dir: Dir,
        tokens: &Option<Vec<u32>>,
        track_rels: bool,
        reset_rels: bool,
    ) -> Result<DataChunk, RunError> {
        let ncols = self.ids.len();
        let mut out_ids: Vec<Vec<u64>> = (0..ncols).map(|_| Vec::new()).collect();
        // A closing hop appends no NODE column (the target is already bound), but
        // it MAY bind a relationship variable — then its id (`e.rel`) is the one
        // new column, and the several closing edges of a pair (retained in
        // reverse order) become distinct observable rows rather than duplicates.
        let mut rel_col: Option<Vec<u64>> = rel_var.map(|_| Vec::new());
        let mut out_used: Vec<Vec<u64>> = Vec::new();
        // Carry OPTIONAL provenance only when present (see `expand`).
        let carry_prov = !self.prov.is_empty();
        let mut out_prov: Vec<usize> = Vec::new();
        // Carry fold weights only when present (see `weights`).
        let carry_w = !self.weights.is_empty();
        let mut out_w: Vec<u64> = Vec::new();
        // Whether any source row took the COUNTED close (recorded once per call).
        let mut counted_close = false;
        for &r in &self.selection {
            let src = self.ids[src_vi][r];
            let want = self.ids[tgt_vi][r];
            // The isomorphism base for this row's walk: empty at a path boundary
            // (`reset_rels`) or when this hop's path is single-hop; else the rels
            // this row's CURRENT path has already traversed.
            let base: &[u64] = if track_rels && !reset_rels {
                self.used_rels.get(r).map_or(&[][..], Vec::as_slice)
            } else {
                &[]
            };
            // THE COUNTED CLOSE. The closing edges of one source row all copy the
            // SAME ids, so without a rel column only their NUMBER is observable —
            // and that number is the closing-edge MULTIPLICITY, which
            // `Graph::edge_count_slim` answers from the sorted CSR row by two
            // `partition_point`s on a single-type table (a walk otherwise; the
            // number is identical either way, only the `graph.edge probe …`
            // counter says which). It applies when no relationship is excluded:
            // no rel variable to bind, and either no isomorphism tracking or an
            // EMPTY base (the first hop of a path, or a single-hop close). A
            // non-empty base must still walk to skip the used rels. The per-row
            // `used` recorded here omits the closing rel: a semijoin is its
            // path's FINAL hop, so the next hop (a later path's first) re-seeds
            // `used` empty and never reads it (`reset`), as does every
            // stage/OPTIONAL boundary — the walk's exact `used` is dead data.
            if rel_col.is_none() && (!track_rels || base.is_empty()) {
                let n = graph.edge_count_slim(src, dir, tokens, want);
                counted_close = true;
                for _ in 0..n {
                    for (vi, col) in out_ids.iter_mut().enumerate() {
                        col.push(self.ids[vi][r]);
                    }
                    if track_rels {
                        out_used.push(base.to_vec());
                    }
                    if carry_prov {
                        out_prov.push(self.prov[r]);
                    }
                    if carry_w {
                        out_w.push(self.weights[r]);
                    }
                }
                budget_check(graph, out_ids[0].len())?;
                continue;
            }
            // Zero-copy reverse adjacency, as `expand`: the closing hop walks the
            // same reversed order (retained for structural fidelity) straight from
            // the cached CSR slice, no per-node Vec.
            graph.adjacent_slim_rev_for_each(src, dir, tokens, |e| {
                if e.peer != want {
                    return; // the hop must CLOSE onto the bound target
                }
                if track_rels && base.contains(&e.rel) {
                    return; // relationship isomorphism — this walk already used it
                }
                for (vi, col) in out_ids.iter_mut().enumerate() {
                    col.push(self.ids[vi][r]);
                }
                if let Some(rc) = rel_col.as_mut() {
                    rc.push(e.rel);
                }
                if track_rels {
                    let mut used = base.to_vec();
                    used.push(e.rel);
                    out_used.push(used);
                }
                if carry_prov {
                    out_prov.push(self.prov[r]);
                }
                if carry_w {
                    out_w.push(self.weights[r]);
                }
            });
            budget_check(graph, out_ids[0].len())?;
        }
        if counted_close {
            counted!("interp.pipeline semijoin counted close");
        }
        let mut vars = self.vars;
        let mut var_kinds = self.var_kinds;
        if let (Some(rname), Some(rc)) = (rel_var, rel_col) {
            out_ids.push(rc);
            vars.push(rname.to_string());
            var_kinds.push(VarKind::Rel);
        }
        let n = out_ids[0].len();
        Ok(DataChunk {
            vars,
            var_kinds,
            ids: out_ids,
            selection: (0..n).collect(),
            used_rels: out_used,
            prov: out_prov,
            weights: out_w,
        })
    }

    /// FRONTIER-BFS VARIABLE-LENGTH EXPAND: append `new_var` as the set of nodes
    /// reachable from the bound var at `src_vi` over `1..=max` `dir` hops of the
    /// hop's types — each reachable node produced ONCE, at its shortest depth.
    /// This is the set-at-a-time counterpart of `interp::expand_var_length_bfs`
    /// and reproduces it PRIMITIVE-FOR-PRIMITIVE, per LIVE source row:
    ///   - `seen` starts EMPTY (the start is NOT pre-seeded); `frontier` starts as
    ///     `[src]`. A node ENTERS `seen` the first time it is reached, fixing its
    ///     shortest depth and single emission. The start is therefore EMITTED if
    ///     it is genuinely re-reached — the downstream `WHERE a <> b` (a two-var
    ///     id filter) removes it, never this operator.
    ///   - `depth` runs `1..=max`, level by level. Per level, for each `u` in the
    ///     current frontier (in frontier order) the FORWARD `adjacent_slim` order
    ///     is walked (NOT the reversed order the fixed-hop `expand` uses); for a
    ///     peer `v`, `seen.insert(v)` gates dedup, `depth < max` gates pushing `v`
    ///     to the next frontier (the traversal bound), and the end-label filter
    ///     (`b_members`) gates EMISSION only — a non-member is still seen and still
    ///     traversed, exactly as the oracle's `node_satisfies` runs AFTER the seen
    ///     / next-push. Emission order = source-row order × frontier order ×
    ///     forward adjacency, each node once at its shortest depth.
    ///   - Each emitted row copies the source row's ids and appends `v`; it
    ///     carries an EMPTY `used` set (node-dedup via `seen` replaces relationship
    ///     isomorphism — there is no rel or path variable). OPTIONAL provenance is
    ///     carried forward only when present, as `expand` does.
    ///
    /// The caller certifies frontier eligibility (`min == 1`, bounded `max`, no
    /// rel/path var, no rel-property test, a NEW end var, the end consumed
    /// DISTINCT-only) before choosing this path.
    #[allow(clippy::too_many_arguments)]
    fn expand_var_length_bfs(
        self,
        graph: &Graph,
        src_vi: usize,
        new_var: &str,
        dir: Dir,
        tokens: &Option<Vec<u32>>,
        b_members: Option<&crate::MembersView>,
        max: u64,
    ) -> Result<DataChunk, RunError> {
        counted!("interp.pipeline var-length BFS ran");
        let mut out_ids: Vec<Vec<u64>> = (0..self.ids.len()).map(|_| Vec::new()).collect();
        let mut new_col: Vec<u64> = Vec::new();
        // Carry OPTIONAL provenance only when present (see `expand`).
        let carry_prov = !self.prov.is_empty();
        let mut out_prov: Vec<usize> = Vec::new();
        // Carry fold weights only when present (see `weights`).
        let carry_w = !self.weights.is_empty();
        let mut out_w: Vec<u64> = Vec::new();
        for &r in &self.selection {
            let src = self.ids[src_vi][r];
            // `seen` is the visited set — a node enters it the first time it is
            // reached, which fixes both its shortest depth and its single emission.
            let mut seen: BTreeSet<u64> = BTreeSet::new();
            let mut frontier: Vec<u64> = vec![src];
            let mut depth = 0u64;
            while depth < max && !frontier.is_empty() {
                depth += 1;
                let mut next: Vec<u64> = Vec::new();
                for &u in &frontier {
                    budget_check(graph, new_col.len() + next.len())?;
                    // FORWARD adjacency order (canary: reversing it diverges an
                    // order-sensitive DISTINCT reach set), fed zero-copy from the
                    // cached CSR slice — no per-node Vec.
                    graph.adjacent_slim_for_each(u, dir, tokens, |e| {
                        let v = e.peer;
                        if !seen.insert(v) {
                            return; // already reached at its shortest depth
                        }
                        if depth < max {
                            next.push(v); // room for a further hop
                        }
                        // The end-label filter gates EMISSION only — a non-member
                        // is still seen and still (above) queued for a next hop.
                        if let Some(m) = b_members {
                            if !m.contains(v) {
                                return;
                            }
                        }
                        for (vi, col) in out_ids.iter_mut().enumerate() {
                            col.push(self.ids[vi][r]);
                        }
                        new_col.push(v);
                        if carry_prov {
                            out_prov.push(self.prov[r]);
                        }
                        if carry_w {
                            out_w.push(self.weights[r]);
                        }
                    });
                }
                frontier = next;
            }
        }
        out_ids.push(new_col);
        let mut vars = self.vars;
        let mut var_kinds = self.var_kinds;
        vars.push(new_var.to_string());
        var_kinds.push(VarKind::Node);
        let n = out_ids[0].len();
        Ok(DataChunk {
            vars,
            var_kinds,
            ids: out_ids,
            selection: (0..n).collect(),
            // A frontier BFS records no traversed relationships (`used: Vec::new()`
            // in the oracle) — the empty vec is the "no rel-iso tracking" marker,
            // as `expand`'s untracked case leaves it.
            used_rels: Vec::new(),
            prov: out_prov,
            weights: out_w,
        })
    }

    /// FILTER: keep live rows whose WHERE predicate over `pred.var`'s property
    /// columns is True — `eval_column` column-at-a-time, exactly `run_streaming`'s
    /// `.truth()` semantics (False/Unknown drop). A non-boolean result column, or
    /// a form `eval_column` cannot vectorise, DECLINES the whole operator
    /// (`Ok(None)`); the general path raises the identical error there.
    fn filter(
        &mut self,
        graph: &Graph,
        params: &BTreeMap<String, Value>,
        pred: &WherePred,
    ) -> Result<Option<()>, RunError> {
        self.filter_labelled(graph, params, pred, None)
    }

    /// [`DataChunk::filter`] with the predicate var's pattern LABELS known
    /// (the seed's, or the hop's end labels): a single-node-var predicate is
    /// then answered by the strict column filter over the property-column
    /// cache — a walk over the WHOLE label that is kept for the next
    /// statement when the live population is a fair share of it, the
    /// population alone otherwise — instead of one record read per distinct
    /// live id on every statement. Every live id of a labelled var is a
    /// member of its labels (the expand dropped the rest), so the pass set
    /// restricts exactly as the per-id evaluation did; a decline falls to it.
    fn filter_labelled(
        &mut self,
        graph: &Graph,
        params: &BTreeMap<String, Value>,
        pred: &WherePred,
        labels: Option<&[String]>,
    ) -> Result<Option<()>, RunError> {
        // BOUND-ENDPOINT EDGE PREDICATE (operator B): `[NOT] (a)-[:T]->(b)` over
        // two bound node vars is an adjacency membership test — exactly what
        // `exists_probe_fast` answers per row on the general path — so it is
        // one `edge_count_slim` (a binary search on a single-type sorted row)
        // per live row, no property load. A non-node endpoint DECLINES the
        // operator (the probe itself hands a non-`Value::Node` binding back to
        // the general matcher, so its semantics are not pinned here).
        if let Some(ep) = &pred.edge {
            let va = self.var_index(&ep.src);
            let vb = self.var_index(&ep.dst);
            if !matches!(self.var_kinds[va], VarKind::Node)
                || !matches!(self.var_kinds[vb], VarKind::Node)
            {
                return Ok(None);
            }
            // `Some(empty)` = a named type never minted: no edge, as the probe's
            // "no named type has ever been minted ⇒ false".
            let tokens = graph.type_tokens_peek(&ep.types);
            let negate = ep.negate;
            counted!("interp.pipeline edge pred filter");
            self.selection.retain(|&r| {
                let hit = graph.edge_count_slim(self.ids[va][r], ep.dir, &tokens, self.ids[vb][r]) > 0;
                hit != negate
            });
            return Ok(Some(()));
        }
        // TWO-VAR NODE/REL IDENTITY (in)equality: compare the two id columns
        // directly. For two bound (non-null) entities `run_streaming`'s `=`/`<>`
        // is exactly id equality — but only WITHIN a kind (a node never equals a
        // rel regardless of id), so a mixed-kind comparison DECLINES the operator
        // (`Ok(None)`) and the general path answers it. No property load, no
        // `eval_column`; keep the live rows the predicate holds on.
        if let Some((other, ne)) = &pred.id_other {
            let va = self.var_index(&pred.var);
            let vb = self.var_index(other);
            if self.var_kinds[va] != self.var_kinds[vb] {
                return Ok(None);
            }
            let ne = *ne;
            self.selection.retain(|&r| {
                let eq = self.ids[va][r] == self.ids[vb][r];
                if ne { !eq } else { eq }
            });
            return Ok(Some(()));
        }
        // Distinct ids of `pred.var` among the LIVE rows (sorted) — evaluate the
        // predicate ONCE per distinct id (O(distinct)), exactly as `finish_topk`
        // and the recognizers do, then keep live rows by membership. The
        // column-at-a-time win: no per-row property column, no per-row eval.
        let vi = self.var_index(&pred.var);
        let mut distinct_set: BTreeSet<u64> = BTreeSet::new();
        for &r in &self.selection {
            distinct_set.insert(self.ids[vi][r]);
        }
        let distinct: Vec<u64> = distinct_set.into_iter().collect();
        if distinct.is_empty() {
            return Ok(Some(()));
        }
        if let Some(labels) = labels.filter(|l| !l.is_empty()) {
            if matches!(self.var_kinds[vi], VarKind::Node) && graph.columnar_scans_enabled() {
                // The population handed to the column filter. A column the
                // cache already holds is read restricted to the live ids for
                // free; otherwise a live population of at least an eighth of
                // a single label walks the WHOLE label so the column is kept
                // (`load_walk_budgeted` keeps only whole-label walks) — the
                // gather costs a few hundred extra reads once and none after;
                // a small population, or a multi-label var, gathers its own
                // ids as before.
                let single = (labels.len() == 1).then(|| labels[0].as_str());
                let cached = single.is_some_and(|l| {
                    pred.props
                        .iter()
                        .filter(|p| p.as_str() != NODE_IDENTITY_KEY)
                        .all(|p| graph.prop_column(l, p, false).is_some())
                });
                let label_n = labels
                    .iter()
                    .map(|l| graph.count_label_nodes(l))
                    .min()
                    .unwrap_or(0);
                let whole = single.is_some() && !cached && whole_label_worth(distinct.len(), label_n);
                let over = if whole {
                    None
                } else {
                    Some(std::sync::Arc::new(distinct.clone()))
                };
                if let Some(pass) = crate::batch::filter_ids_strict(
                    graph, labels, &pred.var, &pred.expr, params, over,
                )? {
                    counted!("interp.pipeline bound-var predicate filtered by columns");
                    if whole {
                        counted!("interp.pipeline bound-var predicate walked its whole label");
                    }
                    self.selection
                        .retain(|&r| pass.binary_search(&self.ids[vi][r]).is_ok());
                    return Ok(Some(()));
                }
            }
        }
        let Some(cols) = load_var_columns_labelled(
            graph,
            self.var_kinds[vi],
            &distinct,
            &pred.props,
            labels,
            params,
        )?
        else {
            return Ok(None); // an id-span over the column budget
        };
        let empty_vm = VarMap::new();
        let scope = Scope::over(params, &empty_vm, graph.wall_ms(), graph.zone_provider());
        let view = crate::vectorized::view(&cols);
        let Some(truth) = eval_column(&pred.expr, &pred.var, distinct.len(), &view, &scope) else {
            return Ok(None);
        };
        // The sorted pass id-set (`distinct` is sorted, so `pass` stays sorted).
        let mut pass: Vec<u64> = Vec::with_capacity(distinct.len());
        for (i, t) in truth.iter().enumerate() {
            match t.truth() {
                Some(Truth::True) => pass.push(distinct[i]),
                Some(_) => {}
                None => return Ok(None), // non-boolean WHERE — the general path errors
            }
        }
        self.selection
            .retain(|&r| pass.binary_search(&self.ids[vi][r]).is_ok());
        Ok(Some(()))
    }

    /// The live rows as `Vec<node id per var>` in PRODUCTION ORDER (the selection
    /// is kept in ascending row index, so scan-order x nested reverse-adjacency
    /// is preserved) — the input to the shared projection tail.
    fn live_rows(&self) -> Vec<Vec<u64>> {
        self.selection
            .iter()
            .map(|&r| (0..self.ids.len()).map(|vi| self.ids[vi][r]).collect())
            .collect()
    }

    /// One live row's ids, in binding order — the winner materialiser.
    fn row_ids(&self, r: usize) -> Vec<u64> {
        (0..self.ids.len()).map(|vi| self.ids[vi][r]).collect()
    }

    /// THE `WITH` BOUNDARY: project this (stage-1) chunk down to the CARRIED
    /// variables (`carried`, indices into this chunk's vars, in the WITH's item
    /// order), dropping every other id-column AND its var_kind, and RESETTING
    /// relationship isomorphism (fresh empty `used_rels`). The result is the SEED
    /// for stage 2 — its vars/var_kinds are exactly the carried ones, in carried
    /// order, so they occupy the LOW indices of the stage-2 chain's binding order
    /// (matching `collect_hops`' prebound prefix), and a stage-2 hop continuing
    /// from a carried var indexes straight into them.
    ///
    /// Rows are taken from this chunk's LIVE selection in PRODUCTION order. When
    /// `distinct` (a `WITH DISTINCT`) the carried-var tuples are deduped
    /// FIRST-SEEN *before* stage 2 — reusing the rev-112 identity discipline: a
    /// carried var is a bound node/rel, whose canonical `agg_key` is injective in
    /// (kind-tag, id), so each column's fixed kind makes the raw id tuple a
    /// byte-identical discriminant of `run_streaming`'s value-key dedup. A single
    /// node/rel var takes the `u64` fast path (`BTreeSet<u64>`); several vars key
    /// on the id tuple (`BTreeSet<Vec<u64>>`). Dedup at the boundary means a
    /// duplicate carried tuple does NOT multiply stage-2 work — the load-bearing
    /// `WITH DISTINCT` semantics. `prov` is left empty (this is not an OPTIONAL
    /// left join). `Ok`-fallible only through the group-cardinality budget.
    fn project_carried(
        &self,
        graph: &Graph,
        carried: &[usize],
        distinct: bool,
    ) -> Result<DataChunk, RunError> {
        let ncols = carried.len();
        let vars: Vec<String> = carried.iter().map(|&i| self.vars[i].clone()).collect();
        let var_kinds: Vec<VarKind> = carried.iter().map(|&i| self.var_kinds[i]).collect();
        let mut out_ids: Vec<Vec<u64>> = (0..ncols).map(|_| Vec::new()).collect();
        if distinct && ncols == 1 {
            // Fast path: one carried node/rel var — dedup on the raw u64 id.
            let ci = carried[0];
            let mut seen: BTreeSet<u64> = BTreeSet::new();
            for &r in &self.selection {
                let id = self.ids[ci][r];
                if seen.insert(id) {
                    out_ids[0].push(id);
                    budget_check(graph, out_ids[0].len())?;
                }
            }
        } else if distinct {
            // Several carried vars — dedup on the id tuple in carried order.
            let mut seen: BTreeSet<Vec<u64>> = BTreeSet::new();
            for &r in &self.selection {
                let tup: Vec<u64> = carried.iter().map(|&ci| self.ids[ci][r]).collect();
                if seen.insert(tup.clone()) {
                    for (k, &ci) in carried.iter().enumerate() {
                        out_ids[k].push(self.ids[ci][r]);
                    }
                    budget_check(graph, out_ids[0].len())?;
                }
            }
        } else {
            // Pass-through: every live row, in production order (no dedup).
            for &r in &self.selection {
                for (k, &ci) in carried.iter().enumerate() {
                    out_ids[k].push(self.ids[ci][r]);
                }
            }
        }
        let n = out_ids.first().map_or(0, Vec::len);
        // A fold never precedes a WITH boundary (folds are planned only on the
        // single-MATCH count-only aggregate), so there is no weight to carry; a
        // DISTINCT carry could not carry one anyway (a dedup discards
        // multiplicity), which is why this is asserted rather than handled.
        debug_assert!(
            self.weights.is_empty(),
            "a WITH boundary never follows a folded hop"
        );
        Ok(DataChunk {
            vars,
            var_kinds,
            ids: out_ids,
            selection: (0..n).collect(),
            // RESET rel-iso at the WITH boundary — Cypher relationship-uniqueness
            // is PER-MATCH-CLAUSE: `run_streaming` starts each MATCH's partials
            // with `used: Vec::new()` (`handle_start`), carrying only bindings —
            // not the traversed-rel set — across a WITH. Stage 2 must not inherit
            // stage 1's `used_rels`.
            used_rels: vec![Vec::new(); n],
            prov: Vec::new(),
            weights: Vec::new(),
        })
    }
}

// ─── Plan recognition ────────────────────────────────────────────────────────

/// A single-variable WHERE predicate the filter operator can vectorise: the
/// expression, the ONE variable it reads, and that variable's properties it
/// touches.
///
/// A SECOND form is carried by the same struct: a two-variable NODE/REL IDENTITY
/// (in)equality `var <op> other` (`a = b` / `a <> b` / `NOT a = b`). When
/// `id_other` is `Some((other, ne))` the filter compares the two id columns
/// directly — no property load, no `eval_column` — which is byte-identical to
/// `run_streaming`'s `=`/`<>` over two bound (non-null) entities. `props` is then
/// empty and `expr` is retained only for provenance. This is what lets the
/// var-length reach set's downstream `WHERE person <> friend` run columnar (the
/// start, emitted when genuinely re-reached, is removed here — never
/// pre-excluded).
#[derive(Clone)]
struct WherePred {
    expr: Expr,
    var: String,
    props: BTreeSet<String>,
    /// `Some((other_var, is_ne))` for the two-var id (in)equality form.
    id_other: Option<(String, bool)>,
    /// A THIRD form: the bound-endpoint EDGE predicate `[NOT] (a)-[:T]->(b)`
    /// (operator B of the LSQB plan). `var` is then `src`, `props` is empty and
    /// `expr` is provenance; the filter answers it per row through
    /// `Graph::edge_count_slim`, or the fold inlines it (`InlinePred::EdgeToBound`).
    edge: Option<EdgePred>,
}

/// The bound-endpoint edge predicate `[NOT] (src)-[:types]->(dst)` — the exact
/// shape `interp::exists_probe_fast` answers with `adjacency_probe`: ONE path,
/// ONE hop, no path/rel variable, no rel props/length, no labels or props on
/// either node, BOTH endpoints bound (and, by the recognisers' guards,
/// non-nullable). `dir` is from `src`'s side (`Both` for `-[:T]-`).
#[derive(Clone)]
struct EdgePred {
    src: String,
    dst: String,
    dir: Dir,
    types: Vec<String>,
    negate: bool,
}

/// Recognise the bound-endpoint edge predicate, or `None`. Bare `(a)-[:T]->(b)`
/// is the positive form, `NOT (a)-[:T]->(b)` the anti-join. Anything richer —
/// a second hop, a var-length, a rel variable/props, a label or prop on either
/// node, an unbound endpoint — is left to `contains_opaque`'s decline so the
/// general matcher keeps its exact semantics (a labelled bound far end
/// RE-VERIFIES the label there, which this probe does not).
fn edge_pred_of(w: &Expr, vars: &[String]) -> Option<EdgePred> {
    let (path, negate) = match w {
        Expr::PatternPredicate(p) => (p.as_ref(), false),
        Expr::Not(inner) => match inner.as_ref() {
            Expr::PatternPredicate(p) => (p.as_ref(), true),
            _ => return None,
        },
        _ => return None,
    };
    if path.var.is_some() || path.shortest || path.hops.len() != 1 {
        return None;
    }
    let (rel, end) = &path.hops[0];
    if rel.var.is_some() || rel.props.is_some() || rel.length.is_some() {
        return None;
    }
    if !path.start.labels.is_empty()
        || path.start.props.is_some()
        || !end.labels.is_empty()
        || end.props.is_some()
    {
        return None;
    }
    let src = path.start.var.as_ref()?;
    let dst = end.var.as_ref()?;
    if !vars.iter().any(|v| v == src) || !vars.iter().any(|v| v == dst) {
        return None;
    }
    let dir = match rel.dir {
        RelDir::Out => Dir::Out,
        RelDir::In => Dir::In,
        RelDir::Undirected => Dir::Both,
    };
    Some(EdgePred {
        src: src.clone(),
        dst: dst.clone(),
        dir,
        types: rel.types.clone(),
        negate,
    })
}

/// One fixed directed hop = one step in the recognised (multi-)path pattern, in
/// the order `run_streaming` executes it: the index of the ALREADY-bound source
/// var to drive from, the direction, relationship types, and the (unpropertied)
/// end node's labels + variable name. `track` = this hop's path has more than
/// one hop (so isomorphism is enforced within it); `reset` = this is the first
/// hop of a later path (k>=2), where the per-row `used` set is re-seeded empty
/// (`run_streaming` resets `Partial.used` per path).
///
/// Two step kinds, distinguished by `tgt`:
///   - `tgt: None` — an EXPAND to the NEW var `var`: appends a column of that
///     var's neighbours (Phase 4a).
///   - `tgt: Some(vi)` — a SEMIJOIN that CLOSES onto the ALREADY-bound var at
///     index `vi` (a cycle / connecting path, Phase 4b1): appends NO column,
///     keeping only source neighbours equal to that bound var's id. Only a
///     path's FINAL hop may be a semijoin.
#[derive(Clone)]
struct Hop {
    src: usize,
    dir: Dir,
    types: Vec<String>,
    labels: Vec<String>,
    var: String,
    /// The bound RELATIONSHIP variable of this hop, if any (`(a)-[r:T]->(b)`).
    /// `expand`/`semijoin` append its id as an extra Rel-kind column; `None` is
    /// the anonymous hop, byte-identical to before.
    rel_var: Option<String>,
    track: bool,
    reset: bool,
    tgt: Option<usize>,
    /// `Some(max)` for a FRONTIER-BFS variable-length hop `-[:T*1..max]-` — the
    /// hop expands set-at-a-time over a visited set (`DataChunk::expand_var_length_bfs`)
    /// instead of the fixed-hop `expand`. `None` is an ordinary fixed hop. Only
    /// the frontier-eligible form is ever recorded here (single-hop single-path,
    /// min 1, bounded, no rel var/props, a NEW end var); every other var-length
    /// shape declines the whole query to the general path.
    varlen: Option<u64>,
    /// The index (into the binding order) of the NODE var an EXPAND hop binds;
    /// `None` for a close (which binds no node). Recorded at recognition so the
    /// count fold can walk the var↔hop tree without re-deriving names.
    end_vi: Option<usize>,
    /// COUNT FOLD (operator A, `docs/lsqb-completeness-plan.md`): this hop is
    /// NOT materialised — `fold_tail` counts the walks through it (and its
    /// whole subtree) and multiplies each live row's weight by that count; the
    /// hop appends only a `NULL_ID` placeholder column. Set by
    /// `plan_count_fold`, never by the recognisers; `false` = the hop expands
    /// exactly as before.
    fold: bool,
    /// The WHERE conjuncts evaluated INSIDE the fold at this hop's end var's
    /// level (each moved out of the plan's `wheres` by `plan_count_fold`).
    inline: Vec<InlinePred>,
}

/// A WHERE conjunct evaluated INSIDE the count fold, at the level of the folded
/// var a hop binds, against a var already in the fold's `bind` — a materialised
/// column of the driving row, or a folded ANCESTOR level. `plan_count_fold`
/// admits a conjunct here only when that other var IS bound by then (the
/// position rule), so the recursion never reads an unbound slot.
///
/// The shared `Bound` suffix is deliberate (it is what the LSQB plan calls
/// them, and it says the other side is a BOUND var, not a literal), so the
/// variant-name lint is waived rather than the names shortened.
#[allow(clippy::enum_variant_names)]
#[derive(Clone)]
enum InlinePred {
    /// `level <> other` — the two-var id inequality (`two_var_id_pred`).
    NeBound(usize),
    /// `level = other`.
    EqBound(usize),
    /// `[NOT] (level)-[:types]->(other)` — `dir` is from the LEVEL var's side
    /// (flipped from the source text when the level var is the pattern's far
    /// end), answered by `Graph::edge_count_slim` (operator B).
    EdgeToBound {
        vi: usize,
        dir: Dir,
        types: Vec<String>,
        negate: bool,
    },
}

/// The recognised CORE READ CHAIN: `MATCH (a:A)-[:T1]->(b:B)-[:T2]->(c:C)…
/// [WHERE <single-var pred>] RETURN <items over the bound vars> [ORDER BY
/// <single-var keys> [SKIP s]] [LIMIT k]`. A chain of N>=1 fixed DIRECTED hops,
/// every node a bound variable (optional label, no props), every rel with no
/// var/props/length; a non-star non-DISTINCT non-aggregating projection; ORDER
/// BY (if present) with single-var `eval_column`-vectorisable keys and a
/// REQUIRED LIMIT; a WHERE (if present) over EXACTLY ONE bound var.
struct CorePlan {
    a_labels: Vec<String>,
    a_var: String,
    hops: Vec<Hop>,
    /// Every bound var in binding order: `[a_var, hops[0].var, …]` plus any bound
    /// rel var each hop introduces (each immediately after its end node).
    vars: Vec<String>,
    /// Per-var KIND, parallel to `vars`.
    var_kinds: Vec<VarKind>,
    /// The one-variable WHERE, if any.
    wheres: Vec<WherePred>,
    /// The seekable inline start anchor `(a:L {id: val})`, if any — the scan seeds
    /// through the range index (`anchored_seed_ids`) instead of a whole-label scan
    /// when the label is above the seek floor. Its equality ALSO rides in `where_`,
    /// so the seeded result equals the whole-label scan then that filter (a pure
    /// performance choice). Without it a point-lookup like IS5 (`Message {id: X}`)
    /// scans the ENTIRE label (measured 95 ms over ~2M Messages vs Neo4j's 1 ms).
    start_anchor: Option<PropAnchor>,
    proj: Projection,
}

/// Which single bound variable an ORDER BY key / WHERE predicate reads.
enum KeyRef {
    /// No bound variable — a constant, folded once.
    Const,
    /// Reads exactly this var (index into `vars`).
    Var(usize),
}

/// Classify `e` as reading exactly one bound var (its index) or none (const),
/// collecting the props it reads on that var into `props[idx]`. Reuses the
/// recognizers' own `key_side` (kept in lockstep with `eval_column`): probing
/// `e` against each var as `key_side`'s "A" slot with an unusable "B" slot
/// yields `Side::A` iff `e` reads ONLY that var (and consts). `None` = a form
/// `eval_column` cannot vectorise, or a key spanning >1 bound var — decline.
fn classify_key(e: &Expr, vars: &[String], props: &mut [BTreeSet<String>]) -> Option<KeyRef> {
    // A pure constant classifies the same against any var pair.
    let (mut da, mut db) = (BTreeSet::new(), BTreeSet::new());
    if let Some(Side::Const) = key_side(e, SENTINEL_A, SENTINEL_B, &mut da, &mut db) {
        return Some(KeyRef::Const);
    }
    for (idx, v) in vars.iter().enumerate() {
        let (mut pa, mut pb) = (BTreeSet::new(), BTreeSet::new());
        if let Some(Side::A) = key_side(e, v, SENTINEL_B, &mut pa, &mut pb) {
            props[idx].extend(pa);
            return Some(KeyRef::Var(idx));
        }
    }
    None
}

/// Resolve an ORDER BY key that is a bare reference to a projection ALIAS to the
/// expression that alias projects, so the core top-k CLASSIFIES and EVALUATES the
/// underlying expr rather than an unbindable name. `ORDER BY cd` under `RETURN
/// message.creationDate AS cd` sorts by `message.creationDate` — exactly the value
/// `run_streaming`'s post-projection ORDER BY scope produces (`project_row_values`
/// evaluates each key against a `scope_row` in which every alias COLUMN is bound
/// over the pre-projection row, so `Var("cd")` resolves to the projected value).
///
/// A `Var(name)` that is a BOUND PATTERN VAR is the WHOLE entity, never an alias,
/// and is returned UNCHANGED (today's behavior — a bare node var in ORDER BY keeps
/// declining through `classify_key`). Resolution is NOT transitive: an alias's
/// target cannot itself reference another alias of the SAME RETURN. Aliases are
/// resolved INSIDE an enclosing scalar fn / arithmetic / property access too, so
/// `ORDER BY toInteger(personId)` under `friend.id AS personId` becomes
/// `toInteger(friend.id)` (the IC11 case) — matching `run_streaming`'s
/// post-projection scope, which binds each alias then evaluates the whole key.
fn resolve_order_key_alias(e: &Expr, vars: &[String], proj: &Projection) -> Expr {
    match e {
        Expr::Var(name) => {
            // A bound pattern var names the entity itself, never a projection alias.
            if vars.iter().any(|v| v == name) {
                return e.clone();
            }
            // A bare alias reference resolves to that item's target expr (NOT
            // transitively — the target is not itself re-resolved); an unknown name
            // is left for `classify_key` to decline.
            match proj
                .items
                .iter()
                .find(|it| it.alias.as_deref() == Some(name.as_str()))
            {
                Some(it) => it.expr.clone(),
                None => e.clone(),
            }
        }
        Expr::Call {
            name,
            distinct,
            args,
            star,
        } => Expr::Call {
            name: name.clone(),
            distinct: *distinct,
            star: *star,
            args: args
                .iter()
                .map(|a| resolve_order_key_alias(a, vars, proj))
                .collect(),
        },
        Expr::Bin(op, l, r) => Expr::Bin(
            *op,
            Box::new(resolve_order_key_alias(l, vars, proj)),
            Box::new(resolve_order_key_alias(r, vars, proj)),
        ),
        Expr::Prop(base, key) => Expr::Prop(
            Box::new(resolve_order_key_alias(base, vars, proj)),
            key.clone(),
        ),
        Expr::Neg(x) => Expr::Neg(Box::new(resolve_order_key_alias(x, vars, proj))),
        Expr::Not(x) => Expr::Not(Box::new(resolve_order_key_alias(x, vars, proj))),
        // Anything else has no alias to resolve within — returned unchanged.
        other => other.clone(),
    }
}

/// The recognised READ CHAIN alone — the `MATCH <path1>, <path2>, … [WHERE
/// <single-var pred>]` prefix shared by the non-aggregating [`CorePlan`] and the
/// group-by-count [`AggPlan`]. Path 1 is a chain of N>=1 fixed DIRECTED hops
/// over a labelled unpropertied start; each SUBSEQUENT path re-roots at an
/// ALREADY-BOUND var and its N>=1 fixed directed hops introduce NEW end vars
/// (Phase 4a — a chain expressed as multiple paths, or a branch), EXCEPT a
/// path's FINAL hop, which may instead CLOSE onto an already-bound var (a
/// semijoin, Phase 4b1). Every node is a bound variable (optional label, no
/// props), every rel carries no var/props/length. The `hops` are the ordered
/// steps (each with its source var index; `tgt` distinguishes expand vs
/// semijoin), in `run_streaming`'s execution order; and the one-variable WHERE,
/// if any. Everything a projection/aggregation tail is then layered on.
struct Chain {
    a_labels: Vec<String>,
    a_var: String,
    hops: Vec<Hop>,
    vars: Vec<String>,
    /// Per-var KIND, parallel to `vars` (Node/Rel) — carried into the plans so
    /// the projection tail materialises each var correctly.
    var_kinds: Vec<VarKind>,
    /// The WHERE as a CONJUNCTION of per-predicate filters — so a scan anchor
    /// (`p.id = 4139`) AND a textual WHERE (`x.prop < …` on a later var) BOTH
    /// ride, each applied as its vars bind. Empty for no WHERE.
    wheres: Vec<WherePred>,
    /// The seekable scan-seed anchor from an inline `(a:L {id: val})`, if any.
    /// Its equality is ALSO folded into `wheres` (so results are correct without
    /// it); this drives the range-index seed so an anchored single-seed chain
    /// need not clone the whole label member list — a pure performance choice
    /// whose seed RESULT equals the whole-label scan then that filter.
    start_anchor: Option<PropAnchor>,
}

/// Recognise the read chain of a single MATCH (pattern + optional WHERE), the
/// prefix both tails require — SINGLE- OR MULTI-path. A path's non-final hops
/// introduce NEW end vars (Phase 4a); its FINAL hop MAY instead CLOSE onto an
/// already-bound var — a CYCLE / connecting-path recorded as a SEMIJOIN step
/// (Phase 4b1). `None` = a shape the columnar expand chain cannot reproduce
/// (rel-driven order, var-length, a NON-final hop onto a bound var, a DISJOINT
/// path whose start is not bound, a rel-var/rel-prop hop, a
/// spanning/opaque WHERE, …) — decline. DIRECTED and UNDIRECTED fixed hops are
/// both accepted (undirected routes through `Dir::Both`).
fn recognise_chain(pattern: &Pattern, where_opt: Option<&Expr>) -> Option<Chain> {
    // Accept an INLINE start-property anchor `(a:L {id: val})` on the scan start
    // (the single-seed shape `MATCH (p:Person {id: 4139})-[:KNOWS]-()-...`) —
    // the SAME opt-in the multistage recognisers use. It is desugared into a
    // source-var equality `a.prop = val` and folded into the WHERE, so the read
    // chain filters byte-identically (a full-label scan then that filter). The
    // range-index SEEK is a pure performance choice layered on separately; the
    // fold alone is already correct.
    let hc = collect_hops(pattern, None, false, true, true)?;
    // The inline start anchor (`p.id = val`) AND any textual WHERE now BOTH ride,
    // ANDed together and split per-predicate by `recognise_where_preds` — so
    // `MATCH (p:Person {id:X})-…-(x) WHERE x.prop …` (IC12's stage 2) is accepted,
    // the anchor filtering the seed and the WHERE its own var. (Previously the
    // single-pred read chain declined the anchor+WHERE combo.)
    let anchor_eq: Option<Expr> = hc.start_anchor.as_ref().map(|(prop, val)| {
        Expr::Bin(
            engram_cypher::ast::BinOp::Eq,
            Box::new(Expr::Prop(
                Box::new(Expr::Var(hc.a_var.clone())),
                prop.clone(),
            )),
            Box::new(val.clone()),
        )
    });
    let combined: Option<Expr> = match (anchor_eq, where_opt) {
        (Some(a), Some(w)) => Some(Expr::And(Box::new(a), Box::new(w.clone()))),
        (Some(a), None) => Some(a),
        (None, w) => w.cloned(),
    };
    // Fold any MID/FINAL-hop inline anchors in too, then split the whole
    // conjunction per-predicate.
    let combined = and_node_anchors(combined.as_ref(), &hc.node_anchors);
    let wheres = recognise_where_preds(combined.as_ref(), &hc.vars)?;
    let start_anchor = hc.start_anchor.as_ref().map(|(prop, val)| PropAnchor {
        prop: prop.clone(),
        values: vec![val.clone()],
    });
    Some(Chain {
        a_labels: hc.a_labels,
        a_var: hc.a_var,
        hops: hc.hops,
        vars: hc.vars,
        var_kinds: hc.var_kinds,
        wheres,
        start_anchor,
    })
}

/// The hop skeleton [`collect_hops`] recognises, before any WHERE is layered on:
/// the scan start (`a_labels`/`a_var`, empty in the re-rooted OPTIONAL case), the
/// ordered expand/semijoin steps, the bound vars in binding order, and the label
/// set known per var (so a later path's restated label is checked, not re-applied).
struct Hops {
    a_labels: Vec<String>,
    a_var: String,
    hops: Vec<Hop>,
    vars: Vec<String>,
    /// Per-var KIND, parallel to `vars`.
    var_kinds: Vec<VarKind>,
    var_labels: BTreeMap<String, Vec<String>>,
    /// An INLINE start-property anchor `(a:L {prop: val})` on the INTRODUCING
    /// scan start, when the caller opted in (`allow_start_anchor`): `(prop, val)`
    /// with `val` a scalar literal or a `$param`. It is `None` for every other
    /// caller (inline props still DECLINE there) and for a start with no props.
    /// The recognizer desugars it into a source-var equality `a.prop = val` that
    /// it ANDs into the WHERE (so it filters byte-identically) and seeds the scan
    /// through the range index (`anchored_seed_ids`).
    start_anchor: Option<(String, Expr)>,
    /// Desugared MID/FINAL-hop inline anchors `(x:L {prop: val})` — each a
    /// `x.prop = val` equality (`val` a scalar/param) the recognizer ANDs into its
    /// WHERE (a pure FILTER — a mid-chain node is not the scan seed, so no index
    /// seek). Populated ONLY when the caller opts in (`allow_node_anchor`); an
    /// inline prop still DECLINES the whole chain otherwise. A caller that opts in
    /// MUST fold these into its WHERE or it silently drops the filter.
    node_anchors: Vec<Expr>,
}

/// The already-bound context handed to [`collect_hops`] for an OPTIONAL pattern:
/// the outer vars in binding order, and the labels known per var (so a restated
/// label is checked, not re-applied). `None` in `collect_hops` is the read-chain
/// case, where path 1 introduces its own labelled scan start.
type Prebound<'a> = (
    &'a [String],
    &'a [VarKind],
    &'a BTreeMap<String, Vec<String>>,
);

/// Recognise the ordered expand/semijoin steps of a pattern — the shared core of
/// the single-MATCH read chain and the OPTIONAL-MATCH left-join pattern.
///
/// With `prebound` = `None` (the read-chain case) path 1 introduces an
/// unpropertied LABELLED start with a variable, and later paths re-root at an
/// already-bound var. With `prebound` = `Some((vars, labels))` (the OPTIONAL
/// case) EVERY path re-roots at an already-bound var — one of the outer vars, or
/// a var an earlier optional path introduced — and there is no scan start. In
/// both cases a path's non-final hops introduce NEW end vars (an EXPAND step) and
/// its FINAL hop may instead CLOSE onto an already-bound var (a SEMIJOIN step);
/// every rel is a fixed directed hop with no var/props/length; every node is a
/// variable with no props. Each hop is tagged with its source var index,
/// isomorphism (`track`) and path-boundary (`reset`) flags. `None` = a shape the
/// columnar expand chain cannot reproduce (rel-driven order, var-length, a
/// NON-final hop onto a bound var, a DISJOINT path whose start is not bound, a
/// rel-var/rel-prop hop, …). A fixed hop may be DIRECTED or UNDIRECTED (the
/// latter routes through `Dir::Both`).
fn collect_hops(
    pattern: &Pattern,
    prebound: Option<Prebound<'_>>,
    allow_hopless_start: bool,
    allow_start_anchor: bool,
    allow_node_anchor: bool,
) -> Option<Hops> {
    if pattern.paths.is_empty() {
        return None;
    }
    let reroot_all = prebound.is_some();
    let mut vars: Vec<String> = Vec::new();
    let mut var_kinds: Vec<VarKind> = Vec::new();
    let mut var_labels: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Some((pv, pk, pl)) = prebound {
        vars.extend(pv.iter().cloned());
        var_kinds.extend(pk.iter().copied());
        var_labels = pl.clone();
    }
    let mut hops: Vec<Hop> = Vec::new();
    let mut a_labels: Vec<String> = Vec::new();
    let mut a_var = String::new();
    let mut start_anchor: Option<(String, Expr)> = None;
    let mut node_anchors: Vec<Expr> = Vec::new();
    // Names an ANONYMOUS `()` node in a path — a traversal step bound to no user
    // variable (the intermediate hop of a 2-hop `(a)-[:T]-()-[:T]-(g)`). We
    // synthesise a unique internal name so the columnar chain carries its column
    // (never projected or filtered); the traversal and relationship isomorphism
    // are byte-identical to a named node, and since Cypher does not require
    // node-distinctness an unnamed node never closes a semijoin.
    let mut anon_counter = 0usize;
    let mut synth_anon = |vars: &[String]| -> String {
        loop {
            let candidate = format!("__pipe_anon_{anon_counter}");
            anon_counter += 1;
            if !vars.iter().any(|v| v == &candidate) {
                return candidate;
            }
        }
    };
    for (pi, path) in pattern.paths.iter().enumerate() {
        if path.var.is_some() || path.shortest {
            return None;
        }
        // The path's source var: in the read chain path 1 introduces a labelled
        // start, and every other path (all of them, when re-rooting for an
        // OPTIONAL) RE-ROOTS at an already-bound var (a disjoint start declines).
        // An ANONYMOUS introducing start (`MATCH (:Person {id:10})-…`, IC12) is
        // given a synthesised internal var — it seeds the scan (its label + anchor
        // apply) but is never projected/filtered by the user; a re-root start must
        // name an already-bound var, so an anonymous one there still declines.
        let introduces_start = pi == 0 && !reroot_all;
        let start_var = match path.start.var.as_deref() {
            Some(v) => v.to_string(),
            None if introduces_start => synth_anon(&vars),
            None => return None,
        };
        // An INLINE start-property map `(a:L {prop: val})`. Accepted ONLY as a
        // SINGLE scalar/param equality on the INTRODUCING scan start, and ONLY
        // when the caller opts in (`allow_start_anchor` — the composite stage 1).
        // It desugars to a source-var anchor the recognizer ANDs into the WHERE.
        // A re-rooted start, a multi-entry map, a non-scalar/var-reading value, or
        // a non-opting caller DECLINES — byte-identical to before (props ⇒ None).
        if path.start.props.is_some() {
            let anchor = if allow_start_anchor && introduces_start {
                start_prop_anchor(path.start.props.as_ref())
            } else {
                None
            };
            match anchor {
                Some((prop, val)) => start_anchor = Some((prop, val)),
                None => return None,
            }
        }
        // A HOPLESS path (a bare node) is a plain label SCAN. It is allowed only
        // as the introducing start of the OUTER of an OPTIONAL (`allow_hopless_
        // start`), never as a read chain (batch.rs owns census scans, so the
        // single-MATCH pipeline keeps declining them) nor as a re-rooted/later
        // path (a no-op or disjoint we do not model).
        if path.hops.is_empty() {
            if introduces_start && allow_hopless_start && !path.start.labels.is_empty() {
                a_labels = path.start.labels.clone();
                a_var = start_var.clone();
                var_labels.insert(start_var.clone(), path.start.labels.clone());
                vars.push(start_var.clone());
                var_kinds.push(VarKind::Node);
                continue;
            }
            return None;
        }
        let mut src = if introduces_start {
            if path.start.labels.is_empty() {
                return None; // no start label ⇒ rel-driven order — decline
            }
            a_labels = path.start.labels.clone();
            a_var = start_var.clone();
            var_labels.insert(start_var.clone(), path.start.labels.clone());
            vars.push(start_var.clone());
            var_kinds.push(VarKind::Node);
            0usize
        } else {
            let Some(idx) = vars.iter().position(|v| v == &start_var) else {
                return None; // a disjoint/cartesian path — its start is not bound
            };
            // A restated start label must already hold on the bound var (the
            // columnar path enforced it at introduction); a NEW label is an
            // extra constraint the expand chain never applied — decline.
            let known = var_labels.get(&start_var).map(Vec::as_slice).unwrap_or(&[]);
            if !path.start.labels.iter().all(|l| known.contains(l)) {
                return None;
            }
            idx
        };
        let track = path.hops.len() > 1;
        for (hi, (rel, node)) in path.hops.iter().enumerate() {
            if rel.props.is_some() || rel.types.is_empty() {
                // Still declined: an inline rel property MAP (`-[r {k: v}]->`,
                // whose `rel_satisfies` equality semantics the columnar filter
                // does not reproduce) and a typeless hop. A VARIABLE-LENGTH rel is
                // handled below (only the frontier-BFS-eligible form; every other
                // var-length shape declines).
                return None;
            }
            // FRONTIER-BFS VARIABLE-LENGTH HOP `-[:T*1..max]-`. Accept ONLY the
            // shape `run_streaming`'s `frontier_ok` runs as a BFS: this hop is the
            // SOLE hop of the SOLE path (no other steps compose with the visited
            // set), the rel has no variable and no property map, the bound is
            // `*1..max` (`min == 1`, a finite `max`), and the end node is a NEW
            // var (a bound far end would be a pair test, not a reach set). The
            // downstream DISTINCT-consumed gate is applied by the recognizer that
            // knows the breaker (`varlen_distinct_consumed`). Anything else — an
            // unbounded `*`, `min != 1`, a path var (declined at the top), a rel
            // var/prop, a multi-hop or multi-path pattern, a bound far end —
            // declines the whole query to the enumerating general path.
            if let Some(vl) = rel.length {
                let end_var = node.var.as_deref()?.to_string();
                let eligible = pattern.paths.len() == 1
                    && path.hops.len() == 1
                    && rel.var.is_none()
                    && node.props.is_none()
                    && vl.min.unwrap_or(1) == 1
                    && vl.max.is_some()
                    && !vars.iter().any(|v| v == &end_var);
                if !eligible {
                    return None;
                }
                let dir = match rel.dir {
                    RelDir::Out => Dir::Out,
                    RelDir::In => Dir::In,
                    RelDir::Undirected => Dir::Both,
                };
                vars.push(end_var.clone());
                var_kinds.push(VarKind::Node);
                var_labels.insert(end_var.clone(), node.labels.clone());
                hops.push(Hop {
                    src,
                    dir,
                    types: rel.types.clone(),
                    labels: node.labels.clone(),
                    var: end_var,
                    rel_var: None,
                    // A frontier BFS carries no per-row `used` set (node-dedup via
                    // the visited set replaces relationship isomorphism), so it
                    // neither tracks nor resets rel-iso.
                    track: false,
                    reset: false,
                    tgt: None,
                    varlen: vl.max,
                    end_vi: Some(vars.len() - 1),
                    fold: false,
                    inline: Vec::new(),
                });
                // The sole hop of the sole path — the loops end here.
                continue;
            }
            // A bound RELATIONSHIP variable (`-[r:T]->`) is now accepted — it
            // becomes an extra Rel-kind column consumed by WHERE / RETURN / ORDER
            // BY / group-by / aggregates over the pipeline. Decline a rel var that
            // collides with an already-bound var (a rel-var self-join we do not
            // model).
            let rel_var: Option<String> = match &rel.var {
                Some(rv) if vars.iter().any(|v| v == rv) => return None,
                other => other.clone(),
            };
            let dir = match rel.dir {
                RelDir::Out => Dir::Out,
                RelDir::In => Dir::In,
                // An UNDIRECTED hop routes through `Dir::Both`, exactly as
                // `run_streaming` does (`hops_stream` maps `RelDir::Undirected
                // => Dir::Both`). Both then walk the SAME `adjacent_slim(src,
                // Both)` — OUT neighbours then IN neighbours, with an IN-side
                // self-loop deduped inside `adjacent_slim` — and both emit them
                // in REVERSE of that order: the pipeline via `adj.iter().rev()`,
                // `run_streaming` via the LIFO pop of `expand_var_length`. So the
                // neighbour order, the self-loop dedup and relationship
                // isomorphism (an undirected edge carries ONE `rel.id`, so a walk
                // reusing it is dropped by the same per-row `used` set) are all
                // byte-identical; no order adjustment is needed here.
                RelDir::Undirected => Dir::Both,
            };
            let var = match node.var.as_deref() {
                Some(v) => v.to_string(),
                None => synth_anon(&vars),
            };
            // An INLINE node-property map on a MID/FINAL-hop node — desugar a SINGLE
            // scalar/param equality `(x:L {prop: val})` into `x.prop = val` the
            // recognizer ANDs into its WHERE (a pure FILTER; a mid-chain node is not
            // the scan seed, so no index seek). Accepted ONLY when the caller opts in
            // (`allow_node_anchor`) AND folds `node_anchors`; a multi-entry map, a
            // non-scalar / var-reading value, or a non-opting caller DECLINES (an
            // inline prop ⇒ None, exactly as before).
            if node.props.is_some() {
                // A node-prop map on a hop that CLOSES onto an already-bound var
                // (a semijoin) is an extra constraint the close never applies —
                // decline, exactly as a restated label on the bound end does
                // (line ~1191). Only a NEW node (an EXPAND end or re-root) turns
                // an inline prop into a real filtered column, so only there does
                // the anchor desugar apply.
                let closes_onto_bound = vars.iter().any(|v| v == &var);
                match (
                    allow_node_anchor,
                    closes_onto_bound,
                    start_prop_anchor(node.props.as_ref()),
                ) {
                    (true, false, Some((prop, val))) => node_anchors.push(Expr::Bin(
                        engram_cypher::ast::BinOp::Eq,
                        Box::new(Expr::Prop(Box::new(Expr::Var(var.clone())), prop)),
                        Box::new(val),
                    )),
                    _ => return None,
                }
            }
            let is_final = hi + 1 == path.hops.len();
            // `reset` re-seeds the isomorphism base empty at a later path's first
            // hop. In the OPTIONAL case each outer row seeds a FRESH single-row
            // chunk (empty `used_rels`), so path 0 already has an empty base and
            // `pi > 0 && hi == 0` is the same correct condition as the read chain.
            let reset = pi > 0 && hi == 0;
            if let Some(tgt_vi) = vars.iter().position(|v| v == &var) {
                // The end var is ALREADY BOUND. Only a path's FINAL hop may CLOSE
                // onto it — a CYCLE / connecting-path (a semijoin, Phase 4b1).
                // A NON-final bound close (a mid-chain self-join) is not a
                // tractable expand chain — decline, keeping the semijoin to the
                // one tail position where its order is `run_streaming`'s.
                if !is_final {
                    return None;
                }
                // A restated label on the bound end must already hold on that
                // var (enforced when it was introduced); a NEW label is an extra
                // constraint the semijoin never applies — decline.
                let known = var_labels.get(&var).map(Vec::as_slice).unwrap_or(&[]);
                if !node.labels.iter().all(|l| known.contains(l)) {
                    return None;
                }
                // A closing hop binds no new node var, but MAY bind a rel var —
                // then it is one appended Rel-kind column (the target `var` above
                // is already bound; the rel var is validated distinct at the top).
                if let Some(rv) = &rel_var {
                    vars.push(rv.clone());
                    var_kinds.push(VarKind::Rel);
                }
                hops.push(Hop {
                    src,
                    dir,
                    types: rel.types.clone(),
                    labels: node.labels.clone(),
                    var,
                    rel_var,
                    track,
                    reset,
                    tgt: Some(tgt_vi),
                    varlen: None,
                    end_vi: None,
                    fold: false,
                    inline: Vec::new(),
                });
                // A semijoin is the FINAL hop; the loop ends, so `src` is not
                // advanced (there is no next hop to continue from).
            } else {
                vars.push(var.clone());
                var_kinds.push(VarKind::Node);
                let node_idx = vars.len() - 1;
                var_labels.insert(var.clone(), node.labels.clone());
                // Bind the rel var AFTER the node (so the next hop continues from
                // the NODE's index, `node_idx`, not the rel column).
                if let Some(rv) = &rel_var {
                    vars.push(rv.clone());
                    var_kinds.push(VarKind::Rel);
                }
                hops.push(Hop {
                    src,
                    dir,
                    types: rel.types.clone(),
                    labels: node.labels.clone(),
                    var,
                    rel_var,
                    track,
                    reset,
                    tgt: None,
                    varlen: None,
                    end_vi: Some(node_idx),
                    fold: false,
                    inline: Vec::new(),
                });
                src = node_idx; // next hop of this path continues from this end NODE
            }
        }
    }
    Some(Hops {
        a_labels,
        a_var,
        hops,
        vars,
        var_kinds,
        var_labels,
        start_anchor,
        node_anchors,
    })
}

/// AND a chain's desugared mid-hop inline anchors (`node_anchors`) into a WHERE.
/// Returns the combined predicate the recognizer's WHERE recognizer then reads,
/// so an inline `(x {prop: val})` filters byte-identically. `None` stays `None`.
fn and_node_anchors(where_opt: Option<&Expr>, anchors: &[Expr]) -> Option<Expr> {
    let mut acc: Option<Expr> = where_opt.cloned();
    for a in anchors {
        acc = Some(match acc {
            Some(w) => Expr::And(Box::new(w), Box::new(a.clone())),
            None => a.clone(),
        });
    }
    acc
}

/// A single scalar/param inline start-property map `(a:L {prop: val})` → the
/// `(prop, val)` anchor, else `None`. A multi-entry map, a whole-map param
/// (`(a $p)`), or a non-scalar/var-reading value declines — those are not a
/// point equality this seeds and filters byte-identically.
fn start_prop_anchor(props: Option<&Expr>) -> Option<(String, Expr)> {
    match props {
        Some(Expr::Map(entries)) if entries.len() == 1 => {
            let (k, v) = &entries[0];
            is_scalar_or_param(v).then(|| (k.clone(), v.clone()))
        }
        _ => None,
    }
}

/// A variable-free SCALAR the anchor seed can evaluate once and probe the index
/// with — a literal or a `$param`. A list/map/expr value is not a point-equality
/// value and declines.
fn is_scalar_or_param(e: &Expr) -> bool {
    matches!(
        e,
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Param(_)
    )
}

/// Recognise an optional single-variable WHERE the [`DataChunk::filter`] operator
/// can vectorise, over `vars`. `Some(None)` = no WHERE; `Some(Some(pred))` = a
/// single-var predicate `eval_column` handles (a `<needle> IN <const list>`
/// property/expr membership among them), OR a two-variable NODE/REL IDENTITY
/// (in)equality `a = b` / `a <> b` / `NOT a = b` the filter applies by comparing
/// id columns; `None` = DECLINE (a spanning, opaque, const-only, or
/// unvectorisable form — a WHOLE-NODE `x IN <list>` membership, whose bare-var
/// needle is not `eval_column`-able, among them). The one place the read chain
/// and the OPTIONAL pattern agree on which WHERE forms are tractable.
fn recognise_single_var_where(
    where_opt: Option<&Expr>,
    vars: &[String],
) -> Option<Option<WherePred>> {
    match where_opt {
        None => Some(None),
        Some(w) => {
            // The bound-endpoint EDGE predicate `[NOT] (a)-[:T]->(b)` — recognised
            // BEFORE the opaque bail (a pattern predicate is opaque to
            // `free_vars`, which is why every other form of it still declines):
            // both endpoints are bound vars, so its earliest safe point IS known
            // (once both bind), and the filter answers it per row exactly as
            // `exists_probe_fast` does. The caller that admits a NULLABLE var
            // (the OPTIONAL recogniser) checks both endpoints against its
            // nullable set, as it does for `id_other`.
            if let Some(ep) = edge_pred_of(w, vars) {
                return Some(Some(WherePred {
                    expr: w.clone(),
                    var: ep.src.clone(),
                    props: BTreeSet::new(),
                    id_other: None,
                    edge: Some(ep),
                }));
            }
            if contains_opaque(w) || expr_has_aggregate(w) {
                return None;
            }
            // A two-variable node/rel identity (in)equality — applied by the
            // filter as a direct id-column comparison (`run_streaming`'s `=`/`<>`
            // over two bound, non-null entities). Recognised BEFORE `classify_key`
            // (which declines a >1-var key).
            if let Some((a, b, ne)) = two_var_id_pred(w, vars) {
                return Some(Some(WherePred {
                    expr: w.clone(),
                    var: a,
                    props: BTreeSet::new(),
                    id_other: Some((b, ne)),
                    edge: None,
                }));
            }
            let mut wp: Vec<BTreeSet<String>> = vec![BTreeSet::new(); vars.len()];
            match classify_key(w, vars, &mut wp)? {
                KeyRef::Const => None, // a const-only WHERE — decline
                KeyRef::Var(idx) => Some(Some(WherePred {
                    expr: w.clone(),
                    var: vars[idx].clone(),
                    props: std::mem::take(&mut wp[idx]),
                    id_other: None,
                    edge: None,
                })),
            }
        }
    }
}

/// Recognise a stage WHERE as a CONJUNCTION of individually-tractable predicates —
/// the generalisation of [`recognise_single_var_where`] the LITERAL IC5's
/// `WHERE person.id = X AND person <> friend` needs. `Some(vec)` = every top-level
/// `AND` conjunct is one shape the filter already handles alone — a single-var
/// property predicate (`classify_key`) or a two-var node/rel id (in)equality
/// (`two_var_id_pred`) — the vec in source order (EMPTY for no WHERE). Each
/// [`WherePred`] is applied as early as ITS referenced vars bind
/// (`build_chunk_from_ids`): a source-var pred filters the seed before the hops, a
/// two-var pred after the hop binding its second var. `None` = DECLINE (any
/// conjunct is neither shape — a >2-var predicate, an `OR`, a function/opaque, or
/// a const-only form), so the whole query falls to the general path identically.
fn recognise_where_preds(where_opt: Option<&Expr>, vars: &[String]) -> Option<Vec<WherePred>> {
    let Some(w) = where_opt else {
        return Some(Vec::new());
    };
    let mut conj = Vec::new();
    conjuncts_of(w, &mut conj);
    let mut out = Vec::with_capacity(conj.len());
    for c in conj {
        // Each conjunct must itself be ONE tractable single-predicate shape; a
        // const-only conjunct (`Some(None)` is impossible here — the single form
        // returns `None` for a const) declines the whole conjunction.
        match recognise_single_var_where(Some(&c), vars)? {
            Some(pred) => out.push(pred),
            None => return None,
        }
    }
    Some(out)
}

/// The predicates of a plan's single optional WHERE as a slice — `&[]` for none,
/// a one-element slice otherwise — so a single-`WherePred` plan feeds the same
/// multi-predicate [`build_chunk`]/[`build_chunk_from_ids`] the conjunction path
/// does.
fn where_slice(w: &Option<WherePred>) -> &[WherePred] {
    w.as_ref().map_or(&[], std::slice::from_ref)
}

/// A source-var property EQUALITY the labelled scan can SEED through the range
/// index instead of a whole-label scan — the pipeline twin of interp's
/// `Seed::PropEq`/`IndexEq`. `prop` is the property; `values` are the var-free
/// RHS(es) (`= v`, or the union of `IN [...]`). The probe spans all labels, so
/// [`anchored_seed_ids`] intersects it with the label members; the equality still
/// runs as a source `WherePred`, so this is a pure performance choice whose seed
/// RESULT equals a whole-label scan then that filter, byte-for-byte.
struct PropAnchor {
    prop: String,
    values: Vec<Expr>,
}

/// Recognise a two-variable NODE/REL IDENTITY (in)equality between two DISTINCT
/// bound variables: `a = b` → `Some((a, b, false))`, `a <> b` and `NOT a = b` →
/// `Some((a, b, true))`. Both sides must be bare bound variables (not property
/// reads); a self-comparison (`a = a`) is left to the const/single-var path.
/// Whether the two vars are the same KIND is checked at filter time (a
/// node-vs-rel comparison there falls back), so this stays a pure AST match.
fn two_var_id_pred(w: &Expr, vars: &[String]) -> Option<(String, String, bool)> {
    let (l, r, ne) = match w {
        Expr::Bin(engram_cypher::ast::BinOp::Eq, l, r) => (l.as_ref(), r.as_ref(), false),
        Expr::Bin(engram_cypher::ast::BinOp::Neq, l, r) => (l.as_ref(), r.as_ref(), true),
        Expr::Not(inner) => match inner.as_ref() {
            Expr::Bin(engram_cypher::ast::BinOp::Eq, l, r) => (l.as_ref(), r.as_ref(), true),
            _ => return None,
        },
        _ => return None,
    };
    let (Expr::Var(a), Expr::Var(b)) = (l, r) else {
        return None;
    };
    if a == b || !vars.iter().any(|v| v == a) || !vars.iter().any(|v| v == b) {
        return None;
    }
    Some((a.clone(), b.clone(), ne))
}

fn recognise_core(sq: &SingleQuery) -> Option<CorePlan> {
    let (pattern, where_opt, proj) = match sq.clauses.as_slice() {
        [
            Clause::Match {
                optional: false,
                pattern,
                where_,
            },
            Clause::Return { proj },
        ] => (pattern, where_.as_ref(), proj),
        _ => return None,
    };
    let chain = recognise_chain(pattern, where_opt)?;
    // A var-length hop is only tractable when its end is consumed DISTINCT-only; a
    // plain (non-distinct, non-aggregate) RETURN never is, so this declines it.
    if !varlen_distinct_consumed(&chain.hops, proj) {
        return None;
    }
    core_over_chain(chain, proj)
}

/// Layer a non-aggregating, NON-DISTINCT projection onto an already-recognised
/// read chain (the single-MATCH chain, or the OPTIONAL-MATCH combined chain).
/// Non-star, >=1 item; ORDER BY keys each single-var (or const) and
/// `eval_column`-vectorisable, with a REQUIRED LIMIT for the bounded top-k;
/// projection items read ONLY bound vars, non-aggregating, non-opaque. `None` =
/// DECLINE.
///
/// A DISTINCT projection is DECLINED here — it is owned by `distinct_over_chain`,
/// which treats `RETURN DISTINCT <items>` as a GROUP-BY with zero aggregate sites
/// (the items are the grouping keys) so the dedup is column-native (via
/// `reduce_agg_groups`) rather than materialising every row through
/// `project_rows_tail`. Routing DISTINCT through the full project tail was a
/// measured regression (per-row node materialisation before the dedup).
fn core_over_chain(chain: Chain, proj: &Projection) -> Option<CorePlan> {
    // A non-star, NON-DISTINCT projection with at least one item.
    if proj.star || proj.distinct || proj.items.is_empty() {
        return None;
    }
    // ORDER BY keys: each single-var over any bound var (or const), non-opaque,
    // non-aggregating — kept in lockstep with the top-k's own `classify_key`. A
    // bare reference to a projection ALIAS (`ORDER BY cd` under `… AS cd`) is
    // resolved to the aliased target expr first, then classified — the recognizer
    // ACCEPTS the alias case that `native_topk` then sorts by (byte-identical to
    // `run_streaming`'s post-projection ORDER BY scope). A bare pattern var stays
    // unchanged and keeps declining through `classify_key`.
    let mut order_props: Vec<BTreeSet<String>> = vec![BTreeSet::new(); chain.vars.len()];
    for o in &proj.order {
        let key = resolve_order_key_alias(&o.expr, &chain.vars, proj);
        if contains_opaque(&key) || expr_has_aggregate(&key) {
            return None;
        }
        classify_key(&key, &chain.vars, &mut order_props)?;
    }
    // ORDER BY requires a LIMIT for the bounded top-k (`native_topk`); a plain
    // projection does not.
    if !proj.order.is_empty() && proj.limit.is_none() {
        return None;
    }
    // Projection items read ONLY bound vars, non-aggregating, non-opaque — the
    // shared tail evaluates each per survivor via `eval_expr`.
    for it in &proj.items {
        if contains_opaque(&it.expr) || expr_has_aggregate(&it.expr) {
            return None;
        }
        let mut fv = Vec::new();
        free_vars_of(&it.expr, &mut fv);
        if !fv.iter().all(|v| chain.vars.contains(v)) {
            return None;
        }
    }
    Some(CorePlan {
        a_labels: chain.a_labels,
        a_var: chain.a_var,
        hops: chain.hops,
        vars: chain.vars,
        var_kinds: chain.var_kinds,
        wheres: chain.wheres,
        start_anchor: chain.start_anchor,
        proj: proj.clone(),
    })
}

// ─── Aggregation plan (group-by + aggregates) ────────────────────────────────

/// How one grouping key / count argument reads the bound vars — the single-var
/// class the columnar reduction can compute column-at-a-time.
enum SingleVar {
    /// A bare bound variable (index into `vars`): group by node IDENTITY,
    /// materialise the node only at output; `count(var)` counts every row (a
    /// bound node is never null).
    Node(usize),
    /// A single-var `eval_column`-vectorisable expression over that var (a
    /// `var.prop` or a boolean/comparison over it): group by / count the VALUE.
    Col(usize),
    /// No bound variable — a constant, folded once.
    Const,
}

/// Classify a grouping key / count argument as reading a single bound var (a
/// bare var, a `var.prop`-style column, or a const). A bare `Var` is not an
/// `eval_column` form, so it is handled here before delegating to `classify_key`
/// (which the ORDER BY / WHERE recognizers share). `None` = spans >1 var, or a
/// form the column path cannot evaluate — decline.
fn classify_single_var(
    e: &Expr,
    vars: &[String],
    props: &mut [BTreeSet<String>],
) -> Option<SingleVar> {
    if let Expr::Var(v) = e {
        return vars.iter().position(|x| x == v).map(SingleVar::Node);
    }
    match classify_key(e, vars, props)? {
        KeyRef::Const => Some(SingleVar::Const),
        KeyRef::Var(i) => Some(SingleVar::Col(i)),
    }
}

/// One grouping key of the aggregating projection, in projection-item order.
struct AggKey {
    kind: GroupKind,
    /// The original key expression — re-evaluated per group at output through
    /// the shared `eval_expr` (byte-identical to the per-tuple projector).
    expr: Expr,
}

/// How a grouping key contributes to the canonical group key and its output.
enum GroupKind {
    Node(usize),
    Col(usize),
    Const,
}

/// How one aggregate call SITE's single argument is computed column-natively per
/// live row, before folding it into that site's [`SiteAcc`]. Mirrors the grouping
/// key's single-var discipline: a star (`count(*)`), a bare bound var (the full
/// node, materialised once per distinct id), an `eval_column`-vectorisable
/// single-var expression (a `var.prop`-style value), or a const. Anything
/// spanning >1 var, or a form the column path cannot feed, DECLINES the whole
/// aggregate.
enum SiteArgPlan {
    /// `count(*)` — the star site folds `None` per live row.
    Star,
    /// A bare bound var (index into `vars`) — the full node value per row.
    Node(usize),
    /// An `eval_column`-vectorisable single-var expression + the var it reads.
    Col(usize, Expr),
    /// A constant argument — one broadcast value, folded per live row.
    Const(Expr),
}

/// Form A's WITH→RETURN payload (boxed in [`AggForm`] to keep the variants a
/// uniform size).
struct WithForm {
    with_proj: Projection,
    post_where: Option<Expr>,
    return_proj: Projection,
}

/// Which trailing shape carries the aggregating projection.
enum AggForm {
    /// Form B: the aggregating RETURN itself.
    Return(Box<Projection>),
    /// Form A: an aggregating WITH (no ORDER/SKIP/LIMIT), an optional post-WITH
    /// WHERE, then a plain RETURN over the WITH aliases.
    With(Box<WithForm>),
}

/// One reduced group: the ids of its first-seen representative row (one node id
/// per bound var) and the group's folded per-site accumulators — one [`SiteAcc`]
/// per [`AggSite`], in site order, pushed in PRODUCTION row order so the fold is
/// byte-identical to `run_streaming`'s.
type Group = (Vec<u64>, Vec<SiteAcc>);

/// The property COLUMNS `reduce_agg_groups` loaded to FORM the groups, handed to
/// `project_agg_groups` so it reuses them instead of re-gathering the same sparse
/// point-loads for the group-key PROJECTION. `vi -> (sorted distinct ids, prop ->
/// values aligned to those ids)`. Only Col group keys carry an entry (a Node key
/// groups on the raw id; a const has none).
pub(crate) type GroupKeyCols = BTreeMap<usize, (Vec<u64>, BTreeMap<String, Vec<Value>>)>;

/// A recognised group-by-AGGREGATE over the read chain: the chain, >=0 single-var
/// grouping keys (zero = a global aggregate), the aggregate SITES lifted from the
/// projection (`count`/`sum`/`avg`/`min`/`max`/`collect`, with DISTINCT, multiple
/// per projection, in compound expressions) each with its per-row argument plan,
/// the per-item projection plan, and the trailing shape (Form A or Form B).
/// DECLINES: a key/arg spanning >1 var, an arg the column path cannot feed, an
/// aggregate the streaming states do not model (`stdev`/`percentile*`/…), a
/// projection-level DISTINCT, a post-WITH second MATCH, or a WITH that
/// orders/pages its own groups.
struct AggPlan {
    a_labels: Vec<String>,
    a_var: String,
    hops: Vec<Hop>,
    vars: Vec<String>,
    /// Per-var KIND, parallel to `vars`.
    var_kinds: Vec<VarKind>,
    /// The WHERE as a CONJUNCTION of per-predicate filters (a scan anchor AND a
    /// textual WHERE both ride). Empty for no WHERE.
    wheres: Vec<WherePred>,
    /// The seekable scan-seed anchor from an inline `(a:L {id: val})` start, if
    /// any — carried from the read chain so `run_aggregate`/`run_distinct` seed
    /// the anchored id set instead of cloning the whole label member list.
    start_anchor: Option<PropAnchor>,
    /// The non-aggregate projection items, in order — may be empty (global).
    group_keys: Vec<AggKey>,
    /// The aggregate sites lifted from the projection, in projection order.
    sites: Vec<AggSite>,
    /// Per-site argument plan, aligned to `sites`.
    site_args: Vec<SiteArgPlan>,
    /// Per projection-item plan (grouping key vs aggregate-bearing expr), aligned
    /// to the aggregating projection's items.
    agg_items: Vec<AggItem>,
    form: AggForm,
}

/// Recognise a trailing aggregating projection over the read chain — the
/// group-by-AGGREGATE operator's plan. Two forms:
///   B: `MATCH <chain> [WHERE] RETURN <keys>, <aggregates> [ORDER BY] [SKIP]
///      [LIMIT]`
///   A: `MATCH <chain> [WHERE] WITH <keys>, <aggregates> [WHERE <post>]
///      RETURN <exprs over the WITH aliases> [ORDER BY] [SKIP] [LIMIT]`
/// Grouping keys are the NON-aggregate items (each a single var / column / const);
/// the aggregate SITES are `count`/`sum`/`avg`/`min`/`max`/`collect` — with
/// DISTINCT, multiple per projection, inside compound expressions — each over a
/// single-var / const / star argument the column path can feed. ZERO grouping
/// keys is a global aggregate (one group over all rows). Declines: an arg/key
/// spanning >1 var, an arg the column path cannot feed, an aggregate the
/// streaming states do not model, a projection-level DISTINCT, a second MATCH
/// after WITH, a WITH that orders/pages its own groups, or a Form-A RETURN that
/// aggregates again or reads a non-alias.
fn recognise_aggregate(sq: &SingleQuery) -> Option<AggPlan> {
    // Form B is `[Match, Return]`; Form A is `[Match, With, Return]`. The
    // aggregating projection is the RETURN (B) or the WITH (A).
    let (pattern, match_where, agg_proj, with_form): (
        _,
        _,
        &Projection,
        Option<(Option<&Expr>, &Projection)>,
    ) = match sq.clauses.as_slice() {
        [
            Clause::Match {
                optional: false,
                pattern,
                where_,
            },
            Clause::Return { proj },
        ] => (pattern, where_.as_ref(), proj, None),
        [
            Clause::Match {
                optional: false,
                pattern,
                where_,
            },
            Clause::With {
                proj: wp,
                where_: post,
            },
            Clause::Return { proj: rp },
        ] => (pattern, where_.as_ref(), wp, Some((post.as_ref(), rp))),
        _ => return None,
    };
    let chain = recognise_chain(pattern, match_where)?;
    // A var-length hop is tractable here only when the aggregating projection
    // consumes its end DISTINCT-only (`count(DISTINCT b)` / `collect(DISTINCT b)`
    // with `b` nowhere else); otherwise decline to the enumerating general path.
    if !varlen_distinct_consumed(&chain.hops, agg_proj) {
        return None;
    }
    let mut plan = aggregate_over_chain(chain, agg_proj, with_form)?;
    // The COUNT FOLD is planned HERE, on the one plan `run_aggregate` executes
    // through `build_chunk` (its `hops`/`wheres` ARE what runs), and not inside
    // `aggregate_over_chain`: the OPTIONAL / multistage / join composites also
    // build their tails through that function but execute their own hop lists,
    // so a fold marked there would move a WHERE off a list that never runs it.
    plan_count_fold(&mut plan);
    // `ENGRAM_TRACE_PLAN=1` also dumps the FOLD MARKS. The `[plan]` dump above
    // shows the path ORDER and nothing about which hops fold, and that gap was
    // where an hour of guessing went: LSQB q2's close costs ~1,000 ns/leaf on
    // the pod as a second path and ~50 ns as the same path's final hop, with
    // identical per-leaf counters, and no reading of the fold could say why.
    // Whether `person2` folds or MATERIALISES is the kind of fact this prints.
    if std::env::var_os("ENGRAM_TRACE_PLAN").is_some() {
        for (i, h) in plan.hops.iter().enumerate() {
            let name = |vi: usize| plan.vars.get(vi).map(String::as_str).unwrap_or("?");
            eprintln!(
                "[fold] hop {i}: {}-[{}]{}{} fold={} root_src={} track={} reset={} inline={} labels={:?}",
                name(h.src),
                h.types.join("|"),
                match h.tgt {
                    Some(t) => format!("->CLOSE {}", name(t)),
                    None => format!("->{}", h.end_vi.map(name).unwrap_or("?")),
                },
                if h.varlen.is_some() { " varlen" } else { "" },
                h.fold,
                name(h.src),
                h.track,
                h.reset,
                h.inline.len(),
                h.labels,
            );
        }
    }
    Some(plan)
}

/// Layer a trailing aggregating projection (Form B RETURN or Form A WITH→RETURN)
/// onto an already-recognised read chain (the single-MATCH chain, or the
/// OPTIONAL-MATCH combined chain). Grouping keys are the non-aggregate items
/// (each single var / column / const); the aggregate SITES are the modelled
/// functions over a single-var / const / star argument. `None` = DECLINE.
fn aggregate_over_chain(
    chain: Chain,
    agg_proj: &Projection,
    with_form: Option<(Option<&Expr>, &Projection)>,
) -> Option<AggPlan> {
    if agg_proj.star || agg_proj.distinct || agg_proj.items.is_empty() {
        return None;
    }
    // This projection must actually aggregate — else the non-aggregate CorePlan
    // owns it and routing here would be wrong.
    if !agg_proj.items.iter().any(|it| expr_has_aggregate(&it.expr)) {
        return None;
    }
    // Form A's WITH breaker carries no ORDER/SKIP/LIMIT (those live on the
    // RETURN); a WITH that orders/pages its own groups is deferred.
    if with_form.is_some()
        && (!agg_proj.order.is_empty() || agg_proj.skip.is_some() || agg_proj.limit.is_some())
    {
        return None;
    }

    // Lift the aggregate SITES + the per-item plan (grouping key vs aggregate
    // expr) EXACTLY as the per-tuple projector does, so their site indices align.
    let (sites, agg_items) = plan_agg_projection(agg_proj);

    // Grouping keys = the NON-aggregate items (those `plan_agg_projection` marked
    // `AggItem::Key`), each a single bound var / column / const the reduction can
    // compute column-at-a-time. A Key item that STILL carries an aggregate is one
    // the streaming states do not model (`stdev`/`percentile*`/a `f(DISTINCT …)`
    // that is not an aggregate) — it never became a site, so decline.
    let mut props: Vec<BTreeSet<String>> = vec![BTreeSet::new(); chain.vars.len()];
    let mut group_keys: Vec<AggKey> = Vec::new();
    let mut grouping_vars: BTreeSet<usize> = BTreeSet::new();
    for (it, plan) in agg_proj.items.iter().zip(&agg_items) {
        if matches!(plan, AggItem::Agg { .. }) {
            continue; // an aggregate-bearing item — validated via its sites below
        }
        if expr_has_aggregate(&it.expr) || contains_opaque(&it.expr) {
            return None; // an unmodelled aggregate, or an opaque grouping key
        }
        let kind = match classify_single_var(&it.expr, &chain.vars, &mut props)? {
            SingleVar::Node(vi) => {
                grouping_vars.insert(vi);
                GroupKind::Node(vi)
            }
            SingleVar::Col(vi) => {
                grouping_vars.insert(vi);
                GroupKind::Col(vi)
            }
            SingleVar::Const => GroupKind::Const,
        };
        group_keys.push(AggKey {
            kind,
            expr: it.expr.clone(),
        });
    }

    // Each aggregate SITE's argument: a star, or a single-var / const argument the
    // column path can feed. DISTINCT folds through the same column value.
    let mut site_args: Vec<SiteArgPlan> = Vec::with_capacity(sites.len());
    for site in &sites {
        if site.star {
            site_args.push(SiteArgPlan::Star);
            continue;
        }
        let arg = match site.args.as_slice() {
            [a] => a,
            _ => return None, // an aggregate with != 1 argument — decline
        };
        if contains_opaque(arg) {
            return None;
        }
        site_args.push(match classify_single_var(arg, &chain.vars, &mut props)? {
            SingleVar::Node(vi) => SiteArgPlan::Node(vi),
            SingleVar::Col(vi) => SiteArgPlan::Col(vi, arg.clone()),
            SingleVar::Const => SiteArgPlan::Const(arg.clone()),
        });
    }

    // An aggregate-bearing item's REWRITTEN expression (aggregates replaced by
    // `$__aggN`) is evaluated over the group TEMPLATE, which materialises ONLY the
    // grouping-key vars. Any remaining free var must therefore be a grouping var,
    // else the template lacks it and the columnar path would diverge — decline.
    // (For well-formed Cypher aggregation this always holds; it guards the rest.)
    for plan in &agg_items {
        if let AggItem::Agg { rewritten, .. } = plan {
            let mut fv = Vec::new();
            free_vars_of(rewritten, &mut fv);
            for v in &fv {
                match chain.vars.iter().position(|x| x == v) {
                    Some(vi) if grouping_vars.contains(&vi) => {}
                    _ => return None,
                }
            }
        }
    }

    let form = match with_form {
        None => AggForm::Return(Box::new(agg_proj.clone())),
        Some((post, rp)) => {
            // The RETURN over the WITH aliases: a plain projection (no aggregate,
            // no star) whose items read ONLY the WITH output aliases; the post-
            // WITH WHERE must not aggregate either.
            if rp.star {
                return None;
            }
            if let Some(w) = post {
                if expr_has_aggregate(w) || contains_opaque(w) {
                    return None;
                }
            }
            let aliases: BTreeSet<String> = agg_proj
                .items
                .iter()
                .enumerate()
                .map(|(i, it)| {
                    it.alias
                        .clone()
                        .or_else(|| it.text.clone())
                        .unwrap_or_else(|| column_name(&it.expr, i))
                })
                .collect();
            for it in &rp.items {
                if expr_has_aggregate(&it.expr) {
                    return None;
                }
                let mut fv = Vec::new();
                free_vars_of(&it.expr, &mut fv);
                if !fv.iter().all(|v| aliases.contains(v)) {
                    return None;
                }
            }
            AggForm::With(Box::new(WithForm {
                with_proj: agg_proj.clone(),
                post_where: post.cloned(),
                return_proj: rp.clone(),
            }))
        }
    };

    Some(AggPlan {
        a_labels: chain.a_labels,
        a_var: chain.a_var,
        hops: chain.hops,
        vars: chain.vars,
        var_kinds: chain.var_kinds,
        wheres: chain.wheres,
        start_anchor: chain.start_anchor,
        group_keys,
        sites,
        site_args,
        agg_items,
        form,
    })
}

// ─── DISTINCT projection (group-by with zero aggregate sites) ─────────────────

/// Recognise a trailing `MATCH <chain> [WHERE] RETURN DISTINCT <items> [ORDER BY]
/// [SKIP] [LIMIT]` — the DISTINCT projection as a GROUP-BY with ZERO aggregate
/// sites. Only the single-MATCH Form-B shape; a `WITH DISTINCT … RETURN`
/// (multi-stage) has a different clause shape and declines here. `None` = DECLINE
/// (not DISTINCT, or a shape `distinct_over_chain` cannot own).
fn recognise_distinct(sq: &SingleQuery) -> Option<AggPlan> {
    let (pattern, where_opt, proj, with_form): (_, _, &Projection, Option<(Option<&Expr>, &Projection)>) =
        match sq.clauses.as_slice() {
            [
                Clause::Match {
                    optional: false,
                    pattern,
                    where_,
                },
                Clause::Return { proj },
            ] => (pattern, where_.as_ref(), proj, None),
            // Fix 29: `MATCH … WITH DISTINCT <keys> [WHERE] RETURN <over the
            // keys> [ORDER BY … LIMIT …]` — the Form-A DISTINCT tail that only
            // the multistage tails reached. The KMProject listing (`… WITH
            // DISTINCT lore RETURN lore.orgId, … ORDER BY lore.repoId LIMIT
            // toInteger($limit)`) ran on the general path: two stages, every
            // TRACKS_REPO relationship decoded in full, each repo projected
            // twice — 2.1–2.6 ms on the mirror against Neo4j's 1.2.
            [
                Clause::Match {
                    optional: false,
                    pattern,
                    where_,
                },
                Clause::With {
                    proj: wp,
                    where_: post,
                },
                Clause::Return { proj: rp },
            ] if wp.distinct => (pattern, where_.as_ref(), wp, Some((post.as_ref(), rp))),
            _ => return None,
        };
    if !proj.distinct {
        return None;
    }
    let chain = recognise_chain(pattern, where_opt)?;
    // `RETURN DISTINCT <items>` consumes every plain var it carries DISTINCT-only,
    // so a var-length end var projected here qualifies; a var-length hop whose end
    // is NOT among them declines to the general path.
    if !varlen_distinct_consumed(&chain.hops, proj) {
        return None;
    }
    match with_form {
        None => distinct_over_chain(chain, proj),
        Some((having, rp)) => {
            // A var-length chain stays with the general path here: its
            // frontier BFS is a declared state the sim sweep must reach
            // (`MATCH (a:BX {i: 900})-[:BR*1..2]->(b:BX) WITH DISTINCT b
            // RETURN b.i` is the sweep's only statement that reaches it),
            // and no production shape pairs a var-length hop with this
            // tail. The `RETURN DISTINCT` form above keeps its var-length
            // acceptance as before.
            if hops_have_varlen(&chain.hops) {
                return None;
            }
            let plan = distinct_form_a_over_chain(chain, proj, having, rp)?;
            counted!("interp.pipeline distinct WITH tail recognised at the top level");
            Some(plan)
        }
    }
}

/// Layer a trailing DISTINCT projection onto an already-recognised read chain, as
/// a GROUP-BY with ZERO aggregate sites: `RETURN DISTINCT <items>` groups the
/// rows by ALL projected items (the items ARE the grouping keys), keeps ONE row
/// per distinct key-tuple in FIRST-SEEN order, then applies ORDER BY / SKIP /
/// LIMIT. Each item becomes an `AggKey`: a bare node/rel var → an IDENTITY key
/// (`GroupKind::Node`, deduped on the raw id — the u64 fast path in
/// `reduce_agg_groups`); a `var.prop`-style single-var expression → an
/// `eval_column` VALUE key (`GroupKind::Col`); a const → `GroupKind::Const`.
/// There are NO sites (`plan_agg_projection` lifts zero, marking every item a
/// grouping `AggItem::Key`), so a group IS just its key tuple and
/// `project_agg_groups` emits the items over each group's first-seen
/// representative bindings — dedup-before-LIMIT, byte-identical to `run_streaming`.
///
/// `None` = DECLINE (the general path answers identically): an item that
/// aggregates or is opaque, an item spanning >1 var / a form the column path
/// cannot feed, or an ORDER BY key `project_agg_groups` would not evaluate
/// identically to the per-tuple full-row eval.
fn distinct_over_chain(chain: Chain, proj: &Projection) -> Option<AggPlan> {
    if proj.star || !proj.distinct || proj.items.is_empty() {
        return None;
    }

    // Each projected item is a grouping key — a single bound var / column / const
    // the reduction can compute column-at-a-time. An item that AGGREGATES (a
    // `DISTINCT` over an aggregate) is not this path's concern; decline so the
    // general path owns it. `whole_vars` records the vars projected as a bare,
    // UNALIASED entity — the whole node/rel is then an output column named for the
    // var, so an ORDER BY key reading it (`b.bx`) resolves over the output.
    let mut props: Vec<BTreeSet<String>> = vec![BTreeSet::new(); chain.vars.len()];
    let mut group_keys: Vec<AggKey> = Vec::with_capacity(proj.items.len());
    let mut whole_vars: BTreeSet<usize> = BTreeSet::new();
    for it in &proj.items {
        if expr_has_aggregate(&it.expr) || contains_opaque(&it.expr) {
            return None;
        }
        let kind = match classify_single_var(&it.expr, &chain.vars, &mut props)? {
            SingleVar::Node(vi) => {
                if it.alias.is_none() || it.alias.as_deref() == Some(chain.vars[vi].as_str()) {
                    whole_vars.insert(vi);
                }
                GroupKind::Node(vi)
            }
            SingleVar::Col(vi) => GroupKind::Col(vi),
            SingleVar::Const => GroupKind::Const,
        };
        group_keys.push(AggKey {
            kind,
            expr: it.expr.clone(),
        });
    }

    // ORDER BY: `project_agg_groups` resolves each key either by MATCHING a
    // projected item's expression (then the key IS the projected value —
    // identical to the full-row eval) or by evaluating over the projected OUTPUT
    // columns. The latter matches the per-tuple full-row eval only when every free
    // var of the key is a WHOLE entity in the output. Decline anything else, so
    // the general path (which evaluates ORDER BY over the full row) owns it —
    // byte-identical, never wrong.
    for o in &proj.order {
        if contains_opaque(&o.expr) || expr_has_aggregate(&o.expr) {
            return None;
        }
        if proj.items.iter().any(|it| it.expr == o.expr) {
            continue; // an item-match — uses the projected value directly
        }
        let mut fv = Vec::new();
        free_vars_of(&o.expr, &mut fv);
        let resolvable = fv.iter().all(|v| {
            chain
                .vars
                .iter()
                .position(|x| x == v)
                .is_some_and(|vi| whole_vars.contains(&vi))
        });
        if !resolvable {
            return None;
        }
    }

    // A DISTINCT projection carries no aggregate, so `plan_agg_projection` marks
    // every item `AggItem::Key` and lifts ZERO sites — the pure-DISTINCT plan.
    let (sites, agg_items) = plan_agg_projection(proj);
    debug_assert!(
        sites.is_empty(),
        "a DISTINCT projection has no aggregate site"
    );

    Some(AggPlan {
        a_labels: chain.a_labels,
        a_var: chain.a_var,
        hops: chain.hops,
        vars: chain.vars,
        var_kinds: chain.var_kinds,
        wheres: chain.wheres,
        start_anchor: chain.start_anchor,
        group_keys,
        sites,
        site_args: Vec::new(),
        agg_items,
        form: AggForm::Return(Box::new(proj.clone())),
    })
}

/// A Form-A DISTINCT tail: `WITH DISTINCT <keys> [WHERE having] RETURN <proj>` —
/// group by the DISTINCT keys (ZERO aggregate sites, like `distinct_over_chain`)
/// then project the RETURN over those keys (like `aggregate_over_chain`'s Form A).
/// This is the varlen-split's stage-2 tail (`WITH DISTINCT friend RETURN
/// friend.name`), which neither the pure-DISTINCT (RETURN DISTINCT) nor the
/// aggregate (needs a site) path owns. `None` = DECLINE.
fn distinct_form_a_over_chain(
    chain: Chain,
    with_proj: &Projection,
    having: Option<&Expr>,
    rp: &Projection,
) -> Option<AggPlan> {
    if !with_proj.distinct || with_proj.star || with_proj.items.is_empty() {
        return None;
    }
    // The WITH DISTINCT items are the group keys — each a single bound var /
    // column / const, no aggregate.
    let mut props: Vec<BTreeSet<String>> = vec![BTreeSet::new(); chain.vars.len()];
    let mut group_keys: Vec<AggKey> = Vec::with_capacity(with_proj.items.len());
    for it in &with_proj.items {
        if expr_has_aggregate(&it.expr) || contains_opaque(&it.expr) {
            return None;
        }
        let kind = match classify_single_var(&it.expr, &chain.vars, &mut props)? {
            SingleVar::Node(vi) => GroupKind::Node(vi),
            SingleVar::Col(vi) => GroupKind::Col(vi),
            SingleVar::Const => GroupKind::Const,
        };
        group_keys.push(AggKey {
            kind,
            expr: it.expr.clone(),
        });
    }
    // The RETURN over the WITH aliases: a plain projection reading ONLY those
    // aliases; the HAVING must not aggregate (mirrors aggregate_over_chain Form A).
    if rp.star {
        return None;
    }
    if let Some(w) = having {
        if expr_has_aggregate(w) || contains_opaque(w) {
            return None;
        }
    }
    let aliases: BTreeSet<String> = with_proj
        .items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            it.alias
                .clone()
                .or_else(|| it.text.clone())
                .unwrap_or_else(|| column_name(&it.expr, i))
        })
        .collect();
    for it in &rp.items {
        if expr_has_aggregate(&it.expr) {
            return None;
        }
        let mut fv = Vec::new();
        free_vars_of(&it.expr, &mut fv);
        if !fv.iter().all(|v| aliases.contains(v)) {
            return None;
        }
    }
    let (sites, agg_items) = plan_agg_projection(with_proj);
    if !sites.is_empty() {
        return None; // a DISTINCT WITH lifts no site; guard the invariant
    }
    Some(AggPlan {
        a_labels: chain.a_labels,
        a_var: chain.a_var,
        hops: chain.hops,
        vars: chain.vars,
        var_kinds: chain.var_kinds,
        wheres: chain.wheres,
        start_anchor: chain.start_anchor,
        group_keys,
        sites,
        site_args: Vec::new(),
        agg_items,
        form: AggForm::With(Box::new(WithForm {
            with_proj: with_proj.clone(),
            post_where: having.cloned(),
            return_proj: rp.clone(),
        })),
    })
}

// ─── Plan build + run ────────────────────────────────────────────────────────

/// One ORDER BY key precomputed as a distinct-aligned value column over the one
/// var it reads (or a single broadcast constant), ready to gather per row.
enum KeyCol {
    Const(Value),
    /// (var index, distinct-aligned value column for that var).
    Var(usize, Vec<Value>),
}

/// The composable columnar operator: recognise the core read chain (single- OR
/// multi-hop, single- OR multi-path) and run `scan -> expand* -> [filter] ->
/// project`, expanding each step from its source var. Byte-identical
/// to the per-tuple `run_streaming` path on every accepted shape, or `Ok(None)`
/// (the general path answers identically). Gated on `columnar_scans_enabled()`;
/// runs FIRST among the columnar attempts, so the `try_vectorized_*` recognizers
/// remain a proven fallback for shapes this declines.
pub(crate) fn plan_and_run_columnar(
    graph: &Graph,
    q: &SingleQuery,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    if !graph.columnar_scans_enabled() {
        return Ok(None);
    }
    // NORMALISATION PRE-PASS: rewrite a shape the recognizers decline into an
    // equivalent one they accept (a RENAMED collect-unwind → its same-name form),
    // then re-plan the rewritten query. The rewrite is byte-identical (a
    // renaming), and the pass declines the rewritten query, so there is no loop;
    // if the rewritten query still declines, we return None and the caller's
    // interp answers the ORIGINAL identically.
    if let Some((rewritten, rw_params)) = normalise_for_columnar(graph, q, params)? {
        return plan_and_run_columnar(graph, &rewritten, &rw_params);
    }
    // ANCHOR SELECTION PRE-PASS: drive the scan from the SELECTIVE end of the
    // pattern rather than from whichever end was typed first. See
    // [`reroot_to_selective_end`] for the measurement and the guards. Like the
    // normalisation above this re-plans the rewritten query, and like it there
    // is no loop: the rewritten start carries the inline map that the rewrite
    // requires to be ABSENT, so the pass declines its own output.
    if let Some(rerooted) = reroot_to_selective_end(graph, q) {
        return plan_and_run_columnar(graph, &rerooted, params);
    }
    // COUNT-ONLY JOIN REORDER (operator C): a `count(*)`-only MATCH returns one
    // row whose content is fixed however the pattern is walked, so its paths may
    // be re-rooted, re-ordered and reversed freely. See
    // [`reorder_for_count_only`] for the admission rule (strictly fewer
    // materialised columns) and for why re-planning the rewrite terminates: the
    // pass is idempotent and declines a rewrite equal to its input.
    if let Some(reordered) = reorder_for_count_only(graph, q) {
        return plan_and_run_columnar(graph, &reordered, params);
    }
    // CONSTANT PROJECTION OVER THE COUNT: `MATCH … RETURN <literals/params>
    // [SKIP] [LIMIT]` emits the SAME row per match, so the reorder above's
    // argument holds for it too — only the number of copies is observable, and
    // that is the count. See [`constant_projection_over_count`] for the
    // admission rule and why the replay is byte-identical.
    if let Some(r) = constant_projection_over_count(graph, q, params)? {
        return Ok(Some(r));
    }
    // The FULL 7-clause LDBC IC5: `MATCH <varlen> WITH DISTINCT <v> MATCH <relWHERE>
    // WITH <collect> OPTIONAL MATCH <correlated> WITH <count> RETURN <topk>`. Its
    // clause shape (7 clauses, two aggregate WITHs straddling an OPTIONAL) is
    // DISJOINT from every recognizer below (all fewer clauses / no interior
    // OPTIONAL), so trying it first claims only this composite and leaves the rest.
    if let Some(ic5) = recognise_ic5(q) {
        return run_ic5(graph, &ic5, params);
    }
    // A MATCH … OPTIONAL MATCH … [WITH|RETURN] left join (Phase 4b2). Its clause
    // shape ([Match, OPTIONAL Match, …]) is disjoint from the single-MATCH
    // aggregate/core recognizers below, so trying it first only claims the
    // OPTIONAL shapes and leaves everything else to them.
    if let Some(opt) = recognise_optional(q) {
        return run_optional(graph, &opt, params);
    }
    // A TWO-STAGE `MATCH … WITH [DISTINCT] <vars> MATCH … RETURN …` read (the
    // IC5 stage-1→stage-2 shape). Its clause shape ([Match, With, Match, Return])
    // is disjoint from every recognizer below, so trying it here claims only the
    // multi-stage shape and leaves the single-stage ones untouched.
    if let Some(ms) = recognise_multistage(q) {
        // IC11's anchored-endpoint semijoin answers the friends-work-in-a-country
        // shape without materialising every friend→company→country row; it declines
        // (Ok(None)) on any other shape, so `run_multistage` is the unchanged fallback.
        if graph.ic11_semijoin_enabled() {
            if let Some(r) = try_ic11_semijoin(graph, &ms, params)? {
                return Ok(Some(r));
            }
        }
        return run_multistage(graph, &ms, params);
    }
    // A DISTINCT relational stage feeding a fused projection + group-by aggregate
    // with NO further graph traversal: `MATCH <chain> WITH DISTINCT <carry> (WITH
    // <projection>)* WITH <aggregate> [WHERE <having>] RETURN <top-k>` (IC4). Its
    // clause shape ([Match, With, With, (With)* Return] — ≥2 WITHs, no MATCH after
    // the first) is DISJOINT from `recognise_multistage` (a MATCH in slot 3) and
    // `recognise_aggregate` (one WITH). Reuses `run_multistage` with EMPTY stage-2
    // hops; the middle projection WITHs fuse into the aggregate by substitution.
    if let Some(pa) = recognise_projected_aggregate(q) {
        return run_multistage(graph, &pa, params);
    }
    // The FULL LDBC IC5 shape: `MATCH <chain1> [WHERE] WITH [DISTINCT] <var> MATCH
    // <chainA> [WHERE] MATCH <chainB> [WHERE] RETURN <group-by aggregate> ORDER BY
    // <total> [LIMIT]` — a two-stage read whose STAGE 2 is itself a two-MATCH
    // conjunctive JOIN. Its clause shape ([Match, With, Match, Match, Return]) is
    // DISJOINT from every recognizer above (OPTIONAL's 2nd Match is `optional`;
    // multistage is 4 clauses) and below (all fewer/other shapes), so trying it
    // here claims only this composite and leaves the rest untouched. It composes
    // the multistage stage-1→WITH boundary with the set-based hash join, SEEDING
    // chainA from the carried set (NOT a fresh label scan) — byte-identical to the
    // nested `run_streaming` ONLY on the shapes it accepts (else it declines).
    if let Some(mj) = recognise_multistage_join(q) {
        return run_multistage_join(graph, &mj, params);
    }
    // A SET-BASED HASH-JOIN for a two-MATCH conjunctive join: `MATCH <chainA>
    // MATCH <chainB> [WHERE] <group-by aggregate> ORDER BY <total> [LIMIT]`,
    // where chainB shares an already-bound var with chainA (Cypher comma-join
    // semantics). Its clause shape ([Match(non-opt), Match(non-opt), <tail>]) is
    // DISJOINT from every recognizer above (OPTIONAL's 2nd Match is `optional`;
    // multistage interposes a WITH) and below (all single-MATCH), so trying it
    // here claims only the two-MATCH join and leaves the rest untouched. It
    // executes the nested-loop join as a hash join (O(N+M)), then feeds the
    // EXISTING group-by/aggregate + ORDER BY/LIMIT tail — byte-identical to
    // `run_streaming` ONLY on the order-insensitive-aggregate + total-order shapes
    // it accepts (else it declines, `recognise_join`).
    if let Some(jn) = recognise_join(q) {
        return run_join(graph, &jn, params);
    }
    // A GENERAL N-stage pipeline: `MATCH … (WITH … MATCH …)+ <tail>` with ≥3
    // MATCHes — DISJOINT from `recognise_multistage` (2 MATCHes) and the
    // multistage-join / IC5 composites (consecutive MATCHes / an interior
    // OPTIONAL, which its strict `WITH`/`MATCH` alternation declines), all tried
    // above. This is what IC3 becomes once the prelude / seed-filter / varlen-split
    // / collect-list rewrites compose.
    if let Some(pl) = recognise_pipeline(q) {
        return run_pipeline(graph, &pl, params);
    }
    // A trailing group-by-aggregate over the chain — tried first, so an
    // aggregating shape routes here rather than falling to the non-aggregate
    // recognizer (which declines aggregates) and then batch.rs.
    if let Some(agg) = recognise_aggregate(q) {
        return run_aggregate(graph, &agg, params);
    }
    // A trailing DISTINCT projection over the chain — `RETURN DISTINCT <items>`
    // is a GROUP-BY with ZERO aggregate sites (the items ARE the grouping keys),
    // so the dedup is column-native (`reduce_agg_groups`) and emits ONE row per
    // distinct key-tuple in first-seen order, then the shared ORDER BY/SKIP/LIMIT
    // tail. Tried before the non-aggregate core (which declines DISTINCT), so a
    // DISTINCT projection never materialises every row through the project tail.
    if let Some(dp) = recognise_distinct(q) {
        return run_distinct(graph, &dp, params);
    }
    let Some(plan) = recognise_core(q) else {
        return Ok(None);
    };

    // IC2's date-ordered k-way merge answers the "recent messages from my friends"
    // shape without the scattered property gather; it declines (Ok(None)) on any
    // other shape, so the ordinary core execution below is the unchanged fallback.
    if graph.ic2_ordered_enabled() {
        if let Some(r) = try_ic2_ordered(graph, &plan, params)? {
            return finish(r);
        }
    }

    let empty_out =
        |g: &Graph| project_rows_tail(g, &plan.proj, params, &plan.vars, &plan.var_kinds, &[]);

    // SCAN + EXPAND + FILTER through the SAME chunk builder the aggregate,
    // DISTINCT and multi-stage paths use (`build_chunk`): the seed's own
    // predicates — the inline `{userId: $u}` anchor included — are answered
    // from the property-column cache (or a seek) BEFORE anything expands, and
    // every other predicate applies at the earliest hop that binds its vars.
    // This path used to seed from `anchored_seed_ids`, expand EVERY seed and
    // only then apply the whole WHERE row by row: on the mirror `MATCH
    // (t:ResearchTask {userId: $u})-[:PROPOSED_GRAPH_WRITE]->(p:GraphWrite
    // Proposal {status: 'pending'}) RETURN p.id … LIMIT 25` walked all 517
    // tasks' adjacency and read 729 records per statement (6.7 ms against
    // Neo4j's 2.8) while the count over the same chain filtered its 416 seeds
    // from the cached column. Byte-identical: each predicate applied earlier
    // keeps the row set and its production order (a source's rows are a
    // contiguous block), an empty seed or an unminted hop type seeds an empty
    // chunk that carries every var column, and a decline is still `None`.
    let Some(chunk) = build_chunk(
        graph,
        &plan.a_labels,
        &plan.a_var,
        &plan.hops,
        &plan.wheres,
        plan.start_anchor.as_ref(),
        params,
    )?
    else {
        return Ok(None); // a budget / non-boolean decline
    };
    debug_assert!(chunk.live() <= chunk.row_count());

    // PROJECT.
    //
    // The FULL project path (all live rows → the shared tail) is taken when there
    // is no ORDER BY. (A DISTINCT projection never reaches here — it is owned by
    // the group-by DISTINCT path above, which dedups column-natively before the
    // tail rather than materialising every live row.)
    if plan.proj.order.is_empty() {
        // No ORDER BY: emit live rows in production order; the tail sorts (if
        // ORDER BY, which is empty here) and applies SKIP/LIMIT.
        let rows = chunk.live_rows();
        return finish(project_rows_tail(
            graph,
            &plan.proj,
            params,
            &plan.vars,
            &plan.var_kinds,
            &rows,
        )?);
    }
    // ORDER BY + LIMIT (non-DISTINCT): bounded top-k over the live rows, then
    // late-materialise the <= cap winners through the shared tail.
    if chunk.live() == 0 {
        return finish(empty_out(graph)?);
    }
    match native_topk(graph, params, &plan, &chunk)? {
        Some(r) => finish(r),
        None => Ok(None), // a budget / type decline while loading key columns
    }
}

/// A DataChunk-native bounded top-k over the LIVE rows for a project with an
/// ORDER BY and a LIMIT, mirroring `finish_topk` over N id columns instead of
/// pairs. For each ORDER BY key it resolves the ONE var it reads (or a const),
/// loads that var's props over its DISTINCT live ids and precomputes the key as
/// a value column; then a bounded heap keyed by (ORDER BY key tuple, production
/// `seq`) keeps the `cap` smallest — `seq` being the row's position in the live
/// selection (= production order) — and the winners late-materialise through
/// `project_rows_tail` (which re-sorts stable, same order, and applies
/// SKIP/LIMIT). `Ok(None)` = a column-budget / type decline; the caller returns
/// None and the general path answers identically.
/// Build (or reuse) the date-ordered per-creator message index: each creator's
/// messages sorted `(date DESC, id ASC)` with the ORDER BY keys carried inline, so
/// the merge never touches the store to rank. Loaded via ONE range scan of the
/// whole date+id columns (fast — the full family, not a scattered subset), grouped
/// by creator through the reverse rel adjacency, cached per epoch. This is a
/// prototype of ordering the adjacency at seal — the true native form.
fn creator_sorted_messages(
    graph: &Graph,
    msg_labels: &[String],
    date_prop: &str,
    creator_types: &[String],
) -> Result<Option<std::sync::Arc<crate::CreatorMsgs>>, RunError> {
    if let Some(c) = graph.creator_msgs_get() {
        return Ok(Some(c));
    }
    let members = graph.members_all(msg_labels).map_err(RunError::Graph)?.to_arc_vec();
    let mut map: crate::CreatorMsgs = crate::CreatorMsgs::new();
    if !members.is_empty() {
        let mut props: BTreeSet<String> = BTreeSet::new();
        props.insert(date_prop.to_string());
        props.insert("id".to_string());
        let Some(cols) = load_var_columns(graph, VarKind::Node, &members[..], &props)? else {
            return Ok(None); // load declined — fall back to the ordinary path
        };
        let (Some(date_col), Some(id_col)) = (cols.get(date_prop), cols.get("id")) else {
            return Ok(None);
        };
        let creator_tokens = graph.type_tokens_peek(creator_types);
        for (i, &msg) in members.iter().enumerate() {
            let date = match &date_col[i] {
                Value::Int(x) => *x,
                Value::Date(x) => *x,
                _ => continue,
            };
            // `mid` is ONLY a sort tiebreaker (the seek ignores it, and the
            // aggregate is order-independent) — so a message without an `id`
            // property must NOT be dropped, or the fast path silently counts
            // fewer messages than the general path. Fall back to the node id,
            // which is always present and equally deterministic.
            let mid = match &id_col[i] {
                Value::Int(x) => *x,
                _ => msg as i64,
            };
            let mut creator: Option<u64> = None;
            graph.adjacent_slim_for_each(msg, Dir::Out, &creator_tokens, |e| {
                if creator.is_none() {
                    creator = Some(e.peer);
                }
            });
            if let Some(c) = creator {
                map.entry(c).or_default().push((date, mid, msg));
            }
        }
        for v in map.values_mut() {
            v.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
    }
    let arc = std::sync::Arc::new(map);
    graph.creator_msgs_set(std::sync::Arc::clone(&arc));
    Ok(Some(arc))
}

/// Resolve a RETURN-alias reference to the expression it names (else the expr as-is).
fn resolve_alias<'a>(e: &'a Expr, items: &'a [engram_cypher::stmt::ProjItem]) -> &'a Expr {
    if let Expr::Var(name) = e {
        for it in items {
            if it.alias.as_deref() == Some(name.as_str()) {
                return &it.expr;
            }
        }
    }
    e
}

/// The date property of the FIRST ORDER BY key iff it is `message.<prop>` DESC.
fn ic2_date_order(proj: &Projection, message_var: &str) -> Option<String> {
    let o = proj.order.first()?;
    if !o.desc {
        return None;
    }
    if let Expr::Prop(base, prop) = resolve_alias(&o.expr, &proj.items) {
        if matches!(base.as_ref(), Expr::Var(v) if v == message_var) {
            return Some(prop.clone());
        }
    }
    None
}

/// The upper bound `T` from a `message.<date_prop> <= T` WHERE conjunct, if present.
fn ic2_date_upper_bound(
    wheres: &[WherePred],
    message_var: &str,
    date_prop: &str,
    graph: &Graph,
    params: &BTreeMap<String, Value>,
) -> Result<Option<i64>, RunError> {
    for w in wheres {
        if let Expr::Bin(engram_cypher::ast::BinOp::Le, l, r) = &w.expr {
            let reads_date = matches!(l.as_ref(), Expr::Prop(b, p)
                if p == date_prop && matches!(b.as_ref(), Expr::Var(v) if v == message_var));
            if reads_date {
                let empty_vm = VarMap::new();
                let scope = Scope::over(params, &empty_vm, graph.wall_ms(), graph.zone_provider());
                match eval_with(r, &scope, None).map_err(RunError::Eval)? {
                    Value::Int(x) => return Ok(Some(x)),
                    Value::Date(x) => return Ok(Some(x)),
                    _ => return Ok(None),
                }
            }
        }
    }
    Ok(None)
}

/// IC2's date-ordered k-way merge. Detects the CorePlan shape `(:A{id})-[:KNOWS]-
/// (friend)<-[:HAS_CREATOR]-(message) WHERE message.<date> <= T ORDER BY
/// message.<date> DESC, … LIMIT k` and answers it from the date-ordered per-creator
/// index: seed the anchor, KNOWS→friends, pull each friend's newest `cap` messages
/// with date<=T from the sorted stream (keys inline — NO store gather to rank), keep
/// the global top-`cap`, and project ONLY those through the shared tail. `Ok(None)`
/// = the shape does not match. Byte-identical to the ordinary expand+gather+top-k.
fn try_ic2_ordered(
    graph: &Graph,
    plan: &CorePlan,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    if plan.hops.len() != 2 || plan.start_anchor.is_none() || plan.proj.distinct {
        return Ok(None);
    }
    let (h_knows, h_creator) = (&plan.hops[0], &plan.hops[1]);
    if h_knows.types != ["KNOWS"]
        || h_creator.types != ["HAS_CREATOR"]
        || h_knows.varlen.is_some()
        || h_creator.varlen.is_some()
        || h_knows.rel_var.is_some()
        || h_creator.rel_var.is_some()
        || !matches!(h_creator.dir, Dir::In)
    {
        return Ok(None);
    }
    let (Some(a_vi), Some(friend_vi), Some(message_vi)) = (
        plan.vars.iter().position(|v| *v == plan.a_var),
        plan.vars.iter().position(|v| *v == h_knows.var),
        plan.vars.iter().position(|v| *v == h_creator.var),
    ) else {
        return Ok(None);
    };
    if h_knows.src != a_vi || plan.proj.limit.is_none() {
        return Ok(None);
    }
    let Some(date_prop) = ic2_date_order(&plan.proj, &h_creator.var) else {
        return Ok(None);
    };
    let Some(t_bound) =
        ic2_date_upper_bound(&plan.wheres, &h_creator.var, &date_prop, graph, params)?
    else {
        return Ok(None);
    };

    let Some(index) =
        creator_sorted_messages(graph, &h_creator.labels, &date_prop, &h_creator.types)?
    else {
        return Ok(None);
    };

    // Anchor person + its KNOWS friends (label-filtered, distinct).
    let (persons, _) = anchored_seed_ids(graph, &plan.a_labels, plan.start_anchor.as_ref(), params)?;
    let knows_tokens = graph.type_tokens_peek(&h_knows.types);
    // Collect friends WITH KNOWS-edge MULTIPLICITY — do NOT dedup. A single-hop
    // `(person)-[:KNOWS]-(friend)` is undirected, and SNB stores KNOWS as a pair of
    // directed edges, so a mutual friend is matched TWICE and every one of its
    // messages appears twice in the result (the general expand does exactly this).
    // Deduping here would drop those duplicate rows and diverge from Neo4j at the
    // LIMIT boundary. (IC11's `KNOWS*1..2` is distinct reachability, so it dedups.)
    let mut friends: Vec<u64> = Vec::new();
    for &p in &persons {
        graph.adjacent_slim_for_each(p, h_knows.dir, &knows_tokens, |e| friends.push(e.peer));
    }
    if !h_knows.labels.is_empty() {
        let fm = graph
            .members_all(&h_knows.labels)
            .map_err(RunError::Graph)?;
        friends.retain(|f| fm.contains(*f));
    }

    let skip = eval_count(graph, plan.proj.skip.as_ref(), params, "SKIP")?.unwrap_or(0);
    let limit = eval_count(graph, plan.proj.limit.as_ref(), params, "LIMIT")?.unwrap_or(0);
    let cap = skip.saturating_add(limit);

    // Each friend's newest `cap` messages with date <= T (a suffix of the
    // date-DESC stream), then the global top-`cap` by (date DESC, id ASC).
    let mut cands: Vec<(i64, i64, u64, u64)> = Vec::new(); // (date, id, friend, msg)
    for &f in &friends {
        if let Some(msgs) = index.get(&f) {
            let start = msgs.partition_point(|&(d, _, _)| d > t_bound);
            for &(d, mid, node) in msgs[start..].iter().take(cap) {
                cands.push((d, mid, f, node));
            }
        }
    }
    cands.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    cands.truncate(cap);

    counted!("interp.pipeline ic2 ordered merge");
    let person = persons.first().copied().unwrap_or(NULL_ID);
    let mut rows: Vec<Vec<u64>> = Vec::with_capacity(cands.len());
    for &(_, _, f, msg) in &cands {
        let mut row = vec![NULL_ID; plan.vars.len()];
        row[a_vi] = person;
        row[friend_vi] = f;
        row[message_vi] = msg;
        rows.push(row);
    }
    Ok(Some(project_rows_tail(
        graph,
        &plan.proj,
        params,
        &plan.vars,
        &plan.var_kinds,
        &rows,
    )?))
}

/// A `var.<prop> = <const>` conjunct in `wheres`, if present — the property and its
/// resolved value (the endpoint anchor, e.g. `country.name = 'Country0'`).
fn ic11_eq_anchor(
    wheres: &[WherePred],
    var: &str,
    graph: &Graph,
    params: &BTreeMap<String, Value>,
) -> Result<Option<(String, Value)>, RunError> {
    for w in wheres {
        if let Expr::Bin(engram_cypher::ast::BinOp::Eq, l, r) = &w.expr {
            if let Expr::Prop(b, p) = l.as_ref() {
                if matches!(b.as_ref(), Expr::Var(v) if v == var) {
                    let empty_vm = VarMap::new();
                    let scope =
                        Scope::over(params, &empty_vm, graph.wall_ms(), graph.zone_provider());
                    let val = eval_with(r, &scope, None).map_err(RunError::Eval)?;
                    return Ok(Some((p.clone(), val)));
                }
            }
        }
    }
    Ok(None)
}

/// A `var.<prop> < <const>` conjunct in `wheres`, if present — the property and the
/// integer bound (e.g. `workAt.workFrom < 2015`).
fn ic11_lt_bound(
    wheres: &[WherePred],
    var: &str,
    graph: &Graph,
    params: &BTreeMap<String, Value>,
) -> Result<Option<(String, i64)>, RunError> {
    for w in wheres {
        if let Expr::Bin(engram_cypher::ast::BinOp::Lt, l, r) = &w.expr {
            if let Expr::Prop(b, p) = l.as_ref() {
                if matches!(b.as_ref(), Expr::Var(v) if v == var) {
                    let empty_vm = VarMap::new();
                    let scope =
                        Scope::over(params, &empty_vm, graph.wall_ms(), graph.zone_provider());
                    return match eval_with(r, &scope, None).map_err(RunError::Eval)? {
                        Value::Int(x) => Ok(Some((p.clone(), x))),
                        _ => Ok(None),
                    };
                }
            }
        }
    }
    Ok(None)
}

/// IC11's ANCHORED-ENDPOINT SEMIJOIN. The shape is `(:P{id})-[:KNOWS*1..2]-(friend)
/// WITH DISTINCT friend MATCH (friend)-[w:WORK_AT]->(company)-[:IS_LOCATED_IN]->
/// (:Country{name}) WHERE w.<from> < T RETURN … ORDER BY … LIMIT k`. The ordinary
/// plan expands EVERY friend→company→country (~4k rows) then filters by name. This
/// RESOLVES the anchored country first, takes its companies as a SET, and filters
/// friends' WORK_AT edges by membership (+ the rel bound) BEFORE materialising —
/// so only the surviving rows reach the shared `project_rows_tail`. `Ok(None)` =
/// the shape does not match. Byte-identical: same WHERE-satisfying rows, same tail.
fn try_ic11_semijoin(
    graph: &Graph,
    ms: &MultiStagePlan,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    if ms.s1_hops.len() != 1 || !ms.distinct || ms.s2_hops.len() != 2 {
        return Ok(None);
    }
    let h_knows = &ms.s1_hops[0];
    let (h_work, h_loc) = (&ms.s2_hops[0], &ms.s2_hops[1]);
    if h_knows.types != ["KNOWS"]
        || h_knows.varlen.is_none()
        || h_work.types != ["WORK_AT"]
        || h_loc.types != ["IS_LOCATED_IN"]
        || !matches!(h_work.dir, Dir::Out)
        || !matches!(h_loc.dir, Dir::Out)
    {
        return Ok(None);
    }
    let Some(workat_var) = h_work.rel_var.clone() else {
        return Ok(None);
    };
    let MultiStageTail::Core(core) = &ms.tail else {
        return Ok(None);
    };
    let friend_var = &h_knows.var;
    let (Some((cname_prop, cname_val)), Some((wf_prop, wf_bound))) = (
        ic11_eq_anchor(&ms.s2_wheres, &h_loc.var, graph, params)?,
        ic11_lt_bound(&ms.s2_wheres, &workat_var, graph, params)?,
    ) else {
        return Ok(None);
    };
    // The stage-2 WHERE must be EXACTLY those two conjuncts, else other filters
    // apply that this fast path does not model — decline.
    if ms.s2_wheres.len() != 2 {
        return Ok(None);
    }

    // Resolve the anchored country node(s), then its companies as a membership set.
    let country_members = graph
        .members_all(&h_loc.labels)
        .map_err(RunError::Graph)?
        .to_arc_vec();
    let mut cname_props: BTreeSet<String> = BTreeSet::new();
    cname_props.insert(cname_prop.clone());
    let Some(ccols) = load_var_columns(graph, VarKind::Node, &country_members[..], &cname_props)?
    else {
        return Ok(None);
    };
    let Some(name_col) = ccols.get(&cname_prop) else {
        return Ok(None);
    };
    let loc_tokens = graph.type_tokens_peek(&h_loc.types);
    let mut company_set: BTreeSet<u64> = BTreeSet::new();
    for (i, &c0) in country_members.iter().enumerate() {
        if name_col[i] == cname_val {
            graph.adjacent_slim_for_each(c0, Dir::In, &loc_tokens, |e| {
                company_set.insert(e.peer);
            });
        }
    }

    // Stage 1: the DISTINCT KNOWS*1..2 friends (the SAME build the general path uses).
    if hops_have_varlen(&ms.s1_hops) && !graph.frontier_expand_enabled() {
        return Ok(None);
    }
    let Some(s1_chunk) = build_chunk(
        graph,
        &ms.s1_a_labels,
        &ms.s1_a_var,
        &ms.s1_hops,
        &ms.s1_wheres,
        ms.s1_anchor.as_ref(),
        params,
    )?
    else {
        return Ok(None);
    };
    let fvi = s1_chunk.var_index(friend_var);
    let mut friends: Vec<u64> = s1_chunk
        .selection
        .iter()
        .map(|&r| s1_chunk.ids[fvi][r])
        .collect();
    friends.sort_unstable();
    friends.dedup();

    // Stage 2 as a SEMIJOIN: each friend's WORK_AT edges to a Country0 company.
    let work_tokens = graph.type_tokens_peek(&h_work.types);
    let mut cand: Vec<(u64, u64, u64)> = Vec::new(); // (friend, workAt rel, company)
    for &f in &friends {
        graph.adjacent_slim_for_each(f, h_work.dir, &work_tokens, |e| {
            if company_set.contains(&e.peer) {
                cand.push((f, e.rel, e.peer));
            }
        });
    }
    if cand.is_empty() {
        return Ok(Some(project_rows_tail(
            graph,
            &core.proj,
            params,
            &core.vars,
            &core.var_kinds,
            &[],
        )?));
    }
    // The rel bound `workAt.<from> < T`, evaluated once per distinct WORK_AT rel.
    let mut rels: Vec<u64> = cand.iter().map(|&(_, r, _)| r).collect();
    rels.sort_unstable();
    rels.dedup();
    let mut wf_props: BTreeSet<String> = BTreeSet::new();
    wf_props.insert(wf_prop.clone());
    let Some(wcols) = load_var_columns(graph, VarKind::Rel, &rels, &wf_props)? else {
        return Ok(None);
    };
    let Some(wf_col) = wcols.get(&wf_prop) else {
        return Ok(None);
    };

    let country0 = country_members
        .iter()
        .enumerate()
        .find(|&(i, _)| name_col[i] == cname_val)
        .map(|(_, &c)| c)
        .unwrap_or(NULL_ID);
    let (Some(friend_vi), Some(work_vi), Some(comp_vi), Some(country_vi)) = (
        core.vars.iter().position(|v| v == friend_var),
        core.vars.iter().position(|v| *v == workat_var),
        core.vars.iter().position(|v| *v == h_work.var),
        core.vars.iter().position(|v| *v == h_loc.var),
    ) else {
        return Ok(None);
    };

    let mut rows: Vec<Vec<u64>> = Vec::with_capacity(cand.len());
    for &(f, rel, comp) in &cand {
        let pos = rels
            .binary_search(&rel)
            .expect("candidate rel is in the distinct set");
        let keep = match &wf_col[pos] {
            Value::Int(x) => *x < wf_bound,
            _ => false,
        };
        if !keep {
            continue;
        }
        let mut row = vec![NULL_ID; core.vars.len()];
        row[friend_vi] = f;
        row[work_vi] = rel;
        row[comp_vi] = comp;
        row[country_vi] = country0;
        rows.push(row);
    }
    counted!("interp.pipeline ic11 semijoin");
    Ok(Some(project_rows_tail(
        graph,
        &core.proj,
        params,
        &core.vars,
        &core.var_kinds,
        &rows,
    )?))
}

fn native_topk(
    graph: &Graph,
    params: &BTreeMap<String, Value>,
    plan: &CorePlan,
    chunk: &DataChunk,
) -> Result<Option<QueryResult>, RunError> {
    let cap = topk_cap(graph, plan, params)?;
    let mut acc = TopKAcc::new(cap);
    if !acc.push_chunk(graph, params, plan, chunk)? {
        return Ok(None); // a budget / type decline while loading key columns
    }
    Ok(Some(acc.finish(graph, params, plan)?))
}

/// The bounded top-k window size `skip + limit` — the `cap` both [`native_topk`]
/// and the batched multi-stage path keep. `recognise_core` guarantees the LIMIT.
fn topk_cap(
    graph: &Graph,
    plan: &CorePlan,
    params: &BTreeMap<String, Value>,
) -> Result<usize, RunError> {
    let limit = eval_count(graph, plan.proj.limit.as_ref(), params, "LIMIT")?
        .expect("recognise_core requires a LIMIT under ORDER BY");
    let skip = eval_count(graph, plan.proj.skip.as_ref(), params, "SKIP")?.unwrap_or(0);
    Ok(skip.saturating_add(limit))
}

/// The bounded top-k **accumulator** behind [`native_topk`]. It holds the `cap`
/// best `(key, seq, ids)` rows under `(ORDER BY key tuple, production seq)` and
/// can be FED MULTIPLE chunks with a CONTINUOUS `seq` — top-k is a monoid under
/// merge-and-trim, so folding batch-by-batch is byte-identical to one pass over
/// the concatenation, PROVIDED the batches arrive in production order (so the
/// global `seq` matches the single-pass tiebreak). This is what lets a
/// bigger-than-RAM stage-2 expand run under bounded memory: each batch's widened
/// chunk is discarded once folded in, leaving only the `<= cap` winners resident.
struct TopKAcc {
    cap: usize,
    /// `(ORDER BY key tuple, production seq, winner row ids)`, kept sorted by
    /// `(key, seq)` — the stable sort's first `cap` rows at all times.
    buf: Vec<(Vec<Value>, u64, Vec<u64>)>,
    /// Live rows folded so far across ALL pushed chunks = the global production
    /// seq. Advances once per live row, exactly as the single-pass enumeration.
    seq: u64,
}

impl TopKAcc {
    fn new(cap: usize) -> Self {
        TopKAcc {
            cap,
            buf: Vec::new(),
            seq: 0,
        }
    }

    /// Fold one chunk's LIVE rows into the buffer, continuing the global `seq`.
    /// `Ok(false)` = a column-budget / type decline (the caller declines the whole
    /// path, exactly as `native_topk` returning `None`). The per-chunk key-column
    /// setup is identical to the historical single-pass body; only `buf`/`seq`/
    /// `cap` persist across calls.
    fn push_chunk(
        &mut self,
        graph: &Graph,
        params: &BTreeMap<String, Value>,
        plan: &CorePlan,
        chunk: &DataChunk,
    ) -> Result<bool, RunError> {
        let order = &plan.proj.order;

        // Resolve every ORDER BY key that is a bare projection ALIAS to the
        // expression it projects (`ORDER BY cd` under `… AS cd` becomes
        // `message.creationDate`), exactly as `recognise_core` did — so
        // classification and the column eval below read the underlying expr, and
        // the sort value equals `run_streaming`'s post-projection ORDER BY scope.
        let keys: Vec<Expr> = order
            .iter()
            .map(|o| resolve_order_key_alias(&o.expr, &plan.vars, &plan.proj))
            .collect();
        // Classify every (resolved) ORDER BY key (const or the one var it reads)
        // and collect the props each var contributes. `recognise_core` validated
        // every key, so classification cannot fail here.
        let mut order_props: Vec<BTreeSet<String>> = vec![BTreeSet::new(); plan.vars.len()];
        let mut key_refs: Vec<KeyRef> = Vec::with_capacity(order.len());
        for key in &keys {
            key_refs.push(
                classify_key(key, &plan.vars, &mut order_props)
                    .expect("recognise_core validated every ORDER BY key"),
            );
        }

        // Per-var DISTINCT live ids (sorted) + loaded key columns, for the vars an
        // ORDER BY key reads. A budget decline on any → the general path.
        let mut distinct: Vec<Vec<u64>> = vec![Vec::new(); plan.vars.len()];
        let mut cols: Vec<BTreeMap<String, Vec<Value>>> = vec![BTreeMap::new(); plan.vars.len()];
        for (vi, props) in order_props.iter().enumerate() {
            if props.is_empty() {
                continue;
            }
            let mut set: BTreeSet<u64> = BTreeSet::new();
            for &r in chunk.selection.iter() {
                set.insert(chunk.ids[vi][r]);
            }
            let d: Vec<u64> = set.into_iter().collect();
            let labels = core_var_labels(plan, &plan.vars[vi]);
            let Some(c) =
                load_var_columns_labelled(graph, chunk.var_kinds[vi], &d, props, labels, params)?
            else {
                return Ok(false);
            };
            distinct[vi] = d;
            cols[vi] = c;
        }

        // Precompute each ORDER BY key as a value column over its var (one
        // `eval_column` per key), broadcasting consts. Divergence from the
        // per-tuple `eval_expr` on the same key is structurally impossible for the
        // accepted forms (props/consts/three-valued booleans) — the same primitive.
        let empty_vm = VarMap::new();
        let scope = Scope::over(params, &empty_vm, graph.wall_ms(), graph.zone_provider());
        let mut keycols: Vec<KeyCol> = Vec::with_capacity(order.len());
        for (key, kref) in keys.iter().zip(&key_refs) {
            match kref {
                KeyRef::Const => {
                    keycols.push(KeyCol::Const(
                        eval_with(key, &scope, None).map_err(RunError::Eval)?,
                    ));
                }
                KeyRef::Var(vi) => {
                    let view = crate::vectorized::view(&cols[*vi]);
                    let Some(col) = eval_column(
                        key,
                        &plan.vars[*vi],
                        distinct[*vi].len(),
                        &view,
                        &scope,
                    ) else {
                        return Ok(false);
                    };
                    keycols.push(KeyCol::Var(*vi, col.into_owned()));
                }
            }
        }

        // Fold every live row into the shared buffer, keyed (ORDER BY keys, global
        // seq). `seq` advances once per live row and CONTINUES across chunks, so
        // the tiebreak is production order over the whole (batched) stream —
        // identical to a single pass over the concatenation.
        for &r in chunk.selection.iter() {
            let seq = self.seq;
            self.seq += 1;
            let mut key: Vec<Value> = Vec::with_capacity(keycols.len());
            for kc in &keycols {
                key.push(match kc {
                    KeyCol::Const(v) => v.clone(),
                    KeyCol::Var(vi, col) => {
                        let id = chunk.ids[*vi][r];
                        let pos = distinct[*vi]
                            .binary_search(&id)
                            .expect("a live id is in its var's distinct set");
                        col[pos].clone()
                    }
                });
            }
            if self.cap == 0 {
                continue;
            }
            let pos = self
                .buf
                .partition_point(|(k, ks, _)| match cmp_order_keys(order, k, &key) {
                    std::cmp::Ordering::Less => true,
                    std::cmp::Ordering::Equal => *ks < seq,
                    std::cmp::Ordering::Greater => false,
                });
            if pos < self.cap {
                self.buf.insert(pos, (key, seq, chunk.row_ids(r)));
                self.buf.truncate(self.cap);
            }
        }
        Ok(true)
    }

    /// LATE-MATERIALISE only the winners (already in (key, seq) order) through the
    /// shared projection tail: it re-sorts stable (same order) and applies
    /// SKIP/LIMIT — byte-identical to the per-tuple path.
    fn finish(
        self,
        graph: &Graph,
        params: &BTreeMap<String, Value>,
        plan: &CorePlan,
    ) -> Result<QueryResult, RunError> {
        let winners: Vec<Vec<u64>> = self.buf.into_iter().map(|(_, _, ids)| ids).collect();
        project_rows_tail(
            graph,
            &plan.proj,
            params,
            &plan.vars,
            &plan.var_kinds,
            &winners,
        )
    }
}

/// One grouping key precomputed as a distinct-aligned value column over the one
/// var it reads (or a broadcast const / a node identity), ready to gather per
/// live row.
enum KeyVal {
    /// Group by node identity — the id column of that var (no property read).
    Node(usize),
    /// (var index, distinct-aligned value column for that var).
    Col(usize, Vec<Value>),
    /// A broadcast constant.
    Const(Value),
}

/// One aggregate site's per-row argument value, precomputed — the value fed to
/// `SiteAcc::push` for each live row (or `None` for a star site).
enum SiteArgVal {
    /// `count(*)` — push `None` per live row.
    Star,
    /// (var index, distinct-aligned value column — a `var.prop` value or a
    /// materialised full node); gather `col[pos]` per row and push `Some`.
    Gather(usize, Vec<Value>),
    /// A broadcast constant argument, pushed `Some(v)` per live row.
    Const(Value),
    /// A non-DISTINCT `count(<bare node var>)` — only PRESENCE matters, so push a
    /// cheap non-null marker when the var is bound (`id != NULL_ID`) and `Null`
    /// otherwise. No node materialisation and no per-row `Value::Node` clone (the
    /// count-over-node hot path). Byte-identical to a materialised `count(node)`,
    /// since `count` only tests non-null; NOT used for DISTINCT (which needs the
    /// node identity) or any value-consuming aggregate.
    Present(usize),
}

/// The value one aggregate site folds for live row `r` — `None` for a star site,
/// else the gathered / broadcast argument value (nulls are skipped by `push`).
fn site_push_value(
    av: &SiteArgVal,
    distinct: &[Vec<u64>],
    chunk: &DataChunk,
    r: usize,
) -> Option<Value> {
    match av {
        SiteArgVal::Star => None,
        SiteArgVal::Gather(vi, col) => {
            let id = chunk.ids[*vi][r];
            // An OPTIONAL-MATCH null-fill row: the var is unbound, so the gathered
            // argument is `Value::Null` — which `SiteAcc::push` skips. That is
            // exactly what makes `count(optvar)` / `collect(optvar)` /
            // `count(optvar.prop)` exclude the unmatched row (NOT `count(*)`),
            // reproducing the per-tuple path's null binding. `NULL_ID` is never in
            // `distinct` (it is filtered out when the distinct set is built), so
            // this branch also keeps the `binary_search` total.
            if id == NULL_ID {
                return Some(Value::Null);
            }
            let pos = distinct[*vi]
                .binary_search(&id)
                .expect("a live id is in its var's distinct set");
            Some(col[pos].clone())
        }
        SiteArgVal::Const(v) => Some(v.clone()),
        SiteArgVal::Present(vi) => {
            // Presence only: bound → a cheap non-null marker (`count` +1); an
            // OPTIONAL null-fill (`NULL_ID`) → `Null` (skipped). No `distinct`
            // lookup, no node materialisation.
            if chunk.ids[*vi][r] == NULL_ID {
                Some(Value::Null)
            } else {
                Some(Value::Bool(true))
            }
        }
    }
}

/// Fold live row `r` into a group's accumulators — one `SiteAcc::push` per site,
/// in site order, exactly as `StreamProjector::push`'s per-site loop does.
fn fold_row(
    accs: &mut [SiteAcc],
    arg_vals: &[SiteArgVal],
    distinct: &[Vec<u64>],
    chunk: &DataChunk,
    r: usize,
) -> Result<(), RunError> {
    for (acc, av) in accs.iter_mut().zip(arg_vals) {
        acc.push(site_push_value(av, distinct, chunk, r))?;
    }
    Ok(())
}

/// [`fold_row`] for a chunk that MAY carry COUNT-FOLD weights: a `count(*)`
/// site adds the row's weight — the number of folded walks the row stands for —
/// instead of 1, in the SAME production order, so the running total is the
/// general path's total. An unweighted chunk folds exactly as before. A
/// weighted chunk only ever reaches an all-`count(*)` reduction
/// (`plan_count_fold` folds nothing otherwise); should any other site meet a
/// weight it would count 1 where `w` is due, so that DECLINES (`Ok(None)`)
/// rather than miscount. A total that would leave `i64` is NOT a decline: the
/// general path's own `i64` accumulator could not represent it either and it
/// could never enumerate that many rows, so it REFUSES
/// (`count_fold_overflow`), exactly as the fold's own arithmetic does.
fn fold_row_weighted(
    accs: &mut [SiteAcc],
    arg_vals: &[SiteArgVal],
    distinct: &[Vec<u64>],
    chunk: &DataChunk,
    r: usize,
) -> Result<Option<()>, RunError> {
    if chunk.weights.is_empty() {
        fold_row(accs, arg_vals, distinct, chunk, r)?;
        return Ok(Some(()));
    }
    let w = chunk.weights[r];
    for acc in accs.iter_mut() {
        match acc {
            SiteAcc::CountStar(n) => {
                let Ok(wi) = i64::try_from(w) else {
                    return Err(count_fold_overflow());
                };
                let Some(next) = n.checked_add(wi) else {
                    return Err(count_fold_overflow());
                };
                *n = next;
            }
            // Unreachable by construction (`plan_count_fold` folds only under
            // all-`count(*)` sites); declining keeps it impossible to miscount.
            _ => return Ok(None),
        }
    }
    Ok(Some(()))
}

/// The DEGREE SHORT-CIRCUIT for the unanchored `MATCH (a:A)-[:R]->(b:B) RETURN
/// b.<prop>, count(a) [ORDER BY …] [LIMIT …]` shape. Instead of materialising the
/// whole (a,b) edge chunk and reducing it, count each target's matching edges from
/// the SOURCE side with the SAME reverse-adjacency walk `expand` uses — so the
/// production / first-seen order is identical — then group by the target PROPERTY
/// VALUE (merging same-valued targets exactly as the general reducer's `agg_key_of`
/// value key would) and feed the SAME `finalize_agg_groups`.
///
/// Byte-identical with NO schema assumption: grouping is by value (it can never
/// split or merge differently from the general path), and groups are built in
/// first-seen-value order (so any downstream ORDER BY tie resolves identically).
/// The win is skipping the full chunk build + the per-edge reduce. `Ok(None)` = the
/// shape does not match, or a load declined — the general path answers identically.
/// BI7's 2-HOP COUNT-ROLLUP. The shape is `MATCH (a:A)-[:R1]->(b:B)-[:R2]->(c:C)
/// RETURN c.<prop>, count(a) ORDER BY count DESC, c.<prop> … LIMIT k` — count the
/// SOURCE grouped by the FAR (2-hop) target. The ordinary plan materialises every
/// (a,b,c) row (BI7: 85k message→country→continent rows for 6 counts). This counts
/// sources per MIDDLE node (b, ~40 countries), then ROLLS those counts up the second
/// hop to the far node (c), never materialising the wide chain. It fires only when
/// the ORDER BY includes the group key (so the group order is TOTAL and the rollup's
/// arrival order can't leak — first-seen never matters). `Ok(None)` = shape mismatch.
fn try_bi7_rollup(
    graph: &Graph,
    plan: &AggPlan,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    if plan.hops.len() != 2 || !plan.wheres.is_empty() || plan.start_anchor.is_some() {
        return Ok(None);
    }
    let (h1, h2) = (&plan.hops[0], &plan.hops[1]);
    if h1.varlen.is_some()
        || h2.varlen.is_some()
        || h1.rel_var.is_some()
        || h2.rel_var.is_some()
        || matches!(h1.dir, Dir::Both)
        || matches!(h2.dir, Dir::Both)
        || h1.types.len() != 1
        || h2.types.len() != 1
    {
        return Ok(None);
    }
    let (Some(vi_a), Some(vi_mid), Some(vi_far)) = (
        plan.vars.iter().position(|v| *v == plan.a_var),
        plan.vars.iter().position(|v| *v == h1.var),
        plan.vars.iter().position(|v| *v == h2.var),
    ) else {
        return Ok(None);
    };
    if h1.src != vi_a || h2.src != vi_mid || !matches!(plan.var_kinds[vi_far], VarKind::Node) {
        return Ok(None);
    }
    let [gk] = plan.group_keys.as_slice() else {
        return Ok(None);
    };
    match &gk.kind {
        GroupKind::Col(v) if *v == vi_far => {}
        _ => return Ok(None),
    }
    let Expr::Prop(inner, gprop) = &gk.expr else {
        return Ok(None);
    };
    if !matches!(inner.as_ref(), Expr::Var(v) if *v == plan.vars[vi_far]) {
        return Ok(None);
    }
    let [site] = plan.sites.as_slice() else {
        return Ok(None);
    };
    if site.name != "count" || site.star || site.distinct {
        return Ok(None);
    }
    let [SiteArgPlan::Node(vi_arg)] = plan.site_args.as_slice() else {
        return Ok(None);
    };
    // A non-distinct `count` over ANY bound var of this chain (source or middle) is
    // the ROW count — every var is non-null in every (a,b,c) row — which is exactly
    // what the rollup sums. (BI7 counts the source; BI8 counts the middle.)
    if *vi_arg != vi_a && *vi_arg != vi_mid {
        return Ok(None);
    }
    let AggForm::Return(proj) = &plan.form else {
        return Ok(None);
    };
    // TOTAL-ORDER guard: the ORDER BY must reference the group key (by its RETURN
    // alias or the expr itself), so distinct groups differ on that key and the
    // rollup's arrival order never leaks. Otherwise decline.
    let gk_name = proj.items.iter().enumerate().find_map(|(i, it)| {
        (it.expr == gk.expr).then(|| {
            it.alias
                .clone()
                .or_else(|| it.text.clone())
                .unwrap_or_else(|| column_name(&it.expr, i))
        })
    });
    let ordered_by_group = proj.order.iter().any(|o| {
        o.expr == gk.expr || matches!((&o.expr, &gk_name), (Expr::Var(v), Some(n)) if v == n)
    });
    if !ordered_by_group {
        return Ok(None);
    }

    let (Some(t1), Some(t2)) = (
        graph.type_tokens_peek(&h1.types),
        graph.type_tokens_peek(&h2.types),
    ) else {
        return finalize_agg_groups(
            graph,
            plan,
            params,
            Vec::new(),
            GroupKeyCols::new(),
            finish_aggregate,
        );
    };
    let (t1, t2) = (Some(t1), Some(t2));
    let a_members = graph.members_all(&plan.a_labels).map_err(RunError::Graph)?;
    let mid_members = (!h1.labels.is_empty())
        .then(|| graph.members_all(&h1.labels).map_err(RunError::Graph))
        .transpose()?;
    let far_members = (!h2.labels.is_empty())
        .then(|| graph.members_all(&h2.labels).map_err(RunError::Graph))
        .transpose()?;

    // Hop 1: count sources per MIDDLE node.
    let mut mid_count: BTreeMap<u64, i64> = BTreeMap::new();
    for a in a_members.iter() {
        graph.adjacent_slim_for_each(a, h1.dir, &t1, |e| {
            if let Some(m) = &mid_members {
                if !m.contains(e.peer) {
                    return;
                }
            }
            *mid_count.entry(e.peer).or_insert(0) += 1;
        });
    }
    // Hop 2: roll each middle node's count up to its FAR node(s).
    let mut far_count: BTreeMap<u64, i64> = BTreeMap::new();
    let mut first_seen: Vec<u64> = Vec::new();
    for (&mid, &cnt) in &mid_count {
        graph.adjacent_slim_for_each(mid, h2.dir, &t2, |e| {
            if let Some(m) = &far_members {
                if !m.contains(e.peer) {
                    return;
                }
            }
            match far_count.entry(e.peer) {
                std::collections::btree_map::Entry::Occupied(mut o) => *o.get_mut() += cnt,
                std::collections::btree_map::Entry::Vacant(v) => {
                    v.insert(cnt);
                    first_seen.push(e.peer);
                }
            }
        });
    }
    if first_seen.is_empty() {
        return finalize_agg_groups(
            graph,
            plan,
            params,
            Vec::new(),
            GroupKeyCols::new(),
            finish_aggregate,
        );
    }

    let sorted: Vec<u64> = far_count.keys().copied().collect(); // BTreeMap keys are sorted
    let mut props: BTreeSet<String> = BTreeSet::new();
    props.insert(gprop.clone());
    let Some(cols) = load_var_columns(graph, VarKind::Node, &sorted, &props)? else {
        return Ok(None);
    };
    let Some(val_col) = cols.get(gprop) else {
        return Ok(None);
    };
    if val_col
        .iter()
        .any(|v| matches!(v, Value::Float(f) if f.is_nan()))
    {
        return Ok(None);
    }

    // Merge far nodes by group-key VALUE, summing counts (total order → arrival
    // order irrelevant).
    let mut rep_ids: Vec<u64> = Vec::new();
    let mut group_counts: Vec<i64> = Vec::new();
    let mut group_of: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    let mut nonce = 0u64;
    for &c in &first_seen {
        let pos = sorted
            .binary_search(&c)
            .expect("a far node is in its sorted set");
        let key = agg_key_of(std::slice::from_ref(&val_col[pos]), &mut nonce);
        let cnt = far_count[&c];
        match group_of.get(&key) {
            Some(&gi) => group_counts[gi] += cnt,
            None => {
                group_of.insert(key, group_counts.len());
                rep_ids.push(c);
                group_counts.push(cnt);
            }
        }
    }

    let mut groups: Vec<Group> = Vec::with_capacity(rep_ids.len());
    for (gi, &rep) in rep_ids.iter().enumerate() {
        let mut ids = vec![NULL_ID; plan.vars.len()];
        ids[vi_far] = rep;
        groups.push((ids, vec![SiteAcc::count_preset(group_counts[gi])]));
    }
    let mut gkc: GroupKeyCols = BTreeMap::new();
    gkc.insert(vi_far, (sorted, cols));
    counted!("interp.pipeline bi7 rollup");
    finalize_agg_groups(graph, plan, params, groups, gkc, finish_aggregate)
}

fn try_degree_aggregate(
    graph: &Graph,
    plan: &AggPlan,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    // ── Shape gate ──
    if !plan.wheres.is_empty() || plan.start_anchor.is_some() {
        return Ok(None);
    }
    let AggForm::Return(_) = &plan.form else {
        return Ok(None);
    };
    let [hop] = plan.hops.as_slice() else {
        return Ok(None);
    };
    if hop.varlen.is_some()
        || hop.rel_var.is_some()
        || hop.types.len() != 1
        || matches!(&hop.dir, Dir::Both)
    {
        return Ok(None);
    }
    let (Some(vi_a), Some(vi_b)) = (
        plan.vars.iter().position(|v| *v == plan.a_var),
        plan.vars.iter().position(|v| *v == hop.var),
    ) else {
        return Ok(None);
    };
    if hop.src != vi_a || vi_a == vi_b || !matches!(plan.var_kinds[vi_b], VarKind::Node) {
        return Ok(None);
    }
    // Exactly one grouping key: a bare property of b.
    let [gk] = plan.group_keys.as_slice() else {
        return Ok(None);
    };
    match &gk.kind {
        GroupKind::Col(v) if *v == vi_b => {}
        _ => return Ok(None),
    }
    let Expr::Prop(inner, b_prop) = &gk.expr else {
        return Ok(None);
    };
    match inner.as_ref() {
        Expr::Var(v) if *v == plan.vars[vi_b] => {}
        _ => return Ok(None),
    }
    // Exactly one aggregate site: a non-distinct count over the source var.
    let [site] = plan.sites.as_slice() else {
        return Ok(None);
    };
    if site.name != "count" || site.star || site.distinct {
        return Ok(None);
    }
    let [SiteArgPlan::Node(vi_arg)] = plan.site_args.as_slice() else {
        return Ok(None);
    };
    if *vi_arg != vi_a {
        return Ok(None);
    }
    counted!("interp.pipeline degree aggregate");

    // ── Count each target's matching in-edges from the SOURCE side, in the EXACT
    // reverse-adjacency order `expand` produces (first-seen order byte-identical). ──
    let Some(tokens) = graph.type_tokens_peek(&hop.types) else {
        // Type never minted → no edges → the keyed aggregate emits no rows.
        return finalize_agg_groups(
            graph,
            plan,
            params,
            Vec::new(),
            GroupKeyCols::new(),
            finish_aggregate,
        );
    };
    let tokens = Some(tokens);
    let a_members = graph.members_all(&plan.a_labels).map_err(RunError::Graph)?;
    let b_members = if hop.labels.is_empty() {
        None
    } else {
        Some(graph.members_all(&hop.labels).map_err(RunError::Graph)?)
    };

    let mut counts: Vec<i64> = Vec::new();
    let mut first_seen: Vec<u64> = Vec::new();
    let mut probes = 0u64;
    for a in a_members.iter() {
        graph.adjacent_slim_rev_for_each(a, hop.dir, &tokens, |e| {
            let peer = e.peer;
            if let Some(bm) = &b_members {
                probes += 1;
                if !graph.members_contains(bm, peer) {
                    return;
                }
            }
            let i = peer as usize;
            if counts.len() <= i {
                counts.resize(i + 1, 0);
            }
            if counts[i] == 0 {
                first_seen.push(peer);
            }
            counts[i] += 1;
        });
    }
    crate::counters::MEMBERS_PROBES.fetch_add(probes, std::sync::atomic::Ordering::Relaxed);
    if first_seen.is_empty() {
        return finalize_agg_groups(
            graph,
            plan,
            params,
            Vec::new(),
            GroupKeyCols::new(),
            finish_aggregate,
        );
    }

    // ── Load b.<prop> once per distinct target, then MERGE by property VALUE in
    // first-seen order, summing the per-target edge counts. ──
    let mut sorted_targets = first_seen.clone();
    sorted_targets.sort_unstable(); // first_seen has no dups (pushed on 0→1)
    let mut props: BTreeSet<String> = BTreeSet::new();
    props.insert(b_prop.clone());
    let Some(cols) = load_var_columns(graph, VarKind::Node, &sorted_targets, &props)? else {
        return Ok(None); // a column-budget / type decline → general path
    };
    let Some(val_col) = cols.get(b_prop) else {
        return Ok(None);
    };
    // A NaN group key is per-OCCURRENCE in the general path (a per-row nonce), which
    // a per-value merge cannot reproduce — decline so it answers there.
    if val_col
        .iter()
        .any(|v| matches!(v, Value::Float(f) if f.is_nan()))
    {
        return Ok(None);
    }

    let mut rep_ids: Vec<u64> = Vec::new();
    let mut group_counts: Vec<i64> = Vec::new();
    // Merge in first-seen order. Primitive keys use the lightweight `NativeKey`
    // (no per-target `Vec<u8>` serialization — that cost made a high-cardinality
    // Int key like BI5, ~470k distinct targets, SLOWER than the reduce it was meant
    // to beat); other value types fall back to `agg_key_of`, one NaN nonce threaded
    // across targets exactly as the reduce threads it across rows. Both group by the
    // same equivalence, so the result is identical.
    if val_col.iter().all(native_key_eligible) {
        let mut group_of: BTreeMap<NativeKey, usize> = BTreeMap::new();
        for &t in &first_seen {
            let pos = sorted_targets
                .binary_search(&t)
                .expect("a counted target is in its sorted set");
            let nk = NativeKey::of(&val_col[pos]);
            let c = counts[t as usize];
            match group_of.get(&nk) {
                Some(&gi) => group_counts[gi] += c,
                None => {
                    group_of.insert(nk, group_counts.len());
                    rep_ids.push(t);
                    group_counts.push(c);
                }
            }
        }
    } else {
        let mut group_of: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
        let mut nonce = 0u64;
        for &t in &first_seen {
            let pos = sorted_targets
                .binary_search(&t)
                .expect("a counted target is in its sorted set");
            let key = agg_key_of(std::slice::from_ref(&val_col[pos]), &mut nonce);
            let c = counts[t as usize];
            match group_of.get(&key) {
                Some(&gi) => group_counts[gi] += c,
                None => {
                    group_of.insert(key, group_counts.len());
                    rep_ids.push(t);
                    group_counts.push(c);
                }
            }
        }
    }

    // ── TOP-K-BEFORE-PROJECT. When the RETURN orders by the COUNT descending as
    // its FIRST key and carries a LIMIT, only groups whose count is >= the k-th
    // largest can reach the output. Dropping the rest BEFORE finalize is the whole
    // win: projecting + sorting the FULL group set is finalize's dominant cost
    // (~230k groups = ~985ms in BI5; the actual result is 20 rows). finalize then
    // sorts + limits identically over the SAME candidate rows — every dropped group
    // has a STRICTLY smaller count than the k-th, so no ORDER BY tie (broken by the
    // later keys) can pull it into the window. All boundary ties (count == the k-th)
    // are kept, so the byte-identical top-k is preserved. No LIMIT / a non-count-DESC
    // first key / a paged SKIP+LIMIT below the group count → no change, as before.
    if let AggForm::Return(proj) = &plan.form {
        let count_name = proj.items.iter().enumerate().find_map(|(i, it)| {
            expr_has_aggregate(&it.expr).then(|| {
                it.alias
                    .clone()
                    .or_else(|| it.text.clone())
                    .unwrap_or_else(|| column_name(&it.expr, i))
            })
        });
        let first_key_is_count_desc = matches!(
            (proj.order.first(), &count_name),
            (Some(o), Some(name)) if o.desc && matches!(&o.expr, Expr::Var(v) if v == name)
        );
        if first_key_is_count_desc {
            if let (Ok(skip), Ok(Some(lim))) = (
                eval_count(graph, proj.skip.as_ref(), params, "SKIP"),
                eval_count(graph, proj.limit.as_ref(), params, "LIMIT"),
            ) {
                let keep = skip.unwrap_or(0).saturating_add(lim);
                if keep > 0 && rep_ids.len() > keep {
                    let mut sorted_counts = group_counts.clone();
                    sorted_counts.sort_unstable_by(|a, b| b.cmp(a));
                    let threshold = sorted_counts[keep - 1];
                    let survivors: Vec<usize> = (0..rep_ids.len())
                        .filter(|&i| group_counts[i] >= threshold)
                        .collect();
                    rep_ids = survivors.iter().map(|&i| rep_ids[i]).collect();
                    group_counts = survivors.iter().map(|&i| group_counts[i]).collect();
                }
            }
        }
    }

    // ── Synthetic groups in first-seen-value order + the group-key column hand-off
    // finalize/projection reuse (a sorted superset of the representative ids). ──
    let mut groups: Vec<Group> = Vec::with_capacity(rep_ids.len());
    for (gi, &rep) in rep_ids.iter().enumerate() {
        let mut ids = vec![NULL_ID; plan.vars.len()];
        ids[vi_b] = rep;
        groups.push((ids, vec![SiteAcc::count_preset(group_counts[gi])]));
    }
    let mut gkc: GroupKeyCols = BTreeMap::new();
    gkc.insert(vi_b, (sorted_targets, cols));
    finalize_agg_groups(graph, plan, params, groups, gkc, finish_aggregate)
}

/// Run the recognised group-by-aggregate: expand+filter the chain, reduce the
/// live rows into first-seen groups with per-group per-site accumulators, then
/// late-materialise + project through the shared aggregating tail. Byte-identical
/// to the per-tuple `run_streaming` aggregation on every accepted shape, or
/// `Ok(None)` (a budget or column decline; the general path answers identically).
fn run_aggregate(
    graph: &Graph,
    plan: &AggPlan,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    // The degree short-circuit answers `count(src) GROUP BY dst` without building
    // the edge chunk. It returns `Ok(None)` on any non-matching shape, so the rest
    // of this function (and the general reduce) is the unchanged fallback.
    if graph.degree_aggregate_enabled() {
        if let Some(r) = try_degree_aggregate(graph, plan, params)? {
            return Ok(Some(r));
        }
    }
    // BI7's 2-hop count-rollup — the same idea one hop further; declines on any
    // other shape.
    if graph.bi7_rollup_enabled() {
        if let Some(r) = try_bi7_rollup(graph, plan, params)? {
            return Ok(Some(r));
        }
    }
    // A frontier-BFS var-length hop runs the columnar BFS only when `run_streaming`
    // itself would (`frontier_ok`'s `graph.frontier_expand_enabled()`); with the
    // toggle off, decline so the enumerating general path answers identically.
    if hops_have_varlen(&plan.hops) && !graph.frontier_expand_enabled() {
        return Ok(None);
    }
    // SCAN + expand/semijoin + WHERE into the read chunk (mirrors the
    // non-aggregate path). An empty a-set or an unminted hop type ⇒ an empty
    // chunk ⇒ zero groups; with >=1 grouping key the aggregating projector emits
    // NO row, and a global aggregate its single zero row, exactly like
    // `run_streaming`.
    let Some(chunk) = build_chunk(
        graph,
        &plan.a_labels,
        &plan.a_var,
        &plan.hops,
        &plan.wheres,
        plan.start_anchor.as_ref(),
        params,
    )?
    else {
        return Ok(None); // a filter budget / non-boolean decline
    };
    run_aggregate_over_chunk(graph, plan, params, &chunk, finish_aggregate)
}

/// Run a recognised DISTINCT projection: build the read chunk (mirrors the
/// non-aggregate path), reduce the live rows into first-seen groups keyed by the
/// projected items with ZERO aggregate sites (`reduce_agg_groups` — the raw-id
/// u64 fast path for a bare node/rel key, `agg_key_of` for value keys), then emit
/// one row per group through `run_agg_return` and the shared ORDER BY / SKIP /
/// LIMIT tail. The group reduction dedups BEFORE the tail's sort/limit, so the
/// DISTINCT-before-LIMIT semantics hold; the output is byte-identical to
/// `run_streaming`'s DISTINCT (the same first-seen canonical-key equivalence).
/// Fires the SAME "hop runs" counter as the non-aggregate path (`finish`), so the
/// pipeline reports a DISTINCT projection as the core path, not the aggregate one.
/// `Ok(None)` = a budget / column decline; the general path answers identically.
fn run_distinct(
    graph: &Graph,
    plan: &AggPlan,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    // The frontier-BFS toggle gate, as in `run_aggregate` — decline the columnar
    // BFS when `run_streaming` would not take it, so ON == OFF.
    if hops_have_varlen(&plan.hops) && !graph.frontier_expand_enabled() {
        return Ok(None);
    }
    let Some(chunk) = build_chunk(
        graph,
        &plan.a_labels,
        &plan.a_var,
        &plan.hops,
        &plan.wheres,
        plan.start_anchor.as_ref(),
        params,
    )?
    else {
        return Ok(None); // a filter budget / non-boolean decline
    };
    match &plan.form {
        AggForm::Return(_) => run_distinct_over_chunk(graph, plan, params, &chunk, finish),
        // The Form-A DISTINCT tail (`WITH DISTINCT <keys> … RETURN …`): a
        // zero-site group-by whose RETURN projects over the WITH aliases —
        // the aggregating tail's own Form-A reduction and projection.
        AggForm::With(_) => run_aggregate_over_chunk(graph, plan, params, &chunk, finish),
    }
}

/// Reduce an already-built read chunk into first-seen DISTINCT groups and project
/// through the shared aggregating tail (zero sites) — the DISTINCT projection's
/// post-chunk half, shared by `run_distinct` (whole scan) and the multi-stage
/// WITH path (stage-2 chunk). `finish` stamps the operator counter of whichever
/// path called it. `Ok(None)` = a column-budget / type decline.
fn run_distinct_over_chunk(
    graph: &Graph,
    plan: &AggPlan,
    params: &BTreeMap<String, Value>,
    chunk: &DataChunk,
    finish: FinishFn,
) -> Result<Option<QueryResult>, RunError> {
    // The grouping-key vars to materialise at output (a const-only key needs
    // none). Every projected item is a grouping key, so this is every var an item
    // reads — exactly the vars `project_agg_groups` binds into the group template.
    let mut gvi: BTreeSet<usize> = BTreeSet::new();
    for gk in &plan.group_keys {
        match gk.kind {
            GroupKind::Node(vi) | GroupKind::Col(vi) => {
                gvi.insert(vi);
            }
            GroupKind::Const => {}
        }
    }
    let group_var_idx: Vec<usize> = gvi.into_iter().collect();

    // ZERO live rows ⇒ zero groups ⇒ zero DISTINCT rows. A DISTINCT projection
    // always has >=1 grouping key, so (unlike a global aggregate) an empty input
    // yields NO row — matching `run_streaming`.
    let (groups, gkc): (Vec<Group>, GroupKeyCols) = if chunk.live() == 0 {
        (Vec::new(), GroupKeyCols::new())
    } else {
        match reduce_agg_groups(graph, plan, params, chunk)? {
            Some(g) => g,
            None => return Ok(None), // a column-budget / type decline
        }
    };

    // A DISTINCT projection is always Form B (the RETURN itself); the group rows
    // project through the shared aggregating tail (with zero sites, each item is a
    // grouping `AggItem::Key`), which dedups (a no-op over the already-unique group
    // rows) → orders → skips → limits.
    let AggForm::Return(proj) = &plan.form else {
        return Ok(None);
    };
    let labels = agg_var_labels(plan);
    let result = run_agg_return(
        graph,
        proj,
        params,
        &plan.vars,
        &plan.var_kinds,
        &labels,
        &group_var_idx,
        &plan.sites,
        &plan.agg_items,
        &gkc,
        groups,
    )?;
    finish(result)
}

/// One morsel's output columns: `(existing-columns, peer-column, rel-column,
/// used-rels, provenance)` — the partial an `expand` worker returns, concatenated
/// in slice order to rebuild the serial result.
type ExpandCols = (
    Vec<Vec<u64>>,
    Vec<u64>,
    Option<Vec<u64>>,
    Vec<Vec<u64>>,
    Vec<usize>,
);

/// Expand one MORSEL — a contiguous slice of the driving `selection` — for the
/// parallel path. Byte-for-byte the same per-row body as [`DataChunk::expand`]'s
/// serial loop, factored out so many threads run it over disjoint slices at once.
/// Pure over the graph's read path (`adjacent_slim_rev_for_each` reads
/// arc-swap/atomic-guarded state), so it is safe to run concurrently; the caller
/// concatenates the returned partials IN SLICE ORDER to reproduce the serial
/// output exactly.
///
/// `produced`/`over` are the WORKERS' shared row-budget account. The serial
/// loop refuses INCREMENTALLY as its output grows; without this, each worker
/// materialised its whole partial and the combined check ran only after the
/// merge — "identical pass/fail", but priced at exactly the memory the budget
/// exists to prevent (LSQB q2's probe grew one worker's partial past 1.6 GiB
/// and OOM-killed the 40 Gi bench pod). Workers add their per-row output here
/// and STOP once the shared total passes the budget — which is precisely the
/// point the serial loop would have refused, so pass/fail is unchanged.
#[allow(clippy::too_many_arguments)]
fn expand_row_slice(
    graph: &Graph,
    ids: &[Vec<u64>],
    used_rels: &[Vec<u64>],
    prov: &[usize],
    selection: &[usize],
    src_vi: usize,
    dir: Dir,
    tokens: &Option<Vec<u32>>,
    b_members: Option<&crate::MembersView>,
    track_rels: bool,
    reset_rels: bool,
    carry_prov: bool,
    want_rel_col: bool,
    produced: &std::sync::atomic::AtomicUsize,
    over: &std::sync::atomic::AtomicBool,
    budget: usize,
) -> ExpandCols {
    use std::sync::atomic::Ordering;
    let ncols = ids.len();
    let mut out_ids: Vec<Vec<u64>> = (0..ncols).map(|_| Vec::new()).collect();
    let mut new_col: Vec<u64> = Vec::new();
    let mut rel_col: Option<Vec<u64>> = if want_rel_col { Some(Vec::new()) } else { None };
    let mut out_used: Vec<Vec<u64>> = Vec::new();
    let mut out_prov: Vec<usize> = Vec::new();
    let mut probes = 0u64;
    let mut counted_len = 0usize;
    for &r in selection {
        // One relaxed load per DRIVING ROW (not per edge): stop as soon as any
        // worker has pushed the shared total over the budget. The partial built
        // so far is discarded by the caller — the statement is refusing.
        if over.load(Ordering::Relaxed) {
            break;
        }
        let src = ids[src_vi][r];
        let base: &[u64] = if track_rels && !reset_rels {
            used_rels.get(r).map_or(&[][..], Vec::as_slice)
        } else {
            &[]
        };
        graph.adjacent_slim_rev_for_each(src, dir, tokens, |e| {
            let peer = e.peer;
            if let Some(m) = b_members {
                probes += 1;
                if !graph.members_contains(m, peer) {
                    return;
                }
            }
            if track_rels && base.contains(&e.rel) {
                return;
            }
            for (vi, col) in out_ids.iter_mut().enumerate() {
                col.push(ids[vi][r]);
            }
            new_col.push(peer);
            if let Some(rc) = rel_col.as_mut() {
                rc.push(e.rel);
            }
            if track_rels {
                let mut used = base.to_vec();
                used.push(e.rel);
                out_used.push(used);
            }
            if carry_prov {
                out_prov.push(prov[r]);
            }
        });
        // Account this row's output against the SHARED budget. A row's fanout
        // enters the total as one add; the flag trips at the same combined
        // count the serial loop's incremental check refuses at.
        let len = new_col.len();
        let added = len - counted_len;
        counted_len = len;
        if added > 0 && produced.fetch_add(added, Ordering::Relaxed) + added > budget {
            over.store(true, Ordering::Relaxed);
            break;
        }
    }
    crate::counters::MEMBERS_PROBES.fetch_add(probes, Ordering::Relaxed);
    (out_ids, new_col, rel_col, out_used, out_prov)
}

/// Run ONE ordered step over a chunk — the single dispatch every hop loop shares:
/// a FRONTIER-BFS var-length hop (`hop.varlen`), a fixed EXPAND (`tgt` None) or a
/// SEMIJOIN close (`tgt` Some). `members` is the end-label member set (sorted) and
/// `tokens` the resolved type tokens for this hop.
fn run_hop(
    graph: &Graph,
    chunk: DataChunk,
    hop: &Hop,
    members: Option<&crate::MembersView>,
    tokens: &Option<Vec<u32>>,
) -> Result<DataChunk, RunError> {
    if let Some(max) = hop.varlen {
        return chunk
            .expand_var_length_bfs(graph, hop.src, &hop.var, hop.dir, tokens, members, max);
    }
    match hop.tgt {
        None => chunk.expand(
            graph,
            hop.src,
            &hop.var,
            hop.rel_var.as_deref(),
            hop.dir,
            tokens,
            members,
            hop.track,
            hop.reset,
        ),
        Some(tgt_vi) => chunk.semijoin(
            graph,
            hop.src,
            tgt_vi,
            hop.rel_var.as_deref(),
            hop.dir,
            tokens,
            hop.track,
            hop.reset,
        ),
    }
}

/// Whether any hop is a frontier-BFS var-length hop.
fn hops_have_varlen(hops: &[Hop]) -> bool {
    hops.iter().any(|h| h.varlen.is_some())
}

/// The frontier-BFS DISTINCT-consumed gate: every var-length hop's end var must
/// be consumed DISTINCT-only by the breaker `proj` — the SAME test `frontier_ok`
/// applies via `interp::breaker_distinct_vars` (`distinct_vars_of_proj`). `true`
/// when there is no var-length hop (nothing to gate). A recognizer calls this
/// with the WITH / RETURN projection that follows the chain; a `false` DECLINES
/// the whole query to the enumerating general path.
fn varlen_distinct_consumed(hops: &[Hop], proj: &Projection) -> bool {
    if !hops_have_varlen(hops) {
        return true;
    }
    let dv = distinct_vars_of_proj(proj);
    hops.iter()
        .filter(|h| h.varlen.is_some())
        .all(|h| dv.contains(&h.var))
}

// ─── COUNT FOLD (operator A of docs/lsqb-completeness-plan.md) ───────────────
//
// `MATCH <chain> RETURN count(*)` materialised every partial walk to count them
// (q1 at SF1: >100M rows for one number). Under an all-`count(*)` aggregate the
// rows themselves are never observed — only how many there are per group — so
// a hop whose end var nothing reads need not be expanded: the number of
// qualifying walks through it (and its whole subtree) can be COUNTED per
// driving row and multiplied into the row's weight. `reduce_agg_groups` then
// adds weights instead of 1s (`fold_row_weighted`). Group FIRST-SEEN order is
// unchanged because a folded var is never a grouping key and a row whose fold
// counts zero walks is DROPPED — exactly the rows the general path never
// produces — so the surviving materialised rows are the general path's rows
// with their multiplicities collapsed.
//
// Per driving row the weight of a folded subtree rooted at var `v` is the
// Yannakakis product-of-sums generalised to the hop TREE:
//   w(v, bind, used) = Π over folded child hops h of v:
//       Σ over e in adj(bind[v], h):
//           [peer ∈ members(h.labels)] · [!(h.track && used ∋ e.rel)]
//         · [every inline pred at h's end holds]
//         · (h closes onto t ? [peer == bind[t]] : w(h.end, bind+peer, used+e.rel))
// A close inside the fold is the closing-edge MULTIPLICITY (parallel edges
// multiply, as `pipeline_semijoin.rs` pins), answered by `edge_count_slim`
// when no used rel must be excluded. `Dir::Both` visits O then I with the
// I-side self-loop skipped — `adjacent_slim_visit`'s rule, shared with
// `expand`. A path-boundary child (`reset`) re-seeds `used` empty, a
// non-tracked one leaves it empty, exactly as `expand`'s `base`.
//
// MEMO: a folded level whose subtree reads no var outside it (no close onto
// and no inline pred against a materialised var or an ancestor) and whose
// tracked hops' types are pairwise disjoint from every other hop of their path
// (so `used` can never exclude anything) is a pure function of its node id, and
// is cached in a dense `Vec<u64>` per level (`u64::MAX` = not yet computed)
// bounded by the row budget. q1's eight levels each visit every edge once.
//
// OVERFLOW: every product and sum is `checked_*`; an overflow (or a count past
// `i64`, the type of `count(*)`) REFUSES the statement (`count_fold_overflow`).
// It is NOT a decline: the overflow itself proves the true count is at or past
// 2^63, which the general path can neither enumerate nor represent, so handing
// it the query would trade a clear message for a scan that cannot finish.

/// THE AGGREGATING TAIL'S OWN READ SET over its bound variables: a var the
/// grouping keys, the site arguments, the projection items, the ORDER BY keys
/// or a post-WITH WHERE reference. A var NOT in this set is never observed by
/// the statement, so an operator may replace its column with a placeholder.
///
/// An alias name marks nothing (it is not a pattern var) and a pattern var
/// SHADOWED by an alias is marked conservatively, which only materialises.
/// Shared by [`plan_count_fold`] (the single-MATCH chain) and
/// [`plan_optional_fold`] (each OPTIONAL leg), so both decide "read" the same
/// way over the same tail.
fn agg_tail_read_set(plan: &AggPlan) -> Vec<bool> {
    let mut read = vec![false; plan.vars.len()];
    let idx = |name: &str| plan.vars.iter().position(|x| x == name);
    let mark_expr = |e: &Expr, read: &mut [bool]| {
        let mut fv = Vec::new();
        free_vars_of(e, &mut fv);
        for v in fv {
            if let Some(vi) = idx(&v) {
                read[vi] = true;
            }
        }
    };
    for gk in &plan.group_keys {
        match gk.kind {
            GroupKind::Node(vi) | GroupKind::Col(vi) => read[vi] = true,
            GroupKind::Const => {}
        }
        mark_expr(&gk.expr, &mut read);
    }
    for sa in &plan.site_args {
        match sa {
            SiteArgPlan::Node(vi) | SiteArgPlan::Col(vi, _) => read[*vi] = true,
            SiteArgPlan::Star | SiteArgPlan::Const(_) => {}
        }
    }
    match &plan.form {
        AggForm::Return(p) => {
            for it in &p.items {
                mark_expr(&it.expr, &mut read);
            }
            for o in &p.order {
                mark_expr(&o.expr, &mut read);
            }
        }
        AggForm::With(wf) => {
            for it in &wf.with_proj.items {
                mark_expr(&it.expr, &mut read);
            }
            if let Some(w) = &wf.post_where {
                mark_expr(w, &mut read);
            }
            for it in &wf.return_proj.items {
                mark_expr(&it.expr, &mut read);
            }
            for o in &wf.return_proj.order {
                mark_expr(&o.expr, &mut read);
            }
        }
    }
    read
}

/// Whether every aggregate site is a plain non-DISTINCT `count(*)` — the one
/// tail a fold may weight, since only `count(*)` reads nothing but the row
/// COUNT. Empty sites is not that tail (there is nothing to weight).
fn all_sites_count_star(sites: &[AggSite]) -> bool {
    !sites.is_empty()
        && sites
            .iter()
            .all(|s| s.name == "count" && s.star && !s.distinct)
}

/// COUNT-FOLD ELIGIBILITY: mark the hops the fold counts instead of expanding
/// and move the WHERE conjuncts it evaluates inline onto them. A no-op (the
/// plan unchanged) unless EVERY site is a non-DISTINCT `count(*)`.
///
/// A hop's end var is FOLDABLE iff it is bound by an EXPAND hop (not the seed,
/// not a rel var, not a var-length end, no rel var on its hop), is a NODE, is
/// READ by nothing — no grouping key, no site argument, no projection item, no
/// ORDER BY key, no property WHERE — and (to a fixpoint) every hop sourced
/// from it is foldable or a close the fold can answer, no close from a
/// materialised var targets it, and every two-var conjunct it appears in can be
/// evaluated at its level: the OTHER var is a folded ANCESTOR (in `bind` when
/// the level is entered) or a materialised var bound BEFORE the fold's root
/// hop runs (index below the root's end var — the position rule: a fold runs
/// at its root hop's position so that a continuation hop's rel-iso base is
/// that row's `used_rels`, and by then only the earlier vars are bound).
/// Anything else materialises as before; a conjunct whose vars all
/// materialise stays a chunk filter.
///
/// A CLOSE hop binds no end var, so its own eligibility is its SOURCE's — with
/// one extra rule: a close that binds a RELATIONSHIP VARIABLE the query reads
/// un-folds its source, because `semijoin` gives that variable one ROW per
/// closing edge and the fold has only a placeholder column to offer.
fn plan_count_fold(plan: &mut AggPlan) {
    if !count_fold_enabled() || plan.sites.is_empty() {
        return;
    }
    if !all_sites_count_star(&plan.sites) {
        return;
    }
    let nvars = plan.vars.len();
    let vars: Vec<String> = plan.vars.clone();
    let idx = |name: &str| vars.iter().position(|x| x == name);

    // Which EXPAND hop binds each var; the seed and every rel var have none.
    let mut binder: Vec<Option<usize>> = vec![None; nvars];
    for (hi, h) in plan.hops.iter().enumerate() {
        if let Some(e) = h.end_vi {
            binder[e] = Some(hi);
        }
    }

    // THE READ SET — a var read anywhere outside the fold must materialise.
    let mut read = agg_tail_read_set(plan);
    // A var-length end materialises (the BFS operator is not modelled by the
    // fold), and so does the end of an EXPAND that binds a rel variable — that
    // hop appends TWO columns and the fold would have to place both.
    // A CLOSE's rel var is deliberately NOT marked here: `read[rv]` must stay
    // the QUERY's own read set, so the fixpoint below can tell a rel var
    // nothing resolves (the placeholder column `run_hop_folded` appends is the
    // whole obligation) from one a projection or predicate reads.
    for h in &plan.hops {
        if let Some(e) = h.end_vi {
            if h.varlen.is_some() || h.rel_var.is_some() {
                read[e] = true;
            }
        }
    }
    // WHERE conjuncts: a property predicate reads its var; a two-var id
    // (in)equality or an edge predicate over two NODE vars is an inline
    // CANDIDATE (`a`/`b` in source order; `dir` from `a`'s side).
    struct Cand {
        wi: usize,
        a: usize,
        b: usize,
        kind: CandKind,
    }
    enum CandKind {
        Ne,
        Eq,
        Edge {
            dir: Dir,
            types: Vec<String>,
            negate: bool,
        },
    }
    let mut cands: Vec<Cand> = Vec::new();
    for (wi, w) in plan.wheres.iter().enumerate() {
        let (a, b, kind) = if let Some(ep) = &w.edge {
            let (Some(a), Some(b)) = (idx(&ep.src), idx(&ep.dst)) else {
                return;
            };
            (
                a,
                b,
                CandKind::Edge {
                    dir: ep.dir,
                    types: ep.types.clone(),
                    negate: ep.negate,
                },
            )
        } else if let Some((other, ne)) = &w.id_other {
            let (Some(a), Some(b)) = (idx(&w.var), idx(other)) else {
                return;
            };
            (a, b, if *ne { CandKind::Ne } else { CandKind::Eq })
        } else {
            if let Some(vi) = idx(&w.var) {
                read[vi] = true;
            }
            continue;
        };
        // The filter compares/probes NODE ids only; a rel-kind endpoint keeps
        // the conjunct on the chunk filter (which declines it to the interp).
        if !matches!(plan.var_kinds[a], VarKind::Node) || !matches!(plan.var_kinds[b], VarKind::Node)
        {
            read[a] = true;
            read[b] = true;
            continue;
        }
        cands.push(Cand { wi, a, b, kind });
    }

    let mut foldable: Vec<bool> = (0..nvars)
        .map(|v| !read[v] && binder[v].is_some() && matches!(plan.var_kinds[v], VarKind::Node))
        .collect();
    if !foldable.iter().any(|&f| f) {
        return;
    }

    // THE FIXPOINT. Each rule may un-fold a var; un-folding a var can invalidate
    // its parent (an expand onto a materialised var) and shift another var's
    // root, so iterate until nothing changes.
    loop {
        let mut changed = false;
        // The source var of the hop that binds `v` (only for a bound var).
        let parent = |v: usize| -> usize { plan.hops[binder[v].expect("bound var")].src };
        // The topmost foldable var on `v`'s ancestor chain: the end var of the
        // ROOT hop whose position the fold runs at.
        let root_end = |v: usize, foldable: &[bool]| -> usize {
            let mut cur = v;
            loop {
                let p = parent(cur);
                if foldable[p] {
                    cur = p;
                } else {
                    return cur;
                }
            }
        };
        // Whether `anc` is a FOLDED ancestor of `v` (so `bind[anc]` is set when
        // `v`'s level is entered).
        let is_ancestor = |anc: usize, v: usize, foldable: &[bool]| -> bool {
            let mut cur = v;
            loop {
                let p = parent(cur);
                if p == anc {
                    return foldable[p];
                }
                if !foldable[p] {
                    return false;
                }
                cur = p;
            }
        };
        for h in &plan.hops {
            if !foldable[h.src] {
                // A close FROM a materialised var ONTO a folded var reads it.
                if let Some(t) = h.tgt {
                    if foldable[t] {
                        foldable[t] = false;
                        changed = true;
                    }
                }
                continue;
            }
            // A hop that binds a RELATIONSHIP VARIABLE the query READS must
            // materialise. `semijoin` gives the closing rel one ROW per closing
            // edge with its id in a real column; the fold replaces those rows
            // with a weight, so all it can offer is a `NULL_ID` placeholder —
            // enough to keep the column layout (`run_hop_folded`), never enough
            // to answer `type(r)`. Un-folding the SOURCE is what materialises
            // the close: `h.fold` for a close is `foldable[h.src]`. (An EXPAND
            // that binds a rel var is already out — `read[e]` above.)
            let rel_var_read = h
                .rel_var
                .as_deref()
                .and_then(idx)
                .is_some_and(|vi| read[vi]);
            if h.tgt.is_some() && rel_var_read {
                foldable[h.src] = false;
                changed = true;
                continue;
            }
            match h.tgt {
                // An expand out of a folded var must itself fold.
                None => {
                    let e = h.end_vi.expect("an expand hop binds its end var");
                    if !foldable[e] {
                        foldable[h.src] = false;
                        changed = true;
                    }
                }
                // A close out of a folded var must target a var in `bind` by
                // then: the level's OWN var, a folded ancestor, or a
                // materialised var bound before the root hop (the position
                // rule).
                Some(t) if t == h.src => {
                    // A SELF-CLOSE `(b)-[:K]-(b)`. `hop_sum`'s close arm reads
                    // `bind[t]`, and the expand that entered this level set
                    // `bind[b] = peer` before calling `level`, so the target is
                    // bound by construction — the same case the inline
                    // candidates below spell as `other == level`. Rel-iso is
                    // unaffected: the close inherits the level's `used` when it
                    // is a tracked continuation (so the arriving self-loop is
                    // excluded) and takes it empty at a path boundary, exactly
                    // as the materialised semijoin does.
                }
                Some(t) => {
                    if foldable[t] && !is_ancestor(t, h.src, &foldable) {
                        // The target is folded but is NOT on this level's
                        // ancestor chain, so it is not in `bind` when the level
                        // runs. MATERIALISE THE TARGET rather than un-folding
                        // the source: the position rule below then decides on
                        // the next iteration. This is never the worse choice —
                        // un-folding the source materialises this same target
                        // anyway (the `!foldable[h.src]` arm above un-folds a
                        // folded close target) PLUS the source's whole chain,
                        // so the set this reaches is a subset of the other's.
                        foldable[t] = false;
                        changed = true;
                    } else if !foldable[t] && t >= root_end(h.src, &foldable) {
                        foldable[h.src] = false;
                        changed = true;
                    }
                }
            }
        }
        for c in &cands {
            let (fa, fb) = (foldable[c.a], foldable[c.b]);
            if !fa && !fb {
                continue; // both materialise: stays a chunk filter
            }
            // The LEVEL is the later-bound folded var; the OTHER must be in
            // `bind` there.
            let (level, other) = match (fa, fb) {
                (true, true) => {
                    if c.a >= c.b {
                        (c.a, c.b)
                    } else {
                        (c.b, c.a)
                    }
                }
                (true, false) => (c.a, c.b),
                _ => (c.b, c.a),
            };
            let ok = if other == level {
                true // a self-edge `(x)-[:T]->(x)`: the level's own node
            } else if foldable[other] {
                is_ancestor(other, level, &foldable)
            } else {
                other < root_end(level, &foldable)
            };
            if !ok {
                foldable[level] = false;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    if !foldable.iter().any(|&f| f) {
        return;
    }

    // APPLY: mark the folded hops, attach each inline conjunct to the hop that
    // binds its level var, and drop those conjuncts from the chunk filters.
    for h in plan.hops.iter_mut() {
        h.fold = match h.tgt {
            None => foldable[h.end_vi.expect("an expand hop binds its end var")],
            Some(_) => foldable[h.src],
        };
    }
    let mut dropped = vec![false; plan.wheres.len()];
    for c in cands {
        let (fa, fb) = (foldable[c.a], foldable[c.b]);
        if !fa && !fb {
            continue;
        }
        let (level, other) = match (fa, fb) {
            (true, true) => {
                if c.a >= c.b {
                    (c.a, c.b)
                } else {
                    (c.b, c.a)
                }
            }
            (true, false) => (c.a, c.b),
            _ => (c.b, c.a),
        };
        let pred = match c.kind {
            CandKind::Ne => InlinePred::NeBound(other),
            CandKind::Eq => InlinePred::EqBound(other),
            CandKind::Edge { dir, types, negate } => InlinePred::EdgeToBound {
                vi: other,
                // `dir` is from `a`'s side; when the level var is `b` the probe
                // starts at the far end, so the arrow flips.
                dir: if level == c.a {
                    dir
                } else {
                    match dir {
                        Dir::Out => Dir::In,
                        Dir::In => Dir::Out,
                        Dir::Both => Dir::Both,
                    }
                },
                types,
                negate,
            },
        };
        let hi = binder[level].expect("a folded var is bound by an expand hop");
        plan.hops[hi].inline.push(pred);
        dropped[c.wi] = true;
    }
    let wheres = std::mem::take(&mut plan.wheres);
    plan.wheres = wheres
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !dropped[*i])
        .map(|(_, w)| w)
        .collect();
}

/// One hop's inline predicates with their type tokens resolved once (only an
/// `EdgeToBound` carries tokens).
type InlinePreds = Vec<(InlinePred, Option<Vec<u32>>)>;

/// The fold's read-only lookups, built once per chunk build from the marked
/// hops (`FoldPlan::new`) and shared by every root's `fold_tail`.
struct FoldPlan<'a> {
    graph: &'a Graph,
    hops: &'a [Hop],
    /// Per hop: the end-label member set (sorted), the filter `expand` applies.
    members: Vec<Option<&'a crate::MembersView>>,
    /// Per hop: its resolved type tokens.
    tokens: &'a [Option<Vec<u32>>],
    /// Per var: the FOLDED hops sourced from it, in hop order.
    children: Vec<Vec<usize>>,
    /// Per hop: its inline predicates with their type tokens resolved once.
    preds: Vec<InlinePreds>,
    /// Per var: whether its level is a pure function of its node id (memoised).
    memo_ok: Vec<bool>,
    /// Per hop: a fold ROOT — folded, sourced from a MATERIALISED var. Each root
    /// runs its own `fold_tail` at its position; roots' weights multiply.
    root: Vec<bool>,
    /// Whether any inline predicate is an edge probe (operator B's counter).
    has_edge_pred: bool,
    /// The memo's id cap: the row budget (a level over it is simply not cached).
    memo_cap: usize,
    /// THE PROBE CAP. `Some(c)`: the fold may stop once the weights it has
    /// kept sum to `c` or more, leaving the remaining driving rows unwalked
    /// (dropped from the selection — they contribute nothing). Set only by
    /// [`constant_projection_over_count`] through the reserved
    /// [`COUNT_CAP_PARAM`], for which a total that is ANY value ≥ `skip+limit`
    /// is exactly as good as the exact one. A plain `count(*)` never carries
    /// it and always sums every row.
    cap: Option<u64>,
    /// The running sum the cap is judged against — shared by the parallel
    /// fold's morsel workers, so every worker stops at the same signal.
    reached: std::sync::atomic::AtomicU64,
}

/// The reserved parameter that carries a probe cap into the count fold —
/// see [`FoldPlan::cap`]. Reserved, not user-facing: a `$__count_cap` a
/// client sends would cap its own count, which is why the name is one no
/// generated statement uses and the projection that receives it is the
/// synthetic `count(*) AS __c` this crate builds.
pub(crate) const COUNT_CAP_PARAM: &str = "__count_cap";

/// The probe cap a statement's parameters carry, if any.
fn count_cap_from(params: &BTreeMap<String, Value>) -> Option<u64> {
    match params.get(COUNT_CAP_PARAM) {
        Some(Value::Int(n)) if *n > 0 => Some(*n as u64),
        _ => None,
    }
}

/// The recursion's mutable state — one per `fold_tail` call.
struct FoldState {
    /// Per var: the dense memo (`u64::MAX` = not computed); empty until used.
    memo: Vec<Vec<u64>>,
    /// Per var: the current binding — the driving row's materialised ids, then
    /// each folded level's node as the recursion enters it.
    bind: Vec<u64>,
    /// The current path's used rels (relationship isomorphism), pushed/popped
    /// as tracked hops descend; taken empty across a path boundary.
    used: Vec<u64>,
    /// Set on any `checked_*` failure — the whole fold then declines.
    overflow: bool,
    /// Whether a memo hit served a level (the memo counter).
    memo_used: bool,
}

impl<'a> FoldPlan<'a> {
    /// `min_vars` widens the per-var state (`bind`, `children`, `memo`) beyond
    /// what the hops themselves index. A driving chunk's columns are ALL copied
    /// into `bind`, so the state must be at least as wide as the chunk: the
    /// single-MATCH chain passes 0 (its every fold root is an EXPAND, whose end
    /// var index already exceeds the chunk's width at that position — a CLOSE
    /// is never a root, since a close's `fold` is its SOURCE's foldability and a
    /// foldable source is itself a folded expand's end), while an OPTIONAL leg
    /// passes the combined width, its roots being sourced from OUTER columns the
    /// leg's own hops need not mention.
    fn new(
        graph: &'a Graph,
        hops: &'a [Hop],
        hop_members: &'a [Option<crate::MembersView>],
        tokens: &'a [Option<Vec<u32>>],
        min_vars: usize,
        cap: Option<u64>,
    ) -> Self {
        // The binding order's width: every index the hops and their inline
        // predicates mention.
        let mut nvars = min_vars;
        for h in hops {
            nvars = nvars.max(h.src + 1);
            if let Some(t) = h.tgt {
                nvars = nvars.max(t + 1);
            }
            if let Some(e) = h.end_vi {
                nvars = nvars.max(e + 1);
            }
            for p in &h.inline {
                let o = match p {
                    InlinePred::NeBound(o) | InlinePred::EqBound(o) => *o,
                    InlinePred::EdgeToBound { vi, .. } => *vi,
                };
                nvars = nvars.max(o + 1);
            }
        }
        let mut folded_var = vec![false; nvars];
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); nvars];
        for (hi, h) in hops.iter().enumerate() {
            if h.fold {
                children[h.src].push(hi);
                if let Some(e) = h.end_vi {
                    folded_var[e] = true;
                }
            }
        }
        let root: Vec<bool> = hops.iter().map(|h| h.fold && !folded_var[h.src]).collect();
        let members: Vec<Option<&'a crate::MembersView>> = hop_members
            .iter()
            .map(|m| m.as_ref())
            .collect();
        let mut has_edge_pred = false;
        let preds: Vec<InlinePreds> = hops
            .iter()
            .map(|h| {
                h.inline
                    .iter()
                    .map(|p| {
                        let toks = match p {
                            InlinePred::EdgeToBound { types, .. } => {
                                has_edge_pred = true;
                                graph.type_tokens_peek(types)
                            }
                            _ => None,
                        };
                        (p.clone(), toks)
                    })
                    .collect()
            })
            .collect();
        // Path membership: path 0 starts at hop 0, every later path at a `reset`.
        let mut path_id = vec![0usize; hops.len()];
        let mut pid = 0usize;
        for (hi, h) in hops.iter().enumerate() {
            if hi > 0 && h.reset {
                pid += 1;
            }
            path_id[hi] = pid;
        }
        let memo_on = count_fold_memo_enabled();
        let memo_ok: Vec<bool> = (0..nvars)
            .map(|u| memo_on && folded_var[u] && memo_ok_for(u, hops, &children, &preds, &path_id))
            .collect();
        let memo_cap = graph.row_budget().unwrap_or(1 << 28);
        FoldPlan {
            graph,
            hops,
            members,
            tokens,
            children,
            preds,
            memo_ok,
            root,
            has_edge_pred,
            memo_cap,
            cap,
            reached: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

/// Whether folded level `u` is a pure function of its node id: every close and
/// inline predicate in its subtree names only subtree vars (or `u` itself),
/// and every TRACKED hop in the subtree has types disjoint from every OTHER hop
/// of its path — so neither the driving row's `bind` nor the inherited `used`
/// set can change the count.
fn memo_ok_for(
    u: usize,
    hops: &[Hop],
    children: &[Vec<usize>],
    preds: &[InlinePreds],
    path_id: &[usize],
) -> bool {
    let mut sub_vars: BTreeSet<usize> = BTreeSet::new();
    sub_vars.insert(u);
    let mut sub_hops: Vec<usize> = Vec::new();
    let mut stack = vec![u];
    while let Some(v) = stack.pop() {
        for &hi in &children[v] {
            sub_hops.push(hi);
            if let Some(e) = hops[hi].end_vi {
                if sub_vars.insert(e) {
                    stack.push(e);
                }
            }
        }
    }
    // An EMPTY type list is the UNTYPED hop `-[]-`, which matches EVERY type, so
    // it is disjoint from nothing — not, as `a.iter().all(..)` alone answers
    // (vacuously true when `a` is empty, and likewise when `b` is), disjoint
    // from everything. Getting this backwards leaves the memo ON for a level
    // whose count depends on the inherited `used` set.
    let disjoint = |a: &[String], b: &[String]| {
        !a.is_empty() && !b.is_empty() && a.iter().all(|t| !b.contains(t))
    };
    for &hi in &sub_hops {
        if let Some(t) = hops[hi].tgt {
            if !sub_vars.contains(&t) {
                return false;
            }
        }
        for (p, _) in &preds[hi] {
            let o = match p {
                InlinePred::NeBound(o) | InlinePred::EqBound(o) => *o,
                InlinePred::EdgeToBound { vi, .. } => *vi,
            };
            if !sub_vars.contains(&o) {
                return false;
            }
        }
        if hops[hi].track {
            for (gi, g) in hops.iter().enumerate() {
                if gi != hi && path_id[gi] == path_id[hi] && !disjoint(&g.types, &hops[hi].types) {
                    return false;
                }
            }
        }
    }
    true
}

/// The count fold's OVERFLOW REFUSAL. Every product and sum inside the fold is
/// `checked_*`; a failure means the true `count(*)` is at or past 2^63, which
/// the general path can neither enumerate (it would have to visit that many
/// walks) nor represent (`count(*)` accumulates in an `i64`). Declining to it
/// would trade a clear refusal for an unbounded scan that cannot finish, so the
/// fold REFUSES instead — the row-budget refusal (`budget_check`) is the
/// precedent and the same trade.
fn count_fold_overflow() -> RunError {
    counted!("interp.pipeline count fold refused an overflow");
    RunError::Semantic(
        "count(*) overflowed: this pattern has at least 2^63 matches, which no integer result can \
         represent and no enumeration can reach"
            .to_string(),
    )
}

/// The fold's dispatch for a FOLDED hop (the twin of `run_hop`, which the
/// materialised hops keep): a ROOT counts its subtree into the row weights
/// (`fold_tail`) and every folded hop appends its placeholder column(s).
fn run_hop_folded(
    chunk: DataChunk,
    hop: &Hop,
    hi: usize,
    plan: &FoldPlan<'_>,
) -> Result<DataChunk, RunError> {
    let mut chunk = if plan.root[hi] {
        fold_tail(chunk, plan, hi)?
    } else {
        chunk
    };
    if hop.tgt.is_none() {
        chunk.null_extend(&hop.var, VarKind::Node);
    }
    // A hop that binds a RELATIONSHIP variable still owes the Rel column
    // `expand`/`semijoin` appends (binding order: the node THEN the rel). A
    // folded CLOSE binds no node and so appends nothing above — without this
    // its rel column is missing and every var bound after it sits one index to
    // the left of the column the plan resolved for it. Nothing reads the
    // placeholder: `plan_count_fold` un-folds a close whose rel var the query
    // reads, and an expand's rel var forces its end var to materialise.
    if let Some(rv) = &hop.rel_var {
        chunk.null_extend(rv, VarKind::Rel);
    }
    Ok(chunk)
}

/// THE COUNT FOLD over one ROOT hop: for every live row, count the qualifying
/// walks through the root's subtree from the row's source node, multiply the
/// row's weight by that count, and DROP the row when it is zero. Runs at the
/// root hop's position, so a continuation root's isomorphism base is exactly
/// the row's `used_rels`. A `checked_*` overflow (or a weight past `i64`)
/// REFUSES the statement (`count_fold_overflow`) rather than declining.
fn fold_tail(
    mut chunk: DataChunk,
    plan: &FoldPlan<'_>,
    root: usize,
) -> Result<DataChunk, RunError> {
    let hop = &plan.hops[root];
    let mut weights: Vec<u64> = if chunk.weights.is_empty() {
        vec![1; chunk.row_count()]
    } else {
        std::mem::take(&mut chunk.weights)
    };
    // MORSEL-PARALLEL when every gate agrees (P-2 of the floor plan). Each
    // row's walk is independent — one `FoldState` per worker is correct
    // because the memo-eligible levels are pure functions of node id — and
    // the partials are concatenated IN MORSEL ORDER, so the kept selection
    // and weights are byte-identical to the serial loop below. The gates
    // mirror `expand`'s, each load-bearing:
    //   - its own lever (off by default; the digest and every published
    //     single-thread number run serial);
    //   - an INSTALLED executor (the engine never spawns);
    //   - no active transaction on this thread (its overlays and read-set
    //     are thread-local — a worker would silently read committed state);
    //   - enough driving rows for the split to beat its own overhead.
    let graph = plan.graph;
    if graph.parallel_fold_enabled()
        && !graph.in_txn()
        && chunk.selection.len() >= graph.parallel_min_rows()
    {
        if let Some(exec) = graph.exec() {
            counted!("interp.pipeline fold parallel");
            let workers = exec.width().min(chunk.selection.len()).max(1);
            let per = chunk.selection.len().div_ceil(workers);
            let morsels: Vec<&[usize]> = chunk.selection.chunks(per).collect();
            type Part = Result<(Vec<(usize, u64)>, bool), RunError>;
            let slots: Vec<std::sync::Mutex<Option<Part>>> =
                morsels.iter().map(|_| std::sync::Mutex::new(None)).collect();
            let ids_ref: &[Vec<u64>] = &chunk.ids;
            let used_ref: &[Vec<u64>] = &chunk.used_rels;
            let weights_ref: &[u64] = &weights;
            exec.for_each(morsels.len(), &|i| {
                let part = fold_rows(plan, hop, root, ids_ref, used_ref, weights_ref, morsels[i]);
                *slots[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(part);
            });
            let mut keep: Vec<usize> = Vec::with_capacity(chunk.selection.len());
            let mut memo_used = false;
            for m in slots {
                let part = m
                    .into_inner()
                    .unwrap_or_else(|e| e.into_inner())
                    .expect("every morsel ran — ScopedExec::for_each returns only when all have");
                let (kept, used_memo) = part?;
                memo_used |= used_memo;
                for (r, w) in kept {
                    weights[r] = w;
                    keep.push(r);
                }
            }
            chunk.selection = keep;
            chunk.weights = weights;
            if memo_used {
                counted!("interp.pipeline count fold memo");
            }
            return Ok(chunk);
        }
    }
    let (kept, memo_used) = fold_rows(
        plan,
        hop,
        root,
        &chunk.ids,
        &chunk.used_rels,
        &weights,
        &chunk.selection,
    )?;
    let mut keep: Vec<usize> = Vec::with_capacity(kept.len());
    for (r, w) in kept {
        weights[r] = w;
        keep.push(r);
    }
    chunk.selection = keep;
    chunk.weights = weights;
    if memo_used {
        counted!("interp.pipeline count fold memo");
    }
    Ok(chunk)
}

/// The fold's per-row body over one slice of driving rows — the whole loop
/// `fold_tail` used to inline, extracted so the serial path and every morsel
/// run EXACTLY the same code. Returns the kept `(row, folded weight)` pairs in
/// row order plus whether the memo served; a `checked_*` failure returns the
/// overflow refusal for the caller to propagate.
fn fold_rows(
    plan: &FoldPlan<'_>,
    hop: &Hop,
    root: usize,
    ids: &[Vec<u64>],
    used_rels: &[Vec<u64>],
    weights: &[u64],
    rows: &[usize],
) -> Result<(Vec<(usize, u64)>, bool), RunError> {
    let nvars = plan.children.len();
    let mut st = FoldState {
        memo: vec![Vec::new(); nvars],
        bind: vec![NULL_ID; nvars],
        used: Vec::new(),
        overflow: false,
        memo_used: false,
    };
    let mut kept: Vec<(usize, u64)> = Vec::with_capacity(rows.len());
    for &r in rows {
        // The probe cap: once the kept weights sum to it, the rest of the
        // driving rows are left unwalked. Checked BEFORE the row so a
        // parallel morsel stops at another worker's signal too.
        if let Some(cap) = plan.cap {
            if plan.reached.load(std::sync::atomic::Ordering::Relaxed) >= cap {
                counted!("pipeline.count fold stopped at the probe cap");
                break;
            }
        }
        // The row's materialised bindings (a folded placeholder is `NULL_ID`,
        // never read: the position rule keeps every referenced var real).
        for (vi, col) in ids.iter().enumerate() {
            st.bind[vi] = col[r];
        }
        // The isomorphism base for the root hop, as `expand` takes it; a reset
        // or untracked root empties it inside `hop_sum`.
        st.used.clear();
        if hop.track && !hop.reset {
            if let Some(u) = used_rels.get(r) {
                st.used.extend_from_slice(u);
            }
        }
        let w = hop_sum(plan, &mut st, root, ids[hop.src][r]);
        if st.overflow {
            return Err(count_fold_overflow());
        }
        if w == 0 {
            continue; // no walk through the subtree: the general path has no row
        }
        let Some(total) = weights[r]
            .checked_mul(w)
            .filter(|t| i64::try_from(*t).is_ok())
        else {
            return Err(count_fold_overflow());
        };
        kept.push((r, total));
        if plan.cap.is_some() {
            plan.reached
                .fetch_add(total, std::sync::atomic::Ordering::Relaxed);
        }
    }
    Ok((kept, st.memo_used))
}

/// THE OPTIONAL FOLD (operator D of `docs/lsqb-completeness-plan.md`) over ONE
/// OPTIONAL clause: count the leg's matches per outer row instead of producing
/// them, and emit ONE row per outer row carrying `max(1, matches)` as its
/// weight.
///
/// The `max(1, ·)` is the LEFT JOIN: an outer row with no match still produces
/// the null-fill row, which `count(*)` counts — so a zero-match row weighs 1,
/// never 0. (That is exactly why the fold is admitted only under `count(*)`:
/// `count(legvar)` would count the null-fill row as 0, and `max(1, ·)` would
/// then be wrong. `plan_optional_fold` gates on it.)
///
/// Each ROOT hop is one independent factor — the clause's comma paths are a
/// cartesian product per outer row, which is what the materialised left join
/// produces — so the leg's match count is their PRODUCT, and a zero factor
/// makes the whole leg unmatched. Every leg var's column is `NULL_ID`, exactly
/// as the merge's null-fill row writes it, so `combined_vars` indexing,
/// `nullable_agg_ok` and `row_ids` are untouched. Rows are never dropped:
/// a left join keeps every outer row.
fn fold_optional_leg(
    outer: DataChunk,
    outer_len: usize,
    plan: &FoldPlan<'_>,
    combined_vars: &[String],
    combined_var_kinds: &[VarKind],
) -> Result<DataChunk, RunError> {
    let nvars = plan.children.len();
    let mut st = FoldState {
        memo: vec![Vec::new(); nvars],
        bind: vec![NULL_ID; nvars],
        used: Vec::new(),
        overflow: false,
        memo_used: false,
    };
    let roots: Vec<usize> = (0..plan.hops.len()).filter(|&hi| plan.root[hi]).collect();
    let ncols = combined_vars.len();
    let live = outer.selection.len();
    let mut out_ids: Vec<Vec<u64>> = (0..ncols).map(|_| Vec::with_capacity(live)).collect();
    let mut out_w: Vec<u64> = Vec::with_capacity(live);
    for &r in &outer.selection {
        // The outer row's bindings; a folded leg var stays `NULL_ID` until the
        // recursion enters its level.
        for (vi, col) in outer.ids.iter().enumerate() {
            st.bind[vi] = col[r];
        }
        let mut legs = 1u64;
        for &hi in &roots {
            // Every root of an OPTIONAL leg re-seeds relationship isomorphism:
            // the clause is its own pattern (`left_join_null_extend` forces
            // `reset` on the first hop, and a later path's first hop carries it
            // already), so nothing the OUTER walk recorded can forbid a leg's
            // edge. `hop_sum` takes `used` empty for such a hop.
            debug_assert!(
                !plan.hops[hi].track || plan.hops[hi].reset,
                "an OPTIONAL leg's fold root must re-seed rel-iso"
            );
            st.used.clear();
            let w = hop_sum(plan, &mut st, hi, outer.ids[plan.hops[hi].src][r]);
            if st.overflow {
                return Err(count_fold_overflow());
            }
            match legs.checked_mul(w) {
                Some(x) => legs = x,
                None => return Err(count_fold_overflow()),
            }
            if legs == 0 {
                break; // no match through this factor: the leg null-fills
            }
        }
        let base = if outer.weights.is_empty() {
            1
        } else {
            outer.weights[r]
        };
        let Some(total) = base
            .checked_mul(legs.max(1))
            .filter(|t| i64::try_from(*t).is_ok())
        else {
            return Err(count_fold_overflow());
        };
        out_w.push(total);
        for (vi, col) in out_ids.iter_mut().enumerate() {
            col.push(if vi < outer_len {
                outer.ids[vi][r]
            } else {
                NULL_ID
            });
        }
    }
    if st.memo_used {
        counted!("interp.pipeline count fold memo");
    }
    counted!("interp.pipeline optional fold");
    let n = out_w.len();
    Ok(DataChunk {
        vars: combined_vars.to_vec(),
        var_kinds: combined_var_kinds.to_vec(),
        ids: out_ids,
        selection: (0..n).collect(),
        // A fresh round: each OPTIONAL clause is its own pattern, so the merge's
        // output carries no used rels and no provenance either.
        used_rels: vec![Vec::new(); n],
        prov: Vec::new(),
        weights: out_w,
    })
}

/// The weight of folded var `u`'s level at `node`: the product over its folded
/// child hops of each hop's walk count. Memoised per node when the level is a
/// pure function of it (`memo_ok`). Returns 0 with `overflow` set on any
/// `checked_*` failure.
fn level(plan: &FoldPlan<'_>, st: &mut FoldState, u: usize, node: u64) -> u64 {
    let memo_ok = plan.memo_ok[u];
    if memo_ok {
        if let Some(&m) = st.memo[u].get(node as usize) {
            if m != u64::MAX {
                st.memo_used = true;
                return m;
            }
        }
    }
    let mut w = 1u64;
    for k in 0..plan.children[u].len() {
        let ci = plan.children[u][k];
        let s = hop_sum(plan, st, ci, node);
        if st.overflow {
            return 0;
        }
        match w.checked_mul(s) {
            Some(x) => w = x,
            None => {
                st.overflow = true;
                return 0;
            }
        }
        if w == 0 {
            break; // a zero factor: the product is 0 whatever follows
        }
    }
    if memo_ok {
        let i = node as usize;
        if i < plan.memo_cap {
            if st.memo[u].len() <= i {
                st.memo[u].resize(i + 1, u64::MAX);
            }
            st.memo[u][i] = w;
        }
    }
    w
}

/// The walk count of folded hop `hi` from `node`: a close's closing-edge
/// multiplicity, or the sum over the hop's qualifying edges of the end level's
/// weight (1 for a leaf). The isomorphism `used` set is inherited for a tracked
/// continuation, taken empty at a path boundary or an untracked hop, and
/// extended by each traversed rel for the level below — `expand`'s `base` /
/// `out_used`, one recursion level at a time.
fn hop_sum(plan: &FoldPlan<'_>, st: &mut FoldState, hi: usize, node: u64) -> u64 {
    let graph = plan.graph;
    let hop = &plan.hops[hi];
    let tokens = &plan.tokens[hi];
    let inherits = hop.track && !hop.reset;
    let saved = if inherits {
        None
    } else {
        Some(std::mem::take(&mut st.used))
    };
    let out = if let Some(t) = hop.tgt {
        // A CLOSE inside the fold: the closing-edge multiplicity onto the bound
        // target, excluding used rels only when there are any to exclude.
        let want = st.bind[t];
        // THE MULTIPLICITY, and the two ways of asking for it.
        //
        // `edge_count_slim` answers it with two `partition_point`s on the sorted
        // CSR row — O(log deg). The walk exists only to EXCLUDE relationships
        // this row has already traversed, which the count cannot express.
        //
        // For `Both`, ask from the BOUND side: the predicate is symmetric in its
        // two nodes (an edge either way satisfies it) but `want` is fixed for the
        // whole subtree while `node` changes every call, so probing `want` reads
        // the SAME hot row instead of a random line of a 17M-entry CSR. Directed
        // probes are NOT symmetric and are left alone.
        let count_edges = |a: u64, b: u64| -> u64 {
            if matches!(hop.dir, Dir::Both) {
                graph.edge_count_slim(b, Dir::Both, tokens, a)
            } else if graph.directed_bound_probe() {
                // The SAME directed question from the bound endpoint's row:
                // `a-[T]->b` read from b's I row is the identical edge set,
                // and b (`want`) is fixed for the whole subtree while `a`
                // changes every call — the hot-row locality the undirected
                // close has had since v55, priced at ~166 ns/row (§25 plan).
                // `Dir::flipped` is what keeps it the same QUESTION; probing
                // the bound row without flipping answers the reverse edge.
                graph.edge_count_slim(b, hop.dir.flipped(), tokens, a)
            } else {
                graph.edge_count_slim(a, hop.dir, tokens, b)
            }
        };
        if !hop.track || st.used.is_empty() {
            count_edges(node, want)
        } else {
            // A non-empty isomorphism set means the count alone cannot answer:
            // an edge already traversed by this walk must not be counted again.
            // That USED to force a linear scan of every neighbour to find at
            // most one — ~12.8M times on LSQB q3's triangle close.
            //
            // It does not have to. The CSR row is sorted by peer, which is what
            // `edge_count_slim` already exploits; `edges_to_peer_slim` runs the
            // same two `partition_point`s and hands back the MATCHING entries,
            // so the exclusion is applied to one or two of them instead of to
            // thirty-six. A failing close costs the search alone; a succeeding
            // one costs the search plus its matches.
            //
            // No sparsity bet: pre-testing with a count was measured and
            // reverted (q3 -5%, but q4 +14%, q5 +8%, q7 +7%, q8 +6% — the extra
            // probe is pure overhead wherever closes succeed). This is cheaper
            // than the old path in BOTH cases.
            //
            // For `Both` the probe reads the BOUND node's row (fixed, hot)
            // rather than the level var's (random in a 17M-entry CSR). The
            // relationship ids are a property of the EDGE, not of which endpoint
            // was read, so `used` filters identically either way.
            let used = &st.used;
            let mut n = 0u64;
            let (probe_from, probe_to, probe_dir) = if matches!(hop.dir, Dir::Both) {
                (want, node, Dir::Both)
            } else if graph.directed_bound_probe() {
                // Bound-side row for the DIRECTED case too — same edge set,
                // flipped direction, hot row (see `count_edges` above). The
                // relationship ids the callback filters on are a property of
                // the EDGE, not of which endpoint was read.
                (want, node, hop.dir.flipped())
            } else {
                (node, want, hop.dir)
            };
            graph.edges_to_peer_slim(probe_from, probe_dir, tokens, probe_to, |e| {
                if !used.contains(&e.rel) {
                    n += 1;
                }
            });
            n
        }
    } else {
        let u = hop.end_vi.expect("a folded expand hop binds its end var");
        let members = plan.members[hi];
        let preds = &plan.preds[hi];
        let has_children = !plan.children[u].is_empty();
        let extends = hop.track;
        let mut total = 0u64;
        let mut probes = 0u64;
        graph.adjacent_slim_for_each(node, hop.dir, tokens, |e| {
            if st.overflow {
                return;
            }
            let peer = e.peer;
            if let Some(m) = members {
                probes += 1;
                if !graph.members_contains(m, peer) {
                    return; // the end-label filter, as `expand`
                }
            }
            if extends && st.used.contains(&e.rel) {
                return; // relationship isomorphism — this walk already used it
            }
            st.bind[u] = peer;
            for (p, ptok) in preds {
                if !pred_holds(graph, st, peer, p, ptok) {
                    return;
                }
            }
            let w = if has_children {
                if extends {
                    st.used.push(e.rel);
                }
                let w = level(plan, st, u, peer);
                if extends {
                    st.used.pop();
                }
                w
            } else {
                1
            };
            match total.checked_add(w) {
                Some(t) => total = t,
                None => st.overflow = true,
            }
        });
        crate::counters::MEMBERS_PROBES.fetch_add(probes, std::sync::atomic::Ordering::Relaxed);
        st.bind[u] = NULL_ID;
        total
    };
    if let Some(s) = saved {
        st.used = s;
    }
    out
}

/// Whether an inline predicate holds for the level's candidate node `peer`
/// against the fold's current bindings.
fn pred_holds(
    graph: &Graph,
    st: &FoldState,
    peer: u64,
    pred: &InlinePred,
    tokens: &Option<Vec<u32>>,
) -> bool {
    match pred {
        InlinePred::NeBound(o) => peer != st.bind[*o],
        InlinePred::EqBound(o) => peer == st.bind[*o],
        InlinePred::EdgeToBound { vi, dir, negate, .. } => {
            // PROBE FROM THE BOUND SIDE when the hop is UNDIRECTED.
            //
            // `edge_count_slim(a, Both, T, b)` and `edge_count_slim(b, Both, T,
            // a)` answer the same question: `Both` admits an edge in either
            // direction, so "is there a T between a and b" is symmetric in its
            // two arguments. Only the ROW LOOKED UP differs — and that decides
            // the cost.
            //
            // `peer` changes on every call; `st.bind[*vi]` is fixed for the
            // whole subtree below its binding. Looking up the bound node reads
            // the SAME adjacency row each time (hot in cache) and binary-searches
            // its ~36 neighbours, where looking up `peer` reads a different row
            // of a 17M-entry CSR per call — a miss each time. LSQB q3's triangle
            // close runs this ~12.8M times.
            //
            // ONLY for `Both`. A directed probe is NOT symmetric: `(a)-[:T]->(b)`
            // and `(b)-[:T]->(a)` are different questions, and swapping them
            // would answer the wrong one.
            let n = if matches!(dir, Dir::Both) {
                graph.edge_count_slim(st.bind[*vi], Dir::Both, tokens, peer)
            } else if graph.directed_bound_probe() {
                // The directed question from the BOUND row with the direction
                // flipped — the same edge set, read from the endpoint that is
                // fixed for the whole subtree (see `count_edges` in `hop_sum`).
                graph.edge_count_slim(st.bind[*vi], dir.flipped(), tokens, peer)
            } else {
                graph.edge_count_slim(peer, *dir, tokens, st.bind[*vi])
            };
            (n > 0) != *negate
        }
    }
}

#[cfg(test)]
mod count_reorder_tests {
    //! The COUNT-ONLY REORDER's TERMINATION property, which no end-to-end test
    //! can observe: `plan_and_run_columnar` re-plans the rewritten query, so a
    //! pass that rewrote its own output would recurse forever. It cannot,
    //! because the pass is IDEMPOTENT — its output's seed is already its first
    //! var, its paths are already oriented at their bound end, and the greedy
    //! re-derives the same order from the same costs — and a rewrite EQUAL to
    //! its input returns `None`. Pinned here, where the pass can be applied to
    //! its own output directly.
    use super::*;
    use engram_cypher::parse_statement;
    use engram_key::{Namespace, Realm};
    use engram_store::Store;

    fn single(src: &str) -> SingleQuery {
        match parse_statement(src).expect("parse") {
            engram_cypher::stmt::Query::Single(s) => s,
            other => panic!("not a single query: {other:?}"),
        }
    }

    #[test]
    fn the_reorder_declines_its_own_output() {
        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        let e = BTreeMap::new();
        let country = g.create_node(&["Country".into()], &e).expect("country");
        let city = g.create_node(&["City".into()], &e).expect("city");
        let p1 = g.create_node(&["Person".into()], &e).expect("p1");
        let p2 = g.create_node(&["Person".into()], &e).expect("p2");
        g.create_rel(city, "IS_PART_OF", country, &e).expect("ipo");
        g.create_rel(p1, "IS_LOCATED_IN", city, &e).expect("ili");
        g.create_rel(p2, "IS_LOCATED_IN", city, &e).expect("ili");
        g.create_rel(p1, "KNOWS", p2, &e).expect("knows");
        let q = single(
            "MATCH (country:Country), \
             (person1:Person)-[:IS_LOCATED_IN]->(city1:City)-[:IS_PART_OF]->(country), \
             (person2:Person)-[:IS_LOCATED_IN]->(city2:City)-[:IS_PART_OF]->(country), \
             (person1)-[:KNOWS]-(person2) \
             RETURN count(*) AS n",
        );
        let once = reorder_for_count_only(&g, &q).expect("the bare path is dropped");
        assert!(
            reorder_for_count_only(&g, &once).is_none(),
            "the pass must decline its own output, or the re-plan recurses"
        );
    }
}

#[cfg(test)]
mod count_fold_tests {
    //! The two fold properties no corpus can reach.
    //!
    //! OVERFLOW: a driving row's weight is the product of every fold it has
    //! passed; a prior fold can leave it anywhere up to `i64::MAX`, and the next
    //! multiplication must REFUSE rather than wrap, saturate, or decline. It is
    //! not a decline because the general path could neither enumerate 2^63 walks
    //! nor represent the answer in `count(*)`'s `i64` — the refusal is the only
    //! honest outcome. Pinned in-crate where the pre-existing weight can be set
    //! directly.
    //!
    //! MEMO DISJOINTNESS: `memo_ok_for` compares two hops' TYPE LISTS, and an
    //! empty list is the untyped hop that matches every type. `collect_hops`
    //! declines a typeless hop today, so this is unreachable from Cypher and is
    //! pinned here against the day it is not.
    use super::*;
    use engram_key::{Namespace, Realm};
    use engram_store::Store;

    fn two_walk_fold() -> (Graph, Vec<Hop>, u64) {
        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        let e = BTreeMap::new();
        let a = g.create_node(&["A".into()], &e).expect("a");
        let b1 = g.create_node(&["B".into()], &e).expect("b1");
        let b2 = g.create_node(&["B".into()], &e).expect("b2");
        g.create_rel(a, "R", b1, &e).expect("r1");
        g.create_rel(a, "R", b2, &e).expect("r2");
        let hops = vec![Hop {
            src: 0,
            dir: Dir::Out,
            types: vec!["R".to_string()],
            labels: Vec::new(),
            var: "b".to_string(),
            rel_var: None,
            track: false,
            reset: false,
            tgt: None,
            varlen: None,
            end_vi: Some(1),
            fold: true,
            inline: Vec::new(),
        }];
        (g, hops, a)
    }

    /// The refusal's shape, asserted once so every site below can just match on
    /// it: a `Semantic` error that NAMES the overflow (a caller reading the
    /// message must be able to tell it from the row-budget refusal).
    fn is_overflow_refusal(e: &RunError) -> bool {
        matches!(e, RunError::Semantic(m) if m.contains("overflow"))
    }

    #[test]
    fn fold_tail_counts_two_walks_and_refuses_past_i64() {
        let (g, hops, a) = two_walk_fold();
        let members = vec![None];
        let tokens = vec![g.type_tokens_peek(&hops[0].types)];
        let plan = FoldPlan::new(&g, &hops, &members, &tokens, 0, None);
        assert!(plan.root[0], "a folded hop off the seed is a root");

        // A fresh row weighs 1: the fold leaves it at the walk count, 2.
        let chunk = DataChunk::seed("a", vec![a]);
        let out = fold_tail(chunk, &plan, 0).expect("2 walks fit");
        assert_eq!(out.weights, vec![2]);
        assert_eq!(out.selection, vec![0]);

        // A row already at `i64::MAX` × 2 leaves `i64`: REFUSE, never wrap and
        // never hand a count of 2^63 to a path that cannot reach it.
        let mut chunk = DataChunk::seed("a", vec![a]);
        chunk.weights = vec![i64::MAX as u64];
        let Err(err) = fold_tail(chunk, &plan, 0) else {
            panic!("a count past i64 must refuse");
        };
        assert!(is_overflow_refusal(&err), "{err:?}");
        // …and a product past `u64` (the `checked_mul` itself) likewise.
        let mut chunk = DataChunk::seed("a", vec![a]);
        chunk.weights = vec![u64::MAX / 2 + 1];
        let Err(err) = fold_tail(chunk, &plan, 0) else {
            panic!("a product past u64 must refuse");
        };
        assert!(is_overflow_refusal(&err), "{err:?}");
        // The largest weight that still fits is kept exactly.
        let mut chunk = DataChunk::seed("a", vec![a]);
        chunk.weights = vec![(i64::MAX as u64) / 2];
        let out = fold_tail(chunk, &plan, 0).expect("fits");
        assert_eq!(out.weights, vec![(i64::MAX as u64) / 2 * 2]);
    }

    #[test]
    fn weighted_count_star_refuses_past_i64_in_the_reducer() {
        let (g, _hops, a) = two_walk_fold();
        let mut chunk = DataChunk::seed("a", vec![a]);
        chunk.weights = vec![i64::MAX as u64];
        let mut accs = vec![SiteAcc::CountStar(1)];
        let arg_vals: Vec<SiteArgVal> = vec![SiteArgVal::Star];
        let distinct: Vec<Vec<u64>> = vec![Vec::new()];
        // 1 + i64::MAX overflows the accumulator: refuse.
        let err = fold_row_weighted(&mut accs, &arg_vals, &distinct, &chunk, 0)
            .expect_err("a total past i64 must refuse");
        assert!(is_overflow_refusal(&err), "{err:?}");
        // A weight that does not even fit `i64` on its own, likewise.
        let mut over = DataChunk::seed("a", vec![a]);
        over.weights = vec![i64::MAX as u64 + 1];
        let mut accs = vec![SiteAcc::CountStar(0)];
        let err = fold_row_weighted(&mut accs, &arg_vals, &distinct, &over, 0)
            .expect_err("a weight past i64 must refuse");
        assert!(is_overflow_refusal(&err), "{err:?}");
        // The largest total that fits is still accumulated exactly, and the
        // STRUCTURAL decline (a non-`count(*)` site meeting a weight) stays a
        // decline — it is a plan the fold never builds, not an overflow.
        let mut accs = vec![SiteAcc::CountStar(0)];
        assert!(
            fold_row_weighted(&mut accs, &arg_vals, &distinct, &chunk, 0)
                .expect("no error")
                .is_some()
        );
        let SiteAcc::CountStar(n) = accs[0] else {
            panic!("count star");
        };
        assert_eq!(n, i64::MAX);
        let _ = g;
    }

    /// `memo_ok_for`'s disjointness test over TYPE LISTS. An empty list is the
    /// untyped hop `-[]-`, which matches every type: it is disjoint from
    /// NOTHING. Read the other way (`a.iter().all(..)` is vacuously true on an
    /// empty list) it would leave the memo ON for a level whose count depends on
    /// the inherited `used` set — the level would then be cached per node id and
    /// re-served to a driving row with a different `used`.
    #[test]
    fn an_untyped_tracked_hop_is_disjoint_from_nothing() {
        let mk = |types: &[&str], src: usize, end: usize, track: bool| Hop {
            src,
            dir: Dir::Out,
            types: types.iter().map(|t| (*t).to_string()).collect(),
            labels: Vec::new(),
            var: format!("v{end}"),
            rel_var: None,
            track,
            reset: false,
            tgt: None,
            varlen: None,
            end_vi: Some(end),
            fold: true,
            inline: Vec::new(),
        };
        // Two hops of ONE path, the tracked subtree hop under var 1.
        let memo_ok = |t0: &[&str], t1: &[&str]| {
            let hops = vec![mk(t0, 0, 1, true), mk(t1, 1, 2, true)];
            let children = vec![vec![0usize], vec![1usize], Vec::new()];
            let preds: Vec<InlinePreds> = vec![Vec::new(), Vec::new()];
            let path_id = vec![0usize, 0usize];
            memo_ok_for(1, &hops, &children, &preds, &path_id)
        };
        // Genuinely disjoint single types: the memo may stay on.
        assert!(memo_ok(&["K"], &["M"]), "K and M share no type");
        // A shared type: off, as before.
        assert!(!memo_ok(&["K"], &["K"]), "K and K share K");
        assert!(!memo_ok(&["K", "M"], &["M"]), "K|M and M share M");
        // THE FIX. An untyped hop matches every type, so it overlaps both a
        // typed sibling and another untyped hop — neither may memoise.
        assert!(!memo_ok(&[], &["K"]), "an untyped hop overlaps :K");
        assert!(!memo_ok(&["K"], &[]), "an untyped TRACKED hop overlaps :K");
        assert!(!memo_ok(&[], &[]), "two untyped hops overlap each other");
    }
}

/// SCAN a labelled start (ASCENDING), run the ordered expand/semijoin steps, and
/// apply the one-variable WHERE — the read chunk both the aggregate and the
/// OPTIONAL outer chain build. An empty a-set OR an unminted hop type seeds an
/// EMPTY chunk (the expand loop still appends every var column, so it carries the
/// full binding order with zero rows) — reproducing the general path's empty
/// result without a special case. `Ok(None)` = a WHERE budget / non-boolean
/// decline (the caller returns None and the general path answers identically).
fn build_chunk(
    graph: &Graph,
    a_labels: &[String],
    a_var: &str,
    hops: &[Hop],
    wheres: &[WherePred],
    anchor: Option<&PropAnchor>,
    params: &BTreeMap<String, Value>,
) -> Result<Option<DataChunk>, RunError> {
    // Fix 48: when the first hop's TYPE has far fewer edges than the seed
    // label has members, the hop table's sources ARE the seed — every
    // other member expands to nothing. The seed's predicates then run over
    // that population (as over a seek's ids). The label scan and its
    // per-member predicate never happen.
    let (seed_ids, sought) = match hop_table_seed(graph, a_labels, hops)? {
        Some(ids) => (ids, true),
        None => anchored_seed_ids(graph, a_labels, anchor, params)?,
    };
    // The seed var's OWN predicates, answered from the property-column
    // cache (or a seek) BEFORE the chunk exists, and then DROPPED: applied
    // as `WherePred`s they went through `load_var_columns`, which has no
    // cache, so `MATCH (t:ResearchTask {userId: $u})-[:PROPOSED_GRAPH_WRITE]
    // ->(p) RETURN count(p)` point-gathered the whole label's `userId` (517
    // record reads) on every statement — 6.8 ms against Neo4j's 1.7 for the
    // 416 survivors. A SCANNED label filters as a whole-label walk (`over`
    // None), so the columns it assembles are kept for the next statement; a
    // sought seed filters over its ids. Strict: a non-boolean row declines
    // the whole shortcut and the predicates run where they always did.
    let own: Vec<usize> = wheres
        .iter()
        .enumerate()
        .filter(|(_, p)| {
            p.var == a_var && p.id_other.is_none() && p.edge.is_none() && !p.props.is_empty()
        })
        .map(|(i, _)| i)
        .collect();
    let mut seed_ids = seed_ids;
    let mut rest: Option<Vec<WherePred>> = None;
    if !own.is_empty() {
        let pred = own
            .iter()
            .map(|&i| wheres[i].expr.clone())
            .reduce(|a, b| Expr::And(Box::new(a), Box::new(b)))
            .expect("non-empty");
        let over = if sought {
            Some(std::sync::Arc::new(seed_ids.clone()))
        } else {
            None
        };
        if let Some(ids) =
            crate::batch::filter_ids_strict(graph, a_labels, a_var, &pred, params, over)?
        {
            counted!("interp.pipeline seed predicates filtered by columns");
            seed_ids = ids.as_ref().clone();
            rest = Some(
                wheres
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !own.contains(i))
                    .map(|(_, p)| p.clone())
                    .collect(),
            );
        }
    }
    let wheres: &[WherePred] = rest.as_deref().unwrap_or(wheres);
    // A whole-label (or index-anchored) scan seeds a FRESH clause (empty per-row
    // `used_rels`).
    build_chunk_from_ids_labelled(
        graph,
        a_var,
        seed_ids,
        &[],
        hops,
        wheres,
        params,
        Some(a_labels),
    )
}

/// The scan seed for a labelled start: the label members ASCENDING, OR — when a
/// seekable source-property anchor is present and the smallest applicable label
/// is above the seek floor — the range-index probe INTERSECTED with the label
/// members (kept ascending). Reuses interp's `Seed::PropEq` machinery verbatim:
/// `property_seek_enabled` / `property_seek_worth_probing` gate the probe and
/// `index_probe_in` resolves the ids (the primitive `PropEq` resolves through).
/// The probe spans all labels, so the intersection re-imposes the label; the
/// anchor equality still runs as a source `WherePred` in `build_chunk_from_ids`,
/// so the seed's RESULT equals a whole-label scan then that filter — the seek is
/// a pure performance choice. A non-seekable value, a probe over the cap, an
/// eval failure, or a label below the floor falls back to the whole-label scan.
fn anchored_seed_ids(
    graph: &Graph,
    a_labels: &[String],
    anchor: Option<&PropAnchor>,
    params: &BTreeMap<String, Value>,
) -> Result<(Vec<u64>, bool), RunError> {
    // The VIEW, not a materialisation. This function used to open by calling
    // `to_arc_vec()` unconditionally — before it knew whether it would seek or
    // scan — and the seek path then used the result for nothing but
    // `binary_search`, which is a membership TEST. On `:Message` at SF1 that is
    // 3.06M ids merged and copied to answer a probe of a handful, and the scan
    // path cloned the result a SECOND time. Measured: `mem_flat_rows` reported
    // 2.03 BILLION ids copied in a 30 s `read-heavy` run — 16 GB of memcpy —
    // from 665 materialisations of exactly 3.06M each.
    let members = graph.members_all(a_labels).map_err(RunError::Graph)?;
    // One copy, and only on the branch that genuinely needs owned ids. `iter`
    // merges the overlay as it goes, where `to_arc_vec().clone()` merged into a
    // fresh vector and then copied that.
    let scan = || -> Vec<u64> {
        let out: Vec<u64> = members.iter().collect();
        counted!("interp.pipeline anchored seed scanned the whole label");
        crate::counters::SEED_SCAN_ROWS
            .fetch_add(out.len() as u64, std::sync::atomic::Ordering::Relaxed);
        out
    };
    let Some(anchor) = anchor else {
        return Ok((scan(), false));
    };
    if !graph.property_seek_enabled() {
        return Ok((scan(), false));
    }
    // Probe only above the seek's label floor (as `run_streaming` does) — the
    // SMALLEST applicable label decides, mirroring interp's `label_fallback`.
    let probe_label = smallest_label_name(graph, a_labels);
    if !graph.property_seek_worth_probing(probe_label.as_deref()) {
        return Ok((scan(), false));
    }
    // The var-free anchor values (literals or params) evaluate once with an empty
    // binding scope. An eval failure (e.g. an unknown param) falls back to the
    // scan; the source `WherePred` then reproduces run_streaming's identical error.
    let empty_vm = VarMap::new();
    let scope = Scope::over(params, &empty_vm, graph.wall_ms(), graph.zone_provider());
    let mut vals: Vec<Value> = Vec::with_capacity(anchor.values.len());
    for e in &anchor.values {
        match eval_with(e, &scope, None) {
            Ok(v) => vals.push(v),
            Err(_) => return Ok((scan(), false)),
        }
    }
    // A DECLARED index on a label this pattern requires is probed SCOPED — the
    // one rule every seek site follows (`Graph::declared_scope_for`).
    let scope = graph
        .declared_scope_for(a_labels, &anchor.prop)
        .map_err(RunError::Graph)?;
    let probed = match scope.as_deref() {
        Some(l) => {
            counted!("interp.pipeline anchored seed probed a declared scoped index");
            graph.index_probe_in_scoped(
                &anchor.prop,
                &vals,
                Some(crate::PROPERTY_SEEK_MAX_PROBE),
                Some(l),
            )
        }
        None => graph.index_probe_in(&anchor.prop, &vals, Some(crate::PROPERTY_SEEK_MAX_PROBE)),
    }
    .map_err(RunError::Graph)?;
    match probed {
        Some(ids) => {
            // Intersect the (all-label) probe with the label members, ASCENDING
            // (both `index_probe_in` and `members_all` are id-sorted; filtering
            // the sorted probe by the member set preserves the order). The probe
            // is SMALL (a single-seed anchor), so test each id by BINARY SEARCH
            // over the sorted members rather than materialising a member-set: on
            // a large label a per-query 500k-element `BTreeSet` was the dominant
            // allocation and serialised the concurrent single-seed path.
            counted!("interp.pipeline anchored seed sought a property index");
            Ok((
                ids.into_iter()
                    .filter(|id| graph.members_contains(&members, *id))
                    .collect(),
                true,
            ))
        }
        None => Ok((scan(), false)), // over cap / non-servable — fall back to the scan
    }
}

/// A type's edges must be this many times fewer than the label's members for
/// its table to seed the chain (fix 48).
const SEED_FROM_TABLE_RATIO: u64 = 16;

/// Fix 48: the seed a SPARSE first hop offers — the ids that have at least
/// one edge of the hop's type(s) in its direction, restricted to the seed
/// label — or `None` when the first hop does not leave the seed var, is
/// undirected / untyped / variable-length, the type is dense relative to the
/// label, or no table may serve. `MATCH (n:UserDataNode {userId: $u})
/// -[r:REPLIED_TO]->(t) RETURN count(r)` on the mirror: the `userId` seek
/// names 38,297 of 38,614 emails (over the cap), so the seed scanned the
/// label and evaluated the predicate 38,337 times, then expanded every
/// email through a REPLIED_TO table holding a handful of edges — 9–14 ms
/// against Neo4j's 1.0–1.7, which drives from the relationship type.
fn hop_table_seed(
    graph: &Graph,
    a_labels: &[String],
    hops: &[Hop],
) -> Result<Option<Vec<u64>>, RunError> {
    let Some(h) = hops.first() else {
        return Ok(None);
    };
    if h.src != 0 || h.types.is_empty() || h.varlen.is_some() || h.tgt.is_some() {
        return Ok(None);
    }
    let tag = match h.dir {
        Dir::Out => b'O',
        Dir::In => b'I',
        Dir::Both => return Ok(None),
    };
    let Some(label) = smallest_label_name(graph, a_labels) else {
        return Ok(None);
    };
    let members_n = graph.count_label_nodes(&label);
    // The type's edge count from the count store (no walk).
    let edges = graph
        .count_hop(&[], h.dir, &h.types, &[])
        .map_err(RunError::Graph)?;
    // A type with NO edge at all seeds NOTHING: no member can expand, so
    // the label scan and its per-member predicate would be spent on an
    // empty answer. The mirror's REPLIED_TO holds zero edges, and `(n:
    // UserDataNode {userId: $u})-[:REPLIED_TO]->(t) RETURN count(r)` still
    // scanned 38k emails for it (12.8 ms against Neo4j's 1.5) on v116,
    // where this returned `None` for a zero count.
    if edges == 0 {
        counted!("interp.pipeline seed driven from the hop's table");
        counted!("interp.pipeline seed emptied by an edgeless type");
        return Ok(Some(Vec::new()));
    }
    if edges.saturating_mul(SEED_FROM_TABLE_RATIO) >= members_n {
        return Ok(None);
    }
    let tokens = graph.type_tokens_peek(&h.types);
    let Some(mut sources) = graph.hop_table_sources(tag, &tokens, members_n as usize) else {
        return Ok(None);
    };
    let members = graph.members_all(a_labels).map_err(RunError::Graph)?;
    sources.retain(|id| graph.members_contains(&members, *id));
    counted!("interp.pipeline seed driven from the hop's table");
    Ok(Some(sources))
}

/// The applicable label with the FEWEST live nodes — interp's `smallest_label`,
/// the cheapest scan and the one the seek's worth-probing floor is judged against.
fn smallest_label_name(graph: &Graph, labels: &[String]) -> Option<String> {
    labels
        .iter()
        .min_by_key(|l| graph.count_label_nodes(l))
        .cloned()
}

/// SCAN from an EXPLICIT seed id set (a Node var) rather than a whole-label scan,
/// then run the ordered expand/semijoin steps and the one-variable WHERE — the
/// seeded twin of [`build_chunk`], used to build a JOIN side (`run_join`) rooted
/// at the ids the OTHER side produced (a semi-join pushdown). Byte-identical to
/// what `build_chunk` would do from the same seed.
///
/// `seed_used` is the per-seed-row `used_rels` the seed carries (aligned to
/// `seed_ids`): an EMPTY slice means a FRESH clause (empty per-row `used`), which
/// is the correct per-MATCH-clause relationship-isomorphism reset — `run_streaming`
/// starts each MATCH's `Partial.used` empty (`match_path`), so chainB may reuse an
/// edge chainA walked. A non-empty `seed_used` (only the rel-iso CANARY supplies
/// one) would inherit the other side's traversed rels and is NOT the semantics.
///
/// An empty seed OR an unminted hop type seeds an EMPTY chunk (the expand loop
/// still appends every var column, carrying the full binding order with zero
/// rows). `Ok(None)` = a WHERE budget / non-boolean decline.
fn build_chunk_from_ids(
    graph: &Graph,
    seed_var: &str,
    seed_ids: Vec<u64>,
    seed_used: &[Vec<u64>],
    hops: &[Hop],
    wheres: &[WherePred],
    params: &BTreeMap<String, Value>,
) -> Result<Option<DataChunk>, RunError> {
    build_chunk_from_ids_labelled(graph, seed_var, seed_ids, seed_used, hops, wheres, params, None)
}

/// [`build_chunk_from_ids`] with the seed var's pattern labels, when known:
/// every predicate over a labelled var (the seed, or a hop's end var) is then
/// answered through the column cache (`DataChunk::filter_labelled`).
#[allow(clippy::too_many_arguments)]
fn build_chunk_from_ids_labelled(
    graph: &Graph,
    seed_var: &str,
    seed_ids: Vec<u64>,
    seed_used: &[Vec<u64>],
    hops: &[Hop],
    wheres: &[WherePred],
    params: &BTreeMap<String, Value>,
    seed_labels: Option<&[String]>,
) -> Result<Option<DataChunk>, RunError> {
    // The pattern labels each var is bound under — the seed's, and each
    // new-var hop's end labels — so a predicate over it can be answered from
    // the label's cached columns. A close hop (`tgt` set) binds no new var.
    let mut labels_of: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Some(l) = seed_labels.filter(|l| !l.is_empty()) {
        labels_of.insert(seed_var.to_string(), l.to_vec());
    }
    for hop in hops {
        if hop.tgt.is_none() && !hop.labels.is_empty() {
            labels_of.insert(hop.var.clone(), hop.labels.clone());
        }
    }
    let mut empty = seed_ids.is_empty();
    let mut hop_members: Vec<Option<crate::MembersView>> = Vec::with_capacity(hops.len());
    let mut hop_tokens: Vec<Option<Vec<u32>>> = Vec::with_capacity(hops.len());
    for hop in hops {
        let members = if hop.labels.is_empty() {
            None
        } else {
            Some(graph.members_all(&hop.labels).map_err(RunError::Graph)?)
        };
        let tokens = graph.type_tokens_peek(&hop.types);
        if matches!(&tokens, Some(v) if v.is_empty()) {
            empty = true; // a named type never minted — no adjacency for the hop
        }
        hop_members.push(members);
        hop_tokens.push(tokens);
    }
    // An empty start (or unminted-type hop) seeds no ids; the expand loop then
    // appends every var column over zero rows, so the chunk still carries the
    // full binding order downstream. This avoids exercising an unminted hop.
    let a_ids_vec = if empty { Vec::new() } else { seed_ids };
    let mut chunk = DataChunk::seed(seed_var, a_ids_vec);
    // The per-clause rel-iso RESET seam: normally empty (a fresh clause), so the
    // seed keeps `DataChunk::seed`'s empty per-row `used`. Applied only when it
    // aligns to the (post-empty-check) seed row count.
    if !seed_used.is_empty() && seed_used.len() == chunk.row_count() {
        chunk.used_rels = seed_used.to_vec();
    }
    // Apply EACH WHERE predicate as EARLY as ALL its referenced vars are bound —
    // the generalisation of the rev-119 single-pred pushdown to a conjunction. A
    // source-var predicate (an anchor like `a.id = x`) filters the seed BEFORE the
    // hops expand — so a var-length BFS runs from the anchored source, not the
    // whole label; a two-var predicate (`a <> b`) applies right after the hop
    // binding its second var. Applying a predicate later than its earliest-bound
    // position is byte-identical (each source's rows are a contiguous block, so
    // row set AND order are unchanged), but it materialises the pre-filter
    // fan-out; for an anchored var-length that OVERFLOWS the row budget — ON would
    // error where the general path, which seeds the anchor, succeeds. So each is
    // pushed to its earliest position.
    let mut pending: Vec<(usize, Vec<String>)> = wheres
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let mut vs = vec![p.var.clone()];
            if let Some((other, _)) = &p.id_other {
                vs.push(other.clone());
            }
            if let Some(e) = &p.edge {
                vs.push(e.dst.clone());
            }
            (i, vs)
        })
        .collect();
    // Before the hops: every predicate over the seed (scan) var alone applies now.
    if apply_ready_preds(graph, params, &mut chunk, &mut pending, wheres, &labels_of)?.is_none() {
        return Ok(None); // a budget / non-boolean decline
    }
    // THE COUNT FOLD's lookups, built once when any hop is folded
    // (`plan_count_fold` marked it); `None` = every hop materialises as before.
    let fold_plan = if hops.iter().any(|h| h.fold) {
        Some(FoldPlan::new(graph, hops, &hop_members, &hop_tokens, 0, count_cap_from(params)))
    } else {
        None
    };
    for (i, hop) in hops.iter().enumerate() {
        let members_slice: Option<&crate::MembersView> = hop_members[i].as_ref();
        chunk = match &fold_plan {
            // A FOLDED hop: its ROOT (source var materialised) counts the whole
            // folded subtree into the row weights here, at its own position
            // (so a continuation hop's rel-iso base is exactly this row's
            // `used_rels`); every folded hop then appends its placeholder
            // column(s). A fold that overflows REFUSES the statement — the
            // general path could neither finish nor represent that count.
            Some(fp) if hop.fold => run_hop_folded(chunk, hop, i, fp)?,
            _ => run_hop(graph, chunk, hop, members_slice, &hop_tokens[i])?,
        };
        if apply_ready_preds(graph, params, &mut chunk, &mut pending, wheres, &labels_of)?
            .is_none()
        {
            return Ok(None);
        }
    }
    if let Some(fp) = &fold_plan {
        counted!("interp.pipeline count fold");
        if fp.has_edge_pred {
            counted!("interp.pipeline edge pred inline");
        }
    }
    // A predicate whose var(s) never bound in this chain is a caller bug, but keep
    // the old fallback (apply once at the end) rather than silently dropping it.
    for (wi, _) in std::mem::take(&mut pending) {
        let labels = labels_of.get(&wheres[wi].var).map(Vec::as_slice);
        if chunk.filter_labelled(graph, params, &wheres[wi], labels)?.is_none() {
            return Ok(None);
        }
    }
    Ok(Some(chunk))
}

/// Apply every still-pending predicate whose referenced vars are ALL bound in
/// `chunk` now, removing each from `pending` as it runs — the shared step
/// `build_chunk_from_ids` calls before the hops and after each one. `Ok(None)` =
/// a filter budget / non-boolean decline (the caller returns None; the general
/// path errors identically). A predicate not yet ready is left for a later hop.
/// `labels_of` maps a var to the pattern labels it is bound under, so its
/// predicates can be answered from the label's cached columns.
fn apply_ready_preds(
    graph: &Graph,
    params: &BTreeMap<String, Value>,
    chunk: &mut DataChunk,
    pending: &mut Vec<(usize, Vec<String>)>,
    wheres: &[WherePred],
    labels_of: &BTreeMap<String, Vec<String>>,
) -> Result<Option<()>, RunError> {
    let mut i = 0;
    while i < pending.len() {
        let ready = pending[i]
            .1
            .iter()
            .all(|v| chunk.vars.iter().any(|x| x == v));
        if ready {
            let (wi, _) = pending.remove(i);
            let labels = labels_of.get(&wheres[wi].var).map(Vec::as_slice);
            if chunk.filter_labelled(graph, params, &wheres[wi], labels)?.is_none() {
                return Ok(None);
            }
        } else {
            i += 1;
        }
    }
    Ok(Some(()))
}

/// Reduce an already-built read chunk into groups and project through the shared
/// aggregating tail — the group-by-aggregate's post-chunk half, shared by
/// `run_aggregate` (whole scan) and the OPTIONAL left-join (outer chunk + null
/// fill). `Ok(None)` = a column-budget / type decline.
fn run_aggregate_over_chunk(
    graph: &Graph,
    plan: &AggPlan,
    params: &BTreeMap<String, Value>,
    chunk: &DataChunk,
    finish: FinishFn,
) -> Result<Option<QueryResult>, RunError> {
    // ZERO live rows ⇒ zero groups, WITHOUT a reduction pass. Reducing an empty
    // chunk would evaluate each aggregate arg column over an empty distinct set,
    // which `eval_column` declines — a spurious `Ok(None)` that would drop the
    // global-aggregate zero row. The general path also special-cases empty here.
    let (groups, gkc): (Vec<Group>, GroupKeyCols) = if chunk.live() == 0 {
        (Vec::new(), GroupKeyCols::new())
    } else {
        match reduce_agg_groups(graph, plan, params, chunk)? {
            Some(g) => g,
            None => return Ok(None), // a column-budget / type decline
        }
    };
    finalize_agg_groups(graph, plan, params, groups, gkc, finish)
}

/// Project reduced groups through the aggregating tail — the post-reduce half of
/// [`run_aggregate_over_chunk`], shared with the batched pipeline path so both
/// finalise identically. Handles the global-aggregate zero row and the
/// Return/With forms.
fn finalize_agg_groups(
    graph: &Graph,
    plan: &AggPlan,
    params: &BTreeMap<String, Value>,
    mut groups: Vec<Group>,
    gkc: GroupKeyCols,
    finish: FinishFn,
) -> Result<Option<QueryResult>, RunError> {
    // The grouping vars to materialise at output (a const key / a global
    // aggregate needs none).
    let mut gvi: BTreeSet<usize> = BTreeSet::new();
    for gk in &plan.group_keys {
        match gk.kind {
            GroupKind::Node(vi) | GroupKind::Col(vi) => {
                gvi.insert(vi);
            }
            GroupKind::Const => {}
        }
    }
    let group_var_idx: Vec<usize> = gvi.into_iter().collect();

    // A GLOBAL aggregate (no grouping key) over ZERO rows still yields ONE row
    // (`count(*)` is 0, `sum` is 0, `avg`/`min`/`max` are null, `collect` is []),
    // exactly `StreamProjector::finish`'s no-grouping-key special case. With any
    // grouping key an empty input yields no rows, so guard on `group_keys` empty.
    if plan.group_keys.is_empty() && groups.is_empty() {
        groups.push((
            Vec::new(),
            plan.sites.iter().map(SiteAcc::for_site).collect(),
        ));
    }

    let labels = agg_var_labels(plan);
    let result = match &plan.form {
        AggForm::Return(proj) => run_agg_return(
            graph,
            proj,
            params,
            &plan.vars,
            &plan.var_kinds,
            &labels,
            &group_var_idx,
            &plan.sites,
            &plan.agg_items,
            &gkc,
            groups,
        )?,
        AggForm::With(wf) => run_agg_with(
            graph,
            &wf.with_proj,
            wf.post_where.as_ref(),
            &wf.return_proj,
            params,
            &plan.vars,
            &plan.var_kinds,
            &labels,
            &group_var_idx,
            &plan.sites,
            &plan.agg_items,
            &gkc,
            groups,
        )?,
    };
    finish(result)
}

/// A single group-key value from a primitive-typed column, used as an owned
/// `BTreeMap` key in the aggregate fast path INSTEAD of `agg_key_of`'s serialized
/// `Vec<u8>`. Eligible only for the types whose VALUE equality IS `agg_key_of`
/// equality: `Null`, `Bool`, `Int`, `Str`. `Float` is excluded (NaN must stay
/// distinct, which `agg_key_of` handles with a per-row nonce); every structured
/// or temporal value is excluded so the mapping never differs from the canonical
/// key. The `Ord` derive is only a total order for the map — grouping is by `Eq`,
/// and first-seen order is preserved by the `groups` Vec, so the result is
/// byte-identical to the general `agg_key_of` path.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum NativeKey {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
}

/// Whether every value in a grouping-key column is `NativeKey`-eligible.
fn native_key_eligible(v: &Value) -> bool {
    matches!(
        v,
        Value::Null | Value::Bool(_) | Value::Int(_) | Value::Str(_)
    )
}

impl NativeKey {
    /// Build the owned key for an already-checked eligible value.
    fn of(v: &Value) -> NativeKey {
        match v {
            Value::Null => NativeKey::Null,
            Value::Bool(b) => NativeKey::Bool(*b),
            Value::Int(i) => NativeKey::Int(*i),
            Value::Str(s) => NativeKey::Str(s.clone()),
            _ => unreachable!("native_key_eligible gates the column"),
        }
    }
}

/// Reduce the chunk's LIVE rows into first-seen groups, each carrying a
/// `Vec<SiteAcc>` folded in PRODUCTION order — the ONLY full pass, column reads +
/// map lookups + per-site `push`, no per-row `eval_with` or row clone. Pushing in
/// production order makes the fold byte-identical to `run_streaming`'s (sums,
/// averages, min/max ties and `collect` encounter order alike). `Ok(None)` = a
/// column-budget / type decline.
fn reduce_agg_groups(
    graph: &Graph,
    plan: &AggPlan,
    params: &BTreeMap<String, Value>,
    chunk: &DataChunk,
) -> Result<Option<(Vec<Group>, GroupKeyCols)>, RunError> {
    // Props each grouping-key COLUMN and each Col-arg site reads, per var. A Node
    // key/arg reads the id column only (a bare-var arg additionally needs the full
    // node, tracked separately); a const reads nothing.
    let mut props: Vec<BTreeSet<String>> = vec![BTreeSet::new(); plan.vars.len()];
    for gk in &plan.group_keys {
        if matches!(gk.kind, GroupKind::Col(_)) {
            let _ = classify_key(&gk.expr, &plan.vars, &mut props);
        }
    }
    // Vars needing a materialised full-node column (a bare-var aggregate arg).
    let mut node_arg_vars: BTreeSet<usize> = BTreeSet::new();
    // Node-kind vars whose ONLY bare-var use is a DISTINCT `count`. `agg_key` of a
    // node is its id ALONE (interp.rs `agg_key`), so a DISTINCT `count(node)`
    // dedups by id — an id-only LIGHT node is byte-identical to the materialised
    // one for this site, with zero store loads. (Rel-kind DISTINCT count and every
    // value-consuming aggregate — collect/min/max — still materialise below.)
    let mut count_node_vars: BTreeSet<usize> = BTreeSet::new();
    for (site, site_arg) in plan.sites.iter().zip(&plan.site_args) {
        match site_arg {
            SiteArgPlan::Col(_, e) => {
                let _ = classify_key(e, &plan.vars, &mut props);
            }
            // A non-DISTINCT `count` over a bare node var needs only presence
            // (`SiteArgVal::Present`), NOT a materialised node column — so it does
            // not force materialisation.
            SiteArgPlan::Node(_) if site.name == "count" && !site.distinct => {}
            // A DISTINCT `count` over a NODE var needs the node identity for its
            // per-group distinct set — but that key is the id alone, so an id-only
            // light node suffices (no `graph.node` load).
            SiteArgPlan::Node(vi)
                if site.name == "count" && matches!(chunk.var_kinds[*vi], VarKind::Node) =>
            {
                count_node_vars.insert(*vi);
            }
            // Any other node-var aggregate (collect/min/max, a DISTINCT count over
            // a REL var) still needs the full entity.
            SiteArgPlan::Node(vi) => {
                node_arg_vars.insert(*vi);
            }
            SiteArgPlan::Star | SiteArgPlan::Const(_) => {}
        }
    }

    // Per-var DISTINCT live ids (sorted) — needed where a column is read OR a bare
    // node arg materialises. A budget decline on any → the general path.
    let mut distinct: Vec<Vec<u64>> = vec![Vec::new(); plan.vars.len()];
    let mut need_distinct: BTreeSet<usize> = node_arg_vars.clone();
    // A DISTINCT `count(node)` also gathers per-row from its var's distinct index
    // (the id-only column is aligned to it), so it too needs `distinct[vi]` built —
    // but never a materialised node column.
    need_distinct.extend(count_node_vars.iter().copied());
    for (vi, p) in props.iter().enumerate() {
        if !p.is_empty() {
            need_distinct.insert(vi);
        }
    }
    for &vi in &need_distinct {
        let mut set: BTreeSet<u64> = BTreeSet::new();
        for &r in &chunk.selection {
            let id = chunk.ids[vi][r];
            // Exclude the OPTIONAL-MATCH null sentinel from the distinct set: a
            // null-fill row has no property to load, and `site_push_value` short-
            // circuits it to `Value::Null` before any `binary_search`. On the
            // non-optional path `NULL_ID` never appears, so this is a no-op there.
            if id != NULL_ID {
                set.insert(id);
            }
        }
        distinct[vi] = set.into_iter().collect();
    }

    // Loaded property columns per var — through the label's cached column
    // where the var's labels are known (fix 33).
    let labels = agg_var_labels(plan);
    let mut cols: Vec<BTreeMap<String, Vec<Value>>> = vec![BTreeMap::new(); plan.vars.len()];
    for (vi, p) in props.iter().enumerate() {
        if p.is_empty() {
            continue;
        }
        let Some(c) = load_var_columns_labelled(
            graph,
            chunk.var_kinds[vi],
            &distinct[vi],
            p,
            labels[vi].as_deref(),
            params,
        )?
        else {
            counted!("interp.pipeline reduce declined: a column over budget");
            return Ok(None);
        };
        cols[vi] = c;
    }

    // Materialised full-ENTITY columns per var (aligned to `distinct[vi]`), for a
    // bare-var aggregate argument (`collect(r)`, `min(r)`, a DISTINCT `count(r)`,
    // and the node equivalents). A Rel-kind var materialises through `rel_of`, a
    // Node-kind through `node_of` — the same value the per-tuple path folds.
    let mut node_cols: Vec<Vec<Value>> = vec![Vec::new(); plan.vars.len()];
    for &vi in &node_arg_vars {
        let mut nc: Vec<Value> = Vec::with_capacity(distinct[vi].len());
        for &id in &distinct[vi] {
            nc.push(match chunk.var_kinds[vi] {
                VarKind::Node => graph.node(id)?.ok_or(GraphError::Missing("node", id))?,
                VarKind::Rel => rel_of(graph, id)?,
            });
        }
        node_cols[vi] = nc;
    }

    let empty_vm = VarMap::new();
    let scope = Scope::over(params, &empty_vm, graph.wall_ms(), graph.zone_provider());

    // Precompute every grouping key as a gatherable column (or node id / const).
    let mut gkv: Vec<KeyVal> = Vec::with_capacity(plan.group_keys.len());
    for gk in &plan.group_keys {
        gkv.push(match gk.kind {
            GroupKind::Node(vi) => KeyVal::Node(vi),
            // A nullable var bound on NO live row (an OPTIONAL leg that matched
            // nothing): its distinct set is empty and every row keys as Null
            // without consulting the column, so there is nothing to evaluate.
            GroupKind::Col(vi) if distinct[vi].is_empty() => KeyVal::Col(vi, Vec::new()),
            GroupKind::Col(vi) => {
                let view = crate::vectorized::view(&cols[vi]);
                let Some(col) = eval_column(
                    &gk.expr,
                    &plan.vars[vi],
                    distinct[vi].len(),
                    &view,
                    &scope,
                ) else {
                    counted!("interp.pipeline reduce declined: a group key the column path cannot evaluate");
                    return Ok(None);
                };
                KeyVal::Col(vi, col.into_owned())
            }
            GroupKind::Const => {
                KeyVal::Const(eval_with(&gk.expr, &scope, None).map_err(RunError::Eval)?)
            }
        });
    }

    // Precompute every site's per-row argument value column.
    let mut arg_vals: Vec<SiteArgVal> = Vec::with_capacity(plan.site_args.len());
    for (site, site_arg) in plan.sites.iter().zip(&plan.site_args) {
        arg_vals.push(match site_arg {
            SiteArgPlan::Star => SiteArgVal::Star,
            // A non-DISTINCT count(node) needs only presence — no node column.
            SiteArgPlan::Node(vi) if site.name == "count" && !site.distinct => {
                SiteArgVal::Present(*vi)
            }
            // A DISTINCT count over a NODE var: gather id-only light nodes aligned
            // to `distinct[vi]`. `agg_key` reads the id alone, so this is
            // byte-identical to the materialised node — no `graph.node` load. (A
            // DISTINCT count over a REL var is NOT in `count_node_vars`, so it
            // falls through to the materialised `node_cols` arm below.)
            SiteArgPlan::Node(vi) if count_node_vars.contains(vi) => {
                let col: Vec<Value> = distinct[*vi]
                    .iter()
                    .map(|&id| Value::Node {
                        id,
                        labels: Vec::new(),
                        props: BTreeMap::new(),
                    })
                    .collect();
                SiteArgVal::Gather(*vi, col)
            }
            SiteArgPlan::Node(vi) => SiteArgVal::Gather(*vi, node_cols[*vi].clone()),
            // A nullable var bound on NO live row: every row's argument is the
            // null-fill's `Null` (`site_push_value` short-circuits the sentinel
            // before any lookup), so the empty column is never read. Until
            // this held an OPTIONAL leg that matched nothing declined the whole
            // statement to the general path on `eval_column`'s empty input.
            SiteArgPlan::Col(vi, _) if distinct[*vi].is_empty() => {
                SiteArgVal::Gather(*vi, Vec::new())
            }
            SiteArgPlan::Col(vi, e) => {
                let view = crate::vectorized::view(&cols[*vi]);
                let Some(col) =
                    eval_column(e, &plan.vars[*vi], distinct[*vi].len(), &view, &scope)
                else {
                    counted!("interp.pipeline reduce declined: an aggregate argument the column path cannot evaluate");
                    return Ok(None);
                };
                SiteArgVal::Gather(*vi, col.into_owned())
            }
            SiteArgPlan::Const(e) => {
                SiteArgVal::Const(eval_with(e, &scope, None).map_err(RunError::Eval)?)
            }
        });
    }

    // ── FIRST-SEEN group-by (the load-bearing order) ───────────────────────────
    // Iterate the chunk's live rows in PRODUCTION order (scan-order × nested
    // reverse-adjacency — already the selection order). Groups are kept in a Vec
    // in the order each distinct key is FIRST encountered; a new group starts with
    // fresh `SiteAcc`s (built from the plan's sites), and each row folds into its
    // group. Pushing in production order reproduces `run_streaming`'s fold exactly.
    let mut groups: Vec<Group> = Vec::new();

    // FAST PATH: a SINGLE node-identity grouping key (the common graph shape,
    // `WITH b, count(*)`). A node's canonical `agg_key` is `(tag 8, id)` —
    // injective in the id (labels/props ignored) — so keying on the raw `u64` id
    // is byte-identical to `agg_key_of`, with NO per-row `Value` build and NO
    // serialization. First-seen order is unchanged (the `groups` Vec is still
    // appended on first sight, in production row order).
    if let [KeyVal::Node(gvi)] = gkv.as_slice() {
        let gvi = *gvi;
        let mut index: BTreeMap<u64, usize> = BTreeMap::new();
        for &r in &chunk.selection {
            let id = chunk.ids[gvi][r];
            let gi = match index.get(&id) {
                Some(&g) => g,
                None => {
                    index.insert(id, groups.len());
                    groups.push((
                        chunk.row_ids(r),
                        plan.sites.iter().map(SiteAcc::for_site).collect(),
                    ));
                    budget_check(graph, groups.len())?;
                    groups.len() - 1
                }
            };
            if fold_row_weighted(&mut groups[gi].1, &arg_vals, &distinct, chunk, r)?
                .is_none()
            {
                return Ok(None);
            }
        }
        return Ok(Some((groups, GroupKeyCols::new())));
    }

    // GLOBAL AGGREGATE: no grouping key — one group over ALL live rows, in
    // first-seen (every row folds into group 0).
    if gkv.is_empty() {
        for &r in &chunk.selection {
            if groups.is_empty() {
                groups.push((
                    chunk.row_ids(r),
                    plan.sites.iter().map(SiteAcc::for_site).collect(),
                ));
            }
            if fold_row_weighted(&mut groups[0].1, &arg_vals, &distinct, chunk, r)?
                .is_none()
            {
                return Ok(None);
            }
        }
        return Ok(Some((groups, GroupKeyCols::new())));
    }

    // FAST PATH: a SINGLE value-COLUMN grouping key whose every value is a
    // primitive (`Null`/`Bool`/`Int`/`Str`). For these, value equality IS
    // `agg_key_of` equality, so keying on an owned `NativeKey` groups identically
    // to the general path WITHOUT the per-row `Vec<Value>` tuple, `Value` clone
    // for `Int`, and — the dominant cost over a full relationship scan — the
    // per-row `agg_key_of` `Vec<u8>` allocation + canonical serialization. Groups
    // are appended on first sight in the SAME production row order, so first-seen
    // order (and thus every downstream sort tie) is unchanged. `col` is the
    // distinct-indexed key column already evaluated above.
    if graph.agg_native_key_enabled() {
        if let [KeyVal::Col(vi, col)] = gkv.as_slice() {
            let vi = *vi;
            if col.iter().all(native_key_eligible) {
                counted!("interp.pipeline aggregate native-key group-by");
                let mut index: BTreeMap<NativeKey, usize> = BTreeMap::new();
                for &r in &chunk.selection {
                    // An OPTIONAL null-fill row (fix 30): its key is Null, as
                    // `null.prop` is on the per-tuple path; the sentinel is
                    // not in the distinct set.
                    let id = chunk.ids[vi][r];
                    let nk = if id == NULL_ID {
                        NativeKey::of(&Value::Null)
                    } else {
                        let pos = distinct[vi]
                            .binary_search(&id)
                            .expect("a live id is in its var's distinct set");
                        NativeKey::of(&col[pos])
                    };
                    let gi = match index.get(&nk) {
                        Some(&g) => g,
                        None => {
                            index.insert(nk, groups.len());
                            groups.push((
                                chunk.row_ids(r),
                                plan.sites.iter().map(SiteAcc::for_site).collect(),
                            ));
                            budget_check(graph, groups.len())?;
                            groups.len() - 1
                        }
                    };
                    if fold_row_weighted(&mut groups[gi].1, &arg_vals, &distinct, chunk, r)?
                .is_none()
            {
                return Ok(None);
            }
                }
                // Same group-key column hand-off the general path builds below, so
                // the projection reuses the loaded property column.
                let mut gkc: GroupKeyCols = BTreeMap::new();
                gkc.entry(vi)
                    .or_insert_with(|| (distinct[vi].clone(), cols[vi].clone()));
                return Ok(Some((groups, gkc)));
            }
        }
    }

    // GENERAL PATH: at least one value / const key, or multiple keys — the SAME
    // canonical serialization `run_streaming` uses (`agg_key_of`), one NaN nonce
    // threaded across every row.
    let mut index: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    let mut nonce = 0u64;
    for &r in &chunk.selection {
        let mut key: Vec<Value> = Vec::with_capacity(gkv.len());
        for k in &gkv {
            key.push(match k {
                // A bare-var identity key. `agg_key` serialises a Node as
                // (tag 8, id) and a Rel as (tag 9, id) — both injective in the id
                // alone — so building the KIND-correct placeholder (empty
                // labels/props) is byte-identical to `run_streaming`'s key over
                // the real value.
                // An OPTIONAL null-fill row (fix 30) keys as Null — the
                // per-tuple path's key for an unmatched optional var (its
                // node is null, `null.prop` is null); the sentinel is in no
                // distinct set.
                KeyVal::Node(vi) if chunk.ids[*vi][r] == NULL_ID => Value::Null,
                KeyVal::Node(vi) => match chunk.var_kinds[*vi] {
                    VarKind::Node => Value::Node {
                        id: chunk.ids[*vi][r],
                        labels: Vec::new(),
                        props: BTreeMap::new(),
                    },
                    VarKind::Rel => Value::Rel {
                        id: chunk.ids[*vi][r],
                        src: 0,
                        dst: 0,
                        rel_type: String::new(),
                        props: BTreeMap::new(),
                    },
                },
                KeyVal::Col(vi, _) if chunk.ids[*vi][r] == NULL_ID => Value::Null,
                KeyVal::Col(vi, col) => {
                    let pos = distinct[*vi]
                        .binary_search(&chunk.ids[*vi][r])
                        .expect("a live id is in its var's distinct set");
                    col[pos].clone()
                }
                KeyVal::Const(v) => v.clone(),
            });
        }
        let ser = agg_key_of(&key, &mut nonce);
        let gi = match index.get(&ser) {
            Some(&g) => g,
            None => {
                index.insert(ser, groups.len());
                groups.push((
                    chunk.row_ids(r),
                    plan.sites.iter().map(SiteAcc::for_site).collect(),
                ));
                budget_check(graph, groups.len())?;
                groups.len() - 1
            }
        };
        if fold_row_weighted(&mut groups[gi].1, &arg_vals, &distinct, chunk, r)?
            .is_none()
        {
            return Ok(None);
        }
    }
    // The group-key columns loaded above, so the projection reuses them.
    let mut gkc: GroupKeyCols = BTreeMap::new();
    for gk in &plan.group_keys {
        if let GroupKind::Col(vi) = gk.kind {
            gkc.entry(vi)
                .or_insert_with(|| (distinct[vi].clone(), cols[vi].clone()));
        }
    }
    Ok(Some((groups, gkc)))
}

/// Record that the pipeline produced the answer (distinguishes a real firing
/// from a silent decline that makes a differential vacuous) and return it.
fn finish(result: QueryResult) -> Result<Option<QueryResult>, RunError> {
    counted!("interp.pipeline hop runs");
    Ok(Some(result))
}

/// Record that the group-by-count operator produced the answer — a distinct
/// counter from `finish`, so a test can assert the AGGREGATE path fired (and
/// that census shapes still decline to batch.rs).
fn finish_aggregate(result: QueryResult) -> Result<Option<QueryResult>, RunError> {
    counted!("interp.pipeline aggregate runs");
    Ok(Some(result))
}

/// Record that the MULTI-STAGE `MATCH … WITH … MATCH … RETURN` pipeline produced
/// the answer — a distinct counter from `finish`/`finish_aggregate`/
/// `finish_optional`, so a test can assert the multi-stage path fired on an
/// accepted shape and did NOT on a declined one. The stage-2 tail is run through
/// this finisher (not the core/aggregate/distinct one it reuses), so exactly one
/// multistage-runs count is recorded per accepted query.
fn finish_multistage(result: QueryResult) -> Result<Option<QueryResult>, RunError> {
    counted!("interp.pipeline multistage runs");
    Ok(Some(result))
}

/// Record that the SET-BASED HASH-JOIN pipeline (`MATCH <chainA> MATCH <chainB>
/// … <group-by aggregate>`) produced the answer — a distinct counter from the
/// others, so a test can assert the join operator FIRED on an accepted shape and
/// did NOT on a declined one.
fn finish_join(result: QueryResult) -> Result<Option<QueryResult>, RunError> {
    counted!("interp.pipeline join runs");
    Ok(Some(result))
}

/// Record that the MULTI-STAGE JOIN pipeline (`MATCH <chain1> WITH [DISTINCT]
/// <var> MATCH <chainA> MATCH <chainB> RETURN <group-by aggregate>`) produced the
/// answer — a distinct counter from `finish_multistage`/`finish_join`, so a test
/// can assert the COMPOSITE (stage-1 → carried seed → two-MATCH hash join → agg)
/// FIRED on an accepted shape (the full LDBC IC5 statement) and did NOT on a
/// declined one (which routes to the single-stage multistage, the join, or the
/// nested general path instead).
fn finish_multistage_join(result: QueryResult) -> Result<Option<QueryResult>, RunError> {
    counted!("interp.pipeline multistage-join runs");
    Ok(Some(result))
}

/// How a post-chunk tail wraps its result and stamps the "which operator fired"
/// counter — `finish`/`finish_aggregate`/`finish_optional`/`finish_multistage`.
/// Threaded into the shared over-chunk tails so the SAME projection/reduction
/// code serves the single-MATCH core, the OPTIONAL left join and the multi-stage
/// WITH path while each records its own operator counter.
type FinishFn = fn(QueryResult) -> Result<Option<QueryResult>, RunError>;

// ─── OPTIONAL MATCH — the left join with null-fill (Phase 4b2) ────────────────

/// A recognised `MATCH <outer> [WHERE] OPTIONAL MATCH <opt> [WHERE] {WITH|RETURN}
/// …` LEFT JOIN. The outer read chain builds a chunk; then, per LIVE outer row,
/// the optional pattern runs as a left join — its matches are kept, or, if it
/// produces NONE, ONE null-fill row where every optional-introduced var binds to
/// `NULL_ID` (materialising to `Value::Null`). The widened chunk then feeds the
/// SAME aggregating / non-aggregating tail the single-MATCH path uses, so
/// `count(optvar)` over a group with only its null row is 0, `collect(optvar)`
/// skips it, and `RETURN optvar.prop` is null for an unmatched outer row.
struct OptionalPlan {
    /// The outer read chain (scanned + expanded to build the outer chunk).
    outer_a_labels: Vec<String>,
    outer_a_var: String,
    outer_hops: Vec<Hop>,
    /// The outer bound vars, in binding order — the FIRST `outer_vars.len()`
    /// columns of the combined chunk, the only columns that never hold `NULL_ID`.
    outer_vars: Vec<String>,
    outer_where: Option<WherePred>,
    /// One left join per OPTIONAL clause, in clause order. Round k's combined
    /// chunk is round k+1's outer chunk, so each clause null-fills INDEPENDENTLY
    /// of the others — a first-clause match survives a second-clause miss with
    /// only the second clause's vars null — which is exactly the per-clause
    /// null row the interpreter's `drive` emits.
    stages: Vec<OptionalStage>,
    /// Outer vars ++ every optional-introduced var, in binding order (stage k's
    /// vars occupy `combined_vars[..stages[k].vars_after]`). The columns at
    /// index `>= outer_vars.len()` are the nullable, optional-introduced vars —
    /// the runner fills them with `NULL_ID` on a no-match row; the recognizer
    /// already validated that the tail consumes them only where null handling
    /// covers it.
    combined_vars: Vec<String>,
    /// Per-var KIND, parallel to `combined_vars` — set on the merged chunk so its
    /// tail materialises node vs relationship vars correctly.
    combined_var_kinds: Vec<VarKind>,
    tail: OptionalTail,
}

/// One OPTIONAL clause's left join over the vars bound before it.
struct OptionalStage {
    /// The clause's ordered expand/semijoin steps. Source/target indices are
    /// into `combined_vars`; every source or target below `outer_len` is an
    /// OUTER (never-null) var — the recognizer declines a re-root at, a close
    /// onto, or a WHERE over an EARLIER clause's nullable var, because the
    /// expand/semijoin/filter operators never model the `NULL_ID` sentinel.
    hops: Vec<Hop>,
    /// The clause's own WHERE, applied WITHIN its left join (a match failing it
    /// counts as no-match ⇒ that outer row null-fills), over this stage's vars.
    where_: Option<WherePred>,
    /// The var boundary this round joins across: `combined_vars[..outer_len]`
    /// is bound before the clause (the round's outer half), and
    /// `combined_vars[outer_len..vars_after]` is what it introduces.
    outer_len: usize,
    vars_after: usize,
}

/// The trailing projection over the combined (outer + optional) vars — the same
/// two tails the single-MATCH pipeline recognises, reused verbatim.
enum OptionalTail {
    Agg(Box<AggPlan>),
    Core(Box<CorePlan>),
}

/// Recognise the OPTIONAL-MATCH left join. Clause shape: a non-optional MATCH,
/// then ONE OR MORE optional MATCHes, then a RETURN (Form B) or a WITH→RETURN
/// (Form A). DECLINES (byte-identical fallback): any other trailing clause
/// shape, an outer/optional pattern the hop recognizer declines, a var-length
/// hop in any clause, an optional WHERE the filter cannot vectorise (a
/// whole-node `x IN <list>` among them — deferred to Phase 4b3), a LATER
/// optional clause that re-roots at / closes onto / filters over an EARLIER
/// optional clause's nullable var (the operators never expand from or compare
/// against the `NULL_ID` sentinel), and a nullable var used in a way the null
/// handling does not cover (a GROUPING key, an ORDER BY key, a
/// non-`count`/`collect` aggregate, or — impossible to bind, hence a natural
/// decline — the outer WHERE).
fn recognise_optional(sq: &SingleQuery) -> Option<OptionalPlan> {
    // [Match(non-opt), Match(opt)+, <tail>]. The optional run is consumed
    // greedily; whatever follows must be the RETURN / WITH→RETURN tail below.
    let [
        Clause::Match {
            optional: false,
            pattern: outer_pattern,
            where_: outer_where_opt,
        },
        after_outer @ ..,
    ] = sq.clauses.as_slice()
    else {
        return None;
    };
    let n_opt = after_outer
        .iter()
        .take_while(|c| matches!(c, Clause::Match { optional: true, .. }))
        .count();
    if n_opt == 0 {
        return None;
    }
    let (opt_clauses, rest) = after_outer.split_at(n_opt);

    // Outer read chain + its single-var WHERE (over outer vars only — a nullable
    // var can never be bound here, so referencing one declines via `classify_key`).
    let outer_hc = collect_hops(outer_pattern, None, true, false, false)?;
    let outer_where = recognise_single_var_where(outer_where_opt.as_ref(), &outer_hc.vars)?;
    // A var-length hop in an OPTIONAL left join is out of scope — decline the
    // whole query to the general path (its runner forces per-hop `reset` for
    // fixed hops, which the BFS operator does not model).
    if hops_have_varlen(&outer_hc.hops) {
        return None;
    }
    let outer_vars = outer_hc.vars.clone();

    // Each optional clause's pattern re-roots every path at an already-bound var
    // (outer, or an earlier path of the SAME clause); its non-final hops
    // introduce NEW nullable vars, a final hop may close onto a bound var
    // (semijoin). The bound context grows clause by clause, so a later clause
    // sees every earlier clause's vars — and must not touch them (see below).
    let mut stages: Vec<OptionalStage> = Vec::with_capacity(n_opt);
    let mut all_hops: Vec<Hop> = outer_hc.hops.clone();
    let mut bound_vars = outer_hc.vars.clone();
    let mut bound_kinds = outer_hc.var_kinds.clone();
    let mut bound_labels = outer_hc.var_labels.clone();
    for clause in opt_clauses {
        let Clause::Match {
            pattern: opt_pattern,
            where_: opt_where_opt,
            ..
        } = clause
        else {
            return None; // unreachable: the run above admits only optional MATCHes
        };
        let outer_len = bound_vars.len();
        let prebound = (
            bound_vars.as_slice(),
            bound_kinds.as_slice(),
            &bound_labels,
        );
        let opt_hc = collect_hops(opt_pattern, Some(prebound), false, false, false)?;
        if hops_have_varlen(&opt_hc.hops) {
            return None;
        }
        // An EARLIER clause's var (index in `outer_vars.len()..outer_len`) may hold
        // `NULL_ID` in this round's outer chunk. `expand`/`semijoin` read a source
        // id straight into the adjacency accessor and a semijoin target into an
        // id compare, and `filter` loads a property column over the ids — none of
        // them models the sentinel, so a hop or WHERE over such a var declines.
        let earlier_nullable = |vi: usize| vi >= outer_vars.len() && vi < outer_len;
        if opt_hc
            .hops
            .iter()
            .any(|h| earlier_nullable(h.src) || h.tgt.is_some_and(earlier_nullable))
        {
            return None;
        }
        // The clause's WHERE, applied WITHIN its left join over the vars bound so
        // far — it may read one of THIS clause's nullable vars (that column holds
        // a real id on a match; the filter never runs on a null-fill row, which
        // is emitted only when the filtered match set is empty), never an
        // earlier clause's.
        let opt_where = recognise_single_var_where(opt_where_opt.as_ref(), &opt_hc.vars)?;
        if let Some(pred) = &opt_where {
            let names_earlier = |name: &str| {
                opt_hc
                    .vars
                    .iter()
                    .position(|v| v == name)
                    .is_some_and(earlier_nullable)
            };
            if names_earlier(&pred.var)
                || pred
                    .id_other
                    .as_ref()
                    .is_some_and(|(other, _)| names_earlier(other))
                // An edge predicate probes BOTH endpoints' ids; a nullable one
                // would hand `NULL_ID` to the adjacency read, which the probe's
                // general-path twin never sees (a null endpoint goes to the
                // general matcher there), so it declines like `id_other`.
                || pred
                    .edge
                    .as_ref()
                    .is_some_and(|e| names_earlier(&e.src) || names_earlier(&e.dst))
            {
                return None;
            }
        }
        all_hops.extend(opt_hc.hops.iter().cloned());
        stages.push(OptionalStage {
            hops: opt_hc.hops,
            where_: opt_where,
            outer_len,
            vars_after: opt_hc.vars.len(),
        });
        bound_vars = opt_hc.vars;
        bound_kinds = opt_hc.var_kinds;
        bound_labels = opt_hc.var_labels;
    }

    let combined_vars = bound_vars;
    let combined_var_kinds = bound_kinds;
    // Every optional-introduced var, across ALL clauses, is nullable in the
    // combined chunk the tail reads.
    let nullable: BTreeSet<usize> = (outer_vars.len()..combined_vars.len()).collect();
    let nullable_names: BTreeSet<String> =
        nullable.iter().map(|&i| combined_vars[i].clone()).collect();

    // The combined chain feeds the tail recognizers (they read only `vars`); its
    // hops/where are inert for the OPTIONAL runner, which executes the outer and
    // each optional clause separately, so `where_` is left `None`.
    let combined_chain = Chain {
        a_labels: outer_hc.a_labels.clone(),
        a_var: outer_hc.a_var.clone(),
        hops: all_hops,
        vars: combined_vars.clone(),
        var_kinds: combined_var_kinds.clone(),
        wheres: Vec::new(),
        // The multistage path carries its own seed anchor (`s1_anchor`); the
        // synthetic combined chain never drives the seed.
        start_anchor: None,
    };

    // The trailing projection: Form B RETURN (aggregate or plain) or Form A
    // WITH→RETURN (aggregate). Then VALIDATE that every nullable var is consumed
    // only where the null handling covers it.
    let tail = match rest {
        [Clause::Return { proj }] => {
            if proj.items.iter().any(|it| expr_has_aggregate(&it.expr)) {
                let agg = aggregate_over_chain(combined_chain, proj, None)?;
                if !nullable_agg_ok(&agg, &nullable, &nullable_names) {
                    return None;
                }
                OptionalTail::Agg(Box::new(agg))
            } else {
                let core = core_over_chain(combined_chain, proj)?;
                if !nullable_core_ok(&core, &nullable) {
                    return None;
                }
                OptionalTail::Core(Box::new(core))
            }
        }
        [
            Clause::With {
                proj: wp,
                where_: post,
            },
            Clause::Return { proj: rp },
        ] => {
            let agg = aggregate_over_chain(combined_chain, wp, Some((post.as_ref(), rp)))?;
            if !nullable_agg_ok(&agg, &nullable, &nullable_names) {
                return None;
            }
            OptionalTail::Agg(Box::new(agg))
        }
        _ => return None, // any other trailing shape (a further reading clause, …)
    };

    // OPERATOR D: mark the legs the fold can COUNT rather than produce. Marked
    // here, on the hop lists `run_optional` actually executes, and after the
    // tail is known — the tail is what decides whether a leg var is read.
    plan_optional_fold(&mut stages, &tail, &combined_var_kinds);

    Some(OptionalPlan {
        outer_a_labels: outer_hc.a_labels,
        outer_a_var: outer_hc.a_var,
        outer_hops: outer_hc.hops,
        outer_vars,
        outer_where,
        stages,
        combined_vars,
        combined_var_kinds,
        tail,
    })
}

/// OPTIONAL-FOLD ELIGIBILITY (operator D): mark every hop of each OPTIONAL
/// clause whose whole leg the fold can COUNT instead of producing. Per leg, so
/// one clause folding does not depend on another.
///
/// A leg folds when:
///   - every aggregate site is a non-DISTINCT `count(*)` — the null-fill row
///     counts as one row under `count(*)`, which is what `max(1, ·)` encodes;
///     any other site (`count(liker)` counts it as ZERO) keeps the ordinary
///     left join;
///   - every var the leg introduces is a NODE the statement never reads
///     ([`agg_tail_read_set`] — the same test the single-MATCH fold applies),
///     since the fold has only a `NULL_ID` placeholder to offer for it;
///   - the clause has no WHERE — the fold evaluates no filter inside a leg, and
///     a leg WHERE reads one of its vars anyway;
///   - every hop is one the fold's recursion can answer
///     ([`leg_hops_foldable`]).
///
/// The lever is read HERE, at plan time, exactly as `plan_count_fold` reads it,
/// so flipping it changes the very next statement.
fn plan_optional_fold(
    stages: &mut [OptionalStage],
    tail: &OptionalTail,
    var_kinds: &[VarKind],
) {
    if !count_fold_enabled() {
        return;
    }
    let OptionalTail::Agg(agg) = tail else {
        return;
    };
    if !all_sites_count_star(&agg.sites) {
        return;
    }
    let read = agg_tail_read_set(agg);
    for stage in stages.iter_mut() {
        if stage.where_.is_some() {
            continue;
        }
        let leg_read_or_rel = (stage.outer_len..stage.vars_after)
            .any(|vi| read[vi] || !matches!(var_kinds[vi], VarKind::Node));
        if leg_read_or_rel || !leg_hops_foldable(&stage.hops, stage.outer_len) {
            continue;
        }
        for h in stage.hops.iter_mut() {
            h.fold = true;
        }
        // The runner already forces `reset` on a clause's FIRST hop (each clause
        // is its own pattern); recording it on the hop makes the fold's own
        // isomorphism base — read from `Hop.reset` inside `hop_sum` — the same
        // one, and leaves the materialised path byte-identical.
        if let Some(h0) = stage.hops.first_mut() {
            h0.reset = true;
        }
    }
}

/// Whether ONE OPTIONAL clause's hops are all foldable, given that every var it
/// introduces (index >= `outer_len`) is unread. Declines a var-length hop and a
/// hop binding a RELATIONSHIP variable (the fold appends no real column for
/// either), and holds a CLOSE to a target that is in `bind` when its level runs:
///
///   - an OUTER var (index < `outer_len`) is a materialised column of the
///     driving row, copied into `bind` before any root runs — the leg's form of
///     the position rule, and always satisfied because every leg var is bound
///     after every outer one;
///   - a LEG var must be the closing level's OWN var or a folded ANCESTOR of it,
///     which the recursion has bound on its way down. A sibling branch's var is
///     still `NULL_ID` there, so it declines.
fn leg_hops_foldable(hops: &[Hop], outer_len: usize) -> bool {
    // Which hop binds each leg var (the outer vars are bound by the outer chain).
    let mut binder: BTreeMap<usize, usize> = BTreeMap::new();
    for (hi, h) in hops.iter().enumerate() {
        if h.varlen.is_some() || h.rel_var.is_some() {
            return false;
        }
        match h.tgt {
            None => {
                let Some(e) = h.end_vi else {
                    return false;
                };
                if e < outer_len {
                    return false; // an expand may not re-bind an outer column
                }
                binder.insert(e, hi);
            }
            Some(t) if t >= outer_len => {
                // Walk the level's ancestor chain up to the outer boundary.
                let mut cur = h.src;
                let mut ok = cur == t;
                while !ok && cur >= outer_len {
                    let Some(&b) = binder.get(&cur) else {
                        return false;
                    };
                    cur = hops[b].src;
                    ok = cur == t;
                }
                if !ok {
                    return false;
                }
            }
            Some(_) => {}
        }
    }
    true
}

/// The order keys carried by an aggregating tail — Form B's own ORDER BY, or
/// Form A's RETURN ORDER BY (the WITH breaker carries none).
fn agg_order_keys(plan: &AggPlan) -> &[engram_cypher::stmt::OrderItem] {
    match &plan.form {
        AggForm::Return(proj) => &proj.order,
        AggForm::With(wf) => &wf.return_proj.order,
    }
}

/// Whether an aggregating tail uses every nullable var only where the null
/// handling covers it: NOT as a grouping key, NOT in an aggregate other than
/// `count`/`collect`, NOT through any argument expression other than the bare
/// var or a direct `var.prop`, and NOT in an ORDER BY key OUTSIDE an aggregate.
/// (A nullable var INSIDE `count(post)` / `collect(post.len)` is fine — the site
/// folds `Value::Null` for a null-fill row, which `SiteAcc::push` skips.)
fn nullable_agg_ok(
    plan: &AggPlan,
    nullable: &BTreeSet<usize>,
    nullable_names: &BTreeSet<String>,
) -> bool {
    // A nullable var as a grouping key (fix 30): every null-fill row keys as
    // Null and groups together, exactly as the per-tuple path groups an
    // unmatched optional var (`node_of` gives Null, `null.prop` is null) —
    // `reduce_agg_groups` keys the sentinel as Null and keeps it out of the
    // distinct sets, and the projection's gathers already map it. Admitted
    // for the bare var and a direct `var.prop`; any other expression over a
    // nullable var could map the null-fill row to a non-null the per-tuple
    // path would evaluate, so it still declines. Until this held `WITH p,
    // collect(DISTINCT a.id) AS ids, r.id AS rid` (the Proposal listing)
    // ran on the general path: 292 projected reads per statement.
    for gk in &plan.group_keys {
        match gk.kind {
            GroupKind::Node(vi) if nullable.contains(&vi) => {
                counted!("interp.pipeline optional admitted a nullable group key");
            }
            GroupKind::Col(vi) if nullable.contains(&vi) => {
                if !is_direct_prop_of(&gk.expr, &plan.vars[vi]) {
                    return false;
                }
                counted!("interp.pipeline optional admitted a nullable group key");
            }
            _ => {}
        }
    }
    // A nullable aggregate ARGUMENT is only null-correct for count/collect (both
    // skip nulls with the RIGHT meaning: count excludes the unmatched row, collect
    // omits it). sum/avg/min/max over a nullable arg is declined.
    //
    // The argument's SHAPE is constrained too. `site_push_value` short-circuits a
    // null-fill (`NULL_ID`) row to `Value::Null` WITHOUT evaluating the site's
    // expression, so only an expression that itself maps a null var to null is
    // byte-identical to the per-tuple path: the bare var, or a direct `var.prop`
    // (`null.prop` is null in `eval`). Any other expression over a nullable var
    // — `IS NULL` / `IS NOT NULL`, `coalesce`, `CASE`, `x AND false`, `x OR true`,
    // `IN`, a scalar fn — can map the null-fill row to a NON-null the per-tuple
    // path counts/collects and the column path would skip, so it DECLINES the
    // whole recogniser to the general path.
    for (site, arg) in plan.sites.iter().zip(&plan.site_args) {
        let reads_nullable = match arg {
            SiteArgPlan::Node(vi) => nullable.contains(vi),
            SiteArgPlan::Col(vi, e) => {
                if !nullable.contains(vi) {
                    false
                } else if is_direct_prop_of(e, &plan.vars[*vi]) {
                    true
                } else {
                    return false; // a null-mapping expression over a nullable var
                }
            }
            SiteArgPlan::Star | SiteArgPlan::Const(_) => false,
        };
        if reads_nullable && !(site.name == "count" || site.name == "collect") {
            return false;
        }
    }
    // An ORDER BY key may reference a nullable var ONLY inside an aggregate
    // (`ORDER BY count(post)` is fine); a nullable var read OUTSIDE an aggregate
    // (`ORDER BY post.length`) is declined. Stripping aggregates then checking the
    // remaining free vars separates the two exactly.
    for o in agg_order_keys(plan) {
        let mut sites: Vec<AggSite> = Vec::new();
        let stripped = extract_aggregates(&o.expr, &mut sites);
        let mut fv = Vec::new();
        free_vars_of(&stripped, &mut fv);
        if fv.iter().any(|v| nullable_names.contains(v)) {
            return false;
        }
    }
    true
}

/// Whether `e` is exactly `var.<prop>` — a ONE-level property read off the bare
/// variable `var`, the one aggregate-argument shape (besides the bare var) that
/// maps a null-fill row to null the same way the per-tuple path does. A nested
/// `var.a.b`, a property read off any other expression, or anything wrapping the
/// read is NOT this shape.
fn is_direct_prop_of(e: &Expr, var: &str) -> bool {
    matches!(e, Expr::Prop(of, _) if matches!(of.as_ref(), Expr::Var(v) if v == var))
}

/// Whether a non-aggregating (core) tail uses every nullable var only where the
/// null handling covers it: a nullable var may be PROJECTED (`RETURN post.prop`
/// → null via `node_of`), but NOT read by an ORDER BY key (the top-k would load a
/// property column over the null sentinel). The key is classified AFTER alias
/// resolution — the same `resolve_order_key_alias` the recogniser
/// (`core_over_chain`) and the top-k (`TopKAcc::push_chunk`) apply — so `post.len
/// AS plen ORDER BY plen` is seen as the `post.len` read it is and declines
/// exactly like the direct `ORDER BY post.len`. Classifying the RAW `Var(plen)`
/// instead saw no bound var and let the alias form through.
fn nullable_core_ok(plan: &CorePlan, nullable: &BTreeSet<usize>) -> bool {
    for o in &plan.proj.order {
        let key = resolve_order_key_alias(&o.expr, &plan.vars, &plan.proj);
        let mut dummy: Vec<BTreeSet<String>> = vec![BTreeSet::new(); plan.vars.len()];
        if let Some(KeyRef::Var(vi)) = classify_key(&key, &plan.vars, &mut dummy) {
            if nullable.contains(&vi) {
                return false;
            }
        }
    }
    true
}

/// The vectorized LEFT-JOIN null-extension: given an already-built OUTER `chunk`
/// (its first `outer_len` vars are the kept columns, copied through unchanged),
/// run every `opt_hops` step over the whole chunk in ONE pass carrying outer-row
/// provenance, apply `opt_where` once, then MERGE — per outer row in input order,
/// its surviving matches in production order, else ONE null-fill row (outer ids
/// kept, every optional var `NULL_ID`). Returns the combined chunk (vars
/// `combined_vars`), or `Ok(None)` on a filter budget / non-boolean decline.
/// Byte-identical to `exec_match`'s optional interleaving. One call per OPTIONAL
/// clause: a later clause's outer chunk is an earlier clause's combined chunk,
/// so its outer columns at index `>= non_nullable_len` may already hold
/// `NULL_ID` (an earlier null-fill) — only the columns below `non_nullable_len`
/// (the non-OPTIONAL outer's) are asserted real. The first optional hop's
/// isomorphism base is forced empty regardless of what the outer walk recorded
/// (each clause is its own pattern), so every round starts fresh.
#[allow(clippy::too_many_arguments)]
fn left_join_null_extend(
    graph: &Graph,
    outer_chunk: DataChunk,
    outer_len: usize,
    non_nullable_len: usize,
    opt_hops: &[Hop],
    opt_where: Option<&WherePred>,
    combined_vars: &[String],
    combined_var_kinds: &[VarKind],
    params: &BTreeMap<String, Value>,
) -> Result<Option<DataChunk>, RunError> {
    debug_assert!(
        non_nullable_len <= outer_len,
        "the never-null prefix lies within the outer columns"
    );
    // Optional-hop end-label members + type tokens (computed ONCE, reused per
    // outer row). A hop whose named type was never minted has empty tokens; its
    // `adjacent_slim` yields nothing, so that outer row null-fills — matching the
    // general path, which finds no optional match.
    let mut opt_members: Vec<Option<crate::MembersView>> = Vec::with_capacity(opt_hops.len());
    let mut opt_tokens: Vec<Option<Vec<u32>>> = Vec::with_capacity(opt_hops.len());
    for hop in opt_hops {
        let members = if hop.labels.is_empty() {
            None
        } else {
            Some(graph.members_all(&hop.labels).map_err(RunError::Graph)?)
        };
        opt_members.push(members);
        opt_tokens.push(graph.type_tokens_peek(&hop.types));
    }

    // THE OPTIONAL FOLD (operator D). `plan_optional_fold` marks a leg's hops
    // ALL-or-nothing, so one flag decides: count the leg's matches per outer row
    // and null-fill every column it would have bound, instead of expanding it and
    // merging. No merge means no row ever moves, so the outer order is trivially
    // the merge's. With the fold lever off nothing is marked and the ordinary
    // left join below runs — the honest differential twin.
    // The `opt_where` term is belt-and-braces: `plan_optional_fold` refuses a
    // leg that carries one (the fold evaluates no filter inside a leg), and
    // dropping a WHERE would silently overcount, so the runner checks too.
    if !opt_hops.is_empty() && opt_hops.iter().all(|h| h.fold) && opt_where.is_none() {
        let fp = FoldPlan::new(graph, opt_hops, &opt_members, &opt_tokens, combined_vars.len(), None);
        return Ok(Some(fold_optional_leg(
            outer_chunk,
            outer_len,
            &fp,
            combined_vars,
            combined_var_kinds,
        )?));
    }

    // VECTORIZED LEFT JOIN — ONE pass over the WHOLE outer chunk, not a fresh
    // sub-chunk per outer row. Attach OUTER-ROW PROVENANCE (each working row's
    // index into the outer chunk's live rows, in order), run every optional step
    // over the whole chunk, then apply the optional WHERE once. `expand`/
    // `semijoin` copy provenance forward in lockstep with the id columns, so a
    // surviving row still names the outer row it descends from.
    let ncols = combined_vars.len();
    // Outer ids compacted to the LIVE rows, indexed by provenance i (0-based over
    // `outer_chunk.selection`, in order) — the source of every null-fill row's
    // outer half, kept after the outer chunk is consumed by the expansion below.
    let outer_live_count = outer_chunk.selection.len();
    let outer_live_ids: Vec<Vec<u64>> = (0..outer_len)
        .map(|vi| {
            outer_chunk
                .selection
                .iter()
                .map(|&r| outer_chunk.ids[vi][r])
                .collect()
        })
        .collect();
    // The outer rows' fold weights (a null-fill row keeps its outer row's
    // weight), compacted like the ids; empty when the outer chunk is unweighted.
    let outer_live_w: Vec<u64> = if outer_chunk.weights.is_empty() {
        Vec::new()
    } else {
        outer_chunk
            .selection
            .iter()
            .map(|&r| outer_chunk.weights[r])
            .collect()
    };

    // Seed provenance on the outer chunk: the i-th live row descends from outer
    // row i. Rows outside the selection are never read (don't-care provenance).
    let mut work = outer_chunk;
    let mut prov = vec![0usize; work.row_count()];
    for (i, &r) in work.selection.iter().enumerate() {
        prov[r] = i;
    }
    work.prov = prov;

    // Run every optional step over the whole chunk in ONE pass each.
    for (i, hop) in opt_hops.iter().enumerate() {
        // Force `reset` on the VERY FIRST optional hop so its isomorphism base is
        // empty regardless of what the OUTER walk recorded — the vectorized
        // equivalent of the old fresh single-row seed. Every later optional hop
        // keeps its own `reset` (a later optional PATH's first hop re-seeds).
        let reset = i == 0 || hop.reset;
        work = match hop.tgt {
            None => {
                let members_slice: Option<&crate::MembersView> = opt_members[i].as_ref();
                work.expand(
                    graph,
                    hop.src,
                    &hop.var,
                    hop.rel_var.as_deref(),
                    hop.dir,
                    &opt_tokens[i],
                    members_slice,
                    hop.track,
                    reset,
                )?
            }
            Some(tgt_vi) => work.semijoin(
                graph,
                hop.src,
                tgt_vi,
                hop.rel_var.as_deref(),
                hop.dir,
                &opt_tokens[i],
                hop.track,
                reset,
            )?,
        };
    }
    // The optional WHERE, applied ONCE over the whole chunk BEFORE the null-fill
    // decision — a match failing it is not a match, so its outer row null-fills if
    // that leaves it with none.
    if let Some(pred) = opt_where {
        if work.filter(graph, params, pred)?.is_none() {
            return Ok(None); // a budget / non-boolean decline
        }
    }

    // MERGE — interleave null-fills to reproduce `exec_match`'s order EXACTLY: per
    // outer row in input order, its surviving rows in production order, else ONE
    // null-fill row. The surviving rows are already grouped by provenance in
    // ASCENDING order (expand/semijoin walk rows in order and append contiguously;
    // filter keeps the selection ascending), so a single pointer walks them in
    // lockstep with the outer rows.
    let mut out_ids: Vec<Vec<u64>> = vec![Vec::new(); ncols];
    let carry_w = !outer_live_w.is_empty();
    let mut out_w: Vec<u64> = Vec::new();
    let survivors = &work.selection;
    let mut p = 0usize;
    // `i` is the OUTER-ROW PROVENANCE value compared against `work.prov`, not a
    // mere index into one collection — a range loop is the clearest form here.
    #[allow(clippy::needless_range_loop)]
    for i in 0..outer_live_count {
        let start = p;
        while p < survivors.len() && work.prov[survivors[p]] == i {
            p += 1;
        }
        if p == start {
            // ZERO surviving rows for outer row i → ONE null-fill row = the outer
            // ids, every optional var `NULL_ID`.
            for (vi, col) in out_ids.iter_mut().enumerate() {
                if vi < outer_len {
                    let id = outer_live_ids[vi][i];
                    // An outer column of an EARLIER optional clause may carry its
                    // own null-fill; only the non-OPTIONAL outer's columns are
                    // guaranteed real ids.
                    debug_assert!(
                        vi >= non_nullable_len || id < NULL_ID,
                        "a real node id must stay below the NULL sentinel"
                    );
                    col.push(id);
                } else {
                    col.push(NULL_ID);
                }
            }
            if carry_w {
                out_w.push(outer_live_w[i]);
            }
        } else {
            // The matches for outer row i — a contiguous block in production order.
            for &sr in &survivors[start..p] {
                for (vi, col) in out_ids.iter_mut().enumerate() {
                    col.push(work.ids[vi][sr]);
                }
                if carry_w {
                    out_w.push(work.weights[sr]);
                }
            }
        }
    }
    debug_assert_eq!(
        p,
        survivors.len(),
        "every surviving row belongs to exactly one outer row"
    );
    let n = out_ids.first().map_or(0, Vec::len);
    Ok(Some(DataChunk {
        vars: combined_vars.to_vec(),
        var_kinds: combined_var_kinds.to_vec(),
        ids: out_ids,
        selection: (0..n).collect(),
        used_rels: vec![Vec::new(); n],
        prov: Vec::new(),
        weights: out_w,
    }))
}

/// Run the OPTIONAL-MATCH left join(s): build the outer chunk, then for EACH
/// optional clause in order run its pattern over the WHOLE current chunk in one
/// vectorized pass (outer-row provenance threaded through `expand`/`semijoin`)
/// and MERGE — per outer row, its surviving rows in production order, else one
/// null-fill row — the round's combined chunk becoming the next round's outer
/// chunk; finally feed the combined chunk to the recognised tail. The nesting
/// is the interpreter's: round k+1 null-fills PER round-k output row, and round
/// k's output order IS `drive`'s clause-k output order, so a later merge nests
/// inside the earlier one exactly. Byte-identical to the per-tuple `exec_match`
/// followed by aggregate/project on every accepted shape, or `Ok(None)` (a
/// filter/column budget decline; the general path answers).
fn run_optional(
    graph: &Graph,
    plan: &OptionalPlan,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    // 1. OUTER chunk: scan + outer hops + outer WHERE.
    let Some(outer_chunk) = build_chunk(
        graph,
        &plan.outer_a_labels,
        &plan.outer_a_var,
        &plan.outer_hops,
        where_slice(&plan.outer_where),
        None,
        params,
    )?
    else {
        return Ok(None);
    };

    // 2. LEFT-JOIN null-extension, once per OPTIONAL clause. Only the
    // non-OPTIONAL outer's columns (`plan.outer_vars`) are never null; a later
    // round's outer chunk carries the earlier rounds' null-fills.
    let mut chunk = outer_chunk;
    for stage in &plan.stages {
        let Some(next) = left_join_null_extend(
            graph,
            chunk,
            stage.outer_len,
            plan.outer_vars.len(),
            &stage.hops,
            stage.where_.as_ref(),
            &plan.combined_vars[..stage.vars_after],
            &plan.combined_var_kinds[..stage.vars_after],
            params,
        )?
        else {
            return Ok(None); // a filter budget / non-boolean decline
        };
        chunk = next;
    }

    // Feed the combined chunk to the recognised tail. The aggregate tail runs
    // through `finish_aggregate` then re-wraps with `finish_optional`, so BOTH the
    // aggregate and the optional operator counters record (unchanged behaviour).
    match &plan.tail {
        OptionalTail::Agg(agg) => {
            match run_aggregate_over_chunk(graph, agg, params, &chunk, finish_aggregate)? {
                Some(r) => finish_optional(r),
                None => Ok(None),
            }
        }
        OptionalTail::Core(core) => {
            run_core_over_chunk(graph, core, params, &chunk, finish_optional)
        }
    }
}

/// Project an already-built chunk through the non-aggregating (core) tail — the
/// production-order rows, or a bounded top-k when the RETURN carries ORDER BY +
/// LIMIT. The OPTIONAL twin of `plan_and_run_columnar`'s projection tail; a
/// nullable var materialises to `Value::Null` via `project_rows_tail`'s
/// `node_of`. `Ok(None)` = a column-budget / type decline in the top-k.
fn run_core_over_chunk(
    graph: &Graph,
    plan: &CorePlan,
    params: &BTreeMap<String, Value>,
    chunk: &DataChunk,
    finish: FinishFn,
) -> Result<Option<QueryResult>, RunError> {
    if plan.proj.order.is_empty() {
        let rows = chunk.live_rows();
        return finish(project_rows_tail(
            graph,
            &plan.proj,
            params,
            &plan.vars,
            &plan.var_kinds,
            &rows,
        )?);
    }
    if chunk.live() == 0 {
        return finish(project_rows_tail(
            graph,
            &plan.proj,
            params,
            &plan.vars,
            &plan.var_kinds,
            &[],
        )?);
    }
    match native_topk(graph, params, plan, chunk)? {
        Some(r) => finish(r),
        None => Ok(None),
    }
}

/// Record that the OPTIONAL-MATCH left join produced the answer — a distinct
/// counter from `finish`/`finish_aggregate`, so a test can assert the OPTIONAL
/// path fired (and that unrelated shapes still decline).
fn finish_optional(result: QueryResult) -> Result<Option<QueryResult>, RunError> {
    counted!("interp.pipeline optional runs");
    Ok(Some(result))
}

// ─── MULTI-STAGE WITH — `MATCH … WITH [DISTINCT] <vars> MATCH … RETURN …` ──────

/// A recognised TWO-STAGE read: a stage-1 MATCH+WHERE, a `WITH [DISTINCT]` that
/// carries pattern variables forward, a stage-2 MATCH+WHERE continuing from a
/// carried var, then the RETURN. This is LDBC SNB IC5's stage-1→stage-2 shape
/// (`… WITH DISTINCT friend MATCH (friend)<-[:HAS_MEMBER]-(forum) …`).
///
/// The runner builds the stage-1 chunk (`build_chunk`), PROJECTS it to the
/// carried vars at the WITH boundary (`DataChunk::project_carried`, optionally
/// dedup-first-seen for DISTINCT, always resetting relationship isomorphism),
/// then EXPANDS stage 2 out of the carried var(s) exactly as the read chain does,
/// and finishes through the SAME core/aggregate/distinct tail the single-stage
/// pipeline uses — stamped with `finish_multistage`.
struct MultiStagePlan {
    /// Stage 1: scan start, expand/semijoin steps and the WHERE.
    s1_a_labels: Vec<String>,
    s1_a_var: String,
    s1_hops: Vec<Hop>,
    /// The stage-1 WHERE as a CONJUNCTION of tractable predicates (IC9's
    /// `person.id = $pid AND person <> friend`), plus any desugared INLINE
    /// `{prop: val}` start anchor, each applied as early as its vars bind — the
    /// SAME `Vec<WherePred>` machinery `recognise_multistage_join` uses.
    s1_wheres: Vec<WherePred>,
    /// A seekable source-property anchor for the stage-1 scan — from the inline
    /// `(person:Person {id: val})` map OR a `person.id = val` WHERE equality — so
    /// the scan SEEDS the range index instead of the whole label. `None` when the
    /// stage-1 start carries no seekable equality (the whole label is scanned).
    s1_anchor: Option<PropAnchor>,
    /// Carried-var indices INTO the stage-1 chain's `vars`, in the WITH's item
    /// order — the columns kept at the boundary. Their vars/kinds become the LOW
    /// indices of the stage-2 binding order.
    carried: Vec<usize>,
    /// `WITH DISTINCT`: dedup the carried tuples first-seen BEFORE stage 2.
    distinct: bool,
    /// Stage 2: expand/semijoin steps over the seed chunk. `src`/`tgt` index the
    /// stage-2 chain's `vars` (carried vars occupy the low indices).
    s2_hops: Vec<Hop>,
    /// Stage-2 WHERE, applied over the stage-2 chunk before the RETURN tail.
    s2_wheres: Vec<WherePred>,
    /// The RETURN tail over the stage-2 vars.
    tail: MultiStageTail,
}

/// The stage-2 RETURN tail — the SAME plans the single-stage pipeline recognises,
/// reused verbatim over the stage-2 chunk.
enum MultiStageTail {
    Core(Box<CorePlan>),
    Agg(Box<AggPlan>),
    Distinct(Box<AggPlan>),
}

/// Recognise `MATCH <chain1> [WHERE] WITH [DISTINCT] <carried vars> MATCH
/// <chain2> [WHERE] RETURN <items> [ORDER BY/SKIP/LIMIT]`. The clause shape
/// (`[Match, With, Match, Return]`) is DISJOINT from every other recognizer
/// (`[Match, Return]`, `[Match, With, Return]`, `[Match, OPTIONAL Match, …]`), so
/// trying it claims only this shape. Its STAGE 1 uses the SAME anchor +
/// var-length + `Vec<WherePred>` conjunction machinery `recognise_multistage_join`
/// does — an inline `(person:Person {id:$pid})` / `person.id = $pid` seek anchor
/// (`s1_anchor` via `prop_eq_index`), a frontier-BFS var-length hop consumed
/// DISTINCT-only by the WITH, and a split conjunction WHERE including the two-var
/// `person <> friend` — so IC9's stage 1 is recognised. DECLINES (byte-identical
/// fallback): a WITH that aggregates / orders / pages / renames / projects a
/// computed expr or `*` or a post-WITH WHERE; a WITH dropping the variable chain2
/// needs (chain2's start is then unbound → `collect_hops` declines); a chain2
/// disconnected from every carried var; three-plus stages (`WITH … WITH …`); an
/// unbounded `*`, a path/rel var, a multi-entry anchor map or a non-splittable
/// stage-1 WHERE; and anything the sub-recognizers (`collect_hops` /
/// `recognise_where_preds` / the tail builders) already decline.
/// Recognise `WITH collect(DISTINCT x) AS xs` immediately followed by
/// `UNWIND xs AS x` — a GLOBAL collect-distinct unwound straight back to the SAME
/// variable, which yields EXACTLY the rows of `WITH DISTINCT x` (IC9's
/// `WITH collect(DISTINCT friend) AS friends UNWIND friends AS friend`). Returns
/// the distinct variable `x`. The collected list `xs` is dropped by the
/// normalisation; if anything downstream reads it the stage-2 / tail recognisers
/// decline (it is not among the carried vars), so the general path answers and
/// parity holds — the collapse is safe precisely because a leak self-declines.
fn collect_distinct_unwind_var(
    with_proj: &Projection,
    unwind_expr: &Expr,
    unwind_alias: &str,
) -> Option<String> {
    if with_proj.distinct
        || with_proj.star
        || !with_proj.order.is_empty()
        || with_proj.skip.is_some()
        || with_proj.limit.is_some()
        || with_proj.items.len() != 1
    {
        return None;
    }
    let item = &with_proj.items[0];
    let list_alias = item.alias.as_deref()?; // `AS xs`
    let Expr::Call {
        name,
        distinct,
        args,
        star,
    } = &item.expr
    else {
        return None;
    };
    if name != "collect" || !*distinct || *star || args.len() != 1 {
        return None;
    }
    let Expr::Var(x) = &args[0] else {
        return None; // collect(DISTINCT <non-var>)
    };
    // UNWIND xs AS x — the list is the collect alias, the element rebinds `x`
    // (a rename `UNWIND xs AS y` is a different shape, left to the general path).
    let Expr::Var(uv) = unwind_expr else {
        return None;
    };
    if uv != list_alias || unwind_alias != x {
        return None;
    }
    Some(x.clone())
}

fn recognise_multistage(sq: &SingleQuery) -> Option<MultiStagePlan> {
    // Two synthesised `WITH DISTINCT x` (one per collect-unwind arm), declared
    // here so they outlive the match that borrows them.
    let synth_with: Projection;
    let synth_with_agg: Projection;
    // `s2w` = an OPTIONAL stage-2 Form-A aggregate WITH (`WITH <agg> [WHERE
    // having]` before the RETURN) — `Some((proj, having))` when the stage-2 tail
    // aggregates through a WITH (IC6's `WITH tag.name AS tagName, count(post) AS
    // postCount`), `None` for the plain `MATCH … RETURN` stage-2 tail. It rides
    // ONLY into the tail builder; the stem (stage-1, carry, stage-2 hops/WHERE) is
    // identical in both cases.
    #[allow(clippy::type_complexity)]
    let (p1, w1, wp, with_where, p2, w2, s2w, rp): (
        _,
        _,
        &Projection,
        _,
        _,
        _,
        Option<(&Projection, Option<&Expr>)>,
        &Projection,
    ) = match sq.clauses.as_slice() {
        [
            Clause::Match {
                optional: false,
                pattern: p1,
                where_: w1,
            },
            Clause::With {
                proj: wp,
                where_: with_where,
            },
            Clause::Match {
                optional: false,
                pattern: p2,
                where_: w2,
            },
            Clause::Return { proj: rp },
        ] => (
            p1,
            w1.as_ref(),
            wp,
            with_where.as_ref(),
            p2,
            w2.as_ref(),
            None,
            rp,
        ),
        // Stage-2 Form-A AGGREGATE tail: `MATCH … WITH <carry> MATCH … WITH <agg>
        // [WHERE having] RETURN …` (IC6, post-prelude). The trailing WITH is the
        // aggregate; the ordinary stem runs unchanged and the tail is built Form A.
        [
            Clause::Match {
                optional: false,
                pattern: p1,
                where_: w1,
            },
            Clause::With {
                proj: wp,
                where_: with_where,
            },
            Clause::Match {
                optional: false,
                pattern: p2,
                where_: w2,
            },
            Clause::With {
                proj: s2wp,
                where_: s2ww,
            },
            Clause::Return { proj: rp },
        ] => (
            p1,
            w1.as_ref(),
            wp,
            with_where.as_ref(),
            p2,
            w2.as_ref(),
            Some((s2wp, s2ww.as_ref())),
            rp,
        ),
        // The `collect(DISTINCT x) AS xs` + `UNWIND xs AS x` variant (IC9): the
        // pair normalises to `WITH DISTINCT x`, then the ordinary two-stage logic
        // below runs unchanged. The collect WITH carries NO post-WITH WHERE.
        [
            Clause::Match {
                optional: false,
                pattern: p1,
                where_: w1,
            },
            Clause::With {
                proj: cwp,
                where_: None,
            },
            Clause::Unwind { expr, alias },
            Clause::Match {
                optional: false,
                pattern: p2,
                where_: w2,
            },
            Clause::Return { proj: rp },
        ] => {
            let dvar = collect_distinct_unwind_var(cwp, expr, alias)?;
            synth_with = Projection {
                distinct: true,
                star: false,
                items: vec![ProjItem {
                    expr: Expr::Var(dvar),
                    alias: None,
                    text: None,
                }],
                order: Vec::new(),
                skip: None,
                limit: None,
            };
            (
                p1,
                w1.as_ref(),
                &synth_with,
                None,
                p2,
                w2.as_ref(),
                None,
                rp,
            )
        }
        // The collect-unwind variant WITH a stage-2 Form-A aggregate tail (IC6):
        // `MATCH … WITH collect(DISTINCT x) AS xs UNWIND xs AS x MATCH … WITH <agg>
        // [WHERE having] RETURN …`.
        [
            Clause::Match {
                optional: false,
                pattern: p1,
                where_: w1,
            },
            Clause::With {
                proj: cwp,
                where_: None,
            },
            Clause::Unwind { expr, alias },
            Clause::Match {
                optional: false,
                pattern: p2,
                where_: w2,
            },
            Clause::With {
                proj: s2wp,
                where_: s2ww,
            },
            Clause::Return { proj: rp },
        ] => {
            let dvar = collect_distinct_unwind_var(cwp, expr, alias)?;
            synth_with_agg = Projection {
                distinct: true,
                star: false,
                items: vec![ProjItem {
                    expr: Expr::Var(dvar),
                    alias: None,
                    text: None,
                }],
                order: Vec::new(),
                skip: None,
                limit: None,
            };
            (
                p1,
                w1.as_ref(),
                &synth_with_agg,
                None,
                p2,
                w2.as_ref(),
                Some((s2wp, s2ww.as_ref())),
                rp,
            )
        }
        _ => return None,
    };

    // The WITH carries ONLY already-bound pattern variables and nothing else: NO
    // post-WITH WHERE, NO `*`, NO ORDER BY / SKIP / LIMIT, and at least one item.
    // (DISTINCT is the one modifier allowed; each item is validated below.)
    if with_where.is_some()
        || wp.star
        || !wp.order.is_empty()
        || wp.skip.is_some()
        || wp.limit.is_some()
        || wp.items.is_empty()
    {
        return None;
    }

    // Stage 1: the read chain. `recognise_chain` discards per-var labels, but the
    // stage-2 restate check needs them, so drive `collect_hops` directly and layer
    // the WHERE on separately (exactly what `recognise_chain` does internally).
    // `allow_start_anchor` = the INLINE `(person:Person {id: val})` start anchor
    // (IC9's seed) — the SAME opt-in `recognise_multistage_join` uses. A
    // HOPLESS stage 1 — `MATCH (t:ResearchTask {userId: $u}) WITH t MATCH
    // (t)-[:PROPOSED_GRAPH_WRITE]->(p:GraphWriteProposal) RETURN count(p)` —
    // is a filtered scan carried into stage 2, exactly the OPTIONAL outer's
    // shape; it ran on the general path (416 seeds expanded one at a time,
    // 6.7 ms on the mirror) while the same hop written in ONE MATCH ran here
    // in 0.8.
    let hc1 = collect_hops(p1, None, true, true, false)?;
    // DESUGAR the inline start anchor into a source-var equality `a.prop = val`
    // and AND it into the stage-1 WHERE, so the single split below carries BOTH
    // the anchor and any textual predicate (IC9's `person <> friend`). The seekable
    // form is then picked out via `prop_eq_index` (interp's own detector) for the
    // index-anchored scan; the equality still runs as a filter (byte-identical).
    let anchor_eq: Option<Expr> = hc1.start_anchor.as_ref().map(|(prop, val)| {
        Expr::Bin(
            engram_cypher::ast::BinOp::Eq,
            Box::new(Expr::Prop(
                Box::new(Expr::Var(hc1.a_var.clone())),
                prop.clone(),
            )),
            Box::new(val.clone()),
        )
    });
    let combined_where: Option<Expr> = match (anchor_eq, w1.cloned()) {
        (Some(a), Some(w)) => Some(Expr::And(Box::new(a), Box::new(w))),
        (Some(a), None) => Some(a),
        (None, w) => w,
    };
    // Split the (possibly conjunctive) stage-1 WHERE into per-predicate filters —
    // IC9's `person.id = $pid AND person <> friend`. DECLINE (the whole query to
    // the general path) if any conjunct is neither a single-var prop pred nor a
    // two-var id pred — the SAME `Vec<WherePred>` gate the join path uses.
    let s1_wheres = recognise_where_preds(combined_where.as_ref(), &hc1.vars)?;
    // The seekable source anchor for the scan seed, if the combined WHERE carries a
    // `a.prop = <var-free>` equality on the scan var (interp's `Seed::PropEq`).
    let s1_anchor = prop_eq_index(combined_where.as_ref(), &hc1.a_var)
        .map(|(prop, values)| PropAnchor { prop, values });
    // FRONTIER-BFS var-length in STAGE 1 (IC9's `KNOWS*1..2`): its end var must be
    // consumed DISTINCT-only by the WITH breaker — the SAME `frontier_ok` gate.
    // (A `WITH DISTINCT friend` carries `friend` distinct-only; a plain `WITH
    // friend` does not, so it declines.) A var-length hop in stage 2 is out of
    // scope here — decline it to the general path.
    if !varlen_distinct_consumed(&hc1.hops, wp) {
        return None;
    }

    // Validate the WITH projection: every item a BARE bound pattern var, or
    // `v AS v` (same name). Collect the carried names in item order; DECLINE a
    // rename (`v AS w`), a non-var expr (`v.p`, a literal, a function), an
    // aggregate, an unbound name, or a duplicate carry (a name collision).
    let mut carried_names: Vec<String> = Vec::with_capacity(wp.items.len());
    for it in &wp.items {
        if expr_has_aggregate(&it.expr) {
            return None; // an aggregate in the WITH — deferred
        }
        let Expr::Var(v) = &it.expr else {
            return None; // not a bare pattern variable
        };
        if let Some(a) = &it.alias {
            if a != v {
                return None; // an alias-rename to a different name — deferred
            }
        }
        if !hc1.vars.contains(v) {
            return None; // not a bound pattern var of stage 1
        }
        if carried_names.contains(v) {
            return None; // a duplicate carry — decline
        }
        carried_names.push(v.clone());
    }

    // Carried-var indices into stage-1 vars, plus the carried KINDS and LABELS the
    // stage-2 re-root / label-restate check consumes.
    let carried: Vec<usize> = carried_names
        .iter()
        .map(|v| {
            hc1.vars
                .iter()
                .position(|x| x == v)
                .expect("carried var is bound in stage 1")
        })
        .collect();
    let carried_kinds: Vec<VarKind> = carried.iter().map(|&i| hc1.var_kinds[i]).collect();
    let mut carried_labels: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for v in &carried_names {
        if let Some(ls) = hc1.var_labels.get(v) {
            carried_labels.insert(v.clone(), ls.clone());
        }
    }

    // Stage 2: EVERY path re-roots at a CARRIED var (prebound = the carried vars
    // ONLY). `collect_hops` declines a stage-2 start not among them — a chain2
    // disconnected from the carry, or one continuing from a DROPPED var.
    let prebound = (
        carried_names.as_slice(),
        carried_kinds.as_slice(),
        &carried_labels,
    );
    let hc2 = collect_hops(p2, Some(prebound), false, false, true)?;
    // A var-length hop in STAGE 2 is out of scope for this increment — decline the
    // whole query to the general path.
    if hops_have_varlen(&hc2.hops) {
        return None;
    }
    // Stage-2 WHERE — the ORIGINAL single-var/two-var-identity recognizer, UNCHANGED
    // (so existing shapes stay byte-identical), plus each mid-hop inline anchor as a
    // SEPARATE single-var pred. IC11's `workAt.workFrom < 2015` (the WHERE) and
    // `country.name = 'Country0'` (the folded anchor) are then two filters applied in
    // turn. An anchor that is not a single-var pred declines the whole query.
    let mut s2_wheres: Vec<WherePred> = Vec::new();
    if let Some(pred) = recognise_single_var_where(w2, &hc2.vars)? {
        s2_wheres.push(pred);
    }
    for anchor in &hc2.node_anchors {
        match recognise_single_var_where(Some(anchor), &hc2.vars)? {
            Some(pred) => s2_wheres.push(pred),
            None => return None,
        }
    }

    // The stage-2 chain feeds the tail recognizers (they read only `vars`); its
    // WHERE is applied by the runner, so the tail plan carries `where_: None`.
    let chain2 = Chain {
        a_labels: hc2.a_labels.clone(),
        a_var: hc2.a_var.clone(),
        hops: hc2.hops.clone(),
        vars: hc2.vars.clone(),
        var_kinds: hc2.var_kinds.clone(),
        wheres: Vec::new(),
        start_anchor: None,
    };

    // The stage-2 tail. With a stage-2 aggregate WITH (`s2w`), the tail is a
    // Form-A aggregate (`WITH <agg> [WHERE having] RETURN …`) — the group-by runs
    // over the stage-2 chunk, then the RETURN projects the WITH aliases; IC6's
    // `WITH tag.name AS tagName, count(post) AS postCount RETURN …`. Otherwise the
    // SAME dispatch order as `plan_and_run_columnar`: an aggregating RETURN → Agg;
    // else a DISTINCT RETURN → Distinct; else Core. (`RETURN *` is declined by all,
    // falling to the general path.) The stage-2 MATCH WHERE (`w2`) is applied by
    // the runner (`s2_wheres`) either way — it is NOT the Form-A HAVING.
    let tail = match s2w {
        // A DISTINCT stage-2 WITH → a Form-A DISTINCT tail (the varlen-split's
        // `WITH DISTINCT friend RETURN …`); otherwise an aggregate tail.
        Some((s2wp, having)) if s2wp.distinct => MultiStageTail::Agg(Box::new(
            distinct_form_a_over_chain(chain2, s2wp, having, rp)?,
        )),
        Some((s2wp, having)) => MultiStageTail::Agg(Box::new(aggregate_over_chain(
            chain2,
            s2wp,
            Some((having, rp)),
        )?)),
        None if rp.items.iter().any(|it| expr_has_aggregate(&it.expr)) => {
            MultiStageTail::Agg(Box::new(aggregate_over_chain(chain2, rp, None)?))
        }
        None if rp.distinct => MultiStageTail::Distinct(Box::new(distinct_over_chain(chain2, rp)?)),
        None => MultiStageTail::Core(Box::new(core_over_chain(chain2, rp)?)),
    };

    Some(MultiStagePlan {
        s1_a_labels: hc1.a_labels,
        s1_a_var: hc1.a_var,
        s1_hops: hc1.hops,
        s1_wheres,
        s1_anchor,
        carried,
        distinct: wp.distinct,
        s2_hops: hc2.hops,
        s2_wheres,
        tail,
    })
}

/// Run a recognised multi-stage WITH: stage-1 chunk → project (+ optional dedup)
/// to carried vars at the WITH boundary → stage-2 expand out of the carried
/// var(s) → stage-2 WHERE → the RETURN tail, stamped `finish_multistage`.
/// Byte-identical to `run_streaming` over the two stages, or `Ok(None)` (a
/// budget / column decline; the general path answers identically).
/// An IC9-shaped index-ordered top-k opportunity extracted from a multistage
/// plan's stage 2 + tail (see [`recognise_index_topk`]).
struct IndexTopkPlan {
    /// The stage-2 index of the end var (message) — always 1 here (friend@0).
    end_var: usize,
    edge_types: Vec<String>,
    /// Direction FROM the end var (message) TO the carried var (friend) — the
    /// reverse of the stage-2 hop, which expands friend → message.
    op_dir: Dir,
    order_prop: String,
    tie_prop: String,
    /// The `< bound` upper (a const/param), evaluated at run time.
    bound: Expr,
    limit: Expr,
}

/// `Expr::Prop(Var(v), key)` → `(v, key)`.
fn prop_of_var(e: &Expr) -> Option<(&str, &str)> {
    if let Expr::Prop(base, key) = e {
        if let Expr::Var(v) = base.as_ref() {
            return Some((v.as_str(), key.as_str()));
        }
    }
    None
}

/// Recognise IC9's stage 2 as an index-ordered top-k: a SINGLE fixed semijoin
/// hop from the ONE carried var (friend) to a new end var (message), a stage-2
/// `end.P < bound` WHERE, and a Core tail ordering `end.P DESC, end.Q ASC LIMIT
/// k` (aliases resolved). `(P, Q)` = (creationDate, message.id) is a total order,
/// so the operator's result is the unique true top-k — byte-identical to the
/// expand-then-`native_topk` path. Any other shape → `None` (the plan runs its
/// normal stage 2). The RUN-TIME selectivity decision (fire vs fall back) is the
/// operator's own scan-budget bail, not made here.
fn recognise_index_topk(plan: &MultiStagePlan) -> Option<IndexTopkPlan> {
    use engram_cypher::ast::BinOp;
    // Exactly one carried var (friend@0), reached by exactly one fixed semijoin
    // hop that introduces a NEW end var (message) — `tgt` is `None` for a new
    // var (it is `Some` only when a hop joins back to an already-bound var). The
    // new var lands at index `carried.len()`, so the stage-2 chunk is exactly
    // [friend@0, message@1] and `try_index_topk` can rebuild it.
    if plan.carried.len() != 1 || plan.s2_hops.len() != 1 {
        return None;
    }
    let hop = &plan.s2_hops[0];
    if hop.varlen.is_some()
        || hop.rel_var.is_some()
        || hop.tgt.is_some()
        || hop.types.is_empty()
        || hop.src != 0
    {
        return None;
    }
    let end_var = plan.carried.len(); // = 1

    let MultiStageTail::Core(core) = &plan.tail else {
        return None;
    };
    if core.vars.get(end_var).map(String::as_str) != Some(hop.var.as_str()) {
        return None; // the stage-2 chunk's var 1 must be the hop's new end var
    }
    let proj = &core.proj;
    if proj.distinct || proj.star || proj.skip.is_some() || proj.order.len() != 2 {
        return None;
    }
    let limit = proj.limit.clone()?;
    let end_name = core.vars.get(end_var)?.as_str();

    // ORDER BY end.P DESC, end.Q ASC (resolving RETURN aliases).
    if !proj.order[0].desc || proj.order[1].desc {
        return None;
    }
    let o0 = resolve_order_key_alias(&proj.order[0].expr, &core.vars, proj);
    let o1 = resolve_order_key_alias(&proj.order[1].expr, &core.vars, proj);
    let (v0, order_prop) = prop_of_var(&o0)?;
    let (v1, tie_prop) = prop_of_var(&o1)?;
    if v0 != end_name || v1 != end_name {
        return None;
    }

    // Stage-2 WHERE: exactly `end.P < bound`, same P as the primary sort key.
    if plan.s2_wheres.len() != 1 {
        return None;
    }
    let Expr::Bin(BinOp::Lt, lhs, rhs) = &plan.s2_wheres[0].expr else {
        return None;
    };
    let (wv, wprop) = prop_of_var(lhs)?;
    if wv != end_name || wprop != order_prop {
        return None;
    }

    Some(IndexTopkPlan {
        end_var,
        edge_types: hop.types.clone(),
        op_dir: match hop.dir {
            Dir::Out => Dir::In,
            Dir::In => Dir::Out,
            Dir::Both => Dir::Both,
        },
        order_prop: order_prop.to_string(),
        tie_prop: tie_prop.to_string(),
        bound: (**rhs).clone(),
        limit,
    })
}

/// Run the index-ordered top-k for a recognised IC9 stage 2 over the carried
/// (friend) seed `chunk`. Returns `Ok(Some(result))` when the operator serves
/// it; `Ok(None)` when it declines (a non-integer bound/order, or the
/// scan-budget bail on a too-selective filter) so `run_multistage` runs its
/// normal stage 2. Byte-identical: the winners feed the SAME Core tail.
fn try_index_topk(
    graph: &Graph,
    itk: &IndexTopkPlan,
    core: &CorePlan,
    chunk: &DataChunk,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    // The bound and LIMIT must be integer constants/params at run time.
    let empty_vm = VarMap::new();
    let scope = Scope::over(params, &empty_vm, graph.wall_ms(), graph.zone_provider());
    let Value::Int(upper) = eval_with(&itk.bound, &scope, None).map_err(RunError::Eval)? else {
        return Ok(None);
    };
    let Some(limit) = crate::interp::eval_count(graph, Some(&itk.limit), params, "LIMIT")? else {
        return Ok(None);
    };

    // The carried (friend) set — the semijoin filter.
    let friend_col = &chunk.ids[0];
    let friends: std::collections::BTreeSet<u64> =
        chunk.selection.iter().map(|&r| friend_col[r]).collect();

    // Scan budget tied to the filter size: the operator may scan ~8×|friends|
    // index entries before it must have filled K, else the filter is too
    // selective and it bails to the expand path (IC2). Floored so a tiny but
    // dense set still gets a fair scan.
    let scan_budget = friends
        .len()
        .saturating_mul(8)
        .max(limit.saturating_mul(64));

    let edge_tokens = graph.type_tokens_peek(&itk.edge_types);
    let Some(winner_ids) = graph
        .index_ordered_topk_semijoin(
            &itk.order_prop,
            upper,
            &edge_tokens,
            itk.op_dir,
            &friends,
            &itk.tie_prop,
            limit,
            scan_budget,
        )
        .map_err(RunError::Graph)?
    else {
        return Ok(None); // operator declined (bail / non-int) → normal stage 2
    };

    // Build a 2-column stage-2 chunk (friend, message) for the winners: each
    // winner message's creator is its `op_dir` neighbour in the friend set, and
    // that is exactly the row the expand path would have produced. Feed the SAME
    // Core tail (its `native_topk` re-sorts these ≤K rows into the identical
    // order and late-materialises the projection) — byte-identical.
    let mut friend_ids: Vec<u64> = Vec::with_capacity(winner_ids.len());
    for &m in &winner_ids {
        let mut creator = None;
        graph.adjacent_slim_for_each(m, itk.op_dir, &edge_tokens, |e| {
            if creator.is_none() && friends.contains(&e.peer) {
                creator = Some(e.peer);
            }
        });
        // A winner came out of the operator BECAUSE its creator is in the set;
        // it is always found. (Defensive: skip if somehow absent.)
        let Some(c) = creator else { continue };
        friend_ids.push(c);
    }
    debug_assert_eq!(friend_ids.len(), winner_ids.len());

    let vars = vec![core.vars[0].clone(), core.vars[itk.end_var].clone()];
    let var_kinds = vec![core.var_kinds[0], core.var_kinds[itk.end_var]];
    let winner_chunk = DataChunk::from_columns(vars, var_kinds, vec![friend_ids, winner_ids]);
    counted!("interp.pipeline index-ordered topk served stage 2");
    run_core_over_chunk(graph, core, params, &winner_chunk, finish_multistage)
}

fn run_multistage(
    graph: &Graph,
    plan: &MultiStagePlan,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    counted!("interp.dispatch run_multistage");
    // The frontier-BFS toggle gate (stage 1 is where a var-length hop lives) —
    // decline the columnar BFS when `run_streaming` would not take it, so ON == OFF.
    if hops_have_varlen(&plan.s1_hops) && !graph.frontier_expand_enabled() {
        return Ok(None);
    }
    // STAGE 1: scan (SEEKING the range index when `s1_anchor` is present) +
    // expand/semijoin + the stage-1 WHERE conjunction — the SAME anchored
    // `build_chunk` call `run_multistage_join` makes.
    let Some(s1_chunk) = build_chunk(
        graph,
        &plan.s1_a_labels,
        &plan.s1_a_var,
        &plan.s1_hops,
        &plan.s1_wheres,
        plan.s1_anchor.as_ref(),
        params,
    )?
    else {
        return Ok(None); // a filter budget / non-boolean decline
    };

    // THE WITH BOUNDARY: project to the carried vars, optionally dedup first-seen
    // (DISTINCT), and RESET relationship isomorphism (fresh empty `used_rels`).
    // The result is the SEED for stage 2 — a fresh chunk exactly like
    // `DataChunk::seed`, so stage 2 relies on that empty base + each hop's own
    // `reset` flag (a later path's first hop), the SAME discipline the read chain
    // uses; no reset is forced here.
    let mut chunk = s1_chunk.project_carried(graph, &plan.carried, plan.distinct)?;

    // INDEX-ORDERED TOP-K (IC9's lever): if stage 2 is a semijoin from the
    // carried set to newest-first messages under a LIMIT, try serving it by
    // scanning the ORDER-BY property's index DESC and filtering against the
    // carried set — a win when that set is non-selective. The operator's own
    // scan-budget bail returns `None` for a too-selective set, and the shape
    // recognizer returns `None` for anything but this exact form; either way we
    // fall through to the normal expand+topk below, byte-identically.
    if let MultiStageTail::Core(core) = &plan.tail {
        if let Some(itk) = recognise_index_topk(plan) {
            if let Some(result) = try_index_topk(graph, &itk, core, &chunk, params)? {
                return Ok(Some(result));
            }
        }
    }

    // STAGE 2 end-label members + type tokens (computed once, as `build_chunk`
    // does). A hop whose named type was never minted yields no adjacency.
    let mut hop_members: Vec<Option<crate::MembersView>> =
        Vec::with_capacity(plan.s2_hops.len());
    let mut hop_tokens: Vec<Option<Vec<u32>>> = Vec::with_capacity(plan.s2_hops.len());
    for hop in &plan.s2_hops {
        let members = if hop.labels.is_empty() {
            None
        } else {
            Some(graph.members_all(&hop.labels).map_err(RunError::Graph)?)
        };
        hop_members.push(members);
        hop_tokens.push(graph.type_tokens_peek(&hop.types));
    }

    // BOUNDED-MEMORY STAGE 2 (Track B working-set bound): when the tail is a
    // top-k (ORDER BY + LIMIT), fold the stage-2 expansion into a bounded top-k
    // accumulator ONE DRIVING BATCH AT A TIME, so a high-fan-out expand over a
    // bigger-than-RAM graph never materialises the whole widened chunk — only the
    // current batch's expansion plus the `<= cap` winners stay resident. This is
    // byte-identical to the full-expand path below: a driving chunk that fits one
    // batch takes the identical single-push route, and batches expand in
    // production order so the global seq matches. Skipped when the chunk carries
    // OPTIONAL provenance (`prov`) — batching would renumber it — so that path
    // keeps the whole-chunk expand.
    if let MultiStageTail::Core(core) = &plan.tail {
        if graph.multistage_topk_batch_enabled()
            && core.proj.limit.is_some()
            && !core.proj.order.is_empty()
            && chunk.prov.is_empty()
        {
            return batched_core_last_stage(
                graph,
                core,
                params,
                &chunk,
                &plan.s2_hops,
                &plan.s2_wheres,
                &hop_members,
                &hop_tokens,
            );
        }
    }

    // STAGE 2: expand/semijoin from the carried var(s), in production order —
    // passing each hop's own `reset` (NOT a forced one), byte-identical to the
    // read chain over a fresh seed.
    for (i, hop) in plan.s2_hops.iter().enumerate() {
        let members_slice: Option<&crate::MembersView> = hop_members[i].as_ref();
        chunk = run_hop(graph, chunk, hop, members_slice, &hop_tokens[i])?;
    }

    // STAGE-2 WHERE — each per-predicate filter in turn (rel-prop, node-anchor, …).
    for pred in &plan.s2_wheres {
        if chunk.filter(graph, params, pred)?.is_none() {
            return Ok(None); // a budget / non-boolean decline
        }
    }

    // THE RETURN TAIL over the stage-2 chunk — the SAME core/aggregate/distinct
    // operators the single-stage pipeline uses, stamped with the multistage
    // counter so a test can assert this path fired.
    match &plan.tail {
        MultiStageTail::Core(core) => {
            run_core_over_chunk(graph, core, params, &chunk, finish_multistage)
        }
        MultiStageTail::Agg(agg) => {
            run_aggregate_over_chunk(graph, agg, params, &chunk, finish_multistage)
        }
        MultiStageTail::Distinct(dp) => {
            run_distinct_over_chunk(graph, dp, params, &chunk, finish_multistage)
        }
    }
}

/// Driving rows per stage-2 batch in [`run_multistage_core_batched`]. The widened
/// chunk a batch materialises is bounded by `BATCH × per-row-fan-out`, so this
/// trades a tighter memory bound (smaller) against per-batch key-column setup
/// (larger). Chosen so the warm/resident benchmark's small driving sets stay a
/// SINGLE batch (identical to the historical single-pass), and only genuinely
/// large bigger-than-RAM driving sets split.
const MULTISTAGE_TOPK_BATCH: usize = 1024;

/// Expand ONE driving batch through a stage's `hops` + `wheres`, or `Ok(None)`
/// on a per-batch decline. The shared per-batch step of both batched tails.
fn expand_last_stage_batch(
    graph: &Graph,
    params: &BTreeMap<String, Value>,
    mut bc: DataChunk,
    hops: &[Hop],
    wheres: &[WherePred],
    hop_members: &[Option<crate::MembersView>],
    hop_tokens: &[Option<Vec<u32>>],
) -> Result<Option<DataChunk>, RunError> {
    for (i, hop) in hops.iter().enumerate() {
        let members_slice: Option<&crate::MembersView> = hop_members[i].as_ref();
        bc = run_hop(graph, bc, hop, members_slice, &hop_tokens[i])?;
    }
    for pred in wheres {
        if bc.filter(graph, params, pred)?.is_none() {
            return Ok(None); // a budget / non-boolean decline
        }
    }
    Ok(Some(bc))
}

/// Bounded-memory last stage for a **top-k** tail: expand the driving chunk ONE
/// BATCH of rows at a time, folding each batch's widened + WHERE-filtered rows
/// into a single [`TopKAcc`]. Peak resident = one batch's expansion + the
/// `<= cap` winners, instead of the whole friend×message cross-product. This is
/// byte-identical to `run_core_over_chunk` over the full expansion: top-k is a
/// monoid under merge-and-trim, the batches arrive in production order (so the
/// global `seq` tiebreak matches), and a driving chunk that fits one batch folds
/// in a single push. `Ok(None)` on any per-batch decline — the caller falls back
/// to the general path, which recomputes the identical result. Shared by the
/// two-stage `run_multistage` (stage 2) and the N-stage `run_pipeline` (last stage).
#[allow(clippy::too_many_arguments)]
fn batched_core_last_stage(
    graph: &Graph,
    core: &CorePlan,
    params: &BTreeMap<String, Value>,
    driving: &DataChunk,
    hops: &[Hop],
    wheres: &[WherePred],
    hop_members: &[Option<crate::MembersView>],
    hop_tokens: &[Option<Vec<u32>>],
) -> Result<Option<QueryResult>, RunError> {
    let cap = topk_cap(graph, core, params)?;
    let mut acc = TopKAcc::new(cap);
    let n = driving.live();
    let mut start = 0;
    let mut batched = false;
    while start < n {
        let end = (start + MULTISTAGE_TOPK_BATCH).min(n);
        // A genuine split (more than one batch) is what the counter records.
        batched |= start > 0 || end < n;
        let bc = driving.driving_slice(start, end);
        let Some(bc) =
            expand_last_stage_batch(graph, params, bc, hops, wheres, hop_members, hop_tokens)?
        else {
            return Ok(None);
        };
        if !acc.push_chunk(graph, params, core, &bc)? {
            return Ok(None); // a column-budget / type decline while loading keys
        }
        start = end;
    }
    if batched {
        counted!("interp.pipeline top-k batched");
    }
    finish_multistage(acc.finish(graph, params, core)?)
}

/// Bounded-memory last stage for a **group-by aggregate** tail (IC3's per-friend
/// `sum` shape): expand the driving chunk ONE BATCH at a time, `reduce_agg_groups`
/// each batch, and CONCATENATE the reduced groups. Peak resident = one batch's
/// expansion + the running (already-reduced, one row per group) group set.
///
/// Byte-identical PROVIDED **no group spans a batch boundary** — then each group
/// is fully folded within its batch, and concatenating in batch order reproduces
/// the single-pass first-seen group order (batches arrive in production order).
/// A runtime **collision guard** enforces exactly that: if a batch re-produces a
/// group key seen in an earlier batch, we `Ok(None)` → the general path (which
/// merges it correctly). For the target shape (`WITH DISTINCT <group-var> MATCH
/// … <group by that var>`) each driving tuple is a distinct group, so no
/// collision ever fires. A GLOBAL aggregate (empty group key) is NOT routed here
/// (every batch would collide on the empty key) — the caller checks.
/// One end of a `message.<date> <cmp> <int-const>` comparison, normalised so the
/// property is always on the left.
enum DateOp {
    Ge,
    Gt,
    Le,
    Lt,
}

/// Flatten a conjunction `a AND b AND …` into its leaf conjuncts (a non-`And`
/// expression is a single leaf). Chained comparisons (`T2 > x >= T1`) desugar to
/// an `And`, so this splits them back into per-bound comparisons.
fn flatten_and<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
    if let Expr::And(l, r) = e {
        flatten_and(l, out);
        flatten_and(r, out);
    } else {
        out.push(e);
    }
}

/// Read a WHERE conjunct as `<msg_var>.<prop> <cmp> <int-const>` (in either
/// orientation), returning the property, the normalised operator, and the integer
/// bound. `None` = not that shape (a NON-date-bound predicate over the message var
/// → the caller declines, since it can't be pre-filtered from the index).
fn date_cmp(
    e: &Expr,
    msg_var: &str,
    graph: &Graph,
    params: &BTreeMap<String, Value>,
) -> Option<(String, DateOp, i64)> {
    use engram_cypher::ast::BinOp;
    let Expr::Bin(op, l, r) = e else {
        return None;
    };
    let prop_of = |x: &Expr| -> Option<String> {
        if let Expr::Prop(b, p) = x {
            if matches!(b.as_ref(), Expr::Var(v) if v == msg_var) {
                return Some(p.clone());
            }
        }
        None
    };
    // A CONSTANT integer — evaluated in an EMPTY scope, so any expression that
    // reads a bound var (e.g. the property side of the comparison) simply fails
    // to evaluate and is reported as "not a constant" (None), never an error.
    let eval_int = |x: &Expr| -> Option<i64> {
        let vm = VarMap::new();
        let scope = Scope::over(params, &vm, graph.wall_ms(), graph.zone_provider());
        match eval_with(x, &scope, None) {
            Ok(Value::Int(v)) => Some(v),
            Ok(Value::Date(v)) => Some(v),
            _ => None,
        }
    };
    if let (Some(prop), Some(val)) = (prop_of(l), eval_int(r)) {
        let dop = match op {
            BinOp::Ge => DateOp::Ge,
            BinOp::Gt => DateOp::Gt,
            BinOp::Le => DateOp::Le,
            BinOp::Lt => DateOp::Lt,
            _ => return None,
        };
        return Some((prop, dop, val));
    }
    if let (Some(prop), Some(val)) = (prop_of(r), eval_int(l)) {
        // `const <op> prop` ⇔ `prop <flipped> const`.
        let dop = match op {
            BinOp::Ge => DateOp::Le,
            BinOp::Gt => DateOp::Lt,
            BinOp::Le => DateOp::Ge,
            BinOp::Lt => DateOp::Gt,
            _ => return None,
        };
        return Some((prop, dop, val));
    }
    None
}

/// IC3's date-windowed last stage. The pipeline's final stage
/// `(friend)<-[:HAS_CREATOR]-(message)-[:IS_LOCATED_IN]->(country)
///  WHERE message.<date> in the window AND <country membership>` otherwise reads
/// EVERY candidate message's date from the store to test the window — the
/// dominant cost (a 1-year window over friends' whole message history). This
/// seeks each friend's in-window messages from the date-ordered `creator_msgs`
/// index (dates carried inline — NO per-message store read), then expands the
/// country hop and applies the remaining WHEREs through the SAME `run_hop` +
/// `filter` + `reduce_agg_groups` + `finalize_agg_groups` the batched path uses,
/// so the CASE/sum/HAVING/order/limit tail is byte-identical. `Ok(None)` = the
/// shape does not match; the caller runs the ordinary batched path.
#[allow(clippy::too_many_arguments)]
fn try_ic3_datewindow(
    graph: &Graph,
    agg: &AggPlan,
    params: &BTreeMap<String, Value>,
    driving: &DataChunk,
    hops: &[Hop],
    wheres: &[WherePred],
    hop_members: &[Option<crate::MembersView>],
    hop_tokens: &[Option<Vec<u32>>],
) -> Result<Option<QueryResult>, RunError> {
    // Shape: HAS_CREATOR(In) → message, then one further fixed hop (IS_LOCATED_IN)
    // → country. No rel vars / var-length / semijoin.
    if hops.len() != 2 {
        return Ok(None);
    }
    let (h_msg, h_co) = (&hops[0], &hops[1]);
    if h_msg.types != ["HAS_CREATOR"]
        || !matches!(h_msg.dir, Dir::In)
        || h_msg.varlen.is_some()
        || h_msg.rel_var.is_some()
        || h_msg.tgt.is_some()
        || h_co.varlen.is_some()
        || h_co.rel_var.is_some()
        || h_co.tgt.is_some()
    {
        return Ok(None);
    }
    // The message column is appended after the carried (driving) columns, so the
    // country hop must drive from exactly `driving.vars.len()`.
    let friend_vi = h_msg.src;
    if friend_vi >= driving.vars.len() || h_co.src != driving.vars.len() {
        return Ok(None);
    }
    // Split the WHEREs: every message-var conjunct must be a date comparison we can
    // reproduce from the index (else decline — we must not skip a filter). All
    // other conjuncts (the country membership) are applied later, unchanged.
    let msg_var = &h_msg.var;
    let mut date_prop: Option<String> = None;
    let mut lo: Option<(i64, bool)> = None;
    let mut hi: Option<(i64, bool)> = None;
    let mut other_wheres: Vec<&WherePred> = Vec::new();
    for w in wheres {
        if &w.var == msg_var {
            // A message-var conjunct must be date bound(s) we can reproduce from
            // the index. A chained `T2 > x >= T1` desugars to an AND — flatten it.
            let mut conj: Vec<&Expr> = Vec::new();
            flatten_and(&w.expr, &mut conj);
            for c in conj {
                let Some((prop, op, val)) = date_cmp(c, msg_var, graph, params) else {
                    return Ok(None);
                };
                if date_prop.get_or_insert_with(|| prop.clone()) != &prop {
                    return Ok(None);
                }
                match op {
                    DateOp::Ge => lo = Some((val, true)),
                    DateOp::Gt => lo = Some((val, false)),
                    DateOp::Le => hi = Some((val, true)),
                    DateOp::Lt => hi = Some((val, false)),
                }
            }
        } else {
            other_wheres.push(w);
        }
    }
    let Some(date_prop) = date_prop else {
        return Ok(None);
    };
    // A bounded window (both ends) — the shape IC3 has and the shape that pays off.
    let (Some((lo_b, lo_inc)), Some((hi_b, hi_inc))) = (lo, hi) else {
        return Ok(None);
    };

    let Some(index) = creator_sorted_messages(graph, &h_msg.labels, &date_prop, &h_msg.types)?
    else {
        return Ok(None);
    };

    // (driving…, message): each live driving row × its friend's in-window messages,
    // the date taken from the index — never from the store.
    let ncols = driving.vars.len();
    let mut cols: Vec<Vec<u64>> = vec![Vec::new(); ncols + 1];
    for &r in &driving.selection {
        let friend = driving.ids[friend_vi][r];
        let Some(msgs) = index.get(&friend) else {
            continue;
        };
        for &(d, _mid, node) in msgs.iter() {
            let lo_ok = if lo_inc { d >= lo_b } else { d > lo_b };
            let hi_ok = if hi_inc { d <= hi_b } else { d < hi_b };
            if lo_ok && hi_ok {
                for (c, col) in cols.iter_mut().enumerate().take(ncols) {
                    col.push(driving.ids[c][r]);
                }
                cols[ncols].push(node);
            }
        }
    }
    let mut vars = driving.vars.clone();
    vars.push(h_msg.var.clone());
    let mut kinds = driving.var_kinds.clone();
    kinds.push(VarKind::Node);
    let mut chunk = DataChunk::from_columns(vars, kinds, cols);

    // Country hop + the remaining WHEREs through the proven primitives — byte-
    // identical to the batched path's country side.
    let co_members: Option<&crate::MembersView> = hop_members[1].as_ref();
    chunk = run_hop(graph, chunk, h_co, co_members, &hop_tokens[1])?;
    for w in &other_wheres {
        if chunk.filter(graph, params, w)?.is_none() {
            return Ok(None);
        }
    }

    // The SAME aggregate tail as `batched_agg_last_stage`, in a single pass (no
    // batch spanning) → projection/CASE/sum/HAVING/order/limit are byte-identical.
    let Some((groups, gkc)) = reduce_agg_groups(graph, agg, params, &chunk)? else {
        return Ok(None);
    };
    counted!("interp.pipeline ic3 datewindow");
    finalize_agg_groups(graph, agg, params, groups, gkc, finish_multistage)
}

#[allow(clippy::too_many_arguments)]
fn batched_agg_last_stage(
    graph: &Graph,
    agg: &AggPlan,
    params: &BTreeMap<String, Value>,
    driving: &DataChunk,
    hops: &[Hop],
    wheres: &[WherePred],
    hop_members: &[Option<crate::MembersView>],
    hop_tokens: &[Option<Vec<u32>>],
) -> Result<Option<QueryResult>, RunError> {
    let mut all_groups: Vec<Group> = Vec::new();
    let mut seen: BTreeSet<Vec<u64>> = BTreeSet::new();
    let mut gkc_all: GroupKeyCols = GroupKeyCols::new();
    let n = driving.live();
    let mut start = 0;
    let mut batched = false;
    while start < n {
        let end = (start + MULTISTAGE_TOPK_BATCH).min(n);
        batched |= start > 0 || end < n;
        let bc = driving.driving_slice(start, end);
        let Some(bc) =
            expand_last_stage_batch(graph, params, bc, hops, wheres, hop_members, hop_tokens)?
        else {
            return Ok(None);
        };
        if bc.live() == 0 {
            start = end;
            continue;
        }
        let (groups, gkc) = match reduce_agg_groups(graph, agg, params, &bc)? {
            Some(g) => g,
            None => return Ok(None), // a column-budget / type decline
        };
        for g in groups {
            if !seen.insert(g.0.clone()) {
                return Ok(None); // a group spans batches — the general path merges it
            }
            all_groups.push(g);
        }
        merge_group_key_cols(&mut gkc_all, gkc);
        start = end;
    }
    if batched {
        counted!("interp.pipeline agg batched");
    }
    finalize_agg_groups(graph, agg, params, all_groups, gkc_all, finish_multistage)
}

/// Fold one batch's group-key output columns into the running set: per grouping
/// var, UNION the distinct ids (dedup — an id carries the same node, so its prop
/// values are equal) and merge the prop value columns, keeping the id vector
/// sorted so the output projection's binary search stays valid.
fn merge_group_key_cols(dst: &mut GroupKeyCols, src: GroupKeyCols) {
    for (vi, (ids, props)) in src {
        let entry = dst
            .entry(vi)
            .or_insert_with(|| (Vec::new(), BTreeMap::new()));
        // Rebuild as id -> (per-prop value) so union + realignment is trivial.
        let mut merged: BTreeMap<u64, BTreeMap<String, Value>> = BTreeMap::new();
        let mut absorb = |ids: &[u64], props: &BTreeMap<String, Vec<Value>>| {
            for (row, &id) in ids.iter().enumerate() {
                let slot = merged.entry(id).or_default();
                for (p, vals) in props {
                    if let Some(v) = vals.get(row) {
                        slot.entry(p.clone()).or_insert_with(|| v.clone());
                    }
                }
            }
        };
        absorb(&entry.0, &entry.1);
        absorb(&ids, &props);
        let new_ids: Vec<u64> = merged.keys().copied().collect();
        let mut new_props: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for id in &new_ids {
            for (p, v) in &merged[id] {
                new_props.entry(p.clone()).or_default().push(v.clone());
            }
        }
        *entry = (new_ids, new_props);
    }
}

/// Substitute a pure-projection WITH's `alias → definition` bindings into a later
/// expression: every `Var(alias)` present in `map` becomes its definition,
/// recursively. A `Var` not in `map` is a carried pattern var and is left as-is.
/// Complex binding-scope / graph-dependent variants (`ListComp`, `Reduce`,
/// `HasLabels`, pattern/list predicates, …) are cloned UNCHANGED — if a mapped
/// alias survives inside one the downstream aggregate simply DECLINES (the alias
/// is not a carry var), which is a safe over-decline, never a mis-answer. The
/// arithmetic / logical / property / call / CASE / list forms the fused
/// projections actually use are substituted fully.
fn substitute_aliases(e: &Expr, map: &BTreeMap<String, Expr>) -> Expr {
    let sub = |x: &Expr| substitute_aliases(x, map);
    let boxed = |x: &Expr| Box::new(substitute_aliases(x, map));
    match e {
        Expr::Var(v) => map.get(v).cloned().unwrap_or_else(|| e.clone()),
        Expr::Prop(b, k) => Expr::Prop(boxed(b), k.clone()),
        Expr::Index(b, i) => Expr::Index(boxed(b), boxed(i)),
        Expr::Slice { of, from, to } => Expr::Slice {
            of: boxed(of),
            from: from.as_ref().map(|x| boxed(x)),
            to: to.as_ref().map(|x| boxed(x)),
        },
        Expr::Bin(op, l, r) => Expr::Bin(*op, boxed(l), boxed(r)),
        Expr::And(l, r) => Expr::And(boxed(l), boxed(r)),
        Expr::Or(l, r) => Expr::Or(boxed(l), boxed(r)),
        Expr::Xor(l, r) => Expr::Xor(boxed(l), boxed(r)),
        Expr::Not(x) => Expr::Not(boxed(x)),
        Expr::Neg(x) => Expr::Neg(boxed(x)),
        Expr::IsNull { of, negated } => Expr::IsNull {
            of: boxed(of),
            negated: *negated,
        },
        Expr::In(l, r) => Expr::In(boxed(l), boxed(r)),
        Expr::List(xs) => Expr::List(xs.iter().map(&sub).collect()),
        Expr::Map(kvs) => Expr::Map(kvs.iter().map(|(k, v)| (k.clone(), sub(v))).collect()),
        Expr::Call {
            name,
            distinct,
            args,
            star,
        } => Expr::Call {
            name: name.clone(),
            distinct: *distinct,
            args: args.iter().map(&sub).collect(),
            star: *star,
        },
        Expr::Case {
            subject,
            arms,
            otherwise,
        } => Expr::Case {
            subject: subject.as_ref().map(|s| boxed(s)),
            arms: arms.iter().map(|(w, t)| (sub(w), sub(t))).collect(),
            otherwise: otherwise.as_ref().map(|o| boxed(o)),
        },
        // Binding-scope / graph-dependent variants: clone unchanged (see doc).
        _ => e.clone(),
    }
}

/// Recognise `MATCH <chain> WITH DISTINCT <carry> (WITH <pure projection>)* WITH
/// <group-by aggregate> [WHERE <having>] RETURN <items> [ORDER BY/SKIP/LIMIT]` —
/// a DISTINCT relational stage feeding a fused projection + aggregate, with NO
/// further graph traversal after the WITH (IC4). The clause shape
/// (`[Match, With, With, (With,)* Return]` — ≥2 WITHs, the LAST aggregating, NO
/// MATCH after the first) is DISJOINT from `recognise_aggregate` (exactly one
/// WITH, or none) and `recognise_multistage` (a MATCH in slot 3).
///
/// The middle projection WITHs are 1:1 row transforms, so they FUSE into the
/// aggregate by alias substitution — `WITH tag, CASE… AS valid … WITH tag,
/// sum(valid) …` becomes `sum(CASE…)` over the carried `{tag, post}` chunk. The
/// leading DISTINCT stage is NOT fused (it dedups (tag, post) pairs, a barrier a
/// group-by cannot absorb) — it becomes the WITH-boundary carry. Reuses
/// `run_multistage` with EMPTY stage-2 hops: stage-1 chunk → project_carried
/// (DISTINCT) → aggregate tail. Byte-identical to the interp, or `None` (any
/// sub-recognizer declines → the general path answers identically).
fn recognise_projected_aggregate(sq: &SingleQuery) -> Option<MultiStagePlan> {
    let clauses = &sq.clauses;
    if clauses.len() < 4 {
        return None; // Match + ≥2 With + Return
    }
    let Clause::Match {
        optional: false,
        pattern: p1,
        where_: w1,
    } = clauses.first()?
    else {
        return None;
    };
    let Clause::Return { proj: rp } = clauses.last()? else {
        return None;
    };
    // Every middle clause a WITH; need ≥2 (else Form-A aggregate owns one WITH).
    let middle = &clauses[1..clauses.len() - 1];
    if middle.len() < 2 {
        return None;
    }
    let mut withs: Vec<(&Projection, Option<&Expr>)> = Vec::with_capacity(middle.len());
    for c in middle {
        let Clause::With { proj, where_ } = c else {
            return None; // a MATCH / UNWIND in the middle — not this shape
        };
        withs.push((proj, where_.as_ref()));
    }
    let (carry_proj, carry_where) = withs[0];
    let (agg_proj, having) = withs[withs.len() - 1];
    let proj_stages = &withs[1..withs.len() - 1];

    // The carry WITH: no post-WHERE, no `*`, no ORDER/SKIP/LIMIT, ≥1 item, each a
    // BARE bound pattern var (or `v AS v`) — exactly `recognise_multistage`'s
    // carry. DISTINCT is the load-bearing modifier (the dedup barrier).
    if carry_where.is_some()
        || carry_proj.star
        || !carry_proj.order.is_empty()
        || carry_proj.skip.is_some()
        || carry_proj.limit.is_some()
        || carry_proj.items.is_empty()
    {
        return None;
    }

    // Stage 1: the read chain, with the inline `(person:Person {id: val})` start
    // anchor opt-in (IC4's seed) — the SAME construction `recognise_multistage`
    // stage 1 uses.
    let hc1 = collect_hops(p1, None, false, true, false)?;
    if !varlen_distinct_consumed(&hc1.hops, carry_proj) {
        return None;
    }
    let anchor_eq: Option<Expr> = hc1.start_anchor.as_ref().map(|(prop, val)| {
        Expr::Bin(
            engram_cypher::ast::BinOp::Eq,
            Box::new(Expr::Prop(
                Box::new(Expr::Var(hc1.a_var.clone())),
                prop.clone(),
            )),
            Box::new(val.clone()),
        )
    });
    let combined_where: Option<Expr> = match (anchor_eq, w1.clone()) {
        (Some(a), Some(w)) => Some(Expr::And(Box::new(a), Box::new(w))),
        (Some(a), None) => Some(a),
        (None, w) => w,
    };
    let s1_wheres = recognise_where_preds(combined_where.as_ref(), &hc1.vars)?;
    let s1_anchor = prop_eq_index(combined_where.as_ref(), &hc1.a_var)
        .map(|(prop, values)| PropAnchor { prop, values });

    // The carry = bare bound vars of stage 1, in item order (mirror
    // `recognise_multistage`).
    let mut carried_names: Vec<String> = Vec::with_capacity(carry_proj.items.len());
    for it in &carry_proj.items {
        if expr_has_aggregate(&it.expr) {
            return None;
        }
        let Expr::Var(v) = &it.expr else {
            return None;
        };
        if let Some(a) = &it.alias {
            if a != v {
                return None;
            }
        }
        if !hc1.vars.contains(v) || carried_names.contains(v) {
            return None;
        }
        carried_names.push(v.clone());
    }
    let carried: Vec<usize> = carried_names
        .iter()
        .map(|v| hc1.vars.iter().position(|x| x == v).expect("carried bound"))
        .collect();
    let carried_kinds: Vec<VarKind> = carried.iter().map(|&i| hc1.var_kinds[i]).collect();

    // FUSE the pure-projection WITHs into the aggregate. Each stage's items are
    // rewritten through the accumulated map (so a projection reading an earlier
    // projection's alias resolves), then added to the map. A projection stage must
    // be a pure 1:1 transform — no aggregate, no DISTINCT, no ORDER/SKIP/LIMIT, no
    // post-WHERE — else it is not fusible and we decline.
    let mut map: BTreeMap<String, Expr> = BTreeMap::new();
    for (proj, pwhere) in proj_stages {
        if pwhere.is_some()
            || proj.distinct
            || proj.star
            || !proj.order.is_empty()
            || proj.skip.is_some()
            || proj.limit.is_some()
            || proj.items.is_empty()
        {
            return None;
        }
        for (i, it) in proj.items.iter().enumerate() {
            if expr_has_aggregate(&it.expr) {
                return None; // an aggregate in a MIDDLE stage — not this shape
            }
            let def = substitute_aliases(&it.expr, &map);
            let alias = it
                .alias
                .clone()
                .or_else(|| it.text.clone())
                .unwrap_or_else(|| column_name(&it.expr, i));
            map.insert(alias, def);
        }
    }
    // Rewrite the aggregate WITH's item exprs through the map (its own output
    // aliases — which the HAVING and RETURN read — are unaffected, being defined
    // HERE, not by the fused projections).
    let mut fused_items: Vec<ProjItem> = Vec::with_capacity(agg_proj.items.len());
    for it in &agg_proj.items {
        fused_items.push(ProjItem {
            expr: substitute_aliases(&it.expr, &map),
            alias: it.alias.clone(),
            text: it.text.clone(),
        });
    }
    let fused_agg = Projection {
        distinct: agg_proj.distinct,
        star: agg_proj.star,
        items: fused_items,
        order: agg_proj.order.clone(),
        skip: agg_proj.skip.clone(),
        limit: agg_proj.limit.clone(),
    };

    // The post-DISTINCT aggregate runs over the CARRY vars as a no-hop chain (the
    // chunk is pre-built + deduped by `run_multistage`; the scan fields are inert
    // for the over-chunk aggregate runner).
    let chain2 = Chain {
        a_labels: Vec::new(),
        a_var: carried_names[0].clone(),
        hops: Vec::new(),
        vars: carried_names.clone(),
        var_kinds: carried_kinds,
        wheres: Vec::new(),
        start_anchor: None,
    };
    // The aggregate must actually aggregate (else the DISTINCT projection, not
    // this, owns the shape) — `aggregate_over_chain` enforces it.
    let tail = MultiStageTail::Agg(Box::new(aggregate_over_chain(
        chain2,
        &fused_agg,
        Some((having, rp)),
    )?));

    Some(MultiStagePlan {
        s1_a_labels: hc1.a_labels,
        s1_a_var: hc1.a_var,
        s1_hops: hc1.hops,
        s1_wheres,
        s1_anchor,
        carried,
        distinct: carry_proj.distinct,
        s2_hops: Vec::new(),
        s2_wheres: Vec::new(),
        tail,
    })
}

// ─── Query normalisation pre-pass (var-rename; scalar prelude) ────────────────
//
// A source-to-source rewrite tried BEFORE the recognizers: it turns a shape the
// recognizers decline into an EQUIVALENT one they accept, then the caller
// re-plans the rewritten query. Every rewrite preserves results exactly (it is a
// renaming or a single-row constant fold), so the columnar answer stays
// byte-identical to the interp on the ORIGINAL query; a rewrite that cannot be
// proven safe simply is not applied (the interp then answers).

/// Does `name` appear as ANY identifier in these read clauses — a Var reference,
/// a property base, a pattern node/rel var (binding OR reference), an UNWIND
/// var? An OVER-approximation (pattern bindings count as mentions): used to prove
/// a rewrite is collision-free, so a false "yes" only declines the rewrite.
fn clauses_mention_var(clauses: &[Clause], name: &str) -> bool {
    clauses.iter().any(|c| clause_mentions_var(c, name))
}

fn expr_mentions_var(e: &Expr, name: &str) -> bool {
    let mut fv = Vec::new();
    free_vars_of(e, &mut fv);
    fv.iter().any(|v| v == name)
}

fn opt_expr_mentions_var(o: &Option<Expr>, name: &str) -> bool {
    o.as_ref().is_some_and(|e| expr_mentions_var(e, name))
}

fn proj_mentions_var(p: &Projection, name: &str) -> bool {
    p.items.iter().any(|it| expr_mentions_var(&it.expr, name))
        || p.order.iter().any(|o| expr_mentions_var(&o.expr, name))
        || opt_expr_mentions_var(&p.skip, name)
        || opt_expr_mentions_var(&p.limit, name)
}

fn node_mentions_var(n: &NodePattern, name: &str) -> bool {
    n.var.as_deref() == Some(name) || opt_expr_mentions_var(&n.props, name)
}

fn rel_mentions_var(r: &RelPattern, name: &str) -> bool {
    r.var.as_deref() == Some(name) || opt_expr_mentions_var(&r.props, name)
}

fn pattern_mentions_var(p: &Pattern, name: &str) -> bool {
    p.paths.iter().any(|path| {
        path.var.as_deref() == Some(name)
            || node_mentions_var(&path.start, name)
            || path
                .hops
                .iter()
                .any(|(r, n)| rel_mentions_var(r, name) || node_mentions_var(n, name))
    })
}

fn clause_mentions_var(c: &Clause, name: &str) -> bool {
    match c {
        Clause::Match {
            pattern, where_, ..
        } => pattern_mentions_var(pattern, name) || opt_expr_mentions_var(where_, name),
        Clause::With { proj, where_ } => {
            proj_mentions_var(proj, name) || opt_expr_mentions_var(where_, name)
        }
        Clause::Unwind { expr, alias } => alias == name || expr_mentions_var(expr, name),
        Clause::Return { proj } => proj_mentions_var(proj, name),
        // A non-read clause is out of the rewrite's scope: assume it might
        // reference `name` (conservative — declines the rewrite).
        _ => true,
    }
}

/// Rename the bound variable `from` → `to` throughout read clauses — pattern
/// node/rel/path vars, property-map exprs, WHERE, projections (items, aliases,
/// ORDER BY, SKIP/LIMIT) and UNWIND. `None` if a non-read clause is present
/// (out of scope). Expr-level renames reuse `substitute_aliases` with the single
/// binding `from → Var(to)`.
fn rename_var_clauses(clauses: &[Clause], from: &str, to: &str) -> Option<Vec<Clause>> {
    let map: BTreeMap<String, Expr> =
        BTreeMap::from([(from.to_string(), Expr::Var(to.to_string()))]);
    clauses
        .iter()
        .map(|c| rename_var_clause(c, from, to, &map))
        .collect()
}

fn rename_ident(s: &str, from: &str, to: &str) -> String {
    if s == from {
        to.to_string()
    } else {
        s.to_string()
    }
}

fn rename_var_node(
    n: &NodePattern,
    from: &str,
    to: &str,
    map: &BTreeMap<String, Expr>,
) -> NodePattern {
    NodePattern {
        var: n.var.as_ref().map(|v| rename_ident(v, from, to)),
        labels: n.labels.clone(),
        props: n.props.as_ref().map(|e| substitute_aliases(e, map)),
    }
}

fn rename_var_rel(
    r: &RelPattern,
    from: &str,
    to: &str,
    map: &BTreeMap<String, Expr>,
) -> RelPattern {
    RelPattern {
        var: r.var.as_ref().map(|v| rename_ident(v, from, to)),
        types: r.types.clone(),
        dir: r.dir,
        props: r.props.as_ref().map(|e| substitute_aliases(e, map)),
        length: r.length,
    }
}

fn rename_var_pattern(p: &Pattern, from: &str, to: &str, map: &BTreeMap<String, Expr>) -> Pattern {
    Pattern {
        paths: p
            .paths
            .iter()
            .map(|path| PathPattern {
                var: path.var.as_ref().map(|v| rename_ident(v, from, to)),
                shortest: path.shortest,
                start: rename_var_node(&path.start, from, to, map),
                hops: path
                    .hops
                    .iter()
                    .map(|(r, n)| {
                        (
                            rename_var_rel(r, from, to, map),
                            rename_var_node(n, from, to, map),
                        )
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn rename_var_proj(
    p: &Projection,
    from: &str,
    to: &str,
    map: &BTreeMap<String, Expr>,
) -> Projection {
    Projection {
        distinct: p.distinct,
        star: p.star,
        items: p
            .items
            .iter()
            .map(|it| ProjItem {
                expr: substitute_aliases(&it.expr, map),
                alias: it.alias.as_ref().map(|a| rename_ident(a, from, to)),
                text: it.text.clone(),
            })
            .collect(),
        order: p
            .order
            .iter()
            .map(|o| engram_cypher::stmt::OrderItem {
                expr: substitute_aliases(&o.expr, map),
                desc: o.desc,
            })
            .collect(),
        skip: p.skip.as_ref().map(|e| substitute_aliases(e, map)),
        limit: p.limit.as_ref().map(|e| substitute_aliases(e, map)),
    }
}

fn rename_var_clause(
    c: &Clause,
    from: &str,
    to: &str,
    map: &BTreeMap<String, Expr>,
) -> Option<Clause> {
    Some(match c {
        Clause::Match {
            optional,
            pattern,
            where_,
        } => Clause::Match {
            optional: *optional,
            pattern: rename_var_pattern(pattern, from, to, map),
            where_: where_.as_ref().map(|w| substitute_aliases(w, map)),
        },
        Clause::With { proj, where_ } => Clause::With {
            proj: rename_var_proj(proj, from, to, map),
            where_: where_.as_ref().map(|w| substitute_aliases(w, map)),
        },
        Clause::Unwind { expr, alias } => Clause::Unwind {
            expr: substitute_aliases(expr, map),
            alias: rename_ident(alias, from, to),
        },
        Clause::Return { proj } => Clause::Return {
            proj: rename_var_proj(proj, from, to, map),
        },
        _ => return None,
    })
}

/// Normalise a RENAMED collect-unwind — `WITH collect(DISTINCT x) AS xs UNWIND
/// xs AS f` with `f != x` — into the SAME-name form `collect_distinct_unwind_var`
/// accepts, by renaming `f → x` in every clause AFTER the UNWIND (IC6's `UNWIND
/// friends AS f`). Safe ONLY when `x` is otherwise unbound after the UNWIND (no
/// name collision) — else declined. Returns the rewritten query, or `None` if no
/// such pair is present / the rename is unsafe.
fn normalise_renamed_unwind(q: &SingleQuery) -> Option<SingleQuery> {
    let cl = &q.clauses;
    for i in 0..cl.len().saturating_sub(1) {
        // A `WITH collect(DISTINCT x) AS xs` (no post-WITH WHERE) directly
        // followed by `UNWIND <xs> AS f`.
        let Clause::With { proj, where_: None } = &cl[i] else {
            continue;
        };
        let Clause::Unwind { expr, alias: f } = &cl[i + 1] else {
            continue;
        };
        if proj.items.len() != 1 || proj.distinct || proj.star {
            continue;
        }
        let item = &proj.items[0];
        let Some(xs) = item.alias.as_deref() else {
            continue;
        };
        let Expr::Call {
            name,
            distinct: true,
            args,
            star: false,
        } = &item.expr
        else {
            continue;
        };
        if name != "collect" || args.len() != 1 {
            continue;
        }
        let Expr::Var(x) = &args[0] else { continue };
        let Expr::Var(uv) = expr else { continue };
        if uv != xs || f == x {
            continue; // not this xs, or already same-name (no rename needed)
        }
        // Rename `f → x` in the clauses AFTER the UNWIND — but only if `x` is not
        // already mentioned there (a collision would change meaning).
        let tail = &cl[i + 2..];
        if clauses_mention_var(tail, x) {
            return None;
        }
        let renamed_tail = rename_var_clauses(tail, f, x)?;
        let mut clauses: Vec<Clause> = cl[..i + 1].to_vec();
        // The UNWIND now binds `x` (same name as the collected var).
        clauses.push(Clause::Unwind {
            expr: expr.clone(),
            alias: x.clone(),
        });
        clauses.extend(renamed_tail);
        return Some(SingleQuery { clauses });
    }
    None
}

/// A scalar `Value` as a literal `Expr`, for substituting a single-row prelude
/// binding into the rest of the query. Only SCALARS fold to a literal — a node,
/// relationship, list, map or temporal has no literal form here, so its prelude
/// DECLINES (the interp answers the original).
fn value_to_literal_expr(v: &Value) -> Option<Expr> {
    match v {
        Value::Null => Some(Expr::Null),
        Value::Bool(b) => Some(Expr::Bool(*b)),
        Value::Int(i) => Some(Expr::Int(*i)),
        Value::Float(f) => Some(Expr::Float(*f)),
        Value::Str(s) => Some(Expr::Str(s.clone())),
        _ => None,
    }
}

/// A constant expression reads no bound variable.
fn expr_is_const(e: &Expr) -> bool {
    let mut fv = Vec::new();
    free_vars_of(e, &mut fv);
    fv.is_empty()
}

fn subst_node_props(n: &NodePattern, map: &BTreeMap<String, Expr>) -> NodePattern {
    NodePattern {
        var: n.var.clone(),
        labels: n.labels.clone(),
        props: n.props.as_ref().map(|e| substitute_aliases(e, map)),
    }
}

fn subst_pattern_exprs(p: &Pattern, map: &BTreeMap<String, Expr>) -> Pattern {
    Pattern {
        paths: p
            .paths
            .iter()
            .map(|path| PathPattern {
                var: path.var.clone(),
                shortest: path.shortest,
                start: subst_node_props(&path.start, map),
                hops: path
                    .hops
                    .iter()
                    .map(|(r, n)| {
                        (
                            RelPattern {
                                var: r.var.clone(),
                                types: r.types.clone(),
                                dir: r.dir,
                                props: r.props.as_ref().map(|e| substitute_aliases(e, map)),
                                length: r.length,
                            },
                            subst_node_props(n, map),
                        )
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn subst_proj_exprs(p: &Projection, map: &BTreeMap<String, Expr>) -> Projection {
    Projection {
        distinct: p.distinct,
        star: p.star,
        items: p
            .items
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let new_expr = substitute_aliases(&it.expr, map);
                // When substitution changed the expr (it read a prelude var), pin
                // the ORIGINAL column identity as an explicit alias, so the dead-
                // const cleanup can find the (now unreferenced) carried column.
                let alias = if new_expr != it.expr {
                    Some(
                        it.alias
                            .clone()
                            .or_else(|| it.text.clone())
                            .unwrap_or_else(|| column_name(&it.expr, i)),
                    )
                } else {
                    it.alias.clone()
                };
                ProjItem {
                    expr: new_expr,
                    alias,
                    text: it.text.clone(),
                }
            })
            .collect(),
        order: p
            .order
            .iter()
            .map(|o| engram_cypher::stmt::OrderItem {
                expr: substitute_aliases(&o.expr, map),
                desc: o.desc,
            })
            .collect(),
        skip: p.skip.as_ref().map(|e| substitute_aliases(e, map)),
        limit: p.limit.as_ref().map(|e| substitute_aliases(e, map)),
    }
}

fn subst_clause_exprs(c: &Clause, map: &BTreeMap<String, Expr>) -> Clause {
    match c {
        Clause::Match {
            optional,
            pattern,
            where_,
        } => Clause::Match {
            optional: *optional,
            pattern: subst_pattern_exprs(pattern, map),
            where_: where_.as_ref().map(|w| substitute_aliases(w, map)),
        },
        Clause::With { proj, where_ } => Clause::With {
            proj: subst_proj_exprs(proj, map),
            where_: where_.as_ref().map(|w| substitute_aliases(w, map)),
        },
        Clause::Unwind { expr, alias } => Clause::Unwind {
            expr: substitute_aliases(expr, map),
            alias: alias.clone(),
        },
        Clause::Return { proj } => Clause::Return {
            proj: subst_proj_exprs(proj, map),
        },
        other => other.clone(),
    }
}

/// Drop WITH items that a prelude substitution left CONSTANT and DEAD — the
/// carried prelude scalar (now a literal) that nothing after the WITH, nor the
/// WITH's own HAVING, references. Enables the collect-unwind normalisation for
/// `WITH <lit> AS k, collect(DISTINCT x) AS xs` (IC6's `WITH knownTagId,
/// collect(…)`). Grouping by a dropped CONSTANT key is one group either way, so
/// the aggregate is unchanged; a DISTINCT over a constant column is unchanged;
/// dropping an unreferenced projected column changes no result. Never empties a
/// WITH (keeps it when every item is dead).
fn drop_dead_const_with_items(clauses: &mut [Clause]) {
    for i in 0..clauses.len() {
        let (head, tail) = clauses.split_at_mut(i + 1);
        let Clause::With { proj, where_ } = &mut head[i] else {
            continue;
        };
        if proj.items.len() < 2 {
            continue;
        }
        let having = where_.clone();
        let kept: Vec<ProjItem> = proj
            .items
            .iter()
            .enumerate()
            .filter(|(j, it)| {
                let alias = it
                    .alias
                    .clone()
                    .or_else(|| it.text.clone())
                    .unwrap_or_else(|| column_name(&it.expr, *j));
                let dead = expr_is_const(&it.expr)
                    && !clauses_mention_var(tail, &alias)
                    && !having
                        .as_ref()
                        .is_some_and(|w| expr_mentions_var(w, &alias));
                !dead
            })
            .map(|(_, it)| it.clone())
            .collect();
        if !kept.is_empty() && kept.len() < proj.items.len() {
            proj.items = kept;
        }
    }
}

/// Normalise a SCALAR PRELUDE — a leading `MATCH <p0> WITH <scalar projections>`
/// that binds constants for the rest of the query (IC6's `MATCH (knownTag:Tag
/// {name:'Music'}) WITH knownTag.id AS knownTagId`). It is EVALUATED once; if it
/// yields EXACTLY ONE row of scalar values, those bindings are substituted as
/// literals into the remaining clauses (including inline `{prop: knownTagId}`
/// anchors), and the now-dead constant carries are dropped — turning a
/// cross-`MATCH` scalar thread into an ordinary anchored query the recognizers
/// accept. DECLINES (interp answers the original, byte-identically) when: the
/// prelude WITH aggregates / stars / orders / pages / carries a post-WHERE; the
/// rest holds a non-read clause; the prelude yields 0 rows (empty) or >1 (a
/// cartesian); or any bound value is not a scalar (a node/list can't be a
/// literal).
/// The index of the first clause where `var` is the START node of a pattern path
/// (a traversal seed) — where a prelude node used for traversal is re-introduced.
fn var_first_pattern_start(clauses: &[Clause], var: &str) -> Option<usize> {
    clauses.iter().position(|c| {
        matches!(c, Clause::Match { pattern, .. }
            if pattern.paths.iter().any(|p| p.start.var.as_deref() == Some(var)))
    })
}

/// The prelude's `NodePattern` for `var` (its labels + props) — the anchor for a
/// re-introduced seed.
fn prelude_node_pattern(p0: &Pattern, var: &str) -> Option<NodePattern> {
    for path in &p0.paths {
        if path.start.var.as_deref() == Some(var) {
            return Some(path.start.clone());
        }
        for (_, n) in &path.hops {
            if n.var.as_deref() == Some(var) {
                return Some(n.clone());
            }
        }
    }
    None
}

/// SEED-FILTER a prelude node used as a traversal start: label the seed scan with
/// the prelude's labels and AND `var = $pname` (node identity) into the clause's
/// WHERE, so the scan filters to the EXACT prelude node (no anchor-uniqueness
/// assumption — it is the specific node id) before the traversal expands. The var
/// stays a bound scan variable (a param cannot be a pattern variable).
fn apply_seed_filter(
    clauses: &mut [Clause],
    idx: usize,
    var: &str,
    labels: &[String],
    pname: &str,
) {
    if let Clause::Match {
        pattern, where_, ..
    } = &mut clauses[idx]
    {
        for path in &mut pattern.paths {
            if path.start.var.as_deref() == Some(var) {
                path.start.labels = labels.to_vec();
            }
        }
        let id_eq = Expr::Bin(
            engram_cypher::ast::BinOp::Eq,
            Box::new(Expr::Var(var.to_string())),
            Box::new(Expr::Param(pname.to_string())),
        );
        *where_ = Some(match where_.take() {
            Some(w) => Expr::And(Box::new(id_eq), Box::new(w)),
            None => id_eq,
        });
    }
}

/// Remove any WITH item that DEFINES `var` from clauses BEFORE `idx` — a carry of
/// a prelude constant that is re-introduced (seed-filtered) at `idx`. Safe: the
/// dropped item is a constant (a prelude binding), so a group/DISTINCT key it fed
/// leaves one group / unchanged distinctness.
fn drop_var_from_carries_before(clauses: &mut [Clause], idx: usize, var: &str) {
    for c in clauses[..idx].iter_mut() {
        if let Clause::With { proj, .. } = c {
            proj.items.retain(|it| {
                let defines = it.alias.as_deref() == Some(var)
                    || (it.alias.is_none() && matches!(&it.expr, Expr::Var(v) if v == var));
                !defines
            });
        }
    }
}

#[allow(clippy::type_complexity)]
/// `Expr::Bin(Eq, Prop(Var(v), key), val)` in either operand order → `(v, key, val)`.
fn prop_eq_of(e: &Expr) -> Option<(&str, &str, &Expr)> {
    use engram_cypher::ast::BinOp;
    let Expr::Bin(BinOp::Eq, l, r) = e else {
        return None;
    };
    if let Some((v, k)) = prop_of_var(l) {
        return Some((v, k, r));
    }
    if let Some((v, k)) = prop_of_var(r) {
        return Some((v, k, l));
    }
    None
}

/// IC12's stage-1 collect-list prelude, served by anchoring on the WHERE value.
///
/// Recognises `MATCH (a:LA)-[:types*0..]->(b:LB) WHERE a.prop=V OR b.prop=V
/// RETURN collect(a.key)` and computes the collected list via
/// [`Graph::collect_anchored_hierarchy`] — anchor on `V`, walk the `b`-hierarchy
/// DOWN — instead of scanning every `a` up. Returns the same one-row `List`
/// result `run_query` would (membership-identical; the caller uses it only as an
/// `IN` set). `None` for any other shape → the general prelude evaluation runs.
fn try_anchored_hierarchy_prelude(
    graph: &Graph,
    p0: &Pattern,
    w0: Option<&Expr>,
    items: &[ProjItem],
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    // Single forward `*0..` hop: (a:LA)-[:types*0..]->(b:LB), no rel var/props.
    if p0.paths.len() != 1 {
        return Ok(None);
    }
    let path = &p0.paths[0];
    if path.shortest || path.hops.len() != 1 {
        return Ok(None);
    }
    let anode = &path.start;
    let (rel, bnode) = &path.hops[0];
    if rel.dir != RelDir::Out || rel.var.is_some() || rel.props.is_some() || rel.types.is_empty() {
        return Ok(None);
    }
    match rel.length {
        Some(vl) if vl.min == Some(0) => {} // `*0..`
        _ => return Ok(None),
    }
    if anode.props.is_some() || bnode.props.is_some() {
        return Ok(None);
    }
    let (Some(a_var), [a_label]) = (anode.var.as_deref(), anode.labels.as_slice()) else {
        return Ok(None);
    };
    let (Some(b_var), [b_label]) = (bnode.var.as_deref(), bnode.labels.as_slice()) else {
        return Ok(None);
    };
    // WHERE = a.prop=V OR b.prop=V (same prop, same value on both sides).
    let Some(Expr::Or(l, r)) = w0 else {
        return Ok(None);
    };
    let (Some((lv, lk, lval)), Some((rv, rk, rval))) = (prop_eq_of(l), prop_eq_of(r)) else {
        return Ok(None);
    };
    if lk != rk {
        return Ok(None);
    }
    let (a_val, b_val) = if lv == a_var && rv == b_var {
        (lval, rval)
    } else if lv == b_var && rv == a_var {
        (rval, lval)
    } else {
        return Ok(None);
    };
    let empty_vm = VarMap::new();
    let scope = Scope::over(params, &empty_vm, graph.wall_ms(), graph.zone_provider());
    let av = eval_with(a_val, &scope, None).map_err(RunError::Eval)?;
    let bv = eval_with(b_val, &scope, None).map_err(RunError::Eval)?;
    if av != bv {
        return Ok(None); // the OR compares against two different values
    }
    // RETURN a single `collect(a.key)` (DISTINCT or not — the downstream `IN`
    // membership makes the two forms equivalent, and the method dedups by node).
    if items.len() != 1 {
        return Ok(None);
    }
    let Expr::Call {
        name, args, star, ..
    } = &items[0].expr
    else {
        return Ok(None);
    };
    if name != "collect" || *star || args.len() != 1 {
        return Ok(None);
    }
    let Some((cv, collect_prop)) = prop_of_var(&args[0]) else {
        return Ok(None);
    };
    if cv != a_var {
        return Ok(None);
    }

    let edge_tokens = graph.type_tokens_peek(&rel.types);
    let Some(values) = graph
        .collect_anchored_hierarchy(a_label, b_label, &edge_tokens, lk, &av, collect_prop)
        .map_err(RunError::Graph)?
    else {
        return Ok(None); // name property never minted → general path
    };
    let alias = items[0]
        .alias
        .clone()
        .or_else(|| items[0].text.clone())
        .unwrap_or_else(|| column_name(&items[0].expr, 0));
    counted!("interp.pipeline anchored hierarchy collect served a prelude");
    Ok(Some(QueryResult {
        columns: vec![alias],
        rows: vec![vec![Value::List(values)]],
    }))
}

/// A prelude rewrite: the re-planned query plus the params it injected.
type PreludeRewrite = (SingleQuery, BTreeMap<String, Value>);

fn scalar_prelude_rewrite(
    graph: &Graph,
    q: &SingleQuery,
    params: &BTreeMap<String, Value>,
) -> Result<Option<PreludeRewrite>, RunError> {
    let cl = &q.clauses;
    if cl.len() < 3 {
        return Ok(None);
    }
    let Clause::Match {
        optional: false,
        pattern: p0,
        where_: w0,
    } = &cl[0]
    else {
        return Ok(None);
    };
    let Clause::With {
        proj: wp0,
        where_: ww0,
    } = &cl[1]
    else {
        return Ok(None);
    };
    if ww0.is_some()
        || wp0.star
        || wp0.distinct
        || !wp0.order.is_empty()
        || wp0.skip.is_some()
        || wp0.items.is_empty()
    {
        return Ok(None);
    }
    // An aggregate item is allowed ONLY if it is a `collect(…)` — a GLOBAL collect
    // yields ONE row whose LIST is injectable as a param (IC3's `collect(city) AS
    // cities`, consumed by `NOT city IN cities`). A count/sum/etc. scalar aggregate
    // is left to the aggregate path (the row-count guard below rejects a GROUPED
    // collect, which yields many rows).
    if wp0.items.iter().any(|it| {
        expr_has_aggregate(&it.expr)
            && !matches!(&it.expr, Expr::Call { name, .. } if name == "collect")
    }) {
        return Ok(None);
    }
    // A collected alias that is UNWOUND downstream belongs to the collect-unwind
    // normalisation (IC6/IC9's `collect(DISTINCT x) AS xs UNWIND xs AS x`), NOT a
    // list-injecting prelude — do not hijack it. Decline if any collect item's
    // alias appears as an UNWIND source in the rest.
    let collect_aliases: BTreeSet<String> = wp0
        .items
        .iter()
        .enumerate()
        .filter(|(_, it)| matches!(&it.expr, Expr::Call { name, .. } if name == "collect"))
        .map(|(i, it)| {
            it.alias
                .clone()
                .or_else(|| it.text.clone())
                .unwrap_or_else(|| column_name(&it.expr, i))
        })
        .collect();
    if !collect_aliases.is_empty()
        && cl[2..].iter().any(|c| {
            matches!(c, Clause::Unwind { expr: Expr::Var(v), .. } if collect_aliases.contains(v))
        })
    {
        return Ok(None);
    }
    // A BARE-variable item (`WITH person, countryX, countryY`) carries a whole
    // bound value — usable ONLY for node-IDENTITY downstream (injected as a param,
    // below). Accept it ONLY with an explicit LIMIT: the LIMIT is the deliberate
    // "pick one row" signal (IC3's `… LIMIT 1`), and it BOUNDS the probe. Without
    // one, a bare-var prelude (`MATCH (e:WX) WITH e RETURN …`) would run an
    // unbounded scan just to decline — so it is left to the general path.
    let has_bare_var = wp0.items.iter().any(|it| matches!(&it.expr, Expr::Var(_)));
    if has_bare_var && wp0.limit.is_none() {
        return Ok(None);
    }
    // The rest must be only READ clauses (the substitution's scope).
    if cl[2..].iter().any(|c| {
        !matches!(
            c,
            Clause::Match { .. }
                | Clause::With { .. }
                | Clause::Unwind { .. }
                | Clause::Return { .. }
        )
    }) {
        return Ok(None);
    }
    // Evaluate the prelude ONCE: `MATCH p0 [WHERE w0] RETURN <wp0 items, aliased>`.
    let eval_items: Vec<ProjItem> = wp0
        .items
        .iter()
        .enumerate()
        .map(|(i, it)| ProjItem {
            expr: it.expr.clone(),
            alias: Some(
                it.alias
                    .clone()
                    .or_else(|| it.text.clone())
                    .unwrap_or_else(|| column_name(&it.expr, i)),
            ),
            text: it.text.clone(),
        })
        .collect();
    let eval_q = SingleQuery {
        clauses: vec![
            Clause::Match {
                optional: false,
                pattern: p0.clone(),
                where_: w0.clone(),
            },
            Clause::Return {
                proj: Projection {
                    distinct: false,
                    star: false,
                    items: eval_items,
                    order: Vec::new(),
                    skip: None,
                    // Honour the prelude's LIMIT (IC3's `LIMIT 1`) so the probe
                    // picks exactly the row the interp's own LIMIT would.
                    limit: wp0.limit.clone(),
                },
            },
        ],
    };
    // SUPPRESS the probe's trace — it is an internal optimisation step, and its
    // events (a scan, a stream) must not pollute the OUTER query's counters /
    // `sometimes_hit` (which a caller's `streamed()` / counter assertions read).
    // IC12 stage 1: an anchored-hierarchy collect serves the same list far more
    // cheaply than scanning every `a`. Falls back to the general evaluation for
    // any other prelude shape.
    let res = match try_anchored_hierarchy_prelude(graph, p0, w0.as_ref(), &wp0.items, params)? {
        Some(r) => r,
        None => engram_observe::with_suppressed_trace(|| {
            crate::interp::run_query(
                graph,
                &engram_cypher::stmt::Query::Single(eval_q),
                params.clone(),
            )
        })?,
    };
    // Exactly one row folds; 0 rows → empty (let the interp produce it with the
    // right columns); >1 → a cartesian we do not model here.
    if res.rows.len() != 1 {
        return Ok(None);
    }
    // Build the substitution: a SCALAR value folds to a literal `Expr`; a
    // NODE/REL value has no literal form, so it is injected as a PARAM (a
    // NUL-prefixed name that cannot collide with a user param) and the var becomes
    // `$__prelude_<col>` — its downstream uses are node-IDENTITY comparisons
    // (`country = countryX`, `country IN [countryX, countryY]`) the node-identity
    // primitive vectorises. The injected params ride into the re-planned query's
    // scope. A value that is neither (a temporal/list/map) declines.
    let mut map: BTreeMap<String, Expr> = BTreeMap::new();
    let mut ext_params = params.clone();
    // Nodes used as a traversal SEED downstream — kept as bound scan vars,
    // seed-filtered to the exact prelude node (not substituted): (var, labels,
    // param, clause index in the rest).
    let mut seed_filters: Vec<(String, Vec<String>, String, usize)> = Vec::new();
    for (col, val) in res.columns.iter().zip(&res.rows[0]) {
        match value_to_literal_expr(val) {
            Some(lit) => {
                map.insert(col.clone(), lit);
            }
            None => match val {
                Value::Node { .. } | Value::Rel { .. } => {
                    let pname = format!("\0__prelude_{col}");
                    ext_params.insert(pname.clone(), val.clone());
                    // A node used as a pattern START is a traversal seed: a param
                    // cannot be a pattern var, so keep the var bound (a labelled
                    // scan) and filter it to this exact node by identity. Its
                    // prelude anchor must carry a label (else no scan) — else
                    // decline.
                    match var_first_pattern_start(&cl[2..], col) {
                        Some(idx) => {
                            let Some(anchor) = prelude_node_pattern(p0, col) else {
                                return Ok(None);
                            };
                            if anchor.labels.is_empty() {
                                return Ok(None);
                            }
                            seed_filters.push((col.clone(), anchor.labels.clone(), pname, idx));
                        }
                        // Identity-only use — substitute the var with the param.
                        None => {
                            map.insert(col.clone(), Expr::Param(pname));
                        }
                    }
                }
                // A collected LIST — inject as a param; its downstream use is a
                // membership test (`NOT city IN cities` → `NOT city IN $cities`,
                // a const-param list the node-identity `IN` vectorises).
                Value::List(_) => {
                    let pname = format!("\0__prelude_{col}");
                    ext_params.insert(pname.clone(), val.clone());
                    map.insert(col.clone(), Expr::Param(pname));
                }
                _ => return Ok(None), // a temporal/map — no injectable form
            },
        }
    }
    let mut rest: Vec<Clause> = cl[2..]
        .iter()
        .map(|c| subst_clause_exprs(c, &map))
        .collect();
    for (var, labels, pname, idx) in &seed_filters {
        apply_seed_filter(&mut rest, *idx, var, labels, pname);
        drop_var_from_carries_before(&mut rest, *idx, var);
    }
    drop_dead_const_with_items(&mut rest);
    Ok(Some((SingleQuery { clauses: rest }, ext_params)))
}

/// Split a VARLEN-then-FIXED pattern followed by `WITH DISTINCT <b, …>` (IC3's
/// `(person)-[:KNOWS*1..2]-(friend)-[:IS_LOCATED_IN]->(city) … WITH DISTINCT
/// friend`) into two stages joined by an intermediate `WITH DISTINCT b`:
///   `MATCH (a)-[:R*..]-(b) [WHERE s1-preds] WITH DISTINCT b
///    MATCH (b)-<fixed hops>->… [WHERE s2-preds] <original WITH>`
/// The frontier-BFS is sole-hop-only, so a varlen FOLLOWED by fixed hops declines;
/// the split makes stage 1 a lone varlen hop the BFS runs. It is BYTE-IDENTICAL
/// precisely because the output is DISTINCT on `b` (the varlen end): the BFS emits
/// each `b` once, the interp emits it with path-multiplicity, and the intermediate
/// (and final) `DISTINCT b` collapses both identically — the fixed-hop expansion
/// of a `b` does not depend on how many varlen paths reached it. DECLINES (interp
/// answers) unless: one path, hop 0 a `*1..max` rel (no var/props), hops 1.. fixed
/// (no var/props/length), the following WITH is `DISTINCT` carrying `b` as bare
/// vars, and every WHERE conjunct falls cleanly into stage 1 (only `{a,b}`) or
/// stage 2 (only `b` + fixed-hop end vars) — a cross-stage conjunct declines.
fn try_split_varlen_then_fixed(cl: &[Clause], i: usize) -> Option<Vec<Clause>> {
    let Clause::Match {
        optional: false,
        pattern,
        where_,
    } = &cl[i]
    else {
        return None;
    };
    let Clause::With { proj, where_: None } = &cl[i + 1] else {
        return None;
    };
    if !proj.distinct || proj.star || !proj.order.is_empty() || proj.skip.is_some() {
        return None;
    }
    if pattern.paths.len() != 1 {
        return None;
    }
    let path = &pattern.paths[0];
    if path.var.is_some() || path.shortest || path.hops.len() < 2 {
        return None;
    }
    let (rel0, node0) = &path.hops[0];
    let vl = rel0.length?;
    if vl.min.unwrap_or(1) != 1 || vl.max.is_none() || rel0.var.is_some() || rel0.props.is_some() {
        return None;
    }
    let b = node0.var.as_deref()?.to_string();
    if node0.props.is_some() {
        return None;
    }
    if path.hops[1..]
        .iter()
        .any(|(r, _)| r.length.is_some() || r.props.is_some() || r.var.is_some())
    {
        return None;
    }
    // Carried DISTINCT vars — bare vars, one of which is `b`.
    let mut carried: Vec<String> = Vec::new();
    for it in &proj.items {
        let Expr::Var(v) = &it.expr else { return None };
        if let Some(a) = &it.alias {
            if a != v {
                return None;
            }
        }
        carried.push(v.clone());
    }
    if !carried.contains(&b) {
        return None;
    }
    // Stage-1 vars = {a, b}; stage-2 end vars = the fixed hops' end vars.
    let a = path.start.var.clone();
    let mut s1_vars: BTreeSet<String> = BTreeSet::new();
    if let Some(av) = &a {
        s1_vars.insert(av.clone());
    }
    s1_vars.insert(b.clone());
    let s2_end: BTreeSet<String> = path.hops[1..]
        .iter()
        .filter_map(|(_, n)| n.var.clone())
        .collect();
    // Every carried var must be bound by stage 2 (b or a fixed-hop end var).
    if !carried.iter().all(|v| *v == b || s2_end.contains(v)) {
        return None;
    }
    // Distribute the WHERE conjuncts.
    let mut conjs = Vec::new();
    if let Some(w) = where_ {
        conjuncts_of(w, &mut conjs);
    }
    let (mut s1_w, mut s2_w): (Vec<Expr>, Vec<Expr>) = (Vec::new(), Vec::new());
    for c in conjs {
        let mut fv = Vec::new();
        free_vars_of(&c, &mut fv);
        if fv.iter().all(|v| s1_vars.contains(v)) {
            s1_w.push(c);
        } else if fv.iter().all(|v| *v == b || s2_end.contains(v)) {
            s2_w.push(c);
        } else {
            return None; // a cross-stage conjunct — decline
        }
    }
    let and_all = |ws: Vec<Expr>| -> Option<Expr> {
        ws.into_iter()
            .reduce(|acc, e| Expr::And(Box::new(acc), Box::new(e)))
    };
    // Stage 1: (a)-[varlen]-(b).
    let s1_match = Clause::Match {
        optional: false,
        pattern: Pattern {
            paths: vec![PathPattern {
                var: None,
                shortest: false,
                start: path.start.clone(),
                hops: vec![path.hops[0].clone()],
            }],
        },
        where_: and_all(s1_w),
    };
    let interm_with = Clause::With {
        proj: Projection {
            distinct: true,
            star: false,
            items: vec![ProjItem {
                expr: Expr::Var(b.clone()),
                alias: None,
                text: None,
            }],
            order: Vec::new(),
            skip: None,
            limit: None,
        },
        where_: None,
    };
    // Stage 2: (b)-<fixed hops>->…, re-rooted at the carried `b`.
    let s2_match = Clause::Match {
        optional: false,
        pattern: Pattern {
            paths: vec![PathPattern {
                var: None,
                shortest: false,
                start: NodePattern {
                    var: Some(b.clone()),
                    labels: Vec::new(),
                    props: None,
                },
                hops: path.hops[1..].to_vec(),
            }],
        },
        where_: and_all(s2_w),
    };
    let orig_with = cl[i + 1].clone();
    let mut out: Vec<Clause> = cl[..i].to_vec();
    out.push(s1_match);
    out.push(interm_with);
    out.push(s2_match);
    out.push(orig_with);
    out.extend_from_slice(&cl[i + 2..]);
    Some(out)
}

/// Try the varlen-then-fixed split at each position; return the first rewrite.
fn normalise_varlen_then_fixed(q: &SingleQuery) -> Option<SingleQuery> {
    let cl = &q.clauses;
    for i in 0..cl.len().saturating_sub(1) {
        if let Some(clauses) = try_split_varlen_then_fixed(cl, i) {
            return Some(SingleQuery { clauses });
        }
    }
    None
}

/// The columnar normalisation pre-pass: try each source-to-source rewrite; return
/// the first rewritten query, or `None` when none applies. The caller re-plans the
/// result columnar; each rewrite is declined on its own output, so there is no
/// loop (successive rewrites compose across re-plans — a renamed-unwind then a
/// scalar prelude, for IC6).
#[allow(clippy::type_complexity)]
fn normalise_for_columnar(
    graph: &Graph,
    q: &SingleQuery,
    params: &BTreeMap<String, Value>,
) -> Result<Option<(SingleQuery, BTreeMap<String, Value>)>, RunError> {
    // A rename / varlen-split carries the params through unchanged; a prelude may
    // EXTEND them with injected node/rel bindings. Prelude is tried before the
    // varlen split so IC3's `person` is seed-filtered first (its clause 5 becomes a
    // splittable varlen-then-fixed on the re-plan).
    if let Some(r) = normalise_renamed_unwind(q) {
        return Ok(Some((r, params.clone())));
    }
    if let Some(r) = scalar_prelude_rewrite(graph, q, params)? {
        return Ok(Some(r));
    }
    if let Some(r) = normalise_varlen_then_fixed(q) {
        return Ok(Some((r, params.clone())));
    }
    if let Some(r) = normalise_where_first_tail(q) {
        return Ok(Some((r, params.clone())));
    }
    if let Some(r) = normalise_seekable_end_first(graph, q)? {
        return Ok(Some((r, params.clone())));
    }
    Ok(None)
}

/// Fix 38: the statement's FIRST path, written from an unbound, unseekable
/// start toward an end whose inline map is a constant on a DECLARED key,
/// runs REVERSED — seeded by the end's seek, walked toward the start. The
/// recognisers anchor on the start alone: `MATCH (g:GeopoliticalEvent)-
/// [:DERIVES_FROM_STORY]->(s:NewsStory {storyId: $storyId}) RETURN
/// properties(g) … LIMIT 1` scanned all 43,822 events and expanded each
/// (43,822 adjacency probes, 57–65 ms on the mirror against Neo4j's 1.7)
/// for the ONE story the seek names; the same pattern written from the
/// story answers in 2.0. `reversed_path` is the subquery rule (v92)
/// applied at the top level: the reversed pattern binds the same rows.
/// A concluding `LIMIT` without an `ORDER BY` is left as written — the
/// rows are the same multiset either way, but which of them a bare LIMIT
/// keeps is the walk order's, and the mirror's own comparator would read
/// a changed pick as a divergence.
fn normalise_seekable_end_first(
    graph: &Graph,
    q: &SingleQuery,
) -> Result<Option<SingleQuery>, RunError> {
    let Some(Clause::Match {
        pattern,
        where_,
        optional: false,
    }) = q.clauses.first()
    else {
        return Ok(None);
    };
    if pattern.paths.len() != 1 {
        return Ok(None);
    }
    if let Some(Clause::Return { proj }) = q.clauses.last() {
        if proj.limit.is_some() && proj.order.is_empty() {
            return Ok(None);
        }
    }
    let Some(rev) = crate::interp::reversed_path(graph, &pattern.paths[0], &[])? else {
        return Ok(None);
    };
    counted!("interp.top-level path reversed to its seekable end");
    let mut out = q.clone();
    out.clauses[0] = Clause::Match {
        pattern: Pattern { paths: vec![rev] },
        where_: where_.clone(),
        optional: false,
    };
    Ok(Some(out))
}

/// Fix 32: `WITH <items> WHERE <having> ORDER BY … SKIP … LIMIT …` parses as
/// the filtering WITH followed by a `WITH *` carrying the tail (the where-first
/// form Neo4j accepts: filter first, then order and page). A concluding PLAIN
/// RETURN after that pair takes the tail as its own ORDER BY / SKIP / LIMIT —
/// the same rows in the same order, since the RETURN projects the filtered,
/// ordered, paged rows one to one and its ORDER BY sees the WITH's names — and
/// the pair collapses to `WITH … WHERE … RETURN … ORDER BY … LIMIT …`, the
/// Form-A shape the aggregate / OPTIONAL / DISTINCT tails recognise. The
/// story tracker (`… WITH s, count(DISTINCT e) AS shared WHERE shared >= 1
/// ORDER BY shared DESC LIMIT 5 RETURN s.storyId AS storyId, s.title AS
/// title`) matched no recogniser and ran on the general path — 291 projected
/// reads per statement against none for the same chain's count on the
/// pipeline (1.8 ms on the mirror vs Neo4j's 0.8).
fn normalise_where_first_tail(q: &SingleQuery) -> Option<SingleQuery> {
    let cl = &q.clauses;
    let n = cl.len();
    if n < 3 {
        return None;
    }
    let [
        Clause::With {
            proj: wp,
            where_: _having,
        },
        Clause::With {
            proj: sp,
            where_: None,
        },
        Clause::Return { proj: rp },
    ] = &cl[n - 3..]
    else {
        return None;
    };
    // The desugared tail: a bare `WITH *` carrying only ORDER BY / SKIP / LIMIT.
    if !sp.star
        || !sp.items.is_empty()
        || sp.distinct
        || (sp.order.is_empty() && sp.skip.is_none() && sp.limit.is_none())
    {
        return None;
    }
    // A plain RETURN: no paging, ordering, DISTINCT or aggregate of its own.
    if rp.star
        || rp.distinct
        || !rp.order.is_empty()
        || rp.skip.is_some()
        || rp.limit.is_some()
        || rp.items.iter().any(|it| expr_has_aggregate(&it.expr))
    {
        return None;
    }
    // The filtering WITH carries no tail of its own (the parser moved it).
    if wp.star || !wp.order.is_empty() || wp.skip.is_some() || wp.limit.is_some() {
        return None;
    }
    let fused = Projection {
        distinct: false,
        star: false,
        items: rp.items.clone(),
        order: sp.order.clone(),
        skip: sp.skip.clone(),
        limit: sp.limit.clone(),
    };
    let mut out: Vec<Clause> = cl[..n - 2].to_vec();
    out.push(Clause::Return { proj: fused });
    counted!("interp.where-first WITH tail fused into the RETURN");
    Some(SingleQuery { clauses: out })
}

// ─── General N-STAGE pipeline (`MATCH … WITH … MATCH … WITH … MATCH … WITH …`) ─

/// One stage of an N-stage pipeline beyond stage 0: the WITH-boundary carry from
/// the previous stage, then this stage's expand hops + WHEREs.
struct PipeStage {
    /// Indices into the PREVIOUS stage's vars — the carried columns, in the WITH's
    /// item order.
    carried: Vec<usize>,
    /// `WITH DISTINCT` — dedup the carried tuples first-seen before this stage.
    distinct: bool,
    hops: Vec<Hop>,
    wheres: Vec<WherePred>,
}

/// A recognised N-stage read pipeline: `MATCH <chain0> [WHERE] (WITH <carry>
/// MATCH <chainK> [WHERE])+ <tail>`. Stage 0 scans; each later stage re-roots at
/// the carried var(s) and expands; the tail (Core / Agg / Distinct, with an
/// optional fused projection chain — IC3's `WITH … CASE … WITH … sum(CASE …)`)
/// runs over the last stage's chunk. Generalises `recognise_multistage` (2
/// stages) to any depth.
struct PipelinePlan {
    s0_a_labels: Vec<String>,
    s0_a_var: String,
    s0_hops: Vec<Hop>,
    s0_wheres: Vec<WherePred>,
    s0_anchor: Option<PropAnchor>,
    stages: Vec<PipeStage>,
    tail: MultiStageTail,
}

/// Validate a WITH carry against the previous stage's vars: every item a BARE
/// bound var (or `v AS v`), no aggregate/rename/order/page. Returns the carried
/// indices + names/kinds/labels (for the next stage's re-root) + the DISTINCT flag.
#[allow(clippy::type_complexity)]
fn pipeline_carry(
    with_proj: &Projection,
    prev_vars: &[String],
    prev_kinds: &[VarKind],
    prev_labels: &BTreeMap<String, Vec<String>>,
) -> Option<(
    Vec<usize>,
    Vec<String>,
    Vec<VarKind>,
    BTreeMap<String, Vec<String>>,
    bool,
)> {
    if with_proj.star
        || !with_proj.order.is_empty()
        || with_proj.skip.is_some()
        || with_proj.limit.is_some()
        || with_proj.items.is_empty()
    {
        return None;
    }
    let mut carried_names: Vec<String> = Vec::with_capacity(with_proj.items.len());
    for it in &with_proj.items {
        if expr_has_aggregate(&it.expr) {
            return None;
        }
        let Expr::Var(v) = &it.expr else { return None };
        if let Some(a) = &it.alias {
            if a != v {
                return None;
            }
        }
        if !prev_vars.contains(v) || carried_names.contains(v) {
            return None;
        }
        carried_names.push(v.clone());
    }
    let carried: Vec<usize> = carried_names
        .iter()
        .map(|v| {
            prev_vars
                .iter()
                .position(|x| x == v)
                .expect("carried bound")
        })
        .collect();
    let carried_kinds: Vec<VarKind> = carried.iter().map(|&i| prev_kinds[i]).collect();
    let mut carried_labels: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for v in &carried_names {
        if let Some(ls) = prev_labels.get(v) {
            carried_labels.insert(v.clone(), ls.clone());
        }
    }
    Some((
        carried,
        carried_names,
        carried_kinds,
        carried_labels,
        with_proj.distinct,
    ))
}

/// Build the tail over the last stage's chain from `[WITH*, RETURN]`: a plain
/// RETURN dispatches Core/Agg/Distinct; a `WITH+ RETURN` fuses the leading
/// projection WITHs into the final breaker WITH (IC3's `WITH … CASE … WITH …
/// sum(CASE …) WHERE … RETURN`) — a Form-A aggregate or DISTINCT tail.
fn build_pipeline_tail(chain: Chain, tail_clauses: &[Clause]) -> Option<MultiStageTail> {
    let (last, withs) = tail_clauses.split_last()?;
    let Clause::Return { proj: rp } = last else {
        return None;
    };
    if withs.is_empty() {
        return Some(if rp.items.iter().any(|it| expr_has_aggregate(&it.expr)) {
            MultiStageTail::Agg(Box::new(aggregate_over_chain(chain, rp, None)?))
        } else if rp.distinct {
            MultiStageTail::Distinct(Box::new(distinct_over_chain(chain, rp)?))
        } else {
            MultiStageTail::Core(Box::new(core_over_chain(chain, rp)?))
        });
    }
    // The LAST WITH is the breaker (aggregate / DISTINCT); the earlier WITHs are
    // pure projections fused into it by alias substitution.
    let (breaker, proj_stages) = withs.split_last()?;
    let Clause::With {
        proj: breaker_proj,
        where_: having,
    } = breaker
    else {
        return None;
    };
    let mut map: BTreeMap<String, Expr> = BTreeMap::new();
    for c in proj_stages {
        let Clause::With { proj, where_: None } = c else {
            return None;
        };
        if proj.distinct
            || proj.star
            || !proj.order.is_empty()
            || proj.skip.is_some()
            || proj.limit.is_some()
            || proj.items.is_empty()
        {
            return None;
        }
        for (i, it) in proj.items.iter().enumerate() {
            if expr_has_aggregate(&it.expr) {
                return None;
            }
            let def = substitute_aliases(&it.expr, &map);
            let alias = it
                .alias
                .clone()
                .or_else(|| it.text.clone())
                .unwrap_or_else(|| column_name(&it.expr, i));
            map.insert(alias, def);
        }
    }
    let fused_items: Vec<ProjItem> = breaker_proj
        .items
        .iter()
        .map(|it| ProjItem {
            expr: substitute_aliases(&it.expr, &map),
            alias: it.alias.clone(),
            text: it.text.clone(),
        })
        .collect();
    let fused = Projection {
        distinct: breaker_proj.distinct,
        star: breaker_proj.star,
        items: fused_items,
        order: breaker_proj.order.clone(),
        skip: breaker_proj.skip.clone(),
        limit: breaker_proj.limit.clone(),
    };
    let plan = if fused.distinct {
        distinct_form_a_over_chain(chain, &fused, having.as_ref(), rp)?
    } else {
        aggregate_over_chain(chain, &fused, Some((having.as_ref(), rp)))?
    };
    Some(MultiStageTail::Agg(Box::new(plan)))
}

/// Recognise a general N-stage pipeline (≥3 MATCHes — DISJOINT from
/// `recognise_multistage`'s 2). The clause shape is `Match (With Match)+ (With)*
/// Return`: stage 0 scans, each interior `WITH`/`MATCH` pair re-roots at the
/// carried var(s) and expands, and the trailing `WITH*`/`RETURN` is the tail over
/// the last stage. This is what IC3 becomes after the prelude / seed-filter /
/// varlen-split / collect-list rewrites compose.
fn recognise_pipeline(sq: &SingleQuery) -> Option<PipelinePlan> {
    let cl = &sq.clauses;
    let match_count = cl
        .iter()
        .filter(|c| {
            matches!(
                c,
                Clause::Match {
                    optional: false,
                    ..
                }
            )
        })
        .count();
    if match_count < 3 {
        return None;
    }
    let last_match = cl.iter().rposition(|c| {
        matches!(
            c,
            Clause::Match {
                optional: false,
                ..
            }
        )
    })?;
    let tail_clauses = &cl[last_match + 1..];
    if !matches!(tail_clauses.last(), Some(Clause::Return { .. })) {
        return None;
    }
    // Stage 0: the scan, with inline start + node anchors (the SAME construction
    // `recognise_multistage` stage 1 uses).
    let Clause::Match {
        optional: false,
        pattern: p0,
        where_: w0,
    } = &cl[0]
    else {
        return None;
    };
    let hc0 = collect_hops(p0, None, false, true, true)?;
    let anchor_eq: Option<Expr> = hc0.start_anchor.as_ref().map(|(prop, val)| {
        Expr::Bin(
            engram_cypher::ast::BinOp::Eq,
            Box::new(Expr::Prop(
                Box::new(Expr::Var(hc0.a_var.clone())),
                prop.clone(),
            )),
            Box::new(val.clone()),
        )
    });
    let combined0 = match (anchor_eq, w0.clone()) {
        (Some(a), Some(w)) => Some(Expr::And(Box::new(a), Box::new(w))),
        (Some(a), None) => Some(a),
        (None, w) => w,
    };
    let combined0 = and_node_anchors(combined0.as_ref(), &hc0.node_anchors);
    let s0_wheres = recognise_where_preds(combined0.as_ref(), &hc0.vars)?;
    let s0_anchor = prop_eq_index(combined0.as_ref(), &hc0.a_var)
        .map(|(prop, values)| PropAnchor { prop, values });

    let mut vars = hc0.vars.clone();
    let mut kinds = hc0.var_kinds.clone();
    let mut var_labels = hc0.var_labels.clone();
    let mut stages: Vec<PipeStage> = Vec::new();
    let mut i = 1usize;
    while i < last_match {
        // (WITH carry, MATCH hops).
        let Clause::With {
            proj: cw,
            where_: None,
        } = &cl[i]
        else {
            return None;
        };
        let (carried, carried_names, carried_kinds, carried_labels, distinct) =
            pipeline_carry(cw, &vars, &kinds, &var_labels)?;
        // A varlen hop in the PREVIOUS stage must be consumed DISTINCT-only by this
        // carry (the frontier-BFS discipline). Stage 0 is the common varlen site.
        let prev_hops: &[Hop] = if stages.is_empty() {
            &hc0.hops
        } else {
            &stages.last().unwrap().hops
        };
        if hops_have_varlen(prev_hops) && !varlen_distinct_consumed(prev_hops, cw) {
            return None;
        }
        let Clause::Match {
            optional: false,
            pattern: pk,
            where_: wk,
        } = &cl[i + 1]
        else {
            return None;
        };
        let prebound = (
            carried_names.as_slice(),
            carried_kinds.as_slice(),
            &carried_labels,
        );
        let hck = collect_hops(pk, Some(prebound), false, false, true)?;
        // A varlen hop in an INTERIOR stage is out of scope (only stage 0's varlen
        // is DISTINCT-consumed by its carry) — decline.
        if hops_have_varlen(&hck.hops) {
            return None;
        }
        // The stage WHERE is a CONJUNCTION split per-predicate (IC3's stage-3
        // `<date-range> AND country IN [countryX, countryY]` — two vars), plus each
        // mid-hop inline anchor ANDed in — the SAME `Vec<WherePred>` gate the join
        // paths use.
        let combined_wk = and_node_anchors(wk.as_ref(), &hck.node_anchors);
        let wheres = recognise_where_preds(combined_wk.as_ref(), &hck.vars)?;
        stages.push(PipeStage {
            carried,
            distinct,
            hops: hck.hops.clone(),
            wheres,
        });
        vars = hck.vars.clone();
        kinds = hck.var_kinds.clone();
        var_labels = hck.var_labels.clone();
        i += 2;
    }
    // The loop consumes each (WITH, MATCH) pair by 2, so after the pair whose
    // MATCH is `last_match` it lands on `last_match + 1`. Anything else means the
    // prefix was not a clean `Match (With Match)+` alternation.
    if i != last_match + 1 || stages.is_empty() {
        return None;
    }
    // The tail over the last stage's chunk — a no-hop chain over its vars.
    let chain = Chain {
        a_labels: Vec::new(),
        a_var: vars.first()?.clone(),
        hops: Vec::new(),
        vars: vars.clone(),
        var_kinds: kinds.clone(),
        wheres: Vec::new(),
        start_anchor: None,
    };
    let tail = build_pipeline_tail(chain, tail_clauses)?;
    Some(PipelinePlan {
        s0_a_labels: hc0.a_labels,
        s0_a_var: hc0.a_var,
        s0_hops: hc0.hops,
        s0_wheres,
        s0_anchor,
        stages,
        tail,
    })
}

/// Run a recognised N-stage pipeline: stage-0 chunk → for each stage
/// project(+dedup) at the WITH boundary, expand its hops, apply its WHEREs → the
/// tail over the final chunk. Byte-identical to `run_streaming` over the stages,
/// or `Ok(None)` (a budget / column decline; the general path answers identically).
fn run_pipeline(
    graph: &Graph,
    plan: &PipelinePlan,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    counted!("interp.dispatch run_pipeline");
    if hops_have_varlen(&plan.s0_hops) && !graph.frontier_expand_enabled() {
        return Ok(None);
    }
    let Some(mut chunk) = build_chunk(
        graph,
        &plan.s0_a_labels,
        &plan.s0_a_var,
        &plan.s0_hops,
        &plan.s0_wheres,
        plan.s0_anchor.as_ref(),
        params,
    )?
    else {
        return Ok(None);
    };
    // Run every stage BUT the last inline (they materialise their intermediate
    // chunk). The last stage — the one whose expansion feeds the tail, IC3's
    // friend×message fan-out — is a candidate for bounded-memory batching.
    let last = plan.stages.len().saturating_sub(1);
    for (si, stage) in plan.stages.iter().enumerate() {
        chunk = chunk.project_carried(graph, &stage.carried, stage.distinct)?;

        // Precompute this stage's end-label members + type tokens once.
        let mut hop_members: Vec<Option<crate::MembersView>> =
            Vec::with_capacity(stage.hops.len());
        let mut hop_tokens: Vec<Option<Vec<u32>>> = Vec::with_capacity(stage.hops.len());
        for hop in &stage.hops {
            let members = if hop.labels.is_empty() {
                None
            } else {
                Some(graph.members_all(&hop.labels).map_err(RunError::Graph)?)
            };
            hop_members.push(members);
            hop_tokens.push(graph.type_tokens_peek(&hop.types));
        }

        // LAST stage + a mergeable tail + no OPTIONAL prov → bounded batching over
        // this (post-`project_carried`) driving chunk. A top-k tail folds via the
        // monoid accumulator; a GROUPED aggregate concatenates per-batch groups
        // under the collision guard. Otherwise fall through to the whole-chunk
        // expand below. Byte-identical either way (single-batch == single pass).
        if si == last && graph.multistage_topk_batch_enabled() && chunk.prov.is_empty() {
            match &plan.tail {
                MultiStageTail::Core(core)
                    if core.proj.limit.is_some() && !core.proj.order.is_empty() =>
                {
                    return batched_core_last_stage(
                        graph,
                        core,
                        params,
                        &chunk,
                        &stage.hops,
                        &stage.wheres,
                        &hop_members,
                        &hop_tokens,
                    );
                }
                MultiStageTail::Agg(agg) if !agg.group_keys.is_empty() => {
                    // IC3's date-windowed HAS_CREATOR seek answers the friends'
                    // in-window messages from the date-ordered index; it declines
                    // (Ok(None)) on any other shape, leaving the batched path.
                    if graph.ic3_datewindow_enabled() {
                        if let Some(r) = try_ic3_datewindow(
                            graph,
                            agg,
                            params,
                            &chunk,
                            &stage.hops,
                            &stage.wheres,
                            &hop_members,
                            &hop_tokens,
                        )? {
                            return Ok(Some(r));
                        }
                    }
                    return batched_agg_last_stage(
                        graph,
                        agg,
                        params,
                        &chunk,
                        &stage.hops,
                        &stage.wheres,
                        &hop_members,
                        &hop_tokens,
                    );
                }
                _ => {}
            }
        }

        for (i, hop) in stage.hops.iter().enumerate() {
            let ms: Option<&crate::MembersView> = hop_members[i].as_ref();
            chunk = run_hop(graph, chunk, hop, ms, &hop_tokens[i])?;
        }
        for pred in &stage.wheres {
            if chunk.filter(graph, params, pred)?.is_none() {
                return Ok(None);
            }
        }
    }
    match &plan.tail {
        MultiStageTail::Core(core) => {
            run_core_over_chunk(graph, core, params, &chunk, finish_multistage)
        }
        MultiStageTail::Agg(agg) => {
            run_aggregate_over_chunk(graph, agg, params, &chunk, finish_multistage)
        }
        MultiStageTail::Distinct(dp) => {
            run_distinct_over_chunk(graph, dp, params, &chunk, finish_multistage)
        }
    }
}

// ─── Two-MATCH set-based HASH JOIN (`MATCH <chainA> MATCH <chainB> …`) ─────────

/// A recognised two-MATCH conjunctive JOIN feeding an order-insensitive
/// group-by/aggregate with a TOTAL order — the IC5 friend/forum shape. Cypher
/// joins successive MATCH clauses on their SHARED (already-bound) variables; the
/// nested `run_streaming` re-runs chainB per chainA row (O(N*M)). This plan runs
/// each side ONCE and HASH-JOINs on the shared id-tuple (O(N+M)), then feeds the
/// EXISTING aggregate + ORDER BY / LIMIT tail.
///
/// Side A is a standalone read chain (its own labelled scan). Side B RE-ROOTS at
/// a var chainA already bound, seeded from side A's DISTINCT ids of that var (a
/// semi-join pushdown) and expanded ONCE — so it enumerates the full
/// (shared-start, …, other-shared) tuples the join needs. Every OTHER shared var
/// (e.g. `friend`) is an EXPAND in side B, so it becomes a joinable column.
struct JoinPlan {
    /// Side A: the standalone read chain.
    a_labels: Vec<String>,
    a_var: String,
    a_hops: Vec<Hop>,
    a_where: Option<WherePred>,
    /// Side B: seeded from side A's DISTINCT ids of the shared start var.
    /// `b_seed_from_a` is the column index INTO side A supplying the seed ids;
    /// `b_seed_var` is the shared start var's name (side B's index 0).
    b_seed_from_a: usize,
    b_seed_var: String,
    b_hops: Vec<Hop>,
    b_where: Option<WherePred>,
    /// The aggregating tail over the COMBINED binding order (a_vars ++ B-only
    /// vars). Its var indices are into that order; the joined chunk is built in
    /// exactly that order so they align.
    tail: AggPlan,
}

/// Recognise `MATCH <chainA> [WHERE] MATCH <chainB> [WHERE] <group-by aggregate>
/// [ORDER BY] [LIMIT]` (Form B RETURN or Form A WITH→RETURN) as a set-based hash
/// join. The clause shape ([Match(non-opt), Match(non-opt), <tail>]) is disjoint
/// from every other recognizer.
///
/// ACCEPT only when the result is order-insensitive under the hash join's
/// different row order: an AGGREGATE with order-insensitive sites
/// (count/min/max), and either a GLOBAL aggregate (one row) or a TOTAL ORDER
/// BY ([`join_tail_order_safe`]). DECLINE (byte-identical fallback → the nested
/// general path): a non-aggregated multi-MATCH (raw, order-sensitive rows), a
/// group-by with no/partial ORDER BY, `collect`/`avg`/unmodelled aggregates, NO
/// shared bound var (a cartesian product), a chainB whose start is not a chainA
/// var, a shared var whose KIND differs across the sides, a spanning WHERE, and
/// anything the sub-recognizers (`collect_hops` / `recognise_single_var_where` /
/// `aggregate_over_chain`) already decline (var-length, OPTIONAL, rel-prop maps,
/// start props, …).
fn recognise_join(sq: &SingleQuery) -> Option<JoinPlan> {
    let (pa, wa, pb, wb, tail_clauses) = match sq.clauses.as_slice() {
        [
            Clause::Match {
                optional: false,
                pattern: pa,
                where_: wa,
            },
            Clause::Match {
                optional: false,
                pattern: pb,
                where_: wb,
            },
            rest @ ..,
        ] => (pa, wa.as_ref(), pb, wb.as_ref(), rest),
        _ => return None,
    };

    // SIDE A: a standalone read chain (its own labelled scan start).
    let hca = collect_hops(pa, None, false, false, false)?;
    let a_where = recognise_single_var_where(wa, &hca.vars)?;

    // SIDE B re-roots at a var chainA already bound — chainB's path-1 start must
    // be a chainA var (its label + kind come from chainA). Prebinding ONLY that
    // start var makes every OTHER shared var (e.g. `friend`) an EXPAND that
    // introduces its own column, so side B enumerates the full tuples the join
    // needs. A start that is NOT a chainA var is a cartesian/uncorrelated product
    // we do not model here — decline.
    let b_start = pb.paths.first()?.start.var.as_deref()?;
    let b_seed_from_a = hca.vars.iter().position(|v| v == b_start)?;
    let seed_names = [b_start.to_string()];
    let seed_kinds = [hca.var_kinds[b_seed_from_a]];
    let mut seed_labels: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Some(ls) = hca.var_labels.get(b_start) {
        seed_labels.insert(b_start.to_string(), ls.clone());
    }
    let hcb = collect_hops(
        pb,
        Some((&seed_names, &seed_kinds, &seed_labels)),
        false,
        false,
        false,
    )?;
    // A var-length hop in either join side is out of scope — decline to the
    // general path.
    if hops_have_varlen(&hca.hops) || hops_have_varlen(&hcb.hops) {
        return None;
    }
    let b_where = recognise_single_var_where(wb, &hcb.vars)?;

    // THE JOIN KEY = the vars bound in BOTH chains (a conjunctive comma-join). At
    // least one is required — no shared var is a cartesian product we do not model
    // (decline). A shared var's KIND must match on both sides.
    let mut shared_count = 0usize;
    for (ai, av) in hca.vars.iter().enumerate() {
        if let Some(bi) = hcb.vars.iter().position(|bv| bv == av) {
            if hca.var_kinds[ai] != hcb.var_kinds[bi] {
                return None; // a Node/Rel mismatch is not a joinable key
            }
            shared_count += 1;
        }
    }
    if shared_count == 0 {
        return None; // no shared bound var — a cartesian product (decline)
    }

    // COMBINED binding order = side A's vars, then side B's NON-shared vars (in B
    // order). The tail plan's var indices are into THIS order; the joined chunk is
    // built in exactly this order so they align.
    let mut combined_vars = hca.vars.clone();
    let mut combined_kinds = hca.var_kinds.clone();
    for (bi, bv) in hcb.vars.iter().enumerate() {
        if !hca.vars.iter().any(|av| av == bv) {
            combined_vars.push(bv.clone());
            combined_kinds.push(hcb.var_kinds[bi]);
        }
    }
    let combined_chain = Chain {
        a_labels: Vec::new(),
        a_var: String::new(),
        hops: Vec::new(),
        vars: combined_vars,
        var_kinds: combined_kinds,
        wheres: Vec::new(),
        start_anchor: None,
    };

    // THE TAIL must be an order-insensitive AGGREGATE with a total order. Only the
    // aggregating Form B RETURN (the IC5 shape); a non-aggregating multi-MATCH
    // (raw, order-sensitive rows) makes `aggregate_over_chain` decline. The Form A
    // WITH→RETURN aggregate is DECLINED here (its ORDER BY sits on the RETURN over
    // the WITH aliases — a second alias indirection the total-order gate does not
    // yet resolve soundly); it falls back to the general path.
    let tail = match tail_clauses {
        [Clause::Return { proj }] => aggregate_over_chain(combined_chain, proj, None)?,
        _ => return None,
    };
    if !join_tail_order_safe(&tail) {
        return None; // order-sensitive aggregate, or a non-total ORDER BY
    }

    Some(JoinPlan {
        a_labels: hca.a_labels,
        a_var: hca.a_var,
        a_hops: hca.hops,
        a_where,
        b_seed_from_a,
        b_seed_var: b_start.to_string(),
        b_hops: hcb.hops,
        b_where,
        tail,
    })
}

/// Whether the hash join's DIFFERENT row order cannot change this aggregate
/// tail's output — the byte-identity gate. The set-based join emits joined rows
/// (and therefore first-SEES groups) in a different order than the nested
/// `run_streaming`; that is invisible in the output ONLY under two conditions,
/// both required.
///
/// CONDITION 1 — every aggregate site is ORDER-INSENSITIVE. `count`/`min`/`max`
/// do not depend on fold order for ANY type. `sum` is DECLINED - a float sum is non-
/// associative and int-ness is not statically known. `collect` (encounter order),
/// `avg` (float division of an
/// order-sensitive float sum) and the unmodelled `stdev`/`percentile*` are NOT —
/// decline them.
///
/// CONDITION 2 — row order is pinned. A GLOBAL aggregate (no grouping key) is ONE
/// row, order-trivial. Otherwise (Form B RETURN) there must be EXACTLY ONE
/// grouping key and a TOTAL ORDER BY whose FINAL key resolves to that grouping
/// key, in one of two ways. Way (a): the final key IS the grouping key — its
/// expr, or the alias the grouping item projects — so each output row is a
/// distinct grouping value and the final key equals that value; distinct rows
/// never tie (SOUND, no data assumption — the IC5 `ORDER BY count DESC, id ASC`
/// case where `id` aliases the single grouping key `forum.id`). Way (b): the
/// grouping key is a bare-NODE identity and the final key reads ONLY that group
/// var (a per-node property/identity such as `forum.id` when grouping by node
/// `forum`) — well-defined per group, and total when unique per node (the
/// differential oracle verifies it for the accepted shapes).
///
/// Everything else declines (the general path answers identically): >1 grouping
/// key, no ORDER BY, a final key that is an aggregate/const/other var, Form A.
fn join_tail_order_safe(plan: &AggPlan) -> bool {
    for site in &plan.sites {
        if !matches!(site.name.as_str(), "count" | "min" | "max") {
            return false;
        }
    }
    // A GLOBAL aggregate is a single row — order-trivial, no ORDER BY needed.
    if plan.group_keys.is_empty() {
        return true;
    }
    // A grouped aggregate needs a TOTAL ORDER BY over a SINGLE grouping key.
    if plan.group_keys.len() != 1 {
        return false; // >1 grouping key: ordering by one leaves the others to tie
    }
    let gk = &plan.group_keys[0];
    match &plan.form {
        // Form B: the aggregating RETURN carries both the group-key projection and
        // the ORDER BY (the join / multistage-join tail).
        AggForm::Return(proj) => {
            let Some(last) = proj.order.last() else {
                return false; // group-by with NO ORDER BY — first-seen unreproducible
            };
            // The ONE grouping-key projection item (the sole `AggItem::Key`).
            let mut group_item: Option<&engram_cypher::stmt::ProjItem> = None;
            for (it, ai) in proj.items.iter().zip(&plan.agg_items) {
                if matches!(ai, AggItem::Key) {
                    if group_item.is_some() {
                        return false; // >1 key item — inconsistent with a single key
                    }
                    group_item = Some(it);
                }
            }
            let Some(group_item) = group_item else {
                return false;
            };
            // (a) The final ORDER BY key IS the grouping key — by expression, or by
            // the alias the grouping item projects (`count(post) DESC, id ASC`).
            if last.expr == group_item.expr {
                return true;
            }
            if let Expr::Var(name) = &last.expr {
                if group_item.alias.as_deref() == Some(name.as_str()) {
                    return true;
                }
            }
            // (b) A bare-NODE grouping key, and the final key reads ONLY that group
            // var (a per-node property/identity — total when unique per node).
            if let GroupKind::Node(gv) = gk.kind {
                let mut fv = Vec::new();
                free_vars_of(&last.expr, &mut fv);
                if !fv.is_empty() && fv.iter().all(|v| plan.vars.get(gv) == Some(v)) {
                    return true;
                }
            }
            false
        }
        // Form A (aggregating WITH → plain RETURN — the full IC5 tail): the ORDER BY
        // lives on the RETURN and the group key is `plan.group_keys[0]` (the WITH's
        // sole key), whose PROPERTIES the RETURN projects (`forum.title`) rather than
        // the bare key. Total order iff the final ORDER BY key IS that key by
        // expression, or reads ONLY the group var (a unique-per-node property such as
        // `forum.id`) — the same (a)/(b) test, sourced from the key not a RETURN item.
        AggForm::With(wf) => {
            let Some(last) = wf.return_proj.order.last() else {
                return false;
            };
            if last.expr == gk.expr {
                return true;
            }
            if let GroupKind::Node(gv) = gk.kind {
                let mut fv = Vec::new();
                free_vars_of(&last.expr, &mut fv);
                if !fv.is_empty() && fv.iter().all(|v| plan.vars.get(gv) == Some(v)) {
                    return true;
                }
            }
            false
        }
    }
}

/// Run a recognised two-MATCH set-based hash join: build side A (standalone
/// chain), build side B seeded from side A's DISTINCT shared-start ids (one
/// expansion pass — the set-based replacement for the per-A-row nested re-scan),
/// HASH-JOIN on the shared id-tuple, then the EXISTING group-by/aggregate +
/// ORDER BY / LIMIT tail (stamped `finish_join`). Byte-identical to the nested
/// `run_streaming` on the accepted shapes, or `Ok(None)` (a budget / column
/// decline; the general path answers identically).
fn run_join(
    graph: &Graph,
    plan: &JoinPlan,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    counted!("interp.dispatch run_join");
    // SIDE A.
    let Some(chunk_a) = build_chunk(
        graph,
        &plan.a_labels,
        &plan.a_var,
        &plan.a_hops,
        where_slice(&plan.a_where),
        None,
        params,
    )?
    else {
        return Ok(None); // a filter budget / non-boolean decline
    };

    // SIDE B seeded from side A's DISTINCT shared-start ids (only start nodes that
    // survived side A can join — a semi-join pushdown). A FRESH clause: empty
    // per-row `used_rels` (`seed_used = &[]`), so chainB's relationship
    // isomorphism is scoped to chainB alone — Cypher rel-uniqueness is
    // per-MATCH-clause (`run_streaming`'s `match_path` starts `Partial.used`
    // empty), so chainB may reuse an edge chainA walked.
    let sidx = plan.b_seed_from_a;
    let mut seed: BTreeSet<u64> = BTreeSet::new();
    for &r in &chunk_a.selection {
        seed.insert(chunk_a.ids[sidx][r]);
    }
    let seed_ids: Vec<u64> = seed.into_iter().collect();
    let Some(chunk_b) = build_chunk_from_ids(
        graph,
        &plan.b_seed_var,
        seed_ids,
        &[],
        &plan.b_hops,
        where_slice(&plan.b_where),
        params,
    )?
    else {
        return Ok(None); // a filter budget / non-boolean decline
    };

    // HASH JOIN on the shared vars → the combined chunk in `plan.tail`'s order.
    let combined = hash_join_chunks(graph, &chunk_a, &chunk_b)?;

    // The EXISTING group-by/aggregate + ORDER BY / LIMIT tail.
    run_aggregate_over_chunk(graph, &plan.tail, params, &combined, finish_join)
}

/// HASH-JOIN two already-built chunks on their SHARED vars → one combined chunk.
/// The shared vars are the join key (id-tuple in side-A var order); the combined
/// row is side A's columns then side B's NON-shared columns (the shared columns
/// coincide, so A's copy is used) — exactly the combined binding order the tail
/// plan indexes. Hash the SMALLER live side into a `BTreeMap` (NOT `HashMap` —
/// deterministic iteration) keyed by the shared id-tuple, probe with the larger,
/// and emit ONE joined row per (A-row, B-row) pair whose key matches (full
/// cross-product multiplicity — a duplicate A-row multiplies exactly as the
/// nested loop's per-A-row re-scan does). The emitted ROW ORDER differs from the
/// nested loop; `join_tail_order_safe` guarantees that is invisible downstream.
fn hash_join_chunks(graph: &Graph, a: &DataChunk, b: &DataChunk) -> Result<DataChunk, RunError> {
    // Shared (a_idx, b_idx) in side-A var order — the fixed key order for build +
    // probe.
    let mut shared: Vec<(usize, usize)> = Vec::new();
    for (ai, av) in a.vars.iter().enumerate() {
        if let Some(bi) = b.vars.iter().position(|bv| bv == av) {
            shared.push((ai, bi));
        }
    }
    // Side B's NON-shared columns, in B order — the extra columns the join appends.
    let extra_b: Vec<usize> = (0..b.vars.len())
        .filter(|&bi| !a.vars.iter().any(|av| av == &b.vars[bi]))
        .collect();
    let ncols = a.vars.len() + extra_b.len();
    let mut out_ids: Vec<Vec<u64>> = (0..ncols).map(|_| Vec::new()).collect();

    let a_key = |ar: usize| -> Vec<u64> { shared.iter().map(|&(ai, _)| a.ids[ai][ar]).collect() };
    let b_key = |br: usize| -> Vec<u64> { shared.iter().map(|&(_, bi)| b.ids[bi][br]).collect() };

    // Assemble the combined row for an (A-row, B-row) pair: side A's columns then
    // side B's non-shared columns.
    macro_rules! emit_pair {
        ($ar:expr, $br:expr) => {{
            for vi in 0..a.vars.len() {
                out_ids[vi].push(a.ids[vi][$ar]);
            }
            for (k, &bi) in extra_b.iter().enumerate() {
                out_ids[a.vars.len() + k].push(b.ids[bi][$br]);
            }
            budget_check(graph, out_ids[0].len())?;
        }};
    }

    if a.live() <= b.live() {
        // HASH the smaller side A, PROBE with side B.
        let mut ht: BTreeMap<Vec<u64>, Vec<usize>> = BTreeMap::new();
        for &ar in &a.selection {
            ht.entry(a_key(ar)).or_default().push(ar);
        }
        for &br in &b.selection {
            if let Some(ars) = ht.get(&b_key(br)) {
                for &ar in ars {
                    emit_pair!(ar, br);
                }
            }
        }
    } else {
        // HASH the smaller side B, PROBE with side A.
        let mut ht: BTreeMap<Vec<u64>, Vec<usize>> = BTreeMap::new();
        for &br in &b.selection {
            ht.entry(b_key(br)).or_default().push(br);
        }
        for &ar in &a.selection {
            if let Some(brs) = ht.get(&a_key(ar)) {
                for &br in brs {
                    emit_pair!(ar, br);
                }
            }
        }
    }

    let mut vars = a.vars.clone();
    let mut var_kinds = a.var_kinds.clone();
    for &bi in &extra_b {
        vars.push(b.vars[bi].clone());
        var_kinds.push(b.var_kinds[bi]);
    }
    let n = out_ids.first().map_or(0, Vec::len);
    // Neither join side is ever folded (folds are planned only on the
    // single-MATCH count-only aggregate), so the joined rows all weigh 1.
    debug_assert!(
        a.weights.is_empty() && b.weights.is_empty(),
        "a hash-join side never carries fold weights"
    );
    Ok(DataChunk {
        vars,
        var_kinds,
        ids: out_ids,
        selection: (0..n).collect(),
        // Nothing downstream expands the joined chunk (the aggregate tail only
        // reduces + projects), so per-row rel-iso / OPTIONAL provenance are moot.
        used_rels: Vec::new(),
        prov: Vec::new(),
        weights: Vec::new(),
    })
}

// ─── The FULL IC5 composite: stage-1 → WITH → two-MATCH JOIN ───────────────────

/// The full LDBC SNB IC5 shape: a two-stage read whose STAGE 2 is itself a
/// two-MATCH conjunctive HASH JOIN. Composes the multistage stage-1→WITH boundary
/// ([`MultiStagePlan`]/`run_multistage`) with the set-based join
/// ([`JoinPlan`]/`run_join`) — the ONE genuinely new wiring is that chainA (side
/// A of the join) is SEEDED FROM THE CARRIED SET rather than a fresh label scan,
/// so only the stage-1-reached seeds drive the join (the load-bearing
/// carried-set restriction the canary breaks).
struct MultiStageJoinPlan {
    /// Stage 1: scan start, expand/semijoin steps and the stage-1 WHERE — the SAME
    /// fields `run_multistage` builds and runs via `build_chunk`.
    s1_a_labels: Vec<String>,
    s1_a_var: String,
    s1_hops: Vec<Hop>,
    /// The stage-1 WHERE as a CONJUNCTION of tractable predicates (the LITERAL
    /// IC5's `person.id = X AND person <> friend`), plus any desugared INLINE
    /// `{prop: val}` start anchor, each applied as early as its vars bind.
    s1_wheres: Vec<WherePred>,
    /// A seekable source-property anchor for the stage-1 scan — from the inline
    /// `(person:Person {id: val})` map OR a `person.id = val` WHERE equality — so
    /// the scan SEEDS the range index instead of the whole label. `None` when the
    /// stage-1 start carries no seekable equality (the whole label is scanned).
    s1_anchor: Option<PropAnchor>,
    /// The SINGLE carried-var index INTO the stage-1 chain's `vars` (the WITH
    /// projects exactly one var — see `recognise_multistage_join`). It becomes
    /// chainA's low index (`0`) after the seed.
    carried: Vec<usize>,
    /// `WITH DISTINCT`: dedup the carried ids first-seen BEFORE the join.
    distinct: bool,
    /// SIDE A = chainA, re-rooted at the carried var and SEEDED FROM THE CARRIED
    /// SET. `a_seed_var` is that carried var's name (chainA's index 0).
    a_seed_var: String,
    a_hops: Vec<Hop>,
    a_where: Option<WherePred>,
    /// SIDE B = chainB, seeded from side A's DISTINCT ids of the shared start var
    /// (`b_seed_from_a` is the column index INTO side A supplying the seed;
    /// `b_seed_var` is that shared var's name) — the SAME wiring as [`JoinPlan`].
    b_seed_from_a: usize,
    b_seed_var: String,
    b_hops: Vec<Hop>,
    b_where: Option<WherePred>,
    /// JOIN-ORDERING re-root: when `true`, chainB has been re-rooted at the CARRIED
    /// var (its `b_hops`/`b_seed_var` are the reversed chain) and is SEEDED FROM THE
    /// DISTINCT CARRIED SET rather than from side A's shared-start column. This
    /// expands O(carried entities' posts) instead of O(shared-start's posts) — the
    /// selective order — while the hash join on the shared vars is unchanged, so
    /// the result multiset is byte-identical. `false` = the forward forum-rooted
    /// chainB (`b_seed_from_a` supplies the seed), the byte-identical fallback.
    b_seed_from_carried: bool,
    /// The aggregating tail over the COMBINED binding order (chainA vars ++ chainB
    /// non-shared vars), gated order-safe by `join_tail_order_safe` — reused
    /// verbatim from the join.
    tail: AggPlan,
}

/// Flip a relationship direction for chain reversal: an incoming edge becomes an
/// outgoing traversal from the other endpoint and vice versa; undirected stays.
fn flip_dir(d: RelDir) -> RelDir {
    match d {
        RelDir::Out => RelDir::In,
        RelDir::In => RelDir::Out,
        RelDir::Undirected => RelDir::Undirected,
    }
}

/// Re-root a SINGLE-path chain pattern at its TERMINAL bound node var, reversing
/// the hops so that var is the seed — the JOIN-ORDERING alternate for chainB. The
/// chain `(n0)-[r1]->(n1)-…-[rk]->(nk)` re-rooted at `nk` becomes
/// `(nk)<-[rk']-(n(k-1))<-…<-[r1']-(n0)`: the node order reversed and each rel's
/// direction flipped ([`flip_dir`]). The result is a syntactically-valid
/// [`Pattern`] that [`collect_hops`] recognises normally via its EXISTING prebound
/// re-root (seed at `seed_var`, expand FORWARD) — `collect_hops` itself never
/// reverses or re-roots at a non-start var, so the reversal is done here at the
/// AST and no hop machinery changes. The reversed walk traverses the SAME edges in
/// the opposite order, so the reachable tuples (and thus the join result) are
/// identical.
///
/// Returns `None` — the caller keeps the forward root, byte-identically — for
/// anything this cannot re-root: a multi-path pattern, a named path /
/// `shortestPath`, a hopless path, or a `seed_var` that is not the path's TERMINAL
/// node. A START or MID-CHAIN re-root is deliberately declined: a start re-root is
/// the forward case, and a mid-chain re-root would split the chain into a branch
/// (two paths from the seed) whose per-path relationship-isomorphism reset is not
/// obviously identical to the single forward path — out of scope, so it falls back.
/// Rewrite a query so its first MATCH drives from an INDEX-SERVABLE endpoint
/// instead of from whichever endpoint was written first.
///
/// # The defect
///
/// `collect_hops` takes the scan root from `path.start` — `introduces_start =
/// pi == 0 && !reroot_all` — and `start_prop_anchor` is consulted only there. A
/// mid-chain or terminal node's inline `{prop: val}` becomes a post-hop FILTER
/// (`node_anchors`), never a seed. So these two spellings of one question plan
/// completely differently:
///
/// ```text
/// MATCH (m:Msg)-[:BY]->(p:Person {pid: 7}) RETURN m.mid LIMIT 25   -- scans every :Msg
/// MATCH (p:Person {pid: 7})<-[:BY]-(m:Msg) RETURN m.mid LIMIT 25   -- seeks one row
/// ```
///
/// Measured in-process over 40,000 messages: **28.0 ms against 0.163 ms, 172x**.
/// Over Bolt against a 100k-node LDBC SNB corpus, the same shape (IS5) cost
/// 24.6x. The scaling report is the proof it is a scan and not a heavier
/// traversal — with the answer size pinned by `LIMIT`, 10x the corpus made the
/// message-first form 11.4x slower and left the person-first form flat.
///
/// A drop-in replacement cannot charge this: the database being replaced picks
/// its anchor by selectivity, so applications are full of patterns written
/// either way round, because either way round is free there.
///
/// # Why here and not in the recognizer
///
/// `recognise_chain`/`recognise_core` are pure AST recognizers with no `Graph`
/// — deliberately, since a plan that is a pure function of the query is far
/// easier to reason about. The label-size guard below needs cardinalities, so
/// the rewrite happens here, where the graph is in hand, and the recognizers
/// stay pure. It is the same shape as the normalisation pre-pass above.
///
/// # Guards
///
/// Only the unambiguous case is taken:
///
///  - the first clause is a non-OPTIONAL MATCH over a single fixed-length path
///    with no path variable (`reroot_single_path_at` declines the rest);
///  - the start offers NO inline map of its own, so nothing is given up;
///  - the terminal node offers one that `start_prop_anchor` accepts;
///  - the terminal's label is no larger than the start's.
///
/// That last guard is what makes this selectivity rather than a preference for
/// the far end. Without it `(a:Tiny)-[:R]->(b:Huge {p: 1})` would reroot, and
/// if the probe then lost at execution the fallback would scan `Huge` where it
/// used to scan `Tiny`.
fn reroot_to_selective_end(graph: &Graph, q: &SingleQuery) -> Option<SingleQuery> {
    if !graph.selective_anchor_enabled() {
        return None;
    }
    let Some(Clause::Match {
        optional: false,
        pattern,
        where_,
    }) = q.clauses.first()
    else {
        return None;
    };
    if pattern.paths.len() != 1 {
        return None;
    }
    let path = &pattern.paths[0];
    if path.var.is_some() || path.shortest || path.hops.is_empty() {
        return None;
    }
    // A variable-length hop's reversal is not modelled here (and the chain
    // recognizer declines one anyway); refuse explicitly rather than rely on a
    // later decline to cover it.
    if path.hops.iter().any(|(rel, _)| rel.length.is_some()) {
        return None;
    }
    // The start must have nothing to seed itself with.
    if path.start.props.is_some() {
        return None;
    }
    let (_, terminal) = path.hops.last().expect("non-empty checked above");
    let seed_var = terminal.var.as_deref()?;
    // ...and the terminal must have an anchor the seed path can actually use.
    // `start_prop_anchor` is the same acceptance test `collect_hops` applies to
    // a start, so a terminal it rejects would not become a seek after rerooting.
    start_prop_anchor(terminal.props.as_ref())?;
    // Cost guard. An unlabelled pattern has no cardinality; treating it as
    // unbounded means an unlabelled terminal never displaces a labelled start.
    let size = |n: &NodePattern| -> u64 {
        n.labels
            .iter()
            .map(|l| graph.count_label_nodes(l))
            .min()
            .unwrap_or(u64::MAX)
    };
    if size(terminal) > size(&path.start) {
        return None;
    }
    let rerooted = reroot_single_path_at(pattern, seed_var)?;
    counted!("pipeline.chain rerooted for selectivity");
    let mut clauses = q.clauses.clone();
    clauses[0] = Clause::Match {
        optional: false,
        pattern: rerooted,
        where_: where_.clone(),
    };
    Some(SingleQuery { clauses })
}

fn reroot_single_path_at(pattern: &Pattern, seed_var: &str) -> Option<Pattern> {
    if pattern.paths.len() != 1 {
        return None;
    }
    let path = &pattern.paths[0];
    if path.var.is_some() || path.shortest || path.hops.is_empty() {
        return None;
    }
    // The seed var must be the path's TERMINAL node (the last hop's end).
    let (_, terminal) = path.hops.last()?;
    if terminal.var.as_deref() != Some(seed_var) {
        return None;
    }
    Some(Pattern {
        paths: vec![reverse_path(path)],
    })
}

/// Walk ONE path backwards: the node sequence `n0..nk` reversed and each rel's
/// direction flipped ([`flip_dir`]), so `(n0)-[r1]->(n1)-…-[rk]->(nk)` becomes
/// `(nk)<-[rk']-(n(k-1))<-…<-[r1']-(n0)`. The reversed walk traverses the SAME
/// relationships in the opposite order, so it matches the same tuples — and
/// relationship isomorphism is unchanged, because a walk is legal iff its rel
/// set is pairwise distinct, which is a property of the SET and not of the order
/// it was built in. The path var / `shortest` are dropped: both callers refuse a
/// path that carries either.
fn reverse_path(path: &PathPattern) -> PathPattern {
    // The node sequence n0..nk (start then each hop's end node).
    let mut nodes: Vec<&NodePattern> = Vec::with_capacity(path.hops.len() + 1);
    nodes.push(&path.start);
    for (_, n) in &path.hops {
        nodes.push(n);
    }
    let k = path.hops.len();
    // Reversed hops: for hi = k-1 … 0, rel r(hi+1) reversed, targeting n(hi).
    let mut rev_hops: Vec<(RelPattern, NodePattern)> = Vec::with_capacity(k);
    for hi in (0..k).rev() {
        let (rel, _) = &path.hops[hi];
        let rr = RelPattern {
            var: rel.var.clone(),
            types: rel.types.clone(),
            dir: flip_dir(rel.dir),
            props: rel.props.clone(),
            length: rel.length,
        };
        rev_hops.push((rr, nodes[hi].clone()));
    }
    PathPattern {
        var: None,
        shortest: false,
        start: nodes[k].clone(),
        hops: rev_hops,
    }
}

// ─── COUNT-ONLY JOIN REORDER (operator C of docs/lsqb-completeness-plan.md) ──

/// Whether a RETURN produces exactly ONE row whose content depends on NOTHING
/// but how many matches the pattern has — every item aggregate-bearing with no
/// free pattern variable left after the aggregates are lifted, every site a
/// non-DISTINCT `count(*)`, and no ORDER BY / SKIP / LIMIT to observe an order
/// with. That is what makes the reorder below UNOBSERVABLE: production order,
/// grouping order and row count are all fixed regardless of how the pattern is
/// walked, so the rewrite can only change the plan, never the answer.
fn count_star_only_return(proj: &Projection) -> bool {
    if proj.star
        || proj.distinct
        || proj.items.is_empty()
        || !proj.order.is_empty()
        || proj.skip.is_some()
        || proj.limit.is_some()
    {
        return false;
    }
    let (sites, items) = plan_agg_projection(proj);
    if !all_sites_count_star(&sites) {
        return false;
    }
    items.iter().all(|it| match it {
        // A grouping key would make the row COUNT depend on the pattern's
        // bindings, not just on how many there are.
        AggItem::Key => false,
        AggItem::Agg { rewritten, .. } => {
            let mut fv = Vec::new();
            free_vars_of(rewritten, &mut fv);
            fv.is_empty()
        }
    })
}

/// Each variable's LABEL UNION across every occurrence in the pattern.
fn var_label_union(pattern: &Pattern) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut add = |n: &NodePattern| {
        let Some(v) = n.var.as_deref() else {
            return;
        };
        let entry = out.entry(v.to_string()).or_default();
        for l in &n.labels {
            if !entry.contains(l) {
                entry.push(l.clone());
            }
        }
    };
    for p in &pattern.paths {
        add(&p.start);
        for (_, n) in &p.hops {
            add(n);
        }
    }
    out
}

/// Restate every variable's LABEL UNION at every one of its occurrences.
///
/// Semantics-preserving: a label written on a variable is a constraint on that
/// variable wherever it is written — every pattern predicate in one MATCH is
/// conjunctive — so restating a label the var already carries elsewhere adds
/// nothing. It is what lets the reordered pattern be RECOGNISED at all:
/// `collect_hops` accepts a later path's restated label only when the var
/// already holds it and DECLINES a label it has not seen, so a path re-rooted at
/// a var whose label was written on a different path would otherwise be refused
/// (and a seed whose label was written elsewhere would have no label to scan).
fn stamp_labels(pattern: &Pattern, union: &BTreeMap<String, Vec<String>>) -> Pattern {
    let stamp = |n: &NodePattern| -> NodePattern {
        let mut out = n.clone();
        if let Some(ls) = n.var.as_deref().and_then(|v| union.get(v)) {
            out.labels = ls.clone();
        }
        out
    };
    Pattern {
        paths: pattern
            .paths
            .iter()
            .map(|p| PathPattern {
                var: p.var.clone(),
                shortest: p.shortest,
                start: stamp(&p.start),
                hops: p.hops.iter().map(|(r, n)| (r.clone(), stamp(n))).collect(),
            })
            .collect(),
    }
}

/// Whether any node of `path` binds `var`.
fn path_touches(path: &PathPattern, var: &str) -> bool {
    path.start.var.as_deref() == Some(var)
        || path
            .hops
            .iter()
            .any(|(_, n)| n.var.as_deref() == Some(var))
}

/// Re-root and re-order a count-only pattern so it can be walked as ONE
/// connected expand chain from its most selective end. See
/// [`reorder_for_count_only`] for why this is unobservable and when it is taken.
/// `None` = a pattern this does not model, or one it cannot connect.
/// Paths in one MATCH past which the ordering search falls back to the greedy.
///
/// The search is `k!` orderings, each scored in `O(hops)`, so six paths is 720
/// scorings of a few dozen multiplications over a memoised fan-out table —
/// microseconds. LSQB's widest count-only pattern (q3) has five.
const ORDER_SEARCH_MAX_PATHS: usize = 6;



/// The memo key for one hop's fan-out: start labels, direction, types, end
/// labels. `count_hop` iterates the smaller labelled side, so the search must
/// not re-ask it once per permutation.
type FanoutKey = (Vec<String>, u8, Vec<String>, Vec<String>);

/// The ordering whose PEAK intermediate row count is smallest, or `None` when
/// no ordering keeps every path attached to the bound set.
///
/// # Why the greedy below is not enough
///
/// It scores the IMMEDIATE step, and it treats a both-ends-bound path as a free
/// close. Both are wrong together on LSQB q2:
///
/// ```cypher
/// MATCH (person1:Person)-[:KNOWS]-(person2:Person),
///       (person1)<-[:HAS_CREATOR]-(comment:Comment)-[:REPLY_OF]->(post:Post)-[:HAS_CREATOR]->(person2)
/// RETURN count(*)
/// ```
///
/// Seeded at `person1` (9,892 Persons — the smallest labelled endpoint),
/// neither path has both ends bound, so the greedy takes the cheaper immediate
/// fan-out: KNOWS, about 36, giving 356k rows. The comment path is then
/// both-ends-bound and taken as a "free close" — but it is THREE hops, so it
/// still expands each row's ~212 comments before its last hop closes onto the
/// bound `person2`. Peak about 75M rows.
///
/// Taking the comment path FIRST costs 9,892 x 212 = ~2.1M and leaves KNOWS a
/// genuine one-hop close. Peak 2.1M — 36x smaller. Measured on the pod at SF1
/// the ratio is real: q2 records **201,912,362** `adjacency tables reused`
/// where the SAME chain with the cycle REMOVED records **3,073,484**.
///
/// So the peak must be tracked per HOP and not per path, and a hop landing on
/// an already-bound var is a SEMIJOIN whose selectivity is `fanout / |target|`
/// rather than an expansion by `fanout`.
///
/// Ties keep the EARLIEST ordering — candidates are generated in index order
/// and only a strict improvement displaces the incumbent — so the choice is a
/// function of the source text and never of a float-equal comparison.
///
/// # A MARGIN was tried here, twice, and removed
///
/// The model is structural — exact label counts and average fan-out, no WHERE
/// selectivity, no degree skew, no cost for a semijoin close — so requiring
/// the search to beat the greedy by some factor before displacing it looked
/// obviously right. Two builds did exactly that (4.0, then 1.0, with the
/// greedy scored on the SAME model first). BOTH measured worse than taking the
/// search outright: LSQB q2 5,207 ms and q4 4,470 against **2,807** and
/// **1,735**, as N=3 medians whose back-to-back spread is under 9%.
///
/// The evidence that motivated the margin did not survive either — it was two
/// "regressions" of 1.07x and 1.09x read off single runs, and re-running one
/// arm showed the harness moves more than that on its own.
///
/// Reconsider a margin only against REPEATED runs showing a real regression
/// (N>=3 per arm), and re-measure the whole LSQB set: both attempts here lost
/// more elsewhere than they could possibly have saved.
fn search_ordering(
    graph: &Graph,
    kept: &[PathPattern],
    seed_var: &str,
    seed_rows: f64,
) -> Option<(f64, f64, Vec<PathPattern>)> {
    let mut memo: BTreeMap<FanoutKey, f64> = BTreeMap::new();
    let mut best: Option<(f64, f64, Vec<PathPattern>)> = None;
    let mut order: Vec<usize> = Vec::with_capacity(kept.len());
    let mut taken = vec![false; kept.len()];
    search_step(
        graph,
        kept,
        seed_var,
        seed_rows,
        &mut memo,
        &mut taken,
        &mut order,
        &mut best,
    );
    best
}

#[allow(clippy::too_many_arguments)]
fn search_step(
    graph: &Graph,
    kept: &[PathPattern],
    seed_var: &str,
    seed_rows: f64,
    memo: &mut BTreeMap<FanoutKey, f64>,
    taken: &mut [bool],
    order: &mut Vec<usize>,
    best: &mut Option<(f64, f64, Vec<PathPattern>)>,
) {
    if order.len() == kept.len() {
        let Some((peak, total, paths)) =
            score_ordering(graph, kept, seed_var, seed_rows, memo, order)
        else {
            return;
        };
        let better = match best {
            None => true,
            Some((bp, bt, _)) => match peak.total_cmp(bp) {
                std::cmp::Ordering::Less => true,
                std::cmp::Ordering::Greater => false,
                std::cmp::Ordering::Equal => total.total_cmp(bt).is_lt(),
            },
        };
        if better {
            *best = Some((peak, total, paths));
        }
        return;
    }
    for i in 0..kept.len() {
        if taken[i] {
            continue;
        }
        taken[i] = true;
        order.push(i);
        search_step(graph, kept, seed_var, seed_rows, memo, taken, order, best);
        order.pop();
        taken[i] = false;
    }
}

/// Score one ordering: `(peak, total, oriented paths)`, or `None` when a path
/// in it never attaches to the bound set — a cartesian factor this pass will
/// not introduce.
fn score_ordering(
    graph: &Graph,
    kept: &[PathPattern],
    seed_var: &str,
    seed_rows: f64,
    memo: &mut BTreeMap<FanoutKey, f64>,
    order: &[usize],
) -> Option<(f64, f64, Vec<PathPattern>)> {
    let mut bound: Vec<String> = vec![seed_var.to_string()];
    let mut rows = seed_rows;
    let mut peak = rows;
    let mut out: Vec<PathPattern> = Vec::with_capacity(order.len());
    for &i in order {
        let p = &kept[i];
        let at = |v: Option<&str>| v.and_then(|v| bound.iter().position(|b| b == v));
        let si = at(p.start.var.as_deref());
        let ei = at(p.hops.last().and_then(|(_, n)| n.var.as_deref()));
        // The SAME orientation rule the greedy uses: a path bound only at its
        // END is reversed; one bound at BOTH starts at the LATER-bound endpoint
        // so its closing hop runs from the deeper var onto the shallower one.
        let reversed = match (si, ei) {
            (Some(s), Some(e)) => e > s,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (None, None) => return None,
        };
        let oriented = if reversed { reverse_path(p) } else { p.clone() };
        // A path whose INTERIOR var an earlier path already bound is a mid-chain
        // self-join. `collect_hops` refuses it in as many words — "Only a
        // path's FINAL hop may CLOSE onto it … A NON-final bound close (a
        // mid-chain self-join) is not a tractable expand chain — decline" — so
        // the whole pipeline would decline and the general path would answer.
        //
        // The greedy avoids this by construction, taking a both-ends-bound path
        // the moment it becomes one. A free search has to rule it out
        // explicitly, or it can "win" with an ordering the recogniser rejects
        // and the pass silently buys a SLOWER plan than the one it replaced.
        let last = oriented.hops.len().saturating_sub(1);
        for (hi, (_, node)) in oriented.hops.iter().enumerate() {
            if hi == last {
                break;
            }
            if node
                .var
                .as_deref()
                .is_some_and(|v| bound.iter().any(|b| b == v))
            {
                return None;
            }
        }
        let mut cur: &[String] = &oriented.start.labels;
        for (rel, node) in &oriented.hops {
            let dir = match rel.dir {
                RelDir::Out => Dir::Out,
                RelDir::In => Dir::In,
                RelDir::Undirected => Dir::Both,
            };
            let key: FanoutKey = (
                cur.to_vec(),
                match dir {
                    Dir::Out => 0u8,
                    Dir::In => 1,
                    Dir::Both => 2,
                },
                rel.types.clone(),
                node.labels.clone(),
            );
            let fanout = match memo.get(&key) {
                Some(f) => *f,
                None => {
                    let f = graph.hop_fanout(cur, dir, &rel.types, &node.labels);
                    memo.insert(key, f);
                    f
                }
            };
            // A hop onto an ALREADY-BOUND var is a semijoin: the expected count
            // of this node's matching edges that land on that ONE node, not on
            // any node of the label.
            let bound_target = node
                .var
                .as_deref()
                .is_some_and(|v| bound.iter().any(|b| b == v));
            let factor = if bound_target {
                let space = label_set_count(graph, &node.labels).max(1) as f64;
                (fanout / space).min(fanout)
            } else {
                fanout
            };
            // A variable-length hop cannot reach here: `reorder_pattern`
            // declines any `rel.length` before the search runs, so no
            // `*min..max` sum is needed.
            rows *= factor;
            peak = peak.max(rows);
            cur = &node.labels;
        }
        for v in std::iter::once(oriented.start.var.as_deref())
            .chain(oriented.hops.iter().map(|(_, n)| n.var.as_deref()))
            .flatten()
        {
            if !bound.iter().any(|b| b == v) {
                bound.push(v.to_string());
            }
        }
        out.push(oriented);
    }
    Some((peak, rows, out))
}

/// Nodes a label set bounds — the SMALLEST label's count (labels are an AND),
/// or every node when unlabelled. Mirrors `cardinality`'s `start_count`.
fn label_set_count(graph: &Graph, labels: &[String]) -> u64 {
    if labels.is_empty() {
        return graph.count_all_nodes();
    }
    labels
        .iter()
        .map(|l| graph.count_label_nodes(l))
        .min()
        .unwrap_or(0)
}

fn reorder_pattern(graph: &Graph, pattern: &Pattern) -> Option<Pattern> {
    if pattern.paths.is_empty() {
        return None;
    }
    // Refuse what the reversal and re-attachment do not model: a named path /
    // shortestPath, a variable-length hop (its reversal is not modelled, and the
    // chain recogniser declines one in a multi-path pattern anyway), and ANY
    // inline property map — moving a propertied node off the scan START would
    // silently give up the index seek `start_prop_anchor` seeds it with, turning
    // a point lookup into a label scan.
    for p in &pattern.paths {
        if p.var.is_some() || p.shortest || p.start.props.is_some() {
            return None;
        }
        for (rel, node) in &p.hops {
            if rel.length.is_some() || node.props.is_some() {
                return None;
            }
        }
    }
    // (1) LABEL STAMPING — every var's label union at every occurrence.
    let stamped = stamp_labels(pattern, &var_label_union(pattern));
    // (2) DROP a bare-node path whose var a path WITH HOPS also binds. A hopless
    // path is a plain label scan that `collect_hops` refuses outside an
    // OPTIONAL's outer, so one in the pattern declines the whole statement — and
    // it constrains nothing once its label is stamped on the var's other
    // occurrences and another path binds it. A bare path whose var NOTHING else
    // binds is a genuine cartesian factor: declined, never dropped.
    let mut kept: Vec<PathPattern> = Vec::with_capacity(stamped.paths.len());
    for p in &stamped.paths {
        if !p.hops.is_empty() {
            kept.push(p.clone());
            continue;
        }
        let v = p.start.var.as_deref()?;
        if !stamped
            .paths
            .iter()
            .any(|o| !o.hops.is_empty() && path_touches(o, v))
        {
            return None;
        }
    }
    if kept.is_empty() {
        return None;
    }
    // (3) SEED — the ENDPOINT var whose smallest label has the fewest nodes (an
    // endpoint, because a path attaches only at its start; a mid-chain var could
    // never be the scan root). Ties keep the FIRST occurrence, so the choice is a
    // function of the source text and not of a float-equal cost.
    //
    // SEARCHING over seed candidates was tried and REVERTED — see
    // `search_ordering`. It fixed LSQB q3 by 27% and cost 22-53% on four other
    // queries, and gating it behind a margin recovered neither.
    let mut seed: Option<(u64, String)> = None;
    for p in &kept {
        let mut ends: Vec<&NodePattern> = vec![&p.start];
        if let Some((_, n)) = p.hops.last() {
            ends.push(n);
        }
        for n in ends {
            let (Some(v), false) = (n.var.as_deref(), n.labels.is_empty()) else {
                continue; // an anonymous or unlabelled node cannot seed a scan
            };
            let c = n
                .labels
                .iter()
                .map(|l| graph.count_label_nodes(l))
                .min()
                .unwrap_or(u64::MAX);
            if seed.as_ref().is_none_or(|(best, _)| c < *best) {
                seed = Some((c, v.to_string()));
            }
        }
    }
    let (seed_rows, seed_var) = seed?;
    // (4) PATH ORDERING — the SEARCH first, and it wins outright when it finds
    // an ordering at all.
    //
    // This ran the other way round for two builds: greedy first, then the
    // search displacing it only past a margin. Both variants measured WORSE
    // than this one on LSQB at SF1 — q2 5,207 ms and q4 4,470 (N=3 medians,
    // back-to-back spread under 9%) against 2,807 and 1,735 here. I could not
    // derive why from the code, and the honest response to "my restructuring
    // is 1.85x slower and I cannot explain it" is to restore what measured
    // best, not to keep reasoning. The margin constant survives, unused by
    // this path, with the conditions under which it should be reconsidered.
    if order_peak_search_enabled() && kept.len() <= ORDER_SEARCH_MAX_PATHS {
        if let Some((peak, total, paths)) =
            search_ordering(graph, &kept, &seed_var, seed_rows as f64)
        {
            counted!("pipeline.ordering chosen by peak search");
            // `ENGRAM_TRACE_PLAN=1` dumps what the ordering search actually
            // decided. Reasoning about this from label counts has been wrong
            // twice; the estimator's own numbers are the only way to see which
            // ordering it picked and what it thought that ordering cost.
            if std::env::var_os("ENGRAM_TRACE_PLAN").is_some() {
                eprintln!(
                    "[plan] seed={seed_var} rows={seed_rows} paths={} peak={peak:.3e} total={total:.3e}",
                    kept.len()
                );
                for (i, pp) in paths.iter().enumerate() {
                    let mut d = pp.start.var.as_deref().unwrap_or("_").to_string();
                    for (rel, n) in &pp.hops {
                        d.push_str(&format!(
                            "-[{}]->{}",
                            rel.types.join("|"),
                            n.var.as_deref().unwrap_or("_")
                        ));
                    }
                    eprintln!("[plan]   {i}: {d}");
                }
            }
            return Some(Pattern { paths });
        }
    }
    // GREEDY PATH ORDERING — the fallback when the search finds nothing. Repeatedly take a path with a BOUND endpoint,
    // preferring one with BOTH ends bound — a pure close adds no rows, and
    // taking it later would leave its far end bound by an earlier path, which
    // `collect_hops` refuses as a mid-chain self-join. Among equals the cheapest
    // by `estimate_path_rows`, then the earliest in source order.
    //
    // ORIENTATION: a path with only its END bound is REVERSED so it starts
    // there; a path with BOTH ends bound is oriented to start at the
    // LATER-bound endpoint, so its closing hop runs from the deeper var onto the
    // shallower one — the direction the count fold's position rule admits.
    let mut bound: Vec<String> = vec![seed_var.clone()];
    let mut taken = vec![false; kept.len()];
    let mut out: Vec<PathPattern> = Vec::with_capacity(kept.len());
    // The greedy's own choice, as INDICES, so the search's candidate can be
    // compared against it on the same cost model rather than replacing it
    // unconditionally. See the margin below.
    let mut greedy_order: Vec<usize> = Vec::with_capacity(kept.len());
    // Whether the greedy produced a COMPLETE ordering. When it did not there is
    // nothing to compare the search against and nothing to fall back to.
    let mut greedy_ok = true;
    for _ in 0..kept.len() {
        let bound_set: BTreeSet<String> = bound.iter().cloned().collect();
        let mut best: Option<(bool, f64, usize, PathPattern)> = None;
        for (i, p) in kept.iter().enumerate() {
            if taken[i] {
                continue;
            }
            let at = |v: Option<&str>| v.and_then(|v| bound.iter().position(|b| b == v));
            let si = at(p.start.var.as_deref());
            let ei = at(p.hops.last().and_then(|(_, n)| n.var.as_deref()));
            let (both, reversed) = match (si, ei) {
                (Some(s), Some(e)) => (true, e > s),
                (Some(_), None) => (false, false),
                (None, Some(_)) => (false, true),
                (None, None) => continue, // not yet connected to the bound set
            };
            let oriented = if reversed { reverse_path(p) } else { p.clone() };
            let cost = graph.estimate_path_rows(&oriented, &bound_set);
            // Only a STRICT improvement displaces the incumbent, and candidates
            // are visited in source order, so a tie keeps the EARLIER path —
            // never a float-equal comparison's whim.
            let better = match &best {
                None => true,
                Some((bb, bc, _, _)) => match both.cmp(bb).reverse() {
                    std::cmp::Ordering::Less => true,
                    std::cmp::Ordering::Greater => false,
                    std::cmp::Ordering::Equal => cost.total_cmp(bc).is_lt(),
                },
            };
            if better {
                best = Some((both, cost, i, oriented));
            }
        }
        // Nothing attaches: a disjoint path this pass will not cartesian-join.
        // The GREEDY gives up here — but the search may still find an ordering,
        // so record the failure and let (5) decide rather than returning from
        // the whole pass. Returning here is what the first cut of this did, and
        // it silently made the search unreachable for exactly the patterns the
        // greedy cannot order — the ones it is most needed for.
        let Some((_, _, bi, oriented)) = best else {
            greedy_ok = false;
            break;
        };
        taken[bi] = true;
        greedy_order.push(bi);
        for v in std::iter::once(oriented.start.var.as_deref())
            .chain(oriented.hops.iter().map(|(_, n)| n.var.as_deref()))
            .flatten()
        {
            if !bound.iter().any(|b| b == v) {
                bound.push(v.to_string());
            }
        }
        out.push(oriented);
    }
    if !greedy_ok {
        return None; // no ordering from either — the general path answers
    }
    Some(Pattern { paths: out })
}

/// How many of a recognised count-only plan's variables MATERIALISE — its
/// binding order minus the vars a folded expand binds. `None` = the pipeline
/// declines the statement outright, which is the worst outcome of all (the
/// enumerating general path answers it).
fn materialised_var_count(q: &SingleQuery) -> Option<usize> {
    let plan = recognise_aggregate(q)?;
    let folded = plan
        .hops
        .iter()
        .filter(|h| h.fold && h.end_vi.is_some())
        .count();
    Some(plan.vars.len() - folded)
}

/// Rewrite a `count(*)`-only MATCH so its pattern is walked from its most
/// selective end as one connected chain, instead of in whatever order it was
/// written.
///
/// # Why this is free to do
///
/// Every accepted statement returns ONE row whose only content is how many
/// matches the pattern has ([`count_star_only_return`]). So production order,
/// group order and row count are all fixed no matter how the pattern is walked,
/// and the rewrite can change only the PLAN. The three rewrites it makes are
/// each semantics-preserving on their own terms: restating a var's label union
/// at every occurrence adds no constraint (labels are conjunctive wherever
/// written); dropping a bare-node path whose var another path binds removes a
/// factor of exactly one row; and reversing a path walks the SAME relationships
/// in the opposite order, which relationship isomorphism cannot tell apart (a
/// walk is legal iff its rel SET is pairwise distinct — an order-free property),
/// and re-ordering the comma paths cannot either, since this engine re-seeds the
/// isomorphism base at every path boundary.
///
/// # Why it is worth doing
///
/// LSQB q3 opens with a hopless `(country:Country)`, which `collect_hops`
/// refuses, so the whole four-MATCH pattern fell to the enumerating general
/// path — measured at SF1 as an 80 GiB allocation for one number. q2's paths
/// are written so the connecting path binds its far end LAST, which leaves the
/// count fold a close onto a sibling branch and materialises it.
///
/// # When it is taken
///
/// Only when the rewrite MATERIALISES STRICTLY FEWER VARIABLES than the source
/// order does — that is the cost proxy, and it is the honest one here: every
/// materialised var is an id column the chunk carries per row, so folding one
/// more of them can only shrink the intermediate. A pattern the recognisers
/// already claim with the same number of materialised columns keeps its source
/// order, so no shape that plans well today can regress; a pattern they decline
/// outright (`None`) is improved by any recognised rewrite.
///
/// The comparison RE-RUNS the recogniser, which is a pure AST pass — the graph
/// is read only for label counts and hop fan-outs, here, exactly as
/// [`reroot_to_selective_end`] does, so the recognisers stay pure.
///
/// # Termination
///
/// The pass is idempotent — its output's seed is its first var, its paths are
/// already oriented at their bound end, and the greedy re-derives the same order
/// from the same costs — and a rewrite EQUAL to its input returns `None`, so
/// re-planning the rewritten query cannot rewrite again.
fn reorder_for_count_only(graph: &Graph, q: &SingleQuery) -> Option<SingleQuery> {
    if !count_only_reorder_enabled() {
        return None;
    }
    let [
        Clause::Match {
            optional: false,
            pattern,
            where_,
        },
        Clause::Return { proj },
    ] = q.clauses.as_slice()
    else {
        return None;
    };
    if !count_star_only_return(proj) {
        return None;
    }
    let reordered = reorder_pattern(graph, pattern)?;
    if reordered == *pattern {
        return None; // nothing to do — and the pass's termination guarantee
    }
    let rewritten = SingleQuery {
        clauses: vec![
            Clause::Match {
                optional: false,
                pattern: reordered,
                where_: where_.clone(),
            },
            Clause::Return { proj: proj.clone() },
        ],
    };
    let after = materialised_var_count(&rewritten)?;
    // `Some(before)` and no improvement — keep the source order. (`None` before
    // is the declined statement, which any recognised rewrite improves on.)
    if materialised_var_count(q).is_some_and(|before| after >= before) {
        return None;
    }
    counted!("pipeline.count-only reordered");
    Some(rewritten)
}

/// Whether every projected item is a literal or a parameter — an expression
/// that reads NOTHING a row binds and cannot differ between two evaluations
/// (a function call is refused even when pure: `rand()` and `timestamp()`
/// would replicate one draw across every row).
fn constant_only_items(proj: &Projection) -> bool {
    !proj.items.is_empty()
        && proj.items.iter().all(|it| {
            matches!(
                it.expr,
                Expr::Null
                    | Expr::Bool(_)
                    | Expr::Int(_)
                    | Expr::Float(_)
                    | Expr::Str(_)
                    | Expr::Param(_)
            )
        })
}

/// Answer `MATCH <pattern> [WHERE] RETURN <literals/params> [SKIP s] [LIMIT k]`
/// through the count fold: `n` = the pattern's match count, then the one
/// constant row replayed `min(n − s, k)` times.
///
/// # Why this is free to do
///
/// The general path emits one row PER MATCH and every one of those rows is the
/// same constant row (the items read no variable). So production order — the
/// property every other projecting statement must preserve, and the reason the
/// count-only reorder admits nothing but `count(*)` — is unobservable here: the
/// output is fixed by HOW MANY matches there are, and SKIP/LIMIT then cut a run
/// of identical rows. That count is exactly what `reorder_for_count_only` and
/// the fold compute, over a pattern walked from its selective end.
///
/// # Why it is worth doing
///
/// An existence probe — `… RETURN 1 LIMIT 1`, the shape `lsqb` derives from
/// every count and the shape an `EXISTS`-style check takes — declined every
/// recogniser and fell to the enumerating general path, which walks the
/// pattern AS WRITTEN. LSQB q3 written as four MATCH clauses re-roots
/// `person2` and `person3` from `country` before any KNOWS close: cubic in the
/// persons per country. Measured: SF0.1 2 s, SF1 180 s (v69 and v71 alike,
/// fresh or warm), against a 4.5 s count of the same pattern. `lsqb` reports a
/// count's millis alone, so that probe hid in every battery for three days as
/// "q3 4.5 s" and surfaced as a hang.
///
/// # Byte-identity
///
/// The replay `UNWIND range(1, r) AS __i RETURN <items>` runs the ORIGINAL
/// projection items through the general path's own projector, so column names
/// (alias, else the captured source text) and value conversion are the general
/// path's, not a re-render. `r` is computed here from `n`, SKIP and LIMIT so the
/// replay carries neither; a row count above the row budget is declined to the
/// general path, whose refusal is then the answer.
///
/// # Termination
///
/// The count statement built here projects `count(*)`, which
/// [`constant_only_items`] refuses, so re-planning it cannot re-enter this pass.
fn constant_projection_over_count(
    graph: &Graph,
    q: &SingleQuery,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    if !graph.const_projection_fold() {
        return Ok(None);
    }
    // `MATCH … [OPTIONAL MATCH …]* RETURN <constants>`: one leading MATCH,
    // any number of OPTIONAL MATCH clauses after it, then the projection.
    // The OPTIONAL clauses ride along because the count-only pipeline's
    // optional fold (operator D) emits exactly one row per outer row with
    // weight `max(1, matches)` — the general path's row count to the row —
    // and because the shape is what `lsqb` derives for q7: at SF3 its
    // general-path enumeration materialises the HAS_TAG × LIKES fan-out
    // past the 20M-row budget BEFORE `LIMIT 1` can stop it, so the probe
    // errored where the count (through the fold) answered in 6 s.
    let Some((Clause::Return { proj }, matches)) = q.clauses.split_last() else {
        return Ok(None);
    };
    let Some((
        Clause::Match {
            optional: false,
            pattern,
            where_,
        },
        optionals,
    )) = matches.split_first()
    else {
        return Ok(None);
    };
    if !optionals
        .iter()
        .all(|c| matches!(c, Clause::Match { optional: true, .. }))
    {
        return Ok(None);
    }
    if proj.star || proj.distinct || !proj.order.is_empty() || !constant_only_items(proj) {
        return Ok(None);
    }
    // SKIP/LIMIT must be integer constants/params — `eval_count` refuses
    // anything else, and a refusal here is the general path's refusal too.
    let skip = eval_count(graph, proj.skip.as_ref(), params, "SKIP")?.unwrap_or(0);
    let limit = eval_count(graph, proj.limit.as_ref(), params, "LIMIT")?;
    let mut count_clauses = Vec::with_capacity(optionals.len() + 2);
    count_clauses.push(Clause::Match {
        optional: false,
        pattern: pattern.clone(),
        where_: where_.clone(),
    });
    count_clauses.extend(optionals.iter().cloned());
    count_clauses.push(Clause::Return {
        proj: Projection {
            distinct: false,
            star: false,
            items: vec![ProjItem::synthetic(
                Expr::Call {
                    name: "count".to_string(),
                    distinct: false,
                    args: vec![],
                    star: true,
                },
                Some("__c".to_string()),
            )],
            order: vec![],
            skip: None,
            limit: None,
        },
    });
    let count_q = SingleQuery {
        clauses: count_clauses,
    };
    // The count through the pipeline ONLY: a pattern the fold declines is left
    // to the general path with the original statement, never counted by
    // enumeration and then enumerated again.
    //
    // With a LIMIT the count is CAPPED at `skip + limit`: the fold stops once
    // its kept weights reach that, so an existence probe walks only as far as
    // its first matches instead of the whole relation set — which, measured
    // on the pod, is what a full-fold probe for q1/q6/q7/q9 costs the NEXT
    // count of q3: +12% from the block cache it displaced. A capped total is
    // any value ≥ `skip + limit`, and `r` below clamps it to `limit`.
    let capped_params;
    let count_params = match limit {
        Some(k) => {
            let mut p = params.clone();
            p.insert(
                COUNT_CAP_PARAM.to_string(),
                Value::Int(i64::try_from(skip.saturating_add(k)).unwrap_or(i64::MAX)),
            );
            capped_params = p;
            &capped_params
        }
        None => params,
    };
    let Some(counted) = plan_and_run_columnar(graph, &count_q, count_params)? else {
        return Ok(None);
    };
    let n = match counted.rows.first().and_then(|r| r.first()) {
        Some(Value::Int(n)) => usize::try_from(*n).unwrap_or(0),
        _ => return Ok(None),
    };
    let mut r = n.saturating_sub(skip);
    if let Some(k) = limit {
        r = r.min(k);
    }
    // `budget_check` is the general path's own row budget; a replay that would
    // exceed it is declined so that path's refusal stands.
    if budget_check(graph, r).is_err() {
        return Ok(None);
    }
    counted!("pipeline.constant projection answered from the count");
    let replay = SingleQuery {
        clauses: vec![
            Clause::Unwind {
                expr: Expr::Call {
                    name: "range".to_string(),
                    distinct: false,
                    args: vec![Expr::Int(1), Expr::Int(r as i64)],
                    star: false,
                },
                alias: "__i".to_string(),
            },
            Clause::Return {
                proj: Projection {
                    distinct: false,
                    star: false,
                    items: proj.items.clone(),
                    order: vec![],
                    skip: None,
                    limit: None,
                },
            },
        ],
    };
    crate::interp::run_single(graph, &replay, params, vec![VarMap::new()]).map(Some)
}

/// Recognise `MATCH <chain1> [WHERE] WITH [DISTINCT] <var> MATCH <chainA> [WHERE]
/// MATCH <chainB> [WHERE] RETURN <group-by aggregate> ORDER BY <total> [LIMIT]` —
/// the full IC5 statement. The clause shape ([Match, With, Match, Match, Return])
/// is DISJOINT from every other recognizer (multistage is 4 clauses; the join
/// has no WITH; OPTIONAL's 2nd Match is `optional`).
///
/// It reuses `recognise_multistage`'s STAGE-1 + WITH validation verbatim, then
/// `recognise_join`'s chainA/chainB chain recognition, shared-var join key,
/// combined binding order, `aggregate_over_chain` tail and `join_tail_order_safe`
/// gate — the ONLY differences from the plain join are (1) chainA re-roots at the
/// carried var (prebound = the carried var) instead of a fresh labelled scan, and
/// (2) exactly ONE carried var is allowed.
///
/// ACCEPT only when: the WITH is the pass-through/DISTINCT-of-one-carried-var form
/// rev 113 accepts (no post-WITH WHERE / `*` / ORDER BY / SKIP / LIMIT / rename /
/// computed expr / aggregate), stage-1's var-length (if any) is BFS-eligible and
/// DISTINCT-consumed by the WITH, chainA starts at the carried var and chainB
/// re-roots at a chainA var sharing >=1 var of matching kind, and the RETURN is
/// an order-safe aggregate. DECLINE (byte-identical fallback → the single-stage
/// multistage, the join, or the nested general path answers identically):
/// anything a sub-recognizer declines, a multi-var carry, a var-length hop in the
/// stage-2 join, no shared var, or a non-order-safe / non-aggregate RETURN.
fn recognise_multistage_join(sq: &SingleQuery) -> Option<MultiStageJoinPlan> {
    let (p1, w1, wp, with_where, pa, wa, pb, wb, rp) = match sq.clauses.as_slice() {
        [
            Clause::Match {
                optional: false,
                pattern: p1,
                where_: w1,
            },
            Clause::With {
                proj: wp,
                where_: with_where,
            },
            Clause::Match {
                optional: false,
                pattern: pa,
                where_: wa,
            },
            Clause::Match {
                optional: false,
                pattern: pb,
                where_: wb,
            },
            Clause::Return { proj: rp },
        ] => (
            p1,
            w1.as_ref(),
            wp,
            with_where.as_ref(),
            pa,
            wa.as_ref(),
            pb,
            wb.as_ref(),
            rp,
        ),
        _ => return None,
    };

    // ─── STAGE 1 + WITH — the SAME validation as `recognise_multistage` ─────────
    // The WITH carries ONLY bound pattern variables: no post-WITH WHERE, no `*`,
    // no ORDER BY / SKIP / LIMIT, at least one item.
    if with_where.is_some()
        || wp.star
        || !wp.order.is_empty()
        || wp.skip.is_some()
        || wp.limit.is_some()
        || wp.items.is_empty()
    {
        return None;
    }
    // Stage 1: the read chain (labels retained for the stage-2 restate check).
    // `allow_start_anchor` = the INLINE `(person:Person {id: val})` start anchor.
    let hc1 = collect_hops(p1, None, false, true, false)?;
    // DESUGAR the inline start anchor into a source-var equality `a.prop = val`
    // and AND it into the stage-1 WHERE, so the single split below carries BOTH
    // the anchor and any textual predicate (`person <> friend`). The seekable form
    // is then picked out via `prop_eq_index` (interp's own detector) for the
    // index-anchored scan; the equality still runs as a filter (byte-identical).
    let anchor_eq: Option<Expr> = hc1.start_anchor.as_ref().map(|(prop, val)| {
        Expr::Bin(
            engram_cypher::ast::BinOp::Eq,
            Box::new(Expr::Prop(
                Box::new(Expr::Var(hc1.a_var.clone())),
                prop.clone(),
            )),
            Box::new(val.clone()),
        )
    });
    let combined_where: Option<Expr> = match (anchor_eq, w1.cloned()) {
        (Some(a), Some(w)) => Some(Expr::And(Box::new(a), Box::new(w))),
        (Some(a), None) => Some(a),
        (None, w) => w,
    };
    // Split the (possibly conjunctive) stage-1 WHERE into per-predicate filters —
    // the LITERAL IC5's `person.id = X AND person <> friend`. DECLINE (whole query
    // to the general path) if any conjunct is neither a single-var prop pred nor a
    // two-var id pred.
    let s1_wheres = recognise_where_preds(combined_where.as_ref(), &hc1.vars)?;
    // The seekable source anchor for the scan seed, if the combined WHERE carries a
    // `a.prop = <var-free>` equality on the scan var (interp's `Seed::PropEq`).
    let s1_anchor = prop_eq_index(combined_where.as_ref(), &hc1.a_var)
        .map(|(prop, values)| PropAnchor { prop, values });
    // A stage-1 FRONTIER-BFS var-length hop (the IC5 shape) must be consumed
    // DISTINCT-only by the WITH breaker — the SAME `varlen_distinct_consumed` gate.
    if !varlen_distinct_consumed(&hc1.hops, wp) {
        return None;
    }
    // Validate the WITH projection: every item a BARE bound pattern var or `v AS v`
    // (same name). DECLINE a rename, a computed expr, an aggregate, an unbound
    // name, or a duplicate carry — exactly as `recognise_multistage`.
    let mut carried_names: Vec<String> = Vec::with_capacity(wp.items.len());
    for it in &wp.items {
        if expr_has_aggregate(&it.expr) {
            return None;
        }
        let Expr::Var(v) = &it.expr else {
            return None;
        };
        if let Some(a) = &it.alias {
            if a != v {
                return None;
            }
        }
        if !hc1.vars.contains(v) {
            return None;
        }
        if carried_names.contains(v) {
            return None;
        }
        carried_names.push(v.clone());
    }
    // EXACTLY ONE carried var — it seeds chainA. Seeding chainA from a SINGLE
    // carried var's ids is byte-identical to the per-carried-TUPLE general path
    // ONLY when the carried tuple IS that one var; a second carried column would
    // let one seed-var value drive stage 2 more than once (once per distinct
    // tuple), which the single-var seed collapses. Multi-var carries DECLINE to
    // the single-stage multistage / general path (which handles them by carrying
    // the full tuple).
    if carried_names.len() != 1 {
        return None;
    }
    let carried: Vec<usize> = carried_names
        .iter()
        .map(|v| {
            hc1.vars
                .iter()
                .position(|x| x == v)
                .expect("carried var is bound in stage 1")
        })
        .collect();
    let carried_kinds: Vec<VarKind> = carried.iter().map(|&i| hc1.var_kinds[i]).collect();
    let mut carried_labels: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for v in &carried_names {
        if let Some(ls) = hc1.var_labels.get(v) {
            carried_labels.insert(v.clone(), ls.clone());
        }
    }

    // ─── STAGE-2 JOIN — chainA re-rooted at the CARRIED var; the REST is the join ─
    // ChainA re-roots at THE carried var (prebound = the carried var only). Its
    // start must therefore be that carried var; a chainA disconnected from the
    // carry declines (`collect_hops`). A var-length hop in the stage-2 join is out
    // of scope — decline.
    let prebound_a = (
        carried_names.as_slice(),
        carried_kinds.as_slice(),
        &carried_labels,
    );
    let hca = collect_hops(pa, Some(prebound_a), false, false, false)?;
    if hops_have_varlen(&hca.hops) {
        return None;
    }
    let a_where = recognise_single_var_where(wa, &hca.vars)?;

    // ChainB re-roots at a chainA var — the SAME wiring as `recognise_join`.
    // Prebinding ONLY chainB's start makes every OTHER shared var (e.g. `friend`)
    // a fresh EXPAND that introduces its own column, so side B enumerates the full
    // tuples the join needs and the hash join re-correlates on the shared vars.
    let b_start = pb.paths.first()?.start.var.as_deref()?;
    let b_seed_from_a = hca.vars.iter().position(|v| v == b_start)?;
    let seed_names = [b_start.to_string()];
    let seed_kinds = [hca.var_kinds[b_seed_from_a]];
    let mut seed_labels: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Some(ls) = hca.var_labels.get(b_start) {
        seed_labels.insert(b_start.to_string(), ls.clone());
    }
    let hcb = collect_hops(
        pb,
        Some((&seed_names, &seed_kinds, &seed_labels)),
        false,
        false,
        false,
    )?;
    if hops_have_varlen(&hcb.hops) {
        return None;
    }
    let b_where = recognise_single_var_where(wb, &hcb.vars)?;

    // ─── JOIN-ORDERING: re-root chainB at the CARRIED var when it is selective ──
    // ChainB (`(forum)-CONTAINER_OF->(post)-HAS_CREATOR->(friend)`) seeded from the
    // shared start (forum) materialises EVERY post of every member forum, then the
    // hash join discards the posts whose creator is not carried — the profiled
    // 158ms floor. But the carried var (`friend`) ALSO appears in chainB (its
    // terminal), so chainB can be re-rooted there:
    // `(friend)<-HAS_CREATOR-(post)<-CONTAINER_OF-(forum)`, seeded from the carried
    // set. That expands O(carried friends' posts) — each friend authored FEW posts,
    // each post has ONE container — instead of O(member-forum posts), while the join
    // on the shared vars `(friend, forum)` is unchanged, so the result multiset is
    // IDENTICAL (verified byte-for-byte by the differential). `collect_hops` itself
    // takes `path.start.var` as the seed and only expands FORWARD; the re-root is
    // done at the AST by REVERSING chainB's path (`reroot_single_path_at`, each hop
    // direction flipped) into a syntactically-valid pattern that `collect_hops`
    // then recognises normally via its existing prebound re-root — no new hop
    // machinery.
    //
    // COST RULE (reported): prefer the carried-var root whenever it is EXPRESSIBLE
    // (chainB is a single path whose TERMINAL is the carried var, and that var is
    // not already chainB's seed) AND stage 1 is ANCHORED — i.e. the carried set is
    // the bounded/selective input (`s1_anchor.is_some()`, always true for the
    // literal IC5 `(person {id:$id})`). Otherwise keep the forward forum-rooted
    // chainB (the byte-identical fallback). A pure |carried| vs |member-forums|
    // seed-size comparison is a poor proxy here — the cost is per-seed FAN-OUT
    // (posts per friend << posts per forum), not seed count — so the selective
    // carried set is preferred on the anchored shape regardless of its size.
    let carried_var = carried_names[0].as_str();
    let mut hcb = hcb;
    let mut b_where = b_where;
    let mut b_seed_var = b_start.to_string();
    let mut b_seed_from_a = b_seed_from_a;
    let mut b_seed_from_carried = false;
    if s1_anchor.is_some() && carried_var != b_start && hcb.vars.iter().any(|v| v == carried_var) {
        if let Some(alt_pb) = reroot_single_path_at(pb, carried_var) {
            let alt_seed_names = [carried_var.to_string()];
            let alt_seed_kinds = [carried_kinds[0]];
            let mut alt_seed_labels: BTreeMap<String, Vec<String>> = BTreeMap::new();
            if let Some(ls) = carried_labels.get(carried_var) {
                alt_seed_labels.insert(carried_var.to_string(), ls.clone());
            }
            if let Some(hcb_alt) = collect_hops(
                &alt_pb,
                Some((&alt_seed_names, &alt_seed_kinds, &alt_seed_labels)),
                false,
                false,
                false,
            ) {
                // The reversed chain must be varlen-free, expose the SAME var SET as
                // the forward one (reversal preserves the nodes, so the join key and
                // combined binding order are unchanged), and its WHERE must still be
                // tractable over its vars. Any miss keeps the forward root.
                let same_var_set = hcb_alt.vars.len() == hcb.vars.len()
                    && hcb_alt.vars.iter().all(|v| hcb.vars.contains(v));
                if !hops_have_varlen(&hcb_alt.hops) && same_var_set {
                    if let Some(alt_where) = recognise_single_var_where(wb, &hcb_alt.vars) {
                        b_where = alt_where;
                        hcb = hcb_alt;
                        b_seed_var = carried_var.to_string();
                        b_seed_from_a = 0; // unused when seeding from the carried set
                        b_seed_from_carried = true;
                    }
                }
            }
        }
    }

    // THE JOIN KEY = the vars bound in BOTH chains (>=1, matching kinds) — the SAME
    // check as `recognise_join`. No shared var is a cartesian product (decline).
    let mut shared_count = 0usize;
    for (ai, av) in hca.vars.iter().enumerate() {
        if let Some(bi) = hcb.vars.iter().position(|bv| bv == av) {
            if hca.var_kinds[ai] != hcb.var_kinds[bi] {
                return None;
            }
            shared_count += 1;
        }
    }
    if shared_count == 0 {
        return None;
    }

    // COMBINED binding order = chainA's vars, then chainB's NON-shared vars — the
    // exact order `hash_join_chunks` builds, so the tail's var indices align.
    let mut combined_vars = hca.vars.clone();
    let mut combined_kinds = hca.var_kinds.clone();
    for (bi, bv) in hcb.vars.iter().enumerate() {
        if !hca.vars.iter().any(|av| av == bv) {
            combined_vars.push(bv.clone());
            combined_kinds.push(hcb.var_kinds[bi]);
        }
    }
    let combined_chain = Chain {
        a_labels: Vec::new(),
        a_var: String::new(),
        hops: Vec::new(),
        vars: combined_vars,
        var_kinds: combined_kinds,
        wheres: Vec::new(),
        start_anchor: None,
    };

    // THE TAIL must be an order-insensitive AGGREGATE with a total order (or a
    // global aggregate) — reused verbatim from the join.
    let tail = aggregate_over_chain(combined_chain, rp, None)?;
    if !join_tail_order_safe(&tail) {
        return None;
    }

    Some(MultiStageJoinPlan {
        s1_a_labels: hc1.a_labels,
        s1_a_var: hc1.a_var,
        s1_hops: hc1.hops,
        s1_wheres,
        s1_anchor,
        carried,
        distinct: wp.distinct,
        a_seed_var: carried_names[0].clone(),
        a_hops: hca.hops,
        a_where,
        b_seed_from_a,
        b_seed_var,
        b_hops: hcb.hops,
        b_where,
        b_seed_from_carried,
        tail,
    })
}

/// Run a recognised full-IC5 composite: STAGE 1 (scan + expand + WHERE) → the
/// WITH boundary (project to the carried var, DISTINCT dedup, rel-iso reset) →
/// SIDE A (chainA) SEEDED FROM THE CARRIED SET → SIDE B (chainB) seeded from side
/// A's DISTINCT shared-start ids → HASH JOIN → the order-safe aggregate tail
/// (stamped `finish_multistage_join`). Byte-identical to the nested `run_streaming`
/// over the whole statement on the accepted shapes, or `Ok(None)` (a budget /
/// column decline; the general path answers identically).
fn run_multistage_join(
    graph: &Graph,
    plan: &MultiStageJoinPlan,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    counted!("interp.dispatch run_multistage_join");
    // The frontier-BFS toggle gate (stage 1 is where a var-length hop lives) —
    // decline the columnar BFS when `run_streaming` would not take it, so ON == OFF.
    if hops_have_varlen(&plan.s1_hops) && !graph.frontier_expand_enabled() {
        return Ok(None);
    }
    // STAGE 1: scan + expand/semijoin + the stage-1 WHERE.
    let Some(s1_chunk) = build_chunk(
        graph,
        &plan.s1_a_labels,
        &plan.s1_a_var,
        &plan.s1_hops,
        &plan.s1_wheres,
        plan.s1_anchor.as_ref(),
        params,
    )?
    else {
        return Ok(None); // a filter budget / non-boolean decline
    };

    // THE WITH BOUNDARY: project to the SINGLE carried var, dedup first-seen
    // (DISTINCT), and reset relationship isomorphism — the carried chunk's one
    // column is chainA's seed.
    let carried_chunk = s1_chunk.project_carried(graph, &plan.carried, plan.distinct)?;

    // SIDE A (chainA) SEEDED FROM THE CARRIED SET — the load-bearing difference
    // from `run_join`: chainA scans ONLY the stage-1-reached carried ids, not a
    // fresh whole-label scan (the canary seeds a full label scan here and
    // over-counts). A repeated seed (a non-DISTINCT carry) multiplies chainA rows
    // exactly as the nested general path re-runs stage 2 per carried row — the
    // hash join preserves that multiplicity. A FRESH clause (empty `used_rels`):
    // chainA's relationship isomorphism is scoped to chainA alone.
    let seed_a: Vec<u64> = carried_chunk
        .selection
        .iter()
        .map(|&r| carried_chunk.ids[0][r])
        .collect();
    let Some(chunk_a) = build_chunk_from_ids(
        graph,
        &plan.a_seed_var,
        seed_a,
        &[],
        &plan.a_hops,
        where_slice(&plan.a_where),
        params,
    )?
    else {
        return Ok(None); // a filter budget / non-boolean decline
    };

    // SIDE B seed — a FRESH clause (empty `used_rels`), always a DISTINCT set so
    // chainB contributes NO multiplicity (the join's row multiplicity comes from
    // chainA alone). Two roots, chosen at plan time:
    //   - JOIN-ORDERING re-root (`b_seed_from_carried`): the DISTINCT carried set
    //     (chainB re-rooted at the carried var, expanding O(carried's posts)); the
    //     shared start (forum) then re-binds as a fresh chainB column and the hash
    //     join re-correlates on `(friend, forum)` exactly as before.
    //   - forward (`run_join`'s wiring): side A's DISTINCT shared-start ids.
    let seed_ids: Vec<u64> = if plan.b_seed_from_carried {
        counted!("interp.pipeline join rerooted from carried");
        let mut seed: BTreeSet<u64> = BTreeSet::new();
        for &r in &carried_chunk.selection {
            seed.insert(carried_chunk.ids[0][r]);
        }
        seed.into_iter().collect()
    } else {
        let sidx = plan.b_seed_from_a;
        let mut seed: BTreeSet<u64> = BTreeSet::new();
        for &r in &chunk_a.selection {
            seed.insert(chunk_a.ids[sidx][r]);
        }
        seed.into_iter().collect()
    };
    let Some(chunk_b) = build_chunk_from_ids(
        graph,
        &plan.b_seed_var,
        seed_ids,
        &[],
        &plan.b_hops,
        where_slice(&plan.b_where),
        params,
    )?
    else {
        return Ok(None); // a filter budget / non-boolean decline
    };

    // HASH JOIN on the shared vars → the combined chunk in `plan.tail`'s order,
    // then the EXISTING group-by/aggregate + ORDER BY / LIMIT tail.
    let combined = hash_join_chunks(graph, &chunk_a, &chunk_b)?;
    run_aggregate_over_chunk(graph, &plan.tail, params, &combined, finish_multistage_join)
}

// ─── FULL 7-CLAUSE LDBC IC5 — collect + correlated OPTIONAL left-join ──────────
//
// The REAL IC5 — NOT the 5-clause two-MATCH-join stand-in `recognise_multistage_
// join` accepts:
//   MATCH (person:Person {id})-[:KNOWS*1..2]-(friend) WHERE NOT person = friend
//   WITH DISTINCT friend
//   MATCH (friend)<-[m:HAS_MEMBER]-(forum) WHERE m.joinDate > X   -- chainA (+ rel filter)
//   WITH forum, collect(friend) AS friends                        -- names `friends`
//   OPTIONAL MATCH (friend)<-[:HAS_CREATOR]-(post)<-[:CONTAINER_OF]-(forum)
//     WHERE friend IN friends                                     -- correlated left join
//   WITH forum, count(post) AS postCount                          -- second aggregate
//   RETURN forum.title AS forumName, postCount ORDER BY postCount DESC, forum.id ASC LIMIT k
//
// It is byte-identical to the two-MATCH-join stand-in EXCEPT for: (a) the joinDate
// REL filter; (b) OPTIONAL — a forum with member-friends but no qualifying post is
// still emitted with postCount 0; (c) `RETURN forum.title`. The plan reuses the
// SAME fast join order as `run_multistage_join`: stage 1 (varlen BFS + WITH
// DISTINCT, anchored/seeded) → chainA seeded from the carried set → DEDUP to
// distinct `(friend, forum)` member pairs → chainB (`friend → post → forum`)
// seeded from the DISTINCT friends so each friend's posts expand ONCE → HASH JOIN
// on `(friend, forum)` → a LEFT-JOIN ZERO-FILL (one null-`post` row per member
// forum) → the group-by-forum `count(post)` tail. Two things are load-bearing: the
// DEDUP (`collect`'s `friend IN friends` counts each post ONCE by set membership,
// so distinct pairs make the count immune to duplicate HAS_MEMBER edges), and the
// zero-fill (the OPTIONAL keeps zero-count forums the inner hash join would drop).
// The `friend IN friends` correlation is VALIDATED (needle = the carried member
// friend, haystack = the collect alias) and DROPPED as vacuous — the outer friend
// is already a member, so the post's creator (= that friend) satisfies it.

/// A recognised full 7-clause IC5. Stage-1 fields mirror `MultiStageJoinPlan`;
/// `pair_cols` dedup chainA to distinct `(friend, forum)` member pairs; `b_hops`
/// are chainB (`friend → post → forum`, seeded from the DISTINCT friends) whose
/// `(friend, forum)` HASH-JOINS the member pairs; `forum_combined_idx` is forum's
/// column in the joined chunk (for the LEFT-JOIN zero-fill); `tail` is the
/// group-by-forum `count(post)` aggregate + the RETURN's ORDER BY / LIMIT.
struct IC5Plan {
    s1_a_labels: Vec<String>,
    s1_a_var: String,
    s1_hops: Vec<Hop>,
    s1_wheres: Vec<WherePred>,
    s1_anchor: Option<PropAnchor>,
    carried: Vec<usize>,
    distinct: bool,
    a_seed_var: String,
    a_hops: Vec<Hop>,
    a_where: Option<WherePred>,
    pair_cols: [usize; 2],
    b_seed_var: String,
    b_hops: Vec<Hop>,
    forum_combined_idx: usize,
    tail: AggPlan,
}

fn recognise_ic5(sq: &SingleQuery) -> Option<IC5Plan> {
    let (p1, w1, wp1, w1p, pa, wa, wp2, w2p, pc, wc, wp3, w3p, rp) = match sq.clauses.as_slice() {
        [
            Clause::Match {
                optional: false,
                pattern: p1,
                where_: w1,
            },
            Clause::With {
                proj: wp1,
                where_: w1p,
            },
            Clause::Match {
                optional: false,
                pattern: pa,
                where_: wa,
            },
            Clause::With {
                proj: wp2,
                where_: w2p,
            },
            Clause::Match {
                optional: true,
                pattern: pc,
                where_: wc,
            },
            Clause::With {
                proj: wp3,
                where_: w3p,
            },
            Clause::Return { proj: rp },
        ] => (
            p1,
            w1,
            wp1,
            w1p,
            pa,
            wa.as_ref(),
            wp2,
            w2p,
            pc,
            wc,
            wp3,
            w3p,
            rp,
        ),
        _ => return None,
    };

    // ─── STAGE 1 + WITH DISTINCT — the SAME validation as recognise_multistage_join
    if w1p.is_some()
        || wp1.star
        || !wp1.order.is_empty()
        || wp1.skip.is_some()
        || wp1.limit.is_some()
        || wp1.items.len() != 1
    {
        return None;
    }
    let hc1 = collect_hops(p1, None, false, true, false)?;
    let anchor_eq: Option<Expr> = hc1.start_anchor.as_ref().map(|(prop, val)| {
        Expr::Bin(
            engram_cypher::ast::BinOp::Eq,
            Box::new(Expr::Prop(
                Box::new(Expr::Var(hc1.a_var.clone())),
                prop.clone(),
            )),
            Box::new(val.clone()),
        )
    });
    let combined_where: Option<Expr> = match (anchor_eq, w1.clone()) {
        (Some(a), Some(w)) => Some(Expr::And(Box::new(a), Box::new(w))),
        (Some(a), None) => Some(a),
        (None, w) => w,
    };
    let s1_wheres = recognise_where_preds(combined_where.as_ref(), &hc1.vars)?;
    let s1_anchor = prop_eq_index(combined_where.as_ref(), &hc1.a_var)
        .map(|(prop, values)| PropAnchor { prop, values });
    if !varlen_distinct_consumed(&hc1.hops, wp1) {
        return None;
    }
    // The single carried var — `friend` — a bare bound pattern var (or `v AS v`).
    let it1 = &wp1.items[0];
    if expr_has_aggregate(&it1.expr) {
        return None;
    }
    let Expr::Var(friend_var) = &it1.expr else {
        return None;
    };
    if let Some(a) = &it1.alias {
        if a != friend_var {
            return None;
        }
    }
    let friend_idx = hc1.vars.iter().position(|v| v == friend_var)?;
    let carried = vec![friend_idx];
    let carried_names = [friend_var.clone()];
    let carried_kinds = [hc1.var_kinds[friend_idx]];
    let mut carried_labels: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Some(ls) = hc1.var_labels.get(friend_var) {
        carried_labels.insert(friend_var.clone(), ls.clone());
    }

    // ─── chainA: (friend)<-[m:HAS_MEMBER]-(forum) WHERE m.joinDate > X ──────────
    let hca = collect_hops(
        pa,
        Some((&carried_names, &carried_kinds, &carried_labels)),
        false,
        false,
        false,
    )?;
    if hops_have_varlen(&hca.hops) {
        return None;
    }
    let a_where = recognise_single_var_where(wa, &hca.vars)?;
    // The forum var = the single NEW Node var chainA introduces (not the carried
    // friend). The rel var (`membership`) is Rel-kind and excluded.
    let forum_positions: Vec<usize> = hca
        .vars
        .iter()
        .enumerate()
        .filter(|(i, v)| hca.var_kinds[*i] == VarKind::Node && *v != friend_var)
        .map(|(i, _)| i)
        .collect();
    if forum_positions.len() != 1 {
        return None;
    }
    let forum_col = forum_positions[0];
    let forum_var = hca.vars[forum_col].clone();
    let friend_col = hca.vars.iter().position(|v| v == friend_var)?;

    // ─── collect WITH: `forum, collect(friend) AS friends` (names `friends`) ────
    if w2p.is_some()
        || wp2.star
        || wp2.distinct
        || !wp2.order.is_empty()
        || wp2.skip.is_some()
        || wp2.limit.is_some()
        || wp2.items.len() != 2
    {
        return None;
    }
    let mut friends_alias: Option<String> = None;
    let mut saw_forum_key = false;
    for it in &wp2.items {
        match &it.expr {
            Expr::Var(v) if v == &forum_var => {
                if let Some(a) = &it.alias {
                    if a != v {
                        return None;
                    }
                }
                saw_forum_key = true;
            }
            Expr::Call { name, args, .. } if name == "collect" && args.len() == 1 => {
                let Expr::Var(cv) = &args[0] else {
                    return None;
                };
                if cv != friend_var {
                    return None;
                }
                let Some(a) = &it.alias else {
                    return None;
                };
                friends_alias = Some(a.clone());
            }
            _ => return None,
        }
    }
    if !saw_forum_key {
        return None;
    }
    let friends_alias = friends_alias?;

    // ─── chainB: (friend)<-[:HAS_CREATOR]-(post)<-[:CONTAINER_OF]-(forum), prebound
    //     FRIEND ONLY so `forum` is a FRESH EXPAND (not a semijoin). Seeded from the
    //     DISTINCT friends it expands each friend's posts ONCE and re-derives their
    //     container forum, and the (friend, forum) HASH JOIN with the member pairs
    //     re-correlates — the fast order (vs. re-expanding a friend's posts once per
    //     (friend, forum) pair). `friend IN friends` is validated + DROPPED as
    //     vacuous (the outer friend is already a member; the post's creator = it).
    let b_prebound_names = [friend_var.clone()];
    let b_prebound_kinds = [hc1.var_kinds[friend_idx]];
    let mut b_prebound_labels: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Some(ls) = hc1.var_labels.get(friend_var) {
        b_prebound_labels.insert(friend_var.clone(), ls.clone());
    }
    let hcb = collect_hops(
        pc,
        Some((&b_prebound_names, &b_prebound_kinds, &b_prebound_labels)),
        false,
        false,
        false,
    )?;
    if hops_have_varlen(&hcb.hops) {
        return None;
    }
    // chainB must EXPAND the same `forum` (the join key, shared with chainA) and
    // introduce exactly one other new var — `post`.
    if !hcb.vars.iter().any(|v| v == &forum_var) {
        return None;
    }
    let post_positions: Vec<usize> = hcb
        .vars
        .iter()
        .enumerate()
        .filter(|(_, v)| *v != friend_var && *v != &forum_var)
        .map(|(i, _)| i)
        .collect();
    if post_positions.len() != 1 {
        return None;
    }
    let post_col = post_positions[0];
    let post_var = hcb.vars[post_col].clone();
    // The correlated WHERE must be EXACTLY `friend IN friends` (needle = carried
    // friend, haystack = the collect alias). Only then is dropping it byte-identical
    // — the outer friend is already a member of the forum. Anything else declines.
    let wc_expr = wc.as_ref()?;
    match wc_expr {
        Expr::In(l, r) => {
            let (Expr::Var(lv), Expr::Var(rv)) = (l.as_ref(), r.as_ref()) else {
                return None;
            };
            if lv != friend_var || rv != &friends_alias {
                return None;
            }
        }
        _ => return None,
    }

    // ─── tail over the JOINED chunk `[friend, forum, post]` — chainA's deduped
    //     (friend, forum) then chainB's non-shared `post` (the `hash_join_chunks`
    //     order): group by forum, count(post), then the RETURN's ORDER BY / LIMIT.
    let combined_vars = vec![friend_var.clone(), forum_var.clone(), post_var];
    let combined_var_kinds = vec![
        hc1.var_kinds[friend_idx],
        hca.var_kinds[forum_col],
        hcb.var_kinds[post_col],
    ];
    let combined_chain = Chain {
        a_labels: Vec::new(),
        a_var: String::new(),
        hops: Vec::new(),
        vars: combined_vars,
        var_kinds: combined_var_kinds,
        wheres: Vec::new(),
        start_anchor: None,
    };
    let tail = aggregate_over_chain(combined_chain, wp3, Some((w3p.as_ref(), rp)))?;
    if !join_tail_order_safe(&tail) {
        return None;
    }

    Some(IC5Plan {
        s1_a_labels: hc1.a_labels,
        s1_a_var: hc1.a_var,
        s1_hops: hc1.hops,
        s1_wheres,
        s1_anchor,
        carried,
        distinct: wp1.distinct,
        a_seed_var: friend_var.clone(),
        a_hops: hca.hops,
        a_where,
        pair_cols: [friend_col, forum_col],
        b_seed_var: friend_var.clone(),
        b_hops: hcb.hops,
        // dedup_a is `[friend, forum]`, a prefix of the joined chunk, so forum sits
        // at index 1 in BOTH — used to seed the zero-fill from chainA's forums.
        forum_combined_idx: 1,
        tail,
    })
}

/// Record that the full 7-clause IC5 pipeline produced the answer.
fn finish_ic5(result: QueryResult) -> Result<Option<QueryResult>, RunError> {
    counted!("interp.pipeline ic5 runs");
    Ok(Some(result))
}

/// Run a recognised full IC5: STAGE 1 (scan + varlen BFS + WHERE) → WITH DISTINCT
/// (project to the carried friend) → chainA seeded from the carried set
/// (`(friend)<-[m:HAS_MEMBER]-(forum)` + the joinDate rel filter) → DEDUP to
/// distinct `(friend, forum)` member pairs → chainB (`friend → post → forum`)
/// seeded from the DISTINCT friends → HASH JOIN on `(friend, forum)` → LEFT-JOIN
/// ZERO-FILL (a null-`post` row per member forum, so a forum with no qualifying
/// post groups with count 0) → group-by-forum `count(post)` + ORDER BY / LIMIT
/// tail. Byte-identical to the nested `run_streaming`, or `Ok(None)` (a budget /
/// column decline; the general path answers identically).
fn run_ic5(
    graph: &Graph,
    plan: &IC5Plan,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    if hops_have_varlen(&plan.s1_hops) && !graph.frontier_expand_enabled() {
        return Ok(None);
    }
    // STAGE 1 + the WITH-DISTINCT carry (the single carried friend).
    let Some(s1_chunk) = build_chunk(
        graph,
        &plan.s1_a_labels,
        &plan.s1_a_var,
        &plan.s1_hops,
        &plan.s1_wheres,
        plan.s1_anchor.as_ref(),
        params,
    )?
    else {
        return Ok(None);
    };
    let carried_chunk = s1_chunk.project_carried(graph, &plan.carried, plan.distinct)?;

    // chainA seeded from the carried set (NOT a fresh label scan): the carried
    // reach restricts the member pairs, exactly as the general path's stage 2.
    let seed_a: Vec<u64> = carried_chunk
        .selection
        .iter()
        .map(|&r| carried_chunk.ids[0][r])
        .collect();
    let Some(chunk_a) = build_chunk_from_ids(
        graph,
        &plan.a_seed_var,
        seed_a,
        &[],
        &plan.a_hops,
        where_slice(&plan.a_where),
        params,
    )?
    else {
        return Ok(None);
    };

    // DEDUP to distinct `(friend, forum)` member pairs. Distinct pairs make the
    // count immune to duplicate HAS_MEMBER edges, matching `collect`'s set-
    // membership `friend IN friends` (each post counted once).
    let dedup_a = chunk_a.project_carried(graph, &plan.pair_cols, true)?;

    // chainB seeded from the DISTINCT carried friends: `friend → post → forum`
    // triples (each friend's posts expanded ONCE — the selective seed side).
    let seed_b: Vec<u64> = {
        let mut s: BTreeSet<u64> = BTreeSet::new();
        for &r in &carried_chunk.selection {
            s.insert(carried_chunk.ids[0][r]);
        }
        s.into_iter().collect()
    };
    let Some(chunk_b) = build_chunk_from_ids(
        graph,
        &plan.b_seed_var,
        seed_b,
        &[],
        &plan.b_hops,
        &[],
        params,
    )?
    else {
        return Ok(None);
    };

    // HASH JOIN the member pairs ⋈ chainB triples on the shared `(friend, forum)`
    // → the matched `[friend, forum, post]` rows.
    let mut combined = hash_join_chunks(graph, &dedup_a, &chunk_b)?;

    // LEFT-JOIN ZERO-FILL — the OPTIONAL's zero-count forums. Every candidate forum
    // (distinct chainA member forum) gets ONE null-`post` row so it forms a group
    // with count 0 when no member-friend posted there. `count(post)` skips NULL, so
    // matched forums are unaffected. `forum_combined_idx` is forum's column, which
    // — dedup_a being a prefix of the joined chunk — is identical in both.
    {
        let gv = plan.forum_combined_idx;
        let mut seen: BTreeSet<u64> = BTreeSet::new();
        for &r in &dedup_a.selection {
            let forum_id = dedup_a.ids[gv][r];
            if seen.insert(forum_id) {
                for (ci, col) in combined.ids.iter_mut().enumerate() {
                    col.push(if ci == gv { forum_id } else { NULL_ID });
                }
            }
        }
        let n = combined.ids.first().map_or(0, Vec::len);
        combined.selection = (0..n).collect();
        combined.used_rels = vec![Vec::new(); n];
    }

    // group-by-forum count(post) + ORDER BY postCount DESC, forum.id ASC LIMIT.
    run_aggregate_over_chunk(graph, &plan.tail, params, &combined, finish_ic5)
}
