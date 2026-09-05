//! R8 — the columnar aggregate scan.
//!
//! `MATCH (n[:L…]) [WHERE p] {RETURN | WITH … RETURN} <aggregates>[, keys]`
//! with no hops and no joins: the statement class that was still 5–14 s on
//! the production port (`IS NOT NULL` counts, label-OR `count(m)`, every
//! `n.x, count(*)` histogram, and the census shape
//! `WITH count(e), count(CASE WHEN exists((e)-[:T]->(:L)) THEN 1 END)`).
//! The row-at-a-time path materialised a node value per row, bound it into
//! a row, evaluated the WHERE and folded. Here the demanded property
//! COLUMNS are read once as id-sorted vectors, walked in alignment with the
//! label's membership, predicates and aggregate arguments are evaluated
//! over locals bound straight from the columns, and the aggregates fold
//! straight from the columns. No `Value::Node`, no props map, no `Row`.
//!
//! ONE evaluator. Every expression over `n` is REWRITTEN — `n.p` → a local
//! `__col_p`, `n:L` → a boolean local, `exists((n)-[:T]->(:L))` → a boolean
//! local answered by the adjacency table — and handed to the same
//! `eval_with` every other path uses, so an operator here means exactly
//! what it means everywhere. An expression that reads `n` any other way
//! declines the whole statement to the general path.

use std::collections::BTreeMap;

use engram_cypher::ast::{BinOp, Expr};
use engram_cypher::bindings::VarMap;
use engram_cypher::eval::{Scope, eval_with, is_aggregate_fn};
use engram_cypher::stmt::{
    Clause, NodePattern, OrderItem, PathPattern, Pattern, Projection, RelDir, SingleQuery,
};
use engram_cypher::{Truth, Value};
use engram_observe::{counted, sometimes};

use crate::interp::{
    AggSite, QueryResult, Row, RunError, SiteAcc, agg_key_of, best_declared_seek, budget_check,
    cmp_order_keys, column_name, conjunct_count, contains_opaque, eval_count, free_vars_of,
    prop_eq_candidates, seek_candidates,
};
use crate::{ColumnFamily, Dir, Graph, PropColumn};

/// One item of the aggregating projection, in the rewritten grammar.
enum Item {
    /// A grouping key — any rewritten expression (`n.k`, `n.k % 7`, …).
    Key(Expr),
    /// An aggregate, with its rewritten argument if not star.
    Agg(AggSite, Option<Expr>),
}

/// An `exists((n)-[:T…]->(:L…))` probe the rewrite lifted into a local.
struct Probe {
    local: String,
    dir: Dir,
    types: Vec<String>,
    labels: Vec<String>,
    /// The far end's inline property map (`(:Country {iso3: $a})`), every
    /// value variable-free, when it carries one. Resolved ONCE per walk into
    /// the sorted ids of `labels` satisfying it (`Walk::probe_ends`), and the
    /// per-member probe then asks whether any typed neighbour is in that
    /// set. Until this existed such a probe declined the whole columnar
    /// stage, and the general path ran the pattern matcher per row: the
    /// production `exists((g)-[:OCCURS_IN|…]->(:Country {iso3: $a}))` over
    /// 44k GeopoliticalEvent took 20.3 s against Neo4j's 40 ms.
    end_filter: Option<Expr>,
}

/// What the rewrite collects while it walks expressions over the scanned
/// variable.
#[derive(Default)]
struct Reads {
    props: Vec<String>,
    labels: Vec<String>,
    probes: Vec<Probe>,
    /// `type(r)` was read — bound per relationship from its type token.
    type_read: bool,
    /// Properties read ONLY as `IS [NOT] NULL` — presence, never a value.
    presence: Vec<String>,
    /// `id(var)` was read — bound per member from its own id (fix 46),
    /// never a record read.
    id_read: bool,
    /// `count{(n)-[:T…]-()}` probes — a degree per member from the
    /// adjacency table.
    degrees: Vec<DegreeProbe>,
    /// The local-name tag: empty for a single-variable scan, `a.` / `r.` /
    /// `b.` for the ends and relationship of a hop.
    tag: String,
}

impl Reads {
    /// Whether the items read a property the predicate never touches —
    /// neither its value nor its presence. Only then can a second phase
    /// save anything: that column is read over the survivors alone.
    fn has_column_beyond(&self, pred: &Reads) -> bool {
        self.props
            .iter()
            .any(|p| !pred.props.contains(p) && !pred.presence.contains(p))
    }

    /// Fold `other`'s reads into these (one walk binds both).
    fn merge(&mut self, other: Reads) {
        self.id_read |= other.id_read;
        for p in other.props {
            if !self.props.contains(&p) {
                self.props.push(p);
            }
        }
        for l in other.labels {
            if !self.labels.contains(&l) {
                self.labels.push(l);
            }
        }
        for p in other.presence {
            if !self.presence.contains(&p) {
                self.presence.push(p);
            }
        }
        for pr in other.probes {
            if !self.probes.iter().any(|q| q.local == pr.local) {
                self.probes.push(pr);
            }
        }
        for d in other.degrees {
            if !self.degrees.iter().any(|q| q.local == d.local) {
                self.degrees.push(d);
            }
        }
        self.type_read |= other.type_read;
    }

    fn tagged(tag: &str) -> Reads {
        Reads {
            tag: tag.to_string(),
            ..Reads::default()
        }
    }
}

/// A degree probe the rewrite lifted into a local.
struct DegreeProbe {
    local: String,
    dir: Dir,
    types: Vec<String>,
}

/// The local `type(r)` rewrites to.
const LOCAL_TYPE: &str = "__type";

/// What kind of thing the scanned variable is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Node,
    Rel,
}

/// What the scan walks: a node population by label, or a relationship
/// population by type — `MATCH ()-[r:T…]->() … RETURN <aggregates over
/// r.props>`, the five SUPPLIES histograms at 6.6–6.9 s on the production
/// port (the general path expanded every node to reach every relationship
/// and decoded each one in full).
enum Source {
    /// Nodes carrying every label in `labels`; when `labels` is empty and
    /// the WHERE implies the node carries one of `any_of`, the population
    /// is that union instead of every node.
    Nodes {
        labels: Vec<String>,
        any_of: Vec<String>,
    },
    Rels {
        types: Vec<String>,
    },
}

/// The labels a satisfying node must carry ONE of, read off the WHERE:
/// `n:A` → {A} (any one label of a conjunction is a superset);
/// `a AND b` → either side's set; `a OR b` → both sides' union, or none
/// if either side implies nothing. Everything else implies nothing.
fn implied_labels(e: &Expr, var: &str) -> Option<Vec<String>> {
    match e {
        Expr::HasLabels { of, labels } => match of.as_ref() {
            Expr::Var(v) if v == var && !labels.is_empty() => Some(vec![labels[0].clone()]),
            _ => None,
        },
        Expr::And(a, b) => implied_labels(a, var).or_else(|| implied_labels(b, var)),
        Expr::Or(a, b) => {
            let mut l = implied_labels(a, var)?;
            for x in implied_labels(b, var)? {
                if !l.contains(&x) {
                    l.push(x);
                }
            }
            Some(l)
        }
        _ => None,
    }
}

impl Reads {
    fn note_prop(&mut self, p: &str) {
        if !self.props.iter().any(|x| x == p) {
            self.props.push(p.to_string());
        }
    }
    fn note_degree(&mut self, dir: Dir, types: &[String]) -> String {
        if let Some(d) = self
            .degrees
            .iter()
            .find(|d| d.dir == dir && d.types == types)
        {
            return d.local.clone();
        }
        let local = format!("__deg_{}{}", self.tag, self.degrees.len());
        self.degrees.push(DegreeProbe {
            local: local.clone(),
            dir,
            types: types.to_vec(),
        });
        local
    }
    fn note_presence(&mut self, p: &str) {
        if !self.presence.iter().any(|x| x == p) {
            self.presence.push(p.to_string());
        }
    }
    /// The presence-only properties: read as `IS [NOT] NULL` and never for
    /// a value (a value read serves the null test too).
    fn presence_only(&self) -> Vec<String> {
        self.presence
            .iter()
            .filter(|p| !self.props.contains(p))
            .cloned()
            .collect()
    }
    fn note_label(&mut self, l: &str) {
        if !self.labels.iter().any(|x| x == l) {
            self.labels.push(l.to_string());
        }
    }
    fn note_probe(
        &mut self,
        dir: Dir,
        types: &[String],
        labels: &[String],
        end_filter: Option<&Expr>,
    ) -> String {
        if let Some(p) = self.probes.iter().find(|p| {
            p.dir == dir && p.types == types && p.labels == labels && p.end_filter.as_ref() == end_filter
        }) {
            return p.local.clone();
        }
        let local = format!("__ex_{}{}", self.tag, self.probes.len());
        self.probes.push(Probe {
            local: local.clone(),
            dir,
            types: types.to_vec(),
            labels: labels.to_vec(),
            end_filter: end_filter.cloned(),
        });
        local
    }
}

/// The final projection when an aggregating WITH precedes the RETURN:
/// expressions over the WITH's aliases, with its own ORDER/SKIP/LIMIT.
struct Final {
    items: Vec<Expr>,
    columns: Vec<String>,
    order: Vec<OrderItem>,
    skip: Option<Expr>,
    limit: Option<Expr>,
}

/// The statement, recognised — or `None` for any shape outside the class.
struct Plan {
    source: Source,
    pred: Option<Expr>,
    reads: Reads,
    items: Vec<Item>,
    columns: Vec<String>,
    /// ORDER BY of the aggregating projection, over its own columns.
    order: Vec<(usize, bool)>,
    /// Every `var.prop = x` (x variable-free) the WHERE carries — the
    /// candidates a derived range index can SEEK instead of scanning the
    /// label; `columnar_seek_ids` picks among them at run time.
    seeks: Vec<(String, Vec<Expr>)>,
    /// Every `var.prop STARTS WITH x` (x variable-free) the WHERE carries —
    /// a PREFIX a declared index seeks as a range (`columnar_seek_ids`).
    prefixes: Vec<(String, Expr)>,
    /// Every `var.prop < / <= / > / >= x` (x variable-free) the WHERE
    /// carries — a RANGE a declared index seeks (fix 47).
    ranges: Vec<(String, engram_cypher::BinOp, Expr)>,
    /// Whether `seeks` IS the whole predicate — every conjunct of the WHERE
    /// (the pattern map's equalities included) is one of them. Then a count
    /// over them can be answered from the indexes alone; see `covered_count`.
    covered: bool,
    skip: Option<Expr>,
    limit: Option<Expr>,
    final_: Option<Final>,
}

/// Whether an aggregate plan can be answered by COUNTING an index intersection
/// without reading a record: no reads of any kind, and every item a `count(*)`
/// (`count(n)` folds into one in `aggregating_items` — a matched node is never
/// null), no DISTINCT, no grouping key.
fn covered_count_applies(plan: &Plan) -> bool {
    let r = &plan.reads;
    // The predicate's rewrite registered the keys it compares as column
    // reads; covered means those ARE the seek keys, and nothing else is read.
    let only_seek_keys = r
        .props
        .iter()
        .all(|p| {
            plan.seeks.iter().any(|(k, _)| k == p)
                || plan.prefixes.iter().any(|(k, _)| k == p)
                || plan.ranges.iter().any(|(k, _, _)| k == p)
        });
    if !(only_seek_keys
        && r.labels.is_empty()
        && r.probes.is_empty()
        && r.degrees.is_empty()
        && r.presence_only().is_empty()
        && !r.type_read)
    {
        return false;
    }
    count_star_only(&plan.items)
}

/// Whether every item is a plain `count(*)` (`count(n)` folds into one in
/// `aggregating_items` — a matched node is never null): no DISTINCT, no
/// grouping key, nothing else.
fn count_star_only(items: &[Item]) -> bool {
    !items.is_empty()
        && items.iter().all(|it| {
            matches!(it, Item::Agg(site, None) if site.star && site.name == "count" && !site.distinct)
        })
}

/// The columns a set of reads takes over ONE label, ALIGNED to the label's
/// members and served from the property-column cache: the value columns
/// as the cache's kept aligned vectors (`Graph::prop_column_aligned` —
/// aligned once per column and kept, no value copied per statement; `align`
/// per statement copied every value of every column read: 44k lists for
/// `$a IN coalesce(g.affectedCountries, [])`, 17.8 ms against Neo4j's 6.9),
/// the presence-only columns built over the members. `None` when a value
/// column is not cached — the walk assembles and keeps it, and the next
/// read comes here.
struct AlignedColumns {
    values: Vec<(String, std::sync::Arc<Vec<Value>>)>,
    presence: Vec<(String, Vec<Value>)>,
}

impl AlignedColumns {
    /// A view of positions `[lo, hi)` of every column — what `eval_column`
    /// reads, as slices.
    fn view(&self, lo: usize, hi: usize) -> crate::vectorized::ColView<'_> {
        let mut view: crate::vectorized::ColView<'_> = BTreeMap::new();
        for (k, c) in &self.values {
            view.insert(k.clone(), &c[lo..hi]);
        }
        for (k, c) in &self.presence {
            view.insert(k.clone(), &c[lo..hi]);
        }
        view
    }
}

fn aligned_columns(
    graph: &Graph,
    label: &str,
    reads: &Reads,
    members: &[u64],
) -> Option<AlignedColumns> {
    let mut values: Vec<(String, std::sync::Arc<Vec<Value>>)> =
        Vec::with_capacity(reads.props.len());
    for p in &reads.props {
        let col = graph.prop_column_aligned(label, p, members)?;
        values.push((local_for_prop(&reads.tag, p), col));
    }
    let mut presence: Vec<(String, Vec<Value>)> = Vec::new();
    for p in reads.presence_only() {
        // A property nothing ever wrote is absent on every member (see
        // `Graph::prop_column_aligned`): a presence column of Nulls.
        if graph.prop_token_peek(&p).is_none() {
            counted!("graph.property column absent everywhere");
            presence.push((local_for_prop(&reads.tag, &p), vec![Value::Null; members.len()]));
            continue;
        }
        let PropColumn::Presence(ids) = graph.prop_column(label, &p, true)? else {
            return None;
        };
        // What the walk binds for a presence-only local: `true` where the
        // property is present, Null where it is not.
        let mut out = Vec::with_capacity(members.len());
        let mut ci = 0usize;
        for &id in members {
            while ci < ids.len() && ids[ci] < id {
                ci += 1;
            }
            out.push(if ci < ids.len() && ids[ci] == id {
                Value::Bool(true)
            } else {
                Value::Null
            });
        }
        presence.push((local_for_prop(&reads.tag, &p), out));
    }
    Some(AlignedColumns { values, presence })
}

/// The members of the label's population satisfying `pred`, by POSITION in
/// `members` and in id order, evaluated column-at-a-time over the cached
/// aligned columns in chunks of `PRED_CHUNK` — stopping at `cap` survivors
/// when the statement has one (a bare LIMIT), so a listing that keeps its
/// first five matches costs the chunks up to the fifth, as Neo4j's
/// pipelined scan does. `None` when a column is not cached, the predicate
/// is a form `eval_column` declines, or it answers a non-boolean (the
/// per-member walk raises that error). Fix 40: the columnar projection
/// bound a scope and walked the predicate per member — `MATCH
/// (s:NewsStory) WHERE s.primaryTopic = $t AND s.status <> 'stale' AND
/// s.lastUpdatedAt > $cutoff … LIMIT 5` evaluated ~20k members at ~1 µs
/// each with every column served from the cache (90–183 ms on the mirror
/// against Neo4j's 3–4), where the same predicate's count over the same
/// columns ran column-at-a-time in a third of the time per member.
const PRED_CHUNK: usize = 4096;

fn survivors_over_cached_columns(
    graph: &Graph,
    label: &str,
    pred: &Expr,
    reads: &Reads,
    members: &[u64],
    cap: Option<usize>,
    scope: &Scope<'_>,
) -> Option<Vec<usize>> {
    let cols = aligned_columns(graph, label, reads, members)?;
    let n = members.len();
    let mut hits: Vec<usize> = Vec::new();
    let mut lo = 0usize;
    while lo < n {
        let hi = n.min(lo + PRED_CHUNK);
        let view = cols.view(lo, hi);
        let truth = crate::vectorized::eval_column(pred, "", hi - lo, &view, scope)?;
        for (i, v) in truth.iter().enumerate() {
            match v.truth() {
                Some(Truth::True) => {
                    hits.push(lo + i);
                    if cap.is_some_and(|c| hits.len() >= c) {
                        return Some(hits);
                    }
                }
                Some(_) => {}
                None => return None,
            }
        }
        lo = hi;
    }
    Some(hits)
}

/// Fix 70: a `COUNT { … }` / `EXISTS { … }` body that is ONE typed, directed
/// hop from a BOUND node to an unbound end carrying at most one label and
/// no map, whose WHERE reads only that end's properties, evaluated
/// column-at-a-time: the end ids from the adjacency table (kept to the
/// label's members), each demanded property gathered from the label's
/// CACHED column by binary search, the predicate over those vectors — no
/// scope bound and no expression walked per neighbour. The KMProject
/// dashboard evaluates eight `COUNT { (x:KMWorkItem)-[:BELONGS_TO_PROJECT]
/// ->(p) WHERE coalesce(x.status, 'backlog') = '…' }` per project row over
/// ~15k items: the `dash` decomposition on the mirror priced ONE such count
/// at 9–16 ms (about a microsecond per item visited, Neo4j's 0.07) and the
/// eight at 75 ms of the statement's 149 (Neo4j 22). `None` — the general
/// matcher — for any richer shape, an uncached column, a predicate the
/// vectoriser declines, or a row answering a non-boolean (the matcher
/// raises); `Some(0)` for a named type never minted. `exists` stops at the
/// first True.
/// One cached column a vectorised subquery gathers from: its local name in
/// the rewritten predicate, the label's `(id, value)` column, and whether
/// the read is PRESENCE-only (`IS [NOT] NULL` — bound Bool/Null, not the value).
type GatheredColumn = (String, std::sync::Arc<Vec<(u64, Value)>>, bool);

pub(crate) fn count_hop_ends_vectorised(
    graph: &Graph,
    pattern: &Pattern,
    where_: Option<&Expr>,
    row: &VarMap,
    params: &BTreeMap<String, Value>,
    exists: bool,
) -> Result<Option<i64>, RunError> {
    if !graph.columnar_scans_enabled() || graph.in_txn_with_writes() || pattern.paths.len() != 1 {
        return Ok(None);
    }
    let path = &pattern.paths[0];
    if path.shortest || path.var.is_some() || path.hops.len() != 1 {
        return Ok(None);
    }
    let (rel, end) = &path.hops[0];
    if rel.var.is_some()
        || rel.props.is_some()
        || rel.length.is_some()
        || rel.types.is_empty()
        || rel.dir == RelDir::Undirected
    {
        return Ok(None);
    }
    let bound_node = |n: &NodePattern| -> Option<u64> {
        match n.var.as_ref().and_then(|v| row.get(v)) {
            Some(Value::Node { id, .. }) => Some(*id),
            _ => None,
        }
    };
    let bare = |n: &NodePattern| n.labels.is_empty() && n.props.is_none();
    let unbound_far = |n: &NodePattern| {
        n.props.is_none()
            && n.labels.len() <= 1
            && !n.var.as_ref().is_some_and(|v| row.contains_key(v))
    };
    let (from, far, dir) = match (bound_node(&path.start), bound_node(end)) {
        (Some(a), None) if bare(&path.start) && unbound_far(end) => {
            (a, end, if rel.dir == RelDir::Out { Dir::Out } else { Dir::In })
        }
        (None, Some(b)) if bare(end) && unbound_far(&path.start) => {
            (b, &path.start, if rel.dir == RelDir::Out { Dir::In } else { Dir::Out })
        }
        _ => return Ok(None),
    };
    let Some(tokens) = graph.type_tokens_peek(&rel.types) else {
        return Ok(None);
    };
    if tokens.is_empty() {
        counted!("interp.subquery hop evaluated column-at-a-time");
        return Ok(Some(0)); // a named type never minted has no edges
    }
    let tokens = Some(tokens);
    let label: Option<&String> = far.labels.first();
    let (rw, reads) = match where_ {
        None => (None, Reads::default()),
        Some(w) => {
            let Some(fv) = far.var.as_deref() else {
                return Ok(None);
            };
            if label.is_none() || contains_opaque(w) || !reads_only(w, std::slice::from_ref(&fv.to_string())) {
                return Ok(None);
            }
            let mut reads = Reads::default();
            let Some(rw) = rewrite(w, fv, Kind::Node, &mut reads) else {
                return Ok(None);
            };
            if !reads.labels.is_empty()
                || !reads.probes.is_empty()
                || !reads.degrees.is_empty()
                || reads.type_read
                || reads.id_read
            {
                return Ok(None);
            }
            (Some(rw), reads)
        }
    };
    // Every demanded column must be CACHED before any adjacency is read:
    // an uncached one hands the whole body back to the matcher.
    let mut columns: Vec<GatheredColumn> = Vec::new();
    if let Some(l) = label {
        for p in &reads.props {
            let Some(PropColumn::Values(col)) = graph.prop_column(l, p, false) else {
                return Ok(None);
            };
            columns.push((local_for_prop(&reads.tag, p), col, false));
        }
        for p in reads.presence_only() {
            let Some(PropColumn::Values(col)) = graph.prop_column(l, &p, false) else {
                return Ok(None);
            };
            columns.push((local_for_prop(&reads.tag, &p), col, true));
        }
    }
    let mut ids: Vec<u64> = Vec::new();
    graph.adjacent_slim_for_each(from, dir, &tokens, |e| ids.push(e.peer));
    if let Some(l) = label {
        let members = graph.members(Some(l))?;
        ids.retain(|id| graph.members_contains(&members, *id));
    }
    let Some(rw) = rw else {
        counted!("interp.subquery hop evaluated column-at-a-time");
        return Ok(Some(ids.len() as i64));
    };
    let mut cols: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for (local, col, presence) in &columns {
        let v: Vec<Value> = ids
            .iter()
            .map(|id| match col.binary_search_by_key(id, |(i, _)| *i) {
                Ok(at) if !matches!(col[at].1, Value::Null) => {
                    if *presence {
                        Value::Bool(true)
                    } else {
                        col[at].1.clone()
                    }
                }
                _ => Value::Null,
            })
            .collect();
        cols.insert(local.clone(), v);
    }
    let empty_vars = VarMap::new();
    let scope = Scope::over(params, &empty_vars, graph.wall_ms(), graph.zone_provider());
    let n = ids.len();
    let mut count = 0i64;
    let mut lo = 0usize;
    while lo < n {
        let hi = n.min(lo + PRED_CHUNK);
        let chunk: crate::vectorized::ColView<'_> =
            cols.iter().map(|(k, v)| (k.clone(), &v[lo..hi])).collect();
        let Some(truth) = crate::vectorized::eval_column(&rw, "", hi - lo, &chunk, &scope) else {
            return Ok(None);
        };
        for v in truth.iter() {
            match v.truth() {
                Some(Truth::True) => {
                    count += 1;
                    if exists {
                        counted!("interp.subquery hop evaluated column-at-a-time");
                        return Ok(Some(1));
                    }
                }
                Some(_) => {}
                None => return Ok(None),
            }
        }
        lo = hi;
    }
    counted!("interp.subquery hop evaluated column-at-a-time");
    Ok(Some(count))
}

/// A `count(*)` over ONE label whose predicate reads only CACHED columns,
/// answered COLUMN-AT-A-TIME: the cached columns are aligned to the members
/// once, the predicate is evaluated over them as vectors (`eval_column`),
/// and the count is the number of TRUE positions — no scope bound and no
/// expression walked per member. `None` when a column is not cached (the
/// walk assembles and keeps it, and the next read comes here) or the
/// predicate is a form `eval_column` declines (the walk answers).
///
/// With the columns cached, `MATCH (n:UserDataNode) WHERE n.nodeType =
/// 'email' AND n.classified = true RETURN count(n)` still bound a scope and
/// walked the predicate 38k times — 14 ms against Neo4j's 3.5 ms index
/// scan; the per-member `bind` + `eval_with` was the whole remaining gap on
/// every plain wide-label count. A non-boolean predicate value declines to
/// the walk, which raises the error the general path raises.
fn count_over_cached_columns(
    graph: &Graph,
    label: &str,
    plan: &Plan,
    scope: &Scope<'_>,
) -> Result<Option<usize>, RunError> {
    let members = graph
        .members_all(std::slice::from_ref(&label.to_string()))
        .map_err(RunError::Graph)?
        .to_arc_vec();
    let Some(cols) = aligned_columns(graph, label, &plan.reads, &members) else {
        return Ok(None);
    };
    let n = members.len();
    let Some(pred) = &plan.pred else {
        return Ok(Some(n));
    };
    let view = cols.view(0, n);
    let Some(truth) = crate::vectorized::eval_column(pred, "", n, &view, scope) else {
        return Ok(None);
    };
    let mut count = 0usize;
    for v in truth.iter() {
        match v.truth() {
            Some(Truth::True) => count += 1,
            Some(_) => {}
            None => return Ok(None), // a non-boolean: the walk raises it
        }
    }
    Ok(Some(count))
}

/// The number of `label`'s members satisfying EVERY seek equality, from the
/// DECLARED scoped indexes alone — `None` unless every equality is on a key
/// with an index declared for this label, every value is a STRING (an
/// Int/Float probe unions the cross-type bucket and needs the verifier), and
/// no transaction write is pending (an index is committed state). The probes
/// are intersected with each other and with the label's membership snapshot,
/// so a node whose label was removed since the index was built is not
/// counted — the case `label_change_and_the_unscoped_index` pins.
///
/// This is what a composite index buys Neo4j: `MATCH (n:UserDataNode
/// {nodeType: 'email', userId: $u}) RETURN count(n)` answered from index
/// entries in 4 ms, where the mirror read 18k records (1.2 s) because neither
/// key alone was selective enough to seek.
fn covered_count(
    graph: &Graph,
    label: &str,
    seeks: &[(String, Vec<Expr>)],
    prefixes: &[(String, Expr)],
    ranges: &[(String, engram_cypher::BinOp, Expr)],
    scope: &Scope,
) -> Result<Option<u64>, RunError> {
    if (seeks.is_empty() && prefixes.is_empty() && ranges.is_empty())
        || !graph.property_seek_enabled()
        || graph.in_txn_with_writes()
    {
        return Ok(None);
    }
    let labels = [label.to_string()];
    let mut acc: Option<Vec<u64>> = None;
    // A RANGE (`prop > x`, …) on a declared key is a range of the same index
    // (fix 47) — exact for string keys; a non-string bound declines.
    for (prop, op, e) in ranges {
        let Some(scoped) = graph.declared_scope_for(&labels, prop).map_err(RunError::Graph)? else {
            return Ok(None);
        };
        let v = eval_with(e, scope, None).map_err(RunError::Eval)?;
        let Some(ids) = graph
            .index_probe_range_scoped(prop, *op, &v, None, Some(&scoped))
            .map_err(RunError::Graph)?
        else {
            return Ok(None);
        };
        counted!("interp.columnar covered count sought a range");
        acc = Some(match acc {
            None => ids,
            Some(prev) => intersect_sorted(&prev, &ids),
        });
    }
    // A PREFIX (`prop STARTS WITH 'x'`) on a declared key is the range
    // `[x, next(x))` of the same index — exactly the members whose string
    // value starts with `x` (a non-string value is outside every string
    // range, as `STARTS WITH` answers null for it). `MATCH
    // (g:GeopoliticalEvent) WHERE g.eventId STARTS WITH 'edgar-8k-' RETURN
    // count(g)` walked 3.9k sought ids re-reading the key it had just
    // sought (7 ms against Neo4j's 1.4); the range's size is the answer.
    for (prop, e) in prefixes {
        let Some(scoped) = graph.declared_scope_for(&labels, prop).map_err(RunError::Graph)? else {
            return Ok(None);
        };
        let Value::Str(prefix) = eval_with(e, scope, None).map_err(RunError::Eval)? else {
            return Ok(None); // a non-string prefix answers null everywhere: the walk says so
        };
        let Some(ids) = graph
            .index_probe_prefix_scoped(prop, &prefix, None, Some(&scoped))
            .map_err(RunError::Graph)?
        else {
            return Ok(None); // an unbounded prefix (empty, or all 0xFF bytes)
        };
        counted!("interp.columnar covered count sought a prefix");
        acc = Some(match acc {
            None => ids,
            Some(prev) => intersect_sorted(&prev, &ids),
        });
    }
    for (prop, values) in seeks {
        let Some(scoped) = graph.declared_scope_for(&labels, prop).map_err(RunError::Graph)? else {
            return Ok(None);
        };
        let mut ids: Vec<u64> = Vec::new();
        for e in values {
            let v = eval_with(e, scope, None).map_err(RunError::Eval)?;
            if !matches!(v, Value::Str(_)) {
                return Ok(None);
            }
            match graph
                .index_probe_eq_scoped(prop, &v, None, Some(&scoped))
                .map_err(RunError::Graph)?
            {
                Some(found) => ids.extend(found),
                None => return Ok(None),
            }
        }
        ids.sort_unstable();
        ids.dedup();
        acc = Some(match acc {
            None => ids,
            Some(prev) => intersect_sorted(&prev, &ids),
        });
    }
    let Some(ids) = acc else {
        return Ok(None);
    };
    let members = graph.members_all(&labels).map_err(RunError::Graph)?;
    let n = ids
        .iter()
        .filter(|id| graph.members_contains(&members, **id))
        .count() as u64;
    counted!("interp.columnar covered count");
    Ok(Some(n))
}

/// The intersection of two ASCENDING id vectors, ascending.
fn intersect_sorted(a: &[u64], b: &[u64]) -> Vec<u64> {
    let mut out = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

/// The ids a property-equality SEEK answers for a scan over `labels`, or
/// `None` when the label scan wins (or nothing can be sought).
///
/// Every equality conjunct is a candidate, and two things decide between
/// them. A candidate with a DECLARED index on a label this pattern requires is
/// probed against that index, SCOPED — only the label's members, the index the
/// operator declared for exactly this shape. The first conjunct is also probed
/// through the partition-wide index when nothing is declared for it, which is
/// the seek as it was (built on first use; the arity that path already pays).
/// The candidate answering the FEWEST ids wins, and only if it beats the scan
/// by `property_seek_wins`' margin.
///
/// Before this the FIRST conjunct was probed, unscoped, and that was the whole
/// decision: `MATCH (n:UserDataNode {nodeType: 'email', userId: $u}) RETURN
/// count(n)` on the production mirror probed `nodeType` (18k of 38k ids, over
/// the cap), scanned the label, and never looked at the `userId` index the
/// catalogue had declared for it. Neo4j answered from its index in 4 ms.
/// How a seek's ids will be consumed — which decides how wide a seek is
/// still a win (`Graph::property_seek_wins_under`).
#[derive(Clone, Copy)]
enum SeekUse {
    /// One full node read per id: the default cap and selectivity.
    PerId,
    /// A WALK over the ids, its columns from the cache or one gather — about
    /// a column entry per id, so any real reduction of the label wins and a
    /// seek eight times wider is still taken.
    Walk,
}

/// The widest seek a walk over the sought ids takes.
const SEEK_WALK_CAP: usize = 8 * crate::PROPERTY_SEEK_MAX_PROBE;
/// A walk wins on halving the label.
const SEEK_WALK_SELECTIVITY: u64 = 2;

fn columnar_seek_ids(
    graph: &Graph,
    labels: &[String],
    seeks: &[(String, Vec<Expr>)],
    prefixes: &[(String, Expr)],
    ranges: &[(String, engram_cypher::BinOp, Expr)],
    use_: SeekUse,
    scope: &Scope,
) -> Result<Option<Vec<u64>>, RunError> {
    if (seeks.is_empty() && prefixes.is_empty() && ranges.is_empty())
        || labels.is_empty()
        || !graph.property_seek_enabled()
    {
        return Ok(None);
    }
    // Prefix and range candidates seek DECLARED keys only (an equality may
    // still probe an undeclared first conjunct). With no equality and no
    // declared key there is nothing to probe — and the label-size test
    // below would rebuild the stats on a cold store for nothing: a
    // whole-store pass charged to a two-node label's budgeted read
    // (`population_scan_interleaved_bare_on_a_paged_store_stops_fetching_at_the_budget`).
    if seeks.is_empty() {
        let mut declared = false;
        for prop in prefixes
            .iter()
            .map(|(p, _)| p)
            .chain(ranges.iter().map(|(p, _, _)| p))
        {
            if graph.declared_scope_for(labels, prop).map_err(RunError::Graph)?.is_some() {
                declared = true;
                break;
            }
        }
        if !declared {
            return Ok(None);
        }
    }
    let floor_label = labels.first().map(|s| s.as_str());
    if !graph.property_seek_worth_probing(floor_label) {
        return Ok(None);
    }
    let (cap_n, selectivity) = match use_ {
        SeekUse::PerId => (crate::PROPERTY_SEEK_MAX_PROBE, crate::PROPERTY_SEEK_SELECTIVITY),
        SeekUse::Walk => (SEEK_WALK_CAP, SEEK_WALK_SELECTIVITY),
    };
    let cap = Some(cap_n);
    let mut best: Option<(Vec<u64>, bool)> = None; // (ids, came from a declared scoped index)
    // PREFIX candidates (`prop STARTS WITH 'x'`) on DECLARED keys only — a
    // prefix is a range over the index the operator declared; nothing is
    // built for an undeclared one.
    for (prop, e) in prefixes {
        let Some(l) = graph.declared_scope_for(labels, prop).map_err(RunError::Graph)? else {
            continue;
        };
        let Value::Str(prefix) = eval_with(e, scope, None).map_err(RunError::Eval)? else {
            continue; // a non-string prefix: the predicate answers Null everywhere
        };
        if let Some(ids) = graph
            .index_probe_prefix_scoped(prop, &prefix, cap, Some(&l))
            .map_err(RunError::Graph)?
        {
            counted!("interp.columnar seek probed a declared prefix");
            if best.as_ref().is_none_or(|(b, _)| ids.len() < b.len()) {
                best = Some((ids, true));
            }
        }
    }
    // RANGE candidates (`prop > x`, …) on DECLARED keys — a trailing key of
    // a declared composite included (fix 47): the range of the scoped index,
    // exact for string keys. `MATCH (s:NewsStory) WHERE … s.status <> 'stale'
    // AND s.lastUpdatedAt > $cutoff … LIMIT 5` walked the whole label under
    // the mirror's `(status, lastUpdatedAt)` index; Neo4j seeks the same
    // index in 2 ms.
    for (prop, op, e) in ranges {
        let Some(l) = graph.declared_scope_for(labels, prop).map_err(RunError::Graph)? else {
            continue;
        };
        let v = eval_with(e, scope, None).map_err(RunError::Eval)?;
        if let Some(ids) = graph
            .index_probe_range_scoped(prop, *op, &v, cap, Some(&l))
            .map_err(RunError::Graph)?
        {
            counted!("interp.columnar seek probed a declared range");
            if best.as_ref().is_none_or(|(b, _)| ids.len() < b.len()) {
                best = Some((ids, true));
            }
        }
    }
    for (i, (prop, values)) in seeks.iter().enumerate() {
        // The declared index on a label the pattern requires whose FIRST
        // property is this key — a composite is ordered by it, so an equality
        // on it is the prefix the index answers. One rule for every seek site:
        // `Graph::declared_scope_for`.
        let scoped_to = graph.declared_scope_for(labels, prop).map_err(RunError::Graph)?;
        let scoped_to = scoped_to.as_deref();
        if scoped_to.is_none() && i > 0 {
            // Undeclared and not the first conjunct: probing it would build a
            // partition-wide index the operator never asked for. The first
            // conjunct keeps that behaviour because it always had it.
            continue;
        }
        let vs: Vec<Value> = values
            .iter()
            .map(|e| eval_with(e, scope, None).map_err(RunError::Eval))
            .collect::<Result<_, _>>()?;
        let probed = match scoped_to {
            Some(l) => {
                counted!("interp.columnar seek probed a declared scoped index");
                graph.index_probe_in_scoped(prop, &vs, cap, Some(l))
            }
            None => graph.index_probe_in(prop, &vs, cap),
        }
        .map_err(RunError::Graph)?;
        let Some(ids) = probed else {
            continue; // over the cap or not index-servable
        };
        if best.as_ref().is_none_or(|(b, _)| ids.len() < b.len()) {
            best = Some((ids, scoped_to.is_some()));
        }
    }
    let Some((ids, scoped)) = best else {
        return Ok(None);
    };
    if !graph.property_seek_wins_under(floor_label, ids.len(), cap_n, selectivity) {
        return Ok(None);
    }
    if scoped {
        counted!("interp.columnar seek chose a declared scoped index");
    }
    Ok(Some(ids))
}

fn local_for_prop(tag: &str, p: &str) -> String {
    format!("__col_{tag}{p}")
}
/// The local `id(var)` rewrites to — under the `__col_` prefix so the
/// vectoriser looks it up as a column and declines when a path has not
/// bound it.
fn local_for_id(tag: &str) -> String {
    format!("__col_{tag}__id")
}
fn local_for_label(tag: &str, l: &str) -> String {
    format!("__lbl_{tag}{l}")
}

/// The single-hop existence shape the rewrite can lift: `(var)-[:T…]->(:L…)`
/// with a bare bound start, no rel variable/props/length, and an unbound
/// (or anonymous) far end that may carry labels — and, when it carries
/// labels, an inline property map whose values read no variable (literals
/// and parameters: `(:Country {iso3: $a})`), returned as the fourth element.
/// An unlabelled far end with a map is refused: the map would have to be
/// resolved over every node in the graph.
fn probe_shape(path: &PathPattern, var: &str) -> Option<ProbeShape> {
    if path.shortest || path.var.is_some() || path.hops.len() != 1 {
        return None;
    }
    if path.start.var.as_deref() != Some(var)
        || !path.start.labels.is_empty()
        || path.start.props.is_some()
    {
        return None;
    }
    let (rel, end) = &path.hops[0];
    if rel.var.is_some() || rel.props.is_some() || rel.length.is_some() {
        return None;
    }
    if end.var.as_deref() == Some(var) {
        return None; // a self-loop shape: the general path judges it
    }
    let end_filter = match &end.props {
        None => None,
        Some(m @ Expr::Map(entries)) => {
            if end.labels.is_empty() || entries.is_empty() || crate::interp::contains_opaque(m) {
                return None;
            }
            let mut free = Vec::new();
            crate::interp::free_vars_of(m, &mut free);
            if !free.is_empty() {
                return None; // a value reading a variable is a per-row map: the general path's
            }
            Some(m.clone())
        }
        Some(_) => return None,
    };
    let dir = match rel.dir {
        RelDir::Out => Dir::Out,
        RelDir::In => Dir::In,
        RelDir::Undirected => Dir::Both,
    };
    Some((dir, rel.types.clone(), end.labels.clone(), end_filter))
}

/// What [`probe_shape`] recognises: direction, relationship types, far-end
/// labels, and the far end's variable-free property map when it has one.
type ProbeShape = (Dir, Vec<String>, Vec<String>, Option<Expr>);

/// Rewrite an expression over `var` into one over locals, collecting what
/// it reads. `None` if it reads `var` any other way — those shapes keep the
/// general path.
fn rewrite(e: &Expr, var: &str, kind: Kind, reads: &mut Reads) -> Option<Expr> {
    let rw = |x: &Expr, reads: &mut Reads| rewrite(x, var, kind, reads).map(Box::new);
    Some(match e {
        Expr::Prop(base, key) => match base.as_ref() {
            Expr::Var(v) if v == var => {
                reads.note_prop(key);
                Expr::Var(local_for_prop(&reads.tag, key))
            }
            _ => Expr::Prop(rw(base, reads)?, key.clone()),
        },
        // Labels and pattern probes are node reads; a relationship declines.
        Expr::HasLabels { .. }
        | Expr::ExistsSub(_)
        | Expr::CountSub(_)
        | Expr::PatternPredicate(_)
            if kind == Kind::Rel =>
        {
            return None;
        }
        // `type(r)` binds from the relationship's type token.
        Expr::Call {
            name,
            distinct: false,
            args,
            star: false,
        } if kind == Kind::Rel
            && name == "type"
            && matches!(args.as_slice(), [Expr::Var(v)] if v == var) =>
        {
            reads.type_read = true;
            Expr::Var(LOCAL_TYPE.to_string())
        }
        // A label test, probe or degree over ANOTHER variable is left for
        // that variable's pass (the hop scan rewrites a, b and r in turn);
        // a single-variable scan never gets here with one, since its free
        // variables were checked first.
        Expr::HasLabels { of, .. } if matches!(of.as_ref(), Expr::Var(v) if v != var) => e.clone(),
        Expr::ExistsSub(body) | Expr::CountSub(body)
            if matches!(crate::interp::pattern_body(body), Some((pattern, _))
                if pattern.paths.len() == 1
                    && pattern.paths[0].start.var.as_deref().is_some_and(|v| v != var)) =>
        {
            e.clone()
        }
        Expr::PatternPredicate(path) if path.start.var.as_deref().is_some_and(|v| v != var) => {
            e.clone()
        }
        Expr::HasLabels { of, labels: ls } => match of.as_ref() {
            Expr::Var(v) if v == var => {
                let mut acc: Option<Expr> = None;
                for l in ls {
                    reads.note_label(l);
                    let t = Expr::Var(local_for_label(&reads.tag, l));
                    acc = Some(match acc {
                        None => t,
                        Some(a) => Expr::And(Box::new(a), Box::new(t)),
                    });
                }
                acc.unwrap_or(Expr::Bool(true))
            }
            _ => return None,
        },
        // A pattern-shaped body — the bare pattern or a Query whose only
        // clause is a plain MATCH of it (`pattern_body`) — lifts to a probe
        // when it has no WHERE; the two spellings are one question.
        Expr::ExistsSub(body) => match crate::interp::pattern_body(body) {
            Some((pattern, None)) if pattern.paths.len() == 1 => {
                let (dir, types, labels, end_filter) = probe_shape(&pattern.paths[0], var)?;
                Expr::Var(reads.note_probe(dir, &types, &labels, end_filter.as_ref()))
            }
            _ => return None,
        },
        // `count{(n)-[:T…]-()}` with an anonymous, unlabelled, prop-free far
        // end is the node's degree — the adjacency table has it.
        Expr::CountSub(body) => match crate::interp::pattern_body(body) {
            Some((pattern, None)) if pattern.paths.len() == 1 => {
                let (dir, types, labels, end_filter) = probe_shape(&pattern.paths[0], var)?;
                if !labels.is_empty() || end_filter.is_some() {
                    return None;
                }
                Expr::Var(reads.note_degree(dir, &types))
            }
            _ => return None,
        },
        Expr::PatternPredicate(path) => {
            let (dir, types, labels, end_filter) = probe_shape(path, var)?;
            Expr::Var(reads.note_probe(dir, &types, &labels, end_filter.as_ref()))
        }
        // Fix 46: `id(var)` is the member's own id — a local the walk binds
        // from the id it is visiting, never a record read. `RETURN id(s)` /
        // `min(id(s))` over a label ran on the general path and decoded
        // every record in full (20k NewsStory records, 4.5 s on the mirror
        // for an id span).
        Expr::Call {
            name,
            distinct: false,
            args,
            star: false,
        } if name.eq_ignore_ascii_case("id")
            && matches!(args.as_slice(), [Expr::Var(v)] if v == var) =>
        {
            reads.id_read = true;
            Expr::Var(local_for_id(&reads.tag))
        }
        Expr::Var(v) if v == var => return None,
        Expr::Var(_)
        | Expr::Param(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null => e.clone(),
        Expr::And(a, b) => Expr::And(rw(a, reads)?, rw(b, reads)?),
        Expr::Or(a, b) => Expr::Or(rw(a, reads)?, rw(b, reads)?),
        Expr::Xor(a, b) => Expr::Xor(rw(a, reads)?, rw(b, reads)?),
        Expr::Not(a) => Expr::Not(rw(a, reads)?),
        Expr::Neg(a) => Expr::Neg(rw(a, reads)?),
        Expr::Bin(op, a, b) => Expr::Bin(*op, rw(a, reads)?, rw(b, reads)?),
        Expr::In(a, b) => Expr::In(rw(a, reads)?, rw(b, reads)?),
        Expr::Index(a, b) => Expr::Index(rw(a, reads)?, rw(b, reads)?),
        // `n.p IS [NOT] NULL` reads presence, not a value: the local is
        // bound from a keys-only column scan unless a value read elsewhere
        // loads the column anyway.
        Expr::IsNull { of, negated } if matches!(of.as_ref(), Expr::Prop(base, _) if matches!(base.as_ref(), Expr::Var(v) if v == var)) =>
        {
            let Expr::Prop(_, key) = of.as_ref() else {
                unreachable!("matched above")
            };
            reads.note_presence(key);
            Expr::IsNull {
                of: Box::new(Expr::Var(local_for_prop(&reads.tag, key))),
                negated: *negated,
            }
        }
        Expr::IsNull { of, negated } => Expr::IsNull {
            of: rw(of, reads)?,
            negated: *negated,
        },
        Expr::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                out.push(rewrite(it, var, kind, reads)?);
            }
            Expr::List(out)
        }
        Expr::Map(pairs) => {
            let mut out = Vec::with_capacity(pairs.len());
            for (k, v) in pairs {
                out.push((k.clone(), rewrite(v, var, kind, reads)?));
            }
            Expr::Map(out)
        }
        Expr::Case {
            subject,
            arms,
            otherwise,
        } => Expr::Case {
            subject: match subject {
                Some(s) => Some(rw(s, reads)?),
                None => None,
            },
            arms: {
                let mut out = Vec::with_capacity(arms.len());
                for (w, t) in arms {
                    out.push((rewrite(w, var, kind, reads)?, rewrite(t, var, kind, reads)?));
                }
                out
            },
            otherwise: match otherwise {
                Some(o) => Some(rw(o, reads)?),
                None => None,
            },
        },
        Expr::Call {
            name,
            distinct,
            args,
            star,
        } => {
            if is_aggregate_fn(name) {
                return None; // aggregates are sites, never nested here
            }
            let mut out = Vec::with_capacity(args.len());
            for a in args {
                out.push(rewrite(a, var, kind, reads)?);
            }
            Expr::Call {
                name: name.clone(),
                distinct: *distinct,
                args: out,
                star: *star,
            }
        }
        _ => return None,
    })
}

/// Recognise an aggregating projection's items over `var`.
fn aggregating_items(
    proj: &Projection,
    var: &str,
    kind: Kind,
    reads: &mut Reads,
) -> Option<(Vec<Item>, Vec<String>)> {
    if proj.star || proj.distinct {
        return None;
    }
    let mut items = Vec::with_capacity(proj.items.len());
    let mut columns = Vec::with_capacity(proj.items.len());
    let mut any_agg = false;
    for (i, it) in proj.items.iter().enumerate() {
        columns.push(
            it.alias
                .clone()
                .or_else(|| it.text.clone())
                .unwrap_or_else(|| column_name(&it.expr, i)),
        );
        match &it.expr {
            Expr::Call {
                name,
                distinct,
                args,
                star,
            } if is_aggregate_fn(name) => {
                any_agg = true;
                let (arg, star) = if *star {
                    if !args.is_empty() {
                        return None;
                    }
                    (None, true)
                } else {
                    match args.as_slice() {
                        // count(n): n is never null in a match — a count(*).
                        [Expr::Var(v)] if *v == var && name == "count" && !*distinct => {
                            (None, true)
                        }
                        [a] => (Some(rewrite(a, var, kind, reads)?), false),
                        _ => return None,
                    }
                };
                items.push(Item::Agg(
                    AggSite {
                        name: name.clone(),
                        distinct: *distinct,
                        args: arg.iter().cloned().collect(),
                        star,
                    },
                    arg,
                ));
            }
            other => items.push(Item::Key(rewrite(other, var, kind, reads)?)),
        }
    }
    if !any_agg {
        return None; // a plain projection streams fine already
    }
    Some((items, columns))
}

fn order_over(proj: &Projection, columns: &[String]) -> Option<Vec<(usize, bool)>> {
    let mut order = Vec::with_capacity(proj.order.len());
    for o in &proj.order {
        let ix = match &o.expr {
            Expr::Var(v) => columns.iter().position(|c| c == v),
            e => proj.items.iter().position(|it| it.expr == *e),
        }?;
        order.push((ix, o.desc));
    }
    Some(order)
}

/// A RETURN over the aggregating WITH's aliases: every variable it reads
/// must be one of them (or one of its own output aliases, in ORDER BY).
fn final_over(proj: &Projection, aliases: &[String]) -> Option<Final> {
    if proj.star || proj.distinct {
        return None;
    }
    let mut items = Vec::with_capacity(proj.items.len());
    let mut columns = Vec::with_capacity(proj.items.len());
    for (i, it) in proj.items.iter().enumerate() {
        if !reads_only(&it.expr, aliases) {
            return None;
        }
        items.push(it.expr.clone());
        columns.push(
            it.alias
                .clone()
                .or_else(|| it.text.clone())
                .unwrap_or_else(|| column_name(&it.expr, i)),
        );
    }
    let mut allowed: Vec<String> = aliases.to_vec();
    allowed.extend(columns.iter().cloned());
    for o in &proj.order {
        if !reads_only(&o.expr, &allowed) {
            return None;
        }
    }
    Some(Final {
        items,
        columns,
        order: proj.order.clone(),
        skip: proj.skip.clone(),
        limit: proj.limit.clone(),
    })
}

/// The scanned variable, its kind, its population and the full WHERE
/// (the clause's own plus the inline property maps as equalities) of one
/// MATCH — or `None` for any pattern outside the class.
fn recognise_source(match_clause: &Clause) -> Option<(String, Kind, Source, Option<Expr>)> {
    let Clause::Match {
        optional: false,
        pattern,
        where_,
    } = match_clause
    else {
        return None;
    };
    if pattern.paths.len() != 1 {
        return None;
    }
    let path = &pattern.paths[0];
    if path.var.is_some() || path.shortest {
        return None;
    }
    let anon = |n: &NodePattern| n.var.is_none() && n.labels.is_empty() && n.props.is_none();
    let mut conjuncts: Vec<Expr> = Vec::new();
    let (var, kind, source) = match path.hops.as_slice() {
        [] => {
            let var = path.start.var.clone()?;
            match &path.start.props {
                None => {}
                Some(Expr::Map(pairs)) => {
                    sometimes!(
                        "interp.columnar scan took an inline node property map",
                        true
                    );
                    for (k, v) in pairs {
                        conjuncts.push(Expr::Bin(
                            BinOp::Eq,
                            Box::new(Expr::Prop(Box::new(Expr::Var(var.clone())), k.clone())),
                            Box::new(v.clone()),
                        ));
                    }
                }
                Some(_) => return None,
            }
            (
                var,
                Kind::Node,
                Source::Nodes {
                    labels: path.start.labels.clone(),
                    any_of: Vec::new(),
                },
            )
        }
        // `()-[r:T…]->()` / `()<-[r:T…]-()`: anonymous, unlabelled,
        // prop-free ends and a named single relationship. Undirected would
        // match each relationship twice — declined. An inline property map
        // is the equalities it abbreviates.
        [(rel, end)] => {
            if !anon(&path.start)
                || !anon(end)
                || rel.length.is_some()
                || matches!(rel.dir, RelDir::Undirected)
            {
                return None;
            }
            let var = rel.var.clone()?;
            match &rel.props {
                None => {}
                Some(Expr::Map(pairs)) => {
                    for (k, v) in pairs {
                        conjuncts.push(Expr::Bin(
                            BinOp::Eq,
                            Box::new(Expr::Prop(Box::new(Expr::Var(var.clone())), k.clone())),
                            Box::new(v.clone()),
                        ));
                    }
                }
                Some(_) => return None,
            }
            (
                var,
                Kind::Rel,
                Source::Rels {
                    types: rel.types.clone(),
                },
            )
        }
        _ => return None,
    };
    let mut full_where: Option<Expr> = where_.clone();
    for c in conjuncts {
        full_where = Some(match full_where {
            None => c,
            Some(w) => Expr::And(Box::new(c), Box::new(w)),
        });
    }
    // Only the scanned variable may be read: any other name is unbound
    // here, and the general path refuses it by name.
    if let Some(w) = &full_where {
        if !reads_only(w, std::slice::from_ref(&var)) {
            return None;
        }
    }
    // An unlabelled node match whose WHERE implies a label disjunction
    // walks that union, not every node.
    let source = match source {
        Source::Nodes { labels, any_of: _ } if labels.is_empty() => Source::Nodes {
            any_of: full_where
                .as_ref()
                .and_then(|w| implied_labels(w, &var))
                .unwrap_or_default(),
            labels,
        },
        other => other,
    };
    Some((var, kind, source, full_where))
}

/// Whether `e` reads no variable outside `allowed`.
fn reads_only(e: &Expr, allowed: &[String]) -> bool {
    let mut free = Vec::new();
    free_vars_of(e, &mut free);
    free.iter().all(|v| allowed.contains(v))
}

fn recognise(q: &SingleQuery) -> Option<Plan> {
    let (match_clause, agg_proj, final_proj) = match q.clauses.as_slice() {
        [m @ Clause::Match { .. }, Clause::Return { proj }] => (m, proj, None),
        [
            m @ Clause::Match { .. },
            Clause::With {
                proj: wp,
                where_: None,
            },
            Clause::Return { proj: rp },
        ] => (m, wp, Some(rp)),
        _ => return None,
    };
    let (var, kind, source, full_where) = recognise_source(match_clause)?;
    // See `recognise_projection`: an identity equality stays the general
    // path's one-get seek.
    if crate::interp::id_seek_expr(full_where.as_ref(), &var).is_some() {
        return None;
    }
    let seeks = prop_eq_candidates(full_where.as_ref(), &var);
    let prefixes = crate::interp::prop_prefix_candidates(full_where.as_ref(), &var);
    let ranges = crate::interp::prop_range_candidates(full_where.as_ref(), &var);
    // Covered: every conjunct is a seek equality, a prefix or a range — the
    // count is then the size of the probes' intersection (`covered_count`).
    let covered = (!seeks.is_empty() || !prefixes.is_empty() || !ranges.is_empty())
        && full_where.as_ref().map(conjunct_count).unwrap_or(0)
            == seeks.len() + prefixes.len() + ranges.len();
    let mut reads = Reads::default();
    let pred = match &full_where {
        None => None,
        Some(w) => Some(rewrite(w, &var, kind, &mut reads)?),
    };
    // A graph-dependent subquery the rewrite could not lift into a probe/local
    // would reach `eval_with(.., None)` — this columnar path has no hooks, so it
    // MUST decline (a correlated `exists {…}` / `count {…}` over a var this scan
    // does not bind). The interp fallback runs it WITH hooks, identically.
    if pred.as_ref().is_some_and(contains_opaque) {
        return None;
    }
    if agg_proj
        .items
        .iter()
        .any(|it| !reads_only(&it.expr, std::slice::from_ref(&var)))
    {
        return None;
    }
    let (items, columns) = aggregating_items(agg_proj, &var, kind, &mut reads)?;
    let order = order_over(agg_proj, &columns)?;
    let final_ = match final_proj {
        None => None,
        Some(rp) => {
            // ORDER/SKIP/LIMIT on the WITH with a RETURN after it is a rarer
            // shape: decline rather than model two pagings.
            if !agg_proj.order.is_empty() || agg_proj.skip.is_some() || agg_proj.limit.is_some() {
                return None;
            }
            Some(final_over(rp, &columns)?)
        }
    };
    Some(Plan {
        source,
        pred,
        reads,
        items,
        columns,
        order,
        seeks,
        prefixes,
        ranges,
        covered,
        skip: agg_proj.skip.clone(),
        limit: agg_proj.limit.clone(),
        final_,
    })
}

/// Order, then page, a row set.
fn order_and_page(
    graph: &Graph,
    params: &BTreeMap<String, Value>,
    mut rows: Vec<Vec<Value>>,
    order: &[OrderItem],
    keys: Vec<Vec<Value>>,
    skip: Option<&Expr>,
    limit: Option<&Expr>,
) -> Result<Vec<Vec<Value>>, RunError> {
    if !order.is_empty() {
        let idx = sorted_indices(order, &keys);
        rows = idx
            .into_iter()
            .map(|i| std::mem::take(&mut rows[i]))
            .collect();
    }
    let skip = eval_count(graph, skip, params, "SKIP")?.unwrap_or(0);
    if skip > 0 {
        rows.drain(..skip.min(rows.len()));
    }
    if let Some(limit) = eval_count(graph, limit, params, "LIMIT")? {
        rows.truncate(limit);
    }
    Ok(rows)
}

/// The row order for an ORDER BY: a single key that is Int in every row
/// (or Str in every row) sorts a `(key, arrival)` vector unstably —
/// arrival is the tiebreak, so the result is the stable sort's — instead
/// of a comparator over `Vec<Value>` per comparison. The 163k-row
/// projection sorted on a string and the 1.79M-degree histogram both spent
/// their sort in that comparator. Anything else takes the comparator.
/// `sorted_indices` over the rows themselves: the keys are columns of the
/// rows (`order` = (column, desc)), so no key vector is built or cloned.
/// A single Int or Str column takes the primitive path.
fn sorted_indices_by_column(order: &[(usize, bool)], rows: &[Vec<Value>]) -> Vec<usize> {
    if let [(ix, desc)] = order {
        let (ix, desc) = (*ix, *desc);
        if rows.iter().all(|r| matches!(r[ix], Value::Int(_))) {
            let mut v: Vec<(i64, usize)> = rows
                .iter()
                .enumerate()
                .map(|(i, r)| match &r[ix] {
                    Value::Int(n) => (if desc { n.wrapping_neg() } else { *n }, i),
                    _ => unreachable!("checked"),
                })
                .collect();
            if !desc || v.iter().all(|(n, _)| *n != i64::MIN) {
                v.sort_unstable();
                sometimes!("interp.columnar order sorted a primitive key", true);
                return v.into_iter().map(|(_, i)| i).collect();
            }
        }
        if rows.iter().all(|r| matches!(r[ix], Value::Str(_))) {
            let mut v: Vec<(&str, usize)> = rows
                .iter()
                .enumerate()
                .map(|(i, r)| match &r[ix] {
                    Value::Str(t) => (t.as_str(), i),
                    _ => unreachable!("checked"),
                })
                .collect();
            if desc {
                v.sort_unstable_by(|a, b| b.0.cmp(a.0).then(a.1.cmp(&b.1)));
            } else {
                v.sort_unstable();
            }
            sometimes!("interp.columnar order sorted a primitive key", true);
            return v.into_iter().map(|(_, i)| i).collect();
        }
    }
    let items: Vec<OrderItem> = order
        .iter()
        .map(|(ix, desc)| OrderItem {
            expr: Expr::Int(*ix as i64),
            desc: *desc,
        })
        .collect();
    let mut idx: Vec<usize> = (0..rows.len()).collect();
    idx.sort_by(|&a, &b| {
        for (o, (ix, _)) in items.iter().zip(order) {
            let ord = cmp_order_keys(
                std::slice::from_ref(o),
                std::slice::from_ref(&rows[a][*ix]),
                std::slice::from_ref(&rows[b][*ix]),
            );
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
    idx
}

/// Order by row columns, then page.
fn order_and_page_by_column(
    graph: &Graph,
    params: &BTreeMap<String, Value>,
    mut rows: Vec<Vec<Value>>,
    order: &[(usize, bool)],
    skip: Option<&Expr>,
    limit: Option<&Expr>,
) -> Result<Vec<Vec<Value>>, RunError> {
    if !order.is_empty() {
        let idx = sorted_indices_by_column(order, &rows);
        rows = idx
            .into_iter()
            .map(|i| std::mem::take(&mut rows[i]))
            .collect();
    }
    let skip = eval_count(graph, skip, params, "SKIP")?.unwrap_or(0);
    if skip > 0 {
        rows.drain(..skip.min(rows.len()));
    }
    if let Some(limit) = eval_count(graph, limit, params, "LIMIT")? {
        rows.truncate(limit);
    }
    Ok(rows)
}

fn sorted_indices(order: &[OrderItem], keys: &[Vec<Value>]) -> Vec<usize> {
    if order.len() == 1 {
        let desc = order[0].desc;
        if keys.iter().all(|k| matches!(k.as_slice(), [Value::Int(_)])) {
            let mut v: Vec<(i64, usize)> = keys
                .iter()
                .enumerate()
                .map(|(i, k)| match &k[0] {
                    Value::Int(n) => (if desc { n.wrapping_neg() } else { *n }, i),
                    _ => unreachable!("checked"),
                })
                .collect();
            // i64::MIN negates to itself: keep the comparator for that edge.
            if !desc || v.iter().all(|(n, _)| *n != i64::MIN) {
                v.sort_unstable();
                sometimes!("interp.columnar order sorted a primitive key", true);
                return v.into_iter().map(|(_, i)| i).collect();
            }
        }
        if keys.iter().all(|k| matches!(k.as_slice(), [Value::Str(_)])) {
            let mut v: Vec<(&str, usize)> = keys
                .iter()
                .enumerate()
                .map(|(i, k)| match &k[0] {
                    Value::Str(t) => (t.as_str(), i),
                    _ => unreachable!("checked"),
                })
                .collect();
            if desc {
                v.sort_unstable_by(|a, b| b.0.cmp(a.0).then(a.1.cmp(&b.1)));
            } else {
                v.sort_unstable();
            }
            sometimes!("interp.columnar order sorted a primitive key", true);
            return v.into_iter().map(|(_, i)| i).collect();
        }
    }
    let mut idx: Vec<usize> = (0..keys.len()).collect();
    idx.sort_by(|&a, &b| cmp_order_keys(order, &keys[a], &keys[b]));
    idx
}

/// The loaded population and columns one scan walks, and the per-id
/// binding of its locals — shared by the aggregate and projection scans.
struct Walk {
    members: std::sync::Arc<Vec<u64>>,
    rel_types: Vec<u32>,
    type_names: BTreeMap<u32, Value>,
    columns: Vec<Vec<(u64, Value)>>,
    cursors: Vec<usize>,
    /// Presence-only columns: (property, ids carrying it, cursor).
    presence: Vec<(String, Vec<u64>, usize)>,
    /// Labels served from membership: (label, members, cursor) — every
    /// label test, since fix 44 (the label-set column walk is gone).
    label_members: Vec<(String, std::sync::Arc<Vec<u64>>, usize)>,
    /// Degree probes resolved to tokens: (local, dir, tokens, never-minted).
    degrees: Vec<(String, Dir, Option<Vec<u32>>, bool)>,
    /// Per probe (aligned with `reads.probes`): the sorted far-end ids its
    /// inline map admits, resolved once at load — `None` for a probe with
    /// no map, which tests the far end's labels alone.
    probe_ends: ProbeEnds,
    /// Per probe (aligned with `reads.probes`): its answer for EVERY member,
    /// computed in one pass over the type's adjacency table at load (fix
    /// 36c) — `None` for a probe that keeps the per-member walk.
    probe_hits: Vec<Option<Vec<bool>>>,
}

/// One entry per probe of a walk's reads: the resolved far-end id set of
/// a probe carrying an inline map, `None` for one without.
type ProbeEnds = Vec<Option<std::sync::Arc<Vec<u64>>>>;

/// The variable a probe's far-end map is filtered under (`(:Country {iso3:
/// $a})` becomes `__probe_end.iso3 = $a` over `:Country`). Internal: no
/// statement can name it.
const PROBE_END_VAR: &str = "__probe_end";

/// A cached column's entries within `[lo, hi)` — the population's id range —
/// as an owned column the walk's cursor takes values from. A population that
/// is a SUBSET of the label (a batch, a hop's ends) gets the entries of the
/// non-members between its ids too, exactly as a span scan would; the bind
/// cursor id-matches and never consults them.
fn restrict_entries(col: &[(u64, Value)], lo: u64, hi: Option<u64>) -> Vec<(u64, Value)> {
    let start = col.partition_point(|(id, _)| *id < lo);
    let end = match hi {
        Some(h) => col.partition_point(|(id, _)| *id < h),
        None => col.len(),
    };
    col[start..end].to_vec()
}

/// [`restrict_entries`] for a presence column (ids only).
fn restrict_ids(ids: &[u64], lo: u64, hi: Option<u64>) -> Vec<u64> {
    let start = ids.partition_point(|id| *id < lo);
    let end = match hi {
        Some(h) => ids.partition_point(|id| *id < h),
        None => ids.len(),
    };
    ids[start..end].to_vec()
}

/// A cached column restricted to a POPULATION (`ids`, ascending): only the
/// population's entries are cloned, by one merge over the two sorted
/// sequences. [`restrict_entries`] restricts to the population's id RANGE,
/// which for a walk over a seek's ids — a few thousand drawn from across a
/// label's whole span — cloned the entire column: `g.eventId STARTS WITH
/// 'edgar-8k-'` sought 3.9k of 44k events and then cloned 44k strings per
/// property to bind 3.9k (7 ms against Neo4j's 1.4 for the plain count).
/// The bind cursor id-matches, so a column holding only the members is
/// exactly what it reads.
fn restrict_entries_to(col: &[(u64, Value)], ids: &[u64]) -> Vec<(u64, Value)> {
    let mut out = Vec::with_capacity(ids.len());
    let mut ci = 0usize;
    for &id in ids {
        while ci < col.len() && col[ci].0 < id {
            ci += 1;
        }
        if ci < col.len() && col[ci].0 == id {
            out.push((id, col[ci].1.clone()));
        }
    }
    out
}

/// [`restrict_entries_to`] for a presence column (ids only).
fn restrict_ids_to(present: &[u64], ids: &[u64]) -> Vec<u64> {
    let mut out = Vec::with_capacity(ids.len().min(present.len()));
    let mut ci = 0usize;
    for &id in ids {
        while ci < present.len() && present[ci] < id {
            ci += 1;
        }
        if ci < present.len() && present[ci] == id {
            out.push(id);
        }
    }
    out
}

/// Load the walk for a source and its reads, or decline (`None`): by the
/// relationship entry budget, or a column wider than the label. Nothing
/// is counted here — a declined scan must not count itself. `params`
/// resolve a probe's far-end map (`$a` in `(:Country {iso3: $a})`).
fn load_walk(
    graph: &Graph,
    source: &Source,
    reads: &Reads,
    params: &BTreeMap<String, Value>,
) -> Result<Option<Walk>, RunError> {
    load_walk_over(graph, source, reads, None, params)
}

/// `load_walk` with the population supplied (the distinct end ids of a
/// hop), instead of read from the source's labels.
fn load_walk_over(
    graph: &Graph,
    source: &Source,
    reads: &Reads,
    over: Option<std::sync::Arc<Vec<u64>>>,
    params: &BTreeMap<String, Value>,
) -> Result<Option<Walk>, RunError> {
    load_walk_budgeted(graph, source, reads, over, None, params)
}

/// The far-end id sets of the probes that carry an inline map, resolved
/// ONCE for the walk: each map becomes a conjunction of `__probe_end.k = v`
/// over the far end's labels and is answered by the column filter
/// (`filter_ids`), whose survivors are exactly the nodes the pattern's own
/// `node_satisfies` would accept. A filter that declines (the columnar
/// paths are off, a column budget) declines the whole walk — the general
/// path then judges the statement, byte-identically.
fn resolve_probe_ends(
    graph: &Graph,
    reads: &Reads,
    params: &BTreeMap<String, Value>,
) -> Result<Option<ProbeEnds>, RunError> {
    let mut out: ProbeEnds = Vec::with_capacity(reads.probes.len());
    for p in &reads.probes {
        let Some(Expr::Map(entries)) = &p.end_filter else {
            // Fix 36b: a LABELLED far end with no map — `NOT EXISTS {
            // (n)-[:MENTIONS_INTEREST]->(:Interest) }` — resolves its
            // label membership ONCE for the walk and probes it as a set;
            // `adjacency_probe_labeled` looked the membership snapshot up
            // per member (38k snapshot lookups for an 18k-row anti-join on
            // the mirror: 66–82 ms against Neo4j's 20). An unlabelled far
            // end stays the plain adjacency probe.
            if !p.labels.is_empty() {
                let members = graph.members_all(&p.labels).map_err(RunError::Graph)?;
                counted!("interp.columnar probe resolved its labelled far end once");
                out.push(Some(members.to_arc_vec()));
            } else {
                out.push(None);
            }
            continue;
        };
        let pred = entries
            .iter()
            .map(|(k, v)| {
                Expr::Bin(
                    engram_cypher::BinOp::Eq,
                    Box::new(Expr::Prop(
                        Box::new(Expr::Var(PROBE_END_VAR.to_string())),
                        k.clone(),
                    )),
                    Box::new(v.clone()),
                )
            })
            .reduce(|a, b| Expr::And(Box::new(a), Box::new(b)))
            .expect("probe_shape refuses an empty map");
        // SEEK the far end first when one of the map's keys has a declared
        // index on its label (`columnar_seek_ids` — the one seek rule): the
        // filter then runs over the probe's candidates rather than the whole
        // far-end label. Without it a far end of 13k `:EmailAsk` was walked
        // for every statement that named one (15 ms for a 10-row count).
        // An unscoped probe's ids may carry the key under another label, so
        // they are kept to the label's members first; the filter re-checks
        // the whole map per candidate either way.
        let empty_vars = VarMap::new();
        let scope = Scope::over(params, &empty_vars, graph.wall_ms(), graph.zone_provider());
        let seeks: Vec<(String, Vec<Expr>)> = entries
            .iter()
            .map(|(k, v)| (k.clone(), vec![v.clone()]))
            .collect();
        let over = match columnar_seek_ids(graph, &p.labels, &seeks, &[], &[], SeekUse::PerId, &scope)? {
            Some(ids) => {
                let members = graph.members_all(&p.labels).map_err(RunError::Graph)?;
                counted!("interp.columnar probe sought its far end");
                Some(std::sync::Arc::new(
                    ids.into_iter()
                        .filter(|id| graph.members_contains(&members, *id))
                        .collect::<Vec<u64>>(),
                ))
            }
            None => None,
        };
        let Some(ids) = filter_ids_in(graph, &p.labels, PROBE_END_VAR, &pred, params, over)?
        else {
            return Ok(None);
        };
        out.push(Some(ids));
    }
    Ok(Some(out))
}

/// `load_walk_over` with the column budget sized from `budget_rows` rather
/// than the population: the ends of a hop are a few thousand ids drawn
/// from a label whose every row carries the property, so the right bound
/// is the END LABEL's rows, not the distinct ends. Measured on the
/// production port: the hop scan declined `(s:Company)-[r:SUPPLIES]->
/// (cus:Company) … s.primaryCountry …` silently for exactly this reason,
/// and its target statement did not move.
fn load_walk_budgeted(
    graph: &Graph,
    source: &Source,
    reads: &Reads,
    over: Option<std::sync::Arc<Vec<u64>>>,
    budget_rows: Option<usize>,
    params: &BTreeMap<String, Value>,
) -> Result<Option<Walk>, RunError> {
    // The probes' far-end sets first: a map the column filter cannot serve
    // declines the walk before any column of the population is read.
    let Some(probe_ends) = resolve_probe_ends(graph, reads, params)? else {
        return Ok(None);
    };
    // Every column read is bounded to the label's id span and budgeted at
    // `factor × |members|` entries: a property the whole graph carries is
    // a 1.79M-entry column however small the label, and nine production
    // statements went from ~0 ms to 0.4–3.2 s reading it. Past the budget
    // the scan DECLINES and the general path's per-id projection answers.
    let (members, rel_types, family): (std::sync::Arc<Vec<u64>>, Vec<u32>, ColumnFamily) =
        match source {
            Source::Nodes { .. } if over.is_some() => (
                over.clone().expect("checked"),
                Vec::new(),
                ColumnFamily::Nodes,
            ),
            Source::Nodes { any_of, .. } if !any_of.is_empty() => (
                graph.members_any(any_of).map_err(RunError::Graph)?.to_arc_vec(),
                Vec::new(),
                ColumnFamily::Nodes,
            ),
            Source::Nodes { labels, .. } => (
                graph.members_all(labels).map_err(RunError::Graph)?.to_arc_vec(),
                Vec::new(),
                ColumnFamily::Nodes,
            ),
            Source::Rels { types } => match graph.rel_members(types).map_err(RunError::Graph)? {
                Some((ids, toks, _ends)) => (ids, toks, ColumnFamily::Rels),
                None => {
                    sometimes!(
                        "interp.columnar rel scan declined by the entry budget",
                        true
                    );
                    return Ok(None);
                }
            },
        };
    let (lo, hi) = match (members.first(), members.last()) {
        (Some(&a), Some(&b)) => (a, Some(b.saturating_add(1))),
        _ => (0, Some(0)),
    };
    let budget =
        graph.columnar_column_budget(budget_rows.unwrap_or(members.len()).max(members.len()));
    // DECLINE BEFORE WALKING when the id span is far wider than the budget.
    // The walk visits every row in `[lo, hi)` and stops at the budget, so on
    // a store whose ids are dense (the paged production mirror — every label
    // interleaved over ~5M ids) a span of more than `budget` rows can only
    // decline, and it declined AFTER visiting `factor × |members|` rows per
    // column: the 143-member ManagedRepo list walked ~4.6k rows twice on
    // every call before gathering the 143 records it wanted, and that walk
    // was the whole 2.2-vs-1.1 ms gap against Neo4j. The span is known from
    // the member ids; when it is more than eight budgets wide the walk is
    // skipped and the gather answers directly. On a store with SPARSE ids a
    // walk that would have fit is skipped for a gather of |members| point
    // reads — the direction that costs a few microseconds, not a scan.
    let span = hi.unwrap_or(lo).saturating_sub(lo) as usize;
    let sparse_label = span > budget.saturating_mul(8);
    // The PROPERTY-COLUMN CACHE (`Graph::prop_column`): a single-label node
    // population reads a column the last walk over that label assembled,
    // restricted to this population's id range, instead of assembling it
    // again by a point read per member; a walk over the WHOLE label keeps
    // what it assembled. The stamp is read before any read so that a commit
    // during the gather retires the column rather than being missed.
    let stamp = graph.column_stamp();
    // A MULTI-LABEL source (`(r:Repo:ManagedRepo)`) reads through its
    // SMALLEST label's cache entry, restricted to the intersection: the
    // cache is keyed per label, and every member of the intersection is a
    // member of each of its labels, so the smallest label's whole column
    // covers it. The walk KEEPS what it gathered only when the intersection
    // IS that label whole (equal counts — the intersection is a subset, so
    // equal counts are equal sets); a strict subset is not a whole-label
    // column and is not filed as one. Before this the two-label population
    // consulted no cache and re-gathered its 143 records on every statement
    // (`store.gets` 143 per run, 1.8 ms against Neo4j's 1.7) while the
    // one-label spelling of the same list was served from the cache.
    let (cache_label, multi_label): (Option<&str>, bool) = match source {
        Source::Nodes { labels, any_of } if labels.len() == 1 && any_of.is_empty() => {
            (Some(labels[0].as_str()), false)
        }
        Source::Nodes { labels, any_of } if labels.len() > 1 && any_of.is_empty() => {
            let smallest = labels
                .iter()
                .min_by_key(|l| graph.count_label_nodes(l))
                .map(String::as_str);
            if smallest.is_some() {
                counted!("interp.columnar multi-label column read through its smallest label");
            }
            (smallest, true)
        }
        _ => (None, false),
    };
    let whole_label = cache_label.is_some()
        && over.is_none()
        && (!multi_label
            || cache_label.is_some_and(|l| graph.count_label_nodes(l) == members.len() as u64));
    let mut columns: Vec<Vec<(u64, Value)>> = Vec::with_capacity(reads.props.len());
    let mut declined: Vec<usize> = Vec::new();
    let mut from_cache: Vec<bool> = vec![false; reads.props.len()];
    for (j, p) in reads.props.iter().enumerate() {
        if let Some(label) = cache_label {
            if let Some(PropColumn::Values(col)) = graph.prop_column(label, p, false) {
                counted!("interp.columnar column read served from the property-column cache");
                // A supplied population (a seek's ids, a batch) takes ONLY
                // its members' entries; the whole label takes the column.
                columns.push(match &over {
                    Some(o) => {
                        counted!("interp.columnar cached column restricted to the population");
                        restrict_entries_to(&col, o)
                    }
                    None if multi_label => restrict_entries_to(&col, members.as_slice()),
                    None => restrict_entries(&col, lo, hi),
                });
                from_cache[j] = true;
                continue;
            }
        }
        if sparse_label {
            counted!("interp.columnar column read skipped the span walk for a sparse label");
            declined.push(j);
            columns.push(Vec::new());
            continue;
        }
        // The range scan over `[lo, hi)` DECLINES (Ok(None)) when the population
        // is SPARSE in the id space — its span holds more rows than the budget
        // because other node/rel types interleave with it (a label of 10,723
        // forums scattered across 546k nodes; on the paged production mirror
        // EVERY label, since the loader wrote them in id order). Such columns
        // fall back to a GATHER of exactly `members`' values below — one
        // record read per member for ALL the declined columns together,
        // byte-identical per column to what the scan would have produced (same
        // token, same tagged bytes, same decode, same settle). The gather is a
        // SUBSET of the range result — it drops the interleaved non-member
        // entries the scan also returns — but both the sequential `bind`
        // cursor (`col[cur].0 < id` advance) and `bind_random`'s binary search
        // id-MATCH, so the missing non-members are never consulted and absent
        // members bind `Null` on both paths. Mirrors `load_family_columns`.
        match graph
            .column_entries_bounded_in(family, p, lo, hi, budget)
            .map_err(RunError::Graph)?
        {
            Some(c) => columns.push(c),
            None => {
                declined.push(j);
                columns.push(Vec::new());
            }
        }
    }
    if !declined.is_empty() {
        let names: Vec<String> = declined.iter().map(|&j| reads.props[j].clone()).collect();
        let gathered = graph
            .column_entries_gather_many(family, &names, members.as_slice())
            .map_err(RunError::Graph)?;
        for (j, col) in declined.into_iter().zip(gathered) {
            columns[j] = col;
        }
    }
    if whole_label {
        if let Some(label) = cache_label {
            for (j, p) in reads.props.iter().enumerate() {
                if !from_cache[j] {
                    graph.keep_prop_column(
                        label,
                        p,
                        stamp,
                        PropColumn::Values(std::sync::Arc::new(columns[j].clone())),
                    );
                }
            }
        }
    }
    let mut presence: Vec<(String, Vec<u64>, usize)> = Vec::new();
    for p in reads.presence_only() {
        if let Some(label) = cache_label {
            if let Some(PropColumn::Presence(ids)) = graph.prop_column(label, &p, true) {
                counted!("interp.columnar column read served from the property-column cache");
                let kept = match &over {
                    Some(o) => {
                        counted!("interp.columnar cached column restricted to the population");
                        restrict_ids_to(&ids, o)
                    }
                    None if multi_label => restrict_ids_to(&ids, members.as_slice()),
                    None => restrict_ids(&ids, lo, hi),
                };
                presence.push((p, kept, 0));
                continue;
            }
        }
        // A presence read (`IS [NOT] NULL`) reads only the column KEYS and
        // never decodes a value. Its gather is `column_presence_gather` — a
        // point read per member asking only whether the property is there —
        // which tolerates an undecodable value exactly as the scan does, so
        // the two are byte-identical in what they answer AND in what they
        // refuse. Before it existed a declined presence scan took the whole
        // stage to the general path, which materialised every member in
        // full: the production NewsArticle enrichment count grew the
        // resident set by 6.75 GB per execution for a `count(a)`.
        let ids: Vec<u64> = if sparse_label {
            counted!("interp.columnar column read skipped the span walk for a sparse label");
            graph
                .column_presence_gather(family, &p, members.as_slice())
                .map_err(RunError::Graph)?
        } else {
            match graph
                .column_presence_bounded_in(family, &p, lo, hi, budget)
                .map_err(RunError::Graph)?
            {
                Some(ids) => ids,
                None => {
                    sometimes!(
                        "interp.columnar scan declined a column wider than its label",
                        true
                    );
                    graph
                        .column_presence_gather(family, &p, members.as_slice())
                        .map_err(RunError::Graph)?
                }
            }
        };
        if whole_label {
            if let Some(label) = cache_label {
                graph.keep_prop_column(
                    label,
                    &p,
                    stamp,
                    PropColumn::Presence(std::sync::Arc::new(ids.clone())),
                );
            }
        }
        presence.push((p, ids, 0));
    }
    // Fix 44: EVERY label test is answered from the label's MEMBERSHIP
    // snapshot — a cursor over its sorted ids beside the population — as
    // the population's own `any_of` labels always were. The rest used to
    // read the LABEL-SET column over the population's id span: the mirror's
    // ids interleave labels, so `(a:WebSource OR a:EmailSource)` over 55
    // KnowledgeArticles walked millions of ids per statement and was
    // budget-bound (2.1–3.1 ms against Neo4j's 0.7 for the count).
    let mut label_members: Vec<(String, std::sync::Arc<Vec<u64>>, usize)> = Vec::new();
    for l in &reads.labels {
        label_members.push((
            l.clone(),
            graph.members(Some(l)).map_err(RunError::Graph)?.to_arc_vec(),
            0,
        ));
    }
    if !label_members.is_empty() {
        counted!("interp.columnar label test answered from membership");
    }
    // `type(r)` per relationship: token → name, resolved once per token.
    let mut type_names: BTreeMap<u32, Value> = BTreeMap::new();
    if reads.type_read {
        for t in &rel_types {
            if !type_names.contains_key(t) {
                let name = graph.type_name(*t).map_err(RunError::Graph)?;
                type_names.insert(*t, Value::Str(name));
            }
        }
    }
    let mut degrees = Vec::with_capacity(reads.degrees.len());
    for d in &reads.degrees {
        // A read never mints: a named type never minted has no edges.
        let live: Vec<String> = d
            .types
            .iter()
            .filter(|t| graph.type_exists(t))
            .cloned()
            .collect();
        let dead = !d.types.is_empty() && live.is_empty();
        let tokens = if live.is_empty() {
            None
        } else {
            let mut v: Vec<u32> = live
                .iter()
                .filter_map(|t| graph.type_token_peek(t))
                .collect();
            v.sort_unstable();
            Some(v)
        };
        degrees.push((d.local.clone(), d.dir, tokens, dead));
    }
    // Fix 36c: a DIRECTED probe is answered for the WHOLE population in one
    // pass over the (side, types) adjacency table — the table borrowed once
    // (`Graph::with_hop_table`), the far-end set or label memberships
    // resolved once, each member's row walked in place — instead of
    // `adjacency_probe_*` per member with its token lookup, membership
    // lookup, per-node Vec and per-visit bookkeeping (~0.9 µs a member: the
    // email revival backlog's `NOT EXISTS {(n)-[:MENTIONS_INTEREST]->
    // (:Interest)}` cost 16 ms over 18k emails on the mirror, the pick 93 ms
    // against Neo4j's 74). An undirected probe, an id past the table's
    // range, a writing transaction or an absent table keep the per-member
    // probe; a type never minted has no edges.
    let mut probe_hits: Vec<Option<Vec<bool>>> = Vec::with_capacity(reads.probes.len());
    let in_table_range = members.last().is_none_or(|&m| m <= crate::DEGREE_TABLE_MAX_ID);
    for (pi, p) in reads.probes.iter().enumerate() {
        let tag = match p.dir {
            Dir::Out => b'O',
            Dir::In => b'I',
            Dir::Both => {
                probe_hits.push(None);
                continue;
            }
        };
        if !in_table_range {
            probe_hits.push(None);
            continue;
        }
        let tokens = graph.type_tokens_peek(&p.types);
        if matches!(&tokens, Some(t) if t.is_empty()) {
            probe_hits.push(Some(vec![false; members.len()]));
            continue;
        }
        let end_set: Option<&[u64]> = probe_ends
            .get(pi)
            .and_then(|s| s.as_deref())
            .map(|v| v.as_slice());
        let mut label_sets: Vec<crate::MembersView> = Vec::new();
        if end_set.is_none() {
            for l in &p.labels {
                label_sets.push(graph.members(Some(l)).map_err(RunError::Graph)?);
            }
        }
        let hits = graph.with_hop_table(tag, &tokens, members.len(), |tbl| {
            let t = tbl?;
            let mut out = Vec::with_capacity(members.len());
            for &id in members.iter() {
                let hit = t.slice(id).iter().any(|e| match end_set {
                    Some(set) => set.binary_search(&e.peer).is_ok(),
                    None => label_sets.iter().all(|m| graph.members_contains(m, e.peer)),
                });
                out.push(hit);
            }
            Some(out)
        });
        if hits.is_some() {
            counted!("interp.columnar probes answered over the population");
        }
        probe_hits.push(hits);
    }
    let cursors = vec![0usize; columns.len()];
    Ok(Some(Walk {
        members,
        rel_types,
        type_names,
        columns,
        cursors,
        presence,
        label_members,
        degrees,
        probe_ends,
        probe_hits,
    }))
}

/// The walk's own events, once the scan has committed to running.
fn note_walk_events(source: &Source, reads: &Reads, walk: &Walk) {
    if walk.probe_ends.iter().any(Option::is_some) {
        counted!("interp.columnar probe resolved its far-end map once");
    }
    if !walk.degrees.is_empty() {
        sometimes!(
            "interp.columnar scan bound a degree from the adjacency table",
            true
        );
    }
    if !walk.presence.is_empty() {
        sometimes!("interp.columnar scan read a column for presence only", true);
    }
    if matches!(source, Source::Nodes { any_of, .. } if !any_of.is_empty()) {
        sometimes!(
            "interp.columnar scan narrowed an unlabelled match to a label disjunction",
            true
        );
    }
    if !walk.label_members.is_empty() {
        sometimes!("interp.columnar scan bound a label from membership", true);
    }
    let _ = reads;
}

impl Walk {
    /// Bind an arbitrary `id` into `scope` by binary search — for the ends
    /// of a hop, whose ids arrive in relationship order, not id order.
    fn bind_random(
        &self,
        graph: &Graph,
        reads: &Reads,
        scope: &mut Scope<'_>,
        id: u64,
    ) -> Result<(), RunError> {
        if reads.id_read {
            scope.bind(&local_for_id(&reads.tag), Value::Int(id as i64));
        }
        for (ci, col) in self.columns.iter().enumerate() {
            let v = match col.binary_search_by_key(&id, |(i, _)| *i) {
                Ok(at) => col[at].1.clone(),
                Err(_) => Value::Null,
            };
            scope.bind(&local_for_prop(&reads.tag, &reads.props[ci]), v);
        }
        for (p, ids, _) in &self.presence {
            let present = ids.binary_search(&id).is_ok();
            scope.bind(
                &local_for_prop(&reads.tag, p),
                if present {
                    Value::Bool(true)
                } else {
                    Value::Null
                },
            );
        }
        for (l, members, _) in &self.label_members {
            scope.bind(
                &local_for_label(&reads.tag, l),
                Value::Bool(members.binary_search(&id).is_ok()),
            );
        }
        // A probe answered over the population is read at the id's member
        // position; an id outside the population (never, for a walk over
        // its own members) or a probe kept per member asks the adjacency.
        let member_pos = self.members.binary_search(&id).ok();
        for (pi, p) in reads.probes.iter().enumerate() {
            let hit = match (self.probe_hits.get(pi), member_pos, self.probe_ends.get(pi)) {
                (Some(Some(hits)), Some(pos), _) => hits[pos],
                (_, _, Some(Some(set))) => graph
                    .adjacency_probe_in_set(id, p.dir, &p.types, set)
                    .map_err(RunError::Graph)?,
                _ => graph
                    .adjacency_probe_labeled(id, p.dir, &p.types, &p.labels)
                    .map_err(RunError::Graph)?,
            };
            scope.bind(&p.local, Value::Bool(hit));
        }
        for (local, dir, tokens, dead) in &self.degrees {
            let n = if *dead {
                0
            } else {
                graph.count_adjacent_memo(id, *dir, tokens)
            };
            scope.bind(local, Value::Int(n as i64));
        }
        Ok(())
    }

    /// Bind member `mi` (id `id`) into `scope`: the columns (absent →
    /// Null), the label booleans, the probes, `type(r)`.
    fn bind(
        &mut self,
        graph: &Graph,
        reads: &Reads,
        scope: &mut Scope<'_>,
        mi: usize,
        id: u64,
    ) -> Result<(), RunError> {
        if reads.type_read {
            let v = self
                .type_names
                .get(&self.rel_types[mi])
                .cloned()
                .unwrap_or(Value::Null);
            scope.bind(LOCAL_TYPE, v);
        }
        if reads.id_read {
            counted!("interp.columnar id bound from the walk");
            scope.bind(&local_for_id(&reads.tag), Value::Int(id as i64));
        }
        for (ci, col) in self.columns.iter_mut().enumerate() {
            let cur = &mut self.cursors[ci];
            while *cur < col.len() && col[*cur].0 < id {
                *cur += 1;
            }
            // The cursor never revisits an entry (members ascend, ids are
            // unique), so the value is TAKEN, not cloned — a 163k-row
            // string projection cloned every string here, again into its
            // row, and again into its sort key.
            let v = if *cur < col.len() && col[*cur].0 == id {
                std::mem::replace(&mut col[*cur].1, Value::Null)
            } else {
                Value::Null
            };
            scope.bind(&local_for_prop(&reads.tag, &reads.props[ci]), v);
        }
        for (p, ids, cur) in self.presence.iter_mut() {
            while *cur < ids.len() && ids[*cur] < id {
                *cur += 1;
            }
            let present = *cur < ids.len() && ids[*cur] == id;
            scope.bind(
                &local_for_prop(&reads.tag, p),
                if present {
                    Value::Bool(true)
                } else {
                    Value::Null
                },
            );
        }
        for (l, members, cur) in self.label_members.iter_mut() {
            while *cur < members.len() && members[*cur] < id {
                *cur += 1;
            }
            let has = *cur < members.len() && members[*cur] == id;
            scope.bind(&local_for_label(&reads.tag, l), Value::Bool(has));
        }
        for (pi, p) in reads.probes.iter().enumerate() {
            // Answered over the population at load (fix 36c), else asked of
            // the adjacency per member.
            let hit = match (self.probe_hits.get(pi), self.probe_ends.get(pi)) {
                (Some(Some(hits)), _) => hits[mi],
                (_, Some(Some(set))) => graph
                    .adjacency_probe_in_set(id, p.dir, &p.types, set)
                    .map_err(RunError::Graph)?,
                _ => graph
                    .adjacency_probe_labeled(id, p.dir, &p.types, &p.labels)
                    .map_err(RunError::Graph)?,
            };
            scope.bind(&p.local, Value::Bool(hit));
        }
        for (local, dir, tokens, dead) in &self.degrees {
            let n = if *dead {
                0
            } else {
                graph.count_adjacent_memo(id, *dir, tokens)
            };
            scope.bind(local, Value::Int(n as i64));
        }
        Ok(())
    }
}

/// Run the statement through the columnar scan, or `None` when it is not
/// in the class (the general path takes it).
/// Default member-batch size for the columnar aggregate scan: the scan folds this
/// many members before discarding their materialised column, bounding peak memory
/// to one batch rather than the whole label's column.
pub(crate) const COLUMNAR_AGG_BATCH: usize = 131072;

pub(crate) fn try_columnar_aggregate(
    graph: &Graph,
    q: &SingleQuery,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    if !graph.columnar_scans_enabled() {
        sometimes!("interp.columnar paths switched off", true);
        return Ok(None);
    }
    let Some(plan) = recognise(q) else {
        return Ok(None);
    };

    let mut fold = Fold::new(&plan.items);
    let empty_vars = VarMap::new();
    let mut scope = Scope::over(params, &empty_vars, graph.wall_ms(), graph.zone_provider());
    // A selective property-equality SEEKS the derived range index and folds
    // over the matches - `WHERE c.primaryCountry = 'USA' RETURN count(c)`
    // probes the ids, keeps those under the label, and pushes each into the
    // fold, instead of decoding the column over the whole label. Taken only
    // when the probe BEATS the label scan and the aggregate lifts no probe/
    // degree (the per-id path binds neither). The single `reads` set covers
    // both the predicate and the items, so one bind suffices.
    let mut sought = false;
    // A COVERED COUNT: the predicate is nothing but string equalities on keys
    // with declared indexes for the one label, and the projection is nothing
    // but `count(*)` — the answer is the size of the probes' intersection
    // (within the label's membership), and no record is read.
    if plan.covered && covered_count_applies(&plan) {
        if let Source::Nodes { labels, any_of } = &plan.source {
            if any_of.is_empty() && labels.len() == 1 {
                if let Some(n) =
                    covered_count(graph, &labels[0], &plan.seeks, &plan.prefixes, &plan.ranges, &scope)?
                {
                    sought = true;
                    counted!("interp.statements run");
                    counted!("interp.columnar aggregate scans");
                    sometimes!("interp.columnar aggregate counted an index intersection", true);
                    for _ in 0..n {
                        fold.push(graph, &scope)?;
                    }
                }
            }
        }
    }
    if !sought && plan.reads.probes.is_empty() && plan.reads.degrees.is_empty() {
        if let Source::Nodes { labels, any_of } = &plan.source {
            if any_of.is_empty() {
                if let Some(ids) = columnar_seek_ids(
                    graph,
                    labels,
                    &plan.seeks,
                    &plan.prefixes,
                    &plan.ranges,
                    SeekUse::PerId,
                    &scope,
                )? {
                    sought = true;
                    counted!("interp.statements run");
                    counted!("interp.columnar aggregate scans");
                    sometimes!("interp.columnar aggregate sought a property index", true);
                    // A PROJECTED decode per sought id (fix 34): only the
                    // properties the plan reads, labels always along. The
                    // full decode this was cost the whole record — 37
                    // UserDataNode records with their raw bodies for a
                    // `count(n)` over a `{nodeType, userId}` seek.
                    let want: std::collections::BTreeSet<String> = plan
                        .reads
                        .props
                        .iter()
                        .chain(plan.reads.presence.iter())
                        .cloned()
                        .collect();
                    for id in ids {
                        let node = graph.node_projected(id, &want).map_err(RunError::Graph)?;
                        let Some(Value::Node { labels: nl, .. }) = &node else {
                            continue;
                        };
                        if !labels.iter().all(|l| nl.contains(l)) {
                            continue; // the value carries this prop under another label
                        }
                        scope.locals.clear();
                        bind_from_projected(&plan.reads, &mut scope, node.as_ref());
                        if let Some(pred) = &plan.pred {
                            let v = eval_with(pred, &scope, None).map_err(RunError::Eval)?;
                            match v.truth() {
                                Some(Truth::True) => {}
                                Some(_) => continue,
                                None => {
                                    return Err(RunError::Semantic(format!(
                                        "WHERE takes a boolean, got {}",
                                        v.type_name()
                                    )));
                                }
                            }
                        }
                        fold.push(graph, &scope)?;
                    }
                }
            }
        }
    }
    // A seek whose ids a WALK would consume (`SeekUse::Walk`), computed ONCE
    // here and driven below. When it names fewer than an EIGHTH of the label
    // it is taken BEFORE the column-at-a-time count: that count evaluates
    // every conjunct over every member with no short-circuit, and
    // `g.eventId STARTS WITH 'edgar-8k-' AND … datetime(g.startAt) >=
    // datetime($since)` parsed 44k datetimes (20 ms on the mirror, Neo4j
    // 11) to answer for the 3.9k the prefix names; a walk over the sought
    // ids binds 3.9k rows from the cached columns and short-circuits.
    let mut walk_seek: Option<Vec<u64>> = None;
    let mut prefer_walk = false;
    if !sought {
        if let Source::Nodes { labels, any_of } = &plan.source {
            if any_of.is_empty() {
                walk_seek = columnar_seek_ids(
                    graph,
                    labels,
                    &plan.seeks,
                    &plan.prefixes,
                    &plan.ranges,
                    SeekUse::Walk,
                    &scope,
                )?;
                if let (Some(ids), Some(l)) = (&walk_seek, labels.first()) {
                    prefer_walk = (ids.len() as u64).saturating_mul(8) < graph.count_label_nodes(l);
                }
            }
        }
    }
    // A plain count over ONE label with every column CACHED: evaluated over
    // the columns as vectors, no per-member scope (`count_over_cached_columns`).
    if !sought
        && !prefer_walk
        && count_star_only(&plan.items)
        && plan.reads.probes.is_empty()
        && plan.reads.degrees.is_empty()
        && plan.reads.labels.is_empty()
        && !plan.reads.type_read
    {
        if let Source::Nodes { labels, any_of } = &plan.source {
            if labels.len() == 1 && any_of.is_empty() {
                if let Some(n) = count_over_cached_columns(graph, &labels[0], &plan, &scope)? {
                    sought = true;
                    counted!("interp.statements run");
                    counted!("interp.columnar aggregate scans");
                    counted!("interp.columnar aggregate counted over cached columns");
                    for _ in 0..n {
                        fold.push(graph, &scope)?;
                    }
                }
            }
        }
    }
    if !sought {
        // Fold every member of one loaded walk: bind, apply the WHERE, push into the
        // running fold. Shared by the whole-walk path and each batch below.
        let fold_walk =
            |walk: &mut Walk, scope: &mut Scope, fold: &mut Fold| -> Result<(), RunError> {
                for mi in 0..walk.members.len() {
                    let id = walk.members[mi];
                    scope.locals.clear();
                    walk.bind(graph, &plan.reads, scope, mi, id)?;
                    if let Some(pred) = &plan.pred {
                        let v = eval_with(pred, scope, None).map_err(RunError::Eval)?;
                        match v.truth() {
                            Some(Truth::True) => {}
                            Some(_) => continue,
                            None => {
                                return Err(RunError::Semantic(format!(
                                    "WHERE takes a boolean, got {}",
                                    v.type_name()
                                )));
                            }
                        }
                    }
                    fold.push(graph, scope)?;
                }
                Ok(())
            };

        // A seek WITH a probe or degree: the per-id path above binds neither,
        // but a WALK over the sought ids binds everything a whole-label walk
        // does — columns by gather, probes from adjacency, degrees from the
        // table — so the seek's ids become the walk's population instead of
        // the label. `MATCH (n:UserDataNode {nodeType: 'email', userId: $u})
        // WHERE exists((n)-[:HAS_ASK]->(:EmailAsk)) RETURN count(n)` walked
        // all 38k emails probing each (96 ms on the mirror, Neo4j 2 ms) to
        // answer for the 10 the declared index named. An UNSCOPED probe's
        // ids may carry the property under another label, so the ids are
        // kept to the label's members first; the walk then re-evaluates the
        // whole predicate per id, exactly as the whole-label walk would.
        // Taken for ANY reads now — the per-id seek above declined (a probe
        // or degree read, or a seek wider than its cap), and a walk over
        // the sought ids costs about a column entry per id, so it takes a
        // seek up to eight times wider that still halves the label
        // (`SeekUse::Walk`): `g.eventId STARTS WITH 'edgar-8k-'` names 3.9k
        // of 44k events, past the per-id cap and well inside this one.
        let mut seek_walked = false;
        {
            if let Source::Nodes { labels, any_of } = &plan.source {
                if any_of.is_empty() {
                    if let Some(ids) = walk_seek.take() {
                        if prefer_walk {
                            counted!("interp.columnar aggregate walked a selective seek instead of vectorising");
                        }
                        let members = graph.members_all(labels).map_err(RunError::Graph)?;
                        let over: Vec<u64> = ids
                            .into_iter()
                            .filter(|id| graph.members_contains(&members, *id))
                            .collect();
                        if let Some(mut walk) = load_walk_over(
                            graph,
                            &plan.source,
                            &plan.reads,
                            Some(std::sync::Arc::new(over)),
                            params,
                        )? {
                            seek_walked = true;
                            counted!("interp.statements run");
                            counted!("interp.columnar aggregate scans");
                            counted!("interp.columnar aggregate walked its probes over a seek");
                            note_walk_events(&plan.source, &plan.reads, &walk);
                            fold_walk(&mut walk, &mut scope, &mut fold)?;
                        }
                    }
                }
            }
        }

        // A LARGE Nodes label is scanned in member BATCHES: `load_walk_over` supplies
        // one contiguous batch's members, so only that batch's column is materialised
        // (BI3's ~1.5M-row language column drops to one batch); the fold accumulates
        // across batches and the members arrive in the SAME sorted order, so the
        // result is byte-identical to a single whole-walk fold. Rels and small labels
        // take the whole-walk path unchanged.
        let batch_members = match &plan.source {
            _ if seek_walked => None,
            Source::Nodes { labels, any_of } if graph.columnar_agg_batch_enabled() => {
                let m = if !any_of.is_empty() {
                    graph.members_any(any_of).map_err(RunError::Graph)?
                } else {
                    graph.members_all(labels).map_err(RunError::Graph)?
                };
                // A single label whose columns are NOT all cached yet walks
                // WHOLE once, so the columns it assembles are kept
                // (`Graph::prop_column` keeps only whole-label columns); the
                // next read batches against the cache. Only when the whole
                // column would fit the cache budget — a column wider than
                // the budget is never kept, so it keeps batching for memory.
                let populate_first = labels.len() == 1
                    && any_of.is_empty()
                    && m.len().saturating_mul(64) <= graph.prop_column_budget()
                    && !graph.prop_columns_current(
                        &labels[0],
                        &plan.reads.props,
                        &plan.reads.presence_only(),
                    );
                if populate_first {
                    counted!("interp.columnar aggregate walked whole to keep its columns");
                }
                (!populate_first && m.len() > graph.columnar_agg_batch_size()).then_some(m)
            }
            _ => None,
        };

        if seek_walked {
            // Folded above, over the seek's ids.
        } else if let Some(members) = batch_members {
            counted!("interp.statements run");
            counted!("interp.columnar aggregate scans");
            counted!("interp.columnar aggregate batched");
            sometimes!("interp.columnar aggregate scan ran", true);
            let bsize = graph.columnar_agg_batch_size();
            let mut first = true;
            let members = members.to_arc_vec();
            for batch in members.chunks(bsize) {
                let over = std::sync::Arc::new(batch.to_vec());
                let Some(mut walk) = load_walk_over(graph, &plan.source, &plan.reads, Some(over), params)?
                else {
                    return Ok(None);
                };
                if first {
                    note_walk_events(&plan.source, &plan.reads, &walk);
                    if plan.reads.type_read {
                        sometimes!("interp.columnar scan bound type(r) from its token", true);
                    }
                    first = false;
                }
                fold_walk(&mut walk, &mut scope, &mut fold)?;
            }
        } else {
            let Some(mut walk) = load_walk(graph, &plan.source, &plan.reads, params)? else {
                return Ok(None);
            };
            counted!("interp.statements run");
            counted!("interp.columnar aggregate scans");
            sometimes!("interp.columnar aggregate scan ran", true);
            if !plan.reads.probes.is_empty() {
                sometimes!("interp.columnar scan lifted an exists probe", true);
            }
            if matches!(plan.source, Source::Rels { .. }) {
                counted!("interp.columnar rel aggregate scans");
                sometimes!("interp.columnar scan ran over relationships", true);
            }
            note_walk_events(&plan.source, &plan.reads, &walk);
            if plan.reads.type_read {
                sometimes!("interp.columnar scan bound type(r) from its token", true);
            }
            fold_walk(&mut walk, &mut scope, &mut fold)?;
        }
    }
    let spec = FoldSpec {
        items: &plan.items,
        columns: &plan.columns,
        order: &plan.order,
        skip: plan.skip.as_ref(),
        limit: plan.limit.as_ref(),
        final_: plan.final_.as_ref(),
    };
    fold.finish(graph, params, &spec, &mut scope).map(Some)
}

/// The aggregating fold shared by the node, relationship and hop scans:
/// groups keyed on the canonical key in first-seen order, one accumulator
/// per aggregate site.
struct Fold<'p> {
    sites: Vec<(&'p AggSite, Option<&'p Expr>)>,
    key_exprs: Vec<&'p Expr>,
    group_index: BTreeMap<Vec<u8>, usize>,
    groups: Vec<(Vec<Value>, Vec<SiteAcc>)>,
    nonce: u64,
}

/// What the fold projects at the end.
struct FoldSpec<'p> {
    items: &'p [Item],
    columns: &'p [String],
    order: &'p [(usize, bool)],
    skip: Option<&'p Expr>,
    limit: Option<&'p Expr>,
    final_: Option<&'p Final>,
}

impl<'p> Fold<'p> {
    fn new(items: &'p [Item]) -> Self {
        let sites = items
            .iter()
            .filter_map(|it| match it {
                Item::Agg(s, a) => Some((s, a.as_ref())),
                Item::Key(_) => None,
            })
            .collect();
        let key_exprs = items
            .iter()
            .filter_map(|it| match it {
                Item::Key(e) => Some(e),
                Item::Agg(..) => None,
            })
            .collect();
        Fold {
            sites,
            key_exprs,
            group_index: BTreeMap::new(),
            groups: Vec::new(),
            nonce: 0,
        }
    }

    /// Fold one bound row (its locals already in `scope`).
    fn push(&mut self, graph: &Graph, scope: &Scope<'_>) -> Result<(), RunError> {
        let mut key = Vec::with_capacity(self.key_exprs.len());
        for k in &self.key_exprs {
            key.push(eval_with(k, scope, None).map_err(RunError::Eval)?);
        }
        let ser = agg_key_of(&key, &mut self.nonce);
        let gi = match self.group_index.get(&ser) {
            Some(&i) => i,
            None => {
                self.groups.push((
                    key,
                    self.sites
                        .iter()
                        .map(|(s, _)| SiteAcc::for_site(s))
                        .collect(),
                ));
                self.group_index.insert(ser, self.groups.len() - 1);
                budget_check(graph, self.groups.len())?;
                self.groups.len() - 1
            }
        };
        let accs = &mut self.groups[gi].1;
        for ((site, arg), acc) in self.sites.iter().zip(accs.iter_mut()) {
            let v = if site.star {
                None
            } else {
                let a = arg.expect("non-star site has an argument");
                Some(eval_with(a, scope, None).map_err(RunError::Eval)?)
            };
            acc.push(v)?;
        }
        Ok(())
    }

    /// Finish: the zero-rows rule, the group rows, ORDER/SKIP/LIMIT, and
    /// the RETURN over the WITH's aliases when there is one.
    fn finish(
        mut self,
        graph: &Graph,
        params: &BTreeMap<String, Value>,
        spec: &FoldSpec<'_>,
        scope: &mut Scope<'_>,
    ) -> Result<QueryResult, RunError> {
        if self.groups.is_empty() && self.key_exprs.is_empty() {
            self.groups.push((
                Vec::new(),
                self.sites
                    .iter()
                    .map(|(s, _)| SiteAcc::for_site(s))
                    .collect(),
            ));
        }
        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(self.groups.len());
        for (key, accs) in self.groups {
            let mut keys = key.into_iter();
            let mut accs = accs.into_iter();
            let mut out = Vec::with_capacity(spec.items.len());
            for it in spec.items {
                out.push(match it {
                    Item::Key(_) => keys.next().expect("key per key item"),
                    Item::Agg(site, _) => accs.next().expect("acc per site").finish(site)?,
                });
            }
            rows.push(out);
        }
        rows = order_and_page_by_column(graph, params, rows, spec.order, spec.skip, spec.limit)?;
        let Some(fin) = spec.final_ else {
            return Ok(QueryResult {
                columns: spec.columns.to_vec(),
                rows,
            });
        };
        // The RETURN over the WITH's aliases: one evaluation per group row
        // with the aliases bound as locals (and the RETURN's own aliases
        // for ORDER).
        let mut out_rows = Vec::with_capacity(rows.len());
        let mut order_keys = Vec::with_capacity(rows.len());
        for r in &rows {
            scope.locals.clear();
            for (c, v) in spec.columns.iter().zip(r) {
                scope.bind(c, v.clone());
            }
            let mut out = Vec::with_capacity(fin.items.len());
            for e in &fin.items {
                out.push(eval_with(e, scope, None).map_err(RunError::Eval)?);
            }
            for (c, v) in fin.columns.iter().zip(&out) {
                scope.bind(c, v.clone());
            }
            let mut k = Vec::with_capacity(fin.order.len());
            for o in &fin.order {
                k.push(eval_with(&o.expr, scope, None).map_err(RunError::Eval)?);
            }
            out_rows.push(out);
            order_keys.push(k);
        }
        let out_rows = order_and_page(
            graph,
            params,
            out_rows,
            &fin.order,
            order_keys,
            fin.skip.as_ref(),
            fin.limit.as_ref(),
        )?;
        Ok(QueryResult {
            columns: fin.columns.clone(),
            rows: out_rows,
        })
    }
}

/// A projected item: a rewritten expression over the locals, or the bare
/// scanned node — materialised LATE, for the rows that survive the filter,
/// the order and the page.
enum ProjItemPlan {
    Expr(Expr),
    Bare,
}

/// A non-aggregating projection over the population: `MATCH (n:L…) [WHERE
/// p] RETURN <exprs over n.props | n> [ORDER BY …] [SKIP s] [LIMIT k]`.
struct ProjPlan {
    source: Source,
    pred: Option<Expr>,
    /// What the predicate reads — loaded FIRST, over the population; the
    /// items' reads (`reads`) are loaded over the survivors only. A
    /// projection that keeps 136 of thousands read the whole `context`
    /// column (blobs) for everyone before filtering.
    pred_reads: Reads,
    reads: Reads,
    items: Vec<ProjItemPlan>,
    columns: Vec<String>,
    /// `RETURN DISTINCT`: deduplicate on the output (a bare node by its
    /// id) in scan order, BEFORE the ordering and the page.
    distinct: bool,
    /// ORDER BY over output aliases (bound as locals) or rewritten
    /// expressions over the variable.
    order: Vec<OrderItem>,
    /// When every ORDER BY item names a projected (non-bare) column — by
    /// alias or by the same expression — the columns themselves are the
    /// keys: no key vector, no clone.
    order_by_column: Option<Vec<(usize, bool)>>,
    /// Two phases (the predicate's reads over the population, the items'
    /// reads over the survivors) — a node source WITH a predicate. Without
    /// one every member survives and the second pass would only repeat
    /// the first; a relationship source cannot be narrowed to survivors.
    two_phase: bool,
    /// Every property-equality the WHERE carries - `var.prop = x` with `x`
    /// reading no variable - that the derived range index could SEEK instead
    /// of scanning the label. `columnar_seek_ids` picks among them; a seek is
    /// taken only when the probe beats the label scan, else the columnar walk
    /// runs untouched.
    seeks: Vec<(String, Vec<Expr>)>,
    /// `var.prop STARTS WITH x` candidates — see `Plan::prefixes`.
    prefixes: Vec<(String, Expr)>,
    /// `var.prop < / <= / > / >= x` candidates — see `Plan::ranges`.
    ranges: Vec<(String, engram_cypher::BinOp, Expr)>,
    skip: Option<Expr>,
    limit: Option<Expr>,
}

fn recognise_projection(q: &SingleQuery) -> Option<ProjPlan> {
    let [m @ Clause::Match { .. }, Clause::Return { proj }] = q.clauses.as_slice() else {
        return None;
    };
    if proj.star || proj.items.is_empty() {
        return None;
    }
    let (var, kind, source, full_where) = recognise_source(m)?;
    // An identity equality (`id(n) = $x` / `elementId(n) = $x`) is the
    // general path's one-get seek; with `id(var)` a walk local (fix 46) this
    // scan would otherwise claim it and read the label.
    if crate::interp::id_seek_expr(full_where.as_ref(), &var).is_some() {
        return None;
    }
    let seeks = prop_eq_candidates(full_where.as_ref(), &var);
    let prefixes = crate::interp::prop_prefix_candidates(full_where.as_ref(), &var);
    let ranges = crate::interp::prop_range_candidates(full_where.as_ref(), &var);
    let mut pred_reads = Reads::default();
    let mut reads = Reads::default();
    let pred = match &full_where {
        None => None,
        Some(w) => Some(rewrite(w, &var, kind, &mut pred_reads)?),
    };
    // See `recognise`: a surviving graph-dependent subquery has no hooks here.
    if pred.as_ref().is_some_and(contains_opaque) {
        return None;
    }
    let mut items = Vec::with_capacity(proj.items.len());
    let mut columns = Vec::with_capacity(proj.items.len());
    for (i, it) in proj.items.iter().enumerate() {
        if !reads_only(&it.expr, std::slice::from_ref(&var)) {
            return None;
        }
        columns.push(
            it.alias
                .clone()
                .or_else(|| it.text.clone())
                .unwrap_or_else(|| column_name(&it.expr, i)),
        );
        items.push(match &it.expr {
            // The bare node: materialised late. A bare relationship has
            // no late path yet — declined.
            Expr::Var(v) if *v == var && kind == Kind::Node => ProjItemPlan::Bare,
            e => {
                let re = rewrite(e, &var, kind, &mut reads)?;
                // A surviving graph-dependent subquery has no hooks here.
                if contains_opaque(&re) {
                    return None;
                }
                ProjItemPlan::Expr(re)
            }
        });
    }
    let bare: Vec<&String> = columns
        .iter()
        .zip(&items)
        .filter(|(_, it)| matches!(it, ProjItemPlan::Bare))
        .map(|(c, _)| c)
        .collect();
    let mut order = Vec::with_capacity(proj.order.len());
    for o in &proj.order {
        let e = match &o.expr {
            Expr::Var(v) if columns.contains(v) => {
                if bare.contains(&v) {
                    return None; // unmaterialised at sort time
                }
                Expr::Var(v.clone())
            }
            e => rewrite(e, &var, kind, &mut reads)?,
        };
        // A surviving graph-dependent subquery in an ORDER BY key has no
        // hooks here either (`ORDER BY COUNT { MATCH (parent:K)-[:HAS]->(w) }`
        // errored "COUNT {} requires a graph context").
        if contains_opaque(&e) {
            return None;
        }
        // Anything else read here is unbound: only aliases and locals.
        let mut free = Vec::new();
        free_vars_of(&e, &mut free);
        if free
            .iter()
            .any(|f| !columns.contains(f) || bare.contains(&f))
            && free.iter().any(|f| !f.starts_with("__"))
        {
            return None;
        }
        order.push(OrderItem {
            expr: e,
            desc: o.desc,
        });
    }
    for e in [&proj.skip, &proj.limit].into_iter().flatten() {
        let mut free = Vec::new();
        free_vars_of(e, &mut free);
        if !free.is_empty() {
            return None;
        }
    }
    let order_by_column: Option<Vec<(usize, bool)>> = proj
        .order
        .iter()
        .map(|o| {
            let ix = match &o.expr {
                Expr::Var(v) => columns.iter().position(|c| c == v),
                e => proj.items.iter().position(|it| it.expr == *e),
            }?;
            if matches!(items[ix], ProjItemPlan::Bare) {
                return None;
            }
            Some((ix, o.desc))
        })
        .collect();
    // Two phases — the predicate's reads over the population, the items'
    // over the survivors — pay off only for a NODE source WITH a predicate
    // whose items read a column the predicate never touches: that column
    // is then read over the survivors alone. Otherwise one walk binds
    // both. Measured on the production port: the first cut ran every node
    // source in two phases (a 163k-row predicate-less projection paid a
    // pass that bound nothing), and the second ran them whenever there
    // was a predicate (`WHERE n.userId IS NOT NULL RETURN DISTINCT
    // n.userId` read `userId` twice — presence, then values — on top of
    // `nodeType`: 183 → 275 ms). A relationship source cannot be narrowed
    // to survivors at all.
    let two_phase = kind == Kind::Node && pred.is_some() && reads.has_column_beyond(&pred_reads);
    if !two_phase {
        reads.merge(std::mem::take(&mut pred_reads));
    }
    Some(ProjPlan {
        source,
        pred,
        pred_reads,
        reads,
        items,
        columns,
        distinct: proj.distinct,
        order,
        order_by_column,
        two_phase,
        seeks,
        prefixes,
        ranges,
        skip: proj.skip.clone(),
        limit: proj.limit.clone(),
    })
}

/// Bind the items' locals for one survivor from a projected node get —
/// the fallback when the survivors' span is too wide for a column read.
/// Probes and degrees are not bound here; a plan that reads them declines
/// this path.
fn bind_from_projected(reads: &Reads, scope: &mut Scope<'_>, node: Option<&Value>) {
    let (labels, props): (&[String], Option<&BTreeMap<String, Value>>) = match node {
        Some(Value::Node { labels, props, .. }) => (labels, Some(props)),
        _ => (&[], None),
    };
    if reads.id_read {
        let id = match node {
            Some(Value::Node { id, .. }) => Value::Int(*id as i64),
            _ => Value::Null,
        };
        scope.bind(&local_for_id(&reads.tag), id);
    }
    for p in &reads.props {
        let v = props.and_then(|m| m.get(p)).cloned().unwrap_or(Value::Null);
        scope.bind(&local_for_prop(&reads.tag, p), v);
    }
    for p in reads.presence_only() {
        let present = props.is_some_and(|m| m.contains_key(&p));
        scope.bind(
            &local_for_prop(&reads.tag, &p),
            if present {
                Value::Bool(true)
            } else {
                Value::Null
            },
        );
    }
    for l in &reads.labels {
        scope.bind(
            &local_for_label(&reads.tag, l),
            Value::Bool(labels.contains(l)),
        );
    }
}

/// Evaluate the items (and the key vector when the order is not by
/// column) for one bound survivor, pushing the row with its id trailing.
fn project_row(
    plan: &ProjPlan,
    scope: &mut Scope<'_>,
    id: u64,
    rows: &mut Vec<Vec<Value>>,
    keys: &mut Vec<Vec<Value>>,
) -> Result<(), RunError> {
    let mut out = Vec::with_capacity(plan.items.len() + 1);
    for it in &plan.items {
        out.push(match it {
            ProjItemPlan::Expr(e) => eval_with(e, scope, None).map_err(RunError::Eval)?,
            ProjItemPlan::Bare => Value::Null,
        });
    }
    let mut k = Vec::new();
    if plan.order_by_column.is_none() && !plan.order.is_empty() {
        for (c, v) in plan.columns.iter().zip(&out) {
            scope.bind(c, v.clone());
        }
        k.reserve(plan.order.len());
        for o in &plan.order {
            k.push(eval_with(&o.expr, scope, None).map_err(RunError::Eval)?);
        }
    }
    out.push(Value::Int(id as i64));
    rows.push(out);
    keys.push(k);
    Ok(())
}

/// The columnar PROJECTION scan with late materialisation: the filter, the
/// projected expressions and the order keys are evaluated from columns;
/// the rows are ordered and paged; only then is a bare `n` materialised,
/// for the rows that remain. `MATCH (e:WorkflowExecution) WHERE e.status
/// IN […] RETURN e ORDER BY e.started_at DESC LIMIT 500` decoded every
/// candidate in full to keep 500 (5.2 s on the production port); `MATCH
/// (s:Bio:Species) RETURN s.taxonId, s.scientificName, s.commonName ORDER
/// BY s.commonName` (163k rows, 5.7 s) built a node and a row per member
/// to read three properties. Rows without ORDER BY come out in member
/// (id) order — the general path's order over the same population.
pub(crate) fn try_columnar_projection(
    graph: &Graph,
    q: &SingleQuery,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    if !graph.columnar_scans_enabled() {
        sometimes!("interp.columnar paths switched off", true);
        return Ok(None);
    }
    let Some(plan) = recognise_projection(q) else {
        return Ok(None);
    };
    let two_phase = plan.two_phase;
    // A LIMIT with no ORDER BY and no DISTINCT is satisfied by ANY skip+limit
    // rows, so the scan stops once it holds them rather than building a row
    // per member and truncating. `MATCH (n:Bio) RETURN n LIMIT 100` built a
    // row for every Bio (110 ms on the production port) to keep 100.
    let early_cap: Option<usize> = if plan.order.is_empty() && !plan.distinct {
        match eval_count(graph, plan.limit.as_ref(), params, "LIMIT")? {
            Some(lim) => {
                let skip = eval_count(graph, plan.skip.as_ref(), params, "SKIP")?.unwrap_or(0);
                Some(skip.saturating_add(lim))
            }
            None => None,
        }
    } else {
        None
    };
    if !two_phase && matches!(plan.source, Source::Nodes { .. }) {
        counted!("interp.columnar projection single-phase nodes");
    }
    let empty_vars = VarMap::new();
    let mut scope = Scope::over(params, &empty_vars, graph.wall_ms(), graph.zone_provider());
    let truth_of = |v: Value| -> Result<bool, RunError> {
        match v.truth() {
            Some(Truth::True) => Ok(true),
            Some(_) => Ok(false),
            None => Err(RunError::Semantic(format!(
                "WHERE takes a boolean, got {}",
                v.type_name()
            ))),
        }
    };
    let has_bare = plan.items.iter().any(|it| matches!(it, ProjItemPlan::Bare));
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut keys: Vec<Vec<Value>> = Vec::new();
    // A selective property-equality SEEKS the derived range index rather
    // than scanning the label: `WHERE c.primaryCountry = 'USA'` probes the
    // ids carrying the value, keeps those under the label (checked per id
    // against the materialised node), and projects each - O(matches) node
    // gets, with no property column decoded over the whole label. Taken
    // only when the probe BEATS the label scan and the projection needs no
    // probe/degree bind (the per-id path supplies neither). The full node
    // binds BOTH read sets: `pred_reads` is emptied into `reads` unless the
    // plan is two-phase, so binding both is correct either way. The
    // columnar walk below is then skipped.
    let mut sought = false;
    if plan.reads.probes.is_empty() && plan.reads.degrees.is_empty() {
        if let Source::Nodes { labels, any_of } = &plan.source {
            if any_of.is_empty() {
                if let Some(ids) = columnar_seek_ids(
                    graph,
                    labels,
                    &plan.seeks,
                    &plan.prefixes,
                    &plan.ranges,
                    SeekUse::PerId,
                    &scope,
                )? {
                    sought = true;
                    counted!("interp.statements run");
                    counted!("interp.columnar projection scans");
                    sometimes!("interp.columnar projection sought a property index", true);
                    // A PROJECTED decode per sought id (fix 34): the
                    // predicate's and the items' reads, labels always along.
                    // The full decode this was cost the whole record: the
                    // email listing's `{nodeType, userId}` seek decoded 37
                    // UserDataNode records with their raw bodies to project
                    // `n.nodeId` for ten rows (2.5 ms on the mirror against
                    // Neo4j's 1.0).
                    let want: std::collections::BTreeSet<String> = plan
                        .pred_reads
                        .props
                        .iter()
                        .chain(plan.pred_reads.presence.iter())
                        .chain(plan.reads.props.iter())
                        .chain(plan.reads.presence.iter())
                        .cloned()
                        .collect();
                    for id in ids {
                        let node = graph.node_projected(id, &want).map_err(RunError::Graph)?;
                        let Some(Value::Node { labels: nl, .. }) = &node else {
                            continue;
                        };
                        if !labels.iter().all(|l| nl.contains(l)) {
                            continue; // the value carries this prop under another label
                        }
                        scope.locals.clear();
                        bind_from_projected(&plan.pred_reads, &mut scope, node.as_ref());
                        bind_from_projected(&plan.reads, &mut scope, node.as_ref());
                        if let Some(pred) = &plan.pred {
                            let pv = eval_with(pred, &scope, None).map_err(RunError::Eval)?;
                            if !truth_of(pv)? {
                                continue;
                            }
                        }
                        project_row(&plan, &mut scope, id, &mut rows, &mut keys)?;
                        budget_check(graph, rows.len())?;
                        if early_cap.is_some_and(|c| rows.len() >= c) {
                            break;
                        }
                    }
                }
            }
        }
    }
    if !sought {
        // Fix 40: a ONE-label node source whose predicate reads only cached
        // value / presence columns is judged COLUMN-AT-A-TIME
        // (`survivors_over_cached_columns`): no scope bound and no
        // expression walked per member, and a bare LIMIT stops at its k-th
        // survivor. A two-phase statement then goes straight to phase 2
        // with the survivors; a single-phase one loads its walk for the
        // items and binds the survivors alone. A column not yet cached, or
        // a predicate the vectoriser declines, keeps the per-member walk
        // (which assembles and keeps the columns for the next statement).
        let phase1_reads = if two_phase { &plan.pred_reads } else { &plan.reads };
        let vector_label: Option<&str> = match (&plan.pred, &plan.source) {
            (Some(_), Source::Nodes { labels, any_of })
                if labels.len() == 1
                    && any_of.is_empty()
                    && phase1_reads.labels.is_empty()
                    && phase1_reads.probes.is_empty()
                    && phase1_reads.degrees.is_empty()
                    && !phase1_reads.type_read =>
            {
                Some(labels[0].as_str())
            }
            _ => None,
        };
        // Single phase (relationships): the items bind from the same walk, so
        // the rows are produced here.
        let mut single_phase_rows: Vec<(u64, usize)> = Vec::new(); // (id, member index)
        let mut vector_phase1: Option<(Vec<u64>, usize)> = None; // (survivors, population)
        if two_phase {
            if let (Some(label), Some(pred)) = (vector_label, &plan.pred) {
                let members = graph
                    .members_all(std::slice::from_ref(&label.to_string()))
                    .map_err(RunError::Graph)?
                    .to_arc_vec();
                if let Some(hits) = survivors_over_cached_columns(
                    graph,
                    label,
                    pred,
                    &plan.pred_reads,
                    &members,
                    early_cap,
                    &scope,
                ) {
                    counted!("interp.columnar projection predicate evaluated column-at-a-time");
                    if early_cap.is_some_and(|c| hits.len() >= c) {
                        counted!("interp.columnar projection stopped at the limit");
                    }
                    let survivors: Vec<u64> = hits.into_iter().map(|mi| members[mi]).collect();
                    vector_phase1 = Some((survivors, members.len()));
                }
            }
        }
        let (survivors, population) = match vector_phase1 {
            Some(done) => done,
            None => {
                // Phase 1 — the predicate over the population, reading only
                // what it needs; the survivors' ids are all that goes to
                // phase 2.
                // Fix 52: a PLAIN limit over a predicate-less one-label
                // source needs only its first `skip + limit` members — the
                // walk (and the column it would assemble and keep) is cut
                // to them. `MATCH (s:Story) RETURN s.storyId LIMIT 3` read
                // the whole label's column for three rows.
                let capped: Option<std::sync::Arc<Vec<u64>>> = match (early_cap, &plan.pred, &plan.source) {
                    (Some(cap), None, Source::Nodes { labels, any_of })
                        if labels.len() == 1 && any_of.is_empty() =>
                    {
                        let members = graph.members_all(labels).map_err(RunError::Graph)?;
                        let ids: Vec<u64> = members.iter().take(cap).collect();
                        counted!("interp.columnar projection walk cut at the plain limit");
                        Some(std::sync::Arc::new(ids))
                    }
                    _ => None,
                };
                let Some(mut pwalk) =
                    load_walk_over(graph, &plan.source, phase1_reads, capped, params)?
                else {
                    return Ok(None);
                };
                let mut survivors: Vec<u64> = Vec::new();
                // Single phase over a label: the predicate column-at-a-time
                // over the walk's own members, the items bound from the walk
                // for the survivors alone.
                let vector_hits: Option<Vec<usize>> = match (two_phase, vector_label, &plan.pred)
                {
                    (false, Some(label), Some(pred)) => survivors_over_cached_columns(
                        graph,
                        label,
                        pred,
                        &plan.reads,
                        &pwalk.members,
                        early_cap,
                        &scope,
                    ),
                    _ => None,
                };
                if let Some(hits) = vector_hits {
                    counted!("interp.columnar projection predicate evaluated column-at-a-time");
                    for mi in hits {
                        let id = pwalk.members[mi];
                        scope.locals.clear();
                        pwalk.bind(graph, &plan.reads, &mut scope, mi, id)?;
                        single_phase_rows.push((id, mi));
                        project_row(&plan, &mut scope, id, &mut rows, &mut keys)?;
                        budget_check(graph, rows.len())?;
                    }
                    if early_cap.is_some_and(|c| rows.len() >= c) {
                        counted!("interp.columnar projection stopped at the limit");
                    }
                } else {
                    for mi in 0..pwalk.members.len() {
                        let id = pwalk.members[mi];
                        scope.locals.clear();
                        pwalk.bind(graph, phase1_reads, &mut scope, mi, id)?;
                        if let Some(pred) = &plan.pred {
                            let v = eval_with(pred, &scope, None).map_err(RunError::Eval)?;
                            if !truth_of(v)? {
                                continue;
                            }
                        }
                        if two_phase {
                            survivors.push(id);
                            if early_cap.is_some_and(|c| survivors.len() >= c) {
                                counted!("interp.columnar projection stopped at the limit");
                                break;
                            }
                            continue;
                        }
                        single_phase_rows.push((id, mi));
                        project_row(&plan, &mut scope, id, &mut rows, &mut keys)?;
                        budget_check(graph, rows.len())?;
                        if early_cap.is_some_and(|c| rows.len() >= c) {
                            counted!("interp.columnar projection stopped at the limit");
                            break;
                        }
                    }
                }
                let population = pwalk.members.len();
                drop(pwalk);
                (survivors, population)
            }
        };
        // Phase 2 — the items over the survivors: a column walk bounded to
        // their span when it fits the budget, else one projected get each
        // (the 136-of-thousands case), else decline.
        if two_phase {
            let survivors = std::sync::Arc::new(survivors);
            // A projected get costs about as much as visiting this many column
            // entries, so the walk over the survivors' span is allowed that
            // many entries per survivor before the per-get path wins. The
            // first cut used the generic 4 x survivors: tens of thousands of
            // survivors spread over a label's span declined to tens of
            // thousands of gets, and `RETURN DISTINCT n.userId` over the
            // emails went 183 -> 275 ms.
            const PER_SURVIVOR_GET_ENTRIES: usize = 8; // x the column budget factor (4)
            let over_walk = load_walk_budgeted(
                graph,
                &plan.source,
                &plan.reads,
                Some(std::sync::Arc::clone(&survivors)),
                Some(survivors.len().saturating_mul(PER_SURVIVOR_GET_ENTRIES)),
                params,
            )?;
            match over_walk {
                Some(mut walk) => {
                    if survivors.len() < population {
                        sometimes!(
                            "interp.columnar projection read items over the survivors",
                            true
                        );
                    }
                    counted!("interp.statements run");
                    counted!("interp.columnar projection scans");
                    sometimes!("interp.columnar projection scan ran", true);
                    note_walk_events(&plan.source, &plan.reads, &walk);
                    for mi in 0..walk.members.len() {
                        let id = walk.members[mi];
                        scope.locals.clear();
                        walk.bind(graph, &plan.reads, &mut scope, mi, id)?;
                        project_row(&plan, &mut scope, id, &mut rows, &mut keys)?;
                        budget_check(graph, rows.len())?;
                    }
                }
                None => {
                    if !plan.reads.probes.is_empty() || !plan.reads.degrees.is_empty() {
                        return Ok(None); // nothing binds those without a walk
                    }
                    counted!("interp.statements run");
                    counted!("interp.columnar projection scans");
                    sometimes!("interp.columnar projection scan ran", true);
                    // (No coverage claim here: property columns now GATHER on a
                    // sparse/wide decline, so this per-survivor branch is reached only by
                    // a presence/label-set decline — a defensive residual, not a sweep state.)
                    let want: std::collections::BTreeSet<String> = plan
                        .reads
                        .props
                        .iter()
                        .chain(plan.reads.presence.iter())
                        .cloned()
                        .collect();
                    for &id in survivors.iter() {
                        let node = graph.node_projected(id, &want).map_err(RunError::Graph)?;
                        scope.locals.clear();
                        bind_from_projected(&plan.reads, &mut scope, node.as_ref());
                        project_row(&plan, &mut scope, id, &mut rows, &mut keys)?;
                        budget_check(graph, rows.len())?;
                    }
                }
            }
        } else {
            counted!("interp.statements run");
            counted!("interp.columnar projection scans");
            sometimes!("interp.columnar projection scan ran", true);
            sometimes!("interp.columnar projection ran over relationships", true);
            let _ = single_phase_rows;
        }
    } // end if !sought - the property-index seek filled rows/keys itself
    if plan.distinct {
        // First occurrence wins, in scan order; a bare node dedupes by id
        // (its placeholder is the trailing id column).
        let mut seen: std::collections::BTreeSet<Vec<u8>> = std::collections::BTreeSet::new();
        let mut nonce = 0u64;
        let mut kept_rows = Vec::with_capacity(rows.len());
        let mut kept_keys = Vec::with_capacity(keys.len());
        for (r, k) in rows.into_iter().zip(keys) {
            let last = r.len() - 1;
            let ident: Vec<Value> = plan
                .items
                .iter()
                .enumerate()
                .map(|(i, it)| match it {
                    ProjItemPlan::Bare => r[last].clone(),
                    ProjItemPlan::Expr(_) => r[i].clone(),
                })
                .collect();
            if seen.insert(agg_key_of(&ident, &mut nonce)) {
                kept_rows.push(r);
                kept_keys.push(k);
            }
        }
        sometimes!("interp.columnar projection deduplicated", true);
        rows = kept_rows;
        keys = kept_keys;
    }
    let rows = match &plan.order_by_column {
        Some(by_col) if !by_col.is_empty() => order_and_page_by_column(
            graph,
            params,
            rows,
            by_col,
            plan.skip.as_ref(),
            plan.limit.as_ref(),
        )?,
        _ => order_and_page(
            graph,
            params,
            rows,
            &plan.order,
            keys,
            plan.skip.as_ref(),
            plan.limit.as_ref(),
        )?,
    };
    if has_bare {
        sometimes!(
            "interp.columnar projection materialised the winners late",
            true
        );
    }
    let mut out_rows = Vec::with_capacity(rows.len());
    for mut r in rows {
        let Some(Value::Int(id)) = r.pop() else {
            return Err(RunError::Semantic("projection row lost its id".into()));
        };
        if has_bare {
            let node = graph.node(id as u64).map_err(RunError::Graph)?;
            for (slot, it) in r.iter_mut().zip(&plan.items) {
                if matches!(it, ProjItemPlan::Bare) {
                    *slot = node.clone().unwrap_or(Value::Null);
                }
            }
        }
        out_rows.push(r);
    }
    Ok(Some(QueryResult {
        columns: plan.columns,
        rows: out_rows,
    }))
}

/// The ids of `labels` that satisfy `pred` — a column-filtered SEED for
/// the general path. `MATCH (e:WorkflowExecution) WHERE e.origin IS NULL
/// OPTIONAL MATCH (w:Workflow {workflow_id: e.workflow_id}) RETURN …
/// e.context …` materialised every WorkflowExecution (with its `context`
/// blob) to keep 136: the conjuncts reading only the start variable are
/// evaluated here from columns first, and only the survivors are
/// materialised. A SOUND prefilter, never a replacement: a row is dropped
/// only on a definite False or Unknown (the full WHERE could not be True),
/// and a non-boolean keeps the row so the full WHERE reproduces its own
/// error. `None` when the predicate is not rewritable over `var`, or the
/// walk declines (a column wider than the label) — the scan proceeds as
/// before.
pub(crate) fn filter_ids(
    graph: &Graph,
    labels: &[String],
    var: &str,
    pred: &Expr,
    params: &BTreeMap<String, Value>,
) -> Result<Option<std::sync::Arc<Vec<u64>>>, RunError> {
    filter_ids_in(graph, labels, var, pred, params, None)
}

/// [`filter_ids`] over a SUPPLIED population (`over`, ascending) instead of
/// the labels' members — a seek's candidates, re-checked against the whole
/// predicate. `None` walks the labels as `filter_ids` does.
fn filter_ids_in(
    graph: &Graph,
    labels: &[String],
    var: &str,
    pred: &Expr,
    params: &BTreeMap<String, Value>,
    over: Option<std::sync::Arc<Vec<u64>>>,
) -> Result<Option<std::sync::Arc<Vec<u64>>>, RunError> {
    filter_ids_mode(graph, labels, var, pred, params, over, false)
}

/// [`filter_ids_in`] as a VERDICT rather than a prefilter: the ids the
/// predicate holds TRUE on, or `None` the moment a row answers with a
/// non-boolean (a type error the general path raises — declining hands the
/// whole decision back rather than guessing). A caller that takes `Some`
/// may DROP the predicate: nothing remains to re-check. The pipeline's
/// seed predicates are the caller — they re-gathered the seed column per
/// statement through `load_var_columns`, which has no cache.
pub(crate) fn filter_ids_strict(
    graph: &Graph,
    labels: &[String],
    var: &str,
    pred: &Expr,
    params: &BTreeMap<String, Value>,
    over: Option<std::sync::Arc<Vec<u64>>>,
) -> Result<Option<std::sync::Arc<Vec<u64>>>, RunError> {
    filter_ids_mode(graph, labels, var, pred, params, over, true)
}

/// One `(id, value)` column per requested property, each ascending by id.
pub(crate) type IdColumns = Vec<Vec<(u64, Value)>>;

/// The VALUE columns of `props` over ONE label's whole membership, each
/// ascending by id and aligned to `props` — served from the property-column
/// cache, or walked / gathered over the label and KEPT for the next
/// statement: exactly the read `load_walk_budgeted` makes for a whole-label
/// filter, so a pipeline operator reading a bound var's properties (an ORDER
/// BY key over a hop's ends, a predicate the strict filter declined) pays
/// the label once instead of a record per distinct id per statement.
/// `None` = a decline (the columnar paths are off, a rel source).
pub(crate) fn label_value_columns(
    graph: &Graph,
    label: &str,
    props: &[String],
    params: &BTreeMap<String, Value>,
) -> Result<Option<IdColumns>, RunError> {
    if !graph.columnar_scans_enabled() || props.is_empty() {
        return Ok(None);
    }
    let source = Source::Nodes {
        labels: vec![label.to_string()],
        any_of: Vec::new(),
    };
    let reads = Reads {
        props: props.to_vec(),
        ..Default::default()
    };
    let Some(walk) = load_walk_over(graph, &source, &reads, None, params)? else {
        return Ok(None);
    };
    Ok(Some(walk.columns))
}

fn filter_ids_mode(
    graph: &Graph,
    labels: &[String],
    var: &str,
    pred: &Expr,
    params: &BTreeMap<String, Value>,
    over: Option<std::sync::Arc<Vec<u64>>>,
    strict: bool,
) -> Result<Option<std::sync::Arc<Vec<u64>>>, RunError> {
    if !graph.columnar_scans_enabled() {
        sometimes!("interp.columnar paths switched off", true);
        return Ok(None);
    }
    if !reads_only(pred, std::slice::from_ref(&var.to_string())) {
        return Ok(None);
    }
    let mut reads = Reads::default();
    let Some(rw) = rewrite(pred, var, Kind::Node, &mut reads) else {
        return Ok(None);
    };
    // A surviving graph-dependent subquery has no hooks here: `rewrite`
    // passes an EXISTS/COUNT whose pattern STARTS from another variable
    // through untouched ("left for that variable's pass"), and on a
    // single-variable seed filter there is no other pass — `WHERE w.id IN
    // $ids AND EXISTS { MATCH (parent:L)-[:R]->(w) WHERE … }` reached the
    // hook-less evaluator below and errored "EXISTS {} requires a graph
    // context". Decline to the interp path, as every other stage does.
    if contains_opaque(&rw) {
        return Ok(None);
    }
    let source = Source::Nodes {
        labels: labels.to_vec(),
        any_of: Vec::new(),
    };
    let empty_vars = VarMap::new();
    let mut scope = Scope::over(params, &empty_vars, graph.wall_ms(), graph.zone_provider());
    // No population supplied: SEEK one from the predicate's own equalities
    // and prefixes on declared keys (`columnar_seek_ids`, the one rule),
    // and walk over the sought ids instead of the whole label. The general
    // path's column-filtered seed reached here for every label scan whose
    // WHERE the seed sites could not seek — the per-id `PropEq` seed
    // declines a key wider than its cap and has no prefix at all — so the
    // multi-clause `MATCH (g:GeopoliticalEvent) WHERE g.eventId STARTS
    // WITH 'edgar-8k-' AND … datetime(g.startAt) >= datetime($since) MATCH
    // …` evaluated three conjuncts (two datetime parses) over all 44k
    // events per statement: 32 ms against Neo4j's 10.9. The walk re-checks
    // the WHOLE predicate per sought id, exactly as it did per member.
    let over = match over {
        Some(o) => Some(o),
        None => {
            let seeks = crate::interp::prop_eq_candidates(Some(pred), var);
            let prefixes = crate::interp::prop_prefix_candidates(Some(pred), var);
            let ranges = crate::interp::prop_range_candidates(Some(pred), var);
            // The predicate IS its one sought conjunct: every sought id
            // satisfies it by the index's contract — the contract the general
            // path's own seed relies on — so there is no column to read. The
            // walk re-checked `t.userId = $u` over the 416 ids the `userId`
            // index had just answered, one record read per id per statement
            // on the paged mirror (a population walk is never kept).
            let sole_conjunct = crate::interp::conjunct_count(pred) == 1
                && seeks.len() + prefixes.len() + ranges.len() == 1;
            match columnar_seek_ids(graph, labels, &seeks, &prefixes, &ranges, SeekUse::Walk, &scope)? {
                Some(ids) => {
                    let members = graph.members_all(labels).map_err(RunError::Graph)?;
                    counted!("interp.seed column filter walked over a seek");
                    let kept: Vec<u64> = ids
                        .into_iter()
                        .filter(|id| graph.members_contains(&members, *id))
                        .collect();
                    if sole_conjunct {
                        counted!("interp.seed column filter answered by its seek alone");
                        counted!("interp.seeds filtered by columns");
                        return Ok(Some(std::sync::Arc::new(kept)));
                    }
                    Some(std::sync::Arc::new(kept))
                }
                None => None,
            }
        }
    };
    // Fix 67: a WHOLE-LABEL filter whose columns are cached is evaluated
    // column-at-a-time (`survivors_over_cached_columns`, the projection
    // path's evaluator since fix 40) instead of a scope bind and an
    // expression walk per member: the AcceptanceCriterion listing — no
    // index on `proposalId` on either engine — evaluated 4,235 expressions
    // over its 2k cached members per statement, 3.0 ms on the mirror
    // against Neo4j's 0.7 label scan. A column not yet cached, a predicate
    // the vectoriser declines, or a row answering a non-boolean keeps the
    // per-member walk below, which assembles and keeps the columns for the
    // next statement and raises (or, as a prefilter, keeps) that row. A
    // supplied population stays on the walk: the cache's aligned columns
    // are aligned to the label's whole membership only.
    if over.is_none()
        && labels.len() == 1
        && reads.labels.is_empty()
        && reads.probes.is_empty()
        && reads.degrees.is_empty()
        && !reads.type_read
    {
        let members = graph.members_all(labels).map_err(RunError::Graph)?.to_arc_vec();
        if let Some(hits) =
            survivors_over_cached_columns(graph, &labels[0], &rw, &reads, &members, None, &scope)
        {
            counted!("interp.seed column filter evaluated column-at-a-time");
            // Every value column came from the cache (the aligned read
            // declines otherwise): the same reads the walk would have
            // reported, so a trace still shows the columns served.
            for _ in &reads.props {
                counted!("interp.columnar column read served from the property-column cache");
            }
            counted!("interp.seeds filtered by columns");
            sometimes!("interp.seed filtered by columns", true);
            let keep: Vec<u64> = hits.into_iter().map(|i| members[i]).collect();
            return Ok(Some(std::sync::Arc::new(keep)));
        }
    }
    let Some(mut walk) = load_walk_over(graph, &source, &reads, over, params)? else {
        return Ok(None);
    };
    let mut keep = Vec::new();
    for mi in 0..walk.members.len() {
        let id = walk.members[mi];
        scope.locals.clear();
        walk.bind(graph, &reads, &mut scope, mi, id)?;
        let v = eval_with(&rw, &scope, None).map_err(RunError::Eval)?;
        match v.truth() {
            Some(Truth::True) => keep.push(id),
            // A prefilter keeps a non-boolean row for the full WHERE to
            // raise on; a verdict declines instead.
            None if !strict => keep.push(id),
            None => return Ok(None),
            Some(_) => {}
        }
    }
    counted!("interp.seeds filtered by columns");
    sometimes!("interp.seed filtered by columns", true);
    Ok(Some(std::sync::Arc::new(keep)))
}

/// One intermediate, non-breaking WITH of a columnar stage: its items
/// rewritten over the scanned variable and the aliases before it, plus
/// its WHERE.
struct ChainWith {
    items: Vec<Expr>,
    columns: Vec<String>,
    where_: Option<Expr>,
}

/// One step of the chain: a WITH, or an UNWIND — a per-row list product
/// (`UNWIND [{self: br.countryA, peer: br.countryB}, {…}] AS pair`), each
/// element bound as the alias before the rest of the chain runs.
enum ChainStep {
    With(ChainWith),
    Unwind { list: Expr, alias: String },
}

/// How the breaker consumes the chain's rows: a plain projection (ordered,
/// paged, post-WHERE), or an aggregating one through the shared fold
/// (`WITH iso AS iso3, sum(CASE …) AS targeted, … ORDER BY … LIMIT 250`).
enum Breaker {
    Project {
        items: Vec<Expr>,
        order: Vec<OrderItem>,
        /// When every ORDER BY item names a projected column, the columns
        /// are the keys — no key vector per row.
        by_column: Option<Vec<(usize, bool)>>,
    },
    Fold {
        items: Vec<Item>,
        order: Vec<(usize, bool)>,
    },
}

/// The stage head as a column walk: `MATCH (n…) [WHERE p] WITH <exprs over
/// n> AS … [WITH …] <breaker>`, where the breaker is a non-aggregating
/// WITH or RETURN with ORDER BY / SKIP / LIMIT over the aliases. The
/// degree histogram — `MATCH (n) WITH n, count{(n)--()} AS d WITH d ORDER
/// BY d …` — built a node and a row per member (1.79M of them, 10 s on
/// the production port) to carry one integer the adjacency table had.
struct StagePlan {
    source: Source,
    pred: Option<Expr>,
    reads: Reads,
    chain: Vec<ChainStep>,
    /// The breaker's columns, and how it consumes the rows.
    columns: Vec<String>,
    breaker: Breaker,
    skip: Option<Expr>,
    limit: Option<Expr>,
    /// The breaker's post-WHERE (a WITH's), over its own aliases.
    post_where: Option<Expr>,
    /// Fix 57: the breaker items that are the scanned node ITSELF (`WITH n
    /// …`). Each is a `Null` placeholder in `items`; the member id rides as
    /// a trailing column through the ordering and paging, and the survivors
    /// alone are materialised into these slots — the projection
    /// recogniser's `ProjItemPlan::Bare`, brought to the stage.
    bare: Vec<usize>,
}

fn recognise_stage(
    prefix: &[Clause],
    breaker: &Clause,
    rest_after: &[Clause],
) -> Option<StagePlan> {
    let [m @ Clause::Match { .. }, withs @ ..] = prefix else {
        return None;
    };
    let (var, kind, source, full_where) = recognise_source(m)?;
    // See `recognise_projection`: an identity equality stays the general
    // path's one-get seek (`MATCH (n) WHERE id(n) = $id …` would otherwise
    // walk every node of the store).
    if crate::interp::id_seek_expr(full_where.as_ref(), &var).is_some() {
        return None;
    }
    let mut reads = Reads::default();
    let pred = match &full_where {
        None => None,
        Some(w) => Some(rewrite(w, &var, kind, &mut reads)?),
    };
    // A surviving graph-dependent subquery has no hooks in this stage:
    // `rewrite` passes an EXISTS/COUNT whose pattern STARTS from another
    // variable through untouched, and the chain evaluated it hook-less —
    // every spelling of `MATCH (w:K) WHERE EXISTS { MATCH (parent:K)-[:HAS]
    // ->(w) … } RETURN w.id` errored "EXISTS {} requires a graph context"
    // with the columnar paths on. Decline to the interp path, as the
    // projection and aggregate recognisers do.
    if pred.as_ref().is_some_and(contains_opaque) {
        return None;
    }
    // Each intermediate WITH: rewritable items over the variable and the
    // aliases so far. A bare `WITH n` alone is the general path's (every
    // differential test forces it that way); a bare `n` beside other items
    // is carried in name only — nothing after the chain may read it.
    // The scanned variable is in scope until a WITH drops it: a WITH
    // rebinds the scope to its items, an UNWIND adds to it. Reading it
    // after a WITH that dropped it is what the general path refuses.
    let mut aliases: Vec<String> = Vec::new();
    let mut chain = Vec::with_capacity(withs.len());
    let mut var_in_scope = true;
    for c in withs {
        if let Clause::Unwind { expr, alias } = c {
            let mut allowed: Vec<String> = aliases.clone();
            if var_in_scope {
                allowed.push(var.clone());
            }
            if !reads_only(expr, &allowed) || *alias == var {
                return None;
            }
            let list = rewrite(expr, &var, kind, &mut reads)?;
            if contains_opaque(&list) {
                return None;
            }
            chain.push(ChainStep::Unwind {
                list,
                alias: alias.clone(),
            });
            aliases.push(alias.clone());
            continue;
        }
        let Clause::With { proj, where_ } = c else {
            return None;
        };
        if proj.star
            || proj.distinct
            || !proj.order.is_empty()
            || proj.skip.is_some()
            || proj.limit.is_some()
        {
            return None;
        }
        let pure_carry =
            proj.items.len() == 1 && matches!(&proj.items[0].expr, Expr::Var(v) if *v == var);
        if pure_carry {
            return None;
        }
        let mut allowed: Vec<String> = aliases.clone();
        if var_in_scope {
            allowed.push(var.clone());
        }
        let mut items = Vec::with_capacity(proj.items.len());
        let mut columns = Vec::with_capacity(proj.items.len());
        let mut next_aliases: Vec<String> = Vec::new();
        let mut carries_var = false;
        for (i, it) in proj.items.iter().enumerate() {
            let name = it
                .alias
                .clone()
                .or_else(|| it.text.clone())
                .unwrap_or_else(|| column_name(&it.expr, i));
            if name == var {
                if !matches!(&it.expr, Expr::Var(v) if *v == var) || !var_in_scope {
                    return None; // shadowing, or carrying what is not in scope
                }
                carries_var = true;
                continue; // carried in name only
            }
            if !reads_only(&it.expr, &allowed) {
                return None;
            }
            let ri = rewrite(&it.expr, &var, kind, &mut reads)?;
            // A surviving graph-dependent subquery has no hooks in this columnar
            // stage — decline to the interp path (see `recognise`).
            if contains_opaque(&ri) {
                return None;
            }
            items.push(ri);
            columns.push(name.clone());
            next_aliases.push(name);
        }
        let where_ = match where_ {
            None => None,
            Some(w) => {
                let mut allowed2 = next_aliases.clone();
                if carries_var {
                    allowed2.push(var.clone());
                }
                if !reads_only(w, &allowed2) {
                    return None;
                }
                let rw = rewrite(w, &var, kind, &mut reads)?;
                if contains_opaque(&rw) {
                    return None;
                }
                Some(rw)
            }
        };
        chain.push(ChainStep::With(ChainWith {
            items,
            columns,
            where_,
        }));
        aliases = next_aliases;
        var_in_scope = carries_var;
    }
    // The breaker.
    let (proj, post_where) = match breaker {
        Clause::With { proj, where_ } => (proj, where_.as_ref()),
        Clause::Return { proj } => (proj, None),
        _ => return None,
    };
    if proj.star || proj.distinct || proj.items.is_empty() {
        return None;
    }
    let mut allowed: Vec<String> = aliases.clone();
    if var_in_scope {
        allowed.push(var.clone());
    }
    // Fix 57: the scanned node itself may LEAVE the stage — `WITH n ORDER
    // BY n.createdAt DESC SKIP … LIMIT …` — as a placeholder column that is
    // hydrated for the survivors alone. Until this held, a bare carry sent
    // the whole stage to the general path, which built a row per member:
    // the inbox listing paged 1,000 of ~38k emails through 125k expression
    // evaluations and an 11k-deep top-k, 294 ms against Neo4j's 113.
    let bare: Vec<usize> = proj
        .items
        .iter()
        .enumerate()
        .filter(|(_, it)| matches!(&it.expr, Expr::Var(v) if *v == var))
        .map(|(i, _)| i)
        .collect();
    if !bare.is_empty() && !var_in_scope {
        return None; // carrying what an earlier WITH dropped
    }
    // A bare carry is a TOP-K's: a plain limit over the carry is the seed's
    // to cut (fix 52 reads only its first members), and this stage would
    // walk the whole label's columns to page it.
    if !bare.is_empty() && proj.order.is_empty() {
        return None;
    }
    if proj.items.iter().any(|it| !reads_only(&it.expr, &allowed)) {
        return None;
    }
    // Nothing after the breaker may read the variable unless it leaves in
    // the rows (a bare carry): otherwise it is not there to read.
    if bare.is_empty() {
        for c in rest_after {
            match crate::interp::clause_mentions_pub(c) {
                Some(names) if !names.contains(&var) => {}
                _ => return None,
            }
        }
    }
    let aggregating = proj
        .items
        .iter()
        .any(|it| contains_aggregate_call(&it.expr));
    if aggregating && !bare.is_empty() {
        return None; // a node as an aggregate's group key: the general path's
    }
    let (columns, breaker) = if aggregating {
        // `count(v)` over the variable is `count(*)`; over an alias it is
        // a real count of non-null values (aggregating_items keeps it).
        let (items, columns) = aggregating_items(proj, &var, kind, &mut reads)?;
        let order = order_over(proj, &columns)?;
        (columns, Breaker::Fold { items, order })
    } else {
        let mut items = Vec::with_capacity(proj.items.len());
        let mut columns = Vec::with_capacity(proj.items.len());
        for (i, it) in proj.items.iter().enumerate() {
            columns.push(
                it.alias
                    .clone()
                    .or_else(|| it.text.clone())
                    .unwrap_or_else(|| column_name(&it.expr, i)),
            );
            items.push(if bare.contains(&i) {
                Expr::Null // the placeholder the survivors' hydration fills
            } else {
                rewrite(&it.expr, &var, kind, &mut reads)?
            });
        }
        let mut order_allowed: Vec<String> = allowed.clone();
        order_allowed.extend(columns.iter().cloned());
        let mut order = Vec::with_capacity(proj.order.len());
        for o in &proj.order {
            if !reads_only(&o.expr, &order_allowed) {
                return None;
            }
            order.push(OrderItem {
                expr: rewrite(&o.expr, &var, kind, &mut reads)?,
                desc: o.desc,
            });
        }
        let by_column: Option<Vec<(usize, bool)>> = proj
            .order
            .iter()
            .map(|o| {
                let ix = match &o.expr {
                    Expr::Var(v) => columns.iter().position(|c| c == v),
                    e => proj.items.iter().position(|it| it.expr == *e),
                }?;
                Some((ix, o.desc))
            })
            .collect();
        (
            columns,
            Breaker::Project {
                items,
                order,
                by_column,
            },
        )
    };
    for e in [&proj.skip, &proj.limit].into_iter().flatten() {
        if !reads_only(e, &[]) {
            return None;
        }
    }
    let post_where = match post_where {
        None => None,
        Some(w) => {
            // A post-WHERE over a bare carry would read the node through a
            // column the rewrite has already turned into walk locals.
            if !bare.is_empty() || !reads_only(w, &columns) {
                return None;
            }
            Some(rewrite(w, &var, kind, &mut reads)?)
        }
    };
    // The breaker's items, ORDER BY keys and post-WHERE are evaluated
    // hook-less too: a `COUNT { MATCH (parent:K)-[:HAS]->(w) WHERE … }`
    // item (an EXISTS/COUNT whose pattern starts from another variable
    // survives `rewrite` untouched) errored "COUNT {} requires a graph
    // context". One check over the finished plan, as the projection and
    // aggregate recognisers apply per item.
    let breaker_opaque = match &breaker {
        Breaker::Project { items, order, .. } => {
            items.iter().any(contains_opaque) || order.iter().any(|o| contains_opaque(&o.expr))
        }
        Breaker::Fold { items, .. } => items.iter().any(|it| match it {
            Item::Key(e) => contains_opaque(e),
            Item::Agg(_, arg) => arg.as_ref().is_some_and(contains_opaque),
        }),
    };
    if breaker_opaque || post_where.as_ref().is_some_and(contains_opaque) {
        return None;
    }
    Some(StagePlan {
        source,
        pred,
        reads,
        chain,
        columns,
        breaker,
        skip: proj.skip.clone(),
        limit: proj.limit.clone(),
        post_where,
        bare,
    })
}

/// Whether `e` contains an aggregate call anywhere.
fn contains_aggregate_call(e: &Expr) -> bool {
    let mut found = false;
    walk_expr(e, &mut |x| {
        if let Expr::Call { name, .. } = x {
            if is_aggregate_fn(name) {
                found = true;
            }
        }
    });
    found
}

/// Visit every sub-expression (pre-order), shallowly over the variants the
/// rewrite knows; anything else is a leaf.
fn walk_expr(e: &Expr, f: &mut dyn FnMut(&Expr)) {
    f(e);
    match e {
        Expr::Prop(b, _) | Expr::Not(b) | Expr::Neg(b) => walk_expr(b, f),
        Expr::And(a, b)
        | Expr::Or(a, b)
        | Expr::Xor(a, b)
        | Expr::Bin(_, a, b)
        | Expr::In(a, b)
        | Expr::Index(a, b) => {
            walk_expr(a, f);
            walk_expr(b, f);
        }
        Expr::IsNull { of, .. } => walk_expr(of, f),
        Expr::List(items) => items.iter().for_each(|x| walk_expr(x, f)),
        Expr::Call { args, .. } => args.iter().for_each(|x| walk_expr(x, f)),
        Expr::Case {
            subject,
            arms,
            otherwise,
        } => {
            if let Some(s) = subject {
                walk_expr(s, f);
            }
            for (w, t) in arms {
                walk_expr(w, f);
                walk_expr(t, f);
            }
            if let Some(o) = otherwise {
                walk_expr(o, f);
            }
        }
        _ => {}
    }
}

/// Walk the chain from the current scope: each WITH binds its aliases
/// and filters; each UNWIND multiplies — `null` and `[]` yield nothing,
/// and a non-list value is refused exactly as the general path refuses
/// it. The sink sees every row that reaches the breaker.
fn walk_chain(
    steps: &[ChainStep],
    scope: &mut Scope<'_>,
    sink: &mut dyn FnMut(&mut Scope<'_>) -> Result<(), RunError>,
) -> Result<(), RunError> {
    let Some((step, rest)) = steps.split_first() else {
        return sink(scope);
    };
    match step {
        ChainStep::With(w) => {
            let mut vals = Vec::with_capacity(w.items.len());
            for e in &w.items {
                vals.push(eval_with(e, scope, None).map_err(RunError::Eval)?);
            }
            for (c, v) in w.columns.iter().zip(vals) {
                scope.bind(c, v);
            }
            if let Some(p) = &w.where_ {
                let v = eval_with(p, scope, None).map_err(RunError::Eval)?;
                match v.truth() {
                    Some(Truth::True) => {}
                    Some(_) => return Ok(()),
                    None => {
                        return Err(RunError::Semantic(format!(
                            "WHERE takes a boolean, got {}",
                            v.type_name()
                        )));
                    }
                }
            }
            walk_chain(rest, scope, sink)
        }
        ChainStep::Unwind { list, alias } => {
            let v = eval_with(list, scope, None).map_err(RunError::Eval)?;
            sometimes!("interp.columnar stage unwound a list", true);
            match v {
                Value::Null => Ok(()),
                Value::List(items) => {
                    for it in items {
                        scope.bind(alias, it);
                        walk_chain(rest, scope, sink)?;
                    }
                    Ok(())
                }
                other => Err(RunError::Semantic(format!(
                    "UNWIND takes a list, got {}",
                    other.type_name()
                ))),
            }
        }
    }
}

/// Run the stage head as a column walk — `None` declines to the general
/// path. Returns the breaker's rows (ordered and paged, post-WHERE
/// applied) with its aliases as keys.
pub(crate) fn try_columnar_stage(
    graph: &Graph,
    prefix: &[Clause],
    breaker: &Clause,
    rest_after: &[Clause],
    input: &[Row],
    params: &BTreeMap<String, Value>,
) -> Result<Option<(Vec<Row>, usize)>, RunError> {
    if !graph.columnar_scans_enabled() {
        sometimes!("interp.columnar paths switched off", true);
        return Ok(None);
    }
    if input.len() != 1 || !input[0].is_empty() {
        return Ok(None); // a stage head only: nothing carried in
    }
    let Some(plan) = recognise_stage(prefix, breaker, rest_after) else {
        return Ok(None);
    };
    // Fix 57's graph-dependent half: a bare carry rides the stage only
    // where its start is NOT selectively seekable. The general path's seed
    // — the same candidates, the same probe — reads a sought minority alone
    // and binds it lean from the columns, while this stage would walk the
    // whole label's columns to page it; so a seek that answers less than
    // half the label (within the seek's cap) keeps the carry there. The
    // inbox page of a user who owns 38k of the 38.6k emails is the stage's;
    // a 500-email user's is the seek's.
    if !plan.bare.is_empty() {
        if let [Clause::Match { pattern, where_, .. }, ..] = prefix {
            if let (Source::Nodes { labels, .. }, [path]) = (&plan.source, pattern.paths.as_slice()) {
                let label = labels.first().map(String::as_str);
                if graph.property_seek_worth_probing(label) {
                    let seed = Row::new();
                    let cands = seek_candidates(graph, path, where_.as_ref(), &seed, params)?;
                    if let Some((_, ids)) =
                        best_declared_seek(graph, labels, &cands, crate::PROPERTY_SEEK_MAX_PROBE)?
                    {
                        if graph.property_seek_wins_under(
                            label,
                            ids.len(),
                            crate::PROPERTY_SEEK_MAX_PROBE,
                            2,
                        ) {
                            sometimes!(
                                "interp.columnar stage left a bare carry to its selective seek",
                                true
                            );
                            return Ok(None);
                        }
                    }
                }
            }
        }
    }
    let Some(mut walk) = load_walk(graph, &plan.source, &plan.reads, params)? else {
        return Ok(None);
    };
    counted!("interp.statements run");
    counted!("interp.columnar stages");
    sometimes!("interp.columnar stage produced a WITH chain", true);
    note_walk_events(&plan.source, &plan.reads, &walk);
    let empty_vars = VarMap::new();
    let mut scope = Scope::over(params, &empty_vars, graph.wall_ms(), graph.zone_provider());
    let truth_of = |v: Value| -> Result<bool, RunError> {
        match v.truth() {
            Some(Truth::True) => Ok(true),
            Some(_) => Ok(false),
            None => Err(RunError::Semantic(format!(
                "WHERE takes a boolean, got {}",
                v.type_name()
            ))),
        }
    };
    // The breaker's rows, before the post-WHERE.
    let rows: Vec<Vec<Value>> = match &plan.breaker {
        Breaker::Project {
            items,
            order,
            by_column,
        } => {
            let by_column = by_column.as_ref().filter(|b| !b.is_empty());
            let mut rows: Vec<Vec<Value>> = Vec::new();
            let mut keys: Vec<Vec<Value>> = Vec::new();
            for mi in 0..walk.members.len() {
                let id = walk.members[mi];
                scope.locals.clear();
                walk.bind(graph, &plan.reads, &mut scope, mi, id)?;
                if let Some(pred) = &plan.pred {
                    let v = eval_with(pred, &scope, None).map_err(RunError::Eval)?;
                    if !truth_of(v)? {
                        continue;
                    }
                }
                walk_chain(&plan.chain, &mut scope, &mut |sc| {
                    let mut out = Vec::with_capacity(items.len());
                    for e in items {
                        out.push(eval_with(e, sc, None).map_err(RunError::Eval)?);
                    }
                    let mut k = Vec::new();
                    if by_column.is_none() && !order.is_empty() {
                        for (c, v) in plan.columns.iter().zip(&out) {
                            sc.bind(c, v.clone());
                        }
                        k.reserve(order.len());
                        for o in order {
                            k.push(eval_with(&o.expr, sc, None).map_err(RunError::Eval)?);
                        }
                    }
                    // Fix 57: the member id rides as a trailing column, past
                    // every real column, for the survivors' hydration.
                    if !plan.bare.is_empty() {
                        out.push(Value::Int(id as i64));
                    }
                    rows.push(out);
                    keys.push(k);
                    budget_check(graph, rows.len())
                })?;
            }
            match by_column {
                Some(by_col) => order_and_page_by_column(
                    graph,
                    params,
                    rows,
                    by_col,
                    plan.skip.as_ref(),
                    plan.limit.as_ref(),
                )?,
                None => order_and_page(
                    graph,
                    params,
                    rows,
                    order,
                    keys,
                    plan.skip.as_ref(),
                    plan.limit.as_ref(),
                )?,
            }
        }
        Breaker::Fold { items, order } => {
            sometimes!("interp.columnar stage folded an aggregating breaker", true);
            let mut fold = Fold::new(items);
            for mi in 0..walk.members.len() {
                let id = walk.members[mi];
                scope.locals.clear();
                walk.bind(graph, &plan.reads, &mut scope, mi, id)?;
                if let Some(pred) = &plan.pred {
                    let v = eval_with(pred, &scope, None).map_err(RunError::Eval)?;
                    if !truth_of(v)? {
                        continue;
                    }
                }
                walk_chain(&plan.chain, &mut scope, &mut |sc| fold.push(graph, sc))?;
            }
            let spec = FoldSpec {
                items,
                columns: &plan.columns,
                order,
                skip: plan.skip.as_ref(),
                limit: plan.limit.as_ref(),
                final_: None,
            };
            fold.finish(graph, params, &spec, &mut scope)?.rows
        }
    };
    // Fix 57: the survivors alone are materialised into the bare slots —
    // the trailing id column comes off first.
    let rows = if plan.bare.is_empty() {
        rows
    } else {
        // Fix 62: a survivor is hydrated to what the CONTINUATION reads of
        // it, not the whole record. The inbox page's continuation reads a
        // dozen of the email's properties and its HAS_ASK adjacency, never
        // the body, yet every one of its 1,000 survivors was decoded in
        // full (fat records on the paged mirror: the orig page stayed
        // 296–357 ms against Neo4j's 107–115 on v121 with the stage
        // running). `carry_demand` follows the carry through the later
        // WITHs under its aliases; a bare use it cannot see through — a
        // whole-node read, a `labels()` call, a star projection, a writing
        // clause — keeps the full record.
        let mut projected: Option<std::collections::BTreeSet<String>> =
            Some(Default::default());
        for &bi in &plan.bare {
            match carry_demand(rest_after, &plan.columns[bi]) {
                Some(set) => {
                    if let Some(u) = projected.as_mut() {
                        u.extend(set);
                    }
                }
                None => projected = None,
            }
        }
        let mut hydrated = Vec::with_capacity(rows.len());
        for mut r in rows {
            let Some(Value::Int(id)) = r.pop() else {
                return Err(RunError::Semantic("stage row lost its id".into()));
            };
            let node = match &projected {
                Some(set) => {
                    counted!("interp.columnar stage hydrated a survivor projected to its continuation");
                    graph.node_projected(id as u64, set).map_err(RunError::Graph)?
                }
                None => graph.node(id as u64).map_err(RunError::Graph)?,
            };
            for &bi in &plan.bare {
                r[bi] = node.clone().unwrap_or(Value::Null);
            }
            counted!("interp.columnar stage hydrated a bare node for a survivor");
            hydrated.push(r);
        }
        hydrated
    };
    let mut kept: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
    for r in rows {
        if let Some(p) = &plan.post_where {
            scope.locals.clear();
            for (c, v) in plan.columns.iter().zip(&r) {
                scope.bind(c, v.clone());
            }
            let v = eval_with(p, &scope, None).map_err(RunError::Eval)?;
            if !truth_of(v)? {
                continue;
            }
        }
        kept.push(r);
    }
    // The CONTINUATION: an aggregating WITH right after the breaker that
    // reads only the breaker's aliases folds over these rows as they are —
    // no `Row` per value, no general projector. The degree histogram's
    // `WITH d ORDER BY d WITH collect(d) AS ds` built 1.79M one-entry rows
    // to collect one integer each.
    if let Some(Clause::With {
        proj: next,
        where_: next_where,
    }) = rest_after.first()
    {
        if let Some((items, columns, order)) = continuation_plan(next, &plan.columns) {
            sometimes!(
                "interp.columnar stage fused the next aggregating WITH",
                true
            );
            let mut fold = Fold::new(&items);
            for r in &kept {
                scope.locals.clear();
                for (c, v) in plan.columns.iter().zip(r) {
                    scope.bind(c, v.clone());
                }
                fold.push(graph, &scope)?;
            }
            let spec = FoldSpec {
                items: &items,
                columns: &columns,
                order: &order,
                skip: next.skip.as_ref(),
                limit: next.limit.as_ref(),
                final_: None,
            };
            let folded = fold.finish(graph, params, &spec, &mut scope)?.rows;
            let mut out_rows = Vec::with_capacity(folded.len());
            for r in folded {
                if let Some(w) = next_where {
                    scope.locals.clear();
                    for (c, v) in columns.iter().zip(&r) {
                        scope.bind(c, v.clone());
                    }
                    let v = eval_with(w, &scope, None).map_err(RunError::Eval)?;
                    if !truth_of(v)? {
                        continue;
                    }
                }
                let mut row = Row::new();
                for (c, v) in columns.iter().zip(r) {
                    row.insert(c.clone(), v);
                }
                out_rows.push(row);
            }
            return Ok(Some((out_rows, 1)));
        }
    }
    let mut out_rows = Vec::with_capacity(kept.len());
    for r in kept {
        let mut row = Row::new();
        for (c, v) in plan.columns.iter().zip(r) {
            row.insert(c.clone(), v);
        }
        out_rows.push(row);
    }
    Ok(Some((out_rows, 0)))
}

/// A fused aggregating WITH: the fold's items and columns, and its ORDER
/// BY over its own columns.
type ContinuationPlan = (Vec<Item>, Vec<String>, Vec<(usize, bool)>);

/// An aggregating WITH that reads only `aliases` (its WHERE too): the
/// fold's items and columns, and its ORDER BY over its own columns.
fn continuation_plan(next: &Projection, aliases: &[String]) -> Option<ContinuationPlan> {
    if next.star || next.distinct || next.items.is_empty() {
        return None;
    }
    if !next
        .items
        .iter()
        .any(|it| contains_aggregate_call(&it.expr))
    {
        return None;
    }
    if next.items.iter().any(|it| !reads_only(&it.expr, aliases)) {
        return None;
    }
    // The fused fold evaluates hook-less: a graph-dependent subquery in an
    // item (fix 72's `sum(COUNT { (b)<-[:R]-(a:A) })` over a carried alias)
    // errored "COUNT {} requires a graph context" here. Decline to the
    // streaming aggregate, which has hooks — as `recognise_stage` does for
    // the breaker's own items.
    if next
        .items
        .iter()
        .any(|it| contains_opaque(&it.expr))
        || next.order.iter().any(|o| contains_opaque(&o.expr))
    {
        return None;
    }
    // No scanned variable here: a name nothing binds, so every alias is a
    // plain local and `count(alias)` stays a real count.
    let mut reads = Reads::default();
    let (items, columns) = aggregating_items(next, "\u{0}none", Kind::Node, &mut reads)?;
    let order = order_over(next, &columns)?;
    Some((items, columns, order))
}

/// One end of a hop: its variable (if named), labels, and what the
/// statement reads of it.
struct HopEnd {
    labels: Vec<String>,
    reads: Reads,
}

/// `MATCH (a[:A…] {…})-[r:T…]->(b[:B…] {…}) [WHERE p(a, r, b)] RETURN
/// <aggregates over a.x, r.y, b.z>[, keys]` (and `<-`), recognised.
struct HopPlan {
    types: Vec<String>,
    /// The storage direction: `->` binds `a` to the source, `<-` to the
    /// destination.
    out: bool,
    a: HopEnd,
    b: HopEnd,
    /// Fix 58: the start's property equalities (its inline map and the
    /// WHERE's `a.k = <const|param>` / `IN [...]` conjuncts, before the
    /// rewrite) — the seek candidates a sought start drives the walk from.
    a_seeks: Vec<(String, Vec<Expr>)>,
    r_reads: Reads,
    pred: Option<Expr>,
    items: Vec<Item>,
    columns: Vec<String>,
    order: Vec<(usize, bool)>,
    skip: Option<Expr>,
    limit: Option<Expr>,
    final_: Option<Final>,
}

/// `count(v)` for any variable the hop binds is `count(*)`: none is ever
/// null in a match.
fn star_counts(proj: &Projection, vars: &[String]) -> Projection {
    let mut p = proj.clone();
    for it in &mut p.items {
        if let Expr::Call {
            name,
            distinct: false,
            args,
            star,
        } = &mut it.expr
        {
            if name == "count"
                && !*star
                && matches!(args.as_slice(), [Expr::Var(v)] if vars.contains(v))
            {
                args.clear();
                *star = true;
            }
        }
    }
    p
}

/// Rewrite an item's expressions over one hop variable.
fn rewrite_item(it: &Item, var: &str, kind: Kind, reads: &mut Reads) -> Option<Item> {
    Some(match it {
        Item::Key(e) => Item::Key(rewrite(e, var, kind, reads)?),
        Item::Agg(site, arg) => Item::Agg(
            site.clone(),
            match arg {
                Some(e) => Some(rewrite(e, var, kind, reads)?),
                None => None,
            },
        ),
    })
}

fn recognise_hop(q: &SingleQuery) -> Option<HopPlan> {
    let (match_clause, agg_proj, final_proj) = match q.clauses.as_slice() {
        [m @ Clause::Match { .. }, Clause::Return { proj }] => (m, proj, None),
        [
            m @ Clause::Match { .. },
            Clause::With {
                proj: wp,
                where_: None,
            },
            Clause::Return { proj: rp },
        ] => (m, wp, Some(rp)),
        _ => return None,
    };
    let Clause::Match {
        optional: false,
        pattern,
        where_,
    } = match_clause
    else {
        return None;
    };
    if pattern.paths.len() != 1 {
        return None;
    }
    let path = &pattern.paths[0];
    if path.var.is_some() || path.shortest {
        return None;
    }
    let [(rel, end)] = path.hops.as_slice() else {
        return None;
    };
    if rel.length.is_some() {
        return None;
    }
    let out = match rel.dir {
        RelDir::Out => true,
        RelDir::In => false,
        RelDir::Undirected => return None,
    };
    // Distinct variable names for the three roles (or anonymous).
    let a_var = path.start.var.clone();
    let b_var = end.var.clone();
    let r_var = rel.var.clone();
    let mut names: Vec<&String> = [&a_var, &b_var, &r_var].into_iter().flatten().collect();
    let n = names.len();
    names.sort();
    names.dedup();
    if names.len() != n {
        return None; // a repeated variable is a self-join, not a scan
    }
    // Inline maps are equalities.
    let mut conjuncts: Vec<Expr> = Vec::new();
    for (var, props) in [
        (&a_var, &path.start.props),
        (&b_var, &end.props),
        (&r_var, &rel.props),
    ] {
        match props {
            None => {}
            Some(Expr::Map(pairs)) => {
                let Some(v) = var else {
                    return None; // an anonymous end with a map: no name to bind
                };
                for (k, val) in pairs {
                    conjuncts.push(Expr::Bin(
                        BinOp::Eq,
                        Box::new(Expr::Prop(Box::new(Expr::Var(v.clone())), k.clone())),
                        Box::new(val.clone()),
                    ));
                }
            }
            Some(_) => return None,
        }
    }
    let mut full_where: Option<Expr> = where_.clone();
    for c in conjuncts {
        full_where = Some(match full_where {
            None => c,
            Some(w) => Expr::And(Box::new(c), Box::new(w)),
        });
    }
    let all_vars: Vec<String> = [&a_var, &b_var, &r_var]
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    if let Some(w) = &full_where {
        if !reads_only(w, &all_vars) {
            return None;
        }
    }
    let a_seeks = a_var
        .as_deref()
        .map(|v| prop_eq_candidates(full_where.as_ref(), v))
        .unwrap_or_default();
    if agg_proj
        .items
        .iter()
        .any(|it| !reads_only(&it.expr, &all_vars))
    {
        return None;
    }
    let mut a = HopEnd {
        labels: path.start.labels.clone(),
        reads: Reads::tagged("a."),
    };
    let mut b = HopEnd {
        labels: end.labels.clone(),
        reads: Reads::tagged("b."),
    };
    let mut r_reads = Reads::tagged("r.");
    // The passes: the node ends first (labels and probes are node reads),
    // the relationship last.
    let mut pred = full_where;
    if let Some(v) = &a_var {
        pred = match pred {
            None => None,
            Some(w) => Some(rewrite(&w, v, Kind::Node, &mut a.reads)?),
        };
    }
    if let Some(v) = &b_var {
        pred = match pred {
            None => None,
            Some(w) => Some(rewrite(&w, v, Kind::Node, &mut b.reads)?),
        };
    }
    if let Some(v) = &r_var {
        pred = match pred {
            None => None,
            Some(w) => Some(rewrite(&w, v, Kind::Rel, &mut r_reads)?),
        };
    }
    // See `recognise`: any subquery none of the hop's vars could lift into a
    // probe has no hooks in this columnar scan — decline to the interp path.
    if pred.as_ref().is_some_and(contains_opaque) {
        return None;
    }
    let proj = star_counts(agg_proj, &all_vars);
    // Items: the first pass builds the sites (over `a`, or a dummy name
    // when `a` is anonymous — nothing then reads it), the others rewrite.
    let first = a_var.clone().unwrap_or_else(|| "\u{0}a".to_string());
    let (mut items, columns) = aggregating_items(&proj, &first, Kind::Node, &mut a.reads)?;
    if let Some(v) = &b_var {
        let mut out = Vec::with_capacity(items.len());
        for it in &items {
            out.push(rewrite_item(it, v, Kind::Node, &mut b.reads)?);
        }
        items = out;
    }
    if let Some(v) = &r_var {
        let mut out = Vec::with_capacity(items.len());
        for it in &items {
            out.push(rewrite_item(it, v, Kind::Rel, &mut r_reads)?);
        }
        items = out;
    }
    let order = order_over(&proj, &columns)?;
    let final_ = match final_proj {
        None => None,
        Some(rp) => {
            if !proj.order.is_empty() || proj.skip.is_some() || proj.limit.is_some() {
                return None;
            }
            Some(final_over(rp, &columns)?)
        }
    };
    Some(HopPlan {
        types: rel.types.clone(),
        out,
        a,
        b,
        a_seeks,
        r_reads,
        pred,
        items,
        columns,
        order,
        skip: proj.skip.clone(),
        limit: proj.limit.clone(),
        final_,
    })
}

/// Fix 62: the properties the clauses after a breaker read of a carried
/// node, FOLLOWING the carry through later WITHs under its aliases —
/// `Some(props)` (possibly empty: an identity use alone), or `None` when
/// something reads it whole. The executor's `demands_after` (fix 51) is
/// the conservative rule for hop ends it binds per row: a bare projection
/// item is a whole-node use there, because the row keeps the value as it
/// stands. A stage survivor is different — it is hydrated ONCE for
/// everything after the breaker, so `WITH n, count(a) AS asks RETURN
/// n.nodeId, n.subject, asks` reads two properties of `n`, not the record.
/// A carry named twice in one WITH, a star projection, a WITH's WHERE or
/// ORDER BY reading the node whole, and any clause kind the walk cannot see
/// through (a write, a CALL, a FOREACH) keep the full record.
fn carry_demand(clauses: &[Clause], var: &str) -> Option<std::collections::BTreeSet<String>> {
    use crate::interp::{VarDemand, collect_demand};
    let mut name = var.to_string();
    let mut props: std::collections::BTreeSet<String> = Default::default();
    // The reads of `name` in one expression, merged; `false` = read whole.
    let merge = |e: &Expr, name: &str, props: &mut std::collections::BTreeSet<String>| -> bool {
        let mut d: BTreeMap<String, VarDemand> = BTreeMap::new();
        collect_demand(e, &mut Vec::new(), &mut d);
        match d.get(name) {
            Some(VarDemand::Full) => false,
            Some(VarDemand::Props(s)) => {
                props.extend(s.iter().cloned());
                true
            }
            None => true,
        }
    };
    for c in clauses {
        match c {
            Clause::Match {
                pattern, where_, ..
            } => {
                for path in &pattern.paths {
                    // The carry as a pattern endpoint is an identity use;
                    // the inline maps and the WHERE read.
                    if let Some(p) = &path.start.props {
                        if !merge(p, &name, &mut props) {
                            return None;
                        }
                    }
                    for (rel, node) in &path.hops {
                        if let Some(p) = &rel.props {
                            if !merge(p, &name, &mut props) {
                                return None;
                            }
                        }
                        if let Some(p) = &node.props {
                            if !merge(p, &name, &mut props) {
                                return None;
                            }
                        }
                    }
                }
                if let Some(w) = where_ {
                    if !merge(w, &name, &mut props) {
                        return None;
                    }
                }
            }
            Clause::Unwind { expr, .. } => {
                if !merge(expr, &name, &mut props) {
                    return None;
                }
            }
            Clause::With { proj, where_ } => {
                if proj.star {
                    return None;
                }
                let mut next: Option<String> = None;
                for it in &proj.items {
                    match &it.expr {
                        Expr::Var(v) if *v == name => {
                            if next.is_some() {
                                return None; // carried twice: keep the record
                            }
                            next = Some(it.alias.clone().unwrap_or_else(|| v.clone()));
                        }
                        e => {
                            if !merge(e, &name, &mut props) {
                                return None;
                            }
                        }
                    }
                }
                // ORDER BY and WHERE of a WITH name its OUTPUT columns.
                let Some(alias) = next else {
                    return Some(props); // dropped: nothing after reads it
                };
                name = alias;
                for o in &proj.order {
                    if !merge(&o.expr, &name, &mut props) {
                        return None;
                    }
                }
                if let Some(w) = where_ {
                    if !merge(w, &name, &mut props) {
                        return None;
                    }
                }
            }
            Clause::Return { proj, .. } => {
                if proj.star {
                    return None;
                }
                for it in &proj.items {
                    if !merge(&it.expr, &name, &mut props) {
                        return None;
                    }
                }
                for o in &proj.order {
                    if !merge(&o.expr, &name, &mut props) {
                        return None;
                    }
                }
                return Some(props);
            }
            _ => return None,
        }
    }
    Some(props)
}

/// Fix 58: the hop's population driven from a SOUGHT start — `(src, dst)`
/// pairs in storage order, read from the start's typed adjacency — or
/// `None` for the whole-type walk. The start drives the walk when its
/// declared-key equality (inline map or WHERE) selects fewer than half its
/// label's members within that bound (`best_declared_seek` under the
/// general path's own candidate rule: the first candidate may be unscoped,
/// every other must be declared). Only the `a` end is asked: a statement
/// whose FAR end carries the sought map is the pipeline aggregate's, which
/// seeds it through its anchored seed and never reaches this scan. The
/// relationship must be read nowhere — no `type(r)`, `id(r)`, property,
/// presence, probe or degree of it — since the adjacency carries no
/// relationship record and the seeded population has no relationship
/// order to bind columns in.
fn hop_seeded_ends(
    graph: &Graph,
    plan: &HopPlan,
    params: &BTreeMap<String, Value>,
) -> Result<Option<Vec<(u64, u64)>>, RunError> {
    let r = &plan.r_reads;
    if r.type_read
        || r.id_read
        || !r.props.is_empty()
        || !r.presence.is_empty()
        || !r.probes.is_empty()
        || !r.degrees.is_empty()
        || !r.labels.is_empty()
    {
        return Ok(None);
    }
    if graph.in_txn_with_writes() {
        return Ok(None);
    }
    let Some(tokens) = graph.type_tokens_peek(&plan.types) else {
        return Ok(None);
    };
    if tokens.is_empty() {
        return Ok(None);
    }
    let tokens = Some(tokens);
    let Some(sought) = hop_end_seek(graph, &plan.a.labels, &plan.a_seeks, params)? else {
        return Ok(None);
    };
    let dir = if plan.out { Dir::Out } else { Dir::In };
    let mut ends: Vec<(u64, u64)> = Vec::new();
    for &id in &sought {
        graph.adjacent_slim_for_each(id, dir, &tokens, |e| {
            ends.push(match dir {
                Dir::Out => (id, e.peer),
                _ => (e.peer, id),
            });
        });
    }
    counted!("interp.columnar hop scan seeded from a sought end");
    Ok(Some(ends))
}

/// The ids a hop end's declared-key equality selects, when they are fewer
/// than half the end's (smallest) label — else `None`. The candidates'
/// values are constants and parameters (an expression that reads a
/// variable, or fails, drops the candidate); the ids are kept to the end's
/// members, as an unscoped probe may carry the key under another label.
fn hop_end_seek(
    graph: &Graph,
    labels: &[String],
    seeks: &[(String, Vec<Expr>)],
    params: &BTreeMap<String, Value>,
) -> Result<Option<Vec<u64>>, RunError> {
    if labels.is_empty() || seeks.is_empty() {
        return Ok(None);
    }
    let Some(label) = labels.iter().min_by_key(|l| graph.count_label_nodes(l)) else {
        return Ok(None);
    };
    if !graph.property_seek_worth_probing(Some(label)) {
        return Ok(None);
    }
    let empty_vars = VarMap::new();
    let scope = Scope::over(params, &empty_vars, graph.wall_ms(), graph.zone_provider());
    let mut cands: Vec<(String, Vec<Value>)> = Vec::new();
    for (k, exprs) in seeks {
        let mut vs = Vec::with_capacity(exprs.len());
        for e in exprs {
            match eval_with(e, &scope, None) {
                Ok(v) if matches!(v, Value::Int(_) | Value::Float(_) | Value::Str(_)) => {
                    vs.push(v);
                }
                _ => {
                    vs.clear();
                    break;
                }
            }
        }
        if !vs.is_empty() {
            cands.push((k.clone(), vs));
        }
    }
    if cands.is_empty() {
        return Ok(None);
    }
    let cap = (graph.count_label_nodes(label) / 2) as usize;
    let Some((_, mut ids)) = best_declared_seek(graph, labels, &cands, cap)? else {
        return Ok(None);
    };
    if !graph.property_seek_wins_under(Some(label), ids.len(), cap, 2) {
        return Ok(None);
    }
    let members = graph.members_all(labels).map_err(RunError::Graph)?;
    ids.retain(|id| graph.members_contains(&members, *id));
    ids.sort_unstable();
    ids.dedup();
    Ok(Some(ids))
}

/// The distinct ids of one end of the population, sorted.
fn distinct_ends(ends: &[(u64, u64)], src: bool) -> std::sync::Arc<Vec<u64>> {
    let mut v: Vec<u64> = ends
        .iter()
        .map(|(s, d)| if src { *s } else { *d })
        .collect();
    v.sort_unstable();
    v.dedup();
    std::sync::Arc::new(v)
}

/// The hop-bearing aggregate scan. The population is the typed
/// relationship walk with its ends; each end's columns are loaded over
/// the span of its distinct ids and bound by binary search; an end's
/// labels filter by membership; the relationship's own columns walk in
/// relationship order; and the fold is the node scan's. `MATCH
/// (s:Company)-[r:SUPPLIES]->(cus:Company) WHERE … RETURN s.primaryCountry
/// AS from, cus.primaryCountry AS to, count(r) ORDER BY count DESC LIMIT
/// 15` expanded every Company and decoded every SUPPLIES in full (1.2 s
/// on the production port).
pub(crate) fn try_columnar_hop_aggregate(
    graph: &Graph,
    q: &SingleQuery,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    if !graph.columnar_scans_enabled() {
        sometimes!("interp.columnar paths switched off", true);
        return Ok(None);
    }
    let Some(plan) = recognise_hop(q) else {
        return Ok(None);
    };
    // Fix 58: a SOUGHT end drives the population. The whole-type walk
    // reads every relationship of the type and gathers both ends' columns
    // over every distinct end, whoever the statement asks about: the
    // mentioned-entity aggregate over ONE user's emails walked all 84k
    // MENTIONS and gathered 38k emails' columns for a user who owns twenty
    // (2.5 s against Neo4j's 2 ms; 2.7 s vs 150 for the user who owns 18k).
    // When an end's declared-key equality selects under half its label,
    // the population is that end's typed adjacency — (src, dst) in storage
    // order, no relationship record read — and the end columns are loaded
    // over the ends it actually reaches.
    let seeded_ends = hop_seeded_ends(graph, &plan, params)?;
    let seeded = seeded_ends.is_some();
    let ends: std::sync::Arc<Vec<(u64, u64)>> = match seeded_ends {
        Some(v) => std::sync::Arc::new(v),
        None => {
            let Some((rel_ids, rel_toks, ends)) =
                graph.rel_members(&plan.types).map_err(RunError::Graph)?
            else {
                sometimes!(
                    "interp.columnar rel scan declined by the entry budget",
                    true
                );
                return Ok(None);
            };
            let _ = (&rel_ids, &rel_toks);
            ends
        }
    };
    // A seeded population with no edge at all answers from an empty fold —
    // a user who owns nothing, an entity nothing mentions — before any end
    // column is loaded: an empty supplied population still sized its walk
    // by the end label's rows and gathered the whole label (12k gets for an
    // unknown user).
    if seeded && ends.is_empty() {
        counted!("interp.statements run");
        counted!("interp.columnar aggregate scans");
        counted!("interp.columnar hop aggregate scans");
        sometimes!("interp.columnar hop scan ran", true);
        let fold = Fold::new(&plan.items);
        let empty_vars = VarMap::new();
        let mut scope = Scope::over(params, &empty_vars, graph.wall_ms(), graph.zone_provider());
        let spec = FoldSpec {
            items: &plan.items,
            columns: &plan.columns,
            order: &plan.order,
            skip: plan.skip.as_ref(),
            limit: plan.limit.as_ref(),
            final_: plan.final_.as_ref(),
        };
        return fold.finish(graph, params, &spec, &mut scope).map(Some);
    }
    // End memberships (label filters) and columns.
    let a_ids = distinct_ends(&ends, plan.out);
    let b_ids = distinct_ends(&ends, !plan.out);
    let a_members = if plan.a.labels.is_empty() {
        None
    } else {
        Some(graph.members_all(&plan.a.labels).map_err(RunError::Graph)?)
    };
    let b_members = if plan.b.labels.is_empty() {
        None
    } else {
        Some(graph.members_all(&plan.b.labels).map_err(RunError::Graph)?)
    };
    // Each end's source carries ITS labels (fix 58): with the population
    // supplied, the loader takes its members from the ids, and the labels
    // only name the property-column cache entry the end's columns are
    // served from, restricted to the population — a labelled end used to
    // read through an unlabelled source, so its columns were gathered by a
    // record read per distinct end on every statement (492k gets for the
    // mentioned-entity aggregate). The walk keeps nothing: a supplied
    // population is never filed as the whole label.
    let a_source = Source::Nodes {
        labels: plan.a.labels.clone(),
        any_of: Vec::new(),
    };
    let b_source = Source::Nodes {
        labels: plan.b.labels.clone(),
        any_of: Vec::new(),
    };
    let a_rows = a_members
        .as_ref()
        .map(|m| m.len())
        .unwrap_or(0)
        .max(a_ids.len());
    let b_rows = b_members
        .as_ref()
        .map(|m| m.len())
        .unwrap_or(0)
        .max(b_ids.len());
    let Some(a_walk) = load_walk_budgeted(
        graph,
        &a_source,
        &plan.a.reads,
        Some(a_ids),
        Some(a_rows),
        params,
    )?
    else {
        // A node end column can no longer decline — a declined value column
        // gathers (v83) and a declined presence column gathers (v90) — so
        // this is reached only by a relationship-side budget decline. A
        // counter, not a floor state: the state it named is gone.
        counted!("interp.columnar hop scan declined an end column");
        return Ok(None);
    };
    let Some(b_walk) = load_walk_budgeted(
        graph,
        &b_source,
        &plan.b.reads,
        Some(b_ids),
        Some(b_rows),
        params,
    )?
    else {
        // A node end column can no longer decline — a declined value column
        // gathers (v83) and a declined presence column gathers (v90) — so
        // this is reached only by a relationship-side budget decline. A
        // counter, not a floor state: the state it named is gone.
        counted!("interp.columnar hop scan declined an end column");
        return Ok(None);
    };
    // The relationship walk (its columns in relationship order) belongs to
    // the whole-type population; a seeded population reads nothing of the
    // relationship (`hop_seeded_ends` requires it).
    let mut r_walk: Option<Walk> = if seeded {
        None
    } else {
        let rel_source = Source::Rels {
            types: plan.types.clone(),
        };
        let Some(w) = load_walk(graph, &rel_source, &plan.r_reads, params)? else {
            return Ok(None);
        };
        Some(w)
    };
    counted!("interp.statements run");
    counted!("interp.columnar aggregate scans");
    counted!("interp.columnar hop aggregate scans");
    sometimes!("interp.columnar hop scan ran", true);
    if a_members.is_some() || b_members.is_some() {
        sometimes!("interp.columnar hop scan filtered an end by label", true);
    }
    let mut fold = Fold::new(&plan.items);
    let empty_vars = VarMap::new();
    let mut scope = Scope::over(params, &empty_vars, graph.wall_ms(), graph.zone_provider());
    for ri in 0..ends.len() {
        let (src, dst) = ends[ri];
        let (a_id, b_id) = if plan.out { (src, dst) } else { (dst, src) };
        if let Some(m) = &a_members {
            if !m.contains(a_id) {
                continue;
            }
        }
        if let Some(m) = &b_members {
            if !m.contains(b_id) {
                continue;
            }
        }
        scope.locals.clear();
        if let Some(rw) = r_walk.as_mut() {
            let rel_id = rw.members[ri];
            rw.bind(graph, &plan.r_reads, &mut scope, ri, rel_id)?;
        }
        a_walk.bind_random(graph, &plan.a.reads, &mut scope, a_id)?;
        b_walk.bind_random(graph, &plan.b.reads, &mut scope, b_id)?;
        if let Some(pred) = &plan.pred {
            let v = eval_with(pred, &scope, None).map_err(RunError::Eval)?;
            match v.truth() {
                Some(Truth::True) => {}
                Some(_) => continue,
                None => {
                    return Err(RunError::Semantic(format!(
                        "WHERE takes a boolean, got {}",
                        v.type_name()
                    )));
                }
            }
        }
        fold.push(graph, &scope)?;
    }
    let spec = FoldSpec {
        items: &plan.items,
        columns: &plan.columns,
        order: &plan.order,
        skip: plan.skip.as_ref(),
        limit: plan.limit.as_ref(),
        final_: plan.final_.as_ref(),
    };
    fold.finish(graph, params, &spec, &mut scope).map(Some)
}
