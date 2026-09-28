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
    Clause, NodePattern, OrderItem, PathPattern, Pattern, ProjItem, Projection, RelDir,
    SingleQuery,
};
use engram_cypher::{Truth, Value};
use engram_observe::{counted, sometimes};

use crate::interp::{
    AggSite, QueryResult, Row, RunError, SiteAcc, agg_key_of, best_declared_seek, budget_check,
    cmp_order_keys, column_name, conjunct_count, contains_opaque, eval_count, free_vars_of,
    prop_eq_candidates, seek_candidates,
};
use crate::pipeline::subquery_end_gather_enabled;
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

    /// Nothing is read of the variable: no property value or presence, no
    /// label test, probe or degree, neither its id nor its type.
    fn reads_nothing(&self) -> bool {
        self.props.is_empty()
            && self.presence.is_empty()
            && self.labels.is_empty()
            && self.probes.is_empty()
            && self.degrees.is_empty()
            && !self.id_read
            && !self.type_read
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
            p.dir == dir
                && p.types == types
                && p.labels == labels
                && p.end_filter.as_ref() == end_filter
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
    texts: Vec<(String, engram_cypher::BinOp, String)>,
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
    let only_seek_keys = r.props.iter().all(|p| {
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
            presence.push((
                local_for_prop(&reads.tag, &p),
                vec![Value::Null; members.len()],
            ));
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

/// Fix 82: whether a scan of `n` members visits its chunks from BOTH ends
/// — a CAPPED scan (a bare LIMIT, no ORDER BY) of more than one chunk.
/// Ids are minted in creation order, so a label's newest members sit at
/// the end of id order, and the listings that cap without ordering are
/// recency-filtered (`lastUpdatedAt > $cutoff`, `status <> 'stale'`): the
/// forward scan met its fifth NewsStory match after the whole label (8 ms
/// on the mirror — every chunk evaluated, the limit reached in the last)
/// where Neo4j's scan of the storyId index meets matches spread at random
/// through UUID order in under 2. A bare LIMIT wants ANY k matches; the k
/// found come back in id order. The column-at-a-time scan and the
/// per-member walk share the order, so a statement answers the same k
/// rows cold (the walk that assembles the columns) and warm.
///
/// Fix 122: and only with a PREDICATE. Both-ends exists to meet a selective
/// filter sooner when the matches sit at the end of id order; with no filter
/// every member matches, the first chunk already answers the whole limit, and
/// reordering the chunks buys nothing while costing locality. The bare
/// listing `MATCH (p:Person) RETURN p.id, p.firstName LIMIT 5000` took this
/// path for no reason.
fn scan_from_both_ends(n: usize, cap: Option<usize>, has_pred: bool) -> bool {
    has_pred && cap.is_some() && n.div_ceil(PRED_CHUNK) > 1
}

/// The chunks of a scan of `n` members in visiting order: forward, or from
/// both ends — the last chunk first, then the first, then the second-last…
fn scan_chunk_order(n: usize, cap: Option<usize>, has_pred: bool) -> Vec<usize> {
    let chunks = n.div_ceil(PRED_CHUNK);
    if !scan_from_both_ends(n, cap, has_pred) {
        return (0..chunks).collect();
    }
    counted!("interp.columnar projection scanned its chunks from both ends for the limit");
    let mut order = Vec::with_capacity(chunks);
    let (mut a, mut b) = (0usize, chunks);
    while a < b {
        b -= 1;
        order.push(b);
        if a < b {
            order.push(a);
            a += 1;
        }
    }
    order
}

/// Put projected rows (their id trailing, `project_row`) and their order
/// keys into id order — the per-member walk's answer after a scan from
/// both ends, as the column-at-a-time scan answers it.
fn sort_rows_by_trailing_id(rows: &mut Vec<Vec<Value>>, keys: &mut Vec<Vec<Value>>) {
    let mut perm: Vec<usize> = (0..rows.len()).collect();
    perm.sort_by_key(|&i| match rows[i].last() {
        Some(Value::Int(id)) => *id,
        _ => i64::MAX,
    });
    let mut r: Vec<Option<Vec<Value>>> = std::mem::take(rows).into_iter().map(Some).collect();
    let mut k: Vec<Option<Vec<Value>>> = std::mem::take(keys).into_iter().map(Some).collect();
    for &i in &perm {
        rows.push(r[i].take().expect("row"));
        keys.push(k[i].take().expect("key"));
    }
}

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
    let from_both_ends = scan_from_both_ends(n, cap, true);
    let mut hits: Vec<usize> = Vec::new();
    let mut done = false;
    for c in scan_chunk_order(n, cap, true) {
        let lo = c * PRED_CHUNK;
        let hi = n.min(lo + PRED_CHUNK);
        let view = cols.view(lo, hi);
        let truth = crate::vectorized::eval_column(pred, "", hi - lo, &view, scope)?;
        for (i, v) in truth.iter().enumerate() {
            match v.truth() {
                Some(Truth::True) => {
                    hits.push(lo + i);
                    if cap.is_some_and(|c| hits.len() >= c) {
                        done = true;
                        break;
                    }
                }
                Some(_) => {}
                None => return None,
            }
        }
        if done {
            break;
        }
    }
    if from_both_ends {
        hits.sort_unstable();
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
    if path.shortest.is_some() || path.var.is_some() || path.hops.len() != 1 {
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
        (Some(a), None) if bare(&path.start) && unbound_far(end) => (
            a,
            end,
            if rel.dir == RelDir::Out {
                Dir::Out
            } else {
                Dir::In
            },
        ),
        (None, Some(b)) if bare(end) && unbound_far(&path.start) => (
            b,
            &path.start,
            if rel.dir == RelDir::Out {
                Dir::In
            } else {
                Dir::Out
            },
        ),
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
            if label.is_none()
                || contains_opaque(w)
                || !reads_only(w, std::slice::from_ref(&fv.to_string()))
            {
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
    // Every demanded column must be CACHED before any adjacency is read.
    // Fix 79: one that is not is LOADED WHOLE and kept (`label_value_columns`
    // — the read the property-column cache files), for a label small
    // enough that reading it whole is the cost of one of today's
    // evaluations, not a gamble. The KMProject dashboard's nine
    // `COUNT { (w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) WHERE
    // coalesce(w.status, 'backlog') = … }` per project declined here on
    // every run, because nothing had ever read `KMWorkItem.status` as a
    // column (the listings bind `properties(w)` whole): 14k projected
    // record reads per statement, 156 ms against Neo4j's 21.5 on the
    // mirror after fix 70. One whole-label read is ~15k gets once; every
    // later body over that label is column-at-a-time. A label past the
    // bound, or a load that declines, still hands the body to the matcher.
    //
    // Fix 121: the ENDS are collected first, because what they cost to read is
    // the whole question. Before this, the column loop ran first and a label
    // past the whole-read ceiling simply declined — permanently, since the two
    // sites that would mint the column refuse on the same constant (fix 118).
    // The fallback was one projected record read per end: 113,065 of them for
    // a query returning twenty-five rows, 137 ms against Neo4j's 28.
    //
    // Knowing the ends first turns the ceiling from a cliff into a choice.
    // The label may be 3,055,774 nodes, but this hop touches a few thousand,
    // and gathering exactly those is bounded by the FAN-OUT rather than by the
    // label. The bare constant remains the right guard for the two `interp.rs`
    // warm sites, which mint a WHOLE column and genuinely scale with the label.
    let mut ids: Vec<u64> = Vec::new();
    graph.adjacent_slim_for_each(from, dir, &tokens, |e| ids.push(e.peer));
    if let Some(l) = label {
        let members = graph.members(Some(l))?;
        ids.retain(|id| graph.members_contains(&members, *id));
    }
    let mut columns: Vec<GatheredColumn> = Vec::new();
    if let Some(l) = label {
        let mut wanted: Vec<(String, bool)> =
            reads.props.iter().map(|p| (p.clone(), false)).collect();
        wanted.extend(reads.presence_only().into_iter().map(|p| (p, true)));
        for (p, presence) in wanted {
            let col = match graph.prop_column(l, &p, false) {
                Some(PropColumn::Values(col)) => col,
                _ => {
                    if graph.count_label_nodes(l) > graph.whole_label_read_max() {
                        // Fix 118's counter. This decline WAS permanent: the
                        // column is absent, the label is over the ceiling, and
                        // the two sites that would mint it refuse on the same
                        // constant. Measured 213 ms then 265 ms over two
                        // passes of the same eight seeds, with no learning.
                        counted!("interp.subquery hop declined: label over the whole-read ceiling");
                        // Fix 121: gather just this hop's ends instead. The
                        // cost is |ends|, not |label|, so the comparison that
                        // matters is between them — a fan-out approaching the
                        // label's size gains nothing and keeps the old path.
                        // `LEAN_COLUMN_BATCH` is the floor: below it, today's
                        // projected reads are already cheap and a gather would
                        // only add a sort.
                        if !subquery_end_gather_enabled()
                            || ids.len() < crate::interp::LEAN_COLUMN_BATCH
                            || (ids.len() as u64).saturating_mul(2) >= graph.count_label_nodes(l)
                        {
                            return Ok(None);
                        }
                        let mut want: Vec<u64> = ids.clone();
                        want.sort_unstable();
                        want.dedup();
                        let Ok(mut gathered) = graph.column_entries_gather_many(
                            ColumnFamily::Nodes,
                            std::slice::from_ref(&p),
                            &want,
                        ) else {
                            return Ok(None);
                        };
                        let Some(mut col) = gathered.pop() else {
                            return Ok(None);
                        };
                        // The lookup below is a binary search, so the gather
                        // has to arrive sorted by id whatever order it came in.
                        col.sort_unstable_by_key(|(i, _)| *i);
                        counted!("interp.subquery hop gathered only its own ends");
                        columns.push((
                            local_for_prop(&reads.tag, &p),
                            std::sync::Arc::new(col),
                            presence,
                        ));
                        continue;
                    }
                    let Some(mut loaded) =
                        label_value_columns(graph, l, std::slice::from_ref(&p), params)?
                    else {
                        return Ok(None);
                    };
                    let Some(col) = loaded.pop() else {
                        return Ok(None);
                    };
                    counted!("interp.subquery hop loaded its far end's column whole");
                    std::sync::Arc::new(col)
                }
            };
            columns.push((local_for_prop(&reads.tag, &p), col, presence));
        }
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

/// Fix 108: ANY aggregate over ONE label whose predicate, grouping keys and
/// aggregate arguments read only CACHED columns, folded COLUMN-AT-A-TIME:
/// the predicate over the aligned columns as vectors, every key and
/// argument evaluated as a column too, and the survivors gathered per
/// position into the fold in member order — the walk's — so the groups'
/// first-seen order is the walk's and every value is the vectoriser's own
/// (which mirrors `eval_with` per element). The count-star form had this
/// (`count_over_cached_columns`); a grouped `count(a)` over the same cached
/// columns still bound a scope and walked its predicate and key per member:
/// the production NewsArticle classification `MATCH (a:NewsArticle) WHERE
/// a.classifiedAt IS NOT NULL AND a.pubDate >= $c AND (a.abuseStatus IS
/// NULL OR …) AND a.contentType IS NOT NULL RETURN a.contentType AS key,
/// count(a)` evaluated 165k expressions over its 67k survivors — 111 ms on
/// the mirror where its count took 22. `Ok(false)` when a column is not
/// cached (the walk assembles and keeps it), a key or argument is a form
/// `eval_column` declines, or the predicate answers a non-boolean (the walk
/// raises it) — the walk answers, as before.
fn fold_over_cached_columns<'p>(
    graph: &Graph,
    label: &str,
    plan: &'p Plan,
    scope: &Scope<'_>,
    fold: &mut Fold<'p>,
) -> Result<bool, RunError> {
    let members = graph
        .members_all(std::slice::from_ref(&label.to_string()))
        .map_err(RunError::Graph)?
        .to_arc_vec();
    let Some(cols) = aligned_columns(graph, label, &plan.reads, &members) else {
        return Ok(false);
    };
    let n = members.len();
    // `id(var)` (fix 46's local): the member's own id, never a record read.
    let id_col: Vec<Value> = if plan.reads.id_read {
        members.iter().map(|&id| Value::Int(id as i64)).collect()
    } else {
        Vec::new()
    };
    // A label wider than the aggregate's batch is folded in member BATCHES —
    // each batch's view, predicate, keys and arguments alone are held — as
    // the walk batches; the fold accumulates across them in member order.
    let batch = if graph.columnar_agg_batch_enabled() && n > graph.columnar_agg_batch_size() {
        graph.columnar_agg_batch_size().max(1)
    } else {
        n.max(1)
    };
    // Folded into a fresh fold and handed over whole: a decline in a later
    // batch (a non-boolean predicate value, a form the vectoriser refuses
    // on that batch's values) leaves the caller's fold untouched.
    let mut local = Fold::new(&plan.items);
    let mut lo = 0usize;
    while lo < n {
        let hi = (lo + batch).min(n);
        let len = hi - lo;
        let mut view = cols.view(lo, hi);
        if plan.reads.id_read {
            view.insert(local_for_id(&plan.reads.tag), &id_col[lo..hi]);
        }
        let survivors: Vec<usize> = match &plan.pred {
            None => (0..len).collect(),
            Some(pred) => {
                let Some(truth) = crate::vectorized::eval_column(pred, "", len, &view, scope)
                else {
                    return Ok(false);
                };
                let mut s = Vec::new();
                for (i, v) in truth.iter().enumerate() {
                    match v.truth() {
                        Some(Truth::True) => s.push(i),
                        Some(_) => {}
                        None => return Ok(false),
                    }
                }
                s
            }
        };
        if !survivors.is_empty() {
            let mut keys: Vec<crate::vectorized::Col<'_>> = Vec::new();
            let mut args: Vec<Option<crate::vectorized::Col<'_>>> = Vec::new();
            for it in &plan.items {
                match it {
                    Item::Key(e) => {
                        let Some(c) = crate::vectorized::eval_column(e, "", len, &view, scope)
                        else {
                            return Ok(false);
                        };
                        keys.push(c);
                    }
                    Item::Agg(_, None) => args.push(None),
                    Item::Agg(_, Some(a)) => {
                        let Some(c) = crate::vectorized::eval_column(a, "", len, &view, scope)
                        else {
                            return Ok(false);
                        };
                        args.push(Some(c));
                    }
                }
            }
            for &i in &survivors {
                let key: Vec<Value> = keys.iter().map(|k| k[i].clone()).collect();
                local.push_values(
                    graph,
                    key,
                    args.iter().map(|a| a.as_ref().map(|c| c[i].clone())),
                )?;
            }
        }
        lo = hi;
    }
    // The counters the walk reports for the same reads: each value column
    // served from the property-column cache, an id bound without a record.
    for _ in &cols.values {
        counted!("interp.columnar column read served from the property-column cache");
    }
    if plan.reads.id_read {
        counted!("interp.columnar id bound from the walk");
    }
    if batch < n {
        counted!("interp.columnar aggregate batched");
    }
    *fold = local;
    Ok(true)
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
    // Fix 95: a label with no live node counts zero — no index built or
    // read for it (`MATCH (p:Part {orgId: $orgId}) RETURN count(p)` built
    // and queried the scoped index for a label that has never held a node).
    if graph.count_label_nodes(label) == 0 {
        counted!("interp.seed answered empty from a label with no member");
        return Ok(Some(0));
    }
    let labels = [label.to_string()];
    let mut acc: Option<Vec<u64>> = None;
    // Fix 115: a DECLARED COMPOSITE whose keys are exactly the seeks (one
    // string each, nothing else to intersect) is ONE probe — the tuple's
    // exact range — where the per-key probes below extract each key's whole
    // match set to intersect them: `{userId: $u, nodeType: 'contact'}`
    // gathered the user's every node and every contact in the store to
    // answer 391 (0.6 ms in the engine against Neo4j's whole 0.7).
    if ranges.is_empty() && prefixes.is_empty() {
        if let Some((covered, ids)) = composite_seek(graph, &labels, seeks, None, scope)? {
            if covered.len() == seeks.len() {
                counted!("interp.columnar covered count sought a composite");
                let members = graph.members_all(&labels).map_err(RunError::Graph)?;
                let n = ids
                    .iter()
                    .filter(|id| graph.members_contains(&members, **id))
                    .count() as u64;
                counted!("interp.columnar covered count");
                return Ok(Some(n));
            }
        }
    }
    // A RANGE (`prop > x`, …) on a declared key is a range of the same index
    // (fix 47) — exact for string keys; a non-string bound declines.
    for (prop, op, e) in ranges {
        let Some(scoped) = graph
            .declared_scope_for(&labels, prop)
            .map_err(RunError::Graph)?
        else {
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
        let Some(scoped) = graph
            .declared_scope_for(&labels, prop)
            .map_err(RunError::Graph)?
        else {
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
        let Some(scoped) = graph
            .declared_scope_for(&labels, prop)
            .map_err(RunError::Graph)?
        else {
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

/// What a composite probe answered: the seek positions the composite's keys
/// covered, and the ids carrying that tuple.
type CompositeHit = (Vec<usize>, Vec<u64>);

/// Fix 115: the seeks' single-valued keys probed as ONE tuple against the
/// declared composite index they cover — `(the seek positions the
/// composite answered, ids)`, or `None` when no declared composite fits
/// them, a value is not a string, or the match set is over `cap`. The ids
/// are a candidate set: exact for the covered keys, a superset for the
/// caller's whole predicate.
fn composite_seek(
    graph: &Graph,
    labels: &[String],
    seeks: &[(String, Vec<Expr>)],
    cap: Option<usize>,
    scope: &Scope,
) -> Result<Option<CompositeHit>, RunError> {
    if seeks.len() < 2 {
        return Ok(None);
    }
    let single: Vec<&str> = seeks
        .iter()
        .filter(|(_, values)| values.len() == 1)
        .map(|(k, _)| k.as_str())
        .collect();
    let Some((label, props)) = graph
        .declared_composite_for(labels, &single)
        .map_err(RunError::Graph)?
    else {
        return Ok(None);
    };
    let mut covered = Vec::with_capacity(props.len());
    let mut values = Vec::with_capacity(props.len());
    for p in &props {
        let Some(i) = seeks
            .iter()
            .position(|(k, values)| k == p && values.len() == 1)
        else {
            return Ok(None);
        };
        let v = eval_with(&seeks[i].1[0], scope, None).map_err(RunError::Eval)?;
        if !matches!(v, Value::Str(_)) {
            return Ok(None);
        }
        covered.push(i);
        values.push(v);
    }
    let Some(ids) = graph
        .index_probe_composite(&label, &props, &values, cap)
        .map_err(RunError::Graph)?
    else {
        return Ok(None);
    };
    Ok(Some((covered, ids)))
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

/// The four kinds of seekable conjunct a clause can offer, gathered.
///
/// One struct rather than four parameters because they always travel together
/// and are always derived from the same WHERE — and because a fifth kind would
/// otherwise mean touching every call site again, which is how the trigram
/// group arrived.
struct SeekCandidates<'a> {
    /// `prop = x` / `prop IN [...]`.
    seeks: &'a [(String, Vec<Expr>)],
    /// `prop STARTS WITH x`.
    prefixes: &'a [(String, Expr)],
    /// `prop > x`, and the rest of the comparisons.
    ranges: &'a [(String, engram_cypher::BinOp, Expr)],
    /// `prop =~ / CONTAINS / STARTS WITH / ENDS WITH 'literal'` conjuncts,
    /// raw. The condition is derived in the loop, after the declared-index
    /// lookup, so an undeclared property costs no analysis at all.
    texts: &'a [(String, engram_cypher::BinOp, String)],
}

impl SeekCandidates<'_> {
    /// Nothing to seek on.
    fn is_empty(&self) -> bool {
        self.seeks.is_empty()
            && self.prefixes.is_empty()
            && self.ranges.is_empty()
            && self.texts.is_empty()
    }
}

fn columnar_seek_ids(
    graph: &Graph,
    labels: &[String],
    cands: &SeekCandidates<'_>,
    use_: SeekUse,
    scope: &Scope,
) -> Result<Option<Vec<u64>>, RunError> {
    let SeekCandidates {
        seeks,
        prefixes,
        ranges,
        texts,
    } = *cands;
    if cands.is_empty() || labels.is_empty() || !graph.property_seek_enabled() {
        return Ok(None);
    }
    // Prefix and range candidates seek DECLARED keys only (an equality may
    // still probe an undeclared first conjunct). With no equality and no
    // declared key there is nothing to probe — and the label-size test
    // below would rebuild the stats on a cold store for nothing: a
    // whole-store pass charged to a two-node label's budgeted read
    // (`population_scan_interleaved_bare_on_a_paged_store_stops_fetching_at_the_budget`).
    if seeks.is_empty() {
        let mut declared = texts
            .iter()
            .any(|(p, _, _)| graph.declared_trigram_for(labels, p).is_some());
        for prop in prefixes
            .iter()
            .map(|(p, _)| p)
            .chain(ranges.iter().map(|(p, _, _)| p))
        {
            if declared {
                break;
            }
            if graph
                .declared_scope_for(labels, prop)
                .map_err(RunError::Graph)?
                .is_some()
            {
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
    // Fix 95: a label with no live node seeks nothing — see
    // `best_declared_seek`, the general path's rule. Behind the size test
    // above on purpose: the count is a maintained statistic the seek
    // consults anyway, never a cold-store rebuild for a two-node label.
    if labels.iter().any(|l| graph.count_label_nodes(l) == 0) {
        counted!("interp.seed answered empty from a label with no member");
        return Ok(Some(Vec::new()));
    }
    let (cap_n, selectivity) = match use_ {
        SeekUse::PerId => (
            crate::PROPERTY_SEEK_MAX_PROBE,
            crate::PROPERTY_SEEK_SELECTIVITY,
        ),
        SeekUse::Walk => (SEEK_WALK_CAP, SEEK_WALK_SELECTIVITY),
    };
    let cap = Some(cap_n);
    let mut best: Option<(Vec<u64>, bool)> = None; // (ids, came from a declared scoped index)
    // PREFIX candidates (`prop STARTS WITH 'x'`) on DECLARED keys only — a
    // prefix is a range over the index the operator declared; nothing is
    // built for an undeclared one.
    for (prop, e) in prefixes {
        let Some(l) = graph
            .declared_scope_for(labels, prop)
            .map_err(RunError::Graph)?
        else {
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
        let Some(l) = graph
            .declared_scope_for(labels, prop)
            .map_err(RunError::Graph)?
        else {
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
    // TEXT candidates (`=~`, `CONTAINS`, `STARTS WITH`, `ENDS WITH`) on a
    // DECLARED trigram index. The answer is a CANDIDATE set that the walk's
    // own predicate re-checks, exactly as a prefix or range candidate is —
    // which is why this competes on `ids.len()` alongside them rather than
    // short-circuiting. A range index answers a prefix better than trigrams
    // do, and when one is declared it simply wins here on its merits.
    for (prop, op, text) in texts {
        let Some(l) = graph.declared_trigram_for(labels, prop) else {
            continue;
        };
        // Derived HERE, not in the recogniser: a pattern parse and a tree
        // analysis are wasted work on a property nobody indexed.
        let Some(q) = crate::interp::text_query_for(*op, text) else {
            continue;
        };
        // The cap here is the LABEL'S, not the caller's: see `text_seek_cap`.
        let text_cap =
            Some(cap.map_or(graph.text_seek_cap(&l), |c| c.min(graph.text_seek_cap(&l))));
        if let Some(ids) = graph.trigram_probe_scoped(prop, &q, text_cap, &l) {
            counted!("interp.columnar seek probed a declared trigram index");
            if best.as_ref().is_none_or(|(b, _)| ids.len() < b.len()) {
                best = Some((ids, true));
            }
        }
    }
    // Fix 115: the declared COMPOSITE the seeks cover, probed as one tuple —
    // the exact population of those keys, where a single key's index names
    // every node carrying that one value.
    if let Some((_, ids)) = composite_seek(graph, labels, seeks, cap, scope)? {
        counted!("interp.columnar seek probed a declared composite");
        if best.as_ref().is_none_or(|(b, _)| ids.len() < b.len()) {
            best = Some((ids, true));
        }
    }
    for (i, (prop, values)) in seeks.iter().enumerate() {
        // The declared index on a label the pattern requires whose FIRST
        // property is this key — a composite is ordered by it, so an equality
        // on it is the prefix the index answers. One rule for every seek site:
        // `Graph::declared_scope_for`.
        let scoped_to = graph
            .declared_scope_for(labels, prop)
            .map_err(RunError::Graph)?;
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
    if path.shortest.is_some() || path.var.is_some() || path.hops.len() != 1 {
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
    if path.var.is_some() || path.shortest.is_some() {
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
    let texts = crate::interp::prop_text_candidates(full_where.as_ref(), &var);
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
    let agg_proj = &star_distinct_counts(agg_proj, &var);
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
        texts,
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
    /// NOT shared with the property-column cache, deliberately. Fix 122
    /// tried: `keep_prop_column` takes `columns[j].clone()`, a second full
    /// copy of every column of a whole-label read, and sharing an `Arc` with
    /// the cache looks like a free removal of it. It is not. `Walk::bind`
    /// MOVES each value out with `std::mem::replace(&mut col[*cur].1,
    /// Value::Null)` rather than cloning it, so a shared column would be
    /// hollowed out under the cache — and `Arc::make_mut` would only restore
    /// the clone lazily, per column, while adding a per-value clone to every
    /// bind. One column copy at keep time is the cheaper half of that trade.
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
    // from the first member's entry, not the column's start: a share cut from
    // the middle of a label (`parallel_stage_fold`) walked all before it
    let mut ci = ids.first().map_or(0, |&f| col.partition_point(|(id, _)| *id < f));
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

/// [`restrict_entries_to`] that TAKES the values out of an owned column
/// (the whole-label walk's own copy, which nothing else reads) instead of
/// cloning them — a 38k-entry column of list values restricted to 18k
/// members would otherwise clone every list.
fn take_entries_to(col: &mut [(u64, Value)], ids: &[u64]) -> Vec<(u64, Value)> {
    let mut out = Vec::with_capacity(ids.len());
    let mut ci = 0usize;
    for &id in ids {
        while ci < col.len() && col[ci].0 < id {
            ci += 1;
        }
        if ci < col.len() && col[ci].0 == id {
            out.push((id, std::mem::replace(&mut col[ci].1, Value::Null)));
        }
    }
    out
}

/// Fix 78: a population at least this share of its label (1/N) is read as
/// the whole label so the columns are kept.
pub(crate) const WHOLE_LABEL_SHARE: u64 = 8;
/// ...and only for a label up to this many members: reading a bigger label
/// whole for a fraction of it is a gamble the population read need not take.
pub(crate) const WHOLE_LABEL_READ_MAX: u64 = 262_144;

/// [`restrict_entries_to`] for a presence column (ids only).
fn restrict_ids_to(present: &[u64], ids: &[u64]) -> Vec<u64> {
    let mut out = Vec::with_capacity(ids.len().min(present.len()));
    // from the first member's entry (see `restrict_entries_to`)
    let mut ci = ids.first().map_or(0, |&f| present.partition_point(|id| *id < f));
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
        let cands = SeekCandidates {
            seeks: &seeks,
            prefixes: &[],
            ranges: &[],
            texts: &[],
        };
        let over = match columnar_seek_ids(graph, &p.labels, &cands, SeekUse::PerId, &scope)? {
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
        let Some(ids) = filter_ids_in(graph, &p.labels, PROBE_END_VAR, &pred, params, over)? else {
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
    // Fix 78: a SUPPLIED population that is a large share of its one label
    // is read as the WHOLE label and restricted afterwards. A population
    // read is never kept (the cache holds whole-label columns only), so a
    // listing over most of a label re-read its columns on every statement:
    // the email classification listing projected eight properties over 18k
    // of the 38k UserDataNode emails and gathered 18k records per run
    // (1,005 ms against Neo4j's 207), and the next run did it again. The
    // whole-label read costs what the population read costs — the same
    // span when the ids spread over the label, else a gather of the label
    // instead of most of it — and the columns it assembles are KEPT, so the
    // next statement over the label reads nothing. Taken only for a
    // one-label node source with no probe or degree (their per-member
    // answers are aligned to the population), when a demanded column is
    // not already cached, the population is at least an eighth of the
    // label, and the label is not so large that reading it whole is a
    // gamble; a declined whole-label walk falls back to the population read.
    if let (Some(over_ids), Source::Nodes { labels, any_of }) = (&over, source) {
        if labels.len() == 1
            && any_of.is_empty()
            && reads.probes.is_empty()
            && reads.degrees.is_empty()
            && !reads.type_read
        {
            let label = labels[0].as_str();
            let total = graph.count_label_nodes(label);
            let uncached = reads.props.iter().any(|p| {
                !matches!(
                    graph.prop_column(label, p, false),
                    Some(PropColumn::Values(_))
                )
            }) || reads.presence_only().iter().any(|p| {
                !matches!(
                    graph.prop_column(label, p, true),
                    Some(PropColumn::Presence(_))
                )
            });
            if uncached
                && total <= graph.whole_label_read_max()
                && (over_ids.len() as u64).saturating_mul(WHOLE_LABEL_SHARE) >= total
            {
                if let Some(mut walk) =
                    load_walk_budgeted(graph, source, reads, None, None, params)?
                {
                    counted!("interp.columnar population read its label whole to keep the columns");
                    for col in walk.columns.iter_mut() {
                        *col = take_entries_to(col, over_ids);
                    }
                    walk.cursors = vec![0; walk.columns.len()];
                    for (_, ids, cur) in walk.presence.iter_mut() {
                        *ids = restrict_ids_to(ids, over_ids);
                        *cur = 0;
                    }
                    for (_, _, cur) in walk.label_members.iter_mut() {
                        *cur = 0;
                    }
                    walk.members = std::sync::Arc::clone(over_ids);
                    return Ok(Some(walk));
                }
                counted!("interp.columnar whole-label read for a population declined");
            }
        }
    }
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
                graph
                    .members_any(any_of)
                    .map_err(RunError::Graph)?
                    .to_arc_vec(),
                Vec::new(),
                ColumnFamily::Nodes,
            ),
            Source::Nodes { labels, .. } => (
                graph
                    .members_all(labels)
                    .map_err(RunError::Graph)?
                    .to_arc_vec(),
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
    // Fix 106: a walk that is NOT kept — an unlabelled population, or a
    // labelled one read over a supplied subset — is skipped as soon as the
    // span is wider than the budget: the walk visits every row of the span
    // and declines past the budget, so on a dense store it can only
    // decline, after the visits, and the gather then runs anyway (the
    // seeded MENTIONS aggregate's unlabelled ends walked twice and gathered
    // once per property: 909 ms on the hop-listing bench against the
    // labelled spelling's 139). A whole-label walk keeps the eight-budget
    // rule: served, it is KEPT as the label's column for every later read.
    let sparse_label = span > budget.saturating_mul(if whole_label { 8 } else { 1 });
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
            if whole_label {
                counted!("interp.columnar column read skipped the span walk for a sparse label");
            } else {
                counted!(
                    "interp.columnar column read skipped the span walk for a sparse population"
                );
            }
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
            if whole_label {
                counted!("interp.columnar column read skipped the span walk for a sparse label");
            } else {
                counted!(
                    "interp.columnar column read skipped the span walk for a sparse population"
                );
            }
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
            graph
                .members(Some(l))
                .map_err(RunError::Graph)?
                .to_arc_vec(),
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
    let in_table_range = members
        .last()
        .is_none_or(|&m| m <= crate::DEGREE_TABLE_MAX_ID);
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
    /// Fix 109: place every sequential cursor at the first entry of id `id`
    /// or above — for a walk whose members are visited in CHUNKS out of id
    /// order (fix 82's both-ends order for a bare LIMIT). `bind`'s cursors
    /// only advance, so a later chunk of LOWER ids found them already past
    /// its entries and bound Null: the production `MATCH (a:NewsArticle)
    /// RETURN a.articleId AS id LIMIT 5000` — two chunks, the last visited
    /// first — answered 4,096 rows of `id: null`. Within a chunk the ids
    /// ascend and chunks never overlap, so a value is still taken once.
    fn seek_cursors(&mut self, id: u64) {
        for (ci, col) in self.columns.iter().enumerate() {
            self.cursors[ci] = col.partition_point(|(cid, _)| *cid < id);
        }
        for (_, ids, cur) in self.presence.iter_mut() {
            *cur = ids.partition_point(|&cid| cid < id);
        }
        for (_, members, cur) in self.label_members.iter_mut() {
            *cur = members.partition_point(|&cid| cid < id);
        }
    }

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
                if let Some(n) = covered_count(
                    graph,
                    &labels[0],
                    &plan.seeks,
                    &plan.prefixes,
                    &plan.ranges,
                    &scope,
                )? {
                    sought = true;
                    counted!("interp.statements run");
                    counted!("interp.columnar aggregate scans");
                    sometimes!(
                        "interp.columnar aggregate counted an index intersection",
                        true
                    );
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
                let cands = SeekCandidates {
                    seeks: &plan.seeks,
                    prefixes: &plan.prefixes,
                    ranges: &plan.ranges,
                    texts: &plan.texts,
                };
                if let Some(ids) = columnar_seek_ids(graph, labels, &cands, SeekUse::PerId, &scope)?
                {
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
                let cands = SeekCandidates {
                    seeks: &plan.seeks,
                    prefixes: &plan.prefixes,
                    ranges: &plan.ranges,
                    texts: &plan.texts,
                };
                walk_seek = columnar_seek_ids(graph, labels, &cands, SeekUse::Walk, &scope)?;
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
    // Fix 108: any other aggregate over ONE label with every column CACHED —
    // grouped, DISTINCT, a non-star argument — folded over the columns as
    // vectors, no per-member scope (`fold_over_cached_columns`).
    if !sought
        && !prefer_walk
        && plan.reads.probes.is_empty()
        && plan.reads.degrees.is_empty()
        && plan.reads.labels.is_empty()
        && !plan.reads.type_read
    {
        if let Source::Nodes { labels, any_of } = &plan.source {
            if labels.len() == 1
                && any_of.is_empty()
                && fold_over_cached_columns(graph, &labels[0], &plan, &scope, &mut fold)?
            {
                sought = true;
                counted!("interp.statements run");
                counted!("interp.columnar aggregate scans");
                counted!("interp.columnar aggregate folded over cached columns");
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
                            counted!(
                                "interp.columnar aggregate walked a selective seek instead of vectorising"
                            );
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
                let Some(mut walk) =
                    load_walk_over(graph, &plan.source, &plan.reads, Some(over), params)?
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
    /// A share's PARTIAL fold (`parallel_stage_fold`): its sums and averages
    /// keep their float addends in arrival order (`SiteAcc::defer_floats`).
    partial: bool,
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
            partial: false,
        }
    }

    /// The group the bound row belongs to — its key evaluated, the group
    /// made on first sight.
    fn group_of(&mut self, graph: &Graph, scope: &Scope<'_>) -> Result<usize, RunError> {
        let mut key = Vec::with_capacity(self.key_exprs.len());
        for k in &self.key_exprs {
            key.push(eval_with(k, scope, None).map_err(RunError::Eval)?);
        }
        self.group_for(graph, key)
    }

    /// Fold one row from PRE-EVALUATED key values and site arguments (fix
    /// 108): the group by its key, each site pushed its argument — `None`
    /// for a star site — exactly as `push` folds a bound row.
    fn push_values(
        &mut self,
        graph: &Graph,
        key: Vec<Value>,
        args: impl Iterator<Item = Option<Value>>,
    ) -> Result<(), RunError> {
        let gi = self.group_for(graph, key)?;
        let accs = &mut self.groups[gi].1;
        for (acc, v) in accs.iter_mut().zip(args) {
            acc.push(v)?;
        }
        Ok(())
    }

    /// The group an evaluated key belongs to, made on first sight.
    fn group_for(&mut self, graph: &Graph, key: Vec<Value>) -> Result<usize, RunError> {
        let ser = agg_key_of(&key, &mut self.nonce);
        Ok(match self.group_index.get(&ser) {
            Some(&i) => i,
            None => {
                let mut accs: Vec<SiteAcc> =
                    self.sites.iter().map(|(s, _)| SiteAcc::for_site(s)).collect();
                if self.partial {
                    accs.iter_mut().for_each(SiteAcc::defer_floats);
                }
                self.groups.push((key, accs));
                self.group_index.insert(ser, self.groups.len() - 1);
                budget_check(graph, self.groups.len())?;
                self.groups.len() - 1
            }
        })
    }

    /// Merge a LATER share's partial into this one (`parallel_stage_fold`):
    /// what folding its rows after this one's would have made. Its groups
    /// follow in its own first-seen order; a group both saw merges its
    /// accumulators ([`SiteAcc::merge_in_order`]); NaN keys never meet, as
    /// they never do in one fold (each partial numbers its own apart).
    /// `Ok(false)` when an accumulator cannot merge exactly — the caller
    /// then folds on its own thread.
    fn merge_later(&mut self, graph: &Graph, later: Fold<'p>) -> Result<bool, RunError> {
        let mut sers: Vec<Vec<u8>> = vec![Vec::new(); later.groups.len()];
        for (ser, i) in later.group_index {
            sers[i] = ser;
        }
        for ((key, accs), ser) in later.groups.into_iter().zip(sers) {
            match self.group_index.get(&ser) {
                Some(&gi) => {
                    for (acc, part) in self.groups[gi].1.iter_mut().zip(accs) {
                        if !acc.merge_in_order(part)? {
                            return Ok(false);
                        }
                    }
                }
                None => {
                    self.groups.push((key, accs));
                    self.group_index.insert(ser, self.groups.len() - 1);
                    budget_check(graph, self.groups.len())?;
                }
            }
        }
        Ok(true)
    }

    /// Fold one bound row (its locals already in `scope`).
    fn push(&mut self, graph: &Graph, scope: &Scope<'_>) -> Result<(), RunError> {
        let gi = self.group_of(graph, scope)?;
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

    /// Fold `n` rows alike in one push (fix 81): the key evaluated once, a
    /// star count advanced by `n`, any other site pushed `n` times. The
    /// seeded hop walk folds a seed's whole degree, or a far end's edge
    /// count, this way.
    fn push_n(&mut self, graph: &Graph, scope: &Scope<'_>, n: u64) -> Result<(), RunError> {
        if n == 0 {
            return Ok(());
        }
        let gi = self.group_of(graph, scope)?;
        let accs = &mut self.groups[gi].1;
        for ((site, arg), acc) in self.sites.iter().zip(accs.iter_mut()) {
            if site.star {
                match acc {
                    SiteAcc::CountStar(c) => *c += n as i64,
                    _ => {
                        for _ in 0..n {
                            acc.push(None)?;
                        }
                    }
                }
            } else {
                let a = arg.expect("non-star site has an argument");
                let v = eval_with(a, scope, None).map_err(RunError::Eval)?;
                for _ in 0..n {
                    acc.push(Some(v.clone()))?;
                }
            }
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
    /// `var.prop =~ / CONTAINS / STARTS WITH / ENDS WITH 'literal'` conjuncts,
    /// recorded RAW. The trigram condition is derived later, in the seek, and
    /// only once a declared index is known to exist — see
    /// `crate::interp::text_query_for`.
    texts: Vec<(String, engram_cypher::BinOp, String)>,
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
    let texts = crate::interp::prop_text_candidates(full_where.as_ref(), &var);
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
        texts,
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
/// Fix 94: the walk column a `RETURN DISTINCT n.p` (one item, no ORDER
/// BY) projects — `Some(column index)` — or `None` for any other shape.
/// `MATCH (n:UserDataNode {nodeType: 'email'}) WHERE n.userId IS NOT NULL
/// RETURN DISTINCT n.userId AS userId` bound, evaluated, boxed into a row
/// and canonically re-keyed every one of 18,373 survivors to keep two
/// values (12.4 ms against Neo4j's 6.9 on the mirror); the column holds
/// the values already, in the survivors' order.
fn distinct_column_item(plan: &ProjPlan, walk: &Walk) -> Option<usize> {
    if !plan.distinct || !plan.order.is_empty() || plan.items.len() != 1 {
        return None;
    }
    let ProjItemPlan::Expr(Expr::Var(local)) = &plan.items[0] else {
        return None;
    };
    if walk.columns.len() != plan.reads.props.len() {
        return None;
    }
    plan.reads
        .props
        .iter()
        .position(|p| local_for_prop(&plan.reads.tag, p) == *local)
}

/// Fix 126: every projected item is a plain COLUMN LOCAL, so the answer is
/// the walk's columns transposed and nothing else — no `Scope`, no per-member
/// `bind`, no expression evaluation per item.
///
/// `MATCH (p:Person) RETURN p.id AS id, p.firstName AS name LIMIT 5000` spent
/// its whole per-member loop clearing a scope, binding two locals into it, and
/// evaluating two `Expr::Var` lookups back out — five thousand times, to move
/// two values it already had in hand.
///
/// Declines on anything that needs the scope: a bare `n`, DISTINCT, an ORDER
/// BY whose keys are not already the projected columns, a type or id read, a
/// probe, a degree, a presence or label test, or an expression that is not a
/// bare column local. Also declines when two items name the SAME column,
/// because the emit TAKES each value out of the column exactly as `bind` does
/// and the second reader would see the `Null` left behind.
fn all_column_items(plan: &ProjPlan, walk: &Walk) -> Option<Vec<usize>> {
    if plan.distinct || !plan.order.is_empty() || plan.items.is_empty() {
        return None;
    }
    if plan.reads.type_read || plan.reads.id_read {
        return None;
    }
    if !walk.presence.is_empty() || !walk.label_members.is_empty() || !walk.degrees.is_empty() {
        return None;
    }
    if walk.columns.len() != plan.reads.props.len() {
        return None;
    }
    let mut cis: Vec<usize> = Vec::with_capacity(plan.items.len());
    for it in &plan.items {
        let ProjItemPlan::Expr(Expr::Var(local)) = it else {
            return None;
        };
        let ci = plan
            .reads
            .props
            .iter()
            .position(|p| local_for_prop(&plan.reads.tag, p) == *local)?;
        if cis.contains(&ci) {
            return None; // the same column twice: the second read would be Null
        }
        cis.push(ci);
    }
    Some(cis)
}

/// Fix 126's emit: one row per id, each column's value taken at its own
/// cursor, then the trailing id `project_row` appends. Byte-for-byte the rows
/// the per-member loop built.
fn emit_column_rows(
    walk: &mut Walk,
    cis: &[usize],
    ids: impl Iterator<Item = u64>,
    rows: &mut Vec<Vec<Value>>,
    keys: &mut Vec<Vec<Value>>,
) {
    for id in ids {
        let mut out = Vec::with_capacity(cis.len() + 1);
        for &ci in cis {
            let col = &mut walk.columns[ci];
            let cur = &mut walk.cursors[ci];
            while *cur < col.len() && col[*cur].0 < id {
                *cur += 1;
            }
            // TAKEN, not cloned — the same trade `Walk::bind` makes, and the
            // reason `all_column_items` refuses to serve one column twice.
            out.push(if *cur < col.len() && col[*cur].0 == id {
                std::mem::replace(&mut col[*cur].1, Value::Null)
            } else {
                Value::Null
            });
        }
        out.push(Value::Int(id as i64));
        rows.push(out);
        keys.push(Vec::new());
    }
}

/// Fix 94: the distinct values of the walk's column `ci` over its members,
/// first occurrence first, each as the row the per-member loop would have
/// built (the value, then the trailing id placeholder) with an empty order
/// key. A member with no entry reads Null; every Null is one value, as
/// DISTINCT has it; a string dedups by its text, anything else by the same
/// canonical key the row-wise dedup uses.
fn dedup_one_column(
    walk: &Walk,
    ci: usize,
    ids: impl Iterator<Item = u64>,
    rows: &mut Vec<Vec<Value>>,
    keys: &mut Vec<Vec<Value>>,
) {
    let col = &walk.columns[ci];
    let mut cur = walk.cursors[ci];
    let mut seen_str: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut seen_other: std::collections::BTreeSet<Vec<u8>> = std::collections::BTreeSet::new();
    let mut seen_null = false;
    let mut nonce = 0u64;
    for id in ids {
        while cur < col.len() && col[cur].0 < id {
            cur += 1;
        }
        let v = if cur < col.len() && col[cur].0 == id {
            &col[cur].1
        } else {
            &Value::Null
        };
        let fresh = match v {
            Value::Null => !std::mem::replace(&mut seen_null, true),
            Value::Str(s) => seen_str.insert(s.as_str()),
            other => seen_other.insert(agg_key_of(std::slice::from_ref(other), &mut nonce)),
        };
        if fresh {
            rows.push(vec![v.clone(), Value::Int(id as i64)]);
            keys.push(Vec::new());
        }
    }
}

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
                let cands = SeekCandidates {
                    seeks: &plan.seeks,
                    prefixes: &plan.prefixes,
                    ranges: &plan.ranges,
                    texts: &plan.texts,
                };
                if let Some(ids) = columnar_seek_ids(graph, labels, &cands, SeekUse::PerId, &scope)?
                {
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
                    // Fix 114: a bare LIMIT over a SOUGHT population visits
                    // the ids NEWEST FIRST — fix 82's rule for the label
                    // scan, which this path lacked. Ids are minted in
                    // creation order and the listings that cap without
                    // ordering are recency-filtered, so the k survivors sit
                    // at the END of id order: the production story pick
                    // (`s.primaryTopic = $t AND s.status <> 'stale' AND
                    // s.lastUpdatedAt > $cutoff … LIMIT 5`) read 1,324
                    // sought stories ascending to find five recent ones
                    // (9 ms on the mirror, Neo4j 1.1). The k found come
                    // back in id order, as the label scan's do.
                    let newest_first = early_cap.is_some() && ids.len() > 1;
                    if newest_first {
                        counted!(
                            "interp.columnar projection sought ids visited newest first for the limit"
                        );
                    }
                    let order: Vec<u64> = if newest_first {
                        ids.into_iter().rev().collect()
                    } else {
                        ids
                    };
                    for id in order {
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
                            counted!("interp.columnar projection stopped at the limit");
                            break;
                        }
                    }
                    if newest_first {
                        sort_rows_by_trailing_id(&mut rows, &mut keys);
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
        let phase1_reads = if two_phase {
            &plan.pred_reads
        } else {
            &plan.reads
        };
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
                // Fix 112: when the label's columns are NOT yet kept and the
                // whole label would fit the property-column cache, the walk
                // is NOT cut: it reads the label whole ONCE — the walk keeps
                // only whole-label columns — and stops at the limit in id
                // order, so this execution answers the first `cap` members
                // exactly as the cut walk did and every later one reads the
                // cache. The production `MATCH (a:NewsArticle) RETURN
                // a.articleId AS id, a.title AS title LIMIT 5000` gathered
                // 5,000 fat records on every execution (54 ms on the mirror,
                // Neo4j 18.5) because a cut walk kept nothing. A label past
                // the budget, or a read of anything but plain properties,
                // keeps the cut.
                // A SMALL limit keeps the cut (fix 52's own case: `LIMIT 3`
                // must not read a whole label for three rows), and so does a
                // label wide enough that its one whole walk would stall the
                // first caller for seconds.
                const WIDEN_MIN_LIMIT: usize = 256;
                const WIDEN_MAX_LABEL: usize = 262_144;
                let mut widened = false;
                let capped: Option<std::sync::Arc<Vec<u64>>> = match (
                    early_cap,
                    &plan.pred,
                    &plan.source,
                ) {
                    (Some(cap), None, Source::Nodes { labels, any_of })
                        if labels.len() == 1 && any_of.is_empty() =>
                    {
                        let members = graph.members_all(labels).map_err(RunError::Graph)?;
                        let keep_whole = !plan.reads.props.is_empty()
                            && plan.reads.probes.is_empty()
                            && plan.reads.degrees.is_empty()
                            && plan.reads.labels.is_empty()
                            && !plan.reads.type_read
                            && cap >= WIDEN_MIN_LIMIT
                            && members.len() > cap
                            && members.len() <= WIDEN_MAX_LABEL
                            && members.len().saturating_mul(64) <= graph.prop_column_budget()
                            && !graph.prop_columns_current(
                                &labels[0],
                                &plan.reads.props,
                                &plan.reads.presence_only(),
                            );
                        if keep_whole {
                            counted!(
                                "interp.columnar projection walked its label whole to keep the columns for its limit"
                            );
                            widened = true;
                            None
                        } else {
                            let ids: Vec<u64> = members.iter().take(cap).collect();
                            counted!("interp.columnar projection walk cut at the plain limit");
                            Some(std::sync::Arc::new(ids))
                        }
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
                let vector_hits: Option<Vec<usize>> = match (two_phase, vector_label, &plan.pred) {
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
                    if let Some(ci) = distinct_column_item(&plan, &pwalk) {
                        // Fix 94: `RETURN DISTINCT n.p` dedups the column over
                        // the survivors (no bare item, so no late row).
                        dedup_one_column(
                            &pwalk,
                            ci,
                            hits.iter().map(|&mi| pwalk.members[mi]),
                            &mut rows,
                            &mut keys,
                        );
                        counted!("interp.columnar projection deduplicated its one column");
                        budget_check(graph, rows.len())?;
                    } else {
                        for mi in hits {
                            let id = pwalk.members[mi];
                            scope.locals.clear();
                            pwalk.bind(graph, &plan.reads, &mut scope, mi, id)?;
                            single_phase_rows.push((id, mi));
                            project_row(&plan, &mut scope, id, &mut rows, &mut keys)?;
                            budget_check(graph, rows.len())?;
                        }
                    }
                    if early_cap.is_some_and(|c| rows.len() >= c) {
                        counted!("interp.columnar projection stopped at the limit");
                    }
                } else {
                    // The per-member walk visits the chunks in the same
                    // order as the column-at-a-time scan (fix 82), so a
                    // capped statement answers the same k rows cold and warm.
                    let n = pwalk.members.len();
                    // Fix 112: a walk widened to keep its columns visits its
                    // chunks FORWARD and stops at the limit — the first `cap`
                    // members in id order, the cut walk's own rows.
                    let has_pred = plan.pred.is_some();
                    let both_ends = !widened && scan_from_both_ends(n, early_cap, has_pred);
                    let order: Vec<usize> = if widened {
                        (0..n.div_ceil(PRED_CHUNK)).collect()
                    } else {
                        scan_chunk_order(n, early_cap, has_pred)
                    };
                    // Fix 126: a capped, predicate-less, single-phase, FORWARD
                    // walk whose every item is a bare column local is just its
                    // columns transposed — take the first `cap` members in id
                    // order and read one value per column. This is the branch
                    // `plat-limit-listing` takes; the plain-walk site below is
                    // the uncapped twin.
                    //
                    // Guarded on all four, each load-bearing: a predicate needs
                    // the scope to evaluate against, a two-phase plan defers its
                    // items to phase 2, a both-ends order visits chunks out of id
                    // order (so the cursors would have to be re-seeked per
                    // chunk), and a bare item is already refused by the
                    // recogniser.
                    let mut emitted_by_columns = false;
                    if !two_phase && plan.pred.is_none() && !both_ends {
                        if let Some(cis) = all_column_items(&plan, &pwalk) {
                            let take = early_cap.unwrap_or(n).min(n);
                            let members = std::sync::Arc::clone(&pwalk.members);
                            emit_column_rows(
                                &mut pwalk,
                                &cis,
                                members[..take].iter().copied(),
                                &mut rows,
                                &mut keys,
                            );
                            counted!(
                                "interp.columnar projection emitted its rows from the columns"
                            );
                            if early_cap.is_some_and(|c| rows.len() >= c) {
                                counted!("interp.columnar projection stopped at the limit");
                            }
                            budget_check(graph, rows.len())?;
                            emitted_by_columns = true;
                        }
                    }
                    let mut done = emitted_by_columns;
                    for c in order {
                        if done {
                            break;
                        }
                        // Fix 109: a chunk visited out of id order places
                        // the bind cursors at its first member first.
                        if both_ends {
                            pwalk.seek_cursors(pwalk.members[c * PRED_CHUNK]);
                        }
                        for mi in c * PRED_CHUNK..n.min((c + 1) * PRED_CHUNK) {
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
                                    done = true;
                                    break;
                                }
                                continue;
                            }
                            single_phase_rows.push((id, mi));
                            project_row(&plan, &mut scope, id, &mut rows, &mut keys)?;
                            budget_check(graph, rows.len())?;
                            if early_cap.is_some_and(|c| rows.len() >= c) {
                                counted!("interp.columnar projection stopped at the limit");
                                done = true;
                                break;
                            }
                        }
                        if done {
                            break;
                        }
                    }
                    if both_ends {
                        survivors.sort_unstable();
                        sort_rows_by_trailing_id(&mut rows, &mut keys);
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
            const PER_SURVIVOR_GET_ENTRIES: usize = 8; // x the column budget factor (8)
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
                    if let Some(ci) = distinct_column_item(&plan, &walk) {
                        // Fix 94: `RETURN DISTINCT n.p` dedups the column.
                        dedup_one_column(
                            &walk,
                            ci,
                            walk.members.iter().copied(),
                            &mut rows,
                            &mut keys,
                        );
                        counted!("interp.columnar projection deduplicated its one column");
                        budget_check(graph, rows.len())?;
                    } else if let Some(cis) = all_column_items(&plan, &walk) {
                        // Fix 126: the rows ARE the columns, transposed.
                        let members = std::sync::Arc::clone(&walk.members);
                        emit_column_rows(
                            &mut walk,
                            &cis,
                            members.iter().copied(),
                            &mut rows,
                            &mut keys,
                        );
                        counted!("interp.columnar projection emitted its rows from the columns");
                        budget_check(graph, rows.len())?;
                    } else {
                        for mi in 0..walk.members.len() {
                            let id = walk.members[mi];
                            scope.locals.clear();
                            walk.bind(graph, &plan.reads, &mut scope, mi, id)?;
                            project_row(&plan, &mut scope, id, &mut rows, &mut keys)?;
                            budget_check(graph, rows.len())?;
                        }
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
            let texts = crate::interp::prop_text_candidates(Some(pred), var);
            let cands = SeekCandidates {
                seeks: &seeks,
                prefixes: &prefixes,
                ranges: &ranges,
                texts: &texts,
            };
            match columnar_seek_ids(graph, labels, &cands, SeekUse::Walk, &scope)? {
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
        let members = graph
            .members_all(labels)
            .map_err(RunError::Graph)?
            .to_arc_vec();
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
        /// An item with an aggregate NESTED in it (`sum(m.length) /
        /// toFloat(count(m))`): the fold's own columns — keys, top-level
        /// aggregates and each lifted aggregate under a hidden name — and
        /// the projection over them that yields the breaker's columns,
        /// ordered and paged there (see `split_nested_aggregates`).
        fin: Option<Box<(Vec<String>, Final)>>,
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
    /// What the predicate ALONE reads. `reads` also holds the chain's and
    /// the breaker's, and a label an item tests (`m:Comment AS isComment`)
    /// kept the predicate from being judged column-at-a-time, although the
    /// predicate itself read no label.
    pred_reads: Reads,
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
    // The names the stage's ONE input row carries in (`try_columnar_stage`
    // binds them as the walk's variables): readable wherever an alias is.
    carried: &[String],
) -> Option<StagePlan> {
    let [m @ Clause::Match { .. }, withs @ ..] = prefix else {
        return None;
    };
    let (var, kind, source, full_where) = recognise_source(m)?;
    // A carried name the pattern re-uses is a BOUND node, not a scan of its
    // label: the general path matches it.
    if carried.contains(&var) {
        return None;
    }
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
    // The predicate is rewritten first into `reads` as well, so both name
    // every local alike.
    let mut pred_reads = Reads::default();
    if let Some(w) = &full_where {
        rewrite(w, &var, kind, &mut pred_reads)?;
    }
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
    let mut aliases: Vec<String> = carried.to_vec();
    let mut chain = Vec::with_capacity(withs.len());
    let mut var_in_scope = true;
    for c in withs {
        if let Clause::Unwind { expr, alias } = c {
            let mut allowed: Vec<String> = aliases.clone();
            if var_in_scope {
                allowed.push(var.clone());
            }
            // Re-declaring a name in scope (an alias, a carried name) is the
            // general path's to refuse.
            if !reads_only(expr, &allowed) || *alias == var || aliases.contains(alias) {
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
    // A bare carry under ANOTHER name (`WITH n AS m ORDER BY m.x`) declines:
    // the ORDER BY reads the alias, and `rewrite` maps reads of the scanned
    // variable only, so `m.x` read the null placeholder and the top-k sorted
    // nulls — `MATCH (m:Message) WITH m AS mm ORDER BY mm.n DESC LIMIT 3
    // RETURN mm.sq` hydrated the first three members by id instead of the
    // three largest (found 2026-09-24). The general path answers it.
    if bare
        .iter()
        .any(|&i| proj.items[i].alias.as_deref().is_some_and(|a| a != var))
    {
        return None;
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
        // Fix 86: so is `count(DISTINCT v)` — a member is one row here.
        let proj = &star_distinct_counts(proj, &var);
        match split_nested_aggregates(proj)? {
            None => {
                let (items, columns) = aggregating_items(proj, &var, kind, &mut reads)?;
                let order = order_over(proj, &columns)?;
                (
                    columns,
                    Breaker::Fold {
                        items,
                        order,
                        fin: None,
                    },
                )
            }
            Some((inner, fin)) => {
                // The lifted aggregates too: `count(DISTINCT v)` nested in an
                // item counts members as a top-level one does.
                let inner = star_distinct_counts(&inner, &var);
                let (items, inner_columns) = aggregating_items(&inner, &var, kind, &mut reads)?;
                counted!("interp.columnar stage lifted an aggregate nested in a breaker item");
                (
                    fin.columns.clone(),
                    Breaker::Fold {
                        items,
                        order: Vec::new(),
                        fin: Some(Box::new((inner_columns, fin))),
                    },
                )
            }
        }
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
        pred_reads,
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

/// The hidden fold column an aggregate lifted out of a breaker item folds
/// into — NUL-prefixed, so no name a statement binds can collide with it.
fn hidden_aggregate(i: usize) -> String {
    format!("\u{0}agg{i}")
}

/// `e` with every aggregate call in it replaced by its hidden fold column
/// (`hidden_aggregate`), each distinct call lifted into `lifted` once —
/// `None` for an expression shape this walk does not cover, or an
/// aggregate over an aggregate (which the general path refuses by name).
fn lift_aggregates(e: &Expr, lifted: &mut Vec<Expr>) -> Option<Expr> {
    if let Expr::Call {
        name, args, star, ..
    } = e
    {
        if *star || is_aggregate_fn(name) {
            if args.iter().any(contains_aggregate_call) {
                return None;
            }
            let i = match lifted.iter().position(|x| x == e) {
                Some(i) => i,
                None => {
                    lifted.push(e.clone());
                    lifted.len() - 1
                }
            };
            return Some(Expr::Var(hidden_aggregate(i)));
        }
    }
    let l = |x: &Expr, lifted: &mut Vec<Expr>| lift_aggregates(x, lifted).map(Box::new);
    Some(match e {
        Expr::Var(_)
        | Expr::Param(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null => e.clone(),
        Expr::Prop(b, k) => Expr::Prop(l(b, lifted)?, k.clone()),
        Expr::Not(a) => Expr::Not(l(a, lifted)?),
        Expr::Neg(a) => Expr::Neg(l(a, lifted)?),
        Expr::And(a, b) => Expr::And(l(a, lifted)?, l(b, lifted)?),
        Expr::Or(a, b) => Expr::Or(l(a, lifted)?, l(b, lifted)?),
        Expr::Xor(a, b) => Expr::Xor(l(a, lifted)?, l(b, lifted)?),
        Expr::Bin(op, a, b) => Expr::Bin(*op, l(a, lifted)?, l(b, lifted)?),
        Expr::In(a, b) => Expr::In(l(a, lifted)?, l(b, lifted)?),
        Expr::Index(a, b) => Expr::Index(l(a, lifted)?, l(b, lifted)?),
        Expr::IsNull { of, negated } => Expr::IsNull {
            of: l(of, lifted)?,
            negated: *negated,
        },
        Expr::List(items) => Expr::List(
            items
                .iter()
                .map(|x| lift_aggregates(x, lifted))
                .collect::<Option<_>>()?,
        ),
        Expr::Map(pairs) => Expr::Map(
            pairs
                .iter()
                .map(|(k, v)| Some((k.clone(), lift_aggregates(v, lifted)?)))
                .collect::<Option<_>>()?,
        ),
        Expr::Case {
            subject,
            arms,
            otherwise,
        } => Expr::Case {
            subject: match subject {
                Some(s) => Some(l(s, lifted)?),
                None => None,
            },
            arms: arms
                .iter()
                .map(|(w, t)| Some((lift_aggregates(w, lifted)?, lift_aggregates(t, lifted)?)))
                .collect::<Option<_>>()?,
            otherwise: match otherwise {
                Some(o) => Some(l(o, lifted)?),
                None => None,
            },
        },
        Expr::Call {
            name,
            distinct,
            args,
            star,
        } => Expr::Call {
            name: name.clone(),
            distinct: *distinct,
            args: args
                .iter()
                .map(|x| lift_aggregates(x, lifted))
                .collect::<Option<_>>()?,
            star: *star,
        },
        _ => return None,
    })
}

/// An aggregating breaker whose items NEST an aggregate in an expression —
/// SNB BI bi1's `sum(message.length) / toFloat(count(message)) AS
/// averageMessageLength` — split into the fold the stage runs and the
/// projection over it that yields the breaker's own columns: the fold keeps
/// each key and top-level aggregate under its column name and folds each
/// nested aggregate into a hidden column; the projection computes every
/// item from those and carries the breaker's ORDER BY / SKIP / LIMIT.
/// `Some(None)` when no item nests one — the fold IS the breaker, as
/// before. `None` declines: a shape the lift does not cover, or a projected
/// expression or ORDER BY reading anything but the fold's columns (a group
/// key re-spelt rather than named, a variable, a subquery).
///
/// Until this held, one such item sent bi1's whole stage to the general
/// path, which decoded each of its 2.1M messages in full at SF3 to fold
/// three properties of them.
fn split_nested_aggregates(proj: &Projection) -> Option<Option<(Projection, Final)>> {
    let top_level =
        |e: &Expr| matches!(e, Expr::Call { name, star, .. } if *star || is_aggregate_fn(name));
    if !proj
        .items
        .iter()
        .any(|it| !top_level(&it.expr) && contains_aggregate_call(&it.expr))
    {
        return Some(None);
    }
    if proj.star || proj.distinct {
        return None;
    }
    let columns: Vec<String> = proj
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
    let mut inner: Vec<ProjItem> = Vec::new();
    let mut fin_items: Vec<Expr> = Vec::with_capacity(proj.items.len());
    let mut lifted: Vec<Expr> = Vec::new();
    for (it, col) in proj.items.iter().zip(&columns) {
        if top_level(&it.expr) || !contains_aggregate_call(&it.expr) {
            // A key or a top-level aggregate: folded under its own name.
            inner.push(ProjItem::synthetic(it.expr.clone(), Some(col.clone())));
            fin_items.push(Expr::Var(col.clone()));
        } else {
            fin_items.push(lift_aggregates(&it.expr, &mut lifted)?);
        }
    }
    let mut order = Vec::with_capacity(proj.order.len());
    for o in &proj.order {
        order.push(OrderItem {
            expr: lift_aggregates(&o.expr, &mut lifted)?,
            desc: o.desc,
        });
    }
    let fold_columns: Vec<String> = inner
        .iter()
        .filter_map(|it| it.alias.clone())
        .chain((0..lifted.len()).map(hidden_aggregate))
        .collect();
    for (i, agg) in lifted.into_iter().enumerate() {
        inner.push(ProjItem::synthetic(agg, Some(hidden_aggregate(i))));
    }
    // The projection is evaluated over the fold's columns alone (and, for
    // ORDER BY, its own), hook-less.
    let mut allowed = fold_columns;
    allowed.extend(columns.iter().cloned());
    if fin_items
        .iter()
        .chain(order.iter().map(|o| &o.expr))
        .any(|e| !reads_only(e, &allowed) || contains_opaque(e))
    {
        return None;
    }
    Some(Some((
        Projection {
            distinct: false,
            star: false,
            items: inner,
            order: Vec::new(),
            skip: None,
            limit: None,
        },
        Final {
            items: fin_items,
            columns,
            order,
            skip: proj.skip.clone(),
            limit: proj.limit.clone(),
        },
    )))
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
                    for it in (items).iter().cloned() {
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
    if input.len() != 1 {
        return Ok(None); // ONE input row: a stage head, or a statement's constants
    }
    // ONE input row carrying values — a total an earlier stage counted, as
    // in SNB BI bi1's `WITH count(message) AS totalMessageCountInt WITH
    // toFloat(totalMessageCountInt) AS totalMessageCount MATCH (message:
    // Message) …` — is the walk's variables, and the plain WITHs leading the
    // prefix are evaluated over it ONCE. Until this held, a stage carrying
    // anything in went to the general path whole: bi1 decoded each of its
    // 2.1M messages in full at SF3 to fold three properties of them.
    let lead = prefix
        .iter()
        .take_while(|c| matches!(c, Clause::With { .. }))
        .count();
    let mut carried_names: Vec<String> = input[0].keys().cloned().collect();
    for c in &prefix[..lead] {
        let Clause::With { proj, where_ } = c else {
            unreachable!("counted as a WITH above")
        };
        // A filter (a row the WITH may drop), a star, or anything a breaker
        // would carry is the general path's.
        if where_.is_some()
            || proj.star
            || proj.distinct
            || !proj.order.is_empty()
            || proj.skip.is_some()
            || proj.limit.is_some()
            || proj.items.iter().any(|it| contains_aggregate_call(&it.expr))
        {
            return Ok(None);
        }
        carried_names = proj
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
    }
    let Some(plan) = recognise_stage(&prefix[lead..], breaker, rest_after, &carried_names) else {
        return Ok(None);
    };
    let mut carried: Row = input[0].clone();
    for c in &prefix[..lead] {
        let Clause::With { proj, .. } = c else {
            unreachable!("counted as a WITH above")
        };
        let mut next = Row::new();
        for (i, it) in proj.items.iter().enumerate() {
            let name = it
                .alias
                .clone()
                .or_else(|| it.text.clone())
                .unwrap_or_else(|| column_name(&it.expr, i));
            let v = crate::interp::eval_expr(graph, &it.expr, &carried, params)?;
            next.insert(name, v);
        }
        carried = next;
    }
    if !carried.is_empty() {
        counted!("interp.columnar stage carried its input row in as variables");
    }
    // The stage's other half of the scope rule: the prefix after the leading
    // WITHs starts at a MATCH that `recognise_stage` walks, so a name it did
    // not carry is not there to read — `reads_only` refused it.
    let prefix = &prefix[lead..];
    // Fix 57's graph-dependent half: a bare carry rides the stage only
    // where its start is NOT selectively seekable. The general path's seed
    // — the same candidates, the same probe — reads a sought minority alone
    // and binds it lean from the columns, while this stage would walk the
    // whole label's columns to page it; so a seek that answers less than
    // half the label (within the seek's cap) keeps the carry there. The
    // inbox page of a user who owns 38k of the 38.6k emails is the stage's;
    // a 500-email user's is the seek's.
    if !plan.bare.is_empty() {
        if let [
            Clause::Match {
                pattern, where_, ..
            },
            ..,
        ] = prefix
        {
            if let (Source::Nodes { labels, .. }, [path]) = (&plan.source, pattern.paths.as_slice())
            {
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
    // SNB BI bi1's fold on the workers (`parallel_stage_fold`); else the one
    // walk on this thread.
    let on_workers = match &plan.breaker {
        Breaker::Fold { items, order, fin } => {
            parallel_stage_fold(graph, &plan, items, order, fin, &carried, params)?
        }
        Breaker::Project { .. } => None,
    };
    let rows: Vec<Vec<Value>> = match on_workers {
        Some(rows) => rows,
        None => match serial_stage_rows(graph, &plan, &carried, params)? {
            Some(rows) => rows,
            None => return Ok(None),
        },
    };
    let mut scope = Scope::over(params, &carried, graph.wall_ms(), graph.zone_provider());
    let truth_of = stage_truth;
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
        //
        // A node the statement's RETURN outputs is output WHOLE: there is no
        // continuation to read it, and `carry_demand` over none answered the
        // empty set — a node carrying its labels and no property.
        let mut projected: Option<std::collections::BTreeSet<String>> =
            (!matches!(breaker, Clause::Return { .. })).then(Default::default);
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
                    counted!(
                        "interp.columnar stage hydrated a survivor projected to its continuation"
                    );
                    graph
                        .node_projected(id as u64, set)
                        .map_err(RunError::Graph)?
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

/// A WHERE's answer as a keep/drop: `true` for True, `false` for False or
/// null, and the error the general path raises for anything else.
fn stage_truth(v: Value) -> Result<bool, RunError> {
    match v.truth() {
        Some(Truth::True) => Ok(true),
        Some(_) => Ok(false),
        None => Err(RunError::Semantic(format!(
            "WHERE takes a boolean, got {}",
            v.type_name()
        ))),
    }
}

/// An aggregating breaker's fold spec. A split breaker
/// (`split_nested_aggregates`): the fold's own columns, unordered and
/// unpaged, and the projection over them orders and pages the breaker's.
fn stage_fold_spec<'s>(
    plan: &'s StagePlan,
    items: &'s [Item],
    order: &'s [(usize, bool)],
    fin: &'s Option<Box<(Vec<String>, Final)>>,
) -> FoldSpec<'s> {
    match fin {
        None => FoldSpec {
            items,
            columns: &plan.columns,
            order,
            skip: plan.skip.as_ref(),
            limit: plan.limit.as_ref(),
            final_: None,
        },
        Some(split) => FoldSpec {
            items,
            columns: &split.0,
            order: &[],
            skip: None,
            limit: None,
            final_: Some(&split.1),
        },
    }
}

/// The stage's rows before the post-WHERE, from ONE walk on this thread:
/// the label's columns (assembled and kept, when not yet cached), the
/// predicate column-at-a-time where it can be, and each survivor bound and
/// carried through the chain into the breaker. `None` when the walk
/// declines (the general path answers).
fn serial_stage_rows(
    graph: &Graph,
    plan: &StagePlan,
    carried: &Row,
    params: &BTreeMap<String, Value>,
) -> Result<Option<Vec<Vec<Value>>>, RunError> {
    let Some(mut walk) = load_walk(graph, &plan.source, &plan.reads, params)? else {
        return Ok(None);
    };
    counted!("interp.statements run");
    counted!("interp.columnar stages");
    sometimes!("interp.columnar stage produced a WITH chain", true);
    note_walk_events(&plan.source, &plan.reads, &walk);
    let mut scope = Scope::over(params, carried, graph.wall_ms(), graph.zone_provider());
    let truth_of = stage_truth;
    // Fix 84: a ONE-label stage whose predicate reads only value / presence
    // columns is judged COLUMN-AT-A-TIME over the walk's own members
    // (`survivors_over_cached_columns`, the projection scan's evaluator
    // since fix 40 and the seed filter's since fix 67): no scope bound and
    // no expression walked for a member the predicate drops. The inbox page
    // (`MATCH (n:UserDataNode {nodeType: 'email', userId: $userId}) WHERE
    // n.classified = true AND (n.abuseStatus IS NULL OR …) WITH n ORDER BY
    // n.createdAt DESC SKIP … LIMIT 1000 …`) bound and evaluated every one
    // of the user's 18k emails per page — 92k expressions, 154 ms on the
    // mirror against Neo4j's 107. A column not yet cached (the walk that
    // assembles it keeps it for the next statement), a predicate the
    // vectoriser declines, or a non-boolean answer keeps the per-member
    // walk, which raises for the row.
    let vector_hits: Option<Vec<usize>> = match (&plan.source, &plan.pred) {
        (Source::Nodes { labels, any_of }, Some(pred))
            if labels.len() == 1
                && any_of.is_empty()
                && plan.pred_reads.labels.is_empty()
                && plan.pred_reads.probes.is_empty()
                && plan.pred_reads.degrees.is_empty()
                && !plan.pred_reads.type_read =>
        {
            survivors_over_cached_columns(
                graph,
                &labels[0],
                pred,
                &plan.pred_reads,
                &walk.members,
                None,
                &scope,
            )
        }
        _ => None,
    };
    if vector_hits.is_some() {
        counted!("interp.columnar stage predicate evaluated column-at-a-time");
    }
    let positions: Vec<usize> = match &vector_hits {
        Some(hits) => hits.clone(),
        None => (0..walk.members.len()).collect(),
    };
    let pred_per_row: Option<&Expr> = if vector_hits.is_some() {
        None
    } else {
        plan.pred.as_ref()
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
            for &mi in &positions {
                let id = walk.members[mi];
                scope.locals.clear();
                walk.bind(graph, &plan.reads, &mut scope, mi, id)?;
                if let Some(pred) = pred_per_row {
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
        Breaker::Fold { items, order, fin } => {
            sometimes!("interp.columnar stage folded an aggregating breaker", true);
            let mut fold = Fold::new(items);
            for &mi in &positions {
                let id = walk.members[mi];
                scope.locals.clear();
                walk.bind(graph, &plan.reads, &mut scope, mi, id)?;
                if let Some(pred) = pred_per_row {
                    let v = eval_with(pred, &scope, None).map_err(RunError::Eval)?;
                    if !truth_of(v)? {
                        continue;
                    }
                }
                walk_chain(&plan.chain, &mut scope, &mut |sc| fold.push(graph, sc))?;
            }
            let spec = stage_fold_spec(plan, items, order, fin);
            fold.finish(graph, params, &spec, &mut scope)?.rows
        }
    };
    Ok(Some(rows))
}

/// SNB BI bi1's stage — `MATCH (message:Message) WHERE message.creationDate <
/// $datetime AND message.content IS NOT NULL WITH … count(message), sum(
/// message.length) …` — folded on the WORKERS. At SF3 its survivors are
/// millions of the 9M messages, each bound from the cached columns, carried
/// through the chain and pushed into ONE fold on one thread: 4.0 s against
/// Neo4j's 2.8.
///
/// The predicate is judged column-at-a-time over the whole label, exactly as
/// the serial walk judges it (the cache's aligned columns are aligned to the
/// label's whole membership and nothing else — `Graph::prop_column_aligned`).
/// The survivors are then cut into one contiguous share per worker; each
/// worker walks its share from the cached columns (`load_walk_over` takes
/// only the share's entries) into its own partial fold, and the partials
/// merge in share order ([`Fold::merge_later`]). Every group is then first
/// seen where the serial fold first sees it, and every accumulator merges to
/// the value the serial fold computes: a sum's or an average's float addends
/// are kept in arrival order and added in the serial order at the finish. Or
/// the merge says it cannot (an integer total the serial order takes past
/// i64), and the stage folds on this thread instead. So does any share that fails
/// to fold, whatever the reason: the serial fold raises the error in its own
/// order, and a share cannot know what the rows before it would have done.
///
/// `None` declines: one worker; a transaction; a scanned node carried bare;
/// a source other than one label; a probe, degree or `type()` read (answered
/// per population at load); a DISTINCT (no exact merge here); a column
/// not yet cached (the serial walk assembles and keeps it); too few
/// survivors to share; or a commit landing while the shares fold.
fn parallel_stage_fold<'p>(
    graph: &Graph,
    plan: &'p StagePlan,
    items: &'p [Item],
    order: &'p [(usize, bool)],
    fin: &'p Option<Box<(Vec<String>, Final)>>,
    carried: &Row,
    params: &BTreeMap<String, Value>,
) -> Result<Option<Vec<Vec<Value>>>, RunError> {
    let Some(exec) = graph.exec().filter(|e| e.width() > 1) else {
        return Ok(None);
    };
    if graph.in_txn() || !plan.bare.is_empty() {
        return Ok(None);
    }
    let Source::Nodes { labels, any_of } = &plan.source else {
        return Ok(None);
    };
    let [label] = labels.as_slice() else {
        return Ok(None);
    };
    if !any_of.is_empty()
        || !plan.reads.probes.is_empty()
        || !plan.reads.degrees.is_empty()
        || plan.reads.type_read
        || !items.iter().all(|it| match it {
            Item::Agg(site, _) => SiteAcc::merges_in_order(site),
            Item::Key(_) => true,
        })
        || !graph.prop_columns_current(label, &plan.reads.props, &plan.reads.presence_only())
    {
        return Ok(None);
    }
    let stamp = graph.column_stamp();
    let members = graph
        .members_all(labels)
        .map_err(RunError::Graph)?
        .to_arc_vec();
    let scope = Scope::over(params, carried, graph.wall_ms(), graph.zone_provider());
    let hits = match &plan.pred {
        Some(pred)
            if plan.pred_reads.labels.is_empty()
                && plan.pred_reads.probes.is_empty()
                && plan.pred_reads.degrees.is_empty()
                && !plan.pred_reads.type_read =>
        {
            survivors_over_cached_columns(
                graph,
                label,
                pred,
                &plan.pred_reads,
                &members,
                None,
                &scope,
            )
        }
        _ => None,
    };
    let pred_per_row: Option<&Expr> = if hits.is_some() {
        None
    } else {
        plan.pred.as_ref()
    };
    let survivors: Vec<u64>;
    let population: &[u64] = match &hits {
        Some(hits) => {
            survivors = hits.iter().map(|&mi| members[mi]).collect();
            &survivors
        }
        None => &members,
    };
    let width = exec.width();
    if population.len() < graph.parallel_min_rows().saturating_mul(width) {
        return Ok(None);
    }
    let per = population.len().div_ceil(width).max(1);
    let shares: Vec<&[u64]> = population.chunks(per).collect();
    type Share<'f> = std::sync::Mutex<Option<Result<Option<Fold<'f>>, RunError>>>;
    let slots: Vec<Share<'p>> = shares.iter().map(|_| std::sync::Mutex::new(None)).collect();
    exec.for_each(shares.len(), &|k| {
        let run = || -> Result<Option<Fold<'p>>, RunError> {
            let over = std::sync::Arc::new(shares[k].to_vec());
            let Some(mut walk) =
                load_walk_over(graph, &plan.source, &plan.reads, Some(over), params)?
            else {
                return Ok(None);
            };
            // the label memberships' cursors start at the share, not at 0
            if let Some(&first) = walk.members.first() {
                walk.seek_cursors(first);
            }
            let mut scope = Scope::over(params, carried, graph.wall_ms(), graph.zone_provider());
            let mut fold = Fold::new(items);
            fold.partial = true;
            // NaN keys never equal: each partial numbers its own apart
            fold.nonce = (k as u64 + 1) << 40;
            for mi in 0..walk.members.len() {
                let id = walk.members[mi];
                scope.locals.clear();
                walk.bind(graph, &plan.reads, &mut scope, mi, id)?;
                if let Some(pred) = pred_per_row {
                    let v = eval_with(pred, &scope, None).map_err(RunError::Eval)?;
                    if !stage_truth(v)? {
                        continue;
                    }
                }
                walk_chain(&plan.chain, &mut scope, &mut |sc| fold.push(graph, sc))?;
            }
            Ok(Some(fold))
        };
        let out = run();
        *slots[k].lock().unwrap_or_else(|e| e.into_inner()) = Some(out);
    });
    let mut merged: Option<Fold<'p>> = None;
    for slot in slots {
        let Some(Ok(Some(part))) = slot.into_inner().unwrap_or_else(|e| e.into_inner()) else {
            counted!("interp.columnar stage fold on the workers declined: a share did not fold");
            return Ok(None);
        };
        match merged.as_mut() {
            None => merged = Some(part),
            Some(m) => {
                if !m.merge_later(graph, part)? {
                    counted!(
                        "interp.columnar stage fold on the workers declined: a sum the serial order would not reproduce"
                    );
                    return Ok(None);
                }
            }
        }
    }
    if graph.column_stamp() != stamp {
        counted!("interp.columnar stage fold on the workers declined: a commit landed while it ran");
        return Ok(None);
    }
    let Some(fold) = merged else {
        return Ok(None);
    };
    counted!("interp.statements run");
    counted!("interp.columnar stages");
    sometimes!("interp.columnar stage produced a WITH chain", true);
    sometimes!("interp.columnar stage folded an aggregating breaker", true);
    counted!("interp.columnar stage folded its survivors on the workers");
    if hits.is_some() {
        counted!("interp.columnar stage predicate evaluated column-at-a-time");
    }
    let mut scope = Scope::over(params, carried, graph.wall_ms(), graph.zone_provider());
    let spec = stage_fold_spec(plan, items, order, fin);
    Ok(Some(fold.finish(graph, params, &spec, &mut scope)?.rows))
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
    if next.items.iter().any(|it| contains_opaque(&it.expr))
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
    /// Fix 81: the WHERE's conjuncts that read the START alone (its inline
    /// map's equalities, `a.k IS NULL OR a.k IN […]`) — the seeded walk
    /// evaluates them ONCE per seed, before it reads the seed's adjacency.
    a_pred: Option<Expr>,
    /// The rest of the WHERE, per row.
    pred: Option<Expr>,
    /// Whether the items and `pred` read the FAR end alone (or nothing):
    /// the seeded walk then folds once per distinct far end, weighted by
    /// its edge count, instead of once per edge.
    far_only: bool,
    items: Vec<Item>,
    columns: Vec<String>,
    order: Vec<(usize, bool)>,
    skip: Option<Expr>,
    limit: Option<Expr>,
    final_: Option<Final>,
}

/// The conjuncts AND-ed back together, in order — `None` for none.
fn and_all(cs: Vec<Expr>) -> Option<Expr> {
    cs.into_iter()
        .reduce(|acc, c| Expr::And(Box::new(acc), Box::new(c)))
}

/// `rewrite` over an optional expression: `Some(None)` for none, `None`
/// when the rewrite declines.
fn rewrite_opt(e: Option<&Expr>, var: &str, kind: Kind, reads: &mut Reads) -> Option<Option<Expr>> {
    match e {
        None => Some(None),
        Some(x) => rewrite(x, var, kind, reads).map(Some),
    }
}

/// Whether the row bound in `scope` passes an optional predicate: absent
/// or True passes, False and Null drop, a non-boolean is the WHERE error.
fn row_passes(pred: &Option<Expr>, scope: &Scope<'_>) -> Result<bool, RunError> {
    let Some(p) = pred else {
        return Ok(true);
    };
    let v = eval_with(p, scope, None).map_err(RunError::Eval)?;
    match v.truth() {
        Some(Truth::True) => Ok(true),
        Some(_) => Ok(false),
        None => Err(RunError::Semantic(format!(
            "WHERE takes a boolean, got {}",
            v.type_name()
        ))),
    }
}

/// Fix 86: `count(DISTINCT v)` over the ONE variable a single-source scan
/// binds is `count(*)`: every member is exactly one row, so the distinct
/// set is the members themselves. Until this, the bare `v` inside the
/// aggregate declined the rewrite (`rewrite` has no local for a whole
/// node) and the statement fell to the general path, which materialised
/// every survivor and kept a serialised id per row: the NewsArticle
/// classification aggregate (`… RETURN a.contentType AS key,
/// count(DISTINCT a) AS n` over 66k survivors) ran 305 ms on the mirror
/// against 128 for the same statement spelt `count(a)` and Neo4j's 225.
/// Only a single-variable scan may do this — a hop's start repeats across
/// its edges (`star_counts` leaves DISTINCT alone there). ORDER BY items
/// naming the call are rewritten alike, so `order_over` still finds them.
fn star_distinct_counts(proj: &Projection, var: &str) -> Projection {
    let mut p = proj.clone();
    let mut rewritten = false;
    let mut star_it = |e: &mut Expr| {
        if let Expr::Call {
            name,
            distinct,
            args,
            star,
        } = e
        {
            if name == "count"
                && *distinct
                && !*star
                && matches!(args.as_slice(), [Expr::Var(v)] if v == var)
            {
                args.clear();
                *star = true;
                *distinct = false;
                rewritten = true;
            }
        }
    };
    for it in &mut p.items {
        star_it(&mut it.expr);
    }
    for o in &mut p.order {
        star_it(&mut o.expr);
    }
    if rewritten {
        counted!("interp.columnar count distinct of the scanned variable counted its members");
    }
    p
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
    if path.var.is_some() || path.shortest.is_some() {
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
    // Fix 81: the WHERE's conjuncts that read the start alone are the
    // start's predicate; the rest stays per row. Both go through the same
    // three passes and are evaluated in sequence, so a row passes exactly
    // when the whole WHERE would (AND is associative under nulls).
    let (a_only, rest): (Option<Expr>, Option<Expr>) = match (&full_where, &a_var) {
        (Some(w), Some(av)) => {
            let mut cs = Vec::new();
            crate::interp::conjuncts_of(w, &mut cs);
            let (a_only, rest): (Vec<Expr>, Vec<Expr>) = cs
                .into_iter()
                .partition(|c| reads_only(c, std::slice::from_ref(av)));
            (and_all(a_only), and_all(rest))
        }
        (w, _) => (None, w.clone()),
    };
    let b_only: Vec<String> = b_var.iter().cloned().collect();
    let far_only = agg_proj
        .items
        .iter()
        .all(|it| reads_only(&it.expr, &b_only))
        && rest.as_ref().is_none_or(|r| reads_only(r, &b_only));
    // The passes: the node ends first (labels and probes are node reads),
    // the relationship last.
    let mut a_pred = a_only;
    let mut pred = rest;
    if let Some(v) = &a_var {
        a_pred = rewrite_opt(a_pred.as_ref(), v, Kind::Node, &mut a.reads)?;
        pred = rewrite_opt(pred.as_ref(), v, Kind::Node, &mut a.reads)?;
    }
    if let Some(v) = &b_var {
        a_pred = rewrite_opt(a_pred.as_ref(), v, Kind::Node, &mut b.reads)?;
        pred = rewrite_opt(pred.as_ref(), v, Kind::Node, &mut b.reads)?;
    }
    if let Some(v) = &r_var {
        a_pred = rewrite_opt(a_pred.as_ref(), v, Kind::Rel, &mut r_reads)?;
        pred = rewrite_opt(pred.as_ref(), v, Kind::Rel, &mut r_reads)?;
    }
    // See `recognise`: any subquery none of the hop's vars could lift into a
    // probe has no hooks in this columnar scan — decline to the interp path.
    if a_pred.as_ref().is_some_and(contains_opaque) || pred.as_ref().is_some_and(contains_opaque) {
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
        a_pred,
        pred,
        far_only,
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

/// Fix 58: the SEEDS of a hop driven from a sought start — the ids its
/// declared-key equality selects, with the hop's type tokens — or `None`
/// for the whole-type walk. The start drives the walk when its
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
fn hop_seeded_seeds(
    graph: &Graph,
    plan: &HopPlan,
    params: &BTreeMap<String, Value>,
) -> Result<Option<HopSeeds>, RunError> {
    if !plan.r_reads.reads_nothing() {
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
    let Some(sought) = hop_end_seek(graph, &plan.a.labels, &plan.a_seeks, params)? else {
        return Ok(None);
    };
    Ok(Some(HopSeeds {
        ids: sought,
        tokens: Some(tokens),
    }))
}

/// A sought start's seeds (sorted, distinct, members of its label) and
/// the hop's type tokens.
struct HopSeeds {
    ids: Vec<u64>,
    tokens: Option<Vec<u32>>,
}

/// Fix 81: the seeded walk. Fix 58 expanded every seed's adjacency into
/// `(src, dst)` pairs and ran the per-row loop over them, so the start-only
/// WHERE was evaluated once per EDGE (83k evaluations of one user's
/// abuse-status test over her 18.7k emails), a bare `count(*)` bound and
/// folded every edge, and a far-end group key was evaluated per edge
/// though it is a function of the end (250k expressions, 310 ms, against
/// Neo4j's 156). Now each seed is bound once and the start's predicate
/// keeps or drops it BEFORE its adjacency is read; a fold that reads
/// nothing of the far end sums each survivor's degree (member peers only,
/// when the far end is labelled); a fold whose keys and residual predicate
/// read the far end alone counts edges per distinct far end and folds each
/// end once, weighted; anything else walks the survivors' edges as before.
/// Groups are made in the order their first row would have arrived, so an
/// unordered result is byte-identical to the per-edge fold's.
fn run_seeded_hop_aggregate(
    graph: &Graph,
    plan: &HopPlan,
    params: &BTreeMap<String, Value>,
    seeds: HopSeeds,
) -> Result<Option<QueryResult>, RunError> {
    counted!("interp.columnar hop scan seeded from a sought end");
    let HopSeeds {
        ids: sought,
        tokens,
    } = seeds;
    let dir = if plan.out { Dir::Out } else { Dir::In };
    let mut fold = Fold::new(&plan.items);
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
    // A seek that selects nobody answers from the empty fold before any
    // column is loaded: an empty supplied population still sized its walk
    // by the end label's rows and gathered the whole label (12k gets for
    // an unknown user).
    if sought.is_empty() {
        counted!("interp.statements run");
        counted!("interp.columnar aggregate scans");
        counted!("interp.columnar hop aggregate scans");
        sometimes!("interp.columnar hop scan ran", true);
        return fold.finish(graph, params, &spec, &mut scope).map(Some);
    }
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
    // The start's columns over the seeds (fix 58: served through its
    // label's property-column cache, restricted to the seeds).
    let a_source = Source::Nodes {
        labels: plan.a.labels.clone(),
        any_of: Vec::new(),
    };
    let a_rows = a_members
        .as_ref()
        .map(|m| m.len())
        .unwrap_or(0)
        .max(sought.len());
    let sought: std::sync::Arc<Vec<u64>> = std::sync::Arc::new(sought);
    let Some(a_walk) = load_walk_budgeted(
        graph,
        &a_source,
        &plan.a.reads,
        Some(std::sync::Arc::clone(&sought)),
        Some(a_rows),
        params,
    )?
    else {
        counted!("interp.columnar hop scan declined an end column");
        return Ok(None);
    };
    counted!("interp.statements run");
    counted!("interp.columnar aggregate scans");
    counted!("interp.columnar hop aggregate scans");
    sometimes!("interp.columnar hop scan ran", true);
    if a_members.is_some() || b_members.is_some() {
        sometimes!("interp.columnar hop scan filtered an end by label", true);
    }
    // The start's predicate, once per seed.
    let survivors: Vec<u64> = match &plan.a_pred {
        None => sought.to_vec(),
        Some(_) => {
            let mut keep = Vec::with_capacity(sought.len());
            for &id in sought.iter() {
                scope.locals.clear();
                a_walk.bind_random(graph, &plan.a.reads, &mut scope, id)?;
                if row_passes(&plan.a_pred, &scope)? {
                    keep.push(id);
                }
            }
            counted!("interp.columnar seeded hop filtered its seeds by the start's predicate");
            keep
        }
    };
    let all_star = fold.sites.iter().all(|(s, _)| s.star);
    let has_keys = !fold.key_exprs.is_empty();
    if all_star && plan.pred.is_none() && plan.b.reads.reads_nothing() {
        // Every row of a seed is alike: its degree is its count.
        for &id in &survivors {
            let n = match &b_members {
                None => graph.count_adjacent_memo(id, dir, &tokens),
                Some(m) => {
                    let mut n = 0u64;
                    graph.adjacent_slim_for_each(id, dir, &tokens, |e| {
                        if graph.members_contains(m, e.peer) {
                            n += 1;
                        }
                    });
                    n
                }
            };
            if n == 0 {
                continue;
            }
            scope.locals.clear();
            if has_keys {
                a_walk.bind_random(graph, &plan.a.reads, &mut scope, id)?;
            }
            fold.push_n(graph, &scope, n)?;
        }
        counted!("interp.columnar seeded hop summed degrees per seed");
        return fold.finish(graph, params, &spec, &mut scope).map(Some);
    }
    let b_source = Source::Nodes {
        labels: plan.b.labels.clone(),
        any_of: Vec::new(),
    };
    if all_star && plan.far_only {
        // The key is a function of the far end: count edges per distinct
        // end, then bind and fold each end once, weighted, in the order
        // the ends first appeared.
        let mut peers: Vec<u64> = Vec::new();
        for &id in &survivors {
            graph.adjacent_slim_for_each(id, dir, &tokens, |e| peers.push(e.peer));
        }
        let mut distinct = peers.clone();
        distinct.sort_unstable();
        distinct.dedup();
        if let Some(m) = &b_members {
            distinct.retain(|&p| graph.members_contains(m, p));
        }
        if distinct.is_empty() {
            return fold.finish(graph, params, &spec, &mut scope).map(Some);
        }
        let mut count = vec![0u64; distinct.len()];
        let mut seen = vec![false; distinct.len()];
        let mut order: Vec<usize> = Vec::with_capacity(distinct.len());
        for p in &peers {
            if let Ok(pos) = distinct.binary_search(p) {
                count[pos] += 1;
                if !seen[pos] {
                    seen[pos] = true;
                    order.push(pos);
                }
            }
        }
        let b_ids = std::sync::Arc::new(distinct);
        let b_rows = b_members
            .as_ref()
            .map(|m| m.len())
            .unwrap_or(0)
            .max(b_ids.len());
        let Some(b_walk) = load_walk_budgeted(
            graph,
            &b_source,
            &plan.b.reads,
            Some(std::sync::Arc::clone(&b_ids)),
            Some(b_rows),
            params,
        )?
        else {
            counted!("interp.columnar hop scan declined an end column");
            return Ok(None);
        };
        for &pos in &order {
            scope.locals.clear();
            b_walk.bind_random(graph, &plan.b.reads, &mut scope, b_ids[pos])?;
            if !row_passes(&plan.pred, &scope)? {
                continue;
            }
            fold.push_n(graph, &scope, count[pos])?;
        }
        counted!("interp.columnar seeded hop folded per distinct far end");
        return fold.finish(graph, params, &spec, &mut scope).map(Some);
    }
    // The general shape: the survivors' edges, one row each.
    let mut ends: Vec<(u64, u64)> = Vec::new();
    for &id in &survivors {
        graph.adjacent_slim_for_each(id, dir, &tokens, |e| {
            ends.push(match dir {
                Dir::Out => (id, e.peer),
                _ => (e.peer, id),
            });
        });
    }
    if ends.is_empty() {
        return fold.finish(graph, params, &spec, &mut scope).map(Some);
    }
    let b_ids = distinct_ends(&ends, !plan.out);
    let b_rows = b_members
        .as_ref()
        .map(|m| m.len())
        .unwrap_or(0)
        .max(b_ids.len());
    let Some(b_walk) = load_walk_budgeted(
        graph,
        &b_source,
        &plan.b.reads,
        Some(b_ids),
        Some(b_rows),
        params,
    )?
    else {
        counted!("interp.columnar hop scan declined an end column");
        return Ok(None);
    };
    for &(src, dst) in &ends {
        let (a_id, b_id) = if plan.out { (src, dst) } else { (dst, src) };
        if let Some(m) = &b_members {
            if !graph.members_contains(m, b_id) {
                continue;
            }
        }
        scope.locals.clear();
        a_walk.bind_random(graph, &plan.a.reads, &mut scope, a_id)?;
        b_walk.bind_random(graph, &plan.b.reads, &mut scope, b_id)?;
        if !row_passes(&plan.pred, &scope)? {
            continue;
        }
        fold.push(graph, &scope)?;
    }
    fold.finish(graph, params, &spec, &mut scope).map(Some)
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
    // the population is that end's typed adjacency — no relationship
    // record read — and the end columns are loaded over the ends it
    // actually reaches (`run_seeded_hop_aggregate`, fix 81: folded per
    // seed or per distinct far end where the fold allows).
    if let Some(seeds) = hop_seeded_seeds(graph, &plan, params)? {
        return run_seeded_hop_aggregate(graph, &plan, params, seeds);
    }
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
    // The relationship walk: its columns in relationship order.
    let rel_source = Source::Rels {
        types: plan.types.clone(),
    };
    let Some(mut r_walk) = load_walk(graph, &rel_source, &plan.r_reads, params)? else {
        return Ok(None);
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
        let rel_id = r_walk.members[ri];
        r_walk.bind(graph, &plan.r_reads, &mut scope, ri, rel_id)?;
        a_walk.bind_random(graph, &plan.a.reads, &mut scope, a_id)?;
        b_walk.bind_random(graph, &plan.b.reads, &mut scope, b_id)?;
        if !row_passes(&plan.a_pred, &scope)? || !row_passes(&plan.pred, &scope)? {
            continue;
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
