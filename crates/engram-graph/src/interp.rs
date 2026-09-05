//! The Cypher interpreter — statements run against the graph, rows of
//! decoded values out. Correctness first: this is the tree-walker the
//! exec-operator planner will later substitute for, behind the same results.
//!
//! Where a construct is not yet supported it refuses BY NAME
//! ([`RunError::Unsupported`]) — never an empty result, which would read as
//! "matched nothing" (the house defect in query-engine form).

use std::collections::{BTreeMap, BTreeSet};

use engram_cypher::ast::Expr;
use engram_cypher::eval::{EvalError, GraphHooks, Scope, eval_with, is_known_function};
use engram_cypher::stmt::{
    Clause, NodePattern, PathPattern, Pattern, ProjItem, Projection, Query, RelDir, RelPattern,
    RemoveItem, SetItem, SingleQuery, SubqueryBody,
};
use engram_cypher::value::{Truth, Value};
use engram_observe::{counted, sometimes};

use crate::{Dir, Graph, GraphError};

/// The NULL sentinel for an OPTIONAL-MATCH binding in a columnar id column
/// (Phase 4b2). An id-column entry of `NULL_ID` is the LEFT-JOIN's null-fill:
/// the optional pattern produced no match for that outer row, so every
/// optional-introduced var binds to `Value::Null` rather than to a node.
///
/// DENSITY INVARIANT: node ids are allocated densely from 1 by `Graph::next_id`
/// (one counter, incremented per entity; bulk mode only ever leaves *gaps*), so
/// a real id reaching `u64::MAX` would require 2^64 nodes and is unreachable.
/// `NULL_ID` therefore never collides with a real node id. `node_of` maps it
/// back to `Value::Null`; wherever an id column is turned into a node binding,
/// route through `node_of` so the sentinel materialises as null, not a lookup.
pub(crate) const NULL_ID: u64 = u64::MAX;

/// Materialise one id-column entry as a node binding — `Value::Null` for the
/// OPTIONAL-MATCH sentinel [`NULL_ID`], else the decoded node (a missing real id
/// is the same hard error the per-tuple path raises). This is the one place the
/// columnar pipeline turns an id into a node value for a possibly-nullable var.
pub(crate) fn node_of(graph: &Graph, id: u64) -> Result<Value, RunError> {
    if id == NULL_ID {
        return Ok(Value::Null);
    }
    Ok(graph.node(id)?.ok_or(GraphError::Missing("node", id))?)
}

/// What a bound-variable id column materialises to: a `node_of` NODE, or a
/// `rel_of` RELATIONSHIP. Carried parallel to the bound-var names so every
/// site that turns a var id into a `Value` (project / group template /
/// aggregate arg) picks the right materialiser, and so every property-column
/// load reads the right record family.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum VarKind {
    Node,
    Rel,
}

/// Materialise one id-column entry as a RELATIONSHIP binding — the `rel_of`
/// sibling of [`node_of`]. `Value::Null` for the OPTIONAL-MATCH sentinel
/// [`NULL_ID`] (so `RETURN r.prop` / `count(r)` on an unmatched optional row
/// behave three-valued), else the decoded relationship exactly as
/// `run_streaming` binds it (`RelRow::to_value`: `Value::Rel { id, src, dst,
/// rel_type, props }`). A missing real id is the same hard error the per-tuple
/// path raises.
pub(crate) fn rel_of(graph: &Graph, id: u64) -> Result<Value, RunError> {
    if id == NULL_ID {
        return Ok(Value::Null);
    }
    Ok(graph
        .rel(id)?
        .ok_or(GraphError::Missing("relationship", id))?
        .to_value())
}

/// Turn a var id column entry into its `Value`, routed by the var's [`VarKind`]
/// — the single dispatch the projection / group-template materialisers share.
pub(crate) fn value_of(graph: &Graph, kind: VarKind, id: u64) -> Result<Value, RunError> {
    match kind {
        VarKind::Node => node_of(graph, id),
        VarKind::Rel => rel_of(graph, id),
    }
}

/// A query's result: columns and rows of decoded values.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    /// Column names, in projection order.
    pub columns: Vec<String>,
    /// The rows.
    pub rows: Vec<Vec<Value>>,
}

/// Why a run refused.
#[derive(Debug)]
pub enum RunError {
    /// Expression evaluation refused.
    Eval(EvalError),
    /// The graph refused.
    Graph(GraphError),
    /// A construct this interpreter does not implement yet — refused by
    /// name, never answered with empty rows.
    Unsupported(String),
    /// A semantic rule (clause ordering, type expectations) was violated.
    Semantic(String),
    /// INTERNAL: a plain `LIMIT` has all the rows it can ever emit, so the
    /// producer must stop. Rides the ordinary `?` unwind through every scan
    /// loop and is caught at the stage loop — it must NEVER surface to a
    /// caller, and `run_streaming` maps a leak to a named internal error.
    #[doc(hidden)]
    Saturated,
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Saturated => write!(
                f,
                "internal: sink saturated — this control-flow marker escaped its stage"
            ),
            RunError::Eval(e) => write!(f, "{e}"),
            RunError::Graph(e) => write!(f, "{e}"),
            RunError::Unsupported(what) => write!(f, "not supported yet: {what}"),
            RunError::Semantic(what) => write!(f, "{what}"),
        }
    }
}

impl std::error::Error for RunError {}

impl From<EvalError> for RunError {
    fn from(e: EvalError) -> Self {
        RunError::Eval(e)
    }
}

impl From<GraphError> for RunError {
    fn from(e: GraphError) -> Self {
        RunError::Graph(e)
    }
}

/// The row: sorted small-vector bindings (see engram_cypher::bindings) —
/// BTreeMap iteration order, one-allocation clones.
pub(crate) type Row = engram_cypher::bindings::VarMap;

/// Collect free variable references in an expression — the names an
/// evaluator would look up in the row. Comprehension-local bindings
/// (`[x IN … | …]`, `reduce(acc = …, x IN …)`, `any(x IN …)`) are excluded;
/// subquery shapes (EXISTS/COUNT blocks, pattern predicates/comprehensions)
/// own their scopes and are deliberately NOT descended into, so this check
/// can only under-report, never falsely refuse.
/// The free variables of an expression, for callers outside this module.
pub(crate) fn free_vars_of(e: &Expr, out: &mut Vec<String>) {
    free_vars(e, &mut Vec::new(), out);
}

fn free_vars(e: &Expr, locals: &mut Vec<String>, out: &mut Vec<String>) {
    match e {
        Expr::Var(v) if !locals.contains(v) && !out.contains(v) => {
            out.push(v.clone());
        }
        Expr::Call { args, .. } | Expr::List(args) => {
            for a in args {
                free_vars(a, locals, out);
            }
        }
        Expr::Map(entries) => {
            for (_, v) in entries {
                free_vars(v, locals, out);
            }
        }
        Expr::Bin(_, a, b)
        | Expr::And(a, b)
        | Expr::Or(a, b)
        | Expr::Xor(a, b)
        | Expr::In(a, b)
        | Expr::Index(a, b) => {
            free_vars(a, locals, out);
            free_vars(b, locals, out);
        }
        Expr::Not(a) | Expr::Neg(a) | Expr::Prop(a, _) => free_vars(a, locals, out),
        Expr::IsNull { of, .. } => free_vars(of, locals, out),
        Expr::Slice { of, from, to } => {
            free_vars(of, locals, out);
            if let Some(f) = from {
                free_vars(f, locals, out);
            }
            if let Some(t) = to {
                free_vars(t, locals, out);
            }
        }
        Expr::Case {
            subject,
            arms,
            otherwise,
        } => {
            if let Some(x) = subject {
                free_vars(x, locals, out);
            }
            for (w, t) in arms {
                free_vars(w, locals, out);
                free_vars(t, locals, out);
            }
            if let Some(x) = otherwise {
                free_vars(x, locals, out);
            }
        }
        Expr::ListComp {
            var,
            source,
            filter,
            map,
        } => {
            free_vars(source, locals, out);
            locals.push(var.clone());
            if let Some(f) = filter {
                free_vars(f, locals, out);
            }
            if let Some(m) = map {
                free_vars(m, locals, out);
            }
            locals.pop();
        }
        Expr::Reduce {
            acc,
            init,
            var,
            source,
            step,
        } => {
            free_vars(init, locals, out);
            free_vars(source, locals, out);
            locals.push(acc.clone());
            locals.push(var.clone());
            free_vars(step, locals, out);
            locals.pop();
            locals.pop();
        }
        Expr::ListPredicate {
            var,
            source,
            filter,
            ..
        } => {
            free_vars(source, locals, out);
            locals.push(var.clone());
            free_vars(filter, locals, out);
            locals.pop();
        }
        Expr::HasLabels { of, .. } => free_vars(of, locals, out),
        Expr::MapProjection { of, items } => {
            free_vars(of, locals, out);
            for it in items {
                if let engram_cypher::ast::MapProjectionItem::Entry(_, e) = it {
                    free_vars(e, locals, out);
                }
            }
        }
        _ => {}
    }
}

/// Compile-time argument-TYPE checks driven by the statically-known variable
/// kinds ('n' node, 'r' rel, 'p' path): `type()` needs a relationship,
/// `length()` a path, `size()` a list/string (never a path), and a path has no
/// property accessor. Only fires on a variable whose kind is unambiguous.
fn check_fn_arg_kinds(
    e: &Expr,
    types: &std::collections::BTreeMap<String, char>,
) -> Result<(), RunError> {
    let bad = |d: &str| RunError::Semantic(format!("{d} (InvalidArgumentType)"));
    match e {
        Expr::Call { name, args, .. } => {
            // An unknown function is a COMPILE-time error (`UnknownFunction`),
            // caught here so `RETURN foo(x)` fails even over an empty match where
            // no row ever reaches the runtime registry. Aggregates and every
            // scalar builtin are known; a dotted name (`datetime.fromEpoch`) is
            // checked as its lower-cased whole.
            if !is_known_function(name) {
                return Err(RunError::Semantic(format!(
                    "unknown function `{name}()` (UnknownFunction)"
                )));
            }
            if let Some(Expr::Var(v)) = args.first() {
                match (name.as_str(), types.get(v)) {
                    ("type", Some('n')) => {
                        return Err(bad("`type()` takes a relationship, not a node"));
                    }
                    ("length", Some('n' | 'r')) => {
                        return Err(bad("`length()` takes a path"));
                    }
                    ("size", Some('p')) => {
                        return Err(bad("`size()` takes a list or string, not a path"));
                    }
                    _ => {}
                }
            }
            for a in args {
                check_fn_arg_kinds(a, types)?;
            }
        }
        Expr::Prop(base, _) => {
            if let Expr::Var(v) = base.as_ref() {
                if types.get(v) == Some(&'p') {
                    return Err(bad("a path has no property accessor"));
                }
            }
            check_fn_arg_kinds(base, types)?;
        }
        Expr::List(args) => {
            for a in args {
                check_fn_arg_kinds(a, types)?;
            }
        }
        Expr::Map(es) => {
            for (_, v) in es {
                check_fn_arg_kinds(v, types)?;
            }
        }
        Expr::Bin(_, a, b)
        | Expr::And(a, b)
        | Expr::Or(a, b)
        | Expr::Xor(a, b)
        | Expr::In(a, b)
        | Expr::Index(a, b) => {
            check_fn_arg_kinds(a, types)?;
            check_fn_arg_kinds(b, types)?;
        }
        Expr::Not(a) | Expr::Neg(a) | Expr::HasLabels { of: a, .. } => {
            check_fn_arg_kinds(a, types)?
        }
        Expr::IsNull { of, .. } => check_fn_arg_kinds(of, types)?,
        Expr::Slice { of, from, to } => {
            check_fn_arg_kinds(of, types)?;
            if let Some(f) = from {
                check_fn_arg_kinds(f, types)?;
            }
            if let Some(t) = to {
                check_fn_arg_kinds(t, types)?;
            }
        }
        Expr::Case {
            subject,
            arms,
            otherwise,
        } => {
            if let Some(s) = subject {
                check_fn_arg_kinds(s, types)?;
            }
            for (w, t) in arms {
                check_fn_arg_kinds(w, types)?;
                check_fn_arg_kinds(t, types)?;
            }
            if let Some(o) = otherwise {
                check_fn_arg_kinds(o, types)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// The PATTERN an `EXISTS { … }` / `COUNT { … }` body is, when it is one:
/// the bare-pattern form (`EXISTS { (n)-[:T]->(:L) [WHERE w] }`) and the
/// full-query form whose only clause is a plain `MATCH` of that pattern
/// (`EXISTS { MATCH (n)-[:T]->(:L) [WHERE w] }`) ask exactly the same
/// question, so every reader that recognises the first must recognise the
/// second through this — the demand walk, the columnar probe lift, the
/// evaluator's adjacency fast path. Until they did, the production spelling
/// `NOT EXISTS { MATCH (n)-[:MENTIONS_INTEREST]->(:Interest) }` cost 2,788 ms
/// over the email label where `NOT exists((n)-[:MENTIONS_INTEREST]->(:Interest))`
/// cost 95: the Query body was never lifted to a probe and demanded the node
/// in FULL, so the count materialised 38k email records. A body with any
/// other clause (a WITH, a second MATCH, an OPTIONAL MATCH, an update) is
/// `None` and keeps the full-subquery treatment.
pub(crate) fn pattern_body(body: &SubqueryBody) -> Option<(&Pattern, Option<&Expr>)> {
    match body {
        SubqueryBody::Pattern { pattern, where_ } => Some((pattern, where_.as_ref())),
        SubqueryBody::Query(q) => match q.clauses.as_slice() {
            [Clause::Match {
                optional: false,
                pattern,
                where_,
            }] => Some((pattern, where_.as_ref())),
            _ => None,
        },
    }
}

/// Whether `e` contains an `EXISTS { … }` / `COUNT { … }` subquery whose body
/// runs an UPDATE clause — read-only subqueries may not write, so that is an
/// InvalidClauseComposition. Walks the boolean-operator spine (where such
/// subqueries live); a subquery nested inside a scalar is rare and left to run.
fn exists_subquery_updates(e: &Expr) -> bool {
    use engram_cypher::stmt::SubqueryBody;
    let body_updates = |b: &SubqueryBody| match b {
        SubqueryBody::Pattern { .. } => false,
        SubqueryBody::Query(sq) => sq.clauses.iter().any(|c| {
            matches!(
                c,
                Clause::Set { .. }
                    | Clause::Create { .. }
                    | Clause::Delete { .. }
                    | Clause::Merge { .. }
                    | Clause::Remove { .. }
                    | Clause::Foreach { .. }
            )
        }),
    };
    match e {
        Expr::ExistsSub(b) | Expr::CountSub(b) => body_updates(b),
        Expr::And(a, b) | Expr::Or(a, b) | Expr::Xor(a, b) => {
            exists_subquery_updates(a) || exists_subquery_updates(b)
        }
        Expr::Not(a) => exists_subquery_updates(a),
        _ => false,
    }
}

/// Collect every `PatternPredicate` path reachable in `e` (a pattern used as a
/// boolean expression, e.g. `WHERE (n)-[r]->(a)`). Such a pattern may only
/// REFERENCE already-bound variables — introducing a new one (`r`, `a`) is an
/// error in openCypher — so the caller checks each collected path's variables.
fn collect_pattern_preds<'a>(e: &'a Expr, out: &mut Vec<&'a PathPattern>) {
    use engram_cypher::ast::MapProjectionItem;
    match e {
        Expr::PatternPredicate(p) => out.push(p),
        Expr::Call { args, .. } | Expr::List(args) => {
            args.iter().for_each(|a| collect_pattern_preds(a, out));
        }
        Expr::Map(es) => es.iter().for_each(|(_, v)| collect_pattern_preds(v, out)),
        Expr::Bin(_, a, b)
        | Expr::And(a, b)
        | Expr::Or(a, b)
        | Expr::Xor(a, b)
        | Expr::In(a, b)
        | Expr::Index(a, b) => {
            collect_pattern_preds(a, out);
            collect_pattern_preds(b, out);
        }
        Expr::Not(a) | Expr::Neg(a) | Expr::Prop(a, _) | Expr::HasLabels { of: a, .. } => {
            collect_pattern_preds(a, out)
        }
        Expr::IsNull { of, .. } => collect_pattern_preds(of, out),
        Expr::Slice { of, from, to } => {
            collect_pattern_preds(of, out);
            from.iter()
                .chain(to.iter())
                .for_each(|x| collect_pattern_preds(x, out));
        }
        Expr::Case {
            subject,
            arms,
            otherwise,
        } => {
            subject
                .iter()
                .chain(otherwise.iter())
                .for_each(|x| collect_pattern_preds(x, out));
            arms.iter().for_each(|(w, t)| {
                collect_pattern_preds(w, out);
                collect_pattern_preds(t, out);
            });
        }
        // For the local-binding forms only the OUTER-scoped `source`/`init` are
        // descended; the filter/map/step see a comprehension-local variable that is
        // (correctly) absent from `all_bound`, so validating a pattern predicate
        // there would false-positive on that local. Missing such a nested predicate
        // is the safe direction (this pass may only miss an error, never invent one).
        Expr::ListComp { source, .. } => collect_pattern_preds(source, out),
        Expr::Reduce { init, source, .. } => {
            collect_pattern_preds(init, out);
            collect_pattern_preds(source, out);
        }
        Expr::ListPredicate { source, .. } => collect_pattern_preds(source, out),
        Expr::MapProjection { of, items } => {
            collect_pattern_preds(of, out);
            items.iter().for_each(|it| {
                if let MapProjectionItem::Entry(_, e) = it {
                    collect_pattern_preds(e, out);
                }
            });
        }
        _ => {}
    }
}

/// Refuse a WHERE that references a variable NOTHING binds — Neo4j refuses
/// this at parse time, and the port benchmark measured what accepting it
/// costs here: `MATCH (n) WHERE n.x = nid` materialised every node in the
/// database into rows before eval discovered `nid` does not exist, and the
/// OOM killer answered instead of the error. Names in scope are the
/// incoming row's bindings plus the pattern's own variables.
fn check_where_scope(where_: &Expr, pattern: &Pattern, rows: &[Row]) -> Result<(), RunError> {
    let mut free = Vec::new();
    free_vars(where_, &mut Vec::new(), &mut free);
    if free.is_empty() {
        return Ok(());
    }
    let mut bound: Vec<String> = rows
        .first()
        .map(|r| r.keys().cloned().collect())
        .unwrap_or_default();
    for path in &pattern.paths {
        bound.extend(path_vars(path));
    }
    for v in free {
        if !bound.contains(&v) {
            sometimes!("interp.unbound WHERE variable refused", true);
            return Err(RunError::Semantic(format!("Variable `{v}` not defined")));
        }
    }
    Ok(())
}

/// Answer a bare count without materialising what it counts.
///
/// The strict shapes: `MATCH (n) RETURN count(n)`, `MATCH (n:L) RETURN
/// count(n)`, and `MATCH ()-[r]->() RETURN count(r)` (either direction, no
/// types, no WHERE, no props, no DISTINCT, no ORDER/SKIP/LIMIT, `count(*)`
/// accepted). Anything looser falls through to the general path — the fast
/// path must be provably equivalent or absent, never approximately right.
/// The general path clones every matched entity's full property map into a
/// row; counting a 1.8M-node graph through it was an OOM, not a number.
/// The relationship-type histogram, answered from adjacency keys:
/// `MATCH ()-[r]->() WITH type(r) AS t, count(*) AS c RETURN … [ORDER BY …]
/// [SKIP …] [LIMIT …]`. The census shape — 5.29M relationship-record
/// decodes on the production port (28.8 s) for information that sits in
/// the key bytes of one O-side walk. Anything richer (labels, types,
/// props, WHERE, extra items) declines to the general path.
fn try_rel_histogram_fast(
    graph: &Graph,
    q: &SingleQuery,
    params: &BTreeMap<String, Value>,
) -> Option<QueryResult> {
    let [
        Clause::Match {
            optional: false,
            pattern,
            where_: None,
        },
        Clause::With {
            proj: with_proj,
            where_: None,
        },
        Clause::Return { proj: ret },
    ] = q.clauses.as_slice()
    else {
        return None;
    };
    // The pattern: exactly ()-[r]->() — one directed untyped hop, nothing
    // constrained, rel var bound.
    if pattern.paths.len() != 1 {
        return None;
    }
    let path = &pattern.paths[0];
    if path.var.is_some() || path.shortest || path.hops.len() != 1 {
        return None;
    }
    let (rel, end) = &path.hops[0];
    if !path.start.labels.is_empty()
        || path.start.props.is_some()
        || path.start.var.is_some()
        || !end.labels.is_empty()
        || end.props.is_some()
        || end.var.is_some()
    {
        return None;
    }
    if !rel.types.is_empty()
        || rel.props.is_some()
        || rel.length.is_some()
        || matches!(rel.dir, engram_cypher::stmt::RelDir::Undirected)
    {
        return None;
    }
    let rvar = rel.var.as_deref()?;
    // The WITH: exactly {type(r) AS a, count(*) AS b}, in either order.
    if with_proj.star
        || with_proj.distinct
        || !with_proj.order.is_empty()
        || with_proj.skip.is_some()
        || with_proj.limit.is_some()
        || with_proj.items.len() != 2
    {
        return None;
    }
    let mut type_alias: Option<String> = None;
    let mut count_alias: Option<String> = None;
    for (i, item) in with_proj.items.iter().enumerate() {
        let alias = item
            .alias
            .clone()
            .or_else(|| item.text.clone())
            .unwrap_or_else(|| column_name(&item.expr, i));
        match &item.expr {
            Expr::Call {
                name,
                distinct: false,
                args,
                star: false,
            } if name == "type" => match args.as_slice() {
                [Expr::Var(v)] if v == rvar => type_alias = Some(alias),
                _ => return None,
            },
            Expr::Call {
                name,
                distinct: false,
                args,
                star: true,
            } if name == "count" && args.is_empty() => count_alias = Some(alias),
            _ => return None,
        }
    }
    let (type_alias, count_alias) = (type_alias?, count_alias?);
    // The RETURN: plain items over the two aliases, ORDER BY over them.
    if ret.star || ret.distinct {
        return None;
    }
    let allowed =
        |e: &Expr| -> bool { matches!(e, Expr::Var(v) if *v == type_alias || *v == count_alias) };
    if !ret.items.iter().all(|it| allowed(&it.expr)) {
        return None;
    }
    if !ret.order.iter().all(|o| allowed(&o.expr)) {
        return None;
    }
    let hist = graph.rel_type_histogram().ok()?;
    sometimes!("interp.count answered by the fast path", true);
    counted!("interp.statements run");
    // Bind each (name, count) as a mini-row and reuse the projector's own
    // ordering + paging semantics.
    let mut out_rows: Vec<Vec<Value>> = Vec::with_capacity(hist.len());
    let mut order_keys: Vec<Vec<Value>> = Vec::with_capacity(hist.len());
    for (name, n) in hist {
        let mut row = Row::new();
        row.insert(type_alias.clone(), Value::Str(name));
        row.insert(count_alias.clone(), Value::Int(n as i64));
        let mut projected = Vec::with_capacity(ret.items.len());
        for it in &ret.items {
            projected.push(eval_expr(graph, &it.expr, &row, params).ok()?);
        }
        let mut key = Vec::with_capacity(ret.order.len());
        for o in &ret.order {
            key.push(eval_expr(graph, &o.expr, &row, params).ok()?);
        }
        out_rows.push(projected);
        order_keys.push(key);
    }
    if !ret.order.is_empty() {
        let mut idx: Vec<usize> = (0..out_rows.len()).collect();
        idx.sort_by(|&a, &b| cmp_order_keys(&ret.order, &order_keys[a], &order_keys[b]));
        out_rows = idx
            .into_iter()
            .map(|i| std::mem::take(&mut out_rows[i]))
            .collect();
    }
    let skip = eval_count(graph, ret.skip.as_ref(), params, "SKIP")
        .ok()?
        .unwrap_or(0);
    if skip > 0 {
        out_rows.drain(..skip.min(out_rows.len()));
    }
    if let Some(limit) = eval_count(graph, ret.limit.as_ref(), params, "LIMIT").ok()? {
        out_rows.truncate(limit);
    }
    let columns = ret
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
    Some(QueryResult {
        columns,
        rows: out_rows,
    })
}

/// The match count of one prop-free, WHERE-free path — `(n:L…)` from the
/// count store, `(:A?)-[:T?]->(:B?)` from adjacency keys and membership
/// snapshots — for `count(*)` (`counted_var` None) or a variable the
/// pattern binds (never null in a match, so `count(v) == count(*)`).
/// `None` for any other shape: a path variable, shortestPath, props,
/// undirected or variable-length hops, more than one hop.
fn fast_count_for_path(
    graph: &Graph,
    path: &PathPattern,
    counted_var: Option<&str>,
) -> Option<u64> {
    if path.var.is_some() || path.shortest || path.start.props.is_some() {
        return None;
    }
    Some(match path.hops.as_slice() {
        // (n) or (n:L) — node counts.
        [] => {
            if let Some(v) = counted_var {
                if path.start.var.as_deref() != Some(v) {
                    return None;
                }
            }
            graph.count_labels_nodes(&path.start.labels).ok()?
        }
        // (:A?)-[:T?]->(:B?) — DIRECTED single-hop match counts, answered
        // from adjacency keys and membership snapshots without a record
        // decode (count_hop). Undirected declines: it matches every
        // relationship twice when both endpoint predicates hold, which is
        // row semantics, not degree arithmetic.
        [(rel, end)] => {
            if end.props.is_some() || rel.props.is_some() || rel.length.is_some() {
                return None;
            }
            if matches!(rel.dir, engram_cypher::stmt::RelDir::Undirected) {
                return None;
            }
            // `(n)-[r]->(n)` — the SAME variable at both ends — matches only
            // self-loops, which degree/label arithmetic cannot express (it counts
            // every edge whose endpoints satisfy the labels, self-loop or not).
            // Decline to the general path, which enforces start == end.
            if path.start.var.is_some() && path.start.var == end.var {
                return None;
            }
            if let Some(v) = counted_var {
                // Any pattern-bound variable counts matches: none is ever
                // null in a match, so count(a) == count(r) == count(*).
                let bound = [
                    path.start.var.as_deref(),
                    rel.var.as_deref(),
                    end.var.as_deref(),
                ];
                if !bound.contains(&Some(v)) {
                    return None;
                }
            }
            let (from_labels, to_labels) = (&path.start.labels, &end.labels);
            // The pattern arrow and the storage direction: `-[]->` walks
            // OUT from the start node; `<-[]-` walks IN.
            let dir = match rel.dir {
                engram_cypher::stmt::RelDir::Out => Dir::Out,
                engram_cypher::stmt::RelDir::In => Dir::In,
                engram_cypher::stmt::RelDir::Undirected => unreachable!(),
            };
            if from_labels.is_empty() && to_labels.is_empty() && rel.types.is_empty() {
                graph.count_all_rels()
            } else {
                graph
                    .count_hop(from_labels, dir, &rel.types, to_labels)
                    .ok()?
            }
        }
        _ => return None,
    })
}

fn try_count_fast(graph: &Graph, q: &SingleQuery) -> Option<QueryResult> {
    let [
        Clause::Match {
            optional: false,
            pattern,
            where_: None,
        },
        Clause::Return { proj },
    ] = q.clauses.as_slice()
    else {
        return None;
    };
    if pattern.paths.len() != 1 {
        return None;
    }
    let path = &pattern.paths[0];
    if path.var.is_some() || path.shortest || path.start.props.is_some() {
        return None;
    }
    if proj.distinct
        || proj.star
        || !proj.order.is_empty()
        || proj.skip.is_some()
        || proj.limit.is_some()
    {
        return None;
    }
    let [item] = proj.items.as_slice() else {
        return None;
    };
    let Expr::Call {
        name,
        distinct: false,
        args,
        star,
    } = &item.expr
    else {
        return None;
    };
    if name != "count" {
        return None;
    }
    let counted_var: Option<&str> = if *star {
        None
    } else {
        match args.as_slice() {
            [Expr::Var(v)] => Some(v.as_str()),
            _ => return None,
        }
    };

    let n = fast_count_for_path(graph, path, counted_var)?;
    sometimes!("interp.count answered by the fast path", true);
    counted!("interp.statements run");
    let col = item
        .alias
        .clone()
        .or_else(|| item.text.clone())
        .unwrap_or_else(|| column_name(&item.expr, 0));
    Some(QueryResult {
        columns: vec![col],
        rows: vec![vec![Value::Int(n as i64)]],
    })
}

/// Refuse a statement whose intermediate row set outgrows the configured
/// budget — the alternative is the OOM killer, which refuses NOTHING and
/// takes every other session with it.
pub(crate) fn budget_check(graph: &Graph, n: usize) -> Result<(), RunError> {
    if let Some(b) = graph.row_budget() {
        if n > b {
            sometimes!("interp.row budget refused a statement", true);
            return Err(RunError::Semantic(format!(
                "row budget exceeded: the statement materialised more than {b} intermediate rows; it would exhaust memory rather than stream"
            )));
        }
    }
    Ok(())
}

/// Run any statement — a query, a schema command (whose result is the
/// empty table), or `SHOW`, the one schema command that ANSWERS (the
/// catalogue listing) rather than mutates.
pub fn run_stmt(
    graph: &Graph,
    stmt: &engram_cypher::stmt::Stmt,
    params: BTreeMap<String, Value>,
) -> Result<QueryResult, RunError> {
    match stmt {
        engram_cypher::stmt::Stmt::Query(q) => run_query(graph, q, params),
        engram_cypher::stmt::Stmt::Schema(cmd) => {
            counted!("interp.statements run");
            if let engram_cypher::stmt::SchemaCmd::Show { subject, tail } = cmd {
                let (columns, rows) = graph.show_schema(subject, *tail)?;
                return Ok(QueryResult { columns, rows });
            }
            graph.apply_schema(cmd)?;
            Ok(QueryResult {
                columns: Vec::new(),
                rows: Vec::new(),
            })
        }
    }
}

/// The output scope of a `WITH`/`RETURN` projection: aliases, bare-var items,
/// and (for `*`) everything already in scope. An unaliased non-var item names a
/// column but not a reusable variable, so it does not enter scope.
fn projected_names(
    proj: &engram_cypher::stmt::Projection,
    prev: &std::collections::BTreeSet<String>,
) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    if proj.star {
        out.extend(prev.iter().cloned());
    }
    for it in &proj.items {
        if let Some(a) = &it.alias {
            out.insert(a.clone());
        } else if let Expr::Var(v) = &it.expr {
            out.insert(v.clone());
        }
    }
    out
}

/// openCypher `VariableAlreadyBound` (a COMPILE-time error, so it must be caught
/// statically — the data-driven runtime check never fires over an empty graph):
/// a `CREATE` may REFERENCE an already-bound node as a bare relationship
/// endpoint, but it cannot re-create it as a standalone node, add labels or
/// properties to it, or reuse a bound relationship variable.
fn check_create_path_bindings(
    path: &engram_cypher::stmt::PathPattern,
    scope: &std::collections::BTreeSet<String>,
) -> Result<(), RunError> {
    let already = |v: &str| {
        Err(RunError::Semantic(format!(
            "Variable `{v}` already bound (VariableAlreadyBound)"
        )))
    };
    let check_node = |n: &engram_cypher::stmt::NodePattern| -> Result<(), RunError> {
        if let Some(v) = &n.var {
            if scope.contains(v) && (!n.labels.is_empty() || n.props.is_some()) {
                return already(v);
            }
        }
        Ok(())
    };
    if path.hops.is_empty() {
        if let Some(v) = &path.start.var {
            if scope.contains(v) {
                return already(v);
            }
        }
    }
    check_node(&path.start)?;
    for (rel, node) in &path.hops {
        if let Some(v) = &rel.var {
            if scope.contains(v) {
                return already(v);
            }
        }
        check_node(node)?;
    }
    Ok(())
}

/// Static semantic checks over one query arm, run before execution so they hold
/// regardless of the data (a compile-time error must fire even when the read
/// that would trip the runtime check returns no rows). Scope-tracking is
/// data-independent: each clause adds the variables it introduces, a `WITH`
/// resets scope to its projection.
fn validate_single(q: &SingleQuery) -> Result<(), RunError> {
    // The node + relationship variables of a path (NOT its path variable) —
    // these may legally repeat (that is how patterns join), so they are never
    // themselves the already-bound error, but a colliding PATH variable is.
    fn node_rel_vars(path: &engram_cypher::stmt::PathPattern) -> Vec<String> {
        let mut vs = Vec::new();
        if let Some(v) = &path.start.var {
            vs.push(v.clone());
        }
        for (rel, node) in &path.hops {
            if let Some(v) = &rel.var {
                vs.push(v.clone());
            }
            if let Some(v) = &node.var {
                vs.push(v.clone());
            }
        }
        vs
    }
    // Each pattern variable's KIND: 'n' node, 'r' relationship, 'p' path. A
    // variable used with two kinds is VariableTypeConflict (except a fresh-path
    // collision, caught earlier as VariableAlreadyBound). Node/rel repetition of
    // the SAME kind is a legal join.
    fn pattern_var_kinds(path: &engram_cypher::stmt::PathPattern) -> Vec<(String, char)> {
        let mut out = Vec::new();
        if let Some(v) = &path.var {
            out.push((v.clone(), 'p'));
        }
        if let Some(v) = &path.start.var {
            out.push((v.clone(), 'n'));
        }
        for (rel, node) in &path.hops {
            if let Some(v) = &rel.var {
                // A VAR-LENGTH rel variable binds a LIST of relationships, so its
                // kind is a VALUE ('v'): it agrees with a list value
                // (`WITH [r1,r2] AS rs … MATCH ()-[rs*]->()`) but still conflicts
                // with node / single-relationship use. A single hop is 'r'.
                out.push((v.clone(), if rel.length.is_none() { 'r' } else { 'v' }));
            }
            if let Some(v) = &node.var {
                out.push((v.clone(), 'n'));
            }
        }
        out
    }
    let mut scope: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut types: BTreeMap<String, char> = BTreeMap::new();
    // Register each pattern var's kind, raising VariableTypeConflict on a kind
    // change. A path kind is never re-registered here — its freshness (and hence
    // any collision) is the VariableAlreadyBound check's job.
    let register = |types: &mut BTreeMap<String, char>,
                    path: &engram_cypher::stmt::PathPattern|
     -> Result<(), RunError> {
        for (v, k) in pattern_var_kinds(path) {
            if k == 'p' {
                types.insert(v, 'p');
            } else if let Some(&old) = types.get(&v) {
                if old != k {
                    return Err(RunError::Semantic(format!(
                        "Variable `{v}` is used as both a {} and a {} (VariableTypeConflict)",
                        kind_word(old),
                        kind_word(k),
                    )));
                }
            } else {
                types.insert(v, k);
            }
        }
        Ok(())
    };
    // A WHERE filters rows and so cannot aggregate — an aggregate there is
    // InvalidAggregation (`WITH count(*) AS c WHERE c > 1` aliases first, so its
    // WHERE holds only `c`, not the call).
    let no_agg_in_where = |w: Option<&Expr>| -> Result<(), RunError> {
        if let Some(w) = w {
            if expr_has_aggregate(w) {
                return Err(RunError::Semantic(
                    "an aggregate is not allowed in WHERE (InvalidAggregation)".into(),
                ));
            }
        }
        Ok(())
    };
    for clause in &q.clauses {
        match clause {
            Clause::Match {
                pattern, where_, ..
            } => {
                no_agg_in_where(where_.as_ref())?;
                // A named-path variable must be FRESH: not bound by a preceding
                // clause, by an earlier path in this MATCH, or by a node/rel var
                // within its own path (`p = (p)-[]-()`). Node/rel vars may repeat.
                let mut local = scope.clone();
                for p in &pattern.paths {
                    if let Some(pv) = &p.var {
                        if local.contains(pv) || node_rel_vars(p).contains(pv) {
                            return Err(RunError::Semantic(format!(
                                "Variable `{pv}` already bound (VariableAlreadyBound)"
                            )));
                        }
                    }
                    register(&mut types, p)?;
                    for v in path_vars(p) {
                        local.insert(v);
                    }
                }
                scope = local;
            }
            Clause::Unwind { alias, .. } => {
                scope.insert(alias.clone());
            }
            Clause::Create { pattern } => {
                for p in &pattern.paths {
                    check_create_path_bindings(p, &scope)?;
                    register(&mut types, p)?;
                    for v in path_vars(p) {
                        scope.insert(v);
                    }
                }
            }
            Clause::Merge { path, .. } => {
                register(&mut types, path)?;
                for v in path_vars(path) {
                    scope.insert(v);
                }
            }
            Clause::With { proj, where_ } => {
                scope = projected_names(proj, &scope);
                no_agg_in_where(where_.as_ref())?;
                // A projection re-derives kinds. An item whose expression is
                // STATICALLY a value (a literal, arithmetic, list, map, boolean)
                // binds a VALUE ('v'); a bare pass-through keeps the source's
                // kind; anything else is unknown and drops out. A later pattern
                // that reuses a 'v' name as a node/rel/path is VariableTypeConflict.
                let mut next: BTreeMap<String, char> = BTreeMap::new();
                for it in &proj.items {
                    let name = it.alias.clone().or_else(|| match &it.expr {
                        Expr::Var(v) => Some(v.clone()),
                        _ => None,
                    });
                    let Some(name) = name else { continue };
                    if matches!(
                        &it.expr,
                        Expr::Int(_)
                            | Expr::Float(_)
                            | Expr::Str(_)
                            | Expr::Bool(_)
                            | Expr::List(_)
                            | Expr::Map(_)
                            | Expr::Bin(..)
                            | Expr::Neg(_)
                            | Expr::Not(_)
                            | Expr::And(..)
                            | Expr::Or(..)
                            | Expr::Xor(..)
                            | Expr::In(..)
                            | Expr::HasLabels { .. }
                            | Expr::IsNull { .. }
                    ) {
                        next.insert(name, 'v');
                    } else if let Expr::Var(v) = &it.expr {
                        if let Some(&k) = types.get(v) {
                            next.insert(name, k);
                        }
                    }
                }
                types = next;
            }
            _ => {}
        }
    }

    // Argument-type checks against the static variable kinds (`type()` on a node,
    // `length()`/`size()` on the wrong shape, a property on a path).
    // A WHERE that is a bare node/rel/path variable (`WHERE (n)`) is not a boolean.
    let where_kind_ok =
        |w: &Expr, types: &std::collections::BTreeMap<String, char>| -> Result<(), RunError> {
            if let Expr::Var(v) = w {
                if matches!(types.get(v), Some('n' | 'r' | 'p')) {
                    return Err(RunError::Semantic(
                    "a node, relationship or path is not a boolean predicate (InvalidArgumentType)"
                        .into(),
                ));
                }
            }
            Ok(())
        };
    for clause in &q.clauses {
        match clause {
            Clause::Match {
                where_: Some(w), ..
            } => {
                where_kind_ok(w, &types)?;
                check_fn_arg_kinds(w, &types)?;
            }
            Clause::With { proj, where_ } => {
                for it in &proj.items {
                    check_fn_arg_kinds(&it.expr, &types)?;
                }
                if let Some(w) = where_ {
                    where_kind_ok(w, &types)?;
                    check_fn_arg_kinds(w, &types)?;
                }
            }
            Clause::Return { proj } => {
                for it in &proj.items {
                    check_fn_arg_kinds(&it.expr, &types)?;
                }
            }
            _ => {}
        }
    }

    // InvalidAggregation: an aggregate inside a list comprehension's per-element
    // body (its filter or map) — `[x IN xs | count(*)]`. This is a property of
    // the expression alone, so it is safe across subquery scopes.
    let comp_ok = |e: &Expr| -> Result<(), RunError> {
        let mut bad_agg = false;
        let mut bad_size = false;
        walk_expr(e, &mut |x| {
            if let Expr::ListComp { filter, map, .. } = x {
                if filter.as_deref().is_some_and(expr_has_aggregate)
                    || map.as_deref().is_some_and(expr_has_aggregate)
                {
                    bad_agg = true;
                }
            }
            // `size((n)-->())` — `size()` over a raw pattern was removed from the
            // language; a pattern predicate is boolean-only (a WHERE), never a
            // list to size. openCypher raises UnexpectedSyntax.
            if let Expr::Call { name, args, .. } = x {
                if name == "size" && args.iter().any(|a| matches!(a, Expr::PatternPredicate(_))) {
                    bad_size = true;
                }
            }
        });
        if bad_agg {
            Err(RunError::Semantic(
                "an aggregate is not allowed in a list comprehension (InvalidAggregation)".into(),
            ))
        } else if bad_size {
            Err(RunError::Semantic(
                "size() over a pattern is not valid syntax (UnexpectedSyntax)".into(),
            ))
        } else {
            Ok(())
        }
    };
    // InvalidAggregation: an aggregate in ORDER BY is only allowed when the
    // projection itself aggregates (then ORDER BY sorts the groups); a
    // non-aggregating projection cannot introduce a fresh aggregate to sort by.
    let order_ok = |proj: &engram_cypher::stmt::Projection| -> Result<(), RunError> {
        if proj.items.iter().any(|it| expr_has_aggregate(&it.expr)) {
            return Ok(());
        }
        for o in &proj.order {
            if expr_has_aggregate(&o.expr) {
                return Err(RunError::Semantic(
                    "an aggregate is not allowed in ORDER BY of a non-aggregating projection \
                     (InvalidAggregation)"
                        .into(),
                ));
            }
        }
        Ok(())
    };
    // ORDER BY after a DISTINCT or AGGREGATING projection is evaluated over the
    // projection's OUTPUT scope: it may reference the output columns and the
    // grouping-key EXPRESSIONS (`ORDER BY a.name + 'C'` with a key `a.name`), but
    // NOT a bare pre-projection variable the horizon dropped (`ORDER BY a.age`
    // when only `a.name` was kept) — openCypher `UndefinedVariable`, a
    // COMPILE-time error that must fire even over an empty match. A plain
    // (non-distinct, non-aggregating) projection keeps its input scope, so this
    // does not constrain it.
    let order_scope_ok = |proj: &engram_cypher::stmt::Projection| -> Result<(), RunError> {
        let aggregating = proj.items.iter().any(|it| expr_has_aggregate(&it.expr));
        if proj.star || (!proj.distinct && !aggregating) || proj.order.is_empty() {
            return Ok(());
        }
        let items: Vec<(String, Expr)> = proj
            .items
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let name = it
                    .alias
                    .clone()
                    .or_else(|| it.text.clone())
                    .unwrap_or_else(|| column_name(&it.expr, i));
                (name, it.expr.clone())
            })
            .collect();
        let cols: std::collections::BTreeSet<&str> =
            items.iter().map(|(n, _)| n.as_str()).collect();
        for o in &proj.order {
            let rw = rewrite_order_over_projection(&o.expr, &items);
            let mut fvs = Vec::new();
            free_vars_of(&rw, &mut fvs);
            if let Some(f) = fvs.iter().find(|f| !cols.contains(f.as_str())) {
                return Err(RunError::Semantic(format!(
                    "ORDER BY references `{f}`, not in scope after the projection (UndefinedVariable)"
                )));
            }
        }
        Ok(())
    };
    for clause in &q.clauses {
        match clause {
            Clause::Match {
                where_: Some(w), ..
            } => comp_ok(w)?,
            Clause::With { proj, where_ } => {
                for it in &proj.items {
                    comp_ok(&it.expr)?;
                }
                if let Some(w) = where_ {
                    comp_ok(w)?;
                }
                order_ok(proj)?;
                order_scope_ok(proj)?;
            }
            Clause::Return { proj } => {
                for it in &proj.items {
                    comp_ok(&it.expr)?;
                }
                order_ok(proj)?;
                order_scope_ok(proj)?;
            }
            Clause::Unwind { expr, .. } => comp_ok(expr)?,
            // DELETE requires a node, relationship, or path. An operand that is
            // STATICALLY a value (a literal, arithmetic, boolean, or a label
            // predicate `n:L`) can never be one — a compile-time error, so it
            // must fire even when the MATCH returns no rows. A variable / call /
            // index / case might yield an entity, so those are left to runtime.
            Clause::Delete { exprs, .. } => {
                for e in exprs {
                    if matches!(
                        e,
                        Expr::Int(_)
                            | Expr::Float(_)
                            | Expr::Str(_)
                            | Expr::Bool(_)
                            | Expr::Bin(..)
                            | Expr::Neg(_)
                            | Expr::Not(_)
                            | Expr::And(..)
                            | Expr::Or(..)
                            | Expr::Xor(..)
                            | Expr::In(..)
                            | Expr::HasLabels { .. }
                            | Expr::IsNull { .. }
                    ) {
                        return Err(RunError::Semantic(
                            "DELETE requires a node, relationship, or path, not a value \
                             (InvalidArgumentType)"
                                .into(),
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    // ── UndefinedVariable ─────────────────────────────────────────────────
    // Every variable BOUND anywhere in the arm; a reference to a name outside
    // this set is UndefinedVariable (compile-time — must fire over an empty
    // graph too). `free_vars_of` UNDER-reports (it doesn't descend subquery /
    // comprehension scopes), so this can only miss a real error, never falsely
    // refuse. `CALL { … }` runs via `run_query_seeded`, which does NOT re-run
    // this validation, so the only cross-scope name is a subquery's YIELDS —
    // added here. A subquery `RETURN *` yields names we can't enumerate, so the
    // check is skipped when one is present.
    let mut all_bound: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut skip_undef = false;
    let add_proj_names =
        |proj: &engram_cypher::stmt::Projection,
         all_bound: &mut std::collections::BTreeSet<String>| {
            for it in &proj.items {
                if let Some(a) = &it.alias {
                    all_bound.insert(a.clone());
                } else if let Expr::Var(v) = &it.expr {
                    all_bound.insert(v.clone());
                }
            }
        };
    for clause in &q.clauses {
        match clause {
            Clause::Match { pattern, .. } | Clause::Create { pattern } => {
                for p in &pattern.paths {
                    for v in path_vars(p) {
                        all_bound.insert(v);
                    }
                }
            }
            Clause::Merge { path, .. } => {
                for v in path_vars(path) {
                    all_bound.insert(v);
                }
            }
            Clause::Unwind { alias, .. } => {
                all_bound.insert(alias.clone());
            }
            Clause::With { proj, .. } | Clause::Return { proj } => {
                // Only ALIASES bind new names here. A bare-var projection
                // (`RETURN n`) is a passthrough that must ALREADY be bound, so it is
                // NOT added — adding it would mask `RETURN foo` (undefined) by
                // declaring `foo` bound off its own reference. (Subquery yields DO
                // bind their bare vars, so `add_proj_names` still adds them below.)
                for it in &proj.items {
                    if let Some(a) = &it.alias {
                        all_bound.insert(a.clone());
                    }
                }
            }
            Clause::CallProcedure { yields, .. } => {
                for (n, a) in yields {
                    all_bound.insert(a.clone().unwrap_or_else(|| n.clone()));
                }
            }
            Clause::Foreach { var, .. } => {
                all_bound.insert(var.clone());
            }
            Clause::CallSubquery { query, .. } => {
                let arm = match query.as_ref() {
                    Query::Single(s) => Some(s),
                    Query::Union { arms, .. } => arms.first(),
                };
                if let Some(Clause::Return { proj }) = arm.and_then(|s| s.clauses.last()) {
                    if proj.star {
                        skip_undef = true;
                    }
                    add_proj_names(proj, &mut all_bound);
                }
            }
            _ => {}
        }
    }
    if !skip_undef {
        let check = |e: &Expr| -> Result<(), RunError> {
            let mut fv = Vec::new();
            free_vars_of(e, &mut fv);
            for v in fv {
                if !all_bound.contains(&v) {
                    return Err(RunError::Semantic(format!(
                        "Variable `{v}` not defined (UndefinedVariable)"
                    )));
                }
            }
            Ok(())
        };
        let check_name = |v: &str| -> Result<(), RunError> {
            if all_bound.contains(v) {
                Ok(())
            } else {
                Err(RunError::Semantic(format!(
                    "Variable `{v}` not defined (UndefinedVariable)"
                )))
            }
        };
        let check_set = |it: &engram_cypher::stmt::SetItem| -> Result<(), RunError> {
            use engram_cypher::stmt::SetItem;
            match it {
                SetItem::Prop { base, value, .. } => {
                    check(base)?;
                    check(value)
                }
                SetItem::Replace { var, value } | SetItem::Merge { var, value } => {
                    check_name(var)?;
                    check(value)
                }
                SetItem::Labels { var, .. } => check_name(var),
            }
        };
        let path_props = |path: &engram_cypher::stmt::PathPattern| -> Result<(), RunError> {
            if let Some(p) = &path.start.props {
                check(p)?;
            }
            for (rel, node) in &path.hops {
                if let Some(p) = &rel.props {
                    check(p)?;
                }
                if let Some(p) = &node.props {
                    check(p)?;
                }
            }
            Ok(())
        };
        for clause in &q.clauses {
            match clause {
                Clause::Match { pattern, .. } => {
                    for p in &pattern.paths {
                        path_props(p)?;
                    }
                    // WHERE is deliberately left to the RUNTIME refusal (which
                    // declines by name before any read), keeping that path — and
                    // its coverage — live.
                }
                Clause::Create { pattern } => {
                    for p in &pattern.paths {
                        path_props(p)?;
                    }
                }
                Clause::Merge {
                    path,
                    on_create,
                    on_match,
                } => {
                    path_props(path)?;
                    for it in on_create.iter().chain(on_match) {
                        check_set(it)?;
                    }
                }
                Clause::Set { items } => {
                    for it in items {
                        check_set(it)?;
                    }
                }
                Clause::Delete { exprs, .. } => {
                    for e in exprs {
                        check(e)?;
                    }
                }
                Clause::With { proj, .. } => {
                    for it in &proj.items {
                        check(&it.expr)?;
                    }
                    // WHERE (a HAVING here) left to the runtime refusal, as above.
                }
                Clause::Return { proj } => {
                    for it in &proj.items {
                        check(&it.expr)?;
                    }
                }
                Clause::Unwind { expr, .. } => check(expr)?,
                Clause::CallProcedure { args, .. } => {
                    for e in args {
                        check(e)?;
                    }
                }
                _ => {}
            }
        }

        // ── Pattern-predicate variables must be pre-bound ──────────────────
        // A pattern used as a boolean (`WHERE (n)-[r]->(a)`) may only REFERENCE
        // bound variables; introducing a new one (`r`, `a`) is UndefinedVariable.
        let check_preds = |e: &Expr| -> Result<(), RunError> {
            let mut preds = Vec::new();
            collect_pattern_preds(e, &mut preds);
            for p in preds {
                // A pattern predicate is a boolean about a RELATIONSHIP path; a bare
                // single node (`WHERE (n)`) is not a predicate — it is an error.
                if p.hops.is_empty() {
                    return Err(RunError::Semantic(
                        "a single node is not a pattern predicate (InvalidArgumentType)".into(),
                    ));
                }
                for v in node_rel_vars(p) {
                    if !all_bound.contains(&v) {
                        return Err(RunError::Semantic(format!(
                            "Variable `{v}` not defined (UndefinedVariable)"
                        )));
                    }
                }
            }
            Ok(())
        };
        for clause in &q.clauses {
            match clause {
                Clause::Match {
                    where_: Some(w), ..
                } => check_preds(w)?,
                Clause::With { proj, where_ } => {
                    for it in &proj.items {
                        check_preds(&it.expr)?;
                    }
                    if let Some(w) = where_ {
                        check_preds(w)?;
                    }
                }
                Clause::Return { proj } => {
                    for it in &proj.items {
                        check_preds(&it.expr)?;
                    }
                }
                Clause::Unwind { expr, .. } => check_preds(expr)?,
                _ => {}
            }
        }
    }

    // ── ColumnNameConflict ────────────────────────────────────────────────
    // A RETURN/WITH cannot project two columns with the same output name
    // (`RETURN 1 AS a, 2 AS a`) — a compile-time error.
    for clause in &q.clauses {
        if let Clause::Return { proj } | Clause::With { proj, .. } = clause {
            let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            for (i, it) in proj.items.iter().enumerate() {
                let name = it
                    .alias
                    .clone()
                    .or_else(|| it.text.clone())
                    .unwrap_or_else(|| column_name(&it.expr, i));
                if !seen.insert(name.clone()) {
                    return Err(RunError::Semantic(format!(
                        "column `{name}` is projected more than once (ColumnNameConflict)"
                    )));
                }
            }
        }
    }

    // ── InvalidClauseComposition (read-only subquery writes) ──────────────
    // An `EXISTS { … }` / `COUNT { … }` subquery is read-only; a write clause
    // inside it (`WHERE exists { MATCH … SET … }`) is a composition error.
    for clause in &q.clauses {
        if let Clause::Match {
            where_: Some(w), ..
        }
        | Clause::With {
            where_: Some(w), ..
        } = clause
        {
            if exists_subquery_updates(w) {
                return Err(RunError::Semantic(
                    "a read-only subquery may not contain an update clause (InvalidClauseComposition)"
                        .into(),
                ));
            }
        }
    }

    // ── AmbiguousAggregationExpression / NonConstantExpression ────────────
    // A projection/order expression that aggregates may only combine the
    // aggregate with SIMPLE grouping keys — a compound non-aggregate operand
    // (`me.age + you.age + count(*)`) is ambiguous; and a non-deterministic
    // function inside an aggregate (`count(rand())`) is NonConstantExpression.
    for clause in &q.clauses {
        if let Clause::Return { proj } | Clause::With { proj, .. } = clause {
            for it in &proj.items {
                if contains_aggregate(&it.expr) {
                    check_agg_ambiguity(&it.expr)?;
                    if agg_over_nondeterministic(&it.expr) {
                        return Err(RunError::Semantic(
                            "a non-deterministic function may not be aggregated (NonConstantExpression)"
                                .into(),
                        ));
                    }
                }
            }
            for o in &proj.order {
                if contains_aggregate(&o.expr) {
                    check_agg_ambiguity(&o.expr)?;
                }
            }
        }
    }

    // ── InvalidParameterUse ───────────────────────────────────────────────
    // A bare parameter as a node/rel's inline properties in MATCH (`MATCH (n
    // $param)`) is a predicate role a parameter may not fill — a parameter inside
    // a property MAP is fine, and CREATE `(n $param)` is a legitimate write.
    for clause in &q.clauses {
        if let Clause::Match { pattern, .. } = clause {
            let is_bare_param = |props: &Option<Expr>| matches!(props, Some(Expr::Param(_)));
            for p in &pattern.paths {
                let bad = is_bare_param(&p.start.props)
                    || p.hops
                        .iter()
                        .any(|(rel, node)| is_bare_param(&rel.props) || is_bare_param(&node.props));
                if bad {
                    return Err(RunError::Semantic(
                        "a parameter may not be a node/relationship predicate in MATCH (InvalidParameterUse)"
                            .into(),
                    ));
                }
            }
        }
    }

    // ── NoSingleRelationshipType (MERGE) ──────────────────────────────────
    // A MERGE relationship must name EXACTLY ONE type — it may have to CREATE it,
    // and cannot invent a typeless or an ambiguous multi-type relationship.
    for clause in &q.clauses {
        if let Clause::Merge { path, .. } = clause {
            for (rel, _) in &path.hops {
                if rel.types.len() != 1 {
                    return Err(RunError::Semantic(
                        "a MERGE relationship needs exactly one type (NoSingleRelationshipType)"
                            .into(),
                    ));
                }
            }
        }
    }

    // ── RelationshipUniquenessViolation ───────────────────────────────────
    // A relationship variable may not be traversed twice within one MATCH — each
    // relationship in a pattern is distinct (`(a)-[r]->()-[r]->(a)`).
    for clause in &q.clauses {
        if let Clause::Match { pattern, .. } = clause {
            let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            for p in &pattern.paths {
                for (rel, _) in &p.hops {
                    if let Some(v) = &rel.var {
                        if !seen.insert(v.clone()) {
                            return Err(RunError::Semantic(format!(
                                "relationship `{v}` is traversed twice in one pattern (RelationshipUniquenessViolation)"
                            )));
                        }
                    }
                }
            }
        }
    }

    // ── VariableAlreadyBound (MERGE) ──────────────────────────────────────
    // MERGE may ANCHOR on an already-bound node inside a larger pattern, but it
    // may not re-declare a bound RELATIONSHIP variable, nor MERGE a lone bound
    // node (`MATCH (a) MERGE (a)`). This is order-sensitive, so scope is tracked
    // clause by clause, with a WITH resetting it to its projected names.
    {
        let mut in_scope: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for clause in &q.clauses {
            match clause {
                Clause::Merge { path, .. } => {
                    for (rel, _) in &path.hops {
                        if let Some(v) = &rel.var {
                            if in_scope.contains(v) {
                                return Err(RunError::Semantic(format!(
                                    "`{v}` is already bound (VariableAlreadyBound)"
                                )));
                            }
                        }
                    }
                    if path.hops.is_empty() {
                        if let Some(v) = &path.start.var {
                            if in_scope.contains(v) {
                                return Err(RunError::Semantic(format!(
                                    "`{v}` is already bound (VariableAlreadyBound)"
                                )));
                            }
                        }
                    }
                    for v in path_vars(path) {
                        in_scope.insert(v);
                    }
                }
                Clause::Match { pattern, .. } | Clause::Create { pattern } => {
                    for p in &pattern.paths {
                        for v in path_vars(p) {
                            in_scope.insert(v);
                        }
                    }
                }
                Clause::Unwind { alias, .. } => {
                    in_scope.insert(alias.clone());
                }
                Clause::Foreach { var, .. } => {
                    in_scope.insert(var.clone());
                }
                Clause::CallProcedure { yields, .. } => {
                    for (n, a) in yields {
                        in_scope.insert(a.clone().unwrap_or_else(|| n.clone()));
                    }
                }
                Clause::With { proj, .. } if !proj.star => {
                    // A non-star WITH RESETS scope to just its projected names.
                    let mut next = std::collections::BTreeSet::new();
                    for it in &proj.items {
                        if let Some(a) = &it.alias {
                            next.insert(a.clone());
                        } else if let Expr::Var(v) = &it.expr {
                            next.insert(v.clone());
                        }
                    }
                    in_scope = next;
                }
                Clause::With { proj, .. } => {
                    // A `WITH *` carries the whole scope forward, plus any aliases.
                    for it in &proj.items {
                        if let Some(a) = &it.alias {
                            in_scope.insert(a.clone());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // ── NoExpressionAlias ─────────────────────────────────────────────────
    // A WITH projects into the next scope by NAME, so every non-variable item
    // must be aliased (`WITH a, count(*)` is invalid — `count(*)` has no name).
    // A bare variable carries its own name; RETURN has no such requirement.
    for clause in &q.clauses {
        if let Clause::With { proj, .. } = clause {
            for it in &proj.items {
                if it.alias.is_none() && !matches!(it.expr, Expr::Var(_)) {
                    return Err(RunError::Semantic(
                        "a non-variable WITH expression must be aliased (NoExpressionAlias)".into(),
                    ));
                }
            }
        }
    }

    Ok(())
}

/// The human word for a pattern-variable kind code.
fn kind_word(k: char) -> &'static str {
    match k {
        'n' => "node",
        'r' => "relationship",
        'v' => "value",
        _ => "path",
    }
}

/// openCypher scopes a `WITH`'s `WHERE` over BOTH the projection AND the WITH's
/// INPUT variables (`UNWIND … AS i WITH x AS y WHERE i <> j`). Engram evaluates
/// the WHERE over the projected row only, so it is hoisted structurally: a
/// non-aggregating `WITH <items> WHERE <w>` whose `<w>` reads input variables not
/// in the projection is split into `WITH <items>, <those inputs> WHERE <w>` then
/// a cleanup `WITH <original output names>` (carrying ORDER/SKIP/LIMIT/DISTINCT).
/// This produces standard clauses, so every execution path handles it unchanged.
/// It fires ONLY when the WHERE names a non-projected variable — an aggregating
/// WITH (a real HAVING) and a WHERE over projected names alone are untouched.
fn hoist_with_where(q: &SingleQuery) -> Option<SingleQuery> {
    use engram_cypher::stmt::{ProjItem, Projection};
    let mut changed = false;
    let mut out: Vec<Clause> = Vec::with_capacity(q.clauses.len());
    for c in &q.clauses {
        if let Clause::With {
            proj,
            where_: Some(w),
        } = c
        {
            // Every item must have a NAME (alias or bare var) to rebuild, and the
            // projection must not aggregate.
            let names: Option<Vec<String>> = if proj.star {
                None
            } else {
                proj.items
                    .iter()
                    .map(|it| {
                        it.alias.clone().or_else(|| match &it.expr {
                            Expr::Var(v) => Some(v.clone()),
                            _ => None,
                        })
                    })
                    .collect()
            };
            if let Some(names) = names {
                if !proj.items.iter().any(|it| expr_has_aggregate(&it.expr)) {
                    let out_set: std::collections::BTreeSet<&String> = names.iter().collect();
                    let mut wfv = Vec::new();
                    free_vars_of(w, &mut wfv);
                    let carry: Vec<String> =
                        wfv.into_iter().filter(|v| !out_set.contains(v)).collect();
                    if !carry.is_empty() {
                        let mut items = proj.items.clone();
                        for v in &carry {
                            items.push(ProjItem {
                                expr: Expr::Var(v.clone()),
                                alias: None,
                                text: None,
                            });
                        }
                        out.push(Clause::With {
                            proj: Projection {
                                distinct: false,
                                star: false,
                                items,
                                order: Vec::new(),
                                skip: None,
                                limit: None,
                            },
                            where_: Some(w.clone()),
                        });
                        out.push(Clause::With {
                            proj: Projection {
                                distinct: proj.distinct,
                                star: false,
                                items: names
                                    .iter()
                                    .map(|n| ProjItem {
                                        expr: Expr::Var(n.clone()),
                                        alias: None,
                                        text: None,
                                    })
                                    .collect(),
                                order: proj.order.clone(),
                                skip: proj.skip.clone(),
                                limit: proj.limit.clone(),
                            },
                            where_: None,
                        });
                        changed = true;
                        continue;
                    }
                }
            }
        }
        out.push(c.clone());
    }
    changed.then_some(SingleQuery { clauses: out })
}

/// Node and relationship ids DELETEd in the running statement. NODE and REL id
/// spaces are independent (`next_id("node")` vs `next_id("rel")`), so a node and
/// a relationship can share a numeric id — the two must be tracked separately or
/// deleting a rel would spuriously flag the node with the same id.
#[derive(Default)]
struct DeletedSets {
    nodes: std::collections::BTreeSet<u64>,
    rels: std::collections::BTreeSet<u64>,
}

thread_local! {
    /// Entities DELETEd in the running statement (see [`note_deleted`]). A later
    /// property/label read on one raises `DeletedEntityAccess` — the row still
    /// carries the pre-delete snapshot, so the graph alone cannot tell. Cleared
    /// at each top-level [`run_query`]; nested subqueries run through
    /// `run_single`, so the set spans the whole statement as required.
    static DELETED_ENTITIES: std::cell::RefCell<DeletedSets> =
        std::cell::RefCell::new(DeletedSets::default());
}

/// Record DELETEd node and relationship ids for the running statement.
fn note_deleted(node_ids: impl IntoIterator<Item = u64>, rel_ids: impl IntoIterator<Item = u64>) {
    DELETED_ENTITIES.with(|d| {
        let mut d = d.borrow_mut();
        d.nodes.extend(node_ids);
        d.rels.extend(rel_ids);
    });
}

/// Whether entity `id` (a node when `is_rel` is false, else a relationship) was
/// DELETEd earlier in the running statement.
fn is_deleted_entity(id: u64, is_rel: bool) -> bool {
    DELETED_ENTITIES.with(|d| {
        let d = d.borrow();
        if is_rel {
            d.rels.contains(&id)
        } else {
            d.nodes.contains(&id)
        }
    })
}

/// §7 — register every node-pattern restriction this statement's non-optional
/// MATCH clauses impose, for the commit-time predicate validator.
///
/// # The two rules, and why each one is a refusal
///
/// **A clause carrying a WHERE registers nothing.** The pattern is then only
/// PART of the predicate, so a restriction built from it alone matches rows the
/// statement would have rejected — and every such row becomes a false conflict.
/// Sound (aborting is always safe) but exactly the failure mode that turns an
/// isolation upgrade into a throughput collapse. Conservative on purpose: a
/// WHERE that constrains some OTHER variable also declines, which costs
/// coverage and cannot cost correctness.
///
/// **An OPTIONAL MATCH registers nothing.** It answers with a null row when
/// nothing matches, so a row appearing later changes its answer in a direction
/// this validator does not model.
///
/// Anything not registered keeps read-set validation, which is today's rule.
fn note_query_restrictions(graph: &Graph, query: &Query, params: &BTreeMap<String, Value>) {
    if !graph.precision_locking_enabled() {
        return;
    }
    let arms: &[SingleQuery] = match query {
        Query::Single(q) => std::slice::from_ref(q),
        Query::Union { arms, .. } => arms,
    };
    for q in arms {
        for clause in &q.clauses {
            let Clause::Match {
                optional: false,
                pattern,
                where_: None,
            } = clause
            else {
                continue;
            };
            for path in &pattern.paths {
                // The START only. A hop's node is reached by traversing an
                // edge, so a phantom there needs a new EDGE and not merely a
                // matching node; registering it would abort on a matching node
                // that nothing connects to us.
                graph.note_restriction(&path.start, params);
            }
        }
    }
}

/// Run one query.
pub fn run_query(
    graph: &Graph,
    query: &Query,
    params: BTreeMap<String, Value>,
) -> Result<QueryResult, RunError> {
    counted!("interp.statements run");
    // A fresh statement: no entity is deleted-from-under-us yet.
    DELETED_ENTITIES.with(|d| *d.borrow_mut() = DeletedSets::default());
    // §7 — RECORD THE PREDICATES, once, BEFORE any planner is chosen.
    //
    // This deliberately does not live in a planner. There are at least three
    // that can serve a single-node MATCH — `match_path`, the demanding
    // streaming path, and `try_columnar_projection` — and a hook in one of them
    // covers only the statements that planner happens to win. The first cut of
    // this item put hooks in two of the three and registered nothing at all for
    // `MATCH (n:P {tag: 'x'}) RETURN n`, because the columnar projection served
    // it. Registering from the AST makes that class of miss impossible: the
    // predicate is a property of the statement, not of the plan chosen for it.
    //
    // It also puts the CLAUSE'S WHERE in scope, which no planner-level site
    // had — and without it a restriction over-approximates its own statement
    // and aborts commits that were fine.
    note_query_restrictions(graph, query, &params);
    match query {
        Query::Single(q) => {
            let hoisted = hoist_with_where(q);
            let q = hoisted.as_ref().unwrap_or(q);
            validate_single(q)?;
            run_single(graph, q, &params, vec![Row::new()])
        }
        Query::Union { all, arms } => {
            let mut columns: Option<Vec<String>> = None;
            let mut rows = Vec::new();
            for arm in arms {
                validate_single(arm)?;
                let hoisted = hoist_with_where(arm);
                let arm = hoisted.as_ref().unwrap_or(arm);
                let r = run_single(graph, arm, &params, vec![Row::new()])?;
                match &columns {
                    None => columns = Some(r.columns),
                    Some(c) if *c != r.columns => {
                        return Err(RunError::Semantic(format!(
                            "UNION arms must project the same columns: {:?} vs {:?}",
                            c, r.columns
                        )));
                    }
                    Some(_) => {}
                }
                rows.extend(r.rows);
            }
            if !all {
                // The fourth dedup site, on the same canonical key as the
                // other three — UNION (without ALL) was also O(n²) and
                // also strict about Int-vs-Float.
                let mut nonce = 0u64;
                let mut seen = std::collections::BTreeSet::new();
                rows.retain(|r| seen.insert(agg_key_of(r, &mut nonce)));
            }
            Ok(QueryResult {
                columns: columns.unwrap_or_default(),
                rows,
            })
        }
    }
}

/// Fix 69: a MATCH / WITH WHERE conjunct that reads NO variable and holds
/// no subquery — `$viewerOrgId IS NOT NULL`, the parameter guard the
/// production visibility statements wrap around every equality — is a
/// per-statement CONSTANT: evaluated once here, a True one leaves the
/// WHERE, a False or Null one (the WHERE can then never hold) replaces the
/// whole WHERE with `false`. The AcceptanceCriterion listing declined the
/// pipeline on its two guard conjuncts alone and ran the general path's
/// seed filter over 4,229 members (3.0 ms on the mirror) where the same
/// statement without the guards ran the pipeline in 0.14 — the guards
/// were the whole cost. A non-boolean constant, or one that fails to
/// evaluate, stays as written: the row-time evaluation raises exactly as
/// it always did. `None` when nothing folds.
fn fold_constant_conjuncts(
    graph: &Graph,
    q: &SingleQuery,
    params: &BTreeMap<String, Value>,
) -> Result<Option<SingleQuery>, RunError> {
    let mut out: Option<SingleQuery> = None;
    let no_row = Row::new();
    for (ci, clause) in q.clauses.iter().enumerate() {
        let w = match clause {
            Clause::Match {
                where_: Some(w), ..
            }
            | Clause::With {
                where_: Some(w), ..
            } => w,
            _ => continue,
        };
        let mut conj = Vec::new();
        conjuncts_of(w, &mut conj);
        let mut kept: Vec<Expr> = Vec::with_capacity(conj.len());
        let mut never = false;
        let mut folded = 0usize;
        for c in conj {
            let mut free = Vec::new();
            free_vars(&c, &mut Vec::new(), &mut free);
            if !free.is_empty() || c.has_subquery() {
                kept.push(c);
                continue;
            }
            match eval_expr(graph, &c, &no_row, params) {
                Ok(Value::Bool(true)) => folded += 1,
                Ok(Value::Bool(false)) | Ok(Value::Null) => {
                    folded += 1;
                    never = true;
                }
                _ => kept.push(c),
            }
        }
        if folded == 0 {
            continue;
        }
        counted!("interp.constant conjunct folded");
        let where2 = if never {
            Some(Expr::Bool(false))
        } else {
            kept.into_iter()
                .reduce(|a, b| Expr::And(Box::new(a), Box::new(b)))
        };
        let target = out.get_or_insert_with(|| q.clone());
        match &mut target.clauses[ci] {
            Clause::Match { where_, .. } | Clause::With { where_, .. } => *where_ = where2,
            _ => unreachable!("only a MATCH or WITH reaches here"),
        }
    }
    Ok(out)
}

/// Fix 72: a `count(<var>)` over the chain a MATCH binds — with NOTHING
/// else of that chain read by the projection that follows — folds the
/// MATCH into the projection as `sum(COUNT { <chain> })`. The production
/// assistant-conversation listing, `MATCH (u:User {userId: $u})
/// -[:HAS_CONVERSATION]->(c) OPTIONAL MATCH (c)-[:HAS_BRANCH]->()
/// -[:HAS_MESSAGE]->(m) WITH c, count(m) AS messageCount RETURN … ORDER BY
/// c.updatedAt DESC SKIP … LIMIT …`, expanded every message of every
/// conversation into a row — 44,800 bare-bound hop ends and 90k
/// expressions on the mirror's largest user — to fold them straight back
/// into fifty integers: 107 ms against Neo4j's 12. With the clause folded,
/// each conversation's count is one `COUNT { … }` evaluation, which
/// `count_chain_fast` answers from the adjacency tables.
///
/// Exact by construction: every input row contributes exactly the number
/// of chain matches it would have expanded into, and `sum` over the group
/// re-adds them — duplicate group keys (two relationships into the same
/// conversation) double as they always did, an earlier OPTIONAL clause's
/// row multiplicity multiplies as it always did, and an OPTIONAL chain
/// with no match contributes its zero where the null row contributed none.
/// A plain MATCH drops the rows whose chain is empty, so a WITH gains
/// `WHERE <alias> > 0` (the count must then be a top-level aliased item);
/// a plain MATCH into a RETURN declines. The chain's WHERE moves into the
/// subquery body. Declined, and left exactly as written: a start not bound
/// by an earlier clause (or carrying labels or props), a bound middle or
/// end, an untyped or variable-length hop, a hop var or props, `count(*)`
/// over an OPTIONAL chain (it counts the null row), `count(DISTINCT …)`,
/// any OTHER aggregate in the projection (`collect(m.x)` reads the rows),
/// any non-aggregate read of a chain var, a `*` projection, and every
/// statement past a clause whose bindings this walker does not model
/// (`CALL { … }`, `FOREACH`). `None` when nothing folds.
fn fold_chain_counts(q: &SingleQuery) -> Option<SingleQuery> {
    fn pattern_vars(p: &Pattern, out: &mut Vec<String>) {
        for path in &p.paths {
            out.extend(path.var.iter().cloned());
            out.extend(path.start.var.iter().cloned());
            for (rel, node) in &path.hops {
                out.extend(rel.var.iter().cloned());
                out.extend(node.var.iter().cloned());
            }
        }
    }
    /// The names a projection leaves bound after it.
    fn projected_names(proj: &Projection, before: &[String]) -> Vec<String> {
        let mut names: Vec<String> = if proj.star { before.to_vec() } else { Vec::new() };
        for it in &proj.items {
            match (&it.alias, &it.expr) {
                (Some(a), _) => names.push(a.clone()),
                (None, Expr::Var(v)) => names.push(v.clone()),
                _ => {}
            }
        }
        names
    }
    /// `count(x)` with `x` one of the chain's vars, non-distinct — or
    /// `count(*)` when the chain is a plain MATCH (`star_ok`).
    fn chain_count(e: &Expr, chain: &[String], star_ok: bool) -> bool {
        match e {
            Expr::Call {
                name,
                distinct: false,
                args,
                star,
            } if name == "count" => {
                if *star {
                    return star_ok && args.is_empty();
                }
                matches!(args.as_slice(), [Expr::Var(v)] if chain.contains(v))
            }
            _ => false,
        }
    }
    fn reads_chain(e: &Expr, chain: &[String]) -> bool {
        let mut free = Vec::new();
        free_vars_of(e, &mut free);
        free.iter().any(|v| chain.contains(v))
    }
    fn fold_one(
        optional: bool,
        pattern: &Pattern,
        where_: Option<&Expr>,
        next: &Clause,
        proj: &Projection,
        visible: &[String],
    ) -> Option<Clause> {
        if pattern.paths.len() != 1 || proj.star {
            return None;
        }
        let path = &pattern.paths[0];
        if path.shortest || path.var.is_some() || path.hops.is_empty() {
            return None;
        }
        if !path.start.labels.is_empty() || path.start.props.is_some() {
            return None;
        }
        let sv = path.start.var.as_ref()?;
        if !visible.contains(sv) {
            return None;
        }
        let mut chain: Vec<String> = Vec::new();
        for (rel, node) in &path.hops {
            if rel.props.is_some()
                || rel.length.is_some()
                || rel.types.is_empty()
                || node.props.is_some()
            {
                return None;
            }
            for v in rel.var.iter().chain(node.var.iter()) {
                if v == sv || visible.contains(v) || chain.contains(v) {
                    return None;
                }
                chain.push(v.clone());
            }
        }
        if let Some(w) = where_ {
            if contains_aggregate(w) {
                return None;
            }
            let mut free = Vec::new();
            free_vars_of(w, &mut free);
            if free
                .iter()
                .any(|v| !chain.contains(v) && !visible.contains(v))
            {
                return None;
            }
        }
        let is_with = matches!(next, Clause::With { .. });
        if !optional && !is_with {
            return None;
        }
        let body = SubqueryBody::Pattern {
            pattern: pattern.clone(),
            where_: where_.cloned(),
        };
        let folded = Expr::Call {
            name: "sum".into(),
            distinct: false,
            args: vec![Expr::CountSub(Box::new(body))],
            star: false,
        };
        let mut counts = 0usize;
        let mut guard: Option<String> = None;
        let mut items = Vec::with_capacity(proj.items.len());
        for it in &proj.items {
            if chain_count(&it.expr, &chain, !optional) {
                counts += 1;
                if !optional {
                    guard = Some(it.alias.clone()?);
                }
                items.push(ProjItem {
                    expr: folded.clone(),
                    alias: it.alias.clone(),
                    text: it.text.clone(),
                });
            } else {
                if contains_aggregate(&it.expr) || reads_chain(&it.expr, &chain) {
                    return None;
                }
                items.push(it.clone());
            }
        }
        if counts == 0 {
            return None;
        }
        if proj
            .order
            .iter()
            .any(|o| contains_aggregate(&o.expr) || reads_chain(&o.expr, &chain))
        {
            return None;
        }
        let proj2 = Projection {
            distinct: proj.distinct,
            star: false,
            items,
            order: proj.order.clone(),
            skip: proj.skip.clone(),
            limit: proj.limit.clone(),
        };
        Some(match next {
            Clause::With { where_: w, .. } => {
                if w
                    .as_ref()
                    .is_some_and(|w| contains_aggregate(w) || reads_chain(w, &chain))
                {
                    return None;
                }
                let mut w2 = w.clone();
                if let Some(g) = guard {
                    let test = Expr::Bin(
                        engram_cypher::ast::BinOp::Gt,
                        Box::new(Expr::Var(g)),
                        Box::new(Expr::Int(0)),
                    );
                    w2 = Some(match w2 {
                        Some(x) => Expr::And(Box::new(x), Box::new(test)),
                        None => test,
                    });
                }
                Clause::With {
                    proj: proj2,
                    where_: w2,
                }
            }
            Clause::Return { .. } => Clause::Return { proj: proj2 },
            _ => return None,
        })
    }

    let n = q.clauses.len();
    let mut out: Vec<Clause> = Vec::with_capacity(n);
    let mut visible: Vec<String> = Vec::new();
    let mut folded = 0usize;
    let mut i = 0;
    while i < n {
        let clause = &q.clauses[i];
        if let (
            Clause::Match {
                optional,
                pattern,
                where_,
            },
            Some(next),
        ) = (clause, q.clauses.get(i + 1))
        {
            let proj = match next {
                Clause::With { proj, .. } | Clause::Return { proj } => Some(proj),
                _ => None,
            };
            if let Some(proj) = proj {
                if let Some(rewritten) =
                    fold_one(*optional, pattern, where_.as_ref(), next, proj, &visible)
                {
                    out.push(rewritten);
                    visible = projected_names(proj, &visible);
                    folded += 1;
                    i += 2;
                    continue;
                }
            }
        }
        match clause {
            Clause::Match { pattern, .. } | Clause::Create { pattern } => {
                pattern_vars(pattern, &mut visible)
            }
            Clause::Merge { path, .. } => pattern_vars(
                &Pattern {
                    paths: vec![path.clone()],
                },
                &mut visible,
            ),
            Clause::Unwind { alias, .. } => visible.push(alias.clone()),
            Clause::With { proj, .. } => visible = projected_names(proj, &visible),
            Clause::Return { .. }
            | Clause::Set { .. }
            | Clause::Remove { .. }
            | Clause::Delete { .. } => {}
            _ => {
                // A clause whose bindings this walker does not model:
                // nothing past it folds.
                out.extend(q.clauses[i..].iter().cloned());
                break;
            }
        }
        out.push(clause.clone());
        i += 1;
    }
    if folded == 0 {
        return None;
    }
    for _ in 0..folded {
        counted!("interp.chain count folded into its projection");
    }
    Some(SingleQuery { clauses: out })
}

/// Fix 45: a top-level `type(r) IN <constant list of strings>` (or `type(r)
/// = 'T'`) conjunct of a MATCH's WHERE, over a relationship variable the
/// pattern binds UNTYPED in exactly one fixed-length hop, folds into that
/// hop's types and leaves the WHERE. An untyped hop expands EVERY
/// relationship of every driving row and judges the type afterwards:
/// `MATCH (n:UserDataNode {userId: $u})-[r]->(t) WHERE type(r) IN
/// ['FRIEND_OF', 'KNOWS', 'WORKS_WITH'] RETURN type(r), count(r)` expanded a
/// 38k-email seed's whole adjacency on the mirror — 6.8 s and +771 MB
/// against Neo4j's 127 ms. Exact: a matched relationship's type is never
/// null, so with the hop restricted to the list the conjunct is always
/// true. `<>`, `NOT … IN`, a parameter list, an empty list or a nested
/// disjunction are left as written. `None` when nothing folds.
fn fold_type_filters(q: &SingleQuery) -> Option<SingleQuery> {
    fn split_and<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
        match e {
            Expr::And(a, b) => {
                split_and(a, out);
                split_and(b, out);
            }
            _ => out.push(e),
        }
    }
    fn type_of(e: &Expr) -> Option<&str> {
        match e {
            Expr::Call {
                name,
                distinct: false,
                star: false,
                args,
            } if name.eq_ignore_ascii_case("type") => match args.as_slice() {
                [Expr::Var(v)] => Some(v.as_str()),
                _ => None,
            },
            _ => None,
        }
    }
    /// `type(r) IN ['A', …]` / `type(r) = 'A'` / `'A' = type(r)` → (r, types).
    fn type_filter(e: &Expr) -> Option<(String, Vec<String>)> {
        match e {
            Expr::In(l, r) => {
                let v = type_of(l)?;
                let Expr::List(items) = r.as_ref() else {
                    return None;
                };
                let mut types: Vec<String> = Vec::with_capacity(items.len());
                for it in items {
                    let Expr::Str(s) = it else {
                        return None;
                    };
                    if !types.contains(s) {
                        types.push(s.clone());
                    }
                }
                if types.is_empty() {
                    return None;
                }
                Some((v.to_string(), types))
            }
            Expr::Bin(engram_cypher::ast::BinOp::Eq, l, r) => {
                if let (Some(v), Expr::Str(s)) = (type_of(l), r.as_ref()) {
                    return Some((v.to_string(), vec![s.clone()]));
                }
                if let (Expr::Str(s), Some(v)) = (l.as_ref(), type_of(r)) {
                    return Some((v.to_string(), vec![s.clone()]));
                }
                None
            }
            _ => None,
        }
    }
    let mut out: Option<SingleQuery> = None;
    for (ci, clause) in q.clauses.iter().enumerate() {
        let Clause::Match {
            optional,
            pattern,
            where_: Some(w),
        } = clause
        else {
            continue;
        };
        // How often each relationship variable is bound in this pattern: a
        // var bound twice is a join, not a hop to retype.
        let mut bound: BTreeMap<&str, usize> = BTreeMap::new();
        for p in &pattern.paths {
            for (rel, _) in &p.hops {
                if let Some(v) = &rel.var {
                    *bound.entry(v.as_str()).or_default() += 1;
                }
            }
        }
        let foldable = |name: &str| {
            bound.get(name) == Some(&1)
                && pattern.paths.iter().flat_map(|p| p.hops.iter()).any(|(rel, _)| {
                    rel.var.as_deref() == Some(name) && rel.types.is_empty() && rel.length.is_none()
                })
        };
        let mut conjuncts: Vec<&Expr> = Vec::new();
        split_and(w, &mut conjuncts);
        let mut folds: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut kept: Vec<Expr> = Vec::with_capacity(conjuncts.len());
        for c in conjuncts {
            match type_filter(c) {
                Some((v, types)) if foldable(&v) && !folds.contains_key(&v) => {
                    folds.insert(v, types);
                }
                _ => kept.push(c.clone()),
            }
        }
        if folds.is_empty() {
            continue;
        }
        let mut pattern2 = pattern.clone();
        for p in &mut pattern2.paths {
            for (rel, _) in &mut p.hops {
                if let Some(t) = rel.var.as_ref().and_then(|v| folds.get(v)) {
                    rel.types = t.clone();
                }
            }
        }
        let where2 = kept
            .into_iter()
            .reduce(|a, b| Expr::And(Box::new(a), Box::new(b)));
        let target = out.get_or_insert_with(|| q.clone());
        target.clauses[ci] = Clause::Match {
            optional: *optional,
            pattern: pattern2,
            where_: where2,
        };
        counted!("interp.type filter folded into its hop");
    }
    out
}

/// Fix 53: within every WHERE of the statement's MATCH and WITH clauses, an
/// operand holding a subquery, a pattern predicate or a pattern
/// comprehension (`Expr::has_subquery`) is moved AFTER the operands that
/// hold none, through every AND / OR chain of the predicate — so the
/// evaluator decides `false AND EXISTS {…}` and `true OR EXISTS {…}` from
/// the cheap side and never runs the body (the lazy connectives in
/// `engram_cypher::eval`; together, Neo4j's SelectOrSemiApply). AND and OR
/// are commutative and associative in three-valued logic and a predicate
/// has no effect, so the answer cannot move; only WHICH of two operands
/// that would both raise raises first can. The viewer-visibility listings
/// spell `(scope-test OR owner-test OR EXISTS {…} OR EXISTS {…}) AND NOT
/// w.status IN […] AND w.assigneeId = $me`: the parser's left-associative
/// tree evaluated the OR group — both membership bodies, eagerly — for
/// every work item before its assignee was compared: 3.4 s on the mirror
/// against Neo4j's 113 ms. `None` when nothing moves.
fn subqueries_last(q: &SingleQuery) -> Option<SingleQuery> {
    fn flatten<'a>(e: &'a Expr, and: bool, out: &mut Vec<&'a Expr>) {
        match e {
            Expr::And(a, b) if and => {
                flatten(a, true, out);
                flatten(b, true, out);
            }
            Expr::Or(a, b) if !and => {
                flatten(a, false, out);
                flatten(b, false, out);
            }
            _ => out.push(e),
        }
    }
    /// The reordered expression, or `None` when it is already in order
    /// (nested chains included).
    fn reorder(e: &Expr) -> Option<Expr> {
        let and = match e {
            Expr::And(..) => true,
            Expr::Or(..) => false,
            Expr::Not(x) => return reorder(x).map(|y| Expr::Not(Box::new(y))),
            _ => return None,
        };
        let mut parts = Vec::new();
        flatten(e, and, &mut parts);
        let mut changed = false;
        let items: Vec<Expr> = parts
            .iter()
            .map(|p| match reorder(p) {
                Some(n) => {
                    changed = true;
                    n
                }
                None => (*p).clone(),
            })
            .collect();
        let first_opaque = parts.iter().position(|p| p.has_subquery());
        let last_cheap = parts.iter().rposition(|p| !p.has_subquery());
        if let (Some(fo), Some(lc)) = (first_opaque, last_cheap) {
            if fo < lc {
                changed = true;
            }
        }
        if !changed {
            return None;
        }
        let (cheap, opaque): (Vec<Expr>, Vec<Expr>) =
            items.into_iter().partition(|x| !x.has_subquery());
        cheap.into_iter().chain(opaque).reduce(|a, b| {
            if and {
                Expr::And(Box::new(a), Box::new(b))
            } else {
                Expr::Or(Box::new(a), Box::new(b))
            }
        })
    }
    let mut out: Option<SingleQuery> = None;
    for (ci, clause) in q.clauses.iter().enumerate() {
        let rewritten = match clause {
            Clause::Match {
                optional,
                pattern,
                where_: Some(w),
            } => reorder(w).map(|w2| Clause::Match {
                optional: *optional,
                pattern: pattern.clone(),
                where_: Some(w2),
            }),
            Clause::With {
                proj,
                where_: Some(w),
            } => reorder(w).map(|w2| Clause::With {
                proj: proj.clone(),
                where_: Some(w2),
            }),
            _ => None,
        };
        if let Some(c) = rewritten {
            out.get_or_insert_with(|| q.clone()).clauses[ci] = c;
            counted!("interp.subquery operands ordered last");
        }
    }
    out
}

pub(crate) fn run_single(
    graph: &Graph,
    q: &SingleQuery,
    params: &BTreeMap<String, Value>,
    mut rows: Vec<Row>,
) -> Result<QueryResult, RunError> {
    // Neo4j refuses a query that concludes with a reading clause at parse
    // time; accepting one here means EXECUTING a pattern nobody projects —
    // the port benchmark measured that as a full unanchored two-hop scan of
    // the database, dead by OOM before any budget counted rows. Refuse the
    // same class, by name, before running anything.
    if let Some(last) = q.clauses.last() {
        let concluding_reader = match last {
            Clause::Match { .. } => Some("MATCH"),
            Clause::Unwind { .. } => Some("UNWIND"),
            Clause::With { .. } => Some("WITH"),
            _ => None,
        };
        if let Some(name) = concluding_reader {
            sometimes!(
                "interp.query concluding with a reading clause refused",
                true
            );
            return Err(RunError::Semantic(format!(
                "Query cannot conclude with {name} (must end with RETURN, an update clause, or a procedure call)"
            )));
        }
    }
    // Fix 45: a `type(r) IN [...]` / `type(r) = '…'` conjunct over an
    // untyped hop folds into the hop's types before ANY path sees the
    // statement — the recognisers, the fused pass and the general path
    // alike expand only the named types.
    let folded = fold_type_filters(q);
    let q: &SingleQuery = folded.as_ref().unwrap_or(q);
    // Fix 69: every var-free WHERE conjunct is evaluated ONCE here — a
    // True one leaves the WHERE, a False/Null one empties it — before any
    // recogniser reads the statement.
    let constant_folded = fold_constant_conjuncts(graph, q, params)?;
    let q: &SingleQuery = constant_folded.as_ref().unwrap_or(q);
    // Fix 53: every WHERE's subquery operands move behind its cheap ones,
    // for every path below alike.
    let ordered = subqueries_last(q);
    let q: &SingleQuery = ordered.as_ref().unwrap_or(q);
    // FIRST PASS: every recogniser sees the ORIGINAL clauses — the
    // multi-MATCH operators (the IC5 hash join and friends) keep every
    // shape they already claim, untouched.
    if let Some(r) = try_recognisers(graph, q, params)? {
        return Ok(r);
    }
    // SECOND PASS — CLAUSE FUSION (W3): when everything declined, a run of
    // consecutive plain MATCH clauses is re-offered as ONE multi-path MATCH
    // — the clause shape the single-MATCH recognisers accept, and the
    // difference between the dominant SNB read shapes streaming
    // row-at-a-time (ic6: 1,622 point gets, 422 raw prefix walks) and
    // running columnar over the adjacency tables (3 gets). STRICTLY
    // ADDITIVE admission: shapes claimed on the first pass never get here.
    // Semantics-preserving in this engine: relationship isomorphism is
    // scoped per PATH — a later comma path's first hop re-seeds the base
    // (`reset_rels`) — so `MATCH P1 MATCH P2` and `MATCH P1, P2` answer
    // identically (pinned by `tests/match_fusion.rs`, the shared-edge case
    // included); the WHEREs are ANDed, unobservable wherever the
    // recognisers admit them (pure predicates only). A decline here falls
    // to the general path with the ORIGINAL clauses.
    if let Some(f) = fuse_consecutive_matches(q) {
        counted!("interp.consecutive matches fused for the recognisers");
        if let Some(r) = try_recognisers(graph, &f, params)? {
            return Ok(r);
        }
    }
    // Fix 72: every recogniser declined — on the general path a
    // `count(<chain var>)` over the chain a MATCH binds folds into its
    // projection as `sum(COUNT { <chain> })`, so the clause never expands a
    // row per path. Behind the recognisers on purpose: the OPTIONAL
    // left-join pipeline counts its hop columnar and keeps every shape it
    // claims.
    let chain_folded = if graph.chain_count_fold_enabled() {
        fold_chain_counts(q)
    } else {
        None
    };
    let q: &SingleQuery = chain_folded.as_ref().unwrap_or(q);
    if streamable(q) {
        return run_streaming(graph, q, params, rows);
    }
    let mut result: Option<QueryResult> = None;
    // The current column names — tracked so `*` can still expand when a clause
    // leaves ZERO rows (its schema is known from the query structure regardless).
    let mut schema: Vec<String> = rows
        .first()
        .map(|r| r.keys().cloned().collect())
        .unwrap_or_default();
    for (i, clause) in q.clauses.iter().enumerate() {
        if result.is_some() {
            return Err(RunError::Semantic("RETURN must be the final clause".into()));
        }
        match clause {
            Clause::Match {
                optional,
                pattern,
                where_,
            } => {
                for p in &pattern.paths {
                    schema.extend(path_vars(p));
                }
                // Fix 51: what the REST of the statement reads of each
                // variable this MATCH binds — the matcher binds its hop
                // ends to that demand instead of in full.
                let demand = demands_after(&q.clauses[i + 1..]);
                rows = exec_match(graph, pattern, where_.as_ref(), *optional, rows, params, &demand)?;
            }
            Clause::Unwind { expr, alias } => {
                schema.push(alias.clone());
                let mut out = Vec::new();
                for row in rows {
                    match eval_expr(graph, expr, &row, params)? {
                        Value::Null => {}
                        Value::List(items) => {
                            for item in items {
                                let mut r = row.clone();
                                r.insert(alias.clone(), item);
                                out.push(r);
                            }
                            budget_check(graph, out.len())?;
                        }
                        other => {
                            return Err(RunError::Semantic(format!(
                                "UNWIND takes a list, got {}",
                                other.type_name()
                            )));
                        }
                    }
                }
                rows = out;
            }
            Clause::With { proj, where_ } => {
                let projected = project(graph, proj, rows, params, &schema)?;
                schema = projected.columns.clone();
                rows = projected.into_rows()?;
                if let Some(w) = where_ {
                    rows = filter_rows(graph, rows, w, params)?;
                }
            }
            Clause::Return { proj } => {
                let projected = project(graph, proj, std::mem::take(&mut rows), params, &schema)?;
                // A final `RETURN *` must have at least one variable in scope — an
                // empty star projects no columns, which a RETURN cannot (unlike a
                // WITH cardinality barrier).
                if projected.columns.is_empty() {
                    return Err(RunError::Semantic(
                        "RETURN * has no variables in scope (NoVariablesInScope)".into(),
                    ));
                }
                result = Some(projected.into_result());
            }
            Clause::Create { pattern } => {
                for p in &pattern.paths {
                    schema.extend(path_vars(p));
                }
                for row in &mut rows {
                    for path in &pattern.paths {
                        exec_create_path(graph, path, row, params, false)?;
                    }
                }
            }
            Clause::Merge {
                path,
                on_create,
                on_match,
            } => {
                schema.extend(path_vars(path));
                let mut out = Vec::new();
                for row in rows {
                    merge_no_null_props(graph, path, &row, params)?;
                    let matched = match_path(graph, path, &row, params, true)?;
                    if matched.is_empty() {
                        // THE RACE WINDOW, made reachable on purpose: a test
                        // hook fires here once, standing in for another writer
                        // whose commit lands between this MERGE's empty match
                        // and its create. The sim sweep cannot race real
                        // threads and stay deterministic, and the convergence
                        // below is a declared state the sweep must reach.
                        if let Some(hook) = graph.take_merge_race_hook() {
                            hook(graph);
                        }
                        let mut r = row.clone();
                        // Fix 75: a refusal an EARLIER statement left on this
                        // thread must not name the winner here.
                        let _ = graph.take_unique_refusal();
                        match exec_create_path(graph, path, &mut r, params, true) {
                            Ok(()) => {
                                sometimes!("interp.merge created", true);
                                apply_set_items(graph, on_create, &mut r, params)?;
                                out.push(r);
                            }
                            // MERGE LOST THE RACE.
                            //
                            // Two concurrent MERGEs of one value can both find
                            // nothing and both take the create arm. The loser's
                            // create then meets the winner's COMMITTED
                            // uniqueness marker and gets a ConstraintViolation —
                            // which is a hard error the Bolt retry loop does not
                            // re-run, so `MERGE` surfaced a violation to a
                            // client instead of converging. Measured at roughly
                            // 1 run in 4 of the racing-merge test.
                            //
                            // A re-MATCH is the exact discriminator, and it
                            // TERMINATES: if the value is now visible the race
                            // is over and this row takes the match arm; if it
                            // still is not, the violated constraint is about
                            // something other than the merged pattern and the
                            // violation is genuine, so it surfaces unchanged.
                            // Mapping the error to a conflict unconditionally
                            // would instead spin a real violation up to the
                            // retry bound before reporting it.
                            Err(RunError::Graph(GraphError::ConstraintViolation(why))) => {
                                // Fix 75: the re-match MISSED the winner about
                                // 1–3 % of the racing-merge test's runs (gate72
                                // under load; v135 1/91 and v136 3/86 alone):
                                // the constraint check had just proved the
                                // winner LIVE by a record read, but the
                                // re-match seeks through the scoped index,
                                // and a commit records the log entries that
                                // advance the index's epoch AFTER it publishes
                                // its rows — in that window the cached index
                                // reads as current and does not hold the
                                // winner. Two levers, in order: the refusal
                                // names the winner, so a hop-less pattern
                                // binds it BY ID and tests it with the same
                                // `node_satisfies` a match would (a record
                                // read, never a derived structure); anything
                                // else settles behind the in-flight writers
                                // — the fence a commit holds until its entries
                                // are recorded — and only then re-matches.
                                let refused = graph.take_unique_refusal();
                                let mut now: Vec<Row> = Vec::new();
                                if let (Some((false, id)), true) = (refused, path.hops.is_empty()) {
                                    if let Some(node) = graph.node(id)? {
                                        if node_satisfies(graph, &node, &path.start, &row, params)? {
                                            counted!("interp.merge converged on the refusing node by id");
                                            let mut r = row.clone();
                                            if let Some(v) = &path.start.var {
                                                r.insert(v.clone(), node);
                                            }
                                            now.push(r);
                                        }
                                    }
                                }
                                if now.is_empty() {
                                    graph.settle_in_flight_writers();
                                    now = match_path(graph, path, &row, params, true)?;
                                }
                                if now.is_empty() {
                                    return Err(RunError::Graph(
                                        GraphError::ConstraintViolation(why),
                                    ));
                                }
                                sometimes!("interp.merge converged after losing a create race", true);
                                counted!("interp.merge races converged");
                                for mut r in now {
                                    apply_set_items(graph, on_match, &mut r, params)?;
                                    out.push(r);
                                }
                            }
                            Err(e) => return Err(e),
                        }
                    } else {
                        for mut r in matched {
                            apply_set_items(graph, on_match, &mut r, params)?;
                            out.push(r);
                        }
                    }
                }
                rows = out;
            }
            Clause::Set { items } => {
                for row in &mut rows {
                    apply_set_items(graph, items, row, params)?;
                }
            }
            Clause::Remove { items } => {
                for row in &mut rows {
                    for item in items {
                        match item {
                            RemoveItem::Prop { base, key } => {
                                let target = eval_expr(graph, base, row, params)?;
                                set_entity_prop(graph, &target, key, &Value::Null)?;
                                refresh_row(graph, row)?;
                            }
                            RemoveItem::Labels { var, labels } => {
                                match row.get(var) {
                                    // openCypher IGNORES REMOVE on a null entity.
                                    Some(Value::Null) => {}
                                    Some(Value::Node { id, .. }) => {
                                        graph.remove_labels(*id, labels)?;
                                        refresh_row(graph, row)?;
                                    }
                                    _ => {
                                        return Err(RunError::Semantic(format!(
                                            "REMOVE labels needs a bound node, `{var}` is not one"
                                        )));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Clause::Delete { detach, exprs } => {
                // DELETE is idempotent WITHIN a statement: the same entity can
                // arrive on more than one row (an undirected match binds each
                // relationship in both orientations; DETACH DELETE removes a
                // relationship a later row also names), and re-deleting it is a
                // no-op, not a "does not exist" error. Other failures (a still-
                // connected node under a non-detach DELETE) still propagate.
                let swallow_missing = |r: Result<(), GraphError>| -> Result<(), RunError> {
                    match r {
                        Ok(()) | Err(GraphError::Missing(..)) => Ok(()),
                        Err(e) => Err(e.into()),
                    }
                };
                // Gather every node and relationship across ALL rows and delete
                // targets (a path/list/map deletes each entity it holds), then
                // remove RELATIONSHIPS before NODES — so a non-detach delete of a
                // node still joined by another target's relationship does not trip
                // StillConnected (`DELETE p0, p1` over two paths sharing a node).
                let (mut rel_ids, mut node_ids) = (Vec::new(), Vec::new());
                for row in &rows {
                    for e in exprs {
                        let mut stack = vec![eval_expr(graph, e, row, params)?];
                        while let Some(item) = stack.pop() {
                            match item {
                                Value::Null => {}
                                Value::Node { id, .. } => node_ids.push(id),
                                Value::Rel { id, .. } => rel_ids.push(id),
                                // A PATH deletes every node and relationship it
                                // holds (DETACH DELETE of a whole path); its trail
                                // recurses exactly like a list.
                                Value::List(inner) | Value::Path(inner) => stack.extend(inner),
                                Value::Map(m) => stack.extend(m.into_values()),
                                other => {
                                    return Err(RunError::Semantic(format!(
                                        "DELETE takes nodes or relationships, got {}",
                                        other.type_name()
                                    )));
                                }
                            }
                        }
                    }
                }
                // Mark them deleted for the rest of the statement: a later
                // `RETURN d.prop` / `labels(d)` on a snapshot still in a row must
                // raise DeletedEntityAccess, which the graph alone cannot detect.
                note_deleted(node_ids.iter().copied(), rel_ids.iter().copied());
                for id in rel_ids {
                    swallow_missing(graph.delete_rel(id))?;
                }
                for id in node_ids {
                    swallow_missing(graph.delete_node(id, *detach))?;
                }
            }
            Clause::Foreach {
                var,
                source,
                updates,
            } => {
                for row in &rows {
                    let list = match eval_expr(graph, source, row, params)? {
                        Value::Null => Vec::new(),
                        Value::List(items) => items,
                        other => {
                            return Err(RunError::Semantic(format!(
                                "FOREACH takes a list, got {}",
                                other.type_name()
                            )));
                        }
                    };
                    for item in list {
                        let mut inner = row.clone();
                        inner.insert(var.clone(), item);
                        let sub = SingleQuery {
                            clauses: updates.clone(),
                        };
                        let _ = run_single(graph, &sub, params, vec![inner])?;
                    }
                }
            }
            Clause::CallSubquery {
                query,
                in_transactions: _,
                imports: _,
            } => {
                let mut out = Vec::new();
                for row in rows {
                    let sub = run_query_seeded(graph, query, params, row.clone())?;
                    if sub.columns.is_empty() {
                        out.push(row);
                    } else {
                        for sub_row in &sub.rows {
                            let mut r = row.clone();
                            for (c, v) in sub.columns.iter().zip(sub_row) {
                                r.insert(c.clone(), v.clone());
                            }
                            out.push(r);
                        }
                    }
                }
                rows = out;
            }
            Clause::CallProcedure {
                name,
                args,
                yields,
                where_,
            } => {
                for (field, alias) in yields {
                    schema.push(alias.clone().unwrap_or_else(|| field.clone()));
                }
                rows = call_procedure(graph, name, args, yields, where_.as_ref(), rows, params)?;
            }
        }
        let _ = i;
    }
    Ok(result.unwrap_or(QueryResult {
        columns: Vec::new(),
        rows: Vec::new(),
    }))
}

fn run_query_seeded(
    graph: &Graph,
    query: &Query,
    params: &BTreeMap<String, Value>,
    seed: Row,
) -> Result<QueryResult, RunError> {
    match query {
        Query::Single(q) => run_single(graph, q, params, vec![seed]),
        Query::Union { .. } => Err(RunError::Unsupported("UNION inside CALL {}".into())),
    }
}

// ─── Expressions, with the graph hooks bound ────────────────────────────────

struct Hooks<'a> {
    graph: &'a Graph,
    params: &'a BTreeMap<String, Value>,
}

/// The both-endpoints-bound single-hop existence probe, answered without
/// the general matcher: `exists((a)-[:T]->(b))` with `a` and `b` already
/// bound nodes is an adjacency membership test, and the general path costs
/// a full pattern materialisation per evaluation. Returns None when the
/// shape is anything richer — extra hops, var-length, a path/rel variable,
/// labels or props to re-verify, an unbound endpoint — and the general
/// matcher keeps its exact semantics.
fn exists_probe_fast(
    graph: &Graph,
    pattern: &Pattern,
    row: &Row,
) -> Result<Option<bool>, RunError> {
    if pattern.paths.len() != 1 {
        return Ok(None);
    }
    let path = &pattern.paths[0];
    if path.shortest || path.var.is_some() || path.hops.len() != 1 {
        return Ok(None);
    }
    let (rel, end) = &path.hops[0];
    if rel.var.is_some() || rel.props.is_some() || rel.length.is_some() {
        return Ok(None);
    }
    if !path.start.labels.is_empty() || path.start.props.is_some() || end.props.is_some() {
        return Ok(None);
    }
    let Some(sv) = path.start.var.as_ref() else {
        return Ok(None);
    };
    let Some(Value::Node { id: a, .. }) = row.get(sv) else {
        return Ok(None); // unbound or null start: general path decides
    };
    let dir = match rel.dir {
        RelDir::Out => Dir::Out,
        RelDir::In => Dir::In,
        RelDir::Undirected => Dir::Both,
    };
    let far_bound = end.var.as_ref().and_then(|v| row.get(v));
    match far_bound {
        Some(Value::Node { id: b, .. }) => {
            if !end.labels.is_empty() {
                return Ok(None); // a bound far end with labels re-verifies: general path
            }
            let hit = graph
                .adjacency_probe(*a, dir, &rel.types, *b)
                .map_err(RunError::Graph)?;
            sometimes!("interp.exists answered by the adjacency probe", true);
            Ok(Some(hit))
        }
        Some(_) => Ok(None), // bound to null or a non-node: general path judges
        None => {
            // Unbound (or anonymous) far end, possibly labelled: any peer
            // carrying every label. `exists((e)-[:OCCURS_IN]->(:Country))`
            // measured 14 s through the general matcher, 27k probes.
            let hit = graph
                .adjacency_probe_labeled(*a, dir, &rel.types, &end.labels)
                .map_err(RunError::Graph)?;
            sometimes!("interp.exists probed a labelled far end", true);
            Ok(Some(hit))
        }
    }
}

/// The bound-start single-hop DEGREE count, answered without the general
/// matcher: `count { (n)--() }` (and its directed/typed forms) with `n`
/// bound and the far end unbound-and-bare is the size of one adjacency
/// list. The general path materialises a full row per adjacent
/// relationship — measured as the degree-histogram census statement
/// running one such count per node, 1.79M times. Any richer shape (far
/// labels or props, a bound far end, extra hops, var-length, a rel var,
/// a WHERE) declines to the general matcher.
fn count_probe_fast(graph: &Graph, pattern: &Pattern, row: &Row) -> Result<Option<i64>, RunError> {
    if pattern.paths.len() != 1 {
        return Ok(None);
    }
    let path = &pattern.paths[0];
    if path.shortest || path.var.is_some() || path.hops.len() != 1 {
        return Ok(None);
    }
    let (rel, end) = &path.hops[0];
    if rel.var.is_some() || rel.props.is_some() || rel.length.is_some() {
        return Ok(None);
    }
    if !end.labels.is_empty()
        || end.props.is_some()
        || !path.start.labels.is_empty()
        || path.start.props.is_some()
    {
        return Ok(None);
    }
    if end.var.as_ref().is_some_and(|v| row.contains_key(v)) {
        return Ok(None); // a bound far end is a pair count, not a degree
    }
    let Some(sv) = path.start.var.as_ref() else {
        return Ok(None);
    };
    let Some(Value::Node { id, .. }) = row.get(sv) else {
        return Ok(None);
    };
    let dir = match rel.dir {
        RelDir::Out => Dir::Out,
        RelDir::In => Dir::In,
        RelDir::Undirected => Dir::Both,
    };
    // Types resolve through token_peek: a COUNT is a read, and a read
    // never mints. A named type that was never minted has no edges.
    let mut types: Vec<String> = Vec::new();
    for t in &rel.types {
        if graph.type_exists(t) {
            types.push(t.clone());
        }
    }
    if !rel.types.is_empty() && types.is_empty() {
        sometimes!("interp.count answered from the adjacency list", true);
        return Ok(Some(0));
    }
    let mut tokens: Option<Vec<u32>> = None;
    if !types.is_empty() {
        let mut v: Vec<u32> = types
            .iter()
            .filter_map(|t| graph.type_token_peek(t))
            .collect();
        v.sort_unstable();
        tokens = Some(v);
    }
    let n = graph.count_adjacent_memo(*id, dir, &tokens);
    sometimes!("interp.count answered from the adjacency list", true);
    Ok(Some(n as i64))
}

/// Fix 72: the bound-start MULTI-hop path count, answered as a walk of ids
/// over the adjacency tables and never as a row per path. `COUNT {
/// (c)-[:HAS_BRANCH]->()-[:HAS_MESSAGE]->() }` with `c` bound is, for
/// every branch the first hop reaches, the DEGREE of its second hop (an
/// unlabelled end) or the number of its neighbours carrying the end's
/// labels (a membership test each). The production assistant-conversation
/// listing counted a user's 44,800 messages through the general matcher —
/// a bare-bound hop end and two expressions per message, 107 ms against
/// Neo4j's 12 — for fifty rows of one integer each.
///
/// Exact when every hop is typed with PAIRWISE-DISJOINT type sets (no
/// relationship can then occur twice on one path, so Cypher's relationship
/// isomorphism holds by construction), no hop is variable-length or
/// carries a var or props, the middles are unbound with no labels or
/// props, and the end is unbound with no props. Duplicates in the frontier
/// are kept: two relationships into the same middle are two paths. A
/// named type never minted has no edges, so the chain counts nothing.
/// Anything richer declines to the matcher. `exists` stops at the first
/// path.
fn count_chain_fast(
    graph: &Graph,
    pattern: &Pattern,
    row: &Row,
    exists: bool,
) -> Result<Option<i64>, RunError> {
    if pattern.paths.len() != 1 {
        return Ok(None);
    }
    let path = &pattern.paths[0];
    // A one-hop chain reaches here only with a labelled end (the bare
    // degree is `count_probe_fast`'s): its count is a membership test per
    // neighbour, columnar or not.
    if path.shortest || path.var.is_some() || path.hops.is_empty() {
        return Ok(None);
    }
    if !path.start.labels.is_empty() || path.start.props.is_some() {
        return Ok(None);
    }
    let Some(sv) = path.start.var.as_ref() else {
        return Ok(None);
    };
    let Some(Value::Node { id: start, .. }) = row.get(sv) else {
        return Ok(None);
    };
    let last = path.hops.len() - 1;
    let mut hops: Vec<(Dir, Option<Vec<u32>>)> = Vec::with_capacity(path.hops.len());
    for (i, (rel, node)) in path.hops.iter().enumerate() {
        if rel.var.is_some() || rel.props.is_some() || rel.length.is_some() || rel.types.is_empty()
        {
            return Ok(None);
        }
        if node.props.is_some() || node.var.as_ref().is_some_and(|v| row.contains_key(v)) {
            return Ok(None); // a bound middle or end is a join, not a walk
        }
        if path.hops[..i]
            .iter()
            .any(|(earlier, _)| earlier.types.iter().any(|t| rel.types.contains(t)))
        {
            return Ok(None); // a shared type could put one relationship on the path twice
        }
        let dir = match rel.dir {
            RelDir::Out => Dir::Out,
            RelDir::In => Dir::In,
            RelDir::Undirected => Dir::Both,
        };
        // A read never mints: a named type never seen has no edges.
        let tokens = graph.type_tokens_peek(&rel.types);
        if matches!(&tokens, Some(v) if v.is_empty()) {
            counted!("interp.count folded a multi-hop chain");
            return Ok(Some(0));
        }
        hops.push((dir, tokens));
    }
    // A labelled node on the chain is a membership test per id reached
    // (`(c)-[:HAS_BRANCH]->(:AssistantBranch)-[:HAS_MESSAGE]->(m)`).
    let mut label_sets: Vec<Vec<crate::derived::MembersView>> = Vec::with_capacity(path.hops.len());
    for (_, node) in &path.hops {
        let mut sets = Vec::with_capacity(node.labels.len());
        for l in &node.labels {
            sets.push(graph.members(Some(l)).map_err(RunError::Graph)?);
        }
        label_sets.push(sets);
    }
    let mut frontier: Vec<u64> = vec![*start];
    for (i, (dir, tokens)) in hops[..last].iter().enumerate() {
        let sets = &label_sets[i];
        let mut next: Vec<u64> = Vec::new();
        for id in &frontier {
            graph.adjacent_slim_for_each(*id, *dir, tokens, |e| {
                if sets.iter().all(|m| graph.members_contains(m, e.peer)) {
                    next.push(e.peer);
                }
            });
        }
        if next.is_empty() {
            counted!("interp.count folded a multi-hop chain");
            return Ok(Some(0));
        }
        frontier = next;
    }
    let (dir, tokens) = &hops[last];
    let sets = &label_sets[last];
    let mut n: u64 = 0;
    for id in &frontier {
        if sets.is_empty() {
            n += graph.count_adjacent_memo(*id, *dir, tokens);
        } else {
            graph.adjacent_slim_for_each(*id, *dir, tokens, |e| {
                if sets.iter().all(|m| graph.members_contains(m, e.peer)) {
                    n += 1;
                }
            });
        }
        if exists && n > 0 {
            break;
        }
    }
    counted!("interp.count folded a multi-hop chain");
    Ok(Some(n as i64))
}

/// `EXISTS { MATCH … }` and `COUNT { MATCH … }` need no RETURN: the
/// subquery's rows are the answer. A body that ends in a reading clause
/// gets a `RETURN 1` so the statement runner can run it — measured on the
/// production port as `Query cannot conclude with MATCH` from
/// `NOT EXISTS { MATCH (p)-[:HAS_VOICE]->(:VoiceProfile) }`, hidden for a
/// revision by a prefilter that dropped every row first.
fn concludable(q: &SingleQuery) -> std::borrow::Cow<'_, SingleQuery> {
    match q.clauses.last() {
        Some(Clause::Match { .. }) | Some(Clause::With { .. }) | Some(Clause::Unwind { .. }) => {
            let mut q2 = q.clone();
            q2.clauses.push(Clause::Return {
                proj: Projection {
                    distinct: false,
                    star: false,
                    items: vec![engram_cypher::stmt::ProjItem {
                        expr: Expr::Int(1),
                        alias: Some("__row".to_string()),
                        text: None,
                    }],
                    order: Vec::new(),
                    skip: None,
                    limit: None,
                },
            });
            sometimes!("interp.subquery concluded with a synthesised RETURN", true);
            std::borrow::Cow::Owned(q2)
        }
        _ => std::borrow::Cow::Borrowed(q),
    }
}

/// A subquery path DRIVEN FROM ITS CONSTANT END. `MATCH (wc:Company
/// {primaryTicker: wt.symbol})-[:SUPPLIES*1..2]-(c:Company {primaryTicker:
/// $ticker})` inside an `EXISTS {}` seeded `wc` per outer row (29 correlated
/// seeks on the mirror) and expanded SUPPLIES two hops out of each — 3,050
/// projected node reads per statement (12.6 ms) — to test an end whose map is
/// a CONSTANT on a DECLARED key and names zero or one company (Neo4j: 1.8 ms,
/// from that end). A path is reversed when its start is unbound and carries
/// no constant declared seek of its own, and its end is either already bound
/// or carries one. The reversed pattern binds the same rows — a path pattern
/// is symmetric once every hop's direction is flipped, and the end's map is
/// still tested per row at its (new) position — and a subquery consumes its
/// rows as an existence test or a count, never in order. Applied to subquery
/// bodies only, for exactly that reason. `None` = keep the path as written.
pub(crate) fn reversed_path(
    graph: &Graph,
    path: &PathPattern,
    bound: &[String],
) -> Result<Option<PathPattern>, RunError> {
    if path.var.is_some() || path.shortest || path.hops.is_empty() {
        return Ok(None);
    }
    let is_bound = |n: &engram_cypher::stmt::NodePattern| -> bool {
        n.var.as_ref().is_some_and(|v| bound.contains(v))
    };
    // A map entry whose value reads NO variable, on a key with an index
    // DECLARED for one of the node's labels: a seek done once per statement.
    let constant_seek = |n: &engram_cypher::stmt::NodePattern| -> Result<bool, RunError> {
        let Some(Expr::Map(entries)) = &n.props else {
            return Ok(false);
        };
        for (k, e) in entries {
            let mut fv = Vec::new();
            free_vars(e, &mut Vec::new(), &mut fv);
            if fv.is_empty() && graph.declared_scope_for(&n.labels, k)?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    };
    if is_bound(&path.start) || constant_seek(&path.start)? {
        return Ok(None);
    }
    let end = &path.hops[path.hops.len() - 1].1;
    if !(is_bound(end) || constant_seek(end)?) {
        return Ok(None);
    }
    let k = path.hops.len();
    let mut hops = Vec::with_capacity(k);
    for i in (0..k).rev() {
        let mut rel = path.hops[i].0.clone();
        rel.dir = match rel.dir {
            RelDir::Out => RelDir::In,
            RelDir::In => RelDir::Out,
            RelDir::Undirected => RelDir::Undirected,
        };
        let node = if i == 0 {
            path.start.clone()
        } else {
            path.hops[i - 1].1.clone()
        };
        hops.push((rel, node));
    }
    counted!("interp.subquery path reversed to its constant end");
    Ok(Some(PathPattern {
        var: None,
        shortest: false,
        start: end.clone(),
        hops,
    }))
}

/// [`reversed_path`] over a subquery body's clauses: each MATCH path is judged
/// with the names the outer row and the body's EARLIER clauses bind (a start
/// bound by a previous MATCH is the best seed there is); the first non-MATCH
/// clause ends the pass.
fn reverse_subquery_paths<'q>(
    graph: &Graph,
    q: &'q SingleQuery,
    row: &Row,
) -> Result<std::borrow::Cow<'q, SingleQuery>, RunError> {
    let mut bound: Vec<String> = row.keys().cloned().collect();
    let mut out: Option<SingleQuery> = None;
    for (ci, c) in q.clauses.iter().enumerate() {
        let Clause::Match { pattern, .. } = c else {
            break;
        };
        for (pi, path) in pattern.paths.iter().enumerate() {
            if let Some(rp) = reversed_path(graph, path, &bound)? {
                let q2 = out.get_or_insert_with(|| q.clone());
                if let Clause::Match { pattern, .. } = &mut q2.clauses[ci] {
                    pattern.paths[pi] = rp;
                }
            }
            for v in path_vars(path) {
                if !bound.contains(&v) {
                    bound.push(v);
                }
            }
        }
    }
    Ok(match out {
        Some(q2) => std::borrow::Cow::Owned(q2),
        None => std::borrow::Cow::Borrowed(q),
    })
}

/// [`reversed_path`] over a bare pattern body (`EXISTS { (a)-[…]-(b {…}) }`).
fn reverse_pattern_paths<'p>(
    graph: &Graph,
    pattern: &'p Pattern,
    row: &Row,
) -> Result<std::borrow::Cow<'p, Pattern>, RunError> {
    let mut bound: Vec<String> = row.keys().cloned().collect();
    let mut out: Option<Pattern> = None;
    for (pi, path) in pattern.paths.iter().enumerate() {
        if let Some(rp) = reversed_path(graph, path, &bound)? {
            out.get_or_insert_with(|| pattern.clone()).paths[pi] = rp;
        }
        for v in path_vars(path) {
            if !bound.contains(&v) {
                bound.push(v);
            }
        }
    }
    Ok(match out {
        Some(p) => std::borrow::Cow::Owned(p),
        None => std::borrow::Cow::Borrowed(pattern),
    })
}

impl GraphHooks for Hooks<'_> {
    fn exists(&self, body: &SubqueryBody, scope: &Scope<'_>) -> Result<Value, EvalError> {
        // A pattern-shaped body (`pattern_body`: the bare pattern, or a
        // Query whose only clause is a plain MATCH of it) takes the pattern
        // path — the adjacency fast probe, then the pattern matcher; any
        // other Query body re-enters the interpreter with the row.
        let any = match pattern_body(body) {
            Some((pattern, where_)) => {
                // Fix 51: an existence test reads nothing of the body's
                // vars beyond its own WHERE — the ends bind to that. Fix
                // 76: and the seed row carries the bound nodes trimmed to
                // that demand (plus the pattern's own inline maps).
                let mut demand = demand_of_exprs(&[where_]);
                let mut keep_full = Vec::new();
                pattern_seed_demand(&pattern.paths, &mut demand, &mut keep_full);
                let row: Row = lean_seed(self.graph, scope, &demand, &keep_full);
                if where_.is_none() {
                    if let Some(hit) =
                        exists_probe_fast(self.graph, pattern, &row).map_err(run_to_eval)?
                    {
                        return Ok(Value::Bool(hit));
                    }
                    // Fix 72: a multi-hop chain is a walk of ids, stopped
                    // at its first path.
                    if let Some(n) = count_chain_fast(self.graph, pattern, &row, true)
                        .map_err(run_to_eval)?
                    {
                        return Ok(Value::Bool(n > 0));
                    }
                }
                // Fix 70: one typed hop from a bound node, its WHERE over the
                // far end's cached columns — vectors, not a scope per row.
                if let Some(n) = crate::batch::count_hop_ends_vectorised(
                    self.graph,
                    pattern,
                    where_,
                    &row,
                    self.params,
                    true,
                )
                .map_err(run_to_eval)?
                {
                    return Ok(Value::Bool(n > 0));
                }
                let pattern =
                    reverse_pattern_paths(self.graph, pattern, &row).map_err(run_to_eval)?;
                !match_pattern_rows(self.graph, &pattern, where_, vec![row], self.params, Some(&demand))
                    .map_err(run_to_eval)?
                    .is_empty()
            }
            None => {
                let SubqueryBody::Query(q) = body else {
                    unreachable!("a bare pattern body is always pattern-shaped")
                };
                // A general body re-enters the interpreter: it may read
                // anything, so it gets the whole row.
                let row: Row = scope.materialise();
                let q = concludable(q);
                let q = reverse_subquery_paths(self.graph, &q, &row).map_err(run_to_eval)?;
                !run_single(self.graph, &q, self.params, vec![row])
                    .map_err(run_to_eval)?
                    .rows
                    .is_empty()
            }
        };
        Ok(Value::Bool(any))
    }

    fn count(&self, body: &SubqueryBody, scope: &Scope<'_>) -> Result<Value, EvalError> {
        let n = match pattern_body(body) {
            Some((pattern, where_)) => {
                // Fix 76: the seed row carries the bound nodes trimmed to
                // the body's demand (its WHERE and the pattern's maps).
                let mut demand = demand_of_exprs(&[where_]);
                let mut keep_full = Vec::new();
                pattern_seed_demand(&pattern.paths, &mut demand, &mut keep_full);
                let row: Row = lean_seed(self.graph, scope, &demand, &keep_full);
                if where_.is_none() {
                    if let Some(n) =
                        count_probe_fast(self.graph, pattern, &row).map_err(run_to_eval)?
                    {
                        return Ok(Value::Int(n));
                    }
                    // Fix 72: a multi-hop chain is a walk of ids whose last
                    // hop is a degree, never a row per path.
                    if let Some(n) = count_chain_fast(self.graph, pattern, &row, false)
                        .map_err(run_to_eval)?
                    {
                        return Ok(Value::Int(n));
                    }
                }
                // Fix 70: one typed hop from a bound node, its WHERE over the
                // far end's cached columns — vectors, not a scope per row.
                if let Some(n) = crate::batch::count_hop_ends_vectorised(
                    self.graph,
                    pattern,
                    where_,
                    &row,
                    self.params,
                    false,
                )
                .map_err(run_to_eval)?
                {
                    return Ok(Value::Int(n));
                }
                let pattern =
                    reverse_pattern_paths(self.graph, pattern, &row).map_err(run_to_eval)?;
                match_pattern_rows(self.graph, &pattern, where_, vec![row], self.params, Some(&demand))
                    .map_err(run_to_eval)?
                    .len()
            }
            None => {
                let SubqueryBody::Query(q) = body else {
                    unreachable!("a bare pattern body is always pattern-shaped")
                };
                let row: Row = scope.materialise();
                let q = concludable(q);
                let q = reverse_subquery_paths(self.graph, &q, &row).map_err(run_to_eval)?;
                run_single(self.graph, &q, self.params, vec![row])
                    .map_err(run_to_eval)?
                    .rows
                    .len()
            }
        };
        Ok(Value::Int(n as i64))
    }

    fn node_by_id(&self, id: u64) -> Result<Value, EvalError> {
        Ok(self
            .graph
            .node(id)
            .map_err(|e: GraphError| run_to_eval(e.into()))?
            .unwrap_or(Value::Null))
    }

    fn is_deleted(&self, id: u64, is_rel: bool) -> bool {
        is_deleted_entity(id, is_rel)
    }

    fn pattern_comp(
        &self,
        path: &PathPattern,
        filter: Option<&Expr>,
        map: &Expr,
        scope: &Scope<'_>,
    ) -> Result<Value, EvalError> {
        // Fix 51: the comprehension reads its vars in its filter and map
        // only. Fix 76: the pattern's inline maps join that demand, and the
        // seed row carries the bound nodes trimmed to it — the KM work-item
        // listing's two comprehensions per row each cloned the fat outer
        // node several times through the matcher (22 of 34 ms on the
        // mirror; locally the cost tracked the node's property count, 7×
        // for 12× the properties).
        let mut demand = demand_of_exprs(&[filter, Some(map)]);
        let mut keep_full = Vec::new();
        pattern_seed_demand(std::slice::from_ref(path), &mut demand, &mut keep_full);
        let row: Row = lean_seed(self.graph, scope, &demand, &keep_full);
        let matches = match_path_with(self.graph, path, &row, self.params, false, Some(&demand))
            .map_err(run_to_eval)?;
        let mut out = Vec::new();
        for m in matches {
            if let Some(f) = filter {
                let v = eval_expr(self.graph, f, &m, self.params).map_err(run_to_eval)?;
                if v.truth() != Some(Truth::True) {
                    continue;
                }
            }
            out.push(eval_expr(self.graph, map, &m, self.params).map_err(run_to_eval)?);
        }
        Ok(Value::List(out))
    }
}

/// Fix 76: what a pattern's INLINE MAPS read of the outer row (`(p:KMProject
/// {id: w.projectRef})` reads `w.projectRef`), folded into `demand`; and the
/// bound variables whose own pattern node carries a map — `node_satisfies`
/// tests such a map against the bound VALUE, so those stay whole
/// (`keep_full`).
fn pattern_seed_demand(
    paths: &[PathPattern],
    demand: &mut BTreeMap<String, VarDemand>,
    keep_full: &mut Vec<String>,
) {
    for path in paths {
        let nodes = std::iter::once(&path.start).chain(path.hops.iter().map(|(_, n)| n));
        for n in nodes {
            if let Some(p) = &n.props {
                collect_demand(p, &mut Vec::new(), demand);
                if let Some(v) = &n.var {
                    keep_full.push(v.clone());
                }
            }
        }
        for (r, _) in &path.hops {
            if let Some(p) = &r.props {
                collect_demand(p, &mut Vec::new(), demand);
            }
        }
    }
}

/// Fix 76: the seed row of a subquery body — a pattern comprehension, an
/// EXISTS / COUNT pattern body — with every bound NODE trimmed to what the
/// body reads of it: a node the body never reads a property of keeps its id
/// and labels (the pattern tests both; identity survives DISTINCT); a node
/// whose properties the body reads keeps exactly those; a node the body
/// uses whole (`| w`, `properties(w)`) stays whole, as does one whose own
/// pattern node carries an inline map (`keep_full`). `scope.materialise()`
/// copied every property of every bound node, and the general matcher then
/// cloned that row several more times per evaluation (the bound-start
/// candidate, the seed row, the candidate insert, a partial per hop): the
/// KM work-item listing's `properties(w)` bound `w` in FULL for the top-k
/// survivors, and its two pattern comprehensions per row cost 22 of the
/// statement's 34 ms on the mirror. A body with a star demand, or the
/// lever off, gets the whole row.
fn lean_seed(
    graph: &Graph,
    scope: &Scope<'_>,
    demand: &BTreeMap<String, VarDemand>,
    keep_full: &[String],
) -> Row {
    if !graph.lean_subquery_seed_enabled() || demand.contains_key(DEMAND_EVERYTHING) {
        return scope.materialise();
    }
    let mut trimmed = false;
    let mut trim = |name: &str, value: &Value| -> Value {
        if let Value::Node { id, labels, props } = value {
            if !props.is_empty() && !keep_full.iter().any(|k| k == name) {
                match demand.get(name) {
                    Some(VarDemand::Full) => {}
                    Some(VarDemand::Props(set)) => {
                        trimmed = true;
                        return Value::Node {
                            id: *id,
                            labels: labels.clone(),
                            props: props
                                .iter()
                                .filter(|(k, _)| set.contains(k.as_str()))
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect(),
                        };
                    }
                    None => {
                        trimmed = true;
                        return Value::Node {
                            id: *id,
                            labels: labels.clone(),
                            props: BTreeMap::new(),
                        };
                    }
                }
            }
        }
        value.clone()
    };
    let mut row = Row::new();
    for (n, v) in scope.vars.iter() {
        row.insert(n.clone(), trim(n, v));
    }
    for (n, v) in &scope.locals {
        row.insert(n.clone(), trim(n, v));
    }
    if trimmed {
        counted!("interp.subquery seeded with a lean row");
    }
    row
}

fn run_to_eval(e: RunError) -> EvalError {
    match e {
        RunError::Eval(e) => e,
        other => EvalError::Function {
            name: "subquery".to_string(),
            detail: other.to_string(),
        },
    }
}

fn eval_expr(
    graph: &Graph,
    expr: &Expr,
    row: &Row,
    params: &BTreeMap<String, Value>,
) -> Result<Value, RunError> {
    let scope = Scope::over(params, row, graph.wall_ms(), graph.zone_provider());
    let hooks = Hooks { graph, params };
    Ok(eval_with(expr, &scope, Some(&hooks))?)
}

fn filter_rows(
    graph: &Graph,
    rows: Vec<Row>,
    predicate: &Expr,
    params: &BTreeMap<String, Value>,
) -> Result<Vec<Row>, RunError> {
    let mut out = Vec::new();
    for row in rows {
        let v = eval_expr(graph, predicate, &row, params)?;
        match v.truth() {
            Some(Truth::True) => out.push(row),
            Some(_) => {}
            None => {
                return Err(RunError::Semantic(format!(
                    "WHERE takes a boolean, got {}",
                    v.type_name()
                )));
            }
        }
    }
    Ok(out)
}

// ─── MATCH ──────────────────────────────────────────────────────────────────

fn exec_match(
    graph: &Graph,
    pattern: &Pattern,
    where_: Option<&Expr>,
    optional: bool,
    rows: Vec<Row>,
    params: &BTreeMap<String, Value>,
    demand_after: &BTreeMap<String, VarDemand>,
) -> Result<Vec<Row>, RunError> {
    if let Some(w) = where_ {
        check_where_scope(w, pattern, &rows)?;
    }
    // The clause's own WHERE reads its rows too: merged into the demand the
    // later clauses raise.
    let mut demand = demand_after.clone();
    if let Some(w) = where_ {
        collect_demand(w, &mut Vec::new(), &mut demand);
    }
    let mut out = Vec::new();
    for row in rows {
        let matched =
            match_pattern_rows(graph, pattern, where_, vec![row.clone()], params, Some(&demand))?;
        if matched.is_empty() && optional {
            // Every variable the pattern would have introduced binds to null
            // — a row that SAYS the match found nothing, distinct from no
            // row at all.
            sometimes!("interp.optional match produced a null row", true);
            let mut r = row;
            for path in &pattern.paths {
                for v in path_vars(path) {
                    r.entry(v).or_insert(Value::Null);
                }
            }
            out.push(r);
        } else {
            out.extend(matched);
        }
    }
    Ok(out)
}

/// Fix 51: the per-row matcher over a pattern, binding each hop end to
/// `demand` (see [`match_path_with`]); `None` binds every end in full.
fn match_pattern_rows(
    graph: &Graph,
    pattern: &Pattern,
    where_: Option<&Expr>,
    mut rows: Vec<Row>,
    params: &BTreeMap<String, Value>,
    demand: Option<&BTreeMap<String, VarDemand>>,
) -> Result<Vec<Row>, RunError> {
    for path in &pattern.paths {
        let mut next = Vec::new();
        for row in rows {
            next.extend(match_path_with(graph, path, &row, params, false, demand)?);
            budget_check(graph, next.len())?;
        }
        rows = next;
    }
    if let Some(w) = where_ {
        rows = filter_rows(graph, rows, w, params)?;
    }
    Ok(rows)
}

/// Fix 51: the property demand the clauses AFTER a MATCH raise on each
/// variable — what the per-row matcher may bind a hop end to instead of the
/// full record. A bare use (`RETURN w`, a pattern reusing `w`, a subquery
/// mentioning it) demands the node in full through `collect_demand`'s own
/// rules; a clause kind this walk cannot see through (a write, a CALL, a
/// FOREACH) demands everything. The clause executor's `MATCH (p:KMProject)
/// … OPTIONAL MATCH (w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) WITH p, mm,
/// max(w.updatedAt) … RETURN … 8 COUNT {} … 2 comprehensions` materialised
/// 34,869 nodes and 16,780 relationships IN FULL per statement on the
/// mirror — every work item of every project for one property, and every
/// far end of every COUNT {} for its label — 1.1 s against Neo4j's 24 ms.
pub(crate) fn demands_after(clauses: &[Clause]) -> BTreeMap<String, VarDemand> {
    let mut demands: BTreeMap<String, VarDemand> = BTreeMap::new();
    let walk = |e: &Expr, demands: &mut BTreeMap<String, VarDemand>| {
        collect_demand(e, &mut Vec::new(), demands);
    };
    let projection = |proj: &Projection, demands: &mut BTreeMap<String, VarDemand>| -> bool {
        if proj.star {
            return false;
        }
        for it in &proj.items {
            walk(&it.expr, demands);
        }
        for o in &proj.order {
            walk(&o.expr, demands);
        }
        true
    };
    for c in clauses {
        match c {
            Clause::Match {
                pattern, where_, ..
            } => {
                for path in &pattern.paths {
                    // A node re-used as a pattern endpoint is an identity use
                    // (presence); its inline map's values are read.
                    let mut nodes = vec![&path.start];
                    for (rel, node) in &path.hops {
                        nodes.push(node);
                        if let Some(p) = &rel.props {
                            walk(p, &mut demands);
                        }
                    }
                    for node in nodes {
                        if let Some(v) = &node.var {
                            demands
                                .entry(v.clone())
                                .or_insert_with(|| VarDemand::Props(std::collections::BTreeSet::new()));
                        }
                        if let Some(p) = &node.props {
                            walk(p, &mut demands);
                        }
                    }
                }
                if let Some(w) = where_ {
                    walk(w, &mut demands);
                }
            }
            Clause::Unwind { expr, .. } => walk(expr, &mut demands),
            Clause::With { proj, where_ } => {
                if !projection(proj, &mut demands) {
                    note_full(&mut demands, DEMAND_EVERYTHING);
                    break;
                }
                if let Some(w) = where_ {
                    walk(w, &mut demands);
                }
            }
            Clause::Return { proj, .. } => {
                if !projection(proj, &mut demands) {
                    note_full(&mut demands, DEMAND_EVERYTHING);
                }
                break;
            }
            _ => {
                note_full(&mut demands, DEMAND_EVERYTHING);
                break;
            }
        }
    }
    demands
}

/// Fix 51: the demand a subquery body's own expressions raise — a pattern
/// comprehension's filter and map, an EXISTS / COUNT body's WHERE.
fn demand_of_exprs(exprs: &[Option<&Expr>]) -> BTreeMap<String, VarDemand> {
    let mut demands = BTreeMap::new();
    for e in exprs.iter().flatten() {
        collect_demand(e, &mut Vec::new(), &mut demands);
    }
    demands
}

fn path_vars(path: &PathPattern) -> Vec<String> {
    let mut vars = Vec::new();
    if let Some(v) = &path.var {
        vars.push(v.clone());
    }
    if let Some(v) = &path.start.var {
        vars.push(v.clone());
    }
    for (rel, node) in &path.hops {
        if let Some(v) = &rel.var {
            vars.push(v.clone());
        }
        if let Some(v) = &node.var {
            vars.push(v.clone());
        }
    }
    vars
}

/// MERGE forbids a NULL inline property: a node/relationship cannot be
/// MATCHed-or-CREATEd on a property whose value is null (a stored null is an
/// absent property, so the MATCH leg and the CREATE leg would disagree —
/// openCypher raises SemanticError/`MergeReadOwnWrites`). Checked per row so a
/// null `$param` is caught at runtime, not only a literal `null`.
fn merge_no_null_props(
    graph: &Graph,
    path: &PathPattern,
    row: &Row,
    params: &BTreeMap<String, Value>,
) -> Result<(), RunError> {
    let has_null = |props: &Option<Expr>| -> Result<bool, RunError> {
        let Some(p) = props else { return Ok(false) };
        Ok(match p {
            // A literal/param-valued map: any entry that evaluates to null.
            Expr::Map(entries) => {
                let mut found = false;
                for (_k, v) in entries {
                    if matches!(eval_expr(graph, v, row, params)?, Value::Null) {
                        found = true;
                        break;
                    }
                }
                found
            }
            // A whole-map parameter/expression: inspect its evaluated values.
            _ => match eval_expr(graph, p, row, params)? {
                Value::Map(m) => m.values().any(|v| matches!(v, Value::Null)),
                _ => false,
            },
        })
    };
    let bad =
        |()| RunError::Semantic("MERGE with a null property value (MergeReadOwnWrites)".into());
    if has_null(&path.start.props)? {
        return Err(bad(()));
    }
    for (rel, node) in &path.hops {
        if has_null(&rel.props)? || has_null(&node.props)? {
            return Err(bad(()));
        }
    }
    Ok(())
}

/// One partial match in progress.
#[derive(Clone)]
struct Partial {
    row: Row,
    /// Node id at the frontier.
    at: u64,
    /// Relationship ids used — Cypher's relationship-isomorphism rule: a
    /// relationship may not repeat within one path match.
    used: Vec<u64>,
    /// The path so far (nodes and rels alternating) for a path variable.
    trail: Vec<Value>,
}

fn node_satisfies(
    graph: &Graph,
    node: &Value,
    pat: &NodePattern,
    row: &Row,
    params: &BTreeMap<String, Value>,
) -> Result<bool, RunError> {
    let Value::Node { labels, props, .. } = node else {
        return Ok(false);
    };
    for l in &pat.labels {
        if !labels.contains(l) {
            return Ok(false);
        }
    }
    if let Some(pv) = &pat.props {
        let want = eval_expr(graph, pv, row, params)?;
        let Value::Map(want) = want else {
            return Err(RunError::Semantic(
                "a pattern's property map must be a map".into(),
            ));
        };
        for (k, v) in &want {
            let have = props.get(k).cloned().unwrap_or(Value::Null);
            if have.eq3(v) != Truth::True {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn rel_satisfies(
    graph: &Graph,
    rel: &crate::RelRow,
    pat_props: &Option<Expr>,
    row: &Row,
    params: &BTreeMap<String, Value>,
) -> Result<bool, RunError> {
    if let Some(pv) = pat_props {
        let want = eval_expr(graph, pv, row, params)?;
        let Value::Map(want) = want else {
            return Err(RunError::Semantic(
                "a pattern's property map must be a map".into(),
            ));
        };
        for (k, v) in &want {
            let have = rel.props.get(k).cloned().unwrap_or(Value::Null);
            if have.eq3(v) != Truth::True {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// A `shortestPath` between two ALREADY-BOUND endpoints, computed by BFS with a
/// visited-NODE set — O(reachable nodes) memory and time. It replaces the
/// enumerating `expand_var_length` (O(rel-distinct walks), which exhausts the
/// process on an unbounded `(a)-[:KNOWS*]-(b)` — the LDBC IC13 OOM), and is
/// byte-identical on LENGTH: a shortest path is node-simple, so the fewest-hops
/// BFS distance equals the fewest-rels enumeration's minimum (a node-revisiting
/// rel-distinct walk is never shorter than the node-simple path). Handles only the
/// shape IC1 and IC13 take — ONE hop, no rel var/props, `min <= 1`, START and END
/// bound to nodes in `seed`. Every other shortestPath shape returns `Ok(None)` and
/// the caller falls back to the enumerating path (unbounded-endpoint patterns like
/// the `run.rs` test, multi-hop, `min > 1`), so no accepted behaviour changes.
/// `Ok(Some(rows))` — handled: 0 rows (unreachable / a restated pattern excludes an
/// endpoint) or 1 row (the shortest path, its trail bound to the path var).
fn try_shortest_path_bfs(
    graph: &Graph,
    path: &PathPattern,
    seed: &Row,
    params: &BTreeMap<String, Value>,
) -> Result<Option<Vec<Row>>, RunError> {
    if !path.shortest || path.hops.len() != 1 {
        return Ok(None);
    }
    let (rel_pat, node_pat) = &path.hops[0];
    if rel_pat.var.is_some() || rel_pat.props.is_some() {
        return Ok(None);
    }
    let (min, max) = match rel_pat.length {
        None => return Ok(None), // a fixed hop is not this var-length shortestPath
        Some(vl) => (vl.min.unwrap_or(1), vl.max),
    };
    if min > 1 {
        return Ok(None); // the BFS distance floors at 1; min>1 falls back
    }
    // START and END must both be bound to nodes in the seed (IC1/IC13 carry both).
    let start_id = match path.start.var.as_ref().and_then(|v| seed.get(v)) {
        Some(Value::Node { id, .. }) => *id,
        _ => return Ok(None),
    };
    let end_id = match node_pat.var.as_ref().and_then(|v| seed.get(v)) {
        Some(Value::Node { id, .. }) => *id,
        _ => return Ok(None),
    };
    // start == end at length >= 1 would revisit the start, which a node-simple
    // shortest path never does — fall back so the enumeration's exact cycle
    // semantics are preserved rather than guessed.
    if start_id == end_id {
        return Ok(None);
    }
    // A restated label/prop on a bound endpoint could exclude it (a bound node
    // usually satisfies its pattern, but not necessarily) — then there is no match.
    let start_node = graph
        .node(start_id)?
        .ok_or(GraphError::Missing("node", start_id))?;
    if !node_satisfies(graph, &start_node, &path.start, seed, params)? {
        return Ok(Some(Vec::new()));
    }
    let end_node = graph
        .node(end_id)?
        .ok_or(GraphError::Missing("node", end_id))?;
    if !node_satisfies(graph, &end_node, node_pat, seed, params)? {
        return Ok(Some(Vec::new()));
    }
    let dir = match rel_pat.dir {
        RelDir::Out => Dir::Out,
        RelDir::In => Dir::In,
        RelDir::Undirected => Dir::Both,
    };
    let tokens = if rel_pat.types.is_empty() {
        None
    } else {
        graph.type_tokens_peek(&rel_pat.types)
    };
    if matches!(&tokens, Some(v) if v.is_empty()) {
        return Ok(Some(Vec::new())); // a named type never minted — no adjacency
    }

    // BOUNDED `*..max`: a single-source forward BFS from `start`, MEMOISED per
    // (source, dir, types, max). IC1's many `firstName='Ana'` seeds share source
    // p=10, so its bounded neighbourhood is built ONCE and each seed is an O(path)
    // distance lookup + reconstruction. Byte-identical on LENGTH (the BFS distance
    // is the shortest node-simple length; the trail carries that many rels — the
    // SAME property the bidirectional BFS already relies on). An UNBOUNDED `*`
    // keeps the per-pair BIDIRECTIONAL BFS below: its early-stop wins for a single
    // far pair (IC13), and a full forward BFS would be unbounded there.
    if let Some(m) = max {
        let tree = graph.forward_bfs_tree(start_id, dir, &tokens, m);
        let Some(&total_len) = tree.dist.get(&end_id) else {
            return Ok(Some(Vec::new())); // unreachable within max — no row
        };
        // Reconstruct start→end via forward parents: [start, rel, node, …, end] —
        // the SAME trail shape `expand_var_length` builds.
        let mut half: Vec<(u64, u64)> = Vec::new(); // (rel into node, node), end→start
        let mut cur = end_id;
        while cur != start_id {
            let (p, r) = tree.parent[&cur];
            half.push((r, cur));
            cur = p;
        }
        half.reverse();
        let mut trail: Vec<Value> = Vec::with_capacity((total_len as usize) * 2 + 1);
        trail.push(start_node.clone());
        for (rel_id, node_id) in half {
            let rel = graph
                .rel(rel_id)?
                .ok_or(GraphError::Missing("relationship", rel_id))?;
            trail.push(rel.to_value());
            let node = graph
                .node(node_id)?
                .ok_or(GraphError::Missing("node", node_id))?;
            trail.push(node);
        }
        let mut row = seed.clone();
        if let Some(v) = &path.var {
            row.insert(v.clone(), Value::Path(trail));
        }
        return Ok(Some(vec![row]));
    }

    // BIDIRECTIONAL BFS — expand from BOTH ends (the forward search follows `dir`;
    // the backward search the REVERSE), always advancing the SMALLER frontier,
    // until they meet. Meeting halves the explored depth — b^⌈d/2⌉ + b^⌊d/2⌋ instead
    // of b^d — which for IC1's 3-hop reach and IC13's far pair is the difference
    // between a big neighbourhood scan and a small one. Byte-identical on LENGTH:
    // the shortest node-simple path length is `fwd_dist[m] + bwd_dist[m]` at the
    // meeting node `m`; the per-side distances + parents reconstruct one such path.
    let rev_dir = match dir {
        Dir::Out => Dir::In,
        Dir::In => Dir::Out,
        Dir::Both => Dir::Both,
    };
    let mut fwd_dist: BTreeMap<u64, u64> = BTreeMap::new();
    let mut bwd_dist: BTreeMap<u64, u64> = BTreeMap::new();
    let mut fwd_parent: BTreeMap<u64, (u64, u64)> = BTreeMap::new();
    let mut bwd_parent: BTreeMap<u64, (u64, u64)> = BTreeMap::new();
    fwd_dist.insert(start_id, 0);
    bwd_dist.insert(end_id, 0);
    let mut fwd_frontier: Vec<u64> = vec![start_id];
    let mut bwd_frontier: Vec<u64> = vec![end_id];
    let mut fd: u64 = 0;
    let mut bd: u64 = 0;
    let mut best: Option<(u64, u64)> = None; // (total length, meeting node)
    loop {
        // A meeting found later is at combined depth >= fd+bd, so once that reaches
        // the current best (or the `*..max` bound) no shorter path can remain.
        if best.is_some_and(|(bt, _)| fd + bd >= bt) {
            break;
        }
        if max.is_some_and(|m| fd + bd >= m) {
            break;
        }
        let (fwd_empty, bwd_empty) = (fwd_frontier.is_empty(), bwd_frontier.is_empty());
        if fwd_empty && bwd_empty {
            break;
        }
        // Advance whichever live frontier is smaller (the work-minimising choice).
        let go_fwd = if fwd_empty {
            false
        } else if bwd_empty {
            true
        } else {
            fwd_frontier.len() <= bwd_frontier.len()
        };
        if go_fwd {
            fd += 1;
            let mut next: Vec<u64> = Vec::new();
            for &node in &fwd_frontier {
                graph.adjacent_slim_for_each(node, dir, &tokens, |e| {
                    if let std::collections::btree_map::Entry::Vacant(slot) = fwd_dist.entry(e.peer)
                    {
                        slot.insert(fd);
                        fwd_parent.insert(e.peer, (node, e.rel));
                        next.push(e.peer);
                    }
                });
            }
            budget_check(graph, fwd_dist.len() + bwd_dist.len())?;
            for &n in &next {
                if let Some(&od) = bwd_dist.get(&n) {
                    let total = fd + od;
                    if best.is_none_or(|(bt, _)| total < bt) {
                        best = Some((total, n));
                    }
                }
            }
            fwd_frontier = next;
        } else {
            bd += 1;
            let mut next: Vec<u64> = Vec::new();
            for &node in &bwd_frontier {
                graph.adjacent_slim_for_each(node, rev_dir, &tokens, |e| {
                    if let std::collections::btree_map::Entry::Vacant(slot) = bwd_dist.entry(e.peer)
                    {
                        slot.insert(bd);
                        bwd_parent.insert(e.peer, (node, e.rel));
                        next.push(e.peer);
                    }
                });
            }
            budget_check(graph, fwd_dist.len() + bwd_dist.len())?;
            for &n in &next {
                if let Some(&od) = fwd_dist.get(&n) {
                    let total = od + bd;
                    if best.is_none_or(|(bt, _)| total < bt) {
                        best = Some((total, n));
                    }
                }
            }
            bwd_frontier = next;
        }
    }
    let Some((total_len, meet)) = best else {
        return Ok(Some(Vec::new())); // unreachable within max — no row
    };
    if max.is_some_and(|m| total_len > m) {
        return Ok(Some(Vec::new())); // the shortest path is longer than `*..max`
    }

    // Reconstruct start→meet (forward parents) then meet→end (backward parents),
    // building the trail `[start_node, rel, node, …, end_node]` — the SAME shape
    // `expand_var_length` builds, so `length(path)` / `size(path)` read identically.
    let mut fwd_half: Vec<(u64, u64)> = Vec::new(); // (rel into node, node), meet→start
    let mut cur = meet;
    while cur != start_id {
        let (p, r) = fwd_parent[&cur];
        fwd_half.push((r, cur));
        cur = p;
    }
    fwd_half.reverse(); // start→…→meet
    let mut trail: Vec<Value> = Vec::with_capacity((total_len as usize) * 2 + 1);
    trail.push(start_node.clone());
    for (rel_id, node_id) in fwd_half {
        let rel = graph
            .rel(rel_id)?
            .ok_or(GraphError::Missing("relationship", rel_id))?;
        trail.push(rel.to_value());
        let node = graph
            .node(node_id)?
            .ok_or(GraphError::Missing("node", node_id))?;
        trail.push(node);
    }
    // meet→end: each backward parent points one step toward end.
    let mut cur = meet;
    while cur != end_id {
        let (p, r) = bwd_parent[&cur];
        let rel = graph
            .rel(r)?
            .ok_or(GraphError::Missing("relationship", r))?;
        trail.push(rel.to_value());
        let node = graph.node(p)?.ok_or(GraphError::Missing("node", p))?;
        trail.push(node);
        cur = p;
    }
    let mut row = seed.clone();
    if let Some(v) = &path.var {
        row.insert(v.clone(), Value::Path(trail));
    }
    Ok(Some(vec![row]))
}

/// Candidate start ids for a path whose start is NOT yet bound: an INLINE scalar
/// `{prop: val}` anchor (`(person1:Person {id:10})`) SEEKS the range index for the
/// few matching ids; everything else materialises the whole label. `node_satisfies`
/// re-checks the label + prop on each candidate, so the index ids are a sound
/// prefilter (the index is not label-scoped). The index is used only when it is
/// NOT bigger than the label scan. Without this, IC13's two endpoints scanned
/// ~10k Persons EACH (20k full node materialisations) instead of two seeks.
/// The SEEK CANDIDATES a start offers, in source order: every entry of its
/// inline `{prop: val}` map, then every `var.prop = <var-free>` /
/// `var.prop IN [..]` conjunct of the clause WHERE (`prop_eq_candidates`),
/// each evaluated against the row — a map value may read an outer bound
/// variable, which is why this runs per seed row. A key the map already
/// named is not repeated from the WHERE. Values a range index cannot order
/// (a boolean, a null, a list) are dropped here, not probed.
pub(crate) fn seek_candidates(
    graph: &Graph,
    path: &PathPattern,
    clause_where: Option<&Expr>,
    seed: &Row,
    params: &BTreeMap<String, Value>,
) -> Result<Vec<(String, Vec<Value>)>, RunError> {
    // An eval failure (an unknown parameter, say) drops the candidate rather
    // than failing the seed: the scan the caller falls back to evaluates the
    // same expression at its own position and raises the identical error —
    // or none, when the label is empty and nothing is ever tested. The
    // pipeline's `anchored_seed_ids` makes the same choice for the same
    // reason: a seek is a performance choice, never a new failure.
    let mut out: Vec<(String, Vec<Value>)> = Vec::new();
    if let Some(Expr::Map(entries)) = &path.start.props {
        for (k, e) in entries {
            let Ok(v) = eval_expr(graph, e, seed, params) else {
                continue;
            };
            if matches!(v, Value::Int(_) | Value::Float(_) | Value::Str(_)) {
                out.push((k.clone(), vec![v]));
            }
        }
    }
    if let Some(var) = path.start.var.as_deref() {
        'conj: for (k, exprs) in prop_eq_candidates(clause_where, var) {
            if out.iter().any(|(have, _)| *have == k) {
                continue;
            }
            let mut vs = Vec::with_capacity(exprs.len());
            for e in &exprs {
                let Ok(v) = eval_expr(graph, e, seed, params) else {
                    continue 'conj;
                };
                vs.push(v);
            }
            if vs
                .iter()
                .all(|v| matches!(v, Value::Int(_) | Value::Float(_) | Value::Str(_)))
            {
                out.push((k, vs));
            }
        }
    }
    Ok(out)
}

/// The MOST SELECTIVE seek among `candidates` for a start requiring `labels`:
/// the smallest id set any of them probes, under `cap`. ONE rule for the
/// general path's three seed sites (the anchored `match_path`, the streaming
/// `IndexEq` map seed, the streaming `PropEq` WHERE seed) — the same rule the
/// columnar count/projection seek (`columnar_seek_ids`) already followed.
///
/// Until this existed each site probed ONE key: the first map entry, or the
/// first WHERE equality. The production email listing, `MATCH (n:UserDataNode
/// {nodeType: 'email', userId: $userId}) WHERE n.classified = true … WITH n
/// ORDER BY n.createdAt DESC SKIP … LIMIT …`, named the 38k-row key first and
/// the 10-row key second; the first-key probe answered 38k ids, "beat" the
/// 38.5k-member label, and the stage materialised 38k full email records
/// (2.7 GB of resident growth, 1,025 ms) to page 9 of them. Neo4j read the
/// same page from its `(nodeType, userId)` index in 2.8 ms. Every shape on
/// that start paid the same way — the OPTIONAL MATCH form, the `EXISTS {}`
/// form, the pattern-comprehension form — because the seed is chosen before
/// any of them run.
///
/// Three rules, all load-bearing:
///
/// 1. **A DECLARED index is probed; an undeclared key is probed only when it
///    is the FIRST candidate** — which is exactly what each site did before,
///    so nothing that sought yesterday stops seeking. `index_probe_eq` builds
///    a partition-wide index on first probe for any property it is handed;
///    an undeclared second key would let a plan incur a build the operator
///    never asked for (3.18M rows for `id` on SF1). A declared index is the
///    operator saying which key is worth it (`declared_scope_for`).
/// 2. **Every probe is CAPPED at the best answer so far** (starting from
///    `cap`): a key that has already lost cannot cost more than the winner.
///    A composite `(nodeType, userId)` declares `nodeType` as its first
///    property, so that key IS declared and probes scoped — and stops at the
///    cap instead of extracting its 38k ids.
/// 3. **Deterministic given the data.** The determinism gate re-runs one seed
///    in two processes and compares digests; the smallest candidate set is a
///    property of the data, a tie keeps the earlier candidate, and nothing
///    here consults cache warmth or timing.
///
/// `Ok(None)` when no candidate probes under the cap — the caller keeps the
/// fallback it always had. Otherwise the winning candidate's INDEX and its
/// ids; a caller that ADOPTS a winner past the first candidate records
/// `seed_chose_later` (the plan changed), and one that discards it (the
/// probe lost to the label scan) records nothing. The ids are a CANDIDATE
/// set, never an oracle: every caller re-verifies each id against the full
/// pattern and WHERE.
pub(crate) fn best_declared_seek(
    graph: &Graph,
    labels: &[String],
    candidates: &[(String, Vec<Value>)],
    cap: usize,
) -> Result<Option<(usize, Vec<u64>)>, RunError> {
    let mut best: Option<(usize, Vec<u64>)> = None;
    for (i, (key, values)) in candidates.iter().enumerate() {
        let scoped_to = graph.declared_scope_for(labels, key)?;
        if scoped_to.is_none() && i > 0 {
            continue;
        }
        let bound = best.as_ref().map_or(cap, |(_, b)| b.len().min(cap));
        let probed = match scoped_to.as_deref() {
            Some(l) => {
                counted!("interp.seed probed a declared scoped index");
                graph.index_probe_in_scoped(key, values, Some(bound), Some(l))?
            }
            None => graph.index_probe_in(key, values, Some(bound))?,
        };
        let Some(ids) = probed else {
            continue; // over the bound, or a value the index cannot order
        };
        if best.as_ref().is_none_or(|(_, b)| ids.len() < b.len()) {
            best = Some((i, ids));
        }
    }
    // Fix 66: a start sought on TWO OR MORE declared keys (`{userId: $u,
    // status: 'open'}` over the `(userId, status)` composite) probed the
    // most selective one and re-verified every candidate by a RECORD read:
    // the Commitment listing read 10 records to answer 0 rows and the
    // repository listing 37 for 0, while Neo4j answered both from the
    // composite index without touching a record. Every other declared key
    // is probed too (up to `INTERSECT_PROBE_CAP` ids each — an index walk,
    // no record) and the candidate set is the INTERSECTION: a subset of the
    // winner's, still a candidate set the caller re-verifies, ascending as
    // before (sorted lists merged; an unsorted answer — a multi-value IN —
    // keeps the winner alone). An empty winner has nothing to narrow.
    if candidates.len() > 1 {
        if let Some((wi, ids)) = best.as_mut() {
            if !ids.is_empty() && ids.is_sorted() {
                for (i, (key, values)) in candidates.iter().enumerate() {
                    if i == *wi {
                        continue;
                    }
                    let Some(l) = graph.declared_scope_for(labels, key)? else {
                        continue;
                    };
                    let Some(other) =
                        graph.index_probe_in_scoped(key, values, Some(INTERSECT_PROBE_CAP), Some(&l))?
                    else {
                        continue;
                    };
                    if !other.is_sorted() {
                        continue;
                    }
                    intersect_sorted(ids, &other);
                    counted!("interp.seed intersected a second declared key");
                    if ids.is_empty() {
                        break;
                    }
                }
            }
        }
    }
    Ok(best)
}

/// Fix 66: how many ids a SECOND declared key may answer for the
/// intersection — an index walk of this many ids costs less than one record
/// read, and a key wider than this narrows too little to be worth it.
const INTERSECT_PROBE_CAP: usize = 16_384;

/// Keep in `ids` (ascending) only the entries also in `other` (ascending).
fn intersect_sorted(ids: &mut Vec<u64>, other: &[u64]) {
    let mut j = 0usize;
    ids.retain(|&id| {
        while j < other.len() && other[j] < id {
            j += 1;
        }
        j < other.len() && other[j] == id
    });
}

/// The adopted seek came from a candidate AFTER the first — the first-key
/// rule would have chosen differently. Recorded only when the ids are
/// actually driven, so the counter reads as "the plan changed".
fn seed_chose_later(winner: usize) {
    if winner > 0 {
        counted!("interp.seed chose a later, more selective declared key");
    }
}

fn anchored_start_candidate_ids(
    graph: &Graph,
    path: &PathPattern,
    seed: &Row,
    params: &BTreeMap<String, Value>,
) -> Result<Vec<u64>, RunError> {
    if graph.property_seek_enabled() {
        if let Some(Expr::Map(entries)) = &path.start.props {
            // MULTI-KEY: probe the declared index, then let `node_satisfies`
            // filter the remaining entries. The probe narrows the candidate
            // stream; it never decides membership — `index_probe_eq` returns
            // "a CANDIDATE set, never an oracle", and every id is re-verified
            // against the FULL pattern afterwards, so the answer is identical
            // to the scan's and so is its ORDER (both candidate sources are
            // ascending by id).
            if entries.len() > 1 && graph.pattern_map_seek_enabled() {
                // The MOST SELECTIVE declared key among the map's entries
                // (`best_declared_seek` — the one rule every general-path
                // seed site follows), probed SCOPED to the label the declared
                // index covers, which is a label this pattern requires. A
                // scoped index holds only that label's members, so the
                // candidate set stays a superset of the answer, which is what
                // lets `node_satisfies` remain the sole authority. An
                // undeclared key is never probed here: only the first
                // candidate may probe unscoped, and the first map entry of an
                // undeclared start scans the label as it always did — the
                // candidate list is built from the map alone (no WHERE), so
                // the undeclared-first probe is refused by passing the map
                // entries after a declared filter.
                let cands = seek_candidates(graph, path, None, seed, params)?;
                let declared: Vec<(String, Vec<Value>)> = {
                    let mut d = Vec::with_capacity(cands.len());
                    for (k, vs) in cands {
                        if graph.declared_scope_for(&path.start.labels, &k)?.is_some() {
                            d.push((k, vs));
                        }
                    }
                    d
                };
                let label_n = path
                    .start
                    .labels
                    .first()
                    .map(|l| graph.count_label_nodes(l));
                let cap = label_n.map_or(usize::MAX, |n| usize::try_from(n).unwrap_or(usize::MAX));
                if let Some((winner, ids)) =
                    best_declared_seek(graph, &path.start.labels, &declared, cap)?
                {
                    if label_n.is_none_or(|n| ids.len() as u64 <= n) {
                        seed_chose_later(winner);
                        counted!("interp.pattern map seeks");
                        sometimes!(
                            "interp.match_path sought a multi-key pattern map",
                            true
                        );
                        return Ok(ids);
                    }
                }
                counted!("interp.pattern map seeks declined");
                sometimes!("interp.multi-key seek declined for the label scan", true);
            }
            if entries.len() == 1 {
                let (key, val_expr) = &entries[0];
                let v = eval_expr(graph, val_expr, seed, params)?;
                if matches!(v, Value::Int(_) | Value::Float(_) | Value::Str(_)) {
                    // A DECLARED index on a label this pattern requires is
                    // probed SCOPED — the one rule every seek site follows
                    // (`Graph::declared_scope_for`); an undeclared key keeps
                    // the partition-wide probe it always had.
                    let scope = graph.declared_scope_for(&path.start.labels, key)?;
                    let label_n = path
                        .start
                        .labels
                        .first()
                        .map(|l| graph.count_label_nodes(l));
                    // Fix 71: the undeclared (partition-wide) probe is
                    // capped at the label's size + 1 — wider than that it
                    // loses to the label below and every extracted id would
                    // be dropped.
                    let probed = match scope.as_deref() {
                        Some(l) => {
                            counted!("interp.anchored seek probed a declared scoped index");
                            graph.index_probe_eq_scoped(key, &v, None, Some(l))?
                        }
                        None => {
                            let cap = label_n.map(|n| n as usize + 1);
                            let r = graph.index_probe_eq(key, &v, cap)?;
                            if r.is_none() && cap.is_some() {
                                counted!("interp.seed undeclared probe capped at the label");
                            }
                            r
                        }
                    };
                    if let Some(ids) = probed {
                        if label_n.is_none_or(|n| ids.len() as u64 <= n) {
                            sometimes!("interp.match_path seeded from a property index", true);
                            return Ok(ids);
                        }
                    }
                }
            }
        }
    }
    Ok(graph.nodes_by_label(path.start.labels.first().map(String::as_str))?)
}

fn match_path(
    graph: &Graph,
    path: &PathPattern,
    seed: &Row,
    params: &BTreeMap<String, Value>,
    for_merge: bool,
) -> Result<Vec<Row>, RunError> {
    match_path_with(graph, path, seed, params, for_merge, None)
}

/// Fix 51: the property set a node bound under `var` (with `pat_props` as
/// its inline map) needs under `demand` — `None` for the full record. The
/// rule `StreamPlan::props_for` applies to the streaming matcher, here for
/// the per-row one: a var nothing later reads binds to its labels and its
/// map's keys; a bare use, a non-literal map or an unknowable demand keeps
/// the full record.
fn demand_props_for(
    demand: Option<&BTreeMap<String, VarDemand>>,
    var: Option<&String>,
    pat_props: &Option<Expr>,
) -> Option<std::collections::BTreeSet<String>> {
    let d = demand?;
    if d.contains_key(DEMAND_EVERYTHING) {
        return None;
    }
    let mut set = std::collections::BTreeSet::new();
    if let Some(v) = var {
        match d.get(v) {
            Some(VarDemand::Full) => return None,
            Some(VarDemand::Props(p)) => set.extend(p.iter().cloned()),
            None => {}
        }
    }
    match pat_props {
        None => {}
        Some(Expr::Map(entries)) => set.extend(entries.iter().map(|(k, _)| k.clone())),
        Some(_) => return None,
    }
    Some(set)
}

/// [`match_path`] binding each hop end (and an unbound start) to `demand`
/// — the properties the rest of the statement, or the subquery body, reads
/// of it — instead of the full record; `None` is the full record everywhere
/// (MERGE, a path variable's trail). The match is byte-identical: labels
/// ride on every projection, the inline map's keys are always part of the
/// set, and a value nothing reads is never observed.
fn match_path_with(
    graph: &Graph,
    path: &PathPattern,
    seed: &Row,
    params: &BTreeMap<String, Value>,
    for_merge: bool,
    demand: Option<&BTreeMap<String, VarDemand>>,
) -> Result<Vec<Row>, RunError> {
    // A path variable exposes every trail node in full.
    let demand = if for_merge || path.var.is_some() {
        None
    } else {
        demand
    };
    // SHORTEST-PATH BFS fast path — a single var-length hop between two bound
    // endpoints (IC1/IC13). BFS is O(reachable nodes); the enumerating fallback
    // below is O(rel-distinct walks) and exhausts memory on an unbounded `*`.
    if path.shortest {
        if let Some(rows) = try_shortest_path_bfs(graph, path, seed, params)? {
            return Ok(rows);
        }
    }

    // Fix 37: a path whose START is unbound and whose LAST node is bound in
    // the row is driven from the bound end — the same reversal the streaming
    // matcher applies (`reverse_bound_end_path`), which this entry point
    // never took. It is the entry point of every pattern comprehension,
    // of an EXISTS/COUNT body the fast probe declines, and of the
    // materialising path's MATCH: the production KMWorkItem listing's
    // `[(parent:KMWorkItem)-[:HAS_EPIC|HAS_TASK|HAS_CHILD]->(w) | parent.id]`
    // scanned every work item IN FULL per output row through here — 104,853
    // record decodes and 93,036 scans for six rows, 13.6 s against Neo4j's
    // 19.6 ms, and +3.5 GB of resident set per statement. MERGE keeps the
    // written order (its absence IS the data flow); the reversed path's start
    // is bound, so the recursion takes the bound branch below.
    if !for_merge && graph.hop_reversal_enabled() {
        let start_pre_bound = path
            .start
            .var
            .as_ref()
            .is_some_and(|v| seed.contains_key(v));
        if !start_pre_bound {
            if let Some(rev) = reverse_bound_end_path(path, seed) {
                counted!("interp.path driven from its bound end");
                return match_path_with(graph, &rev, seed, params, for_merge, demand);
            }
        }
    }
    let start_props = demand_props_for(demand, path.start.var.as_ref(), &path.start.props);

    // Start candidates.
    let mut partials: Vec<Partial> = Vec::new();
    let start_bound = path.start.var.as_ref().and_then(|v| seed.get(v)).cloned();
    let candidates: Vec<Value> = match start_bound {
        // Fix 51: a bound start with no inline map to test is the row's
        // own value — its labels ride on every projection, so nothing is
        // re-read; a map re-materialises under the demand (the map's keys
        // are in it).
        Some(node @ Value::Node { .. }) if path.start.props.is_none() && demand.is_some() => {
            counted!("interp.matcher reused the bound start");
            vec![node]
        }
        Some(Value::Node { id, .. }) => {
            vec![mat_node(graph, id, start_props.as_ref())?.ok_or(GraphError::Missing("node", id))?]
        }
        Some(Value::Null) => return Ok(Vec::new()),
        Some(other) => {
            return Err(RunError::Semantic(format!(
                "`{}` is bound to a {}, not a node",
                path.start.var.as_deref().unwrap_or("?"),
                other.type_name()
            )));
        }
        None => {
            // NARROWING (default off): a candidate that `node_satisfies` will
            // reject contributes nothing but its ABSENCE, so recording it in
            // the read set is conservative rather than necessary — and on a
            // label scan that is the difference between O(label) and O(1)
            // entries for validation to walk under the global commit latch.
            //
            // MERGE is excluded unconditionally: it is the "write on the basis
            // of absence" shape, so for it the absence IS the data flow.
            let narrow = graph.read_set_bindings_only() && !for_merge;
            let ids = anchored_start_candidate_ids(graph, path, seed, params)?;
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                let got = if narrow {
                    graph.node_unrecorded(id)?
                } else {
                    mat_node(graph, id, start_props.as_ref())?
                };
                if let Some(n) = got {
                    out.push(n);
                }
            }
            out
        }
    };
    let narrow = graph.read_set_bindings_only() && !for_merge;
    for cand in candidates {
        if !node_satisfies(graph, &cand, &path.start, seed, params)? {
            if narrow {
                counted!("graph.rejected candidates kept out of the read set");
            }
            continue;
        }
        if narrow {
            // ACCEPTED: it became a binding, so it is recorded now — exactly
            // the entry `store_get_peek` would have made at materialisation.
            if let Value::Node { id, .. } = &cand {
                graph.note_node_read(*id);
            }
        }
        let Value::Node { id, .. } = &cand else {
            unreachable!("candidates are nodes")
        };
        let mut row = seed.clone();
        if let Some(v) = &path.start.var {
            row.insert(v.clone(), cand.clone());
        }
        partials.push(Partial {
            row,
            at: *id,
            used: Vec::new(),
            trail: vec![cand.clone()],
        });
    }

    // Hops.
    for (rel_pat, node_pat) in &path.hops {
        let dir = match rel_pat.dir {
            RelDir::Out => Dir::Out,
            RelDir::In => Dir::In,
            RelDir::Undirected => Dir::Both,
        };
        let types = if rel_pat.types.is_empty() {
            None
        } else {
            Some(rel_pat.types.clone())
        };
        let (min, max) = match rel_pat.length {
            None => (1, Some(1)),
            Some(vl) => (vl.min.unwrap_or(1), vl.max),
        };
        // Fix 51: the far end binds to its demand; the trail (a path
        // variable) keeps every node full, decided above.
        let peer_props = demand_props_for(demand, node_pat.var.as_ref(), &node_pat.props);
        if peer_props.is_some() {
            counted!("interp.matcher bound a hop end to its demand");
        }
        // Fix 73: a presence-only relationship variable binds lean; a
        // var-free one-key map on a declared key resolves once.
        let rel_lean = rel_pat.var.is_some()
            && matches!(
                demand_props_for(demand, rel_pat.var.as_ref(), &rel_pat.props),
                Some(ref s) if s.is_empty()
            );
        let end_set = if demand.is_some()
            && node_pat.var.as_ref().is_none_or(|v| !seed.contains_key(v))
        {
            resolve_constant_end(graph, node_pat, params)?
        } else {
            None
        };
        let end_props = if end_set.is_some() {
            demand_props_for(demand, node_pat.var.as_ref(), &None)
        } else {
            None
        };
        let mut next: Vec<Partial> = Vec::new();
        for p in partials {
            expand_var_length(
                graph,
                &p,
                dir,
                types.as_deref(),
                rel_pat,
                node_pat,
                min,
                max,
                params,
                // The trail is read by a path variable and by the shortest-
                // path pick below; nothing else walks it, and without it an
                // anonymous hop reads adjacency keys alone.
                path.var.is_some() || path.shortest || demand.is_none(),
                peer_props.as_ref(),
                rel_lean,
                end_set.as_deref().map(|v| v.as_slice()),
                end_props.as_ref(),
                &mut |p2| {
                    next.push(p2);
                    Ok(())
                },
            )?;
        }
        partials = next;
        if path.shortest && !partials.is_empty() {
            // shortestPath: the expansion below already explores shorter
            // depths first, so the FIRST completion per start row wins.
            // (Handled inside expand for var-length; single-hop is trivially
            // shortest.)
        }
    }

    let mut out = Vec::new();
    let mut shortest_len: Option<usize> = None;
    for p in partials {
        if path.shortest {
            let len = p.trail.len();
            match shortest_len {
                None => shortest_len = Some(len),
                Some(s) if len < s => {
                    shortest_len = Some(len);
                    out.clear();
                }
                Some(s) if len > s => continue,
                Some(_) => continue, // one shortest path, as Neo4j returns
            }
        }
        let mut row = p.row;
        if let Some(v) = &path.var {
            row.insert(v.clone(), Value::Path(p.trail.clone()));
        }
        out.push(row);
    }
    Ok(out)
}

/// Fix 73: a hop end with a VAR-FREE one-key map on a DECLARED key of its
/// one label — `(:User {userId: $u})` — resolved ONCE into the sorted ids
/// carrying the label and the value (`Graph::constant_end_ids`, memoised
/// on the property's epoch), so every peer of every row is a binary search
/// and never a projected record read. The CommunityPost listing tested the
/// map on 12,100 peers by a projected get each for eight rows. `None`
/// keeps the per-peer record test: a multi-key or correlated map, more
/// than one label, an undeclared key, a non-string value, a set over
/// `CONSTANT_END_CAP`, a writing transaction, or seeks switched off.
fn resolve_constant_end(
    graph: &Graph,
    node_pat: &NodePattern,
    params: &BTreeMap<String, Value>,
) -> Result<Option<std::sync::Arc<Vec<u64>>>, RunError> {
    if !graph.property_seek_enabled() || graph.in_txn_with_writes() {
        return Ok(None);
    }
    let [label] = node_pat.labels.as_slice() else {
        return Ok(None);
    };
    let Some(Expr::Map(entries)) = &node_pat.props else {
        return Ok(None);
    };
    let [(key, e)] = entries.as_slice() else {
        return Ok(None);
    };
    if e.has_subquery() {
        return Ok(None);
    }
    let mut fv = Vec::new();
    free_vars_of(e, &mut fv);
    if !fv.is_empty() {
        return Ok(None);
    }
    match graph.declared_scope_for(std::slice::from_ref(label), key)? {
        Some(l) if &l == label => {}
        _ => return Ok(None),
    }
    let v = eval_expr(graph, e, &Row::new(), params)?;
    if !matches!(v, Value::Str(_)) {
        return Ok(None);
    }
    Ok(graph.constant_end_ids(label, key, &v, crate::CONSTANT_END_CAP)?)
}

/// Fix 73: the far end of a hop, bound from its RESOLVED end set — the set
/// already proves the label and the map, so a demand of nothing beyond
/// the map is the id with its pattern label and no record; any other
/// demand binds through `mat_end` (cached columns, else a projected get)
/// without the map's keys.
fn bind_resolved_end(
    graph: &Graph,
    id: u64,
    end_props: Option<&std::collections::BTreeSet<String>>,
    node_pat: &NodePattern,
) -> Result<Value, RunError> {
    if let Some(set) = end_props {
        if set.is_empty() {
            counted!("interp.matcher bound a hop end from the resolved end set");
            return Ok(Value::Node {
                id,
                labels: node_pat.labels.clone(),
                props: BTreeMap::new(),
            });
        }
    }
    mat_end(graph, id, end_props, node_pat)?.ok_or_else(|| GraphError::Missing("node", id).into())
}

#[allow(clippy::too_many_arguments)]
fn expand_var_length(
    graph: &Graph,
    from: &Partial,
    dir: Dir,
    types: Option<&[String]>,
    rel_pat: &engram_cypher::stmt::RelPattern,
    node_pat: &NodePattern,
    min: u64,
    max: Option<u64>,
    params: &BTreeMap<String, Value>,
    want_trail: bool,
    peer_props: Option<&std::collections::BTreeSet<String>>,
    // Fix 73: the relationship variable's every read is presence-only
    // (`r IS NOT NULL`, `count(r)`): bind it LEAN from the adjacency entry.
    rel_lean: bool,
    // Fix 73: the hop end's resolved id set (sorted) and the demand on it
    // beyond the map, when `resolve_constant_end` answered.
    end_set: Option<&[u64]>,
    end_props: Option<&std::collections::BTreeSet<String>>,
    emit: &mut dyn FnMut(Partial) -> Result<(), RunError>,
) -> Result<(), RunError> {
    // Depth-first over rel-distinct walks; min..=max depths that satisfy the
    // target node pattern complete. Fixed-length hops are the (1, Some(1))
    // case of the same loop.
    struct State {
        at: u64,
        used: Vec<u64>,
        rels: Vec<Value>,
        trail: Vec<Value>,
        depth: u64,
    }
    let is_var_length = rel_pat.length.is_some();
    // If the relationship variable is ALREADY bound (carried in from a prior
    // clause — e.g. `WITH r AS r2 MATCH ()-[r2]->()`), this single hop is
    // CONSTRAINED to that exact relationship; it must not re-enumerate every
    // incident edge. `None` = not bound (normal enumeration); `Some(Some(id))`
    // = bound, match only that rel id; `Some(None)` = bound to a non-Rel value
    // (e.g. Null from an OPTIONAL) → nothing matches.
    let bound_rel_id: Option<Option<u64>> = if is_var_length {
        None
    } else {
        match rel_pat.var.as_ref().and_then(|v| from.row.get(v)) {
            Some(Value::Rel { id, .. }) => Some(Some(*id)),
            Some(_) => Some(None),
            None => None,
        }
    };
    // A VAR-LENGTH relationship variable bound to a LIST — `WITH [r1, r2] AS rs
    // MATCH (a)-[rs*]->(b)` — pins the walk to EXACTLY that ordered sequence of
    // relationships: the path has the list's length, and hop `d` must traverse
    // `rs[d]`. `Some(seq)` drives the constrained walk; a list with any non-Rel
    // element yields `Some(vec![…])` that simply won't match (a sound decline).
    let bound_rel_seq: Option<Vec<u64>> = if is_var_length {
        match rel_pat.var.as_ref().and_then(|v| from.row.get(v)) {
            Some(Value::List(items)) => Some(
                items
                    .iter()
                    .map(|v| match v {
                        Value::Rel { id, .. } => *id,
                        // A non-Rel element: an id no real edge carries, so the
                        // sequence cannot complete (the pattern matches nothing).
                        _ => u64::MAX,
                    })
                    .collect(),
            ),
            _ => None,
        }
    } else {
        None
    };
    // A bound rel-list fixes the walk length to the list's length.
    let (min, max) = match &bound_rel_seq {
        Some(seq) => (seq.len() as u64, Some(seq.len() as u64)),
        None => (min, max),
    };
    let mut stack = vec![State {
        at: from.at,
        used: from.used.clone(),
        rels: Vec::new(),
        trail: from.trail.clone(),
        depth: 0,
    }];
    let mut emitted = 0usize;
    while let Some(s) = stack.pop() {
        // The stack IS the frontier memory; completions stream to the caller, so
        // only legacy Vec callers re-accumulate them. Each frame carries a `used`
        // (and, when a trail is wanted, `trail`) vector of length ~its depth, so
        // the MEMORY is frames × depth, not frames. For a BOUNDED hop depth is
        // small and the row count is a faithful proxy (kept byte-identical). For an
        // UNBOUNDED `*` — where a path can grow to the graph diameter and a
        // non-shortestPath, non-DISTINCT enumeration would otherwise exhaust memory
        // long before the row count trips — weight the count by depth so the budget
        // bounds the actual allocation. shortestPath itself no longer reaches here
        // (it BFSes); this guards the remaining general unbounded enumeration.
        let budget_count = if max.is_none() {
            emitted.saturating_add(stack.len().saturating_mul(1 + s.depth as usize))
        } else {
            emitted + stack.len()
        };
        budget_check(graph, budget_count)?;
        // Complete here? Fix 73: a resolved end set decides the label and
        // the map by a binary search — a peer outside it never completes,
        // one inside binds without the record test.
        let in_end_set = end_set.map(|set| set.binary_search(&s.at).is_ok());
        if s.depth >= min && in_end_set != Some(false) {
            let node = if in_end_set.is_some() {
                bind_resolved_end(graph, s.at, end_props, node_pat)?
            } else {
                mat_end(graph, s.at, peer_props, node_pat)?
                    .ok_or(GraphError::Missing("node", s.at))?
            };
            let mut probe_row = from.row.clone();
            if in_end_set.is_some() || node_satisfies(graph, &node, node_pat, &probe_row, params)? {
                // Bound-target check: if the target var is already bound, the
                // frontier must BE that node.
                let target_ok = match node_pat.var.as_ref().and_then(|v| from.row.get(v)) {
                    Some(Value::Node { id, .. }) => *id == s.at,
                    Some(Value::Null) => false,
                    Some(_) => false,
                    None => true,
                };
                if target_ok {
                    if let Some(v) = &node_pat.var {
                        probe_row.insert(v.clone(), node.clone());
                    }
                    if let Some(v) = &rel_pat.var {
                        let bound = if is_var_length {
                            Value::List(s.rels.clone())
                        } else {
                            s.rels.first().cloned().unwrap_or(Value::Null)
                        };
                        probe_row.insert(v.clone(), bound);
                    }
                    emitted += 1;
                    emit(Partial {
                        row: probe_row,
                        at: s.at,
                        used: s.used.clone(),
                        trail: s.trail.clone(),
                    })?;
                }
            }
        }
        // Deeper?
        if max.is_none_or(|m| s.depth < m) {
            // SLIM expansion: no rel props to test, no rel variable to bind,
            // no trail to carry — the key bytes are the whole relationship.
            // rels_of fetched and decoded every relationship record and the
            // loop valued each one for a variable nothing read; on the
            // production port that was every anonymous `-[:T]->` hop.
            // Fix 73: a relationship VARIABLE nothing reads beyond its
            // presence takes the same slim expansion, bound LEAN — id, ends
            // and type from the adjacency entry, no record. A directed hop
            // only (an undirected visit does not say which side an entry
            // came from), fixed length, not pinned to a bound relationship.
            let lean_rel = rel_pat.var.is_some()
                && rel_lean
                && !is_var_length
                && bound_rel_id.is_none()
                && bound_rel_seq.is_none()
                && !matches!(dir, Dir::Both);
            if rel_pat.props.is_none() && (rel_pat.var.is_none() || lean_rel) && !want_trail {
                let tokens = match types {
                    Some(ts) => graph.type_tokens_peek(ts),
                    None => None,
                };
                if !matches!(&tokens, Some(v) if v.is_empty()) {
                    sometimes!("interp.expansion read only adjacency keys", true);
                    let mut type_names: BTreeMap<u32, String> = BTreeMap::new();
                    let mut name_err: Option<GraphError> = None;
                    // Zero-copy forward adjacency straight from the cached CSR
                    // slice — the enumerating oracle pushes each peer as a new
                    // stack frame; no per-node Vec is materialised.
                    graph.adjacent_slim_for_each(s.at, dir, &tokens, |e| {
                        if s.used.contains(&e.rel) {
                            return; // relationship isomorphism
                        }
                        let mut used = s.used.clone();
                        used.push(e.rel);
                        let mut rels = s.rels.clone();
                        if lean_rel {
                            let rel_type = match type_names.get(&e.type_token) {
                                Some(n) => n.clone(),
                                None => match graph.rel_type_name(e.type_token) {
                                    Ok(n) => {
                                        type_names.insert(e.type_token, n.clone());
                                        n
                                    }
                                    Err(err) => {
                                        if name_err.is_none() {
                                            name_err = Some(err);
                                        }
                                        return;
                                    }
                                },
                            };
                            let (src, dst) = match dir {
                                Dir::Out => (s.at, e.peer),
                                _ => (e.peer, s.at),
                            };
                            counted!("interp.matcher bound a lean relationship");
                            rels.push(Value::Rel {
                                id: e.rel,
                                src,
                                dst,
                                rel_type,
                                props: BTreeMap::new(),
                            });
                        }
                        stack.push(State {
                            at: e.peer,
                            used,
                            rels,
                            trail: s.trail.clone(),
                            depth: s.depth + 1,
                        });
                    });
                    if let Some(err) = name_err {
                        return Err(RunError::Graph(err));
                    }
                }
                continue;
            }
            for rel in graph.rels_of(s.at, dir, types)? {
                if s.used.contains(&rel.id) {
                    continue; // relationship isomorphism
                }
                if let Some(want) = bound_rel_id {
                    // A bound rel variable pins this hop to exactly one edge.
                    if want != Some(rel.id) {
                        continue;
                    }
                }
                if let Some(seq) = &bound_rel_seq {
                    // A bound rel-LIST pins hop `depth` to `rs[depth]`.
                    if seq.get(s.depth as usize) != Some(&rel.id) {
                        continue;
                    }
                }
                if !rel_satisfies(graph, &rel, &rel_pat.props, &from.row, params)? {
                    continue;
                }
                let peer = if rel.src == s.at { rel.dst } else { rel.src };
                // Directional legs of an undirected walk are both offered by
                // rels_of(Both); for Out/In the scan already filtered.
                // Trail nodes exist only for a path variable to read; a
                // walk nobody paths over never materialises what it passes.
                let mut trail = s.trail.clone();
                if want_trail {
                    let peer_node = graph.node(peer)?.ok_or(GraphError::Missing("node", peer))?;
                    trail.push(rel.to_value());
                    trail.push(peer_node);
                }
                let mut used = s.used.clone();
                used.push(rel.id);
                let mut rels = s.rels.clone();
                rels.push(rel.to_value());
                stack.push(State {
                    at: peer,
                    used,
                    rels,
                    trail,
                    depth: s.depth + 1,
                });
            }
        }
    }
    Ok(())
}

/// Frontier-BFS variable-length expansion — the set-at-a-time counterpart of
/// the enumerating `expand_var_length`, for a bounded `*1..max` hop whose end
/// node the breaker consumes DISTINCT-only. Each reachable node is produced
/// ONCE, at its shortest depth, so the O(paths) flat rows the enumerating path
/// builds and then collapses at the DISTINCT never exist. The caller enforces
/// the soundness conditions before choosing this path: min == 1 (a reachable
/// node's shortest depth then always lands in `1..=max`), no relationship or
/// path variable, and no relationship-property test (adjacency alone drives
/// it). The clause WHERE (e.g. `NOT a = b`) is applied by the sink downstream
/// exactly as for the enumerating path, so the start is NOT pre-excluded: it is
/// produced if genuinely re-reached and the downstream filter removes it.
#[allow(clippy::too_many_arguments)]
fn expand_var_length_bfs(
    graph: &Graph,
    from: &Partial,
    dir: Dir,
    types: Option<&[String]>,
    node_pat: &NodePattern,
    max: u64,
    params: &BTreeMap<String, Value>,
    peer_props: Option<&std::collections::BTreeSet<String>>,
    emit: &mut dyn FnMut(Partial) -> Result<(), RunError>,
) -> Result<(), RunError> {
    let tokens = match types {
        Some(ts) => graph.type_tokens_peek(ts),
        None => None,
    };
    // A named type that was never minted matches nothing.
    if matches!(&tokens, Some(v) if v.is_empty()) {
        return Ok(());
    }
    sometimes!("interp.var-length ran as a frontier BFS", true);
    // If the end variable is already bound, the only endpoint that can match is
    // that node; a non-node binding matches nothing.
    let bound_target: Option<u64> = match node_pat.var.as_ref().and_then(|v| from.row.get(v)) {
        Some(Value::Node { id, .. }) => Some(*id),
        Some(_) => return Ok(()),
        None => None,
    };
    // `seen` is the visited set — a node enters it the first time it is reached,
    // which fixes both its shortest depth and its single emission.
    let mut seen: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    let mut frontier: Vec<u64> = vec![from.at];
    let mut emitted = 0usize;
    let mut depth = 0u64;
    while depth < max && !frontier.is_empty() {
        depth += 1;
        let mut next: Vec<u64> = Vec::new();
        for &u in &frontier {
            budget_check(graph, emitted + next.len())?;
            for e in graph.adjacent_slim(u, dir, &tokens) {
                let v = e.peer;
                if !seen.insert(v) {
                    continue; // already reached at its shortest depth
                }
                if depth < max {
                    next.push(v); // room for a further hop
                }
                if let Some(t) = bound_target {
                    if v != t {
                        continue;
                    }
                }
                let node = mat_end(graph, v, peer_props, node_pat)?
                    .ok_or(GraphError::Missing("node", v))?;
                if !node_satisfies(graph, &node, node_pat, &from.row, params)? {
                    continue;
                }
                let mut row = from.row.clone();
                if let Some(vn) = &node_pat.var {
                    row.insert(vn.clone(), node);
                }
                emitted += 1;
                emit(Partial {
                    row,
                    at: v,
                    used: Vec::new(),
                    trail: Vec::new(),
                })?;
            }
        }
        frontier = next;
    }
    Ok(())
}

// ─── Streaming execution for read-only chains ───────────────────────────────
//
// The materialising loop below buffers EVERY intermediate row with fully
// decoded values — a Vec<Row> between each pair of clauses. On the full
// production port that made memory the verdict for whole statement classes:
// a 5.3M-relationship type histogram needs O(groups) state and was paying
// O(rows); a full-graph count died before it counted. Reading clauses here
// PUSH rows one at a time into a chained sink instead: aggregation folds
// into per-group accumulators, ORDER BY buffers projected values only,
// cartesian paths nest as streams. The budget now guards what the CALLER
// asked to hold — outputs, sort buffers, groups, expansion frontiers —
// never transient intermediates.
//
// Scope, deliberately: chains of MATCH / UNWIND / WITH / RETURN with no
// shortestPath. Writes, CALL, and shortest fall back to the materialising
// path unchanged — the correctness reference stays the reference.

// ── The stage planner: seeds, demands, projection pushdown ─────────────────
//
// Three decisions per streaming stage, all measured against the incumbent
// on the full-scale corpus before they existed:
//
//  - SEED: `(a:Bio:Species)` scanned Bio's 294k fat nodes to keep the
//    handful of Species (69× slower than the incumbent); the seed now picks
//    the smallest label by the count store. `()-[r:T]->()` seeded from all
//    1.79M nodes (415 SECONDS against sub-millisecond); it now drives from
//    the relationship partition.
//  - DEMAND: which properties each pattern variable actually needs, from
//    every expression in the stage. A bare use of the variable (RETURN n,
//    WITH n AS m, count(DISTINCT n)) demands the FULL node, so projection
//    can only ever widen a result, never narrow one.
//  - TRAIL: path values are built only when a path variable exists to read
//    them — otherwise variable-length walks stop materialising the nodes
//    they pass through.

/// What a stage's expressions demand of one variable.
#[derive(Clone)]
pub(crate) enum VarDemand {
    Full,
    Props(std::collections::BTreeSet<String>),
}

fn note_prop(demands: &mut BTreeMap<String, VarDemand>, var: &str, key: &str) {
    match demands
        .entry(var.to_string())
        .or_insert_with(|| VarDemand::Props(std::collections::BTreeSet::new()))
    {
        VarDemand::Full => {}
        VarDemand::Props(set) => {
            set.insert(key.to_string());
        }
    }
}

fn note_full(demands: &mut BTreeMap<String, VarDemand>, var: &str) {
    demands.insert(var.to_string(), VarDemand::Full);
}

/// The demand key meaning "every variable, in full" — a subquery body with
/// a star reads names no analysis here can enumerate.
const DEMAND_EVERYTHING: &str = "*";

/// Walk an expression recording property-only versus full-value uses of
/// each variable. Comprehension locals are excluded; subquery bodies own
/// their scopes and are treated as FULL uses of every outer variable they
/// mention (collected via `free_vars`' conservative rules is not possible
/// here — EXISTS/COUNT/pattern comprehensions re-enter the interpreter, so
/// their free variables are demanded in full).
pub(crate) fn collect_demand(
    e: &Expr,
    locals: &mut Vec<String>,
    demands: &mut BTreeMap<String, VarDemand>,
) {
    match e {
        // count(v) / count(DISTINCT v) with a BARE variable: the aggregate
        // needs the value's PRESENCE (and, under DISTINCT, its identity —
        // which the canonical key reads from the id, not the props). The
        // generic bare-var rule below would demand Full and decode every
        // property of every counted node — 1.79M full decodes on the
        // production count(m) statement, for a number.
        Expr::Call { name, args, .. } if name == "count" => match args.as_slice() {
            [Expr::Var(v)] if !locals.contains(v) => {
                demands
                    .entry(v.clone())
                    .or_insert_with(|| VarDemand::Props(std::collections::BTreeSet::new()));
            }
            _ => {
                for a in args {
                    collect_demand(a, locals, demands);
                }
            }
        },
        Expr::Prop(inner, key) => {
            if let Expr::Var(v) = inner.as_ref() {
                if !locals.contains(v) {
                    note_prop(demands, v, key);
                }
                return;
            }
            collect_demand(inner, locals, demands);
        }
        Expr::Var(v) if !locals.contains(v) => {
            note_full(demands, v);
        }
        Expr::Call { args, .. } | Expr::List(args) => {
            for a in args {
                collect_demand(a, locals, demands);
            }
        }
        Expr::Map(entries) => {
            for (_, v) in entries {
                collect_demand(v, locals, demands);
            }
        }
        Expr::Bin(_, a, b)
        | Expr::And(a, b)
        | Expr::Or(a, b)
        | Expr::Xor(a, b)
        | Expr::Index(a, b) => {
            collect_demand(a, locals, demands);
            collect_demand(b, locals, demands);
        }
        // `x IN [list]` compares by IDENTITY: `eq3` reads a node's id, never
        // its props. So a BARE variable needle needs only its id - the same
        // presence-level demand `count(x)` takes - not a full decode. The
        // list side is collected normally, and any OTHER use of `x` (a
        // property, a bare return) upgrades it through the merge. `WHERE
        // friend IN friends` over 50,698 friends was 50k FULL Person decodes
        // (every prop, including the content blob) for a membership test.
        Expr::In(needle, haystack) => {
            match needle.as_ref() {
                Expr::Var(v) if !locals.contains(v) => {
                    demands
                        .entry(v.clone())
                        .or_insert_with(|| VarDemand::Props(std::collections::BTreeSet::new()));
                }
                other => collect_demand(other, locals, demands),
            }
            collect_demand(haystack, locals, demands);
        }
        Expr::Not(a) | Expr::Neg(a) => collect_demand(a, locals, demands),
        // Fix 73: `v IS NULL` / `v IS NOT NULL` over a bare variable reads
        // its NULLNESS and nothing else — a presence demand, the same the
        // `count(v)` rule takes. `WITH p, r IS NOT NULL AS relevant` over
        // an OPTIONAL hop demanded the relationship in FULL, so every edge
        // of every post was a relationship record read (12,100 for eight
        // rows on the CommunityPost listing).
        Expr::IsNull { of, .. } => match of.as_ref() {
            Expr::Var(v) if !locals.contains(v) => {
                demands
                    .entry(v.clone())
                    .or_insert_with(|| VarDemand::Props(std::collections::BTreeSet::new()));
            }
            other => collect_demand(other, locals, demands),
        },
        Expr::Slice { of, from, to } => {
            collect_demand(of, locals, demands);
            if let Some(f) = from {
                collect_demand(f, locals, demands);
            }
            if let Some(t) = to {
                collect_demand(t, locals, demands);
            }
        }
        Expr::Case {
            subject,
            arms,
            otherwise,
        } => {
            if let Some(x) = subject {
                collect_demand(x, locals, demands);
            }
            for (w, t) in arms {
                collect_demand(w, locals, demands);
                collect_demand(t, locals, demands);
            }
            if let Some(x) = otherwise {
                collect_demand(x, locals, demands);
            }
        }
        Expr::ListComp {
            var,
            source,
            filter,
            map,
        } => {
            collect_demand(source, locals, demands);
            locals.push(var.clone());
            if let Some(f) = filter {
                collect_demand(f, locals, demands);
            }
            if let Some(m) = map {
                collect_demand(m, locals, demands);
            }
            locals.pop();
        }
        Expr::Reduce {
            acc,
            init,
            var,
            source,
            step,
        } => {
            collect_demand(init, locals, demands);
            collect_demand(source, locals, demands);
            locals.push(acc.clone());
            locals.push(var.clone());
            collect_demand(step, locals, demands);
            locals.pop();
            locals.pop();
        }
        Expr::ListPredicate {
            var,
            source,
            filter,
            ..
        } => {
            collect_demand(source, locals, demands);
            locals.push(var.clone());
            collect_demand(filter, locals, demands);
            locals.pop();
        }
        // `m:Label` reads LABELS, which every projected (slim) node carries
        // in full — walking into the bare variable would demand every
        // property for a label test (measured: the label-OR count(m)
        // statement stayed at 132 s after the aggregate-demand fix because
        // this arm re-widened it).
        Expr::HasLabels { of, .. } => match of.as_ref() {
            Expr::Var(v) if !locals.contains(v) => {
                demands
                    .entry(v.clone())
                    .or_insert_with(|| VarDemand::Props(std::collections::BTreeSet::new()));
            }
            other => collect_demand(other, locals, demands),
        },
        Expr::MapProjection { of, items } => {
            // `n {.a, k: e, .*}` — `.a` is a property read; `.*` copies
            // every property, which is a FULL demand.
            if let Expr::Var(v) = of.as_ref() {
                if !locals.contains(v) {
                    let mut full = false;
                    for it in items {
                        match it {
                            engram_cypher::ast::MapProjectionItem::Property(k) => {
                                note_prop(demands, v, k);
                            }
                            engram_cypher::ast::MapProjectionItem::AllProperties => full = true,
                            _ => {}
                        }
                    }
                    if full {
                        note_full(demands, v);
                    }
                }
            } else {
                collect_demand(of, locals, demands);
            }
            for it in items {
                if let engram_cypher::ast::MapProjectionItem::Entry(_, e) = it {
                    collect_demand(e, locals, demands);
                }
            }
        }
        // Subqueries re-enter the interpreter with the whole row: every
        // free variable inside is demanded in full, conservatively.
        // Pattern-shaped subqueries: an outer variable that appears ONLY as
        // a bare endpoint of the inner pattern is consumed by IDENTITY —
        // the expansion starts from its id and the fast paths read nothing
        // else — so it demands presence, not Full. Measured: the degree
        // histogram (`WITH n, count { (n)--() } AS d` over 1.79M nodes)
        // spent most of 670 s fully decoding and re-cloning fat nodes whose
        // properties nothing read. Endpoints carrying inner props, rel
        // variables reused from outside, comprehensions and full Query
        // bodies keep today's conservative Full.
        // A pattern-shaped body — the bare pattern, or a Query whose only
        // clause is a plain MATCH of it (`pattern_body`) — demands its
        // endpoints by identity and its WHERE's reads; `EXISTS { MATCH
        // (p)-[:HV]->() WHERE p.v IS NOT NULL }` demands `p.v` through the
        // WHERE walk exactly as the bare-pattern spelling does.
        Expr::ExistsSub(body) | Expr::CountSub(body) => match pattern_body(body) {
            Some((pattern, where_)) => {
                demand_pattern_endpoints(pattern, locals, demands);
                if let Some(w) = where_ {
                    collect_demand(w, locals, demands);
                }
            }
            // Any other Query body: `free_vars` does not look inside it, so
            // every name its clauses mention is demanded in full (an inner
            // binding demanded in full is harmless — it is not an outer
            // variable); a star inside it demands EVERYTHING in full.
            None => {
                let SubqueryBody::Query(q) = body.as_ref() else {
                    unreachable!("a bare pattern body is always pattern-shaped")
                };
                for c in &q.clauses {
                    match clause_mentions(c) {
                        Some(names) => {
                            for v in names {
                                if !locals.contains(&v) {
                                    note_full(demands, &v);
                                }
                            }
                        }
                        None => note_full(demands, DEMAND_EVERYTHING),
                    }
                }
            }
        },
        Expr::PatternPredicate(path) => {
            let one = Pattern {
                paths: vec![(**path).clone()],
            };
            demand_pattern_endpoints(&one, locals, demands);
        }
        // Fix 76: a pattern comprehension demands like an EXISTS body —
        // its endpoints by identity (a bound endpoint with an inline map in
        // full), its inline maps', filter's and map's reads walked normally.
        // The arm used to ask `free_vars`, which has no comprehension arm
        // and answered NOTHING: the outer node was bound without the
        // properties the comprehension read, so `MATCH (w:KMWorkItem {id:
        // $id}) RETURN [(w)-[:BELONGS_TO_PROJECT]->(p) | w.title][0]`
        // answered NULL and a correlated map `(p {id: w.projectRef})`
        // matched nothing — silently, since fix 51.
        Expr::PatternComp { path, filter, map } => {
            let one = Pattern {
                paths: vec![(**path).clone()],
            };
            demand_pattern_endpoints(&one, locals, demands);
            if let Some(f) = filter {
                collect_demand(f, locals, demands);
            }
            collect_demand(map, locals, demands);
        }
        _ => {}
    }
}

/// Demand for the variables an inner pattern mentions: a node endpoint
/// with no inner props is an identity use (presence); one WITH inner
/// props is matched against those props, and any expression inside the
/// props map is walked normally. Relationship variables inside a
/// subquery are its own bindings, never outer demand.
fn demand_pattern_endpoints(
    pattern: &Pattern,
    locals: &mut Vec<String>,
    demands: &mut BTreeMap<String, VarDemand>,
) {
    for path in &pattern.paths {
        let mut nodes: Vec<&engram_cypher::stmt::NodePattern> = vec![&path.start];
        for (rel, node) in &path.hops {
            nodes.push(node);
            if let Some(props) = &rel.props {
                collect_demand(props, locals, demands);
            }
        }
        for node in nodes {
            let Some(v) = node.var.as_ref() else {
                continue;
            };
            if locals.contains(v) {
                continue;
            }
            match &node.props {
                None => {
                    demands
                        .entry(v.clone())
                        .or_insert_with(|| VarDemand::Props(std::collections::BTreeSet::new()));
                }
                Some(props) => {
                    note_full(demands, v);
                    collect_demand(props, locals, demands);
                }
            }
        }
    }
}

/// How one path finds its first bindings.
enum Seed {
    /// The start variable is already bound in the row.
    Bound,
    /// Scan a label's members — the SMALLEST label when several apply.
    Label(usize),
    /// Scan every node (the shape gives nothing better).
    AllNodes,
    /// Drive from the relationship partition: a single unconstrained-start
    /// hop never visits a node it does not bind.
    Rels,
    /// Probe a derived range index with a pattern-map equality; the label
    /// scan (smallest label pre-picked) stays as the runtime fallback and
    /// WINS whenever it is the smaller candidate set.
    IndexEq {
        /// The pattern property-map key whose value the probe uses.
        key: String,
        /// The smallest label's index into `start.labels`, if any.
        label_fallback: Option<usize>,
    },
    /// A point lookup by identity: the WHERE says `elementId(n) = <expr>`
    /// or `id(n) = <expr>` with the other side never reading `n`. The
    /// expression is evaluated per row (a parameter, an UNWIND alias, an
    /// outer variable), the node is ONE get, and labels, map and WHERE
    /// still run over it. The cutover's hydrate — `UNWIND $ids AS eid
    /// MATCH (n) WHERE elementId(n) = eid` — scanned every node per id
    /// over Bolt (30 s an id on the production export).
    ById(Expr),
    /// A property-equality SEEK: the WHERE says `n.prop = <expr>` with the
    /// other side never reading `n`. The derived range index (the BTREE
    /// property index) answers the ids, filtered to the label — a SEEK, not
    /// a label scan. `WHERE c.primaryCountry = 'USA'` scanned every Company
    /// (the dominant per-statement gap vs Neo4j, which seeks a BTREE index).
    /// The label scan is the always-correct fallback and WINS when smaller.
    PropEq {
        prop: String,
        /// One value for `= x`, several for `IN [a, b, ...]`; the seek unions
        /// the per-value probes. All are variable-free (literals or params).
        values: Vec<Expr>,
        label_fallback: Option<usize>,
    },
    /// Fix 49: a top-level WHERE conjunct `EXISTS { (var)-[…]->(:L {k: $x}) }`
    /// (or the bare pattern predicate) whose far end is CONSTANT-SEEKABLE
    /// seeds `var` from the REVERSED probe — the ids the path binds to `var`
    /// when walked from that end — instead of scanning the label and probing
    /// every member. The production KM listing `MATCH (w:KMWorkItem) WHERE
    /// true AND EXISTS { (w)-[:BELONGS_TO_PROJECT]->(:KMProject {id:
    /// $projectId}) } RETURN properties(w) … ORDER BY … SKIP … LIMIT …`
    /// materialised all 15.5k work items IN FULL and opened a visitor scan
    /// per item to answer for the 63 the project's incoming edges name —
    /// 1.8–2.0 s against Neo4j's 13 ms, which seeks the project and expands.
    /// The conjunct still runs per candidate at its position; the seed is a
    /// superset filter for a positive top-level conjunct, never an answer.
    ExistsProbe {
        /// The reversed path: starts at the constant end, ends at `var`.
        path: Box<PathPattern>,
        /// The smallest label's index into `start.labels`, if any.
        label_fallback: Option<usize>,
        /// Fix 68: the WHERE conjunct the probe came from — satisfied by
        /// every start the probe names, so the clause WHERE runs without it.
        conjunct: Expr,
    },
}

/// Fix 49: the reversed probe path a top-level positive `EXISTS { … }` /
/// pattern-predicate conjunct on `var` offers as a SEED — `None` when no
/// conjunct qualifies. Qualifies: a pattern-shaped body with no inner WHERE
/// and one path that starts at a BARE `(var)` (no labels, no map — the outer
/// pattern carries those), fixed-length hops, no path variable, and an end
/// [`reversed_path`] accepts (bound, or a var-free map on a DECLARED key).
/// A `NOT EXISTS`, a disjunction or a nested use never reaches here:
/// `conjuncts_of` walks the AND spine only, and the reversal is a superset
/// filter exactly for a conjunct that must hold. A store error while asking
/// the catalogue declines the seed (the label scan answers as before).
fn exists_seed_path(
    graph: &Graph,
    where_: Option<&Expr>,
    var: &str,
) -> Option<(PathPattern, Expr)> {
    let w = where_?;
    let mut conj = Vec::new();
    conjuncts_of(w, &mut conj);
    for c in &conj {
        let path: &PathPattern = match c {
            Expr::PatternPredicate(p) => p,
            Expr::ExistsSub(body) => match pattern_body(body) {
                Some((pattern, None)) if pattern.paths.len() == 1 => &pattern.paths[0],
                _ => continue,
            },
            _ => continue,
        };
        if path.var.is_some()
            || path.shortest
            || path.hops.is_empty()
            || path.start.var.as_deref() != Some(var)
            || !path.start.labels.is_empty()
            || path.start.props.is_some()
            || path.hops.iter().any(|(rel, node)| {
                rel.length.is_some() || node.var.as_deref() == Some(var)
            })
        {
            continue;
        }
        if let Ok(Some(rp)) = reversed_path(graph, path, &[]) {
            // Fix 68: the conjunct travels with the path — a start seeded
            // from this probe satisfies it by construction, so the clause
            // WHERE drops it (`strip_conjunct`) instead of re-testing it
            // per row.
            return Some((rp, c.clone()));
        }
    }
    None
}

/// Fix 68: `where_` without every conjunct on its AND spine equal to
/// `conjunct` — `None` when nothing is left. The existence conjunct that
/// SEEDED the clause (`Seed::ExistsProbe`) holds for every start the probe
/// named: the probe walked that very pattern from its constant end, so
/// testing it again per row bought nothing — the KM listing paid an
/// adjacency read and a projected get per work item (197 each) to confirm
/// what its seed had established.
fn strip_conjunct(where_: &Expr, conjunct: &Expr) -> Option<Expr> {
    let mut conj = Vec::new();
    conjuncts_of(where_, &mut conj);
    conj.into_iter()
        .filter(|c| c != conjunct)
        .reduce(|a, b| Expr::And(Box::new(a), Box::new(b)))
}

/// A property equality the WHERE carries for `var`: `(prop, other)` from
/// `var.prop = other` / `other = var.prop`, provided `other` never reads
/// `var` and holds no subquery. The dual of `id_seek_expr`, over an
/// ordinary property rather than the identity.
pub(crate) fn prop_eq_index(where_: Option<&Expr>, var: &str) -> Option<(String, Vec<Expr>)> {
    prop_eq_candidates(where_, var).into_iter().next()
}

/// EVERY property equality the WHERE carries for `var`, in conjunct order —
/// each `(prop, values)` a seek could serve. [`prop_eq_index`] is the first
/// of these; the columnar count/projection seek consults them all and lets
/// the DECLARED index and the probe counts decide, because the first conjunct
/// is where the author put it, not the most selective one: the production
/// shape `{nodeType: 'email', userId: $u}` named the 18k-row key first and the
/// 1-row key second, and the seek probed the first and scanned the label.
pub(crate) fn prop_eq_candidates(where_: Option<&Expr>, var: &str) -> Vec<(String, Vec<Expr>)> {
    let Some(w) = where_ else {
        return Vec::new();
    };
    let mut conj = Vec::new();
    conjuncts_of(w, &mut conj);
    let var_free = |e: &Expr| -> bool {
        let mut fv = Vec::new();
        free_vars(e, &mut Vec::new(), &mut fv);
        fv.is_empty()
    };
    let mut out = Vec::new();
    for c in conj {
        if contains_opaque(&c) {
            continue;
        }
        // `n.prop = <value>` — the value must read NO variable (a literal or a
        // param). A value reading another (unbound) path variable cannot be
        // evaluated at seed time (the cartesian `a.k = b.k` case), and one
        // reading an outer bound variable is a per-row seek the memo owns.
        if let Expr::Bin(engram_cypher::BinOp::Eq, a, b) = &c {
            for (side, other) in [(a, b), (b, a)] {
                if let Expr::Prop(base, key) = side.as_ref() {
                    if matches!(base.as_ref(), Expr::Var(v) if v == var) && var_free(other) {
                        out.push((key.clone(), vec![(**other).clone()]));
                        break;
                    }
                }
            }
            continue;
        }
        // `n.prop IN [<literals>]` — the same seek over several values, unioned.
        if let Expr::In(lhs, rhs) = &c {
            if let (Expr::Prop(base, key), Expr::List(items)) = (lhs.as_ref(), rhs.as_ref()) {
                if matches!(base.as_ref(), Expr::Var(v) if v == var)
                    && !items.is_empty()
                    && items.iter().all(var_free)
                {
                    out.push((key.clone(), items.clone()));
                }
            }
        }
    }
    out
}

/// Every `var.prop STARTS WITH <var-free>` conjunct the WHERE carries, in
/// conjunct order — a PREFIX a declared range index can seek as the range
/// `[prefix, next(prefix))` (`Graph::index_probe_prefix_scoped`). The
/// production `g.eventId STARTS WITH 'edgar-8k-'` walked 44k events per
/// statement while Neo4j seeked its index.
pub(crate) fn prop_prefix_candidates(where_: Option<&Expr>, var: &str) -> Vec<(String, Expr)> {
    let Some(w) = where_ else {
        return Vec::new();
    };
    let mut conj = Vec::new();
    conjuncts_of(w, &mut conj);
    let mut out = Vec::new();
    for c in conj {
        if contains_opaque(&c) {
            continue;
        }
        if let Expr::Bin(engram_cypher::BinOp::StartsWith, a, b) = &c {
            if let Expr::Prop(base, key) = a.as_ref() {
                let mut fv = Vec::new();
                free_vars(b, &mut Vec::new(), &mut fv);
                if matches!(base.as_ref(), Expr::Var(v) if v == var) && fv.is_empty() {
                    out.push((key.clone(), (**b).clone()));
                }
            }
        }
    }
    out
}

/// Fix 47: every `var.prop <op> <value>` (and `<value> <op> var.prop`,
/// mirrored) with `<op>` one of `<`, `<=`, `>`, `>=` and a variable-free
/// value — the RANGE a declared index seeks (`columnar_seek_ids`). Neo4j's
/// composite `NewsStory(status, lastUpdatedAt)` index answers `s.status <>
/// 'stale' AND s.lastUpdatedAt > $cutoff … LIMIT 5` in 2 ms from its
/// entries; the mirror declares the same index and read the whole label.
pub(crate) fn prop_range_candidates(
    where_: Option<&Expr>,
    var: &str,
) -> Vec<(String, engram_cypher::BinOp, Expr)> {
    use engram_cypher::BinOp;
    let Some(w) = where_ else {
        return Vec::new();
    };
    let mut conj = Vec::new();
    conjuncts_of(w, &mut conj);
    let var_free = |e: &Expr| -> bool {
        let mut fv = Vec::new();
        free_vars(e, &mut Vec::new(), &mut fv);
        fv.is_empty()
    };
    let mut out = Vec::new();
    for c in conj {
        if contains_opaque(&c) {
            continue;
        }
        let Expr::Bin(op, a, b) = &c else {
            continue;
        };
        let mirrored = match op {
            BinOp::Lt => BinOp::Gt,
            BinOp::Le => BinOp::Ge,
            BinOp::Gt => BinOp::Lt,
            BinOp::Ge => BinOp::Le,
            _ => continue,
        };
        if let Expr::Prop(base, key) = a.as_ref() {
            if matches!(base.as_ref(), Expr::Var(v) if v == var) && var_free(b) {
                out.push((key.clone(), *op, (**b).clone()));
                continue;
            }
        }
        if let Expr::Prop(base, key) = b.as_ref() {
            if matches!(base.as_ref(), Expr::Var(v) if v == var) && var_free(a) {
                out.push((key.clone(), mirrored, (**a).clone()));
            }
        }
    }
    out
}

/// The identity equality a WHERE carries for `var`, if any: the other
/// side of `elementId(var) = e` / `id(var) = e` (either order), provided
/// it never reads `var` and holds no subquery.
pub(crate) fn id_seek_expr(where_: Option<&Expr>, var: &str) -> Option<Expr> {
    let w = where_?;
    let mut conj = Vec::new();
    conjuncts_of(w, &mut conj);
    for c in conj {
        if contains_opaque(&c) {
            continue;
        }
        let Expr::Bin(engram_cypher::BinOp::Eq, a, b) = &c else {
            continue;
        };
        for (side, other) in [(a, b), (b, a)] {
            let Expr::Call { name, args, .. } = side.as_ref() else {
                continue;
            };
            let lname = name.to_ascii_lowercase();
            if (lname == "elementid" || lname == "id")
                && args.len() == 1
                && matches!(&args[0], Expr::Var(v) if v == var)
                && !conjunct_reads_var(other, var)
            {
                return Some((**other).clone());
            }
        }
    }
    None
}

/// WHERE conjuncts hoisted to the earliest point their variables exist.
///
/// A pushed conjunct is a SOUND PREFILTER, never a replacement: it drops a
/// row only when it evaluates to a definite False or Unknown — outcomes
/// under which the full conjunction could never be True — and any conjunct
/// that evaluates to a non-boolean simply declines to filter, leaving the
/// original WHERE (which still runs at its legacy position) to reproduce
/// the exact error. Semantics cannot drift; only wasted work can.
struct PushedFilters {
    /// Conjuncts over variables bound BEFORE the pattern — one evaluation
    /// per input row, ahead of any scan.
    entry: Vec<Expr>,
    /// Conjuncts by the EARLIEST path index that binds them — applied as
    /// soon as that path completes, pruning later paths' fan-out.
    after_path: Vec<Vec<Expr>>,
}

struct StagePlan {
    demands: BTreeMap<String, VarDemand>,
    /// One Vec<Seed> per prefix clause (empty for non-Match clauses).
    seeds: Vec<Vec<Seed>>,
    /// Pushed WHERE prefilters per prefix clause (None for non-Match).
    filters: Vec<Option<PushedFilters>>,
    /// Fix 68: per prefix clause, the clause WHERE with the conjunct its
    /// existence-probe seed satisfies REMOVED (`Some(None)` when nothing is
    /// left); `None` where the clause keeps its own WHERE.
    probe_where: Vec<Option<Option<Expr>>>,
    /// The variables the breaker AND everything after this stage may read -
    /// used to prune dead variables out of an UNWIND fan-out. `None` when the
    /// breaker projects `*` (or is unmodelled): then everything is live.
    live_out: Option<std::collections::BTreeSet<String>>,
    /// Per MATCH-bound variable, the properties this stage binds LEAN and the
    /// top-k breaker re-materialises only for its k survivors (late
    /// projection). Empty unless the stage is a top-k with output-only props.
    deferred: BTreeMap<String, std::collections::BTreeSet<String>>,
    /// End-node variables the BREAKER consumes DISTINCT-only (`WITH DISTINCT v`
    /// or `collect/count(DISTINCT v)` with v used nowhere else). A bounded
    /// `*1..n` hop binding such a variable can run as a frontier BFS: each
    /// reached node once, the visited set IS the DISTINCT.
    frontier_vars: std::collections::BTreeSet<String>,
    /// Fix 52: the `(SKIP, LIMIT)` expressions of a PLAIN limit (no ORDER
    /// BY, no DISTINCT, no aggregate) that follows a lone, hop-less,
    /// map-less, WHERE-less one-label MATCH at the head of the statement —
    /// every start is exactly one row for the breaker, so the seed needs
    /// only the first `skip + limit` members of the label, in id order. The
    /// projector already stops the producer at that cap (`plain_cap`), but
    /// the seed's BATCH path had assembled the whole label's columns before
    /// the first row reached it: `MATCH (s:NewsStory) WITH s LIMIT 200 …`
    /// grew the mirror's pod by 1.8–5.2 GB per statement (task #116).
    seed_cap: Option<(Option<Expr>, Option<Expr>)>,
    /// Fix 56: MATCH-bound node variables the concluding top-k RETURN reads
    /// WHOLE (`properties(w)`, bare `w`) while everything before it — the
    /// pattern maps, the WHEREs, the ORDER BY — reads only properties. Bound
    /// LEAN on those key properties (their demand is rewritten to that set)
    /// and re-materialised in full by the projector for its skip+limit
    /// survivors alone. The viewer-visibility listing decoded every one of
    /// 15,494 work items in full before its WHERE ran (2.7 s against Neo4j's
    /// 116 ms) for the 200 it paged.
    late_full: std::collections::BTreeSet<String>,
}

impl StagePlan {
    /// Fix 52: the plain cap on the seed, evaluated with the statement's
    /// parameters — `None` when the stage has none.
    fn seed_cap(&self, graph: &Graph, params: &BTreeMap<String, Value>) -> Result<Option<usize>, RunError> {
        let Some((skip, limit)) = &self.seed_cap else {
            return Ok(None);
        };
        let skip = eval_count(graph, skip.as_ref(), params, "SKIP")?.unwrap_or(0);
        let limit = eval_count(graph, limit.as_ref(), params, "LIMIT")?.unwrap_or(0);
        Ok(Some(skip.saturating_add(limit)))
    }

    /// The projection set for a pattern variable: what the stage reads of
    /// it plus its own pattern-map keys; `None` means materialise in full.
    fn props_for(
        &self,
        var: Option<&String>,
        pat_props: &Option<Expr>,
    ) -> Option<std::collections::BTreeSet<String>> {
        let mut set = std::collections::BTreeSet::new();
        if self.demands.contains_key(DEMAND_EVERYTHING) {
            return None;
        }
        if let Some(v) = var {
            match self.demands.get(v) {
                Some(VarDemand::Full) => return None,
                Some(VarDemand::Props(p)) => set.extend(p.iter().cloned()),
                None => {}
            }
        }
        match pat_props {
            None => {}
            Some(Expr::Map(entries)) => set.extend(entries.iter().map(|(k, _)| k.clone())),
            // A non-literal property map (a parameter, say) — unknowable
            // keys, so the node must come in full.
            Some(_) => return None,
        }
        Some(set)
    }
}

/// Whether an expression contains a shape whose variable usage
/// `free_vars` cannot see (subqueries, pattern predicates/comprehensions).
/// Such conjuncts are never pushed: their earliest safe point is unknowable
/// statically, and the full WHERE evaluates them exactly where it always did.
pub(crate) fn contains_opaque(e: &Expr) -> bool {
    // One definition of "opaque", owned by the AST (the evaluator's lazy
    // connectives read the same one): a shape both sides classify alike.
    e.has_subquery()
}

/// Flatten an AND tree into conjuncts.
pub(crate) fn conjuncts_of(e: &Expr, out: &mut Vec<Expr>) {
    if let Expr::And(a, b) = e {
        conjuncts_of(a, out);
        conjuncts_of(b, out);
    } else {
        out.push(e.clone());
    }
}

/// How many conjuncts a WHERE has — what a set of seek equalities must equal
/// in number for the predicate to be answered from indexes alone.
pub(crate) fn conjunct_count(e: &Expr) -> usize {
    let mut v = Vec::new();
    conjuncts_of(e, &mut v);
    v.len()
}

/// A sound prefilter step: drop the row on a DEFINITE non-True; decline to
/// judge anything else (the full WHERE still runs later).
fn prefilter_pass(
    graph: &Graph,
    conjunct: &Expr,
    row: &Row,
    params: &BTreeMap<String, Value>,
) -> Result<bool, RunError> {
    match eval_expr(graph, conjunct, row, params)?.truth() {
        Some(Truth::True) | None => Ok(true),
        Some(_) => {
            sometimes!("interp.pushed conjunct pruned a row", true);
            Ok(false)
        }
    }
}

/// Every variable name a clause can READ — an over-approximation (pattern
/// variables count whether they bind or reference), which is the safe
/// direction for liveness: a name mentioned anywhere after a WITH keeps
/// that item live. `None` for clause shapes the walk does not model, so
/// liveness falls back to "everything is live".
/// `clause_mentions` for the batch module.
pub(crate) fn clause_mentions_pub(c: &Clause) -> Option<Vec<String>> {
    clause_mentions(c)
}

/// Whether clause `c` mentions `var` ONLY as `var.prop` reads, collecting the
/// props — the companion of [`clause_mentions`] that says HOW a name is used.
/// `Some(false)` on any whole-entity use (a bare occurrence, a pattern that
/// binds or reuses the name, an UNWIND alias shadowing it, a subquery);
/// `None` for a clause shape the walk cannot see through. Sites mirror
/// `clause_mentions` exactly, so a read this misses is one liveness misses.
fn clause_prop_only(
    c: &Clause,
    var: &str,
    props: &mut std::collections::BTreeSet<String>,
) -> Option<bool> {
    // `count(var)` / `count(DISTINCT var)` needs the value's PRESENCE and,
    // under DISTINCT, its identity — which the canonical key reads from the
    // id, never the props (the same rule `collect_demand` applies). So a
    // later `RETURN count(s)` is not a whole-entity use; treating it as one
    // kept the full decode on every `WITH s … RETURN count(s)` shape.
    fn prop_only(e: &Expr, var: &str, props: &mut std::collections::BTreeSet<String>) -> bool {
        match e {
            Expr::Call { name, args, .. }
                if name == "count" && matches!(args.as_slice(), [Expr::Var(v)] if v == var) =>
            {
                true
            }
            other => group_key_prop_only(other, var, props),
        }
    }
    let mut ok = true;
    match c {
        Clause::Match {
            pattern, where_, ..
        } => {
            for path in &pattern.paths {
                // A NODE endpoint carrying no props map is an IDENTITY use:
                // the expansion starts from the node's id and reads nothing
                // else (`demand_pattern_endpoints` applies the same rule to
                // a subquery's pattern). A path variable, a relationship
                // variable, or an endpoint restating a props map on `var`
                // is a whole-entity use. Until this held, `WITH n ORDER BY …
                // LIMIT … OPTIONAL MATCH (n)-[:HAS_ASK]->(a)` demanded every
                // email in FULL for the ten it paged.
                if path.var.as_deref() == Some(var) {
                    return Some(false);
                }
                let mut nodes: Vec<&engram_cypher::stmt::NodePattern> = vec![&path.start];
                for (rel, node) in &path.hops {
                    if rel.var.as_deref() == Some(var) {
                        return Some(false);
                    }
                    nodes.push(node);
                }
                if nodes
                    .iter()
                    .any(|n| n.var.as_deref() == Some(var) && n.props.is_some())
                {
                    return Some(false);
                }
                if let Some(p) = &path.start.props {
                    ok &= prop_only(p, var, props);
                }
                for (rel, node) in &path.hops {
                    if let Some(p) = &rel.props {
                        ok &= prop_only(p, var, props);
                    }
                    if let Some(p) = &node.props {
                        ok &= prop_only(p, var, props);
                    }
                }
            }
            if let Some(w) = where_ {
                ok &= prop_only(w, var, props);
            }
        }
        Clause::Unwind { expr, alias } => {
            if alias == var {
                return Some(false);
            }
            ok &= prop_only(expr, var, props);
        }
        // See-through like `clause_mentions`: a `WITH *` carries the name on
        // unchanged and the clauses after it are walked in turn. So does a
        // BARE `WITH var` (or `var AS var`): the item reads nothing of the
        // node itself — as a grouping key it is keyed by identity — and
        // what the clauses after it read is already in this walk, since
        // the caller walks every later clause. An alias (`WITH var AS w`)
        // renames the later reads out of this walk's sight and stays a
        // whole-entity use.
        Clause::With { proj, where_ } => {
            for it in &proj.items {
                let carried_bare = matches!(&it.expr, Expr::Var(v) if v == var)
                    && it.alias.as_deref().is_none_or(|a| a == var);
                if carried_bare {
                    continue;
                }
                ok &= prop_only(&it.expr, var, props);
            }
            for o in &proj.order {
                ok &= prop_only(&o.expr, var, props);
            }
            if let Some(w) = where_ {
                ok &= prop_only(w, var, props);
            }
        }
        Clause::Return { proj } => {
            if proj.star {
                return None;
            }
            for it in &proj.items {
                ok &= prop_only(&it.expr, var, props);
            }
            for o in &proj.order {
                ok &= prop_only(&o.expr, var, props);
            }
        }
        _ => return None,
    }
    Some(ok)
}

fn clause_mentions(c: &Clause) -> Option<Vec<String>> {
    let mut out = Vec::new();
    match c {
        Clause::Match {
            pattern, where_, ..
        } => {
            for path in &pattern.paths {
                out.extend(path_vars(path));
                if let Some(p) = &path.start.props {
                    free_vars(p, &mut Vec::new(), &mut out);
                }
                for (rel, node) in &path.hops {
                    if let Some(p) = &rel.props {
                        free_vars(p, &mut Vec::new(), &mut out);
                    }
                    if let Some(p) = &node.props {
                        free_vars(p, &mut Vec::new(), &mut out);
                    }
                }
            }
            if let Some(w) = where_ {
                free_vars(w, &mut Vec::new(), &mut out);
            }
        }
        Clause::Unwind { expr, alias } => {
            free_vars(expr, &mut Vec::new(), &mut out);
            out.push(alias.clone());
        }
        // A `WITH *` is SEE-THROUGH: it carries every name unchanged to the
        // clauses after it, and those clauses are walked too, so its own
        // mentions are exactly its items, ORDER BY and WHERE. It used to
        // answer None ("carries everything: all live"), which made every
        // name live AND fully demanded behind it — and the parser desugars
        // `WITH s, count(e) AS n WHERE n >= 1 ORDER BY n DESC LIMIT 5` into
        // exactly such a `WITH *` tail, so the production story tracker
        // decoded every grouped NewsStory in full for a RETURN of two
        // properties. A `RETURN *` stays opaque: it USES everything.
        Clause::With { proj, where_ } => {
            for it in &proj.items {
                free_vars(&it.expr, &mut Vec::new(), &mut out);
            }
            for o in &proj.order {
                free_vars(&o.expr, &mut Vec::new(), &mut out);
            }
            if let Some(w) = where_ {
                free_vars(w, &mut Vec::new(), &mut out);
            }
        }
        Clause::Return { proj } => {
            if proj.star {
                return None;
            }
            for it in &proj.items {
                free_vars(&it.expr, &mut Vec::new(), &mut out);
            }
            for o in &proj.order {
                free_vars(&o.expr, &mut Vec::new(), &mut out);
            }
        }
        _ => return None,
    }
    Some(out)
}

/// The row to carry into an UNWIND fan-out: only the variables the remaining
/// clauses may read (`clause_mentions` over-approximates, so this never drops
/// a live one), never the unwound alias (re-inserted per item). A
/// `collect(x) AS big UNWIND big AS y MATCH …` idiom otherwise clones the
/// whole `big` list — often full nodes — into every unwound row AND every
/// downstream candidate: O(rows x |big|), the LDBC friends-of-friends blowup
/// (SNB IC5/IC6/IC9). `None` from any remaining clause (a `*` or an
/// unmodelled shape) means everything is live — the row is carried unchanged.
fn live_carry(
    rest: &[Clause],
    live_out: Option<&std::collections::BTreeSet<String>>,
    row: &Row,
    alias: &str,
) -> Row {
    let Some(base) = live_out else {
        return row.clone(); // the breaker carries all: prune nothing
    };
    let mut live: std::collections::BTreeSet<String> = base.clone();
    for c in rest {
        match clause_mentions(c) {
            Some(vs) => live.extend(vs),
            None => return row.clone(),
        }
    }
    row.iter()
        .filter(|(k, _)| k.as_str() != alias && live.contains(k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// The output-only properties a top-k breaker can defer to its k survivors:
/// for `ORDER BY <cheap keys> LIMIT k` (no DISTINCT, no aggregate), a property
/// a MATCH-bound variable exposes ONLY through the RETURN/WITH output items -
/// never an ORDER key (aliases resolved), a WHERE, a prior clause, or the
/// pattern - is not needed until the winners are known. Empty = defer nothing.
/// The late-projection plan of a top-k stage: per MATCH-bound variable, the
/// output-only PROPERTIES deferred past the top-k (`deferred`), and — fix
/// 56, a concluding RETURN only — the variables read WHOLE by the breaker
/// but only by property before it, each with the property set those key
/// reads need (`late_full`: bound lean on that set, hydrated in full for the
/// survivors).
type LatePlan = (
    BTreeMap<String, std::collections::BTreeSet<String>>,
    BTreeMap<String, std::collections::BTreeSet<String>>,
);

fn late_deferred(
    prefix: &[Clause],
    breaker: &Clause,
    input_names: &[String],
    demands: &BTreeMap<String, VarDemand>,
) -> LatePlan {
    let empty = (BTreeMap::new(), BTreeMap::new());
    let proj = match breaker {
        Clause::Return { proj } | Clause::With { proj, .. } => proj,
        _ => return empty,
    };
    if proj.star
        || proj.distinct
        || proj.items.iter().any(|it| contains_aggregate(&it.expr))
    {
        return empty;
    }
    let topk = !proj.order.is_empty() && proj.limit.is_some();
    // A whole-node output is hydrated by the RETURN's projector; a WITH
    // breaker hands its rows to a later stage that would read the lean node.
    let concluding = matches!(breaker, Clause::Return { .. });
    // Fix 65: without a top-k there is nothing to defer PAST, and the only
    // lean binding left is a concluding RETURN's whole-node output over a
    // residual-tested single start (`residual_single_start`).
    if !topk && !concluding {
        return empty;
    }
    // NODE variables only: the projector re-materialises a deferred node
    // through `node_projected` / `graph.node`, never a relationship. A
    // relationship's deferral was harmless while every relationship was
    // bound in full; fix 73 binds a presence-only one LEAN, so deferring
    // `r.since` past the top-k would hand the projector a relationship
    // with no properties and nothing to hydrate it from.
    let mut match_bound: std::collections::BTreeSet<String> = Default::default();
    for c in prefix {
        if let Clause::Match { pattern, .. } = c {
            for path in &pattern.paths {
                let nodes = std::iter::once(&path.start).chain(path.hops.iter().map(|(_, n)| n));
                for v in nodes.filter_map(|n| n.var.as_ref()) {
                    if !input_names.contains(v) {
                        match_bound.insert(v.clone());
                    }
                }
            }
        }
    }
    if match_bound.is_empty() {
        return empty;
    }
    let mut key: BTreeMap<String, VarDemand> = BTreeMap::new();
    let walk = |e: &Expr, d: &mut BTreeMap<String, VarDemand>| {
        collect_demand(e, &mut Vec::new(), d);
    };
    for c in prefix {
        match c {
            Clause::Match {
                pattern, where_, ..
            } => {
                for path in &pattern.paths {
                    if let Some(pp) = &path.start.props {
                        walk(pp, &mut key);
                    }
                    for (rel, node) in &path.hops {
                        if let Some(pp) = &rel.props {
                            walk(pp, &mut key);
                        }
                        if let Some(pp) = &node.props {
                            walk(pp, &mut key);
                        }
                    }
                }
                if let Some(w) = where_ {
                    walk(w, &mut key);
                }
            }
            Clause::Unwind { expr, .. } => walk(expr, &mut key),
            Clause::With { proj: wp, where_ } => {
                for it in &wp.items {
                    // Fix 73: a BARE carry under its own name (`WITH p, …`)
                    // reads nothing of the node — it is the same binding
                    // handed on (the rule `clause_prop_only` applies). It
                    // demanded the node in FULL here, which kept every
                    // top-k RETURN behind such a WITH on the full seed:
                    // the CommunityPost listing decoded 4,000 posts for
                    // the eight it paged.
                    if let Expr::Var(v) = &it.expr {
                        if it.alias.as_deref().is_none_or(|a| a == v) {
                            continue;
                        }
                    }
                    walk(&it.expr, &mut key);
                }
                for o in &wp.order {
                    walk(&o.expr, &mut key);
                }
                if let Some(w) = where_ {
                    walk(w, &mut key);
                }
            }
            _ => {}
        }
    }
    if let Clause::With {
        where_: Some(w), ..
    } = breaker
    {
        walk(w, &mut key);
    }
    for o in &proj.order {
        walk(&o.expr, &mut key);
        let mut fvs = Vec::new();
        free_vars_of(&o.expr, &mut fvs);
        for fv in fvs {
            for (i, it) in proj.items.iter().enumerate() {
                let name = it
                    .alias
                    .clone()
                    .or_else(|| it.text.clone())
                    .unwrap_or_else(|| column_name(&it.expr, i));
                if name != fv {
                    continue;
                }
                // Fix 56: an ORDER BY through an alias of `properties(var)`
                // reads the SAME properties the variable would — `RETURN
                // properties(w) AS w … ORDER BY w.updatedAt` reads
                // `updatedAt` of w, not the whole node. Walking the aliased
                // item demanded the node in full for the key and kept every
                // listing of this spelling on the full decode. A BARE carry
                // (`WITH n … ORDER BY n.createdAt`) keeps the whole-node key
                // read it always had: its lean column binding already holds
                // every property the stage reads, and treating the key as a
                // property set would defer the output props past the top-k
                // into a point read per survivor (`lean_starts_from_columns`).
                let base = match &it.expr {
                    Expr::Call { name, args, .. } if name.eq_ignore_ascii_case("properties") => {
                        match args.as_slice() {
                            [Expr::Var(v)] => Some(v),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                match base {
                    Some(v) => {
                        let mut props = std::collections::BTreeSet::new();
                        if group_key_prop_only(&o.expr, &name, &mut props) {
                            for p in &props {
                                note_prop(&mut key, v, p);
                            }
                        } else {
                            note_full(&mut key, v);
                        }
                    }
                    None => walk(&it.expr, &mut key),
                }
            }
        }
    }
    let mut deferred: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    let mut late_full: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    for var in &match_bound {
        if let Some(VarDemand::Full) = key.get(var) {
            continue; // something before the breaker reads it whole
        }
        match demands.get(var) {
            Some(VarDemand::Props(full)) if topk => {
                let keyset: std::collections::BTreeSet<&String> = match key.get(var) {
                    Some(VarDemand::Props(sset)) => sset.iter().collect(),
                    _ => Default::default(),
                };
                let d: std::collections::BTreeSet<String> = full
                    .iter()
                    .filter(|pp| !keyset.contains(pp))
                    .cloned()
                    .collect();
                if !d.is_empty() {
                    deferred.insert(var.clone(), d);
                }
            }
            // Fix 56: the breaker alone reads it WHOLE — bind the key
            // properties, hydrate the survivors. Fix 65: without a top-k
            // the same holds for a residual-tested single start — the
            // projector hydrates each survivor as it arrives.
            Some(VarDemand::Full)
                if concluding && (topk || residual_single_start(prefix, input_names, var)) =>
            {
                if !topk {
                    counted!("interp.stage bound a whole-node output lean for its residual");
                }
                let keyset: std::collections::BTreeSet<String> = match key.get(var) {
                    Some(VarDemand::Props(sset)) => sset.clone(),
                    _ => Default::default(),
                };
                late_full.insert(var.clone(), keyset);
            }
            _ => {}
        }
    }
    (deferred, late_full)
}

/// Fix 65: whether `var` is the start of the prefix's ONLY pattern — a
/// hop-less, non-optional single node with no path variable, no UNWIND
/// beside it — with a test left on it past the seek: two or more
/// equalities (a map key, or a `var.p = <constant>` conjunct; the seek
/// takes one) or any other conjunct reading it. Binding such a start lean
/// and hydrating its survivors reads fewer records than binding every seek
/// candidate in full: the repository listing (`MATCH (n:UserDataNode
/// {userId: $userId, nodeType: 'repository'}) RETURN properties(n) AS n
/// ORDER BY n.createdAt DESC`) decoded 120 candidates in full for its 14
/// rows. A start every candidate of which survives — `MATCH (r:ManagedRepo)
/// RETURN properties(r)`, a sole unique-key seek — would pay a second read
/// per row, and a hop or a second pattern would hydrate per OUTPUT row, one
/// start many times; both stay on the full seed.
fn residual_single_start(prefix: &[Clause], input_names: &[String], var: &str) -> bool {
    if input_names.iter().any(|n| n == var) {
        return false;
    }
    fn note_where(w: &Expr, var: &str, equalities: &mut usize, other: &mut bool) {
        let mut stack = vec![w];
        while let Some(e) = stack.pop() {
            if let Expr::And(a, b) = e {
                stack.push(a);
                stack.push(b);
                continue;
            }
            let mut fvs = Vec::new();
            free_vars_of(e, &mut fvs);
            if !fvs.iter().any(|v| v == var) {
                continue;
            }
            let prop_eq_const = match e {
                Expr::Bin(engram_cypher::BinOp::Eq, a, b) => [(a, b), (b, a)].iter().any(|(side, rest)| {
                    let on_var = matches!(
                        side.as_ref(),
                        Expr::Prop(base, _) if matches!(base.as_ref(), Expr::Var(v) if v == var)
                    );
                    let mut rest_vars = Vec::new();
                    free_vars_of(rest, &mut rest_vars);
                    on_var && rest_vars.is_empty()
                }),
                _ => false,
            };
            if prop_eq_const {
                *equalities += 1;
            } else {
                *other = true;
            }
        }
    }
    let mut paths = 0usize;
    let mut equalities = 0usize;
    let mut other = false;
    for c in prefix {
        match c {
            Clause::Match {
                optional,
                pattern,
                where_,
            } => {
                for path in &pattern.paths {
                    paths += 1;
                    if path.start.var.as_deref() != Some(var)
                        || *optional
                        || !path.hops.is_empty()
                        || path.var.is_some()
                    {
                        return false;
                    }
                    if let Some(Expr::Map(entries)) = &path.start.props {
                        equalities += entries.len();
                    }
                }
                if let Some(w) = where_ {
                    note_where(w, var, &mut equalities, &mut other);
                }
            }
            Clause::Unwind { .. } => return false,
            Clause::With { where_: Some(w), .. } => note_where(w, var, &mut equalities, &mut other),
            _ => {}
        }
    }
    paths == 1 && (other || equalities >= 2)
}

fn plan_stage(
    graph: &Graph,
    prefix: &[Clause],
    breaker: &Clause,
    input_names: &[String],
    live_after: Option<&[String]>,
    props_after: Option<&BTreeMap<String, std::collections::BTreeSet<String>>>,
) -> StagePlan {
    let mut demands: BTreeMap<String, VarDemand> = BTreeMap::new();
    let walk = |e: &Expr, demands: &mut BTreeMap<String, VarDemand>| {
        collect_demand(e, &mut Vec::new(), demands);
    };
    // LIVENESS for WITH items: a bare variable a WITH carries forward that
    // NO later clause mentions is dead the moment it is projected — it
    // rides in the row under its name, but nothing reads it, so it needs
    // presence, never Full. Measured: `WITH n, count { (n)--() } AS d`
    // over 1.79M nodes fully decoded and re-cloned every fat node for a
    // projection the next clause (`WITH d ORDER BY d`) never read.
    // `None` means liveness is unknown (a clause shape the mention walk
    // cannot see through) and every item stays live.
    let mentions_after = |from: usize, stage_after: Option<&[String]>| -> Option<Vec<String>> {
        let mut out: Vec<String> = stage_after?.to_vec();
        for c in prefix.iter().skip(from) {
            out.extend(clause_mentions(c)?);
        }
        out.extend(clause_mentions(breaker)?);
        Some(out)
    };
    let with_items = |proj: &Projection,
                      live: Option<&[String]>,
                      props_after: Option<&BTreeMap<String, std::collections::BTreeSet<String>>>,
                      demands: &mut BTreeMap<String, VarDemand>| {
        for (i, it) in proj.items.iter().enumerate() {
            if let (Expr::Var(v), Some(live)) = (&it.expr, live) {
                let out_name = it
                    .alias
                    .clone()
                    .or_else(|| it.text.clone())
                    .unwrap_or_else(|| column_name(&it.expr, i));
                if !live.contains(&out_name) {
                    sometimes!("interp.dead projection demanded presence only", true);
                    demands
                        .entry(v.clone())
                        .or_insert_with(|| VarDemand::Props(std::collections::BTreeSet::new()));
                    continue;
                }
                // LIVE, but every later mention is a PROPERTY READ: the
                // bare `WITH s` used to demand the FULL node — every
                // property of every grouped NewsStory decoded and carried
                // through the aggregate — for a RETURN that read two of
                // them. The properties the later clauses read are the
                // demand; a bare later use (RETURN s, a pattern reusing
                // s, a subquery) is not in the map and keeps Full.
                if let Some(props) = props_after.and_then(|m| m.get(&out_name)) {
                    counted!("interp.live projection demanded only the properties read after it");
                    let entry = demands
                        .entry(v.clone())
                        .or_insert_with(|| VarDemand::Props(std::collections::BTreeSet::new()));
                    if let VarDemand::Props(set) = entry {
                        set.extend(props.iter().cloned());
                    }
                    continue;
                }
            }
            collect_demand(&it.expr, &mut Vec::new(), demands);
        }
    };
    let pattern_exprs = |pattern: &Pattern, demands: &mut BTreeMap<String, VarDemand>| {
        for path in &pattern.paths {
            if let Some(p) = &path.start.props {
                collect_demand(p, &mut Vec::new(), demands);
            }
            for (rel, node) in &path.hops {
                if let Some(p) = &rel.props {
                    collect_demand(p, &mut Vec::new(), demands);
                }
                if let Some(p) = &node.props {
                    collect_demand(p, &mut Vec::new(), demands);
                }
            }
        }
    };
    for (ci, c) in prefix.iter().enumerate() {
        match c {
            Clause::Match {
                pattern, where_, ..
            } => {
                pattern_exprs(pattern, &mut demands);
                if let Some(w) = where_ {
                    walk(w, &mut demands);
                }
            }
            Clause::Unwind { expr, .. } => walk(expr, &mut demands),
            Clause::With { proj, where_ } => {
                let live = mentions_after(ci + 1, live_after);
                // A prefix WITH is followed by more of THIS stage. What the
                // rest of the stage (the later prefix clauses and the
                // breaker) reads of each carried name is summarised the same
                // way the stage boundary summarises the clauses beyond it; a
                // carry the breaker passes on bare is the breaker's own
                // decision — its `with_items` runs after this and a Full
                // demand wins the merge. Until this held only liveness
                // narrowed a prefix WITH, and `MATCH (t:ResearchTask) WHERE …
                // WITH t MATCH (t)-[:…]->(p) RETURN count(p)` demanded every
                // carried node in FULL: 416 full records per statement on the
                // mirror (21 ms against Neo4j's 2.8) for a node the rest of
                // the stage used as an identity.
                let props_map: Option<BTreeMap<String, std::collections::BTreeSet<String>>> =
                    live.as_ref().map(|names| {
                        let mut uniq: Vec<&String> = names.iter().collect();
                        uniq.sort();
                        uniq.dedup();
                        let mut m = BTreeMap::new();
                        for name in uniq {
                            let mut props = std::collections::BTreeSet::new();
                            let prop_only = prefix[ci + 1..]
                                .iter()
                                .chain(std::iter::once(breaker))
                                .all(|c| clause_prop_only(c, name, &mut props) == Some(true));
                            if prop_only {
                                counted!(
                                    "interp.prefix projection demanded only the properties read after it"
                                );
                                m.insert(name.clone(), props);
                            }
                        }
                        m
                    });
                with_items(proj, live.as_deref(), props_map.as_ref(), &mut demands);
                for o in &proj.order {
                    walk(&o.expr, &mut demands);
                }
                if let Some(w) = where_ {
                    walk(w, &mut demands);
                }
            }
            _ => {}
        }
    }
    match breaker {
        Clause::Return { proj } => {
            for it in &proj.items {
                walk(&it.expr, &mut demands);
            }
            for o in &proj.order {
                walk(&o.expr, &mut demands);
            }
        }
        Clause::With { proj, .. } => {
            with_items(proj, live_after, props_after, &mut demands);
            for o in &proj.order {
                walk(&o.expr, &mut demands);
            }
        }
        _ => {}
    }
    if let Clause::With {
        where_: Some(w), ..
    } = breaker
    {
        walk(w, &mut demands);
    }

    // Late projection: defer output-only properties of MATCH-bound variables
    // past the top-k, binding lean and re-materialising only the k survivors.
    let (deferred, late_full_keys) = if graph.late_projection_enabled() {
        late_deferred(prefix, breaker, input_names, &demands)
    } else {
        (BTreeMap::new(), BTreeMap::new())
    };
    for (dvar, dprops) in &deferred {
        if let Some(VarDemand::Props(set)) = demands.get_mut(dvar) {
            for pp in dprops {
                set.remove(pp);
            }
        }
    }
    // Fix 56: a whole-node output of the concluding top-k is bound on its
    // key properties alone; the projector hydrates the survivors.
    let mut late_full: std::collections::BTreeSet<String> = Default::default();
    for (v, keyset) in late_full_keys {
        demands.insert(v.clone(), VarDemand::Props(keyset));
        late_full.insert(v);
        counted!("interp.stage bound a whole-node output lean for the top-k");
    }

    // Seeds, with the bound-name walk the scope check already performs.
    let mut bound: Vec<String> = input_names.to_vec();
    let mut seeds: Vec<Vec<Seed>> = Vec::with_capacity(prefix.len());
    let mut filters: Vec<Option<PushedFilters>> = Vec::with_capacity(prefix.len());
    let mut probe_where: Vec<Option<Option<Expr>>> = Vec::with_capacity(prefix.len());
    for c in prefix {
        match c {
            Clause::Match {
                pattern, where_, ..
            } => {
                let mut per_path = Vec::with_capacity(pattern.paths.len());
                for path in &pattern.paths {
                    let start_bound = path.start.var.as_ref().is_some_and(|v| bound.contains(v));
                    let id_seek = path
                        .start
                        .var
                        .as_deref()
                        .and_then(|v| id_seek_expr(where_.as_ref(), v));
                    let prop_eq = if graph.property_seek_enabled() {
                        path.start
                            .var
                            .as_deref()
                            .and_then(|v| prop_eq_index(where_.as_ref(), v))
                    } else {
                        None
                    };
                    // Fix 49: a positive top-level existence conjunct toward a
                    // constant-seekable end seeds the start from that end.
                    let exists_probe: Option<(PathPattern, Expr)> = match path.start.var.as_deref() {
                        Some(v) if !start_bound && graph.hop_reversal_enabled() => {
                            exists_seed_path(graph, where_.as_ref(), v)
                        }
                        _ => None,
                    };
                    let smallest_label = || -> Option<usize> {
                        if path.start.labels.is_empty() {
                            return None;
                        }
                        if path.start.labels.len() == 1 {
                            return Some(0);
                        }
                        let mut best = 0usize;
                        let mut best_n = u64::MAX;
                        for (i, l) in path.start.labels.iter().enumerate() {
                            let n = graph.count_label_nodes(l);
                            if n < best_n {
                                best_n = n;
                                best = i;
                            }
                        }
                        sometimes!("interp.seed picked the smallest label", true);
                        Some(best)
                    };
                    // A pattern-map entry makes the start a point lookup the
                    // range index can serve; whether it BEATS the label scan
                    // is decided at execution, when both sizes are known.
                    let index_key: Option<String> = match &path.start.props {
                        Some(Expr::Map(entries)) => entries.first().map(|(k, _)| k.clone()),
                        _ => None,
                    };
                    // Fix 49: the existence probe outranks an equality on an
                    // UNDECLARED key — that seek probes (or builds) an unscoped
                    // index the operator never asked for and usually loses to
                    // the label, which then scans; a DECLARED equality keeps
                    // its place (the seek the catalogue promised).
                    let declared = |key: &str| -> bool {
                        graph
                            .declared_scope_for(&path.start.labels, key)
                            .ok()
                            .flatten()
                            .is_some()
                    };
                    let eq_declared = prop_eq.as_ref().is_some_and(|(p, _)| declared(p))
                        || index_key.as_deref().is_some_and(declared);
                    let mut exists_probe = exists_probe;
                    let probe_first = exists_probe.is_some() && !eq_declared;
                    let seed = if start_bound {
                        Seed::Bound
                    } else if let Some(e) = id_seek {
                        Seed::ById(e)
                    } else if probe_first {
                        let (rp, conjunct) = exists_probe.take().expect("probe_first");
                        Seed::ExistsProbe {
                            path: Box::new(rp),
                            label_fallback: smallest_label(),
                            conjunct,
                        }
                    } else if let Some((prop, values)) = prop_eq {
                        Seed::PropEq {
                            prop,
                            values,
                            label_fallback: smallest_label(),
                        }
                    } else if let Some(key) = index_key {
                        Seed::IndexEq {
                            key,
                            label_fallback: smallest_label(),
                        }
                    } else if let Some((rp, conjunct)) = exists_probe {
                        Seed::ExistsProbe {
                            path: Box::new(rp),
                            label_fallback: smallest_label(),
                            conjunct,
                        }
                    } else if !path.start.labels.is_empty() {
                        Seed::Label(smallest_label().expect("labels checked non-empty"))
                    } else if path.start.props.is_none()
                        && path.var.is_none()
                        && path.hops.len() == 1
                        && path.hops[0].0.length.is_none()
                    {
                        Seed::Rels
                    } else {
                        Seed::AllNodes
                    };
                    per_path.push(seed);
                    for v in path_vars(path) {
                        bound.push(v);
                    }
                }
                seeds.push(per_path);
                // Fix 68: a clause whose FIRST path seeds from an existence
                // probe has that conjunct satisfied for every start the
                // probe names — the clause WHERE (and the prefilters pushed
                // out of it) run without it.
                let pruned_where: Option<Option<Expr>> = match (seeds.last().and_then(|s| s.first()), c) {
                    (
                        Some(Seed::ExistsProbe { conjunct, .. }),
                        Clause::Match {
                            where_: Some(w), ..
                        },
                    ) => {
                        counted!("interp.seed probe's conjunct pruned from the WHERE");
                        Some(strip_conjunct(w, conjunct))
                    }
                    _ => None,
                };
                let effective_where: Option<&Expr> = match (&pruned_where, c) {
                    (Some(p), _) => p.as_ref(),
                    (None, Clause::Match { where_, .. }) => where_.as_ref(),
                    _ => None,
                };
                probe_where.push(pruned_where.clone());
                // Pushed WHERE prefilters: place each conjunct at the
                // earliest position its variables exist. `bound` already
                // includes this pattern's variables; reconstruct the
                // per-position sets from the entry scope forward.
                let pushed = if let (Some(w), Clause::Match { pattern, .. }) = (effective_where, c) {
                    let entry_names: Vec<String> = {
                        let mut n = bound.clone();
                        for path in &pattern.paths {
                            for v in path_vars(path) {
                                if let Some(pos) = n.iter().rposition(|x| *x == v) {
                                    n.remove(pos);
                                }
                            }
                        }
                        n
                    };
                    let mut conj = Vec::new();
                    conjuncts_of(w, &mut conj);
                    let mut pf = PushedFilters {
                        entry: Vec::new(),
                        after_path: vec![Vec::new(); pattern.paths.len()],
                    };
                    for cexpr in conj {
                        if contains_opaque(&cexpr) {
                            continue; // stays in the full WHERE, unpushed
                        }
                        let mut free = Vec::new();
                        free_vars(&cexpr, &mut Vec::new(), &mut free);
                        if free.iter().all(|v| entry_names.contains(v)) {
                            pf.entry.push(cexpr);
                            continue;
                        }
                        let mut have = entry_names.clone();
                        let mut placed = false;
                        for (pi, path) in pattern.paths.iter().enumerate() {
                            have.extend(path_vars(path));
                            if free.iter().all(|v| have.contains(v)) {
                                // The LAST path's position is where the full
                                // WHERE already runs — pushing there buys
                                // nothing and would double-evaluate.
                                if pi + 1 < pattern.paths.len() {
                                    pf.after_path[pi].push(cexpr);
                                }
                                placed = true;
                                break;
                            }
                        }
                        let _ = placed; // unplaceable conjuncts stay in the full WHERE
                    }
                    if pf.entry.is_empty() && pf.after_path.iter().all(Vec::is_empty) {
                        None
                    } else {
                        Some(pf)
                    }
                } else {
                    None
                };
                filters.push(pushed);
            }
            Clause::Unwind { alias, .. } => {
                bound.push(alias.clone());
                seeds.push(Vec::new());
                filters.push(None);
                probe_where.push(None);
            }
            Clause::With { proj, .. } => {
                let mut next: Vec<String> = if proj.star { bound.clone() } else { Vec::new() };
                for (i, item) in proj.items.iter().enumerate() {
                    next.push(
                        item.alias
                            .clone()
                            .or_else(|| item.text.clone())
                            .unwrap_or_else(|| column_name(&item.expr, i)),
                    );
                }
                bound = next;
                seeds.push(Vec::new());
                filters.push(None);
                probe_where.push(None);
            }
            _ => {
                seeds.push(Vec::new());
                filters.push(None);
                probe_where.push(None);
            }
        }
    }
    // The breaker (`rest[0]` of the stage) and `live_after` name every
    // variable read downstream of this stage's prefix - what an UNWIND in the
    // prefix must carry. `drive`'s `rest` alone omits the breaker.
    let live_out: Option<std::collections::BTreeSet<String>> =
        match (clause_mentions(breaker), live_after) {
            (Some(mut vs), Some(la)) => {
                vs.extend(la.iter().cloned());
                Some(vs.into_iter().collect())
            }
            _ => None,
        };
    // Fix 52: a plain-limit breaker over a lone, hop-less, map-less,
    // WHERE-less one-label MATCH needs only its first `skip + limit`
    // starts (see `StagePlan::seed_cap`).
    let seed_cap: Option<(Option<Expr>, Option<Expr>)> = match (prefix, breaker) {
        (
            [Clause::Match {
                optional: false,
                pattern,
                where_: None,
            }],
            Clause::With { proj, .. } | Clause::Return { proj, .. },
        ) if input_names.is_empty()
            && pattern.paths.len() == 1
            && pattern.paths[0].hops.is_empty()
            && !pattern.paths[0].shortest
            && pattern.paths[0].start.props.is_none()
            && pattern.paths[0].start.labels.len() == 1
            && !proj.star
            && proj.order.is_empty()
            && !proj.distinct
            && proj.limit.is_some()
            && !proj.items.iter().any(|it| contains_aggregate(&it.expr)) =>
        {
            Some((proj.skip.clone(), proj.limit.clone()))
        }
        _ => None,
    };
    StagePlan {
        demands,
        seeds,
        filters,
        probe_where,
        live_out,
        deferred,
        frontier_vars: breaker_distinct_vars(breaker),
        seed_cap,
        late_full,
    }
}

/// The variables a breaker consumes DISTINCT-only — a node bound to one can be
/// produced once (frontier BFS) instead of once per path. Two SOUND cases are
/// recognised. Case one: `WITH/RETURN DISTINCT <items>` with NO aggregate
/// anywhere in the projection, where the whole row is de-duplicated so any
/// variable it carries has its multiplicity collapsed (every plain variable
/// mentioned qualifies). Case two: `collect(DISTINCT v)` / `count(DISTINCT v)`
/// where `v` appears NOWHERE else in the projection, so the DISTINCT aggregate
/// ignores `v`'s multiplicity and `v` is not a grouping key. Anything subtler
/// (a non-distinct aggregate over v, v as a grouping key, v in ORDER BY without
/// DISTINCT) is deliberately excluded — a false positive here would silently
/// drop rows.
fn breaker_distinct_vars(breaker: &Clause) -> std::collections::BTreeSet<String> {
    match breaker {
        Clause::With { proj, .. } | Clause::Return { proj } => distinct_vars_of_proj(proj),
        _ => std::collections::BTreeSet::new(),
    }
}

/// The variables a PROJECTION consumes DISTINCT-only — the projection-level core
/// of [`breaker_distinct_vars`], factored out so the columnar pipeline
/// (`pipeline::plan_and_run_columnar`) can apply the IDENTICAL frontier-BFS
/// eligibility test against the WITH / RETURN it recognises. See
/// [`breaker_distinct_vars`] for the two sound cases.
pub(crate) fn distinct_vars_of_proj(proj: &Projection) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    // Bail on any projection that a subquery/pattern/label predicate could hide
    // a variable use inside — a false positive here silently drops rows.
    if !proj.items.iter().all(|it| expr_analyzable(&it.expr)) {
        return out;
    }
    // Case 1: global DISTINCT over an aggregate-free projection.
    if proj.distinct && !proj.star && proj.items.iter().all(|it| !expr_has_aggregate(&it.expr)) {
        for it in &proj.items {
            collect_plain_vars(&it.expr, &mut out);
        }
        return out;
    }
    // Case 2: a variable used only inside a DISTINCT aggregate. Count every
    // mention of each variable, and every mention that is INSIDE a
    // `f(DISTINCT v)` aggregate; a variable whose two tallies match (>0) is
    // consumed distinct-only.
    let mut mentions: BTreeMap<String, usize> = BTreeMap::new();
    let mut distinct_agg: BTreeMap<String, usize> = BTreeMap::new();
    for it in &proj.items {
        count_var_mentions(&it.expr, false, &mut mentions, &mut distinct_agg);
    }
    for (v, total) in &mentions {
        if distinct_agg.get(v).copied().unwrap_or(0) == *total {
            out.insert(v.clone());
        }
    }
    out
}

/// Whether an expression contains an aggregate function call (a `DISTINCT`
/// aggregate, or one of the known reducers). Conservative: an unknown call is
/// treated as NOT an aggregate, so Case 1 only fires on obviously-safe shapes.
pub(crate) fn expr_has_aggregate(e: &Expr) -> bool {
    let mut found = false;
    walk_expr(e, &mut |x| {
        if let Expr::Call {
            name,
            distinct,
            star,
            ..
        } = x
        {
            if *distinct || *star || is_aggregate_name(name) {
                found = true;
            }
        }
    });
    found
}

fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name,
        "count"
            | "collect"
            | "sum"
            | "avg"
            | "min"
            | "max"
            | "stdev"
            | "stdevp"
            | "percentilecont"
            | "percentiledisc"
    )
}

/// Add every plain `Var` name mentioned in `e` (through properties too).
fn collect_plain_vars(e: &Expr, out: &mut std::collections::BTreeSet<String>) {
    walk_expr(e, &mut |x| {
        if let Expr::Var(v) = x {
            out.insert(v.clone());
        }
    });
}

/// Tally variable mentions and mentions that sit inside a `f(DISTINCT v)`
/// aggregate. `in_distinct_agg` is set true while walking such a call's args.
fn count_var_mentions(
    e: &Expr,
    in_distinct_agg: bool,
    mentions: &mut BTreeMap<String, usize>,
    distinct_agg: &mut BTreeMap<String, usize>,
) {
    match e {
        Expr::Var(v) => {
            *mentions.entry(v.clone()).or_insert(0) += 1;
            if in_distinct_agg {
                *distinct_agg.entry(v.clone()).or_insert(0) += 1;
            }
        }
        Expr::Call {
            name,
            distinct,
            args,
            ..
        } => {
            let inner = in_distinct_agg || (*distinct && is_aggregate_name(name));
            for a in args {
                count_var_mentions(a, inner, mentions, distinct_agg);
            }
        }
        _ => {
            for_each_child(e, &mut |c| {
                count_var_mentions(c, in_distinct_agg, mentions, distinct_agg)
            });
        }
    }
}

/// Visit `e` and every sub-expression, pre-order.
fn walk_expr(e: &Expr, f: &mut dyn FnMut(&Expr)) {
    f(e);
    for_each_child(e, &mut |c| walk_expr(c, f));
}

/// Visit the DIRECT sub-expressions of `e`. Every compound variant is handled;
/// leaves have none. Kept exhaustive so an aggregate can never hide from
/// `expr_has_aggregate` inside a variant this forgot to descend into.
fn for_each_child(e: &Expr, f: &mut dyn FnMut(&Expr)) {
    match e {
        Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::Param(_)
        | Expr::Var(_) => {}
        Expr::List(xs) => xs.iter().for_each(f),
        Expr::Map(kvs) => kvs.iter().for_each(|(_, v)| f(v)),
        Expr::Prop(b, _) => f(b),
        Expr::Index(a, b) => {
            f(a);
            f(b);
        }
        Expr::Slice { of, from, to } => {
            f(of);
            if let Some(x) = from {
                f(x);
            }
            if let Some(x) = to {
                f(x);
            }
        }
        Expr::Bin(_, a, b)
        | Expr::And(a, b)
        | Expr::Or(a, b)
        | Expr::Xor(a, b)
        | Expr::In(a, b) => {
            f(a);
            f(b);
        }
        Expr::Not(a) | Expr::Neg(a) => f(a),
        Expr::IsNull { of, .. } => f(of),
        Expr::Call { args, .. } => args.iter().for_each(f),
        Expr::Case {
            subject,
            arms,
            otherwise,
        } => {
            if let Some(s) = subject {
                f(s);
            }
            for (w, t) in arms {
                f(w);
                f(t);
            }
            if let Some(o) = otherwise {
                f(o);
            }
        }
        Expr::ListComp {
            source,
            filter,
            map,
            ..
        } => {
            f(source);
            if let Some(x) = filter {
                f(x);
            }
            if let Some(x) = map {
                f(x);
            }
        }
        Expr::Reduce {
            init, source, step, ..
        } => {
            f(init);
            f(source);
            f(step);
        }
        // Subqueries, pattern predicates, label tests, map projections: their
        // children are not descended here. `expr_analyzable` refuses any
        // projection containing one, so the walkers above never need to see
        // inside — a bare `_` keeps this sound without enumerating them.
        _ => {}
    }
}

/// Whether `e` is built entirely from ordinary expressions the DISTINCT-only
/// analysis can trust. A subquery, pattern predicate, label test or map
/// projection could hide a variable use or an aggregate that `for_each_child`
/// does not descend into, so a projection containing one is not analyzed.
fn expr_analyzable(e: &Expr) -> bool {
    let mut ok = true;
    walk_expr(e, &mut |x| {
        if matches!(
            x,
            Expr::HasLabels { .. }
                | Expr::PatternPredicate(_)
                | Expr::ListPredicate { .. }
                | Expr::MapProjection { .. }
                | Expr::ExistsSub(_)
                | Expr::CountSub(_)
                | Expr::PatternComp { .. }
        ) {
            ok = false;
        }
    });
    ok
}

/// Materialise a node under a projection set (`None` = full).
fn mat_node(
    graph: &Graph,
    id: u64,
    set: Option<&std::collections::BTreeSet<String>>,
) -> Result<Option<Value>, RunError> {
    match set {
        None => Ok(graph.node(id)?),
        Some(s) => Ok(graph.node_projected(id, s)?),
    }
}

/// Fix 60: a hop end bound to a DEMAND (fix 51) whose every property is a
/// CACHED column of the end's one pattern label is built from the columns
/// — a membership test and a binary search per property — instead of a
/// projected store get per row. The clause executor's matcher runs the
/// `COUNT {}` / `EXISTS {}` bodies and the pattern comprehensions of a
/// statement once per outer row, so the KMProject dashboard's eight
/// `COUNT { (w:KMWorkItem)-[:BELONGS_TO_PROJECT]->(p) WHERE w.status = … }`
/// per project read 18,053 work items projected from the store (~10 µs
/// each: 208 ms against Neo4j's 22) for a `status` the label's column
/// already held. The node is exactly the projected read's: the demanded
/// properties present on the node and the pattern's label, which the
/// membership test has just proven (a node whose LABELS the statement
/// reads is Full-demanded and never comes here). Any column not cached,
/// an id the label does not hold, a writing transaction (its buffered
/// version must win) or the columnar paths off: the store read, as before.
fn mat_end(
    graph: &Graph,
    id: u64,
    peer_props: Option<&std::collections::BTreeSet<String>>,
    node_pat: &NodePattern,
) -> Result<Option<Value>, RunError> {
    // Fix 74: with ONE pattern label, the label's membership decides a
    // NON-member's fate before any record is read. Every caller tests the
    // node it gets back with `node_satisfies`, whose first check is the
    // pattern's labels, so a non-member can never survive — yet both
    // branches below fell to `mat_node` for it: a projected (or full)
    // record read decoded only to be dropped. `UNWIND $names AS name MATCH
    // (e:Entity {name})<-[:MENTIONS]-(a:NewsArticle) RETURN count(a)` on
    // the mirror paid 902 projected gets for 183 articles, because 719 of
    // the entities' MENTIONS edges come from emails (4.9 ms against
    // Neo4j's 1.3). The sentinel is the id with NO labels: `node_satisfies`
    // rejects it on the label test exactly as it rejected the decoded
    // record. Same guards as the member branches: the columnar paths on,
    // no writing transaction (its buffered labels must win).
    if let [label] = node_pat.labels.as_slice() {
        if peer_props.is_some() && graph.columnar_scans_enabled() && !graph.in_txn_with_writes() {
            let members = graph.members(Some(label))?;
            if !graph.members_contains(&members, id) {
                counted!("interp.matcher rejected a non-member hop end from membership");
                return Ok(Some(Value::Node {
                    id,
                    labels: Vec::new(),
                    props: BTreeMap::new(),
                }));
            }
        }
    }
    // Fix 68: an end NOTHING reads — an empty demand: no map, no property,
    // no later use — needs no record. Bare, it is its id (an adjacency
    // entry names a live node); with one pattern label, the label's
    // membership decides (a non-member was rejected above). The seed
    // probe's walk bound every work item this way.
    if let Some(set) = peer_props {
        if set.is_empty() && !graph.in_txn_with_writes() {
            match node_pat.labels.as_slice() {
                [] => {
                    counted!("interp.matcher bound a hop end bare");
                    return Ok(Some(Value::Node {
                        id,
                        labels: Vec::new(),
                        props: BTreeMap::new(),
                    }));
                }
                [label] if graph.columnar_scans_enabled() => {
                    let members = graph.members(Some(label))?;
                    if graph.members_contains(&members, id) {
                        counted!("interp.matcher bound a hop end bare");
                        return Ok(Some(Value::Node {
                            id,
                            labels: vec![label.clone()],
                            props: BTreeMap::new(),
                        }));
                    }
                }
                _ => {}
            }
        }
    }
    if let (Some(set), [label]) = (peer_props, node_pat.labels.as_slice()) {
        if !set.is_empty() && graph.columnar_scans_enabled() && !graph.in_txn_with_writes() {
            let members = graph.members(Some(label))?;
            if graph.members_contains(&members, id) {
                let mut props = BTreeMap::new();
                let mut served = true;
                for p in set {
                    match graph.prop_column(label, p, false) {
                        Some(crate::PropColumn::Values(col)) => {
                            if let Ok(at) = col.binary_search_by_key(&id, |(i, _)| *i) {
                                let v = col[at].1.clone();
                                if !matches!(v, Value::Null) {
                                    props.insert(p.clone(), v);
                                }
                            }
                        }
                        _ => {
                            served = false;
                            break;
                        }
                    }
                }
                if served {
                    counted!("interp.matcher bound a hop end from the label's cached columns");
                    return Ok(Some(Value::Node {
                        id,
                        labels: vec![label.clone()],
                        props,
                    }));
                }
            }
        }
    }
    mat_node(graph, id, peer_props)
}

/// A seed population below which the per-id projected read is used as is:
/// the column read has a fixed cost (a membership view, a cache lookup per
/// property) that a handful of ids never repays.
const LEAN_COLUMN_BATCH: usize = 64;

/// Fix 41: the LEAN start population bound from the label's COLUMNS in one
/// read — `Some(nodes)` carrying exactly the demanded properties (plus the
/// start map's keys, which `node_satisfies` tests on them) and the pattern's
/// labels — instead of a projected store get per seed. The inbox listing
/// (`MATCH (n:UserDataNode {nodeType, userId}) WHERE … WITH n ORDER BY
/// n.createdAt DESC SKIP … LIMIT 1000 …`) seeded 18,111 emails through the
/// column filter and then read every one of them back (18,111 projected
/// gets; 9,787 of them block-cache misses on the saturated 4 GB cache —
/// the records are fat) to bind ONE sort key: 1,251 ms for page one against
/// Neo4j's 104, which reads the key from its index and never touches a
/// record. The columns come from `load_var_columns_labelled` — served from
/// the property-column cache, walked whole and kept when the population
/// is a large share of the label, else gathered for exactly these ids (a
/// point read of the named properties per id — no worse than the projected
/// get it replaces). `None` = not applicable (no demand set, no labels, a
/// small population, or a column read that declined): the caller's per-id
/// path stands. Only ids that carry the pattern's labels are bound — an
/// unscoped probe may answer ids under other labels, and the fabricated
/// label list must be true of every node it is put on.
fn lean_starts_from_columns(
    graph: &Graph,
    ids: &[u64],
    start: &NodePattern,
    start_set: Option<&std::collections::BTreeSet<String>>,
    params: &BTreeMap<String, Value>,
) -> Result<Option<Vec<Value>>, RunError> {
    let Some(set) = start_set else {
        return Ok(None);
    };
    if !graph.columnar_scans_enabled() || start.labels.is_empty() || ids.len() < LEAN_COLUMN_BATCH {
        return Ok(None);
    }
    let mut props: std::collections::BTreeSet<String> =
        set.iter().filter(|p| !p.starts_with("__")).cloned().collect();
    if let Some(Expr::Map(entries)) = &start.props {
        for (k, _) in entries {
            props.insert(k.clone());
        }
    }
    if props.is_empty() {
        return Ok(None);
    }
    let members = graph.members_all(&start.labels)?;
    let mut distinct: Vec<u64> = ids
        .iter()
        .copied()
        .filter(|id| graph.members_contains(&members, *id))
        .collect();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() < LEAN_COLUMN_BATCH {
        return Ok(None);
    }
    let Some(cols) = crate::pipeline::load_var_columns_labelled(
        graph,
        VarKind::Node,
        &distinct,
        &props,
        Some(&start.labels),
        params,
    )?
    else {
        return Ok(None);
    };
    counted!("interp.seed starts bound from the label column");
    let mut out = Vec::with_capacity(distinct.len());
    for (i, &id) in distinct.iter().enumerate() {
        let mut m = BTreeMap::new();
        for (p, col) in &cols {
            if let Some(v) = col.get(i) {
                if !matches!(v, Value::Null) {
                    m.insert(p.clone(), v.clone());
                }
            }
        }
        out.push(Value::Node {
            id,
            labels: start.labels.clone(),
            props: m,
        });
    }
    Ok(Some(out))
}

type RowSink<'s> = &'s mut dyn FnMut(Row) -> Result<(), RunError>;

/// The columnar recogniser cascade over one clause list — `Ok(Some)` when a
/// fast path claimed and answered the query, `Ok(None)` when every
/// recogniser declined (the caller falls back, or retries with the FUSED
/// clause list — see `run_single`).
fn try_recognisers(
    graph: &Graph,
    q: &SingleQuery,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    if let Some(r) = try_count_fast(graph, q) {
        return Ok(Some(r));
    }
    if let Some(r) = try_rel_histogram_fast(graph, q, params) {
        return Ok(Some(r));
    }
    if let Some(r) = crate::batch::try_columnar_aggregate(graph, q, params)? {
        return Ok(Some(r));
    }
    if let Some(r) = crate::batch::try_columnar_projection(graph, q, params)? {
        return Ok(Some(r));
    }
    if let Some(r) = crate::pipeline::plan_and_run_columnar(graph, q, params)? {
        return Ok(Some(r));
    }
    if let Some(r) = crate::vectorized::try_vectorized_hop_filter_count(graph, q, params)? {
        return Ok(Some(r));
    }
    if let Some(r) = crate::vectorized::try_vectorized_hop_topk(graph, q, params)? {
        return Ok(Some(r));
    }
    if let Some(r) = crate::vectorized::try_vectorized_unwind_hop_topk(graph, q, params)? {
        return Ok(Some(r));
    }
    if let Some(r) = crate::vectorized::try_vectorized_collect_ic9_topk(graph, q, params)? {
        return Ok(Some(r));
    }
    if let Some(r) = crate::batch::try_columnar_hop_aggregate(graph, q, params)? {
        return Ok(Some(r));
    }
    Ok(None)
}

/// Fuse runs of consecutive plain (non-OPTIONAL, non-shortest) MATCH
/// clauses into one multi-path MATCH, for the recogniser cascade only —
/// see the call site in `run_single` for why this is semantics-preserving
/// in this engine. `None` when there is no adjacent pair to fuse.
fn fuse_consecutive_matches(q: &SingleQuery) -> Option<SingleQuery> {
    fn fusable(c: &Clause) -> bool {
        matches!(
            c,
            Clause::Match {
                optional: false,
                pattern,
                ..
            } if pattern.paths.iter().all(|p| !p.shortest)
        )
    }
    if !q
        .clauses
        .windows(2)
        .any(|w| fusable(&w[0]) && fusable(&w[1]))
    {
        return None;
    }
    let mut out: Vec<Clause> = Vec::with_capacity(q.clauses.len());
    for c in &q.clauses {
        if fusable(c) {
            if let Some(Clause::Match {
                optional: false,
                pattern: prev_p,
                where_: prev_w,
            }) = out.last_mut()
            {
                if let Clause::Match {
                    pattern, where_, ..
                } = c
                {
                    prev_p.paths.extend(pattern.paths.iter().cloned());
                    *prev_w = match (prev_w.take(), where_.clone()) {
                        (None, w) | (w, None) => w,
                        (Some(a), Some(b)) => Some(Expr::And(Box::new(a), Box::new(b))),
                    };
                    continue;
                }
            }
        }
        out.push(c.clone());
    }
    Some(SingleQuery { clauses: out })
}

fn streamable(q: &SingleQuery) -> bool {
    let read_only = q.clauses.iter().all(|c| match c {
        Clause::Match { pattern, .. } => pattern.paths.iter().all(|p| !p.shortest),
        Clause::Unwind { .. } | Clause::With { .. } | Clause::Return { .. } => true,
        _ => false,
    });
    // A final `RETURN *` needs its column schema even over ZERO rows, which the
    // streaming projector cannot supply — keep it on the interpreter loop, which
    // tracks the schema across clauses.
    let star_return = matches!(q.clauses.last(), Some(Clause::Return { proj }) if proj.star);
    read_only && matches!(q.clauses.last(), Some(Clause::Return { .. })) && !star_return
}

fn with_is_breaker(proj: &Projection) -> bool {
    proj.distinct
        || !proj.order.is_empty()
        || proj.skip.is_some()
        || proj.limit.is_some()
        || proj.items.iter().any(|it| contains_aggregate(&it.expr))
}

/// `MATCH p1, p2, …, pn WHERE w` with every path a bare single node is
/// exactly `MATCH p1 MATCH p2 … MATCH pn WHERE w`: no relationships, so
/// the one-MATCH isomorphism rule has nothing to bind, and the rows and
/// their order are the same nested product. The rewritten shape is what
/// the clause memo and the equality index already join — without it a
/// cartesian-with-equality inside ONE clause rescanned the inner label
/// per outer row. OPTIONAL MATCH is never split: its null row covers the
/// whole pattern at once, and two OPTIONAL clauses would emit different
/// rows when only the second fails.
fn normalize_cartesian_matches(q: &SingleQuery) -> Option<SingleQuery> {
    let single_node = |p: &PathPattern| p.hops.is_empty() && p.var.is_none() && !p.shortest;
    let mut changed = false;
    let mut out: Vec<Clause> = Vec::with_capacity(q.clauses.len());
    for c in &q.clauses {
        match c {
            Clause::Match {
                optional: false,
                pattern,
                where_,
            } if pattern.paths.len() > 1 && pattern.paths.iter().all(single_node) => {
                changed = true;
                sometimes!("interp.cartesian MATCH split into clauses", true);
                let last = pattern.paths.len() - 1;
                for (i, path) in pattern.paths.iter().enumerate() {
                    out.push(Clause::Match {
                        optional: false,
                        pattern: Pattern {
                            paths: vec![path.clone()],
                        },
                        where_: if i == last { where_.clone() } else { None },
                    });
                }
            }
            other => out.push(other.clone()),
        }
    }
    if changed {
        let mut q2 = q.clone();
        q2.clauses = out;
        Some(q2)
    } else {
        None
    }
}

fn run_streaming(
    graph: &Graph,
    q: &SingleQuery,
    params: &BTreeMap<String, Value>,
    input: Vec<Row>,
) -> Result<QueryResult, RunError> {
    if let Some(q2) = normalize_cartesian_matches(q) {
        return run_streaming(graph, &q2, params, input);
    }
    sometimes!("interp.streamed a read-only chain", true);
    counted!("interp.statements run");
    stream_stage(graph, &q.clauses, input, params, &Default::default())
}

/// Whether a RETURN is the top-k shape `StreamProjector::new` bounds: ordered
/// and limited, neither DISTINCT nor aggregating nor `*`.
fn topk_return_shape(proj: &Projection) -> bool {
    !proj.star
        && !proj.distinct
        && !proj.order.is_empty()
        && proj.limit.is_some()
        && !proj.items.iter().any(|it| contains_aggregate(&it.expr))
}

/// Lever G' (fix 27). For a name a concluding top-k RETURN outputs BARE and
/// otherwise reads only by property — in its other items and in its ORDER
/// BY, directly or through the bare item's alias — the properties those
/// reads need. `None` when any use is a whole-entity one (a function over
/// the node, a subquery, an ORDER BY on the node itself) or the name is not
/// output bare at all.
fn late_full_reads(
    proj: &Projection,
    name: &str,
) -> Option<std::collections::BTreeSet<String>> {
    let mut props = std::collections::BTreeSet::new();
    let mut aliases: Vec<String> = Vec::new();
    for (i, it) in proj.items.iter().enumerate() {
        // Fix 73: `properties(name)` outputs the node's whole map exactly
        // as the bare item outputs the node — hydrated for the survivors,
        // its alias reads the lean map at the key (`ORDER BY p.createdAt`
        // through `properties(p) AS p`).
        let whole_output = match &it.expr {
            Expr::Var(v) => v == name,
            Expr::Call { name: f, args, .. } if f.eq_ignore_ascii_case("properties") => {
                matches!(args.as_slice(), [Expr::Var(v)] if v == name)
            }
            _ => false,
        };
        if whole_output {
            aliases.push(
                it.alias
                    .clone()
                    .or_else(|| it.text.clone())
                    .unwrap_or_else(|| column_name(&it.expr, i)),
            );
            continue;
        }
        if !group_key_prop_only(&it.expr, name, &mut props) {
            return None;
        }
    }
    if aliases.is_empty() {
        return None;
    }
    for o in &proj.order {
        if !group_key_prop_only(&o.expr, name, &mut props) {
            return None;
        }
        // `RETURN p AS x … ORDER BY x.priority` reads p's property through
        // the output column, which at the key is the LEAN node.
        for a in &aliases {
            if a != name && !group_key_prop_only(&o.expr, a, &mut props) {
                return None;
            }
        }
    }
    Some(props)
}

/// Execute one pipeline stage: a streaming prefix of reading clauses into a
/// collecting breaker (an aggregating/ordering WITH, or the RETURN), then
/// recurse on whatever follows the breaker. `late_full` names the input-row
/// variables the previous stage bound LEAN for this stage's top-k RETURN to
/// hydrate in full for its survivors (empty everywhere else).
fn stream_stage(
    graph: &Graph,
    clauses: &[Clause],
    input: Vec<Row>,
    params: &BTreeMap<String, Value>,
    late_full: &std::collections::BTreeSet<String>,
) -> Result<QueryResult, RunError> {
    let mut split = clauses.len();
    for (i, c) in clauses.iter().enumerate() {
        match c {
            Clause::Return { .. } => {
                split = i;
                break;
            }
            Clause::With { proj, .. } if with_is_breaker(proj) => {
                split = i;
                break;
            }
            _ => {}
        }
    }
    let (prefix, rest) = clauses.split_at(split);
    // Unbound-WHERE refusal, statically, before any scan — the same check
    // the materialising path runs per MATCH, over the same name scope.
    let mut bound: Vec<String> = input
        .first()
        .map(|r| r.keys().cloned().collect())
        .unwrap_or_default();
    for c in prefix {
        match c {
            Clause::Match {
                pattern, where_, ..
            } => {
                if let Some(w) = where_ {
                    let mut probe = Row::new();
                    for n in &bound {
                        probe.insert(n.clone(), Value::Null);
                    }
                    check_where_scope(w, pattern, &[probe])?;
                }
                for path in &pattern.paths {
                    bound.extend(path_vars(path));
                }
            }
            Clause::Unwind { alias, .. } => bound.push(alias.clone()),
            Clause::With { proj, .. } => {
                // A pure WITH rebinds the scope to its projected names; the
                // star carries everything already bound.
                let mut next: Vec<String> = if proj.star { bound.clone() } else { Vec::new() };
                for (i, item) in proj.items.iter().enumerate() {
                    next.push(
                        item.alias
                            .clone()
                            .or_else(|| item.text.clone())
                            .unwrap_or_else(|| column_name(&item.expr, i)),
                    );
                }
                bound = next;
            }
            _ => {}
        }
    }

    let input_names: Vec<String> = input
        .first()
        .map(|r| r.keys().cloned().collect())
        .unwrap_or_default();
    match rest.first() {
        Some(Clause::Return { proj }) => {
            // A late-full carry is a contract with the StreamProjector below:
            // every other answer path would output the lean node.
            let hydrates = !late_full.is_empty();
            // A constant-count stage concluding the statement: `… RETURN
            // threads, count(r) AS edges`.
            if let Some(rows) = (!hydrates)
                .then(|| {
                    constant_count_stage(graph, prefix, proj, None, &input, &input_names, params)
                })
                .transpose()?
                .flatten()
            {
                sometimes!("interp.constant count stage answered at the RETURN", true);
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
                let out: Vec<Vec<Value>> = rows
                    .iter()
                    .map(|r| {
                        columns
                            .iter()
                            .map(|c| r.get(c).cloned().unwrap_or(Value::Null))
                            .collect()
                    })
                    .collect();
                return Ok(QueryResult { columns, rows: out });
            }
            if let Some((rows, _)) = (!hydrates)
                .then(|| {
                    crate::batch::try_columnar_stage(graph, prefix, &rest[0], &[], &input, params)
                })
                .transpose()?
                .flatten()
            {
                sometimes!("interp.columnar stage concluded at the RETURN", true);
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
                let out: Vec<Vec<Value>> = rows
                    .iter()
                    .map(|r| {
                        columns
                            .iter()
                            .map(|c| r.get(c).cloned().unwrap_or(Value::Null))
                            .collect()
                    })
                    .collect();
                return Ok(QueryResult { columns, rows: out });
            }
            let plan = plan_stage(graph, prefix, &rest[0], &input_names, Some(&[]), None);
            let mut collector = StreamProjector::new(graph, proj, params, plan.deferred.clone())?;
            // The previous stage's lean carries AND (fix 56) this stage's
            // own lean-bound whole-node outputs are hydrated for the
            // survivors alike.
            let mut hydrate = late_full.clone();
            hydrate.extend(plan.late_full.iter().cloned());
            collector.late_full = hydrate;
            let caches: Vec<std::cell::RefCell<MemoSlot>> = (0..prefix.len())
                .map(|_| std::cell::RefCell::new(MemoSlot::Untouched))
                .collect();
            for row in input {
                match drive(
                    graph,
                    prefix,
                    &plan.seeds,
                    &plan,
                    &caches,
                    row,
                    params,
                    &mut |r| collector.push(r),
                ) {
                    Err(RunError::Saturated) => break, // the LIMIT is full
                    other => other?,
                }
            }
            Ok(collector.finish()?.into_result())
        }
        Some(Clause::With { proj, where_ }) => {
            // A BARE-COUNT stage — `[OPTIONAL] MATCH (v:L…) WITH <carried>,
            // count(v) AS c` — answers from the count store. Chains of these
            // (`OPTIONAL MATCH (s:Bio:Species) WITH count(s) AS a OPTIONAL
            // MATCH (d:Med:Disease) WITH a, count(d) AS b …`) each streamed a
            // full label per stage (5 s on the production port) for a
            // number the store keeps current.
            if let Some(rows) = constant_count_stage(
                graph,
                prefix,
                proj,
                where_.as_ref(),
                &input,
                &input_names,
                params,
            )? {
                sometimes!(
                    "interp.bare count stage answered from the count store",
                    true
                );
                return stream_stage(graph, &rest[1..], rows, params, &Default::default());
            }
            // The stage head as a column walk (a WITH chain over one scanned
            // variable, ordered and paged at this breaker).
            if let Some((rows, consumed)) = crate::batch::try_columnar_stage(
                graph,
                prefix,
                &rest[0],
                &rest[1..],
                &input,
                params,
            )? {
                return stream_stage(
                    graph,
                    &rest[1 + consumed..],
                    rows,
                    params,
                    &Default::default(),
                );
            }
            // Liveness past this breaker: what the remaining clauses read.
            let mut after: Option<Vec<String>> = Some(Vec::new());
            for c in &rest[1..] {
                match (after.as_mut(), clause_mentions(c)) {
                    (Some(acc), Some(m)) => acc.extend(m),
                    _ => after = None,
                }
            }
            // And HOW they read it: a name every later clause touches only
            // as `name.prop` maps to those props; a bare use anywhere (or a
            // clause shape the walk cannot see through) leaves it out, and
            // the projection then demands the full node as before.
            let mut props_after: Option<BTreeMap<String, std::collections::BTreeSet<String>>> =
                after.as_ref().map(|names| {
                    let mut uniq: Vec<&String> = names.iter().collect();
                    uniq.sort();
                    uniq.dedup();
                    let mut m = BTreeMap::new();
                    for name in uniq {
                        let mut props = std::collections::BTreeSet::new();
                        let prop_only = rest[1..]
                            .iter()
                            .all(|c| clause_prop_only(c, name, &mut props) == Some(true));
                        if prop_only {
                            m.insert(name.clone(), props);
                        }
                    }
                    m
                });
            // Lever G' (fix 27): a name whose ONLY later clause is a top-k
            // RETURN that outputs it bare and otherwise reads it by property
            // is bound LEAN here — the properties those reads need — and
            // hydrated in full at the RETURN for its k survivors alone.
            // Until this held `WITH p, collect(DISTINCT a.id) AS ids RETURN
            // p, ids ORDER BY p.priority DESC, p.proposedAt DESC LIMIT 25`
            // decoded every grouped Proposal in full for the 25 it paged
            // (7.2 ms on the mirror against Neo4j's 2.1).
            let mut late_full: std::collections::BTreeSet<String> = Default::default();
            if graph.late_projection_enabled() {
                if let (Some(names), Some(m), [Clause::Return { proj: rp }]) =
                    (after.as_deref(), props_after.as_mut(), &rest[1..])
                {
                    if topk_return_shape(rp) {
                        // Only a NODE variable this breaker carries bare
                        // (`WITH p …`, any alias) qualifies: one bound by a
                        // pattern of this stage, or an input the previous
                        // stage bound as a node. A list, a scalar, or a
                        // relationship is never hydrated.
                        let node_var = |v: &str| -> bool {
                            prefix.iter().any(|c| match c {
                                Clause::Match { pattern, .. } => {
                                    pattern.paths.iter().any(|path| {
                                        path.start.var.as_deref() == Some(v)
                                            || path
                                                .hops
                                                .iter()
                                                .any(|(_, n)| n.var.as_deref() == Some(v))
                                    })
                                }
                                _ => false,
                            }) || matches!(
                                input.first().and_then(|r| r.get(v)),
                                Some(Value::Node { .. })
                            )
                        };
                        let carried: std::collections::BTreeSet<String> = proj
                            .items
                            .iter()
                            .enumerate()
                            .filter_map(|(i, it)| match &it.expr {
                                Expr::Var(v) if node_var(v) => Some(
                                    it.alias
                                        .clone()
                                        .or_else(|| it.text.clone())
                                        .unwrap_or_else(|| column_name(&it.expr, i)),
                                ),
                                _ => None,
                            })
                            .collect();
                        let mut uniq: Vec<&String> = names.iter().collect();
                        uniq.sort();
                        uniq.dedup();
                        for name in uniq {
                            if m.contains_key(name) || !carried.contains(name) {
                                continue;
                            }
                            if let Some(props) = late_full_reads(rp, name) {
                                counted!(
                                    "interp.breaker bound a bare carry lean for the RETURN's top-k"
                                );
                                m.insert(name.clone(), props);
                                late_full.insert(name.clone());
                            }
                        }
                    }
                }
            }
            let plan = plan_stage(
                graph,
                prefix,
                &rest[0],
                &input_names,
                after.as_deref(),
                props_after.as_ref(),
            );
            let mut collector = StreamProjector::new(graph, proj, params, plan.deferred.clone())?;
            let caches: Vec<std::cell::RefCell<MemoSlot>> = (0..prefix.len())
                .map(|_| std::cell::RefCell::new(MemoSlot::Untouched))
                .collect();
            for row in input {
                match drive(
                    graph,
                    prefix,
                    &plan.seeds,
                    &plan,
                    &caches,
                    row,
                    params,
                    &mut |r| collector.push(r),
                ) {
                    Err(RunError::Saturated) => break, // the LIMIT is full
                    other => other?,
                }
            }
            let mut rows = collector.finish()?.into_rows()?;
            if let Some(w) = where_ {
                rows = filter_rows(graph, rows, w, params)?;
            }
            stream_stage(graph, &rest[1..], rows, params, &late_full)
        }
        _ => unreachable!("streamable() requires a concluding RETURN"),
    }
}

/// Per-clause memo of an INDEPENDENT single-node scan, one slot per clause
/// of the stage. `None` until (and unless) the Match arm builds it.
type ClauseScanCache = [std::cell::RefCell<MemoSlot>];

/// The memo engages on the SECOND row that reaches a clause, never the
/// first: a single-row stage (every `MATCH (n) …` at a stage head) must
/// STREAM its scan, one candidate at a time. Building the memo there
/// materialises the whole candidate population for a replay that never
/// happens — measured as the production run's OOM: `MATCH (n)` over 1.79M
/// full-prop nodes stacked the entire graph's values on top of a 15.9GB
/// resident set. The first row pays the ordinary streaming cost; only a
/// second row proves the scan repeats.
enum MemoSlot {
    /// No row has reached this clause yet.
    Untouched,
    /// One row streamed through without a memo; the next one builds it.
    SeenOnce,
    /// Built — every further row replays it.
    Built(std::sync::Arc<CachedScan>),
}

/// A memoised clause scan: the candidates, plus (when the WHERE carries a
/// correlated string equality on the scan variable) an index over them.
struct CachedScan {
    cands: Vec<Value>,
    eq: Option<EqIndex>,
}

/// An index over cached candidates for ONE correlated conjunct of shape
/// `cand.key = <outer expr>` (either side). Buckets hold only candidates
/// whose key is a STRING — the one type with no cross-type equality — and
/// everything else (missing key, non-string key) sits in `residual`, judged
/// per row by the WHERE exactly as before. Index lists are in scan order,
/// and iteration MERGES bucket and residual by position, so the emitted
/// row order is byte-identical to the linear walk.
struct EqIndex {
    /// The indexed conjuncts: (candidate property, outer-side expression).
    /// Several conjuncts form a COMPOSITE key.
    keys: Vec<(String, Expr)>,
    /// Candidates by canonical composite key — the G0 key, which encodes
    /// Cypher `=`-equivalence exactly (numerics unified, NaN never equal,
    /// temporals field-wise, nodes by id), so EVERY type buckets and the
    /// rev-42 string-only rule with its residual list is gone. A missing
    /// property keys as Null; nothing ever probes with Null (see
    /// `survivors`), so such candidates can never match — correct, since
    /// `null = x` is never True.
    buckets: BTreeMap<Vec<u8>, Vec<u32>>,
}

/// What one incoming row must look at, per the equality index.
enum EqSurvivors<'a> {
    All,
    Bucket(&'a [u32]),
}

impl EqIndex {
    /// The candidates this row can join with. Any outer side evaluating
    /// to Null means NO candidate: `inner = null` is Null, never True, so
    /// the full WHERE could not keep any — an OPTIONAL clause still emits
    /// its null row through the caller's `any` logic.
    fn survivors(
        &self,
        graph: &Graph,
        row: &Row,
        params: &BTreeMap<String, Value>,
    ) -> Result<EqSurvivors<'_>, RunError> {
        static EMPTY: [u32; 0] = [];
        let mut outer = Vec::with_capacity(self.keys.len());
        for (_, rhs) in &self.keys {
            let v = eval_expr(graph, rhs, row, params)?;
            if matches!(v, Value::Null) {
                return Ok(EqSurvivors::Bucket(&EMPTY));
            }
            outer.push(v);
        }
        let mut nonce = 0u64; // a NaN outer keys uniquely: an empty bucket, correctly
        let key = agg_key_of(&outer, &mut nonce);
        Ok(EqSurvivors::Bucket(
            self.buckets.get(&key).map(Vec::as_slice).unwrap_or(&EMPTY),
        ))
    }
}

/// Every non-opaque top-level conjunct of shape `var.key = rhs` (or
/// reversed) whose other side never reads `var` — the correlated
/// equalities worth indexing the memo by, as one composite key.
fn indexable_equalities(where_: Option<&Expr>, var: &str) -> Vec<(String, Expr)> {
    let mut out = Vec::new();
    let Some(w) = where_ else {
        return out;
    };
    let mut conj = Vec::new();
    conjuncts_of(w, &mut conj);
    for c in conj {
        if contains_opaque(&c) {
            continue;
        }
        let Expr::Bin(engram_cypher::BinOp::Eq, a, b) = &c else {
            continue;
        };
        for (side, other) in [(a, b), (b, a)] {
            if let Expr::Prop(base, key) = side.as_ref() {
                if matches!(base.as_ref(), Expr::Var(v) if v == var)
                    && !conjunct_reads_var(other, var)
                {
                    out.push((key.clone(), (**other).clone()));
                    break;
                }
            }
        }
    }
    out
}

/// Whether `e` reads `var` anywhere.
fn conjunct_reads_var(e: &Expr, var: &str) -> bool {
    let mut out = Vec::new();
    free_vars(e, &mut Vec::new(), &mut out);
    out.iter().any(|v| v == var)
}

/// Whether a conjunct's free variables are exactly a subset of `{var}` —
/// the test that splits a pushed prefilter into "part of the memo" (applied
/// once, at build) and "correlated" (applied per row, always).
fn conjunct_only_on(c: &Expr, var: &str) -> bool {
    let mut out = Vec::new();
    free_vars(c, &mut Vec::new(), &mut out);
    out.iter().all(|v| v == var)
}

/// What the clause-scan memo may cache: the scan variable, and the
/// start map's entries when the map is correlated (applied per row).
type MemoScan<'a> = (&'a str, Option<&'a [(String, Expr)]>);

/// Whether this Match clause's pattern is an INDEPENDENT single-node scan:
/// nothing about it reads the incoming row, so its candidate set is
/// identical for every row of the stage and can be scanned ONCE. Measured
/// on the production port (risk-by-country): re-scanning 23k sanctions
/// with a record decode each, once per upstream row, was ~90M decodes —
/// the whole statement's wall clock.
fn independent_single_node<'a>(pattern: &'a Pattern, row: &Row) -> Option<MemoScan<'a>> {
    if pattern.paths.len() != 1 {
        return None;
    }
    let path = &pattern.paths[0];
    if path.shortest || path.var.is_some() || !path.hops.is_empty() {
        return None;
    }
    let var = path.start.var.as_deref()?;
    if row.contains_key(var) {
        return None; // bound: the scan is a lookup, not a population
    }
    // A start map that reads outer variables — `(w:Workflow {workflow_id:
    // e.workflow_id})` — does not make the CANDIDATE SET correlated, only
    // the match: the label's members are the same for every row. The map
    // is returned so the memo builds WITHOUT it and joins BY it (a hash
    // probe per row, the map re-checked per candidate). Measured on the
    // production port: this shape was 9,312 projected gets for 136 rows
    // against 68 candidates, once per row, because the memo refused it.
    // A map that reads the scan variable itself (`{x: w.y}`) stays out.
    let mut correlated: Option<&[(String, Expr)]> = None;
    if let Some(props) = &path.start.props {
        if !conjunct_only_on(props, var) {
            let Expr::Map(entries) = props else {
                return None; // a parameter map or an expression: not indexable
            };
            if entries
                .iter()
                .any(|(_, e)| contains_opaque(e) || conjunct_reads_var(e, var))
            {
                return None;
            }
            correlated = Some(entries.as_slice());
        }
    }
    Some((var, correlated))
}

/// Every property-map expression a path carries (node and relationship
/// maps), for a free-variable check.
fn pattern_prop_exprs(path: &PathPattern) -> Vec<&Expr> {
    let mut out = Vec::new();
    if let Some(p) = &path.start.props {
        out.push(p);
    }
    for (rel, node) in &path.hops {
        if let Some(p) = &rel.props {
            out.push(p);
        }
        if let Some(p) = &node.props {
            out.push(p);
        }
    }
    out
}

/// Recognise and answer a CONSTANT-COUNT stage: the prefix is exactly one
/// MATCH of a single path that reads nothing from the input (no bound
/// variable, no carried variable in its WHERE or property maps), and the
/// WITH / RETURN projects only carried input variables plus one
/// `count(v)` / `count(*)` where `v` is a variable the pattern binds (no
/// DISTINCT, ORDER, SKIP, LIMIT, star, or post-WHERE). The match count is
/// a constant C — the fast count for a bare pattern, or the columnar scan
/// over the pattern as its own statement when it carries props or a
/// WHERE — and the stage is the input's carried groups × C.
///
/// Semantics per row: an OPTIONAL match with zero matches still yields its
/// (null) row, so `count(v)` is 0 and `count(*)` the multiplicity; a
/// non-OPTIONAL match with zero matches yields NO row when there are
/// carried keys, and the single all-aggregate row `0` when there are none.
/// Chains like `OPTIONAL MATCH (s:Bio:Species) WITH count(s) AS a OPTIONAL
/// MATCH (d:Med:Disease) WITH a, count(d) AS b …` (5 s on the production
/// port) and `MATCH (t:EmailThread) WITH count(t) AS threads MATCH
/// (:UserEmail)-[r:IN_THREAD]->(:EmailThread) RETURN threads, count(r)`
/// (4.2 s) each streamed a full population per stage for a number a fast
/// path had in hand.
fn constant_count_stage(
    graph: &Graph,
    prefix: &[Clause],
    proj: &Projection,
    where_: Option<&Expr>,
    input: &[Row],
    input_names: &[String],
    params: &BTreeMap<String, Value>,
) -> Result<Option<Vec<Row>>, RunError> {
    let [
        Clause::Match {
            optional,
            pattern,
            where_: match_where,
        },
    ] = prefix
    else {
        return Ok(None);
    };
    if where_.is_some()
        || proj.star
        || proj.distinct
        || !proj.order.is_empty()
        || proj.skip.is_some()
        || proj.limit.is_some()
        || pattern.paths.len() != 1
    {
        return Ok(None);
    }
    let path = &pattern.paths[0];
    if path.var.is_some() || path.shortest {
        return Ok(None);
    }
    // The pattern's own variables. A bound start is a lookup, not a
    // population; a WHERE or property map over a carried variable is a
    // join — both decline.
    let own = path_vars(path);
    if own.iter().any(|v| input_names.iter().any(|n| n == v)) {
        return Ok(None);
    }
    let mut reads: Vec<String> = Vec::new();
    if let Some(w) = match_where {
        free_vars_of(w, &mut reads);
    }
    for e in pattern_prop_exprs(path) {
        free_vars_of(e, &mut reads);
    }
    if reads.iter().any(|v| !own.contains(v)) {
        return Ok(None);
    }
    let mut carried: Vec<(String, String)> = Vec::new(); // (output name, input name)
    let mut count: Option<(String, bool)> = None; // (alias, is count(*))
    for (i, it) in proj.items.iter().enumerate() {
        let out = it
            .alias
            .clone()
            .or_else(|| it.text.clone())
            .unwrap_or_else(|| column_name(&it.expr, i));
        match &it.expr {
            Expr::Var(v) if input_names.iter().any(|n| n == v) => carried.push((out, v.clone())),
            Expr::Call {
                name,
                distinct: false,
                args,
                star,
            } if name == "count" && count.is_none() => {
                let is_star = *star && args.is_empty();
                let is_var = matches!(args.as_slice(), [Expr::Var(v)] if own.contains(v));
                if !is_star && !is_var {
                    return Ok(None);
                }
                count = Some((out, is_star));
            }
            _ => return Ok(None),
        }
    }
    let Some((count_alias, is_star)) = count else {
        return Ok(None);
    };
    // The constant.
    let n: i64 = if match_where.is_none() && pattern_prop_exprs(path).is_empty() {
        match fast_count_for_path(graph, path, None) {
            Some(n) => {
                sometimes!(
                    "interp.constant count stage answered by the fast count",
                    true
                );
                n as i64
            }
            None => return Ok(None),
        }
    } else {
        let q = SingleQuery {
            clauses: vec![
                Clause::Match {
                    optional: false,
                    pattern: pattern.clone(),
                    where_: match_where.clone(),
                },
                Clause::Return {
                    proj: Projection {
                        distinct: false,
                        star: false,
                        items: vec![engram_cypher::stmt::ProjItem {
                            expr: Expr::Call {
                                name: "count".to_string(),
                                distinct: false,
                                args: Vec::new(),
                                star: true,
                            },
                            alias: Some("c".to_string()),
                            text: None,
                        }],
                        order: Vec::new(),
                        skip: None,
                        limit: None,
                    },
                },
            ],
        };
        match crate::batch::try_columnar_aggregate(graph, &q, params)? {
            Some(res) => match res.rows.first().and_then(|r| r.first()) {
                Some(Value::Int(c)) => {
                    sometimes!(
                        "interp.constant count stage answered by the columnar scan",
                        true
                    );
                    *c
                }
                _ => return Ok(None),
            },
            None => return Ok(None),
        }
    };
    // Rows reaching the WITH per input row: |L| matches, or the one null
    // row an OPTIONAL match keeps when |L| is 0.
    let per_input_rows: i64 = if n == 0 && *optional { 1 } else { n };
    let per_input_count: i64 = if n == 0 && *optional && is_star { 1 } else { n };
    // Group by carried values in first-seen order.
    let mut index: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    let mut groups: Vec<(Vec<Value>, i64)> = Vec::new();
    let mut nonce = 0u64;
    for row in input {
        if per_input_rows == 0 {
            continue; // nothing reaches the WITH from this row
        }
        let vals: Vec<Value> = carried
            .iter()
            .map(|(_, i)| row.get(i).cloned().unwrap_or(Value::Null))
            .collect();
        let key = agg_key_of(&vals, &mut nonce);
        match index.get(&key) {
            Some(&g) => groups[g].1 += per_input_count,
            None => {
                index.insert(key, groups.len());
                groups.push((vals, per_input_count));
            }
        }
    }
    if groups.is_empty() && carried.is_empty() {
        groups.push((Vec::new(), 0)); // a global aggregate over nothing is one row
    }
    let mut rows = Vec::with_capacity(groups.len());
    for (vals, c) in groups {
        let mut out = Row::new();
        for ((o, _), v) in carried.iter().zip(vals) {
            out.insert(o.clone(), v);
        }
        out.insert(count_alias.clone(), Value::Int(c));
        rows.push(out);
    }
    counted!("interp.bare count stages");
    Ok(Some(rows))
}

/// Push one row through a chain of reading clauses into the sink.
#[allow(clippy::too_many_arguments)]
fn drive(
    graph: &Graph,
    clauses: &[Clause],
    seeds: &[Vec<Seed>],
    plan: &StagePlan,
    caches: &ClauseScanCache,
    row: Row,
    params: &BTreeMap<String, Value>,
    sink: RowSink,
) -> Result<(), RunError> {
    let Some((first, rest)) = clauses.split_first() else {
        return sink(row);
    };
    let empty_seeds: Vec<Seed> = Vec::new();
    let (clause_seeds, rest_seeds) = match seeds.split_first() {
        Some((c, r)) => (c, r),
        None => (&empty_seeds, &[] as &[Vec<Seed>]),
    };
    let clause_ix = plan.filters.len() - seeds.len();
    let pushed = plan.filters.get(clause_ix).and_then(|o| o.as_ref());
    match first {
        Clause::Match {
            optional,
            pattern,
            where_,
        } => {
            // Fix 68: the clause WHERE without the conjunct its probe seed
            // satisfies (the plan's `probe_where`), else the clause's own.
            let where_: &Option<Expr> = plan
                .probe_where
                .get(clause_ix)
                .and_then(|o| o.as_ref())
                .unwrap_or(where_);
            // Entry prefilters: conjuncts over pre-bound variables drop the
            // row before any scan runs. Sound by construction — a definite
            // non-True conjunct means the full WHERE could never pass.
            if let Some(pf) = pushed {
                for c in &pf.entry {
                    if !prefilter_pass(graph, c, &row, params)? {
                        if *optional {
                            // The row still surfaces as the null-bound row,
                            // exactly as a failed OPTIONAL match does.
                            sometimes!("interp.optional match produced a null row", true);
                            let mut r = row;
                            for path in &pattern.paths {
                                for v in path_vars(path) {
                                    r.entry(v).or_insert(Value::Null);
                                }
                            }
                            return drive(graph, rest, rest_seeds, plan, caches, r, params, sink);
                        }
                        return Ok(());
                    }
                }
            }
            // ── The independent-scan memo ────────────────────────────
            // One path that reads nothing from the incoming row scans the
            // same candidates for every row of this stage: scan once,
            // replay per row. Correlated prefilters and the full WHERE
            // still run per row, so answers are byte-identical — only the
            // per-row record decodes disappear.
            if let (Some((var, corr_map)), Some(slot)) = (
                independent_single_node(pattern, &row),
                caches.get(clause_ix),
            ) {
                let seed0 = clause_seeds.first().unwrap_or(&Seed::AllNodes);
                // A correlated map is applied per row, never at build: the
                // build scans the map-STRIPPED path from the label (an
                // index-equality seed on the map's key has no row to probe
                // with, so its label fallback drives the build instead).
                let stripped: Option<PathPattern> = corr_map.map(|_| {
                    let mut p = pattern.paths[0].clone();
                    p.start.props = None;
                    p
                });
                let fallback: Seed;
                let build_seed: &Seed = match (corr_map.is_some(), seed0) {
                    (
                        true,
                        Seed::IndexEq {
                            label_fallback: Some(ix),
                            ..
                        },
                    ) => {
                        fallback = Seed::Label(*ix);
                        &fallback
                    }
                    (
                        true,
                        Seed::IndexEq {
                            label_fallback: None,
                            ..
                        },
                    ) => {
                        fallback = Seed::AllNodes;
                        &fallback
                    }
                    (_, s) => s,
                };
                let build_path: &PathPattern = stripped.as_ref().unwrap_or(&pattern.paths[0]);
                // The map's equalities, as the WHERE form would have written
                // them: re-checked per candidate through the evaluator, so
                // Null, NaN and cross-type cases keep Cypher's `=`.
                let map_tests: Vec<Expr> = corr_map
                    .map(|entries| {
                        entries
                            .iter()
                            .map(|(k, e)| {
                                Expr::Bin(
                                    engram_cypher::BinOp::Eq,
                                    Box::new(Expr::Prop(
                                        Box::new(Expr::Var(var.to_string())),
                                        k.clone(),
                                    )),
                                    Box::new(e.clone()),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let state = {
                    let mut b = slot.borrow_mut();
                    match &*b {
                        MemoSlot::Untouched => {
                            *b = MemoSlot::SeenOnce;
                            None // first row: stream, build nothing
                        }
                        MemoSlot::SeenOnce => Some(None),
                        MemoSlot::Built(rc) => Some(Some(std::sync::Arc::clone(rc))),
                    }
                };
                // A correlated map whose key has a DECLARED index for a label
                // the pattern requires is a per-row SEEK, not a memo: the memo
                // would scan the whole label ONCE to answer every row from an
                // equality index, and on a wide label that once is the whole
                // statement — `UNWIND o.watchlist AS ticker OPTIONAL MATCH
                // (c:Company {primaryTicker: ticker})` built its memo over
                // every Company for 31 tickers: 34.7 s against Neo4j's 2.6 ms.
                // The declaration is the operator saying which key to seek;
                // an undeclared key keeps the memo (one scan beats a scan per
                // row, and no build the operator never asked for).
                let declared_correlated = match corr_map {
                    Some(entries) => {
                        let labels = &pattern.paths[0].start.labels;
                        let mut any = false;
                        for (k, _) in entries {
                            if graph.declared_scope_for(labels, k)?.is_some() {
                                any = true;
                                break;
                            }
                        }
                        if any {
                            counted!("interp.clause scan memo declined for a declared correlated key");
                        }
                        any
                    }
                    None => false,
                };
                if let (Some(existing), false) = (
                    state,
                    declared_correlated
                        || matches!(
                            build_seed,
                            Seed::Bound
                                | Seed::Rels
                                | Seed::ById(_)
                                | Seed::PropEq { .. }
                                | Seed::ExistsProbe { .. }
                        ),
                ) {
                    let cached = {
                        match existing {
                            Some(c) => {
                                sometimes!("interp.clause scan memo reused", true);
                                c
                            }
                            None => {
                                // Build-time prefilters, taken from the FULL
                                // WHERE rather than the pushdown plan: the
                                // planner deliberately leaves a conjunct
                                // placeable only at the LAST path unpushed
                                // ("the full WHERE already runs there"), and
                                // for a single-path clause that is every
                                // conjunct. The memo changes the economics —
                                // a var-only conjunct applied ONCE per
                                // candidate at build shrinks every later
                                // row's loop. Sound for the same reason all
                                // prefilters are: it drops only a definite
                                // non-True, which the full WHERE (still run
                                // per row, unchanged) could never keep.
                                let mut build_filters: Vec<Expr> = Vec::new();
                                if let Some(w) = where_ {
                                    let mut conj = Vec::new();
                                    conjuncts_of(w, &mut conj);
                                    for c in conj {
                                        if !contains_opaque(&c) && conjunct_only_on(&c, var) {
                                            build_filters.push(c);
                                        }
                                    }
                                }
                                let mut cands: Vec<Value> = Vec::new();
                                let build_row = Row::new();
                                if corr_map.is_some() {
                                    sometimes!(
                                        "interp.clause scan memo built without its correlated map",
                                        true
                                    );
                                }
                                match_path_stream_demanding(
                                    graph,
                                    build_path,
                                    build_seed,
                                    plan,
                                    &build_row,
                                    params,
                                    where_.as_ref(),
                                    &pattern.paths[0].start.props,
                                    &mut |mut r| {
                                        for c in &build_filters {
                                            if !prefilter_pass(graph, c, &r, params)? {
                                                return Ok(());
                                            }
                                        }
                                        budget_check(graph, cands.len() + 1)?;
                                        cands
                                            .push(r.remove(var).expect("path binds its start var"));
                                        Ok(())
                                    },
                                )?;
                                counted!("interp.clause scan memos built");
                                let eq = {
                                    let mut keys = indexable_equalities(where_.as_ref(), var);
                                    if let Some(entries) = corr_map {
                                        for (k, e) in entries {
                                            keys.push((k.clone(), e.clone()));
                                        }
                                    }
                                    if keys.is_empty() {
                                        None
                                    } else {
                                        let mut buckets: BTreeMap<Vec<u8>, Vec<u32>> =
                                            BTreeMap::new();
                                        let mut nonce = 0u64;
                                        for (i, cand) in cands.iter().enumerate() {
                                            let props = match cand {
                                                Value::Node { props, .. }
                                                | Value::Rel { props, .. } => Some(props),
                                                _ => None,
                                            };
                                            let tuple: Vec<Value> = keys
                                                .iter()
                                                .map(|(k, _)| {
                                                    props
                                                        .and_then(|p| p.get(k))
                                                        .cloned()
                                                        .unwrap_or(Value::Null)
                                                })
                                                .collect();
                                            buckets
                                                .entry(agg_key_of(&tuple, &mut nonce))
                                                .or_default()
                                                .push(i as u32);
                                        }
                                        counted!("interp.clause scan memo indexed");
                                        Some(EqIndex { keys, buckets })
                                    }
                                };
                                let rc = std::sync::Arc::new(CachedScan { cands, eq });
                                *slot.borrow_mut() = MemoSlot::Built(std::sync::Arc::clone(&rc));
                                rc
                            }
                        }
                    };
                    let keep = if *optional { Some(row.clone()) } else { None };
                    let mut any = false;
                    // One working row per OUTER row: candidates overwrite
                    // the scan variable in place, and only rows the
                    // correlated prefilters and the WHERE both keep pay a
                    // real clone. Cloning first was 69M copies of the fat
                    // outer bindings on the production statement, nearly
                    // all of them for rows the very next line dropped.
                    let mut work = row.clone();
                    // The equality index picks which candidates this row
                    // must even look at; a two-pointer merge of the two
                    // scan-ordered index lists keeps emission order
                    // byte-identical to the plain walk.
                    let survivors = match &cached.eq {
                        Some(ix) => ix.survivors(graph, &row, params)?,
                        None => EqSurvivors::All,
                    };
                    // Index lists were built in scan order, so iterating one directly
                    // preserves the plain walk's emission order exactly.
                    let all: Vec<u32>;
                    let indices: &[u32] = match survivors {
                        EqSurvivors::All => {
                            all = (0..cached.cands.len() as u32).collect();
                            &all
                        }
                        EqSurvivors::Bucket(b) => {
                            sometimes!("interp.clause scan answered from the equality index", true);
                            b
                        }
                    };
                    for &ci in indices {
                        let cand = &cached.cands[ci as usize];
                        work.insert(var.to_string(), cand.clone());
                        let mut pruned = false;
                        if let Some(pf) = pushed {
                            if let Some(after) = pf.after_path.first() {
                                for c in after {
                                    if !conjunct_only_on(c, var)
                                        && !prefilter_pass(graph, c, &work, params)?
                                    {
                                        pruned = true;
                                        break;
                                    }
                                }
                            }
                        }
                        if pruned {
                            continue;
                        }
                        // The correlated map, per candidate: the bucket found
                        // it by canonical key; `=` decides it.
                        let mut map_ok = true;
                        for t in &map_tests {
                            let v = eval_expr(graph, t, &work, params)?;
                            if !matches!(v.truth(), Some(Truth::True)) {
                                map_ok = false;
                                break;
                            }
                        }
                        if !map_ok {
                            continue;
                        }
                        if !map_tests.is_empty() {
                            sometimes!("interp.clause scan memo joined by its map", true);
                        }
                        // The WHERE, with its exact legacy semantics.
                        if let Some(w) = where_ {
                            let v = eval_expr(graph, w, &work, params)?;
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
                        any = true;
                        drive(
                            graph,
                            rest,
                            rest_seeds,
                            plan,
                            caches,
                            work.clone(),
                            params,
                            sink,
                        )?;
                    }
                    if !any && *optional {
                        sometimes!("interp.optional match produced a null row", true);
                        let mut r = keep.expect("cloned for optional");
                        for path in &pattern.paths {
                            for v in path_vars(path) {
                                r.entry(v).or_insert(Value::Null);
                            }
                        }
                        drive(graph, rest, rest_seeds, plan, caches, r, params, sink)?;
                    }
                    return Ok(());
                }
            }
            let keep = if *optional { Some(row.clone()) } else { None };
            let mut any = false;
            stream_pattern(
                graph,
                &pattern.paths,
                clause_seeds,
                pushed,
                plan,
                where_.as_ref(),
                row,
                params,
                &mut |r| {
                    any = true;
                    drive(graph, rest, rest_seeds, plan, caches, r, params, sink)
                },
            )?;
            if !any && *optional {
                sometimes!("interp.optional match produced a null row", true);
                let mut r = keep.expect("cloned for optional");
                for path in &pattern.paths {
                    for v in path_vars(path) {
                        r.entry(v).or_insert(Value::Null);
                    }
                }
                drive(graph, rest, rest_seeds, plan, caches, r, params, sink)?;
            }
            Ok(())
        }
        Clause::Unwind { expr, alias } => match eval_expr(graph, expr, &row, params)? {
            Value::Null => Ok(()),
            Value::List(items) => {
                // Dead-variable scope pruning: carry only what the remaining
                // clauses read, so a fat collected list is not cloned into
                // every unwound row and every downstream candidate.
                let carry = if graph.scope_pruning_enabled() {
                    let c = live_carry(rest, plan.live_out.as_ref(), &row, alias);
                    if c.iter().count() < row.iter().filter(|(k, _)| k.as_str() != alias).count() {
                        counted!("interp.unwind pruned a dead scope var");
                    }
                    c
                } else {
                    row.clone()
                };
                for item in items {
                    let mut r = carry.clone();
                    r.insert(alias.clone(), item);
                    drive(graph, rest, rest_seeds, plan, caches, r, params, sink)?;
                }
                Ok(())
            }
            other => Err(RunError::Semantic(format!(
                "UNWIND takes a list, got {}",
                other.type_name()
            ))),
        },
        Clause::With { proj, where_ } => {
            // A PURE With (the breaker split guarantees it): project the row
            // in place — star carries existing bindings, items rebind.
            let mut next = if proj.star { row.clone() } else { Row::new() };
            for (i, item) in proj.items.iter().enumerate() {
                let name = item
                    .alias
                    .clone()
                    .or_else(|| item.text.clone())
                    .unwrap_or_else(|| column_name(&item.expr, i));
                let v = eval_expr(graph, &item.expr, &row, params)?;
                next.insert(name, v);
            }
            if let Some(w) = where_ {
                let v = eval_expr(graph, w, &next, params)?;
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
            drive(graph, rest, rest_seeds, plan, caches, next, params, sink)
        }
        _ => unreachable!("streamable() admits only reading clauses here"),
    }
}

/// A single hop whose START is unbound and whose END is bound in the row
/// runs REVERSED — driven from the bound node's adjacency, O(degree),
/// not a full scan of the unbound start. `(parent)-[:HAS_ELEMENT]->(n)`
/// with `n` bound (the docs hydrate) scanned every node as `parent`: 22 s
/// for 3 ids on the production export, against 4 ms for the bare seek.
///
/// `Some(reversed)` when the shape qualifies: exactly one hop, no path
/// variable (a named path records node/rel order, which reversal flips),
/// not `shortestPath`, not variable-length, the start unbound and bare of
/// a correlated seed, the end var bound to a node in `row`. The reversed
/// path swaps the endpoints and flips the rel direction; the same
/// variables bind to the same nodes, discovered from the other end.
fn reverse_bound_end_path(path: &PathPattern, row: &Row) -> Option<PathPattern> {
    // A path whose START is unbound but whose LAST node IS bound in the
    // incoming row drives from the wrong end: the unbound start seeds a full
    // scan, then filters down to the one bound node — O(all nodes) per row.
    // Reverse the whole fixed-length chain so it starts at the bound node and
    // walks toward the unbound start: O(the bound side's adjacency). This
    // generalises the single-hop hydrate reversal to N hops — LDBC SNB IC5's
    // `(friend)<-[:HAS_CREATOR]-(post)<-[:CONTAINER_OF]-(forum)` with `forum`
    // bound scanned every Person once per forum, 76 s on a 200-person graph.
    if path.shortest || path.var.is_some() || path.hops.is_empty() {
        return None; // a path variable's trail order would change under reversal
    }
    // The start must be genuinely unbound (a bound start already drives right).
    if let Some(v) = &path.start.var {
        if row.contains_key(v) {
            return None;
        }
    }
    // Every hop must be fixed-length; a variable-length hop's reversal (which
    // depends on the min/max being symmetric per intermediate) is not modelled.
    if path.hops.iter().any(|(rel, _)| rel.length.is_some()) {
        return None;
    }
    // The LAST node must be bound to a concrete node. (An intermediate-only
    // bound node is a rarer shape left to the forward path.)
    let (_, last) = path.hops.last().expect("non-empty checked above");
    let last_bound = last
        .var
        .as_ref()
        .is_some_and(|v| matches!(row.get(v), Some(Value::Node { .. })));
    if !last_bound {
        return None;
    }
    Some(reverse_path(path))
}

/// Turn a fixed-length path end for end: the last node becomes the start, the
/// hops run in the opposite order and every relationship direction flips.
///
/// The same variables bind to the same nodes; only the order they are
/// DISCOVERED in changes. Shared by both reversal triggers — the bound-end one
/// above and the index-servable-end one below — so there is exactly one piece
/// of code that knows how to turn a path around.
///
/// The caller is responsible for the structural preconditions (no path
/// variable, no `shortestPath`, no variable-length hop); this function assumes
/// them and would produce a wrong answer without them.
fn reverse_path(path: &PathPattern) -> PathPattern {
    let flip = |d: RelDir| match d {
        RelDir::Out => RelDir::In,
        RelDir::In => RelDir::Out,
        RelDir::Undirected => RelDir::Undirected,
    };
    // Nodes in original order: nodes[0] = start, nodes[i+1] = hops[i].1. The
    // reversed path starts at the last node and, walking the hops from last to
    // first with each rel direction flipped, ends at the original start.
    let mut nodes: Vec<&NodePattern> = Vec::with_capacity(path.hops.len() + 1);
    nodes.push(&path.start);
    for (_, n) in &path.hops {
        nodes.push(n);
    }
    let k = path.hops.len();
    let new_start = nodes[k].clone();
    let mut new_hops: Vec<(RelPattern, NodePattern)> = Vec::with_capacity(k);
    for i in (0..k).rev() {
        let (rel, _) = &path.hops[i];
        new_hops.push((
            RelPattern {
                dir: flip(rel.dir),
                ..rel.clone()
            },
            nodes[i].clone(),
        ));
    }
    PathPattern {
        var: None,
        shortest: false,
        start: new_start,
        hops: new_hops,
    }
}

/// The inline `{prop: value}` key a node pattern offers to the range index, if
/// any. `(p:Person {id: 7})` offers `id`; `(p:Person)` offers nothing.
fn inline_index_key(n: &NodePattern) -> Option<String> {
    match &n.props {
        Some(Expr::Map(entries)) => entries.first().map(|(k, _)| k.clone()),
        _ => None,
    }
}

/// Reverse a path so it drives from an INDEX-SERVABLE end rather than from
/// whichever end happened to be typed first.
///
/// # The defect
///
/// Seed selection consults `path.start` and nothing else, so
/// `MATCH (m:Msg)-[:BY]->(p:Person {pid: 7})` scans every `:Msg` and applies
/// `{pid: 7}` as a post-hop filter, while the identical
/// `MATCH (p:Person {pid: 7})<-[:BY]-(m:Msg)` seeks one indexed row. Measured
/// in-process on a 40,000-message corpus: **28.0 ms against 0.163 ms — 172x**,
/// from nothing but which end was written first. Over Bolt against a 100k-node
/// LDBC SNB corpus the same shape (IS5) cost 24.6x.
///
/// This matters disproportionately for a drop-in replacement: the database
/// being replaced picks its anchor by selectivity, so applications are full of
/// patterns written the "wrong" way round — there is no wrong way round there.
///
/// # When it fires
///
/// Only when reversing is unambiguously the better plan:
///
///  - the start is unbound and offers NO seed of its own (no inline map — an
///    id/property seek from the WHERE is handled by the seed plan and takes
///    precedence, so a start with one never reaches here);
///  - the last node DOES offer an inline equality map;
///  - the last node's label is no larger than the start's, so if the index
///    probe loses at execution the fallback scan is over the smaller label.
///
/// That last guard is what keeps this a selectivity decision rather than a
/// blanket preference for the far end. Without it,
/// `(a:Tiny)-[:R]->(b:Huge {unindexed: 1})` would reverse into a scan of
/// `Huge` — trading a small scan for a large one.
fn reverse_to_selective_end(
    graph: &Graph,
    path: &PathPattern,
    row: &Row,
) -> Option<PathPattern> {
    if !graph.property_seek_enabled() || !graph.selective_anchor_enabled() {
        return None;
    }
    // Structural gates, identical to the bound-end reversal: a path variable
    // records node/rel order that reversal flips, and a variable-length hop's
    // reversal is not modelled.
    if path.shortest || path.var.is_some() || path.hops.is_empty() {
        return None;
    }
    if path.hops.iter().any(|(rel, _)| rel.length.is_some()) {
        return None;
    }
    // A bound start already drives from the right end.
    if let Some(v) = &path.start.var {
        if row.contains_key(v) {
            return None;
        }
    }
    // A start that can seed itself keeps the plan it has.
    if inline_index_key(&path.start).is_some() {
        return None;
    }
    let (_, last) = path.hops.last().expect("non-empty checked above");
    inline_index_key(last)?;
    // A bound last node is the other reversal's case, which ran first.
    if last.var.as_ref().is_some_and(|v| row.contains_key(v)) {
        return None;
    }
    // The cost guard. `count_label_nodes` over an unlabelled pattern has no
    // answer, so an unlabelled end is only taken when the start is unlabelled
    // too (both scan everything; the seek can only help).
    let size = |n: &NodePattern| -> u64 {
        n.labels
            .iter()
            .map(|l| graph.count_label_nodes(l))
            .min()
            .unwrap_or(u64::MAX)
    };
    if size(last) > size(&path.start) {
        return None;
    }
    sometimes!("interp.path driven from its index-servable end", true);
    Some(reverse_path(path))
}

/// Stream a pattern's paths as nested expansions; the WHERE applies after
/// every path has bound, exactly as the materialising path does.
#[allow(clippy::too_many_arguments)]
fn stream_pattern(
    graph: &Graph,
    paths: &[PathPattern],
    seeds: &[Seed],
    pushed: Option<&PushedFilters>,
    plan: &StagePlan,
    where_: Option<&Expr>,
    row: Row,
    params: &BTreeMap<String, Value>,
    sink: RowSink,
) -> Result<(), RunError> {
    let Some((p0, rest)) = paths.split_first() else {
        if let Some(w) = where_ {
            let v = eval_expr(graph, w, &row, params)?;
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
        return sink(row);
    };
    let (seed0, rest_seeds) = match seeds.split_first() {
        Some((c, r)) => (c, r),
        None => (&Seed::AllNodes, &[] as &[Seed]),
    };
    // Which path index this recursion level is executing.
    let path_ix = pushed
        .map(|pf| pf.after_path.len().saturating_sub(paths.len()))
        .unwrap_or(0);
    // Only the first path of the pattern reads the clause WHERE for its
    // seed: later paths' conjuncts are the pushed prefilters' business.
    let first_path = pushed
        .map(|pf| pf.after_path.len() == paths.len())
        .unwrap_or(true);
    let seed_where = if first_path { where_ } else { None };
    match_path_stream(graph, p0, seed0, plan, &row, params, seed_where, &mut |r| {
        if let Some(pf) = pushed {
            if let Some(after) = pf.after_path.get(path_ix) {
                for c in after {
                    if !prefilter_pass(graph, c, &r, params)? {
                        return Ok(()); // pruned before later paths fan out
                    }
                }
            }
        }
        stream_pattern(
            graph, rest, rest_seeds, pushed, plan, where_, r, params, sink,
        )
    })
}

/// Stream one path's matches from a seed row — one start candidate
/// materialised at a time, hops recursing depth-first, completions pushed
/// to the sink and dropped.
#[allow(clippy::too_many_arguments)]
fn match_path_stream(
    graph: &Graph,
    path: &PathPattern,
    seed_plan: &Seed,
    plan: &StagePlan,
    seed: &Row,
    params: &BTreeMap<String, Value>,
    clause_where: Option<&Expr>,
    sink: RowSink,
) -> Result<(), RunError> {
    match_path_stream_demanding(
        graph,
        path,
        seed_plan,
        plan,
        seed,
        params,
        clause_where,
        &path.start.props,
        sink,
    )
}

/// [`match_path_stream`] with the start node's property DEMAND taken from
/// `demand_map` rather than the path's own map: the clause-scan memo
/// streams a map-STRIPPED path (a correlated map has no row to evaluate
/// against at build) but must still materialise the map's keys, or every
/// cached candidate would carry Null where the join key should be.
#[allow(clippy::too_many_arguments)]
fn match_path_stream_demanding(
    graph: &Graph,
    path: &PathPattern,
    seed_plan: &Seed,
    plan: &StagePlan,
    seed: &Row,
    params: &BTreeMap<String, Value>,
    clause_where: Option<&Expr>,
    demand_map: &Option<Expr>,
    sink: RowSink,
) -> Result<(), RunError> {
    debug_assert!(!path.shortest, "shortestPath takes the materialising path");
    // Reverse a bound-end single hop FIRST — before the relationship-driven
    // seed, which for `(parent)-[:HAS_ELEMENT]->(n)` with n bound would
    // scan the WHOLE HAS_ELEMENT partition (millions of edges) to find the
    // few ending at n. The reversed path starts at the bound node, so it
    // drives from n's incoming adjacency: O(degree), the hydrate's fix.
    let start_pre_bound = path
        .start
        .var
        .as_ref()
        .is_some_and(|v| seed.contains_key(v));
    if !start_pre_bound && graph.hop_reversal_enabled() {
        if let Some(rev) = reverse_bound_end_path(path, seed) {
            sometimes!("interp.hop driven from its bound end", true);
            return match_path_stream_demanding(
                graph,
                &rev,
                &Seed::Bound,
                plan,
                seed,
                params,
                clause_where,
                &rev.start.props,
                sink,
            );
        }
        // Then the same reversal for an INDEX-SERVABLE far end — the case where
        // neither endpoint is bound but one of them names a row the range index
        // can find. Runs after the bound-end check because a bound endpoint is
        // strictly more selective than an indexed one, and its `Seed::Bound` is
        // cheaper than any probe.
        //
        // The new seed is computed from the REVERSED start, not inherited: the
        // incoming `seed_plan` describes the original start, and reusing it
        // would seed the new plan from the wrong node's predicate.
        if let Some(rev) = reverse_to_selective_end(graph, path, seed) {
            let key = inline_index_key(&rev.start)
                .expect("reverse_to_selective_end requires an inline map on the new start");
            let label_fallback = if rev.start.labels.is_empty() {
                None
            } else {
                // Smallest label, matching the seed plan's own tie-break: the
                // fallback only runs when the probe loses, and it should then
                // scan the cheapest label the pattern allows.
                let mut best = 0usize;
                let mut best_n = u64::MAX;
                for (i, l) in rev.start.labels.iter().enumerate() {
                    let n = graph.count_label_nodes(l);
                    if n < best_n {
                        best_n = n;
                        best = i;
                    }
                }
                Some(best)
            };
            return match_path_stream_demanding(
                graph,
                &rev,
                &Seed::IndexEq {
                    key,
                    label_fallback,
                },
                plan,
                seed,
                params,
                clause_where,
                &rev.start.props,
                sink,
            );
        }
    }
    if matches!(seed_plan, Seed::Rels) {
        return rel_driven_stream(graph, path, plan, seed, params, sink);
    }
    let want_trail = path.var.is_some();
    // Frontier-BFS eligibility: a lone bounded `*1..n` hop, no path/rel variable
    // and no rel-property test, whose end node the breaker consumes DISTINCT-
    // only. When all hold, the hop runs set-at-a-time over a visited set.
    let frontier_ok = graph.frontier_expand_enabled()
        && path.var.is_none()
        && path.hops.len() == 1
        && {
            let (rel_pat, node_pat) = &path.hops[0];
            rel_pat.var.is_none()
                && rel_pat.props.is_none()
                && matches!(rel_pat.length, Some(vl) if vl.min.unwrap_or(1) == 1 && vl.max.is_some())
                && node_pat
                    .var
                    .as_ref()
                    .is_some_and(|v| plan.frontier_vars.contains(v))
        };
    // A path VARIABLE (`p`) can expose ANY trail node's properties via `nodes(p)`,
    // so — like the peer nodes, which `expand_var_length` materialises in FULL —
    // the start node must carry all its properties, not just the demand set
    // (which is empty for an ANONYMOUS start, dropping `nodes(p)[0]`'s props).
    // Without a path variable the narrower demand set still stands.
    let start_set = if want_trail {
        None
    } else {
        plan.props_for(path.start.var.as_ref(), demand_map)
    };
    let hop_sets: Vec<Option<std::collections::BTreeSet<String>>> = path
        .hops
        .iter()
        .map(|(_, node_pat)| plan.props_for(node_pat.var.as_ref(), &node_pat.props))
        .collect();
    // Fix 73: per hop, a presence-only relationship variable binds lean and
    // a var-free one-key map on a declared key resolves once (memoised on
    // the graph, so every row of the stage shares the set).
    let mut hop_lean: Vec<HopLeanPlan> = Vec::with_capacity(path.hops.len());
    for (rel_pat, node_pat) in &path.hops {
        let rel_lean = rel_pat.var.is_some()
            && !want_trail
            && matches!(
                plan.props_for(rel_pat.var.as_ref(), &rel_pat.props),
                Some(ref s) if s.is_empty()
            );
        let resolved = if !want_trail
            && node_pat.var.as_ref().is_none_or(|v| !seed.contains_key(v))
        {
            resolve_constant_end(graph, node_pat, params)?
                .map(|ids| (ids, plan.props_for(node_pat.var.as_ref(), &None)))
        } else {
            None
        };
        hop_lean.push((rel_lean, resolved));
    }
    let start_bound = path.start.var.as_ref().and_then(|v| seed.get(v)).cloned();
    let handle_start = |cand: Value, sink: RowSink| -> Result<(), RunError> {
        if !node_satisfies(graph, &cand, &path.start, seed, params)? {
            return Ok(());
        }
        let Value::Node { id, .. } = &cand else {
            unreachable!("candidates are nodes")
        };
        let mut row = seed.clone();
        let at = *id;
        if let Some(v) = &path.start.var {
            row.insert(v.clone(), cand.clone());
        }
        let partial = Partial {
            row,
            at,
            used: Vec::new(),
            trail: if want_trail { vec![cand] } else { Vec::new() },
        };
        hops_stream(
            graph,
            path,
            &hop_sets,
            &hop_lean,
            want_trail,
            frontier_ok,
            0,
            partial,
            params,
            sink,
        )
    };
    // Fix 64: a bound start with no inline map to test is the row's own
    // value — the executor's rule since fix 51, never applied here. The
    // earlier clause bound it under this stage's whole demand (what every
    // later clause and the sink read of it), so the row's node already
    // carries what the pattern tests and the sink read. The UserTrack
    // listing (`MATCH (n:UserTrack {userId: $u}) OPTIONAL MATCH
    // (n)-[:PERFORMED_BY]->(a) RETURN properties(n), a.title …`) re-read
    // every track IN FULL for the OPTIONAL hop: 1,668 full decodes for 834
    // rows. Kept on the re-read: a path variable (its trail wants the whole
    // node), a demand map (the clause-scan memo's stripped path), and a
    // pattern label the bound node does not list — a lean binding carries
    // only its own pattern's labels, and the re-read has them all.
    let reuse_bound = match &start_bound {
        Some(Value::Node { labels, .. }) => {
            path.start.props.is_none()
                && demand_map.is_none()
                && !want_trail
                && path.start.labels.iter().all(|l| labels.contains(l))
        }
        _ => false,
    };
    match start_bound {
        Some(node @ Value::Node { .. }) if reuse_bound => {
            counted!("interp.stream matcher reused the bound start");
            handle_start(node, sink)
        }
        Some(Value::Node { id, .. }) => {
            // The row already holds the binding; re-materialise under the
            // projection so pattern tests see what they need.
            let n =
                mat_node(graph, id, start_set.as_ref())?.ok_or(GraphError::Missing("node", id))?;
            handle_start(n, sink)
        }
        Some(Value::Null) => Ok(()),
        Some(other) => Err(RunError::Semantic(format!(
            "`{}` is bound to a {}, not a node",
            path.start.var.as_deref().unwrap_or("?"),
            other.type_name()
        ))),
        None => {
            // The identity seek: one get, then the pattern's own tests.
            if let Seed::ById(e) = seed_plan {
                let v = eval_expr(graph, e, seed, params)?;
                let id = match &v {
                    Value::Int(i) if *i >= 0 => Some(*i as u64),
                    Value::Str(t) => t.strip_prefix("n:").and_then(|d| d.parse::<u64>().ok()),
                    _ => None,
                };
                sometimes!("interp.seed looked a node up by its id", true);
                if let Some(id) = id {
                    if let Some(n) = mat_node(graph, id, start_set.as_ref())? {
                        handle_start(n, sink)?;
                    }
                }
                return Ok(());
            }
            // The property-equality SEEK: probe the range index, and take it
            // only when it beats the label scan (as `IndexEq` does). The
            // probe returns ids across all labels carrying the value, so the
            // pattern's labels/props/WHERE still run per candidate.
            if let Seed::PropEq {
                prop,
                values,
                label_fallback,
            } = seed_plan
            {
                let label =
                    label_fallback.and_then(|ix| path.start.labels.get(ix).map(String::as_str));
                let probed = if graph.property_seek_worth_probing(label) {
                    let vs: Vec<Value> = values
                        .iter()
                        .map(|e| eval_expr(graph, e, seed, params))
                        .collect::<Result<_, _>>()?;
                    // The seed's own equality FIRST — it keeps the unscoped
                    // probe it always had when undeclared — then every OTHER
                    // key the start offers (the inline map's entries, the
                    // WHERE's other equalities), each only when DECLARED for
                    // a label this pattern requires; the most selective wins
                    // (`best_declared_seek`). The first conjunct is where the
                    // author put it, not the most selective one: the email
                    // listing's WHERE led with `classified = true` (a boolean
                    // no index orders) and its map held the 10-row `userId`.
                    let mut cands: Vec<(String, Vec<Value>)> = vec![(prop.clone(), vs)];
                    for (k, v) in seek_candidates(graph, path, clause_where, seed, params)? {
                        if k != *prop && graph.declared_scope_for(&path.start.labels, &k)?.is_some()
                        {
                            cands.push((k, v));
                        }
                    }
                    best_declared_seek(
                        graph,
                        &path.start.labels,
                        &cands,
                        crate::PROPERTY_SEEK_MAX_PROBE,
                    )?
                } else {
                    None
                };
                if let Some((winner, ids)) = probed {
                    let use_index = graph.property_seek_wins(label, ids.len());
                    if use_index {
                        seed_chose_later(winner);
                        sometimes!("interp.seed sought a property index", true);
                        match lean_starts_from_columns(graph, &ids, &path.start, start_set.as_ref(), params)? {
                            Some(starts) => {
                                for n in starts {
                                    handle_start(n, sink)?;
                                }
                            }
                            None => {
                                for id in ids {
                                    if let Some(n) = mat_node(graph, id, start_set.as_ref())? {
                                        handle_start(n, sink)?;
                                    }
                                }
                            }
                        }
                        return Ok(());
                    }
                }
                // The probe declined (non-indexable value) or the label is
                // smaller: fall through to the label scan below.
            }
            // An index-served point lookup, when the probe answers AND beats
            // the label scan; the label path is the always-correct fallback.
            if let Seed::IndexEq {
                key,
                label_fallback,
            } = seed_plan
            {
                // Every key the start offers — the map's entries (the plan's
                // `key` is the first of them) and the WHERE's equalities —
                // kept when it IS the plan's key or is DECLARED for a label
                // this pattern requires: the most selective under the seek
                // cap wins (`best_declared_seek`). The map's first key used to
                // be the ONLY probe, and on `{nodeType: 'email', userId: $u}`
                // it answered 38k of the label's 38.5k ids, "won", and every
                // shape on that start materialised 38k email records.
                let cands: Vec<(String, Vec<Value>)> = {
                    let mut c = Vec::new();
                    for (k, v) in seek_candidates(graph, path, clause_where, seed, params)? {
                        if k == *key || graph.declared_scope_for(&path.start.labels, &k)?.is_some() {
                            c.push((k, v));
                        }
                    }
                    c
                };
                let mut probed: Option<(usize, Vec<u64>)> = best_declared_seek(
                    graph,
                    &path.start.labels,
                    &cands,
                    crate::PROPERTY_SEEK_MAX_PROBE,
                )?;
                if probed.is_none() {
                    // Nothing under the cap: the map's first key exactly as
                    // before — uncapped, scoped when declared — so a wide
                    // single-key map keeps the probe-vs-label decision it had.
                    let probe_value = match &path.start.props {
                        Some(Expr::Map(entries)) => entries
                            .iter()
                            .find(|(k, _)| k == key)
                            .map(|(_, e)| eval_expr(graph, e, seed, params))
                            .transpose()?,
                        _ => None,
                    };
                    if let Some(v) = probe_value {
                        // Fix 71: an UNDECLARED key's probe walks the
                        // PARTITION-WIDE index, and its answer only wins when
                        // it is no wider than the label (`use_index` below)
                        // — so it is capped at the label's size + 1: over
                        // that it would lose anyway, and every id it
                        // extracted would be dropped. `MATCH (n:UserTrack
                        // {userId: $u}) …` — no index on either engine — pulled
                        // the user's ~20k ids across every label out of the
                        // `userId` index to keep none of them for an
                        // 834-member label (1.8 ms against Neo4j's 0.5 for
                        // 0 rows).
                        let fallback_cap: Option<usize> = label_fallback
                            .and_then(|ix| path.start.labels.get(ix))
                            .map(|l| graph.count_label_nodes(l) as usize + 1);
                        probed = match graph.declared_scope_for(&path.start.labels, key)?.as_deref()
                        {
                            Some(l) => {
                                counted!("interp.seed probed a declared scoped index");
                                graph.index_probe_eq_scoped(key, &v, None, Some(l))?
                            }
                            None => {
                                let r = graph.index_probe_eq(key, &v, fallback_cap)?;
                                if r.is_none() && fallback_cap.is_some() {
                                    counted!("interp.seed undeclared probe capped at the label");
                                }
                                r
                            }
                        }
                        .map(|ids| (0, ids));
                    }
                }
                if let Some((winner, ids)) = probed {
                    let label = label_fallback
                        .and_then(|ix| path.start.labels.get(ix).map(String::as_str));
                    let use_index = match label {
                        Some(l) => graph.count_label_nodes(l) >= ids.len() as u64,
                        None => true,
                    };
                    if use_index {
                        seed_chose_later(winner);
                        sometimes!("interp.seed probed a range index", true);
                        match lean_starts_from_columns(graph, &ids, &path.start, start_set.as_ref(), params)? {
                            Some(starts) => {
                                for n in starts {
                                    handle_start(n, sink)?;
                                }
                            }
                            None => {
                                for id in ids {
                                    if let Some(n) = mat_node(graph, id, start_set.as_ref())? {
                                        handle_start(n, sink)?;
                                    }
                                }
                            }
                        }
                        return Ok(());
                    }
                }
            }
            // Fix 49: the existence probe's constant end names the starts —
            // walk the reversed path once (its start seeks; `w` is its last
            // node), keep the label's members, and start from those ids in
            // id order, exactly the order the label scan would have visited
            // them in. The conjunct itself still runs per candidate.
            if let Seed::ExistsProbe { path: rp, .. } = seed_plan {
                let sv = path
                    .start
                    .var
                    .as_deref()
                    .expect("an existence probe names its start");
                // Fix 68: only the IDS of `sv` are read from the probe's
                // rows, so every node of the reversed path binds to an
                // EMPTY demand (labels and map keys alone — a bare end is
                // built from its adjacency, no record read) and no
                // relationship record is fetched. The walk that seeded the
                // KM listing decoded all 197 items and their 197
                // BELONGS_TO_PROJECT records in full to keep 197 ids.
                let mut lean: BTreeMap<String, VarDemand> = BTreeMap::new();
                for v in path_vars(rp) {
                    lean.insert(v, VarDemand::Props(Default::default()));
                }
                counted!("interp.seed probe walked its path lean");
                let probe_rows = match_path_with(graph, rp, seed, params, false, Some(&lean))?;
                let mut ids: Vec<u64> = probe_rows
                    .iter()
                    .filter_map(|r| match r.get(sv) {
                        Some(Value::Node { id, .. }) => Some(*id),
                        _ => None,
                    })
                    .collect();
                ids.sort_unstable();
                ids.dedup();
                if !path.start.labels.is_empty() {
                    let members = graph.members_all(&path.start.labels)?;
                    ids.retain(|id| graph.members_contains(&members, *id));
                }
                counted!("interp.seed driven from an existence probe's constant end");
                match lean_starts_from_columns(graph, &ids, &path.start, start_set.as_ref(), params)? {
                    Some(starts) => {
                        for n in starts {
                            handle_start(n, sink)?;
                        }
                    }
                    None => {
                        for id in ids {
                            if let Some(n) = mat_node(graph, id, start_set.as_ref())? {
                                handle_start(n, sink)?;
                            }
                        }
                    }
                }
                return Ok(());
            }
            let label = match seed_plan {
                Seed::Label(ix) => path.start.labels.get(*ix).map(String::as_str),
                Seed::IndexEq {
                    label_fallback: Some(ix),
                    ..
                }
                | Seed::PropEq {
                    label_fallback: Some(ix),
                    ..
                }
                | Seed::ExistsProbe {
                    label_fallback: Some(ix),
                    ..
                } => path.start.labels.get(*ix).map(String::as_str),
                _ => path.start.labels.first().map(String::as_str),
            };
            // The column-filtered seed: the WHERE conjuncts reading only the
            // start variable are evaluated from columns first, and only the
            // survivors are materialised (sound prefilter — the full WHERE
            // still runs at its position). The start's INLINE MAP joins the
            // conjuncts, entry by entry, when its values read no variable:
            // `(t:ResearchTask {userId: $u})` with no index on the key reached
            // this scan and materialised the whole label in full (517 records
            // per statement on the mirror) for `node_satisfies` to test the
            // map, where the WHERE form of the same test read one cached
            // column. The map is still tested per candidate afterwards.
            if let Some(sv) = path.start.var.as_deref() {
                let mut conj = Vec::new();
                if let Some(w) = clause_where {
                    conjuncts_of(w, &mut conj);
                }
                if let Some(Expr::Map(entries)) = &path.start.props {
                    for (k, e) in entries {
                        let mut fv = Vec::new();
                        free_vars(e, &mut Vec::new(), &mut fv);
                        if fv.is_empty() {
                            counted!("interp.seed column filter took the pattern map");
                            conj.push(Expr::Bin(
                                engram_cypher::BinOp::Eq,
                                Box::new(Expr::Prop(
                                    Box::new(Expr::Var(sv.to_string())),
                                    k.clone(),
                                )),
                                Box::new(e.clone()),
                            ));
                        }
                    }
                }
                let own: Vec<Expr> = conj
                    .into_iter()
                    .filter(|c| !contains_opaque(c) && conjunct_only_on(c, sv))
                    .collect();
                if !own.is_empty() {
                    let pred = own
                        .into_iter()
                        .reduce(|a, b| Expr::And(Box::new(a), Box::new(b)))
                        .expect("non-empty");
                    if let Some(ids) =
                        crate::batch::filter_ids(graph, &path.start.labels, sv, &pred, params)?
                    {
                        match lean_starts_from_columns(graph, &ids, &path.start, start_set.as_ref(), params)? {
                            Some(starts) => {
                                for n in starts {
                                    handle_start(n, sink)?;
                                }
                            }
                            None => {
                                for id in ids.iter() {
                                    if let Some(n) = mat_node(graph, *id, start_set.as_ref())? {
                                        handle_start(n, sink)?;
                                    }
                                }
                            }
                        }
                        return Ok(());
                    }
                }
            }
            // Fix 52: a plain limit over a lone label scan needs only its
            // first `cap` members — the batch path below would assemble the
            // WHOLE label's columns first, and the column-bound start below
            // would read whole columns for a handful of rows.
            let seed_cap = if matches!(seed_plan, Seed::Label(_)) {
                plan.seed_cap(graph, params)?
            } else {
                None
            };
            if let Some(cap) = seed_cap {
                let mut ids = graph.nodes_by_label(label)?;
                if ids.len() > cap {
                    ids.truncate(cap);
                }
                counted!("interp.seed scan cut at the plain limit");
                for id in ids {
                    if let Some(n) = mat_node(graph, id, start_set.as_ref())? {
                        handle_start(n, sink)?;
                    }
                }
                return Ok(());
            }
            // Fix 61: the "candidate batch" path that stood here — one
            // PARTITION-WIDE column scan per demanded property plus the
            // label-set column, filtered to the label's members — is
            // retired. Its share gate (the label must be a quarter of all
            // nodes) admitted the 15k-member KMWorkItem label on the paged
            // mirror, and the moment fix 56 gave the visibility listing's
            // `w` a property demand, its seed walked the whole 5M-record
            // store six times: 341k block misses, 96 s against Neo4j's 112
            // ms, +9 GB. `lean_starts_from_columns` below is the same intent
            // done right — the label's own columns, span-bounded and
            // byte-budgeted, gathered by member otherwise, and kept in the
            // property-column cache for the next statement.
            let ids = graph.nodes_by_label(label)?;
            match lean_starts_from_columns(graph, &ids, &path.start, start_set.as_ref(), params)? {
                Some(starts) => {
                    for n in starts {
                        handle_start(n, sink)?;
                    }
                }
                None => {
                    for id in ids {
                        if let Some(n) = mat_node(graph, id, start_set.as_ref())? {
                            handle_start(n, sink)?;
                        }
                    }
                }
            }
            Ok(())
        }
    }
}

/// Drive a single unconstrained-start hop from the relationship partition:
/// every relationship of the pattern's types visits exactly once, endpoints
/// materialise only when the pattern binds or tests them. The node-driven
/// plan for this shape visited every node in the database first.
fn rel_driven_stream(
    graph: &Graph,
    path: &PathPattern,
    plan: &StagePlan,
    seed: &Row,
    params: &BTreeMap<String, Value>,
    sink: RowSink,
) -> Result<(), RunError> {
    sometimes!("interp.seed drove from relationships", true);
    let (rel_pat, end_pat) = &path.hops[0];
    let types = if rel_pat.types.is_empty() {
        None
    } else {
        Some(rel_pat.types.clone())
    };
    let start_set = plan.props_for(path.start.var.as_ref(), &None);
    let end_set = plan.props_for(end_pat.var.as_ref(), &end_pat.props);
    let end_constrained = !end_pat.labels.is_empty() || end_pat.props.is_some();
    let end_bound: Option<u64> = end_pat.var.as_ref().and_then(|v| match seed.get(v) {
        Some(Value::Node { id, .. }) => Some(*id),
        _ => None,
    });
    // Both endpoints named by the SAME variable (`(n)-[r]->(n)`) constrain the
    // hop to a self-loop: start and end must be one node, or the single binding
    // of `n` would be inconsistent (the end write would silently clobber the
    // start). Anonymous / distinct-var ends are unaffected.
    let same_endpoint_var = path.start.var.is_some() && path.start.var == end_pat.var;
    // A relationship variable ALREADY bound (carried in from a prior clause,
    // e.g. `WITH r AS r2 MATCH ()-[r2]->()`) pins this hop to that one edge —
    // the rel scan must not re-offer every relationship. `None` = not bound;
    // `Some(Some(id))` = match only that id; `Some(None)` = bound to a non-Rel
    // (e.g. Null) → nothing matches.
    let rel_bound: Option<Option<u64>> =
        rel_pat
            .var
            .as_ref()
            .and_then(|v| seed.get(v))
            .map(|val| match val {
                Value::Rel { id, .. } => Some(*id),
                _ => None,
            });
    let mut inner = |rel: crate::RelRow| -> Result<(), RunError> {
        if let Some(want) = rel_bound {
            if want != Some(rel.id) {
                return Ok(());
            }
        }
        if !rel_satisfies(graph, &rel, &rel_pat.props, seed, params)? {
            return Ok(());
        }
        let orientations: &[(u64, u64)] = match rel_pat.dir {
            RelDir::Out => &[(rel.src, rel.dst)],
            RelDir::In => &[(rel.dst, rel.src)],
            // A self-loop matches an undirected pattern EXACTLY ONCE — its two
            // orientations are the same binding, so they must not double-count.
            RelDir::Undirected if rel.src == rel.dst => &[(rel.src, rel.dst)],
            RelDir::Undirected => &[(rel.src, rel.dst), (rel.dst, rel.src)],
        };
        let rel_value = Value::Rel {
            id: rel.id,
            src: rel.src,
            dst: rel.dst,
            rel_type: rel.rel_type.clone(),
            props: rel.props.clone(),
        };
        for &(a_id, b_id) in orientations {
            if same_endpoint_var && a_id != b_id {
                continue; // `(n)-…->(n)` matches only a self-loop
            }
            if let Some(want) = end_bound {
                if b_id != want {
                    continue;
                }
            }
            let mut row = seed.clone();
            if let Some(v) = &path.start.var {
                let a = mat_node(graph, a_id, start_set.as_ref())?
                    .ok_or(GraphError::Missing("node", a_id))?;
                row.insert(v.clone(), a);
            }
            if end_constrained || end_pat.var.is_some() {
                let b = mat_node(graph, b_id, end_set.as_ref())?
                    .ok_or(GraphError::Missing("node", b_id))?;
                if !node_satisfies(graph, &b, end_pat, seed, params)? {
                    continue;
                }
                if let Some(v) = &end_pat.var {
                    if end_bound.is_none() {
                        row.insert(v.clone(), b);
                    }
                }
            }
            if let Some(v) = &rel_pat.var {
                row.insert(v.clone(), rel_value.clone());
            }
            sink(row)?;
        }
        Ok(())
    };
    let mut ierr: Option<RunError> = None;
    let scan = graph.for_each_rel(types.as_deref(), &mut |rel| match inner(rel) {
        Ok(()) => Ok(()),
        Err(e) => {
            ierr = Some(e);
            // Stop the scan; the recorded error rethrows below.
            Err(GraphError::Missing("sink", 0))
        }
    });
    if let Some(e) = ierr {
        return Err(e);
    }
    scan?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
/// Fix 73: per hop of a streamed path, the relationship variable's
/// presence-only flag and the end's resolved set + residual demand.
type HopLeanPlan = (
    bool,
    Option<(std::sync::Arc<Vec<u64>>, Option<std::collections::BTreeSet<String>>)>,
);

#[allow(clippy::too_many_arguments)]
fn hops_stream(
    graph: &Graph,
    path: &PathPattern,
    hop_sets: &[Option<std::collections::BTreeSet<String>>],
    hop_lean: &[HopLeanPlan],
    want_trail: bool,
    frontier: bool,
    hop: usize,
    partial: Partial,
    params: &BTreeMap<String, Value>,
    sink: RowSink,
) -> Result<(), RunError> {
    if hop == path.hops.len() {
        let mut row = partial.row;
        if let Some(v) = &path.var {
            row.insert(v.clone(), Value::Path(partial.trail.clone()));
        }
        return sink(row);
    }
    let (rel_pat, node_pat) = &path.hops[hop];
    let dir = match rel_pat.dir {
        RelDir::Out => Dir::Out,
        RelDir::In => Dir::In,
        RelDir::Undirected => Dir::Both,
    };
    let types = if rel_pat.types.is_empty() {
        None
    } else {
        Some(rel_pat.types.clone())
    };
    let (min, max) = match rel_pat.length {
        None => (1, Some(1)),
        Some(vl) => (vl.min.unwrap_or(1), vl.max),
    };
    // The caller certifies frontier eligibility for the whole path (one hop,
    // min 1, bounded, no rel/path var, DISTINCT-only endpoint), so this hop
    // runs as a frontier BFS and the emit closure just completes the path.
    if frontier {
        let max = max.expect("frontier eligibility requires a bounded hop");
        return expand_var_length_bfs(
            graph,
            &partial,
            dir,
            types.as_deref(),
            node_pat,
            max,
            params,
            hop_sets.get(hop).and_then(|o| o.as_ref()),
            &mut |p2| {
                hops_stream(
                    graph,
                    path,
                    hop_sets,
                    hop_lean,
                    want_trail,
                    false,
                    hop + 1,
                    p2,
                    params,
                    sink,
                )
            },
        );
    }
    let (rel_lean, resolved) = match hop_lean.get(hop) {
        Some((lean, r)) => (*lean, r.as_ref()),
        None => (false, None),
    };
    expand_var_length(
        graph,
        &partial,
        dir,
        types.as_deref(),
        rel_pat,
        node_pat,
        min,
        max,
        params,
        want_trail,
        hop_sets.get(hop).and_then(|o| o.as_ref()),
        rel_lean,
        resolved.map(|(ids, _)| ids.as_slice()),
        resolved.and_then(|(_, props)| props.as_ref()),
        &mut |p2| {
            hops_stream(
                graph,
                path,
                hop_sets,
                hop_lean,
                want_trail,
                false,
                hop + 1,
                p2,
                params,
                sink,
            )
        },
    )
}

/// The streaming collector behind a breaker: NON-aggregating projections
/// buffer projected value rows (output plus sort keys — what the caller
/// asked to hold, budget-guarded); aggregating projections fold into
/// per-group accumulators through the SAME `fold_aggregate_values` the
/// materialising path uses.
struct StreamProjector<'a> {
    graph: &'a Graph,
    params: &'a BTreeMap<String, Value>,
    proj: &'a Projection,
    /// Resolved (name, expr) items; None until a first row resolves `*`.
    items: Option<Vec<(String, Expr)>>,
    columns: Vec<String>,
    aggregating: bool,
    // Non-aggregating state.
    buf: Vec<(Vec<Value>, Vec<Value>)>,
    /// Early drop bound for plain LIMIT (no order, no distinct): rows past
    /// skip+limit can never appear in the output.
    plain_cap: Option<usize>,
    /// Top-k bound for ORDER BY … LIMIT (no distinct): the buffer holds
    /// the skip+limit smallest under the sort, kept sorted by
    /// (order keys, arrival) so ties resolve EXACTLY as the stable full
    /// sort would — the output is byte-identical, only the memory shrinks.
    topk_cap: Option<usize>,
    /// Arrival counter — the stable-sort tiebreak.
    arrivals: u64,
    topk: Vec<(Vec<Value>, u64, Vec<Value>)>,
    /// Late projection: output-only properties per MATCH-bound variable, bound
    /// lean and re-materialised for the k survivors only. Empty = off.
    deferred: BTreeMap<String, std::collections::BTreeSet<String>>,
    /// The top-k when late-projecting: holds the ROW (its expensive props
    /// unbound), not the projected output — the winners are projected at
    /// finish, after their deferred properties are fetched.
    topk_late: Vec<(Vec<Value>, u64, Row)>,
    /// Late FULL materialisation (fix 27): input-row variables the previous
    /// stage bound LEAN (only this projection's ORDER BY / property reads)
    /// because this top-k RETURN is the only clause after it and outputs
    /// them bare. The k survivors are replaced by the full node at finish;
    /// every loser is never decoded. Empty = off.
    late_full: std::collections::BTreeSet<String>,
    // Aggregating state.
    agg_items: Vec<AggItem>,
    sites: Vec<AggSite>,
    /// Per ORDER BY item: the rewritten key (aggregates lifted to `$__aggN`) and
    /// its site range, when the key CONTAINS an aggregate (`ORDER BY $p +
    /// avg(x)`); `None` otherwise. The sites are appended to `sites`, folded like
    /// any other, and substituted at finish.
    order_agg: Vec<Option<(Expr, std::ops::Range<usize>)>>,
    group_index: BTreeMap<Vec<u8>, usize>,
    /// The per-projector NaN nonce: no two NaNs ever share a key.
    nan_nonce: u64,
    groups: Vec<AggGroup>,
}

pub(crate) enum AggItem {
    /// A grouping item: evaluated per row for the key, and re-evaluated on
    /// the group's template at emit — exactly the materialising behaviour.
    Key,
    /// An aggregate-bearing item: the rewritten expression plus its site
    /// index range in `sites`.
    Agg {
        rewritten: Expr,
        site_range: std::ops::Range<usize>,
    },
}

struct AggGroup {
    template: Row,
    accs: Vec<SiteAcc>,
}

pub(crate) enum SiteAcc {
    CountStar(i64),
    /// A streaming fold: running state, with the DISTINCT filter (the
    /// canonical-key set) applied at push. Accumulate-then-fold cloned
    /// every aggregated value into a Vec first — `count(m)` over the
    /// full-scale corpus stored 1.79M full node values to count them
    /// (123 s measured). The push order is the fold's iteration order, so
    /// sums, averages, min/max ties and collect order are byte-identical.
    Stream {
        distinct: Option<(std::collections::BTreeSet<Vec<u8>>, u64)>,
        state: AggState,
    },
    /// Any aggregate the streaming states do not model — behaviour today,
    /// unchanged: buffer, then fold through `fold_aggregate_values`.
    Values(Vec<Value>),
}

pub(crate) enum AggState {
    Count(i64),
    Collect(Vec<Value>),
    Sum {
        int: i64,
        float: f64,
        any_float: bool,
    },
    Avg {
        total: f64,
        n: u64,
    },
    Min(Option<Value>),
    Max(Option<Value>),
}

impl SiteAcc {
    /// Fold one site value in — `None` for a star site — exactly as the
    /// projector's group loop does (null inputs skipped, DISTINCT by the
    /// canonical key).
    pub(crate) fn push(&mut self, v: Option<Value>) -> Result<(), RunError> {
        match (self, v) {
            (SiteAcc::CountStar(n), _) => *n += 1,
            (SiteAcc::Stream { distinct, state }, Some(v)) => {
                if !matches!(v, Value::Null) {
                    let keep = match distinct {
                        Some((seen, nonce)) => {
                            seen.insert(agg_key_of(std::slice::from_ref(&v), nonce))
                        }
                        None => true,
                    };
                    if keep {
                        state.push(v)?;
                    }
                }
            }
            (SiteAcc::Values(vals), Some(v)) => {
                if !matches!(v, Value::Null) {
                    vals.push(v);
                }
            }
            (SiteAcc::Stream { .. } | SiteAcc::Values(_), None) => {
                unreachable!("non-star site has a value")
            }
        }
        Ok(())
    }

    /// The site's final value.
    pub(crate) fn finish(self, site: &AggSite) -> Result<Value, RunError> {
        Ok(match self {
            SiteAcc::CountStar(n) => Value::Int(n),
            SiteAcc::Stream { state, .. } => state.finish(),
            SiteAcc::Values(vals) => fold_aggregate_values(&site.name, site.distinct, vals)?,
        })
    }

    /// A non-distinct `count` accumulator pre-loaded with `n` — byte-identical to
    /// folding `n` present rows through a fresh `count(<var>)` site (`for_site`
    /// builds exactly `Stream { distinct: None, state: Count(0) }`, and
    /// `AggState::Count(n).finish()` is `Value::Int(n)`). The degree short-circuit
    /// builds these from a computed count instead of folding one row per edge.
    pub(crate) fn count_preset(n: i64) -> SiteAcc {
        SiteAcc::Stream {
            distinct: None,
            state: AggState::Count(n),
        }
    }

    /// The accumulator for a site. Star is CountStar; the six modelled
    /// functions stream; anything else keeps the buffering fallback.
    pub(crate) fn for_site(site: &AggSite) -> SiteAcc {
        if site.star {
            return SiteAcc::CountStar(0);
        }
        let state = match site.name.as_str() {
            "count" => AggState::Count(0),
            "collect" => AggState::Collect(Vec::new()),
            "sum" => AggState::Sum {
                int: 0,
                float: 0.0,
                any_float: false,
            },
            "avg" => AggState::Avg { total: 0.0, n: 0 },
            "min" => AggState::Min(None),
            "max" => AggState::Max(None),
            _ => return SiteAcc::Values(Vec::new()),
        };
        SiteAcc::Stream {
            distinct: if site.distinct {
                Some((std::collections::BTreeSet::new(), 0))
            } else {
                None
            },
            state,
        }
    }
}

impl AggState {
    /// Fold one non-null value in — the same arithmetic, error messages
    /// and tie rules as `fold_aggregate_values`, applied per row.
    fn push(&mut self, v: Value) -> Result<(), RunError> {
        match self {
            AggState::Count(n) => *n += 1,
            AggState::Collect(vals) => vals.push(v),
            AggState::Sum {
                int,
                float,
                any_float,
            } => match v {
                Value::Int(i) => {
                    *int = int
                        .checked_add(i)
                        .ok_or(RunError::Eval(EvalError::Overflow("sum")))?;
                }
                Value::Float(f) => {
                    *any_float = true;
                    *float += f;
                }
                other => {
                    return Err(RunError::Semantic(format!(
                        "sum() over a {}",
                        other.type_name()
                    )));
                }
            },
            AggState::Avg { total, n } => {
                match v {
                    Value::Int(i) => *total += i as f64,
                    Value::Float(f) => *total += f,
                    other => {
                        return Err(RunError::Semantic(format!(
                            "avg() over a {}",
                            other.type_name()
                        )));
                    }
                }
                *n += 1;
            }
            AggState::Min(best) => {
                let take = match best {
                    None => true,
                    Some(b) => minmax_cmp(&v, b) == std::cmp::Ordering::Less,
                };
                if take {
                    *best = Some(v);
                }
            }
            AggState::Max(best) => {
                let take = match best {
                    None => true,
                    Some(b) => minmax_cmp(&v, b) == std::cmp::Ordering::Greater,
                };
                if take {
                    *best = Some(v);
                }
            }
        }
        Ok(())
    }

    /// The final value — empty-input behaviour identical to the fold:
    /// count 0, collect [], sum Int(0), avg/min/max Null.
    fn finish(self) -> Value {
        match self {
            AggState::Count(n) => Value::Int(n),
            AggState::Collect(vals) => Value::List(vals),
            AggState::Sum {
                int,
                float,
                any_float,
            } => {
                if any_float {
                    Value::Float(float + int as f64)
                } else {
                    Value::Int(int)
                }
            }
            AggState::Avg { total, n } => {
                if n == 0 {
                    Value::Null
                } else {
                    Value::Float(total / n as f64)
                }
            }
            AggState::Min(best) | AggState::Max(best) => best.unwrap_or(Value::Null),
        }
    }
}

impl<'a> StreamProjector<'a> {
    fn new(
        graph: &'a Graph,
        proj: &'a Projection,
        params: &'a BTreeMap<String, Value>,
        deferred: BTreeMap<String, std::collections::BTreeSet<String>>,
    ) -> Result<Self, RunError> {
        let aggregating = proj.items.iter().any(|it| contains_aggregate(&it.expr));
        let plain_cap =
            if !aggregating && proj.order.is_empty() && !proj.distinct && proj.limit.is_some() {
                let skip = eval_count(graph, proj.skip.as_ref(), params, "SKIP")?.unwrap_or(0);
                let limit = eval_count(graph, proj.limit.as_ref(), params, "LIMIT")?.unwrap_or(0);
                Some(skip + limit)
            } else {
                None
            };
        let topk_cap =
            if !aggregating && !proj.order.is_empty() && !proj.distinct && proj.limit.is_some() {
                let skip = eval_count(graph, proj.skip.as_ref(), params, "SKIP")?.unwrap_or(0);
                let limit = eval_count(graph, proj.limit.as_ref(), params, "LIMIT")?.unwrap_or(0);
                Some(skip + limit)
            } else {
                None
            };
        let mut me = StreamProjector {
            graph,
            params,
            proj,
            items: None,
            columns: Vec::new(),
            aggregating,
            buf: Vec::new(),
            plain_cap,
            topk_cap,
            arrivals: 0,
            topk: Vec::new(),
            deferred,
            topk_late: Vec::new(),
            late_full: Default::default(),
            agg_items: Vec::new(),
            sites: Vec::new(),
            order_agg: Vec::new(),
            group_index: BTreeMap::new(),
            nan_nonce: 0,
            groups: Vec::new(),
        };
        if !proj.star {
            me.resolve_items(&Row::new())?;
        }
        Ok(me)
    }

    /// Whether the top-k keeps lean ROWS and projects at finish: a deferred
    /// property set, or a carried node the previous stage bound lean for
    /// this projection to hydrate in full for its survivors.
    fn defers(&self) -> bool {
        !self.deferred.is_empty() || !self.late_full.is_empty()
    }

    /// Fix 56: the ORDER BY key of a lean row evaluated from the ROW'S
    /// bindings alone, without projecting the items first — `None` when an
    /// ORDER BY expression reads an output column the row does not already
    /// hold under the same name with the same properties (an alias of an
    /// expression, say), which `project_row_values` resolves by projecting.
    /// The late-projecting top-k projected EVERY item per row to reach the
    /// key: `properties(w)` and two pattern comprehensions per work item,
    /// for a key that reads `w.updatedAt`. A column that is the variable
    /// itself, or `properties(var)` under the variable's own name (the
    /// listings' `RETURN properties(w) AS w … ORDER BY w.updatedAt`), reads
    /// the same properties either way.
    fn order_key_direct(&self, row: &Row) -> Result<Option<Vec<Value>>, RunError> {
        let items = self.items.as_ref().expect("resolved");
        let mut key = Vec::with_capacity(self.proj.order.len());
        for o in &self.proj.order {
            let mut fvs = Vec::new();
            free_vars_of(&o.expr, &mut fvs);
            for fv in &fvs {
                if let Some(ix) = self.columns.iter().position(|c| c == fv) {
                    let same = match &items[ix].1 {
                        Expr::Var(v) => v == fv,
                        Expr::Call { name, args, .. } if name.eq_ignore_ascii_case("properties") => {
                            matches!(args.as_slice(), [Expr::Var(v)] if v == fv)
                        }
                        _ => false,
                    };
                    if !same || !row.contains_key(fv) {
                        return Ok(None);
                    }
                } else if !row.contains_key(fv) {
                    return Ok(None);
                }
            }
            key.push(eval_expr(self.graph, &o.expr, row, self.params)?);
        }
        counted!("interp.top-k key read from the lean row");
        Ok(Some(key))
    }

    /// Replace every late-full carry in the row with its FULL node. The
    /// previous stage bound only what this projection's keys read; the
    /// output item is the whole node, and only a survivor reaches here.
    fn hydrate_late_full(&self, row: &mut Row) -> Result<(), RunError> {
        for var in &self.late_full {
            let Some(Value::Node { id, .. }) = row.get(var) else {
                continue;
            };
            let nid = *id;
            if let Some(full) = self.graph.node(nid)? {
                counted!("interp.late projection re-materialised a carried node for a survivor");
                row.insert(var.clone(), full);
            }
        }
        Ok(())
    }

    /// Expand `*` (from the first row's bindings, name order) and the items,
    /// mirroring `project()` exactly; prepare the aggregate site plan.
    fn resolve_items(&mut self, first_row: &Row) -> Result<(), RunError> {
        let mut items: Vec<(String, Expr)> = Vec::new();
        if self.proj.star {
            let mut names: Vec<String> = first_row.keys().cloned().collect();
            names.sort();
            for n in names {
                items.push((n.clone(), Expr::Var(n)));
            }
        }
        for (i, item) in self.proj.items.iter().enumerate() {
            let name = item
                .alias
                .clone()
                .or_else(|| item.text.clone())
                .unwrap_or_else(|| column_name(&item.expr, i));
            items.push((name, item.expr.clone()));
        }
        // Same rule as `project()`: a `*` that expands to nothing is legal and
        // projects zero columns; only a non-star empty projection is an error.
        // This path used to refuse the star too, which made every `WITH *` /
        // `RETURN *` over ZERO rows fail — and `WITH … WHERE … ORDER BY … LIMIT …`
        // parses to a `WITH *`, so the platform's story-tracker read (which finds
        // no story most of the time) would have failed on the very fix meant to
        // admit it (2026-09-04, caught by running the musl binary over Bolt).
        if items.is_empty() && !self.proj.star {
            return Err(RunError::Semantic(
                "a projection needs at least one item".into(),
            ));
        }
        self.columns = items.iter().map(|(n, _)| n.clone()).collect();
        if self.aggregating {
            for (_, e) in &items {
                if contains_aggregate(e) {
                    let start = self.sites.len();
                    let rewritten = extract_aggregates(e, &mut self.sites);
                    self.agg_items.push(AggItem::Agg {
                        rewritten,
                        site_range: start..self.sites.len(),
                    });
                } else {
                    self.agg_items.push(AggItem::Key);
                }
            }
            // An ORDER BY key may itself aggregate (`ORDER BY $p + avg(x)`) —
            // lift its aggregates to sites so they are computed over the group.
            for o in &self.proj.order {
                if contains_aggregate(&o.expr) {
                    let start = self.sites.len();
                    let rewritten = extract_aggregates(&o.expr, &mut self.sites);
                    self.order_agg
                        .push(Some((rewritten, start..self.sites.len())));
                } else {
                    self.order_agg.push(None);
                }
            }
        }
        self.items = Some(items);
        Ok(())
    }

    fn push(&mut self, row: Row) -> Result<(), RunError> {
        if self.items.is_none() {
            self.resolve_items(&row)?;
        }
        if !self.aggregating {
            if let Some(cap) = self.plain_cap {
                if self.buf.len() >= cap {
                    // Every emittable row is buffered: stop the PRODUCER,
                    // not just the buffering — `MATCH (n:Bio) RETURN n
                    // LIMIT 100` measured 10.1 s because the scan kept
                    // materialising candidates this line then discarded.
                    sometimes!("interp.plain limit stopped the producer", true);
                    return Err(RunError::Saturated);
                }
            }
            if self.defers() {
                if let Some(cap) = self.topk_cap {
                    // Late projection: the expensive props are unbound, so the
                    // key is computed on the lean row (cheap; deferred props
                    // read as null but the key never uses them) and the ROW is
                    // kept for the winners to project at finish.
                    let key = match self.order_key_direct(&row)? {
                        Some(k) => k,
                        None => {
                            let items = self.items.as_ref().expect("resolved");
                            let (_, k) = project_row_values(
                                self.graph,
                                items,
                                &self.columns,
                                &self.proj.order,
                                row.clone(),
                                self.params,
                            )?;
                            k
                        }
                    };
                    let seq = self.arrivals;
                    self.arrivals += 1;
                    if cap == 0 {
                        return Ok(());
                    }
                    if topk_push(&mut self.topk_late, cap, (key, seq, row), &self.proj.order) {
                        sometimes!("interp.top-k bounded the sort", true);
                    }
                    return Ok(());
                }
            }
            // A late-full carry reaching a projector with no top-k (a shape
            // the stage boundary and this constructor disagree on) is
            // hydrated per row: correct, and counted so it is seen.
            let mut row = row;
            if !self.late_full.is_empty() {
                counted!("interp.late full carry hydrated eagerly");
                self.hydrate_late_full(&mut row)?;
            }
            let items = self.items.as_ref().expect("resolved");
            let (out, key) = project_row_values(
                self.graph,
                items,
                &self.columns,
                &self.proj.order,
                row,
                self.params,
            )?;
            if let Some(cap) = self.topk_cap {
                // Bounded by (sort key, arrival): the heap holds the stable
                // sort's first `cap` rows at all times (fix 43 — `topk_push`).
                let seq = self.arrivals;
                self.arrivals += 1;
                if cap == 0 {
                    return Ok(());
                }
                if topk_push(&mut self.topk, cap, (key, seq, out), &self.proj.order) {
                    sometimes!("interp.top-k bounded the sort", true);
                }
                return Ok(());
            }
            self.buf.push((out, key));
            budget_check(self.graph, self.buf.len())?;
            return Ok(());
        }
        // Aggregating: group key from the non-aggregate items, per-site
        // argument values folded incrementally.
        let items = self.items.as_ref().expect("resolved");
        let mut key = Vec::new();
        for ((_, e), kind) in items.iter().zip(&self.agg_items) {
            if matches!(kind, AggItem::Key) {
                key.push(eval_expr(self.graph, e, &row, self.params)?);
            }
        }
        // Per-site argument values for THIS row (before the row moves).
        let mut site_vals: Vec<Option<Value>> = Vec::with_capacity(self.sites.len());
        for site in &self.sites {
            if site.star {
                site_vals.push(None);
            } else {
                let arg = site.args.first().ok_or_else(|| {
                    RunError::Semantic(format!("{}() needs an argument", site.name))
                })?;
                let v = eval_expr(self.graph, arg, &row, self.params)?;
                site_vals.push(Some(v));
            }
        }
        // Find or create the group by the CANONICAL key — Neo4j's measured
        // equivalence (numerics unify, NaN never does), one encoding shared
        // with every DISTINCT in the engine. The first-seen value tuple
        // stays as the group's representative, exactly as Neo4j returns it.
        let ser = agg_key_of(&key, &mut self.nan_nonce);
        let gi = if let Some(&i) = self.group_index.get(&ser) {
            i
        } else {
            let accs = self.sites.iter().map(SiteAcc::for_site).collect();
            self.groups.push(AggGroup {
                template: row,
                accs,
            });
            self.group_index.insert(ser, self.groups.len() - 1);
            budget_check(self.graph, self.groups.len())?;
            self.groups.len() - 1
        };
        let g = &mut self.groups[gi];
        for (acc, v) in g.accs.iter_mut().zip(site_vals) {
            match (acc, v) {
                (SiteAcc::CountStar(n), _) => *n += 1,
                (SiteAcc::Stream { distinct, state }, Some(v)) => {
                    if !matches!(v, Value::Null) {
                        let keep = match distinct {
                            Some((seen, nonce)) => {
                                seen.insert(agg_key_of(std::slice::from_ref(&v), nonce))
                            }
                            None => true,
                        };
                        if keep {
                            state.push(v)?;
                        }
                    }
                }
                (SiteAcc::Values(vals), Some(v)) => {
                    if !matches!(v, Value::Null) {
                        vals.push(v);
                    }
                }
                (SiteAcc::Stream { .. } | SiteAcc::Values(_), None) => {
                    unreachable!("non-star site has a value")
                }
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Projected, RunError> {
        if self.items.is_none() {
            // Zero rows and `*` unresolved: the star contributes nothing,
            // exactly as the materialising path reads an empty row set.
            self.resolve_items(&Row::new())?;
        }
        let items = self.items.take().expect("resolved");
        if !self.aggregating {
            if self.topk_cap.is_some() {
                if self.defers() {
                    // Late projection: materialise the deferred properties for
                    // the k survivors only, then project and page them.
                    let mut late = std::mem::take(&mut self.topk_late);
                    topk_sort(&mut late, &self.proj.order);
                    if !late.is_empty() && !self.deferred.is_empty() {
                        counted!("interp.late projection deferred a property");
                    }
                    let mut out_rows = Vec::with_capacity(late.len());
                    let mut order_keys = Vec::with_capacity(late.len());
                    for (k, _, mut row) in late {
                        for (var, props) in &self.deferred {
                            if let Some(Value::Node {
                                id, props: bound, ..
                            }) = row.get_mut(var)
                            {
                                let nid = *id;
                                if let Some(Value::Node { props: fp, .. }) =
                                    self.graph.node_projected(nid, props)?
                                {
                                    for (pk, pv) in fp {
                                        bound.insert(pk, pv);
                                    }
                                }
                            }
                        }
                        self.hydrate_late_full(&mut row)?;
                        let (out, _k) = project_row_values(
                            self.graph,
                            &items,
                            &self.columns,
                            &self.proj.order,
                            row,
                            self.params,
                        )?;
                        out_rows.push(out);
                        order_keys.push(k);
                    }
                    return project_tail(
                        self.graph,
                        self.proj,
                        self.params,
                        self.columns,
                        out_rows,
                        order_keys,
                    );
                }
                // Sorted (key, arrival) here — the tail re-sorts (stable,
                // same order) and applies SKIP/LIMIT exactly.
                let mut kept = std::mem::take(&mut self.topk);
                topk_sort(&mut kept, &self.proj.order);
                let mut out_rows = Vec::with_capacity(kept.len());
                let mut order_keys = Vec::with_capacity(kept.len());
                for (k, _, o) in kept {
                    out_rows.push(o);
                    order_keys.push(k);
                }
                return project_tail(
                    self.graph,
                    self.proj,
                    self.params,
                    self.columns,
                    out_rows,
                    order_keys,
                );
            }
            let (out_rows, order_keys): (Vec<_>, Vec<_>) = self.buf.into_iter().unzip();
            return project_tail(
                self.graph,
                self.proj,
                self.params,
                self.columns,
                out_rows,
                order_keys,
            );
        }
        // An aggregation over zero rows with NO grouping keys still yields
        // one row (count(*) over nothing is 0, not absence).
        if self.groups.is_empty()
            && self
                .agg_items
                .iter()
                .all(|k| matches!(k, AggItem::Agg { .. }))
        {
            let accs = self.sites.iter().map(SiteAcc::for_site).collect();
            self.groups.push(AggGroup {
                template: Row::new(),
                accs,
            });
        }
        let mut out_rows = Vec::with_capacity(self.groups.len());
        let mut order_keys = Vec::with_capacity(self.groups.len());
        for g in self.groups {
            // Fold every site's accumulator through the ONE shared
            // implementation of the aggregate semantics.
            let mut computed: Vec<Value> = Vec::with_capacity(self.sites.len());
            for (site, acc) in self.sites.iter().zip(g.accs) {
                computed.push(match acc {
                    SiteAcc::CountStar(n) => Value::Int(n),
                    SiteAcc::Stream { state, .. } => state.finish(),
                    SiteAcc::Values(vals) => fold_site(self.graph, site, vals, self.params)?,
                });
            }
            let mut out = Vec::with_capacity(items.len());
            for ((_, e), kind) in items.iter().zip(&self.agg_items) {
                match kind {
                    AggItem::Key => {
                        out.push(eval_expr(self.graph, e, &g.template, self.params)?);
                    }
                    AggItem::Agg {
                        rewritten,
                        site_range,
                    } => {
                        let mut p = self.params.clone();
                        for (local, global) in site_range.clone().enumerate() {
                            p.insert(format!("__agg{global}"), computed[global].clone());
                            let _ = local;
                        }
                        out.push(eval_expr(self.graph, rewritten, &g.template, &p)?);
                    }
                }
            }
            // ORDER BY: projected scope first, grouping expression by
            // structural match second — the materialising path's exact rule.
            let mut scope_row = Row::new();
            for (c, v) in self.columns.iter().zip(&out) {
                scope_row.insert(c.clone(), v.clone());
            }
            let mut okey = Vec::new();
            for (oi, o) in self.proj.order.iter().enumerate() {
                if let Some(Some((rewritten, range))) = self.order_agg.get(oi) {
                    // An aggregating ORDER BY key: substitute the group's site
                    // values for its `$__aggN` holes, then evaluate.
                    let mut p = self.params.clone();
                    for global in range.clone() {
                        p.insert(format!("__agg{global}"), computed[global].clone());
                    }
                    okey.push(eval_expr(self.graph, rewritten, &scope_row, &p)?);
                } else if let Some(j) = items.iter().position(|(_, e)| *e == o.expr) {
                    okey.push(out[j].clone());
                } else {
                    let rw = rewrite_order_over_projection(&o.expr, &items);
                    okey.push(eval_expr(self.graph, &rw, &scope_row, self.params)?);
                }
            }
            order_keys.push(okey);
            out_rows.push(out);
        }
        project_tail(
            self.graph,
            self.proj,
            self.params,
            self.columns,
            out_rows,
            order_keys,
        )
    }
}

// ─── CREATE / MERGE / SET ───────────────────────────────────────────────────

fn eval_props(
    graph: &Graph,
    props: &Option<Expr>,
    row: &Row,
    params: &BTreeMap<String, Value>,
) -> Result<BTreeMap<String, Value>, RunError> {
    match props {
        None => Ok(BTreeMap::new()),
        Some(e) => match eval_expr(graph, e, row, params)? {
            Value::Map(m) => Ok(m),
            other => Err(RunError::Semantic(format!(
                "a property map must be a map, got {}",
                other.type_name()
            ))),
        },
    }
}

fn exec_create_path(
    graph: &Graph,
    path: &PathPattern,
    row: &mut Row,
    params: &BTreeMap<String, Value>,
    // When creating a MERGE pattern, an UNDIRECTED relationship defaults to
    // OUTGOING (openCypher); a bare CREATE still requires an explicit direction.
    merge_create: bool,
) -> Result<(), RunError> {
    if path.shortest {
        return Err(RunError::Semantic("CREATE cannot take shortestPath".into()));
    }
    // A STANDALONE node pattern (no relationships) that names an already-bound
    // node is VariableAlreadyBound — there is nothing to create.
    if path.hops.is_empty() {
        if let Some(v) = &path.start.var {
            if matches!(row.get(v), Some(Value::Node { .. })) {
                return Err(RunError::Semantic(format!(
                    "Variable `{v}` already bound (VariableAlreadyBound): \
                     a standalone CREATE of a bound node creates nothing"
                )));
            }
        }
    }
    let mut trail: Vec<Value> = Vec::new();
    let mut at = ensure_node(graph, &path.start, row, params)?;
    trail.push(graph.node(at)?.ok_or(GraphError::Missing("node", at))?);
    for (rel_pat, node_pat) in &path.hops {
        if rel_pat.length.is_some() {
            return Err(RunError::Semantic(
                "CREATE cannot take a variable-length pattern".into(),
            ));
        }
        if rel_pat.types.len() != 1 {
            return Err(RunError::Semantic(
                "CREATE needs exactly one relationship type".into(),
            ));
        }
        // A relationship variable that is already bound cannot be re-created.
        if let Some(v) = &rel_pat.var {
            if row.get(v).is_some() {
                return Err(RunError::Semantic(format!(
                    "Variable `{v}` already bound (VariableAlreadyBound)"
                )));
            }
        }
        let dst = ensure_node(graph, node_pat, row, params)?;
        let props = eval_props(graph, &rel_pat.props, row, params)?;
        let (src_id, dst_id) = match rel_pat.dir {
            RelDir::Out => (at, dst),
            RelDir::In => (dst, at),
            RelDir::Undirected if merge_create => (at, dst),
            RelDir::Undirected => {
                return Err(RunError::Semantic(
                    "CREATE needs a directed relationship".into(),
                ));
            }
        };
        let rel_id = graph.create_rel(src_id, &rel_pat.types[0], dst_id, &props)?;
        let rel = graph.rel(rel_id)?.expect("just created");
        if let Some(v) = &rel_pat.var {
            row.insert(v.clone(), rel.to_value());
        }
        trail.push(rel.to_value());
        trail.push(graph.node(dst)?.ok_or(GraphError::Missing("node", dst))?);
        at = dst;
    }
    if let Some(v) = &path.var {
        row.insert(v.clone(), Value::Path(trail));
    }
    Ok(())
}

fn ensure_node(
    graph: &Graph,
    pat: &NodePattern,
    row: &mut Row,
    params: &BTreeMap<String, Value>,
) -> Result<u64, RunError> {
    if let Some(v) = &pat.var {
        if let Some(bound) = row.get(v) {
            let Value::Node { id, .. } = bound else {
                return Err(RunError::Semantic(format!(
                    "`{v}` is bound to a {}, not a node",
                    bound.type_name()
                )));
            };
            // openCypher VariableAlreadyBound: a bound node may be REFERENCED as
            // a relationship endpoint, but CREATE cannot re-declare it with
            // labels or properties (that is SET's job).
            if !pat.labels.is_empty() || pat.props.is_some() {
                return Err(RunError::Semantic(format!(
                    "Variable `{v}` already bound (VariableAlreadyBound): \
                     CREATE cannot add labels or properties to a bound node"
                )));
            }
            return Ok(*id);
        }
    }
    let props = eval_props(graph, &pat.props, row, params)?;
    let id = graph.create_node(&pat.labels, &props)?;
    if let Some(v) = &pat.var {
        let node = graph.node(id)?.expect("just created");
        row.insert(v.clone(), node);
    }
    Ok(id)
}

fn set_entity_prop(
    graph: &Graph,
    target: &Value,
    key: &str,
    value: &Value,
) -> Result<(), RunError> {
    match target {
        Value::Null => Ok(()),
        Value::Node { id, .. } => Ok(graph.set_prop(true, *id, key, value)?),
        Value::Rel { id, .. } => Ok(graph.set_prop(false, *id, key, value)?),
        other => Err(RunError::Semantic(format!(
            "SET needs a node or relationship, got {}",
            other.type_name()
        ))),
    }
}

fn apply_set_items(
    graph: &Graph,
    items: &[SetItem],
    row: &mut Row,
    params: &BTreeMap<String, Value>,
) -> Result<(), RunError> {
    for item in items {
        match item {
            SetItem::Prop { base, key, value } => {
                let target = eval_expr(graph, base, row, params)?;
                let v = eval_expr(graph, value, row, params)?;
                set_entity_prop(graph, &target, key, &v)?;
            }
            SetItem::Replace { var, value } => {
                let target = row
                    .get(var)
                    .cloned()
                    .ok_or_else(|| RunError::Semantic(format!("`{var}` is not bound")))?;
                let v = eval_expr(graph, value, row, params)?;
                let (existing, new_map) = match (&target, &v) {
                    // openCypher IGNORES SET on a null entity — empty maps → no-op.
                    (Value::Null, _) => (BTreeMap::new(), BTreeMap::new()),
                    (Value::Node { props, .. } | Value::Rel { props, .. }, Value::Map(m)) => {
                        (props.clone(), m.clone())
                    }
                    (
                        Value::Node { props, .. } | Value::Rel { props, .. },
                        Value::Node { props: m, .. },
                    ) => (props.clone(), m.clone()),
                    _ => {
                        return Err(RunError::Semantic(
                            "SET n = … needs a bound entity and a map".into(),
                        ));
                    }
                };
                for k in existing.keys() {
                    if !new_map.contains_key(k) {
                        set_entity_prop(graph, &target, k, &Value::Null)?;
                    }
                }
                for (k, val) in &new_map {
                    set_entity_prop(graph, &target, k, val)?;
                }
            }
            SetItem::Merge { var, value } => {
                let target = row
                    .get(var)
                    .cloned()
                    .ok_or_else(|| RunError::Semantic(format!("`{var}` is not bound")))?;
                let v = eval_expr(graph, value, row, params)?;
                let Value::Map(m) = v else {
                    return Err(RunError::Semantic("SET n += … needs a map".into()));
                };
                for (k, val) in &m {
                    set_entity_prop(graph, &target, k, val)?;
                }
            }
            SetItem::Labels { var, labels } => match row.get(var) {
                // openCypher IGNORES SET on a null entity (from an OPTIONAL MATCH).
                Some(Value::Null) => {}
                Some(Value::Node { id, .. }) => graph.add_labels(*id, labels)?,
                _ => {
                    return Err(RunError::Semantic(format!(
                        "SET labels needs a bound node, `{var}` is not one"
                    )));
                }
            },
        }
        refresh_row(graph, row)?;
    }
    Ok(())
}

/// Re-materialise every bound entity — bindings are snapshots, and a
/// mutation in this statement must be visible to the next read of the SAME
/// binding (Cypher's visibility rule within a statement).
fn refresh_row(graph: &Graph, row: &mut Row) -> Result<(), RunError> {
    for v in row.values_mut() {
        match v {
            Value::Node { id, .. } => {
                if let Some(fresh) = graph.node(*id)? {
                    *v = fresh;
                }
            }
            Value::Rel { id, .. } => {
                if let Some(fresh) = graph.rel(*id)? {
                    *v = fresh.to_value();
                }
            }
            _ => {}
        }
    }
    Ok(())
}

// ─── Projection: aliases, aggregation, order, pagination ────────────────────

struct Projected {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
}

impl Projected {
    fn into_result(self) -> QueryResult {
        QueryResult {
            columns: self.columns,
            rows: self.rows,
        }
    }

    fn into_rows(self) -> Result<Vec<Row>, RunError> {
        let mut out = Vec::with_capacity(self.rows.len());
        for r in self.rows {
            let mut m = Row::new();
            for (c, v) in self.columns.iter().zip(r) {
                m.insert(c.clone(), v);
            }
            out.push(m);
        }
        Ok(out)
    }
}

const AGG_FNS: &[&str] = &[
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "collect",
    "stdev",
    "stdevp",
    "percentilecont",
    "percentiledisc",
];

/// A projection/order expression simple enough to stand as a grouping-key
/// operand inside an aggregating expression: a bare variable, a property chain
/// on one, or a constant. Anything compound is ambiguous when mixed with an
/// aggregate.
fn is_simple_group_key(e: &Expr) -> bool {
    match e {
        Expr::Var(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Param(_) => true,
        Expr::Prop(base, _) => is_simple_group_key(base),
        // A constructor/wrapper OVER grouping keys is fine (`[a]`, `{k: a}`,
        // `toLower(a)`); only a bare ARITHMETIC/comparison combination of grouping
        // keys mixed with an aggregate (`me.age + you.age + count(*)`) is ambiguous.
        Expr::List(args) => args.iter().all(is_simple_group_key),
        Expr::Map(es) => es.iter().all(|(_, v)| is_simple_group_key(v)),
        Expr::Call { args, .. } => args.iter().all(is_simple_group_key),
        _ => false,
    }
}

/// AmbiguousAggregationExpression: within an expression that aggregates, every
/// NON-aggregate operand must be a simple grouping key. `me.age + count(*)` is
/// fine; `me.age + you.age + count(*)` combines a COMPOUND key with the
/// aggregate, which is ambiguous — even when that compound is returned separately.
fn check_agg_ambiguity(e: &Expr) -> Result<(), RunError> {
    let ambiguous = || {
        RunError::Semantic(
            "a compound expression may not be combined with an aggregate (AmbiguousAggregationExpression)"
                .into(),
        )
    };
    if !contains_aggregate(e) {
        return if is_simple_group_key(e) {
            Ok(())
        } else {
            Err(ambiguous())
        };
    }
    match e {
        // The aggregate itself — its arguments are unrestricted.
        Expr::Call { name, star, .. } if *star || AGG_FNS.contains(&name.as_str()) => Ok(()),
        Expr::Bin(_, a, b)
        | Expr::And(a, b)
        | Expr::Or(a, b)
        | Expr::Xor(a, b)
        | Expr::In(a, b)
        | Expr::Index(a, b) => {
            check_agg_ambiguity(a)?;
            check_agg_ambiguity(b)
        }
        Expr::Not(a) | Expr::Neg(a) | Expr::Prop(a, _) => check_agg_ambiguity(a),
        Expr::IsNull { of, .. } => check_agg_ambiguity(of),
        Expr::Call { args, .. } | Expr::List(args) => {
            for a in args {
                check_agg_ambiguity(a)?;
            }
            Ok(())
        }
        // Case / comprehension / other wrappers: conservatively accept (the strict
        // TCK cases are all arithmetic Bin-shaped).
        _ => Ok(()),
    }
}

/// Whether `e` calls a NON-DETERMINISTic function (`rand`, `randomUUID`).
fn calls_nondeterministic(e: &Expr) -> bool {
    match e {
        Expr::Call { name, args, .. } => {
            matches!(name.as_str(), "rand" | "randomuuid")
                || args.iter().any(calls_nondeterministic)
        }
        Expr::List(args) => args.iter().any(calls_nondeterministic),
        Expr::Map(es) => es.iter().any(|(_, v)| calls_nondeterministic(v)),
        Expr::Bin(_, a, b)
        | Expr::And(a, b)
        | Expr::Or(a, b)
        | Expr::Xor(a, b)
        | Expr::In(a, b)
        | Expr::Index(a, b) => calls_nondeterministic(a) || calls_nondeterministic(b),
        Expr::Not(a) | Expr::Neg(a) | Expr::Prop(a, _) | Expr::HasLabels { of: a, .. } => {
            calls_nondeterministic(a)
        }
        Expr::IsNull { of, .. } => calls_nondeterministic(of),
        _ => false,
    }
}

/// NonConstantExpression: a non-deterministic function INSIDE an aggregate — the
/// grouping would be ill-defined (`count(rand())`).
fn agg_over_nondeterministic(e: &Expr) -> bool {
    match e {
        Expr::Call {
            name, star, args, ..
        } if *star || AGG_FNS.contains(&name.as_str()) => args.iter().any(calls_nondeterministic),
        Expr::Call { args, .. } | Expr::List(args) => args.iter().any(agg_over_nondeterministic),
        Expr::Map(es) => es.iter().any(|(_, v)| agg_over_nondeterministic(v)),
        Expr::Bin(_, a, b)
        | Expr::And(a, b)
        | Expr::Or(a, b)
        | Expr::Xor(a, b)
        | Expr::In(a, b)
        | Expr::Index(a, b) => agg_over_nondeterministic(a) || agg_over_nondeterministic(b),
        Expr::Not(a) | Expr::Neg(a) | Expr::Prop(a, _) | Expr::HasLabels { of: a, .. } => {
            agg_over_nondeterministic(a)
        }
        Expr::IsNull { of, .. } => agg_over_nondeterministic(of),
        _ => false,
    }
}

fn contains_aggregate(e: &Expr) -> bool {
    match e {
        Expr::Call { name, star, .. } if *star || AGG_FNS.contains(&name.as_str()) => true,
        Expr::Call { args, .. } | Expr::List(args) => args.iter().any(contains_aggregate),
        Expr::Map(entries) => entries.iter().any(|(_, v)| contains_aggregate(v)),
        Expr::Bin(_, a, b)
        | Expr::And(a, b)
        | Expr::Or(a, b)
        | Expr::Xor(a, b)
        | Expr::In(a, b)
        | Expr::Index(a, b) => contains_aggregate(a) || contains_aggregate(b),
        Expr::Not(a) | Expr::Neg(a) | Expr::Prop(a, _) => contains_aggregate(a),
        Expr::IsNull { of, .. } => contains_aggregate(of),
        Expr::Slice { of, from, to } => {
            contains_aggregate(of)
                || from.as_deref().is_some_and(contains_aggregate)
                || to.as_deref().is_some_and(contains_aggregate)
        }
        Expr::Case {
            subject,
            arms,
            otherwise,
        } => {
            subject.as_deref().is_some_and(contains_aggregate)
                || arms
                    .iter()
                    .any(|(w, t)| contains_aggregate(w) || contains_aggregate(t))
                || otherwise.as_deref().is_some_and(contains_aggregate)
        }
        // The comprehension family — `[s IN collect(x) WHERE … | s]` is the
        // corpus's collect-then-filter idiom, and missing these arms made
        // the whole item read as a GROUPING KEY, which then refused the
        // aggregate as scalar. Found by the decoded-values run, twice.
        Expr::ListComp {
            source,
            filter,
            map,
            ..
        } => {
            contains_aggregate(source)
                || filter.as_deref().is_some_and(contains_aggregate)
                || map.as_deref().is_some_and(contains_aggregate)
        }
        Expr::Reduce {
            init, source, step, ..
        } => contains_aggregate(init) || contains_aggregate(source) || contains_aggregate(step),
        Expr::ListPredicate { source, filter, .. } => {
            contains_aggregate(source) || contains_aggregate(filter)
        }
        Expr::HasLabels { of, .. } => contains_aggregate(of),
        Expr::MapProjection { of, items } => {
            contains_aggregate(of)
                || items.iter().any(|it| match it {
                    engram_cypher::ast::MapProjectionItem::Entry(_, e) => contains_aggregate(e),
                    _ => false,
                })
        }
        _ => false,
    }
}

/// A stable name for an unaliased item — the alias rule everything downstream
/// leans on. Simple shapes render as Neo4j would; anything else is aliased by
/// the caller or gets a positional name.
/// The default column name for an unaliased projection item. openCypher names
/// such a column by the VERBATIM text of its expression (`coalesce(a.x, a.y)`,
/// `count(DISTINCT a)`, `a = b`), so this renders the AST back to canonical
/// Cypher. It is a pure function of the AST, shared by the tree-walker and the
/// columnar fast paths, so both agree on the header (byte-identity intact). The
/// `column_{i}` fallback survives only for the graph/subquery-shaped variants
/// that have no faithful short rendering.
pub(crate) fn column_name(e: &Expr, i: usize) -> String {
    render_expr(e).unwrap_or_else(|| format!("column_{i}"))
}

/// Canonical Cypher rendering of `e`, or `None` for the subquery/pattern-shaped
/// variants (which fall back to `column_{i}`).
fn render_expr(e: &Expr) -> Option<String> {
    use engram_cypher::ast::{BinOp, ListPredicateKind as LK, MapProjectionItem as MI};
    let r = |x: &Expr| render_expr(x).unwrap_or_else(|| "?".to_string());
    let joined = |xs: &[Expr]| xs.iter().map(&r).collect::<Vec<_>>().join(", ");
    let s = match e {
        Expr::Null => "null".to_string(),
        Expr::Bool(b) => b.to_string(),
        Expr::Int(n) => n.to_string(),
        Expr::Float(f) => render_float(*f),
        Expr::Str(v) => render_string(v),
        Expr::Param(p) => format!("${p}"),
        Expr::Var(v) => v.clone(),
        Expr::List(xs) => format!("[{}]", joined(xs)),
        Expr::Map(kvs) => format!(
            "{{{}}}",
            kvs.iter()
                .map(|(k, v)| format!("{k}: {}", r(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::Prop(base, key) => format!("{}.{key}", r(base)),
        Expr::Index(base, idx) => format!("{}[{}]", r(base), r(idx)),
        Expr::Slice { of, from, to } => format!(
            "{}[{}..{}]",
            r(of),
            from.as_deref().map(&r).unwrap_or_default(),
            to.as_deref().map(&r).unwrap_or_default()
        ),
        Expr::Bin(op, l, rr) => {
            let ops = match op {
                BinOp::Add => "+",
                BinOp::Sub => "-",
                BinOp::Mul => "*",
                BinOp::Div => "/",
                BinOp::Mod => "%",
                BinOp::Pow => "^",
                BinOp::Eq => "=",
                BinOp::Neq => "<>",
                BinOp::Lt => "<",
                BinOp::Le => "<=",
                BinOp::Gt => ">",
                BinOp::Ge => ">=",
                BinOp::StartsWith => "STARTS WITH",
                BinOp::EndsWith => "ENDS WITH",
                BinOp::Contains => "CONTAINS",
                BinOp::Regex => "=~",
            };
            format!("{} {ops} {}", r(l), r(rr))
        }
        Expr::And(l, rr) => format!("{} AND {}", r(l), r(rr)),
        Expr::Or(l, rr) => format!("{} OR {}", r(l), r(rr)),
        Expr::Xor(l, rr) => format!("{} XOR {}", r(l), r(rr)),
        Expr::Not(x) => format!("NOT {}", r(x)),
        Expr::Neg(x) => format!("-{}", r(x)),
        Expr::IsNull { of, negated } => {
            format!("{} IS {}NULL", r(of), if *negated { "NOT " } else { "" })
        }
        Expr::In(l, rr) => format!("{} IN {}", r(l), r(rr)),
        Expr::Call {
            name,
            distinct,
            args,
            star,
        } => {
            let inner = if *star {
                "*".to_string()
            } else {
                format!(
                    "{}{}",
                    if *distinct { "DISTINCT " } else { "" },
                    joined(args)
                )
            };
            format!("{name}({inner})")
        }
        Expr::Case {
            subject,
            arms,
            otherwise,
        } => {
            let mut out = "CASE".to_string();
            if let Some(sub) = subject {
                out.push_str(&format!(" {}", r(sub)));
            }
            for (w, t) in arms {
                out.push_str(&format!(" WHEN {} THEN {}", r(w), r(t)));
            }
            if let Some(o) = otherwise {
                out.push_str(&format!(" ELSE {}", r(o)));
            }
            out.push_str(" END");
            out
        }
        Expr::ListComp {
            var,
            source,
            filter,
            map,
        } => {
            let mut out = format!("[{var} IN {}", r(source));
            if let Some(f) = filter {
                out.push_str(&format!(" WHERE {}", r(f)));
            }
            if let Some(m) = map {
                out.push_str(&format!(" | {}", r(m)));
            }
            out.push(']');
            out
        }
        Expr::Reduce {
            acc,
            init,
            var,
            source,
            step,
        } => format!(
            "reduce({acc} = {}, {var} IN {} | {})",
            r(init),
            r(source),
            r(step)
        ),
        Expr::HasLabels { of, labels } => {
            format!(
                "{}{}",
                r(of),
                labels.iter().map(|l| format!(":{l}")).collect::<String>()
            )
        }
        Expr::ListPredicate {
            kind,
            var,
            source,
            filter,
        } => {
            let k = match kind {
                LK::Any => "any",
                LK::All => "all",
                LK::None => "none",
                LK::Single => "single",
            };
            format!("{k}({var} IN {} WHERE {})", r(source), r(filter))
        }
        Expr::MapProjection { of, items } => {
            let its = items
                .iter()
                .map(|it| match it {
                    MI::Property(k) => format!(".{k}"),
                    MI::AllProperties => ".*".to_string(),
                    MI::Entry(k, v) => format!("{k}: {}", r(v)),
                    MI::Variable(v) => v.clone(),
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}{{{its}}}", r(of))
        }
        // Subquery / pattern-shaped expressions have no faithful short name —
        // the caller falls back to `column_{i}`.
        Expr::PatternPredicate(_)
        | Expr::ExistsSub(_)
        | Expr::CountSub(_)
        | Expr::PatternComp { .. } => return None,
    };
    Some(s)
}

/// Cypher rendering of a float column name: whole values keep a `.0`, the
/// non-finite values use Cypher's spellings.
fn render_float(f: f64) -> String {
    if f.is_nan() {
        "NaN".to_string()
    } else if f.is_infinite() {
        if f < 0.0 { "-Infinity" } else { "Infinity" }.to_string()
    } else {
        format!("{f:?}") // `1.0`, `1.5` — keeps the decimal point
    }
}

/// Single-quoted string literal rendering for a column name.
fn render_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Rewrite an aggregating/DISTINCT ORDER BY key so it is expressed over the
/// PROJECTION'S OUTPUT: any maximal sub-expression equal to a projected item's
/// SOURCE expression (a grouping key, or an aggregate) becomes a reference to
/// that item's output column. `ORDER BY a.name + 'C'` with a grouping key
/// `a.name AS name` becomes `name + 'C'`, evaluable once `a` has left scope; a
/// term with NO such match (`a.age` when only `a.name` was grouped) is left as
/// is, so it then fails to resolve against the projected row — openCypher's
/// `UndefinedVariable`. Whole-node match is tried BEFORE recursing, so `a.name`
/// is replaced as a unit rather than descending into the out-of-scope `a`.
fn rewrite_order_over_projection(e: &Expr, items: &[(String, Expr)]) -> Expr {
    if let Some((col, _)) = items.iter().find(|(_, ie)| ie == e) {
        return Expr::Var(col.clone());
    }
    let sub = |x: &Expr| rewrite_order_over_projection(x, items);
    let boxed = |x: &Expr| Box::new(rewrite_order_over_projection(x, items));
    match e {
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
        // A bare Var / literal / binding-scope or graph-dependent form: unchanged
        // (an exact Var match was already handled by the items lookup above).
        _ => e.clone(),
    }
}

fn project(
    graph: &Graph,
    proj: &Projection,
    rows: Vec<Row>,
    params: &BTreeMap<String, Value>,
    // The incoming column names, used to expand `*` when there are ZERO rows (an
    // aggregating/optional MATCH can produce none while its schema is still known).
    input_schema: &[String],
) -> Result<Projected, RunError> {
    // Expand `*` into the visible variables, in name order.
    let mut items: Vec<(String, Expr)> = Vec::new();
    if proj.star {
        let mut names: Vec<String> = match rows.first() {
            Some(r) => r.keys().cloned().collect(),
            None => input_schema.to_vec(),
        };
        names.sort();
        names.dedup();
        for n in names {
            items.push((n.clone(), Expr::Var(n)));
        }
    }
    for (i, item) in proj.items.iter().enumerate() {
        let name = item
            .alias
            .clone()
            .or_else(|| item.text.clone())
            .unwrap_or_else(|| column_name(&item.expr, i));
        items.push((name, item.expr.clone()));
    }
    // A `*` that expands to nothing (a WITH/RETURN in an empty scope) is legal — it
    // projects zero columns; only a non-star empty projection is an error.
    if items.is_empty() && !proj.star {
        return Err(RunError::Semantic(
            "a projection needs at least one item".into(),
        ));
    }

    let aggregating = items.iter().any(|(_, e)| contains_aggregate(e));
    let columns: Vec<String> = items.iter().map(|(n, _)| n.clone()).collect();
    let mut out_rows: Vec<Vec<Value>> = Vec::new();
    // Keys for ORDER BY evaluated in the POST-projection scope (aliases
    // visible), falling back to the pre-projection row for un-projected vars.
    let mut order_keys: Vec<Vec<Value>> = Vec::new();

    if aggregating {
        // Implicit grouping: the non-aggregate items are the key.
        let mut groups: Vec<(Vec<Value>, Vec<Row>)> = Vec::new();
        for row in rows {
            let mut key = Vec::new();
            for (_, e) in items.iter().filter(|(_, e)| !contains_aggregate(e)) {
                key.push(eval_expr(graph, e, &row, params)?);
            }
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, g)) => g.push(row),
                None => groups.push((key, vec![row])),
            }
        }
        // An aggregation over zero rows with NO grouping keys still yields
        // one row (count(*) over nothing is 0, not absence).
        if groups.is_empty() && items.iter().all(|(_, e)| contains_aggregate(e)) {
            groups.push((Vec::new(), Vec::new()));
        }
        for (_, group) in groups {
            let mut out = Vec::with_capacity(items.len());
            let template = group.first().cloned().unwrap_or_default();
            for (_, e) in &items {
                if contains_aggregate(e) {
                    out.push(eval_aggregate(graph, e, &group, params)?);
                } else {
                    out.push(eval_expr(graph, e, &template, params)?);
                }
            }
            // ORDER BY in aggregated projections reads the projected values.
            // A key may also be spelled as the GROUPING EXPRESSION rather
            // than its alias (`RETURN s.name AS name … ORDER BY s.name`) —
            // Neo4j accepts that, and `s` is not a column, so resolve it by
            // structural match against the projection items first.
            let mut scope_row = Row::new();
            for (c, v) in columns.iter().zip(&out) {
                scope_row.insert(c.clone(), v.clone());
            }
            let mut key = Vec::new();
            for o in &proj.order {
                if let Some(j) = items.iter().position(|(_, e)| *e == o.expr) {
                    key.push(out[j].clone());
                } else {
                    let rw = rewrite_order_over_projection(&o.expr, &items);
                    key.push(eval_expr(graph, &rw, &scope_row, params)?);
                }
            }
            order_keys.push(key);
            out_rows.push(out);
        }
    } else {
        for row in rows {
            let (out, key) = project_row_values(graph, &items, &columns, &proj.order, row, params)?;
            order_keys.push(key);
            out_rows.push(out);
        }
    }

    project_tail(graph, proj, params, columns, out_rows, order_keys)
}

/// Evaluate one row's projected values and ORDER BY key — shared by the
/// materialising loop and the streaming collector.
fn project_row_values(
    graph: &Graph,
    items: &[(String, Expr)],
    columns: &[String],
    order: &[engram_cypher::stmt::OrderItem],
    row: Row,
    params: &BTreeMap<String, Value>,
) -> Result<(Vec<Value>, Vec<Value>), RunError> {
    let mut out = Vec::with_capacity(items.len());
    for (_, e) in items {
        out.push(eval_expr(graph, e, &row, params)?);
    }
    let mut scope_row = row;
    for (c, v) in columns.iter().zip(&out) {
        scope_row.insert(c.clone(), v.clone());
    }
    let mut key = Vec::new();
    for o in order {
        key.push(eval_expr(graph, &o.expr, &scope_row, params)?);
    }
    Ok((out, key))
}

/// DISTINCT, ORDER BY, SKIP, LIMIT over projected value rows — the one
/// tail both execution paths share.
fn project_tail(
    graph: &Graph,
    proj: &Projection,
    params: &BTreeMap<String, Value>,
    mut columns: Vec<String>,
    mut out_rows: Vec<Vec<Value>>,
    mut order_keys: Vec<Vec<Value>>,
) -> Result<Projected, RunError> {
    if proj.distinct {
        // Canonical-key set: O(n log n) where Vec::contains was O(n²)
        // (25k DISTINCT rows measured 6.5s), and the SAME equivalence as
        // grouping — the three dedup paths previously disagreed.
        let mut nonce = 0u64;
        let mut seen = std::collections::BTreeSet::new();
        let mut keep = Vec::with_capacity(out_rows.len());
        for (r, k) in out_rows.into_iter().zip(order_keys) {
            if seen.insert(agg_key_of(&r, &mut nonce)) {
                keep.push((r, k));
            }
        }
        let (r, k): (Vec<_>, Vec<_>) = keep.into_iter().unzip();
        out_rows = r;
        order_keys = k;
    }

    if !proj.order.is_empty() {
        let mut idx: Vec<usize> = (0..out_rows.len()).collect();
        idx.sort_by(|&a, &b| cmp_order_keys(&proj.order, &order_keys[a], &order_keys[b]));
        out_rows = idx.into_iter().map(|i| out_rows[i].clone()).collect();
    }

    let skip = eval_count(graph, proj.skip.as_ref(), params, "SKIP")?;
    let limit = eval_count(graph, proj.limit.as_ref(), params, "LIMIT")?;
    if let Some(s) = skip {
        out_rows = out_rows.into_iter().skip(s).collect();
    }
    if let Some(l) = limit {
        out_rows.truncate(l);
    }

    columns.shrink_to_fit();
    Ok(Projected {
        columns,
        rows: out_rows,
    })
}

/// Late-materialise a caller-selected set of `(a,b)` id pairs — given in the
/// caller's production/top-k order — through the SAME projection and ORDER
/// BY/SKIP/LIMIT tail the streaming collector uses. A vectorised hop operator
/// that has already chosen the winners (by a column filter + bounded top-k
/// over ids, never a per-tuple row clone) calls this to project ONLY those
/// survivors: `a_var`/`b_var` are bound to their full nodes and every
/// projection item / ORDER BY key is evaluated by the shared `eval_expr`, so
/// the output rows — values, column names, tie order under a stable re-sort —
/// are byte-identical to the per-tuple path. The pairs must already be the
/// `skip+limit` smallest under the sort; `project_tail` re-sorts (stable, same
/// order) and applies SKIP/LIMIT exactly.
pub(crate) fn project_pairs_tail(
    graph: &Graph,
    proj: &Projection,
    params: &BTreeMap<String, Value>,
    a_var: &str,
    b_var: &str,
    pairs: &[(u64, u64)],
) -> Result<QueryResult, RunError> {
    // The two-var special case of the N-var late-materialise: one row of ids
    // per pair, `[a_var, b_var]` the binding order. Delegating keeps the
    // pair-shaped recognizers on exactly the code the N-var pipeline uses.
    let vars = [a_var.to_string(), b_var.to_string()];
    let kinds = [VarKind::Node, VarKind::Node];
    let rows: Vec<Vec<u64>> = pairs.iter().map(|&(a, b)| vec![a, b]).collect();
    project_rows_tail(graph, proj, params, &vars, &kinds, &rows)
}

/// Late-materialise a caller-selected set of N-variable id ROWS — each
/// `rows[i]` holds one node id per variable in `vars` order, given in the
/// caller's production/top-k order — through the SAME projection and ORDER
/// BY/SKIP/LIMIT tail the streaming collector uses. The N-var generalisation of
/// [`project_pairs_tail`]: a vectorised multi-hop operator that has already
/// chosen its winner rows (by a column filter + bounded top-k over ids, never a
/// per-tuple row clone) binds every var to its full node and evaluates each
/// projection item / ORDER BY key through the shared `eval_expr`, so the output
/// — values, column names, tie order under a stable re-sort — is byte-identical
/// to the per-tuple path. The rows must already be the `skip+limit` smallest
/// under the sort; `project_tail` re-sorts (stable, same order) and applies
/// SKIP/LIMIT exactly.
/// How many LEADING rows a projection can possibly return — `SKIP + LIMIT` —
/// or `None` when the tail may reorder or drop rows before the limit applies.
///
/// An ORDER BY chooses WHICH rows survive, so no prefix of the unordered input
/// is the answer. DISTINCT can drop rows, so a prefix of `limit` rows may
/// dedup to fewer than `limit` and under-answer. Without either, `project_tail`
/// truncates exactly this sequence at exactly this point, so taking the prefix
/// early is byte-identical to taking it late — and it is the rule the
/// per-tuple path already applies (`interp.plain limit stopped the producer`),
/// including its consequence that an expression which would error on a row
/// past the limit is never evaluated.
fn limit_prefix(
    graph: &Graph,
    proj: &Projection,
    params: &BTreeMap<String, Value>,
) -> Result<Option<usize>, RunError> {
    if !proj.order.is_empty() || proj.distinct || proj.limit.is_none() {
        return Ok(None);
    }
    let Some(limit) = eval_count(graph, proj.limit.as_ref(), params, "LIMIT")? else {
        return Ok(None);
    };
    let skip = eval_count(graph, proj.skip.as_ref(), params, "SKIP")?.unwrap_or(0);
    Ok(Some(limit.saturating_add(skip)))
}

pub(crate) fn project_rows_tail(
    graph: &Graph,
    proj: &Projection,
    params: &BTreeMap<String, Value>,
    vars: &[String],
    kinds: &[VarKind],
    rows: &[Vec<u64>],
) -> Result<QueryResult, RunError> {
    // Resolve items exactly as `StreamProjector::resolve_items` does for a
    // non-star projection: alias, else the positional column name.
    let mut items: Vec<(String, Expr)> = Vec::with_capacity(proj.items.len());
    for (i, item) in proj.items.iter().enumerate() {
        let name = item
            .alias
            .clone()
            .or_else(|| item.text.clone())
            .unwrap_or_else(|| column_name(&item.expr, i));
        items.push((name, item.expr.clone()));
    }
    let columns: Vec<String> = items.iter().map(|(n, _)| n.clone()).collect();
    // LIMIT PUSHDOWN. `project_tail` applies SKIP/LIMIT only AFTER every row has
    // been materialised, and materialising a Node var decodes the WHOLE record.
    //
    // Measured on the pod, SF1: `is5-anchored`
    // (`MATCH (p:Person {id: K})<-[:HAS_CREATOR]-(m:Message) RETURN m.id LIMIT 25`)
    // recorded 1,212 `graph.nodes materialised in full` and 1,212 `store.gets`
    // — the person's every message, decoded to return twenty-five. The same walk
    // with the projection removed (`RETURN count(*)`) recorded ZERO of each and
    // one adjacency lookup, so the decode was the shape's entire cost.
    let rows = match limit_prefix(graph, proj, params)? {
        Some(n) if n < rows.len() => {
            counted!("interp.projection truncated to the limit before materialising");
            &rows[..n]
        }
        _ => rows,
    };
    // VAR PRUNING. The row is built for `project_row_values`, which evaluates
    // the projection items and the ORDER BY keys against it — so a var NEITHER
    // mentions costs a full record decode and is then never read. `is5` binds
    // `p` and `m` and returns only `m.id`, so half of every decode was waste on
    // top of the limit above.
    //
    // A var whose name is absent from every item and every order key cannot be
    // reached by `eval_expr`: lookup is by name, and an expression that named
    // it would appear in `free_vars_of`.
    //
    // `RETURN *` is the exception and it is NOT expanded into items — `star` is
    // a flag on the projection beside them (`RETURN *, x` sets both). A star
    // carries every variable by name, so under one NOTHING may be pruned.
    let mut referenced: Vec<String> = Vec::new();
    for (_, e) in &items {
        free_vars_of(e, &mut referenced);
    }
    for o in &proj.order {
        free_vars_of(&o.expr, &mut referenced);
    }
    let wanted: Vec<bool> = vars
        .iter()
        .map(|v| proj.star || referenced.iter().any(|r| r == v))
        .collect();
    if wanted.iter().any(|w| !w) {
        counted!("interp.projection skipped a var no item or order key reads");
    }

    let mut out_rows: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
    let mut order_keys: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
    for ids in rows {
        let mut row = Row::new();
        for (j, var) in vars.iter().enumerate() {
            if !wanted[j] {
                continue;
            }
            // `value_of`: an OPTIONAL-MATCH sentinel (`NULL_ID`) materialises to
            // `Value::Null` (so `RETURN optvar.prop` is null on an unmatched
            // outer row), exactly as the per-tuple path's null binding does; a
            // Rel-kind var materialises through `rel_of`, a Node-kind through
            // `node_of`.
            row.insert(var.clone(), value_of(graph, kinds[j], ids[j])?);
        }
        let (out, key) = project_row_values(graph, &items, &columns, &proj.order, row, params)?;
        out_rows.push(out);
        order_keys.push(key);
    }
    Ok(project_tail(graph, proj, params, columns, out_rows, order_keys)?.into_result())
}

/// Plan an aggregating projection into its aggregate SITES and a per-item plan
/// (each projection item is a grouping key, or an aggregate-bearing expression
/// with its site index range) — EXACTLY as `StreamProjector::resolve_items`
/// does, the single traversal both the per-tuple projector and the columnar
/// operator share so their site indices align.
pub(crate) fn plan_agg_projection(proj: &Projection) -> (Vec<AggSite>, Vec<AggItem>) {
    let mut sites: Vec<AggSite> = Vec::new();
    let mut items: Vec<AggItem> = Vec::with_capacity(proj.items.len());
    for item in &proj.items {
        if contains_aggregate(&item.expr) {
            let start = sites.len();
            let rewritten = extract_aggregates(&item.expr, &mut sites);
            items.push(AggItem::Agg {
                rewritten,
                site_range: start..sites.len(),
            });
        } else {
            items.push(AggItem::Key);
        }
    }
    (sites, items)
}

/// Whether every reference to `var` in `e` is a `var.<prop>` PROPERTY read
/// (each `<prop>` collected into `props`), never a WHOLE-entity use. Returns
/// `false` the moment `var` appears any other way — a bare reference, a
/// function argument (`id(var)`), a label predicate (`var:L`), a map projection
/// subject, or anywhere inside an opaque subquery/pattern shape whose variable
/// use `free_vars` cannot see — because a Map of gathered properties can
/// reproduce `var.prop` EXACTLY (`eval` reads `.prop` off a Map and off a
/// Node/Rel identically) but nothing else. A `false` return means the caller
/// must MATERIALISE the group-key entity in full, byte-identical to today. The
/// match is exhaustive (no wildcard) so a new `Expr` variant cannot silently be
/// treated as prop-only.
fn group_key_prop_only(e: &Expr, var: &str, props: &mut BTreeSet<String>) -> bool {
    match e {
        // `var.key` — the one shape the gathered column reproduces. Record the
        // key and DO NOT descend into the `var` child (it is not a whole use).
        Expr::Prop(of, key) => {
            if matches!(of.as_ref(), Expr::Var(v) if v == var) {
                props.insert(key.clone());
                true
            } else {
                group_key_prop_only(of, var, props)
            }
        }
        // A bare occurrence of the group-key var is a WHOLE-entity use.
        Expr::Var(v) => v != var,
        // No variables — trivially prop-only.
        Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::Param(_) => true,
        Expr::List(items) => items.iter().all(|x| group_key_prop_only(x, var, props)),
        Expr::Map(entries) => entries
            .iter()
            .all(|(_, x)| group_key_prop_only(x, var, props)),
        Expr::Call { args, .. } => args.iter().all(|x| group_key_prop_only(x, var, props)),
        Expr::Bin(_, a, b)
        | Expr::And(a, b)
        | Expr::Or(a, b)
        | Expr::Xor(a, b)
        | Expr::In(a, b)
        | Expr::Index(a, b) => {
            group_key_prop_only(a, var, props) && group_key_prop_only(b, var, props)
        }
        Expr::Not(a) | Expr::Neg(a) => group_key_prop_only(a, var, props),
        Expr::IsNull { of, .. } | Expr::HasLabels { of, .. } => group_key_prop_only(of, var, props),
        Expr::Slice { of, from, to } => {
            group_key_prop_only(of, var, props)
                && from
                    .as_deref()
                    .is_none_or(|x| group_key_prop_only(x, var, props))
                && to
                    .as_deref()
                    .is_none_or(|x| group_key_prop_only(x, var, props))
        }
        Expr::Case {
            subject,
            arms,
            otherwise,
        } => {
            subject
                .as_deref()
                .is_none_or(|x| group_key_prop_only(x, var, props))
                && arms.iter().all(|(w, t)| {
                    group_key_prop_only(w, var, props) && group_key_prop_only(t, var, props)
                })
                && otherwise
                    .as_deref()
                    .is_none_or(|x| group_key_prop_only(x, var, props))
        }
        // Comprehensions bind their OWN locals (shadowing is possible):
        // `free_vars` models those scopes exactly, so a comprehension that
        // does not read `var` at all — `[x IN collect({name: ent.name})
        // WHERE x.name IS NOT NULL]` beside a carried `n` — is trivially
        // prop-only. Until this held it declined the carry to FULL: the
        // production email revival pick decoded 16,084 emails (bodies
        // along, 5,900 block misses) for a top-1 whose RETURN read three
        // properties of it — 2.1 s against Neo4j's 74 ms. One that DOES
        // read `var` is declined as before (fix 36a).
        Expr::ListComp { .. }
        | Expr::Reduce { .. }
        | Expr::ListPredicate { .. }
        | Expr::MapProjection { .. } => {
            let mut fv = Vec::new();
            free_vars(e, &mut Vec::new(), &mut fv);
            if fv.iter().any(|v| v == var) {
                false
            } else {
                counted!("interp.comprehension beside a carry reads nothing of it");
                true
            }
        }
        // Fix 72: a PATTERN-shaped subquery (`sum(COUNT { (b)<-[:R]-(a:A) })`
        // — the fold's spelling of a counted chain) uses an outer endpoint
        // by IDENTITY: the expansion starts from its id and reads nothing
        // else (`demand_pattern_endpoints` applies the same rule). A group
        // key named bare as such an endpoint stays prop-only; a path or
        // relationship var on the name, or an endpoint restating a props
        // map on it, is a whole-entity use; the body's WHERE and inline
        // maps are walked like any other expression, so their reads of
        // the var join the gathered props. Until this held the fold's
        // projection declined the carry to FULL: every grouped node
        // decoded for a count that never read it.
        Expr::ExistsSub(body) | Expr::CountSub(body) => match pattern_body(body) {
            Some((pattern, where_)) => {
                let endpoints = pattern.paths.iter().all(|path| {
                    if path.var.as_deref() == Some(var) {
                        return false;
                    }
                    let nodes = std::iter::once(&path.start).chain(path.hops.iter().map(|(_, n)| n));
                    for n in nodes {
                        if n.var.as_deref() == Some(var) && n.props.is_some() {
                            return false;
                        }
                        if let Some(p) = &n.props {
                            if !group_key_prop_only(p, var, props) {
                                return false;
                            }
                        }
                    }
                    path.hops.iter().all(|(rel, _)| {
                        rel.var.as_deref() != Some(var)
                            && rel
                                .props
                                .as_ref()
                                .is_none_or(|p| group_key_prop_only(p, var, props))
                    })
                });
                endpoints && where_.is_none_or(|w| group_key_prop_only(w, var, props))
            }
            None => false,
        },
        // The opaque pattern shapes own scopes `free_vars` cannot see into;
        // decline the whole projection to the materialising fallback
        // whenever one appears — correctness first.
        Expr::PatternPredicate(_) | Expr::PatternComp { .. } => false,
    }
}

/// Gather the referenced PROPERTIES of every grouping-key var over its DISTINCT
/// ids, so an aggregating projection can read `key.prop` from a column instead
/// of MATERIALISING the whole group-key entity per group (`value_of` →
/// `graph.node`, which decodes every property). Returns one `id -> Value::Map`
/// lookup per grouping var in `group_var_idx` order — each Map holds exactly the
/// properties the projection reads on that var, aligned so an ABSENT property is
/// `Value::Null` (identical to `value_of(node).prop` on a missing property).
///
/// `Ok(None)` = fall back to full materialisation, byte-identical to today: a
/// grouping var is used as a WHOLE entity somewhere the Map cannot reproduce, a
/// column span is over the loader's budget, there is no grouping key at all
/// (a global / const aggregate), or a group key is the OPTIONAL null sentinel.
#[allow(clippy::too_many_arguments)]
fn gather_group_key_columns(
    graph: &Graph,
    gkc: &crate::pipeline::GroupKeyCols,
    items: &[(String, Expr)],
    agg_items: &[AggItem],
    order: &[engram_cypher::stmt::OrderItem],
    vars: &[String],
    kinds: &[VarKind],
    // The pattern labels each var is bound under (fix 33): a single-label
    // node var's gather reads the label's cached column instead of a record
    // per group. Empty (or `None` per var) where the caller has no plan.
    var_labels: &[Option<Vec<String>>],
    params: &BTreeMap<String, Value>,
    group_var_idx: &[usize],
    // The group-key id rows to gather FOR — every group, or only the top-k
    // survivors when `project_agg_groups` has already chosen them: on ic6 the
    // gather over all 9,599 groups cost more than the projection it replaced.
    group_ids: &[Vec<u64>],
    // The expressions the clauses AFTER this projection evaluate over its
    // output (an aggregating WITH's post-WHERE and the RETURN's items and
    // ORDER BY keys) — what a BARE group-key carry is read by. Empty for an
    // aggregating RETURN, whose bare key IS the output.
    later: &[&Expr],
) -> Result<Option<Vec<BTreeMap<u64, Value>>>, RunError> {
    // A global (no grouping key) or const-only aggregate has no group-key entity
    // to gather — the template stays empty, unchanged.
    if group_var_idx.is_empty() {
        return Ok(None);
    }
    // The properties each grouping var is read through, and — as a side effect of
    // `group_key_prop_only` returning false — the whole-entity guard. Scan exactly
    // the expressions the group TEMPLATE evaluates: each grouping item's
    // expression (`AggItem::Key`) and each aggregate-bearing item's REWRITTEN
    // expression (`AggItem::Agg` — its aggregate sites already lifted to `$__aggN`,
    // so only its non-aggregate parts read the template), plus the ORDER BY keys.
    //
    // A BARE carry — `WITH p, collect(DISTINCT a.id) AS ids` — is the one
    // grouping item that is not a property read of its var, and it made every
    // such WITH materialise its key in full (73 full Proposal records per
    // statement on the mirror, 6.6 ms against Neo4j's 1.9) although the
    // RETURN after it read only `p.id`. The carry's readers are the LATER
    // clauses: when every one of them reads the var by property, the gathered
    // Map is what they read, byte-identical (`p.id` off a Map equals `.id` off
    // the node, absent-property Null included). A later bare use, a function
    // over the var, an alias on the carry, or no later clause at all keeps the
    // full materialisation.
    let mut props: Vec<BTreeSet<String>> = vec![BTreeSet::new(); group_var_idx.len()];
    let mut bare_carries = 0usize;
    for (gi, &vi) in group_var_idx.iter().enumerate() {
        let var = &vars[vi];
        for ((name, e), plan) in items.iter().zip(agg_items) {
            let scanned = match plan {
                AggItem::Key => e,
                AggItem::Agg { rewritten, .. } => rewritten,
            };
            let bare_carry = matches!(plan, AggItem::Key)
                && matches!(scanned, Expr::Var(v) if v == var)
                && name == var;
            if bare_carry {
                if later.is_empty() {
                    return Ok(None); // the key IS the output
                }
                bare_carries += 1;
                continue;
            }
            if !group_key_prop_only(scanned, var, &mut props[gi]) {
                return Ok(None);
            }
        }
        for o in order {
            if !group_key_prop_only(&o.expr, var, &mut props[gi]) {
                return Ok(None);
            }
        }
        for e in later {
            if !group_key_prop_only(e, var, &mut props[gi]) {
                return Ok(None);
            }
        }
    }
    if bare_carries > 0 {
        counted!("interp.agg bare group key gathered for its later reads");
    }
    // Gather each grouping var's referenced columns over its DISTINCT group-key
    // ids and build the `id -> stand-in` lookup. The stand-in is a KIND-correct
    // entity carrying the id and only the gathered properties — never a bare
    // `Value::Map`: a projection that is DISTINCT (`WITH DISTINCT b RETURN
    // b.bx`) dedups its output by `agg_key_of`, which keys a node or
    // relationship by its id and a map by its contents, so three distinct
    // nodes sharing the gathered values collapsed into one row while the
    // per-tuple path kept all three. `b.bx` off the stand-in is the same
    // property read either way (absent-property Null included).
    let stand_in = |kind: VarKind, id: u64, props: BTreeMap<String, Value>| match kind {
        VarKind::Node => Value::Node {
            id,
            labels: Vec::new(),
            props,
        },
        VarKind::Rel => Value::Rel {
            id,
            src: 0,
            dst: 0,
            rel_type: String::new(),
            props,
        },
    };
    let mut lookups: Vec<BTreeMap<u64, Value>> = Vec::with_capacity(group_var_idx.len());
    for (gi, &vi) in group_var_idx.iter().enumerate() {
        let mut distinct: BTreeSet<u64> = BTreeSet::new();
        for ids in group_ids {
            let id = ids[vi];
            // A nullable grouping var (fix 30: the OPTIONAL tail admits a bare
            // or direct-property key over an unmatched optional var) carries
            // the null sentinel for its null-fill group; it has no properties
            // to gather and the template binds it as Null, so it stays out of
            // the id span (which it would widen pathologically).
            if id == NULL_ID {
                continue;
            }
            distinct.insert(id);
        }
        let distinct: Vec<u64> = distinct.into_iter().collect();
        // REUSE the columns `reduce_agg_groups` already loaded to FORM the groups
        // when they cover every projected property (IC5's group key `forum.id`
        // always is) — eliminating the redundant second sparse point-gather.
        // `reduced.0` is a SUPERSET of `distinct`, sorted, so a group id binary-
        // searches into it; byte-identical to a fresh load (same store, same
        // primitive). Otherwise fall back to the load.
        let reduced = gkc.get(&vi);
        let all_reused = reduced
            .map(|(_, c)| props[gi].iter().all(|pr| c.contains_key(pr)))
            .unwrap_or(false);
        let mut lookup: BTreeMap<u64, Value> = BTreeMap::new();
        if all_reused {
            let (rd, rc) = reduced.expect("all_reused implies Some");
            for &id in &distinct {
                let pos = rd
                    .binary_search(&id)
                    .expect("a group id is in the reduction's distinct set");
                let mut m: BTreeMap<String, Value> = BTreeMap::new();
                for pr in &props[gi] {
                    m.insert(pr.clone(), rc[pr][pos].clone());
                }
                lookup.insert(id, stand_in(kinds[vi], id, m));
            }
            counted!("interp.agg group-key cols reused");
        } else {
            // The SAME primitive the rest of the pipeline uses — the label's
            // cached column when the var's labels are known (fix 33), else a
            // range scan that point-gathers on a sparse/wide decline. `None` =
            // over the budget.
            let labels = var_labels.get(vi).and_then(|l| l.as_deref());
            let Some(cols) = crate::pipeline::load_var_columns_labelled(
                graph,
                kinds[vi],
                &distinct,
                &props[gi],
                labels,
                params,
            )?
            else {
                return Ok(None);
            };
            for (i, &id) in distinct.iter().enumerate() {
                let mut m: BTreeMap<String, Value> = BTreeMap::new();
                for (pcol, col) in &cols {
                    m.insert(pcol.clone(), col[i].clone());
                }
                lookup.insert(id, stand_in(kinds[vi], id, m));
            }
        }
        lookups.push(lookup);
    }
    counted!("interp.agg group-key props gathered");
    Ok(Some(lookups))
}

/// Late-materialise a general aggregating projection's GROUP ROWS through the
/// SHARED projection tail — the columnar group-by-aggregate's output path, the
/// generalisation of the former count-only projector and the aggregating twin of
/// [`project_rows_tail`]. Each group carries the ids of its FIRST-SEEN
/// representative row (one node id per var in `vars`) and the group's folded
/// per-site accumulators (`SiteAcc`, one per `AggSite`, in site order — the
/// pipeline pushed values into them in PRODUCTION order, so `run_streaming`'s
/// fold is reproduced byte-for-byte). For each group the grouping-key vars
/// (`group_var_idx`) are materialised into a template Row ONCE, then every
/// projection item is evaluated exactly as the per-tuple aggregating projector
/// (`StreamProjector::finish`) does: a grouping item (`AggItem::Key`) through
/// `eval_expr` over the template (so a var-key yields the node and a `var.prop`
/// key its value), and an aggregate-bearing item (`AggItem::Agg`) by `finish`ing
/// each of its sites through the ONE shared aggregate implementation, SUBSTITUTING
/// each finished value as the synthetic `$__aggN` parameter, and evaluating the
/// REWRITTEN expression over the template — so `count(*)`, `sum(x)+1`,
/// `1.0*sum(x)/count(*)` and multiple aggregates per item all reduce to the same
/// substitution. ORDER BY resolves by structural match against the projection
/// items first (`ORDER BY count(*)` beside `RETURN count(*) AS c`), else over the
/// projected output columns — byte-identical to the per-tuple path. `project_tail`
/// then re-sorts (stable) and applies SKIP/LIMIT.
#[allow(clippy::too_many_arguments)]
fn project_agg_groups(
    graph: &Graph,
    proj: &Projection,
    params: &BTreeMap<String, Value>,
    vars: &[String],
    kinds: &[VarKind],
    var_labels: &[Option<Vec<String>>],
    group_var_idx: &[usize],
    sites: &[AggSite],
    agg_items: &[AggItem],
    gkc: &crate::pipeline::GroupKeyCols,
    groups: Vec<(Vec<u64>, Vec<SiteAcc>)>,
    later: &[&Expr],
) -> Result<Projected, RunError> {
    // Resolve items exactly as `StreamProjector::resolve_items` — alias, else the
    // positional column name. The recognizer declines `*`, so there is none.
    let mut items: Vec<(String, Expr)> = Vec::with_capacity(proj.items.len());
    for (i, item) in proj.items.iter().enumerate() {
        let name = item
            .alias
            .clone()
            .or_else(|| item.text.clone())
            .unwrap_or_else(|| column_name(&item.expr, i));
        items.push((name, item.expr.clone()));
    }
    let columns: Vec<String> = items.iter().map(|(n, _)| n.clone()).collect();
    // Gather the group-key PROPERTIES the projection reads instead of
    // materialising each group-key entity in full — the waste this optimisation
    // removes (IC5's `RETURN forum.id …` decoded one full Forum node per group
    // just to read the group key). `None` = every group-key use needs the whole
    // entity, or a span declined: the full `value_of` template below, unchanged.
    // Fold every site's accumulator through the ONE shared implementation of
    // the aggregate semantics — byte-identical to `StreamProjector::finish`.
    // Done for EVERY group up front (a pure function of the accumulator, so
    // the order is immaterial) because an aggregate-keyed top-k below reads
    // its ORDER BY keys straight off these finished values.
    let mut finished: Vec<(Vec<u64>, Vec<Value>)> = Vec::with_capacity(groups.len());
    for (ids, accs) in groups {
        let mut computed: Vec<Value> = Vec::with_capacity(sites.len());
        for (site, acc) in sites.iter().zip(accs) {
            computed.push(acc.finish(site)?);
        }
        finished.push((ids, computed));
    }

    // ── TOP-K BEFORE PROJECTION ─────────────────────────────────────────────
    //
    // `ORDER BY c DESC LIMIT 10` over `RETURN t.name, count(*) AS c` used to
    // PROJECT EVERY GROUP — a template Row, an `eval_expr` per item, a scope
    // row — and then let `project_tail` sort and keep ten. On ic6 that is
    // 9,599 groups × 3 expression evaluations (the 28,800 the profile counted)
    // for ten survivors: ~70 ms of a 90 ms query whose whole walk is 10 ms.
    //
    // When every ORDER BY key is a PURE aggregate item, the key IS the
    // finished site value — `eval_expr` of a bare `$__aggN` returns exactly
    // `computed[N]` — so the survivors can be chosen here with the SAME
    // comparator and the SAME stable tie rule `project_tail` applies, and only
    // they are projected. The tail then re-sorts (stably, same order) and
    // applies SKIP/LIMIT exactly, as `project_pairs_tail` already relies on.
    // Rows are byte-identical; only the work changes. Declined for DISTINCT
    // (dedup after selection could fall short of the limit), for any key that
    // is not a pure aggregate, and when the limit does not shrink the set.
    let survivors: Option<Vec<bool>> = if graph.agg_topk_before_project()
        && !proj.distinct
        && !proj.order.is_empty()
        && proj.limit.is_some()
    {
        agg_topk_survivors(graph, proj, params, &items, agg_items, &finished)?
    } else {
        None
    };
    if survivors.is_some() {
        counted!("interp.agg top-k selected before projection");
    }

    // Gather the group-key PROPERTIES the projection reads instead of
    // materialising each group-key entity in full — the waste this
    // optimisation removes (IC5's `RETURN forum.id …` decoded one full Forum
    // node per group just to read the group key). Gathered AFTER the top-k
    // selection and ONLY for the survivors: v70 measured the gather over all
    // 9,599 of ic6's groups at ~40 ms, more than the projection loop it sits
    // beside. `None` = every group-key use needs the whole entity, or a span
    // declined: the full `value_of` template below, unchanged.
    let gather_ids: Vec<Vec<u64>> = match &survivors {
        Some(keep) => finished
            .iter()
            .enumerate()
            .filter(|(g, _)| keep[*g])
            .map(|(_, (ids, _))| ids.clone())
            .collect(),
        None => finished.iter().map(|(ids, _)| ids.clone()).collect(),
    };
    let gathered = gather_group_key_columns(
        graph,
        gkc,
        &items,
        agg_items,
        &proj.order,
        vars,
        kinds,
        var_labels,
        params,
        group_var_idx,
        &gather_ids,
        later,
    )?;

    let mut out_rows: Vec<Vec<Value>> = Vec::with_capacity(finished.len());
    let mut order_keys: Vec<Vec<Value>> = Vec::with_capacity(finished.len());
    for (g, (ids, computed)) in finished.into_iter().enumerate() {
        if let Some(keep) = &survivors {
            if !keep[g] {
                continue;
            }
        }
        // The template: only the grouping-key vars of the first-seen row. When the
        // group key is read ONLY through properties, bind a `Value::Map` of the
        // GATHERED columns (no full-node decode) — `key.prop` off that Map is
        // byte-identical to `value_of(node).prop`, absent-property NULL included.
        // Otherwise materialise the entity late (once per group), exactly as the
        // per-tuple projector keeps the group's `template`. A global (no grouping
        // key) aggregate has none, so the template stays empty.
        let mut template = Row::new();
        for (gi, &vi) in group_var_idx.iter().enumerate() {
            let bound = match &gathered {
                // The OPTIONAL null-fill group of a nullable grouping var (fix
                // 30) binds Null — `null.prop` is null, as the per-tuple path
                // reads an unmatched optional var.
                Some(_) if ids[vi] == NULL_ID => Value::Null,
                // Every real group-key id was collected into this lookup above,
                // so the get always hits — the Map replaces a full-node
                // materialisation.
                Some(lookups) => lookups[gi]
                    .get(&ids[vi])
                    .cloned()
                    .expect("every group-key id was gathered"),
                // `value_of` maps the null sentinel to Null itself; routing by
                // kind keeps the id→value mapping in one place for both node
                // and relationship group keys.
                None => value_of(graph, kinds[vi], ids[vi])?,
            };
            template.insert(vars[vi].clone(), bound);
        }
        let mut out = Vec::with_capacity(items.len());
        for ((_, e), plan) in items.iter().zip(agg_items) {
            match plan {
                AggItem::Key => out.push(eval_expr(graph, e, &template, params)?),
                AggItem::Agg {
                    rewritten,
                    site_range,
                } => {
                    let mut p = params.clone();
                    for global in site_range.clone() {
                        p.insert(format!("__agg{global}"), computed[global].clone());
                    }
                    out.push(eval_expr(graph, rewritten, &template, &p)?);
                }
            }
        }
        let mut scope_row = Row::new();
        for (c, v) in columns.iter().zip(&out) {
            scope_row.insert(c.clone(), v.clone());
        }
        let mut okey = Vec::with_capacity(proj.order.len());
        for o in &proj.order {
            if let Some(j) = items.iter().position(|(_, e)| *e == o.expr) {
                okey.push(out[j].clone());
            } else {
                okey.push(eval_expr(graph, &o.expr, &scope_row, params)?);
            }
        }
        order_keys.push(okey);
        out_rows.push(out);
    }
    project_tail(graph, proj, params, columns, out_rows, order_keys)
}

/// The survivor mask for an aggregating ORDER BY + LIMIT projection whose
/// EVERY ORDER BY key is a pure aggregate item (`count(*) AS c … ORDER BY c`):
/// the key is the finished site value and needs no projection to compute.
///
/// `None` — the general path projects every group — when any key is not a
/// pure aggregate (a group-key property, an expression over an aggregate),
/// when SKIP/LIMIT do not evaluate, or when `skip + limit` does not shrink the
/// group set. Selection uses `project_tail`'s comparator on the same keys with
/// the same stable rule (equal keys stay in first-seen order), so the survivors
/// are exactly the rows the full sort would have kept.
fn agg_topk_survivors(
    graph: &Graph,
    proj: &Projection,
    params: &BTreeMap<String, Value>,
    items: &[(String, Expr)],
    agg_items: &[AggItem],
    finished: &[(Vec<u64>, Vec<Value>)],
) -> Result<Option<Vec<bool>>, RunError> {
    let mut key_sites: Vec<usize> = Vec::with_capacity(proj.order.len());
    for o in &proj.order {
        // Resolve exactly as the projection loop does: a structural match
        // against an item's expression (`ORDER BY count(*)`), else the
        // OUTPUT COLUMN the expression names (`ORDER BY c` beside
        // `count(*) AS c` — a bare variable that only exists as a column).
        // Either way the key's value is `out[j]`, which for a pure aggregate
        // item is the finished site value.
        let structural = items.iter().position(|(_, e)| *e == o.expr);
        let by_column = match &o.expr {
            Expr::Var(name) => items.iter().position(|(n, _)| n == name),
            _ => None,
        };
        let Some(j) = structural.or(by_column) else {
            return Ok(None);
        };
        match &agg_items[j] {
            AggItem::Agg {
                rewritten,
                site_range,
            } if site_range.len() == 1
                && matches!(rewritten, Expr::Param(p) if *p == format!("__agg{}", site_range.start)) =>
            {
                key_sites.push(site_range.start);
            }
            _ => return Ok(None),
        }
    }
    let skip = eval_count(graph, proj.skip.as_ref(), params, "SKIP")?.unwrap_or(0);
    let Some(limit) = eval_count(graph, proj.limit.as_ref(), params, "LIMIT")? else {
        return Ok(None);
    };
    let keep = skip.saturating_add(limit);
    if keep >= finished.len() {
        return Ok(None);
    }
    let keys: Vec<Vec<Value>> = finished
        .iter()
        .map(|(_, computed)| key_sites.iter().map(|&s| computed[s].clone()).collect())
        .collect();
    let mut idx: Vec<usize> = (0..finished.len()).collect();
    // `sort_by` is stable: equal keys keep first-seen order, exactly as the tail.
    idx.sort_by(|&a, &b| cmp_order_keys(&proj.order, &keys[a], &keys[b]));
    let mut mask = vec![false; finished.len()];
    for &i in &idx[..keep] {
        mask[i] = true;
    }
    Ok(Some(mask))
}

/// Run a general aggregating RETURN (the pipeline's Form B — `MATCH <chain>
/// RETURN <keys>, <aggregates> [ORDER BY …] [SKIP] [LIMIT]`) over groups the
/// columnar operator already reduced. The RETURN *is* the aggregating
/// projection, so this is [`project_agg_groups`] directly.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_agg_return(
    graph: &Graph,
    proj: &Projection,
    params: &BTreeMap<String, Value>,
    vars: &[String],
    kinds: &[VarKind],
    var_labels: &[Option<Vec<String>>],
    group_var_idx: &[usize],
    sites: &[AggSite],
    agg_items: &[AggItem],
    gkc: &crate::pipeline::GroupKeyCols,
    groups: Vec<(Vec<u64>, Vec<SiteAcc>)>,
) -> Result<QueryResult, RunError> {
    Ok(project_agg_groups(
        graph,
        proj,
        params,
        vars,
        kinds,
        var_labels,
        group_var_idx,
        sites,
        agg_items,
        gkc,
        groups,
        &[],
    )?
    .into_result())
}

/// Run a general aggregating WITH (the pipeline's Form A — `MATCH <chain> WITH
/// <keys>, <aggregates> [WHERE <post>] RETURN <exprs over the WITH aliases>
/// [ORDER BY …] [SKIP] [LIMIT]`) over groups the columnar operator already
/// reduced. The WITH breaker carries no ORDER/SKIP/LIMIT (the recognizer declines
/// those), so its group rows stay in FIRST-SEEN order; the optional post-WITH
/// WHERE then filters those rows and the plain RETURN projects them — through the
/// very code the streamed WITH→RETURN stages use (`filter_rows` + `project`), so
/// the output is byte-identical.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_agg_with(
    graph: &Graph,
    with_proj: &Projection,
    post_where: Option<&Expr>,
    return_proj: &Projection,
    params: &BTreeMap<String, Value>,
    vars: &[String],
    kinds: &[VarKind],
    var_labels: &[Option<Vec<String>>],
    group_var_idx: &[usize],
    sites: &[AggSite],
    agg_items: &[AggItem],
    gkc: &crate::pipeline::GroupKeyCols,
    groups: Vec<(Vec<u64>, Vec<SiteAcc>)>,
) -> Result<QueryResult, RunError> {
    // Lever G' on this tail (fix 31): a group-key carry the top-k RETURN
    // outputs BARE and otherwise reads only by property (`WITH p, collect(…)
    // AS ids, r.id AS rid RETURN p, ids, rid ORDER BY p.priority DESC, …
    // LIMIT 25`) is gathered like any other carry — its ORDER BY and other
    // property reads — and the k output rows are hydrated with the full node
    // afterwards. Until this held the bare item kept every group's key in
    // full: 73 Proposal records decoded for the 25 paged, the one full-record
    // read left on the listing after the left join took it.
    // `(name, the property reads the RETURN makes of it — through the bare
    // item's alias too: `RETURN p AS proposal … ORDER BY proposal.priority`
    // reads p's property off the output column)`.
    let late_full: Vec<(String, BTreeSet<String>)> = if graph.late_projection_enabled()
        && topk_return_shape(return_proj)
    {
        with_proj
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| {
                let name = it
                    .alias
                    .clone()
                    .or_else(|| it.text.clone())
                    .unwrap_or_else(|| column_name(&it.expr, i));
                // A bare carry of a grouping var (`WITH p …`, not an alias
                // and not an expression) whose later reads admit it.
                let carried = matches!(&it.expr, Expr::Var(v) if *v == name)
                    && post_where.is_none_or(|w| {
                        let mut ps = BTreeSet::new();
                        group_key_prop_only(w, &name, &mut ps)
                    });
                if !carried {
                    return None;
                }
                late_full_reads(return_proj, &name).map(|props| (name, props))
            })
            .collect()
    } else {
        Vec::new()
    };
    let late_names: Vec<&String> = late_full.iter().map(|(n, _)| n).collect();
    // The alias reads as direct property reads of the carry, so the gather
    // below sees them (it scans `later` for `p.<prop>` only).
    let alias_reads: Vec<Expr> = late_full
        .iter()
        .flat_map(|(name, props)| {
            props
                .iter()
                .map(|p| Expr::Prop(Box::new(Expr::Var(name.clone())), p.clone()))
        })
        .collect();
    // What the clauses after the WITH read of its output: the post-WHERE, the
    // RETURN's items and its ORDER BY keys (SKIP/LIMIT are var-free). A bare
    // RETURN item of a late-full carry is not a read of its properties.
    let later: Vec<&Expr> = post_where
        .into_iter()
        .chain(return_proj.items.iter().filter_map(|it| match &it.expr {
            Expr::Var(v) if late_names.contains(&v) => None,
            e => Some(e),
        }))
        .chain(return_proj.order.iter().map(|o| &o.expr))
        .chain(alias_reads.iter())
        .collect();
    let projected = project_agg_groups(
        graph,
        with_proj,
        params,
        vars,
        kinds,
        var_labels,
        group_var_idx,
        sites,
        agg_items,
        gkc,
        groups,
        &later,
    )?;
    let with_cols = projected.columns.clone();
    let mut rows = projected.into_rows()?;
    if let Some(w) = post_where {
        rows = filter_rows(graph, rows, w, params)?;
    }
    let mut result = project(graph, return_proj, rows, params, &with_cols)?.into_result();
    if !late_full.is_empty() {
        // The output columns that ARE a late-full carry, hydrated for the k
        // rows the top-k kept; a carry that was materialised in full anyway
        // (the gather declined) is re-read once per row, byte-identical.
        let hydrate: Vec<usize> = return_proj
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| matches!(&it.expr, Expr::Var(v) if late_names.contains(&v)))
            .map(|(i, _)| i)
            .collect();
        for row in &mut result.rows {
            for &ci in &hydrate {
                if let Some(Value::Node { id, .. }) = row.get(ci) {
                    let nid = *id;
                    if let Some(full) = graph.node(nid)? {
                        counted!("interp.agg bare return item hydrated for a survivor");
                        row[ci] = full;
                    }
                }
            }
        }
    }
    Ok(result)
}

/// The streaming projector's bounded top-k as a binary MAX-heap over (sort
/// key, arrival): the root is the kept row that would be dropped first, so
/// a push is O(log k). It replaced a sorted `Vec` kept by `partition_point`,
/// `insert` and `truncate` — an O(k) memmove per row: the inbox listing's
/// `ORDER BY n.createdAt DESC SKIP 10000 LIMIT 1000` keeps k = 11,000 rows
/// and received 18k, ~11 GB moved per statement, and page 10 cost 2.0 s
/// more than page 1 on the mirror for identical work (fix 43). `topk_sort`
/// restores (key, arrival) order at finish — the tail's stable re-sort
/// relies on it. Returns whether the entry was kept.
fn topk_push<T>(
    heap: &mut Vec<(Vec<Value>, u64, T)>,
    cap: usize,
    entry: (Vec<Value>, u64, T),
    order: &[engram_cypher::stmt::OrderItem],
) -> bool {
    if cap == 0 {
        return false;
    }
    // `a` sorts AFTER `b`: a later key, or an equal key and a later arrival.
    let after = |a: &(Vec<Value>, u64, T), b: &(Vec<Value>, u64, T)| -> bool {
        match cmp_order_keys(order, &a.0, &b.0) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => a.1 > b.1,
        }
    };
    if heap.len() < cap {
        heap.push(entry);
        let mut i = heap.len() - 1;
        while i > 0 {
            let p = (i - 1) / 2;
            if after(&heap[i], &heap[p]) {
                heap.swap(i, p);
                i = p;
            } else {
                break;
            }
        }
        return true;
    }
    // Full: the entry displaces the root only when it sorts BEFORE it.
    if !after(&heap[0], &entry) {
        return false;
    }
    heap[0] = entry;
    let n = heap.len();
    let mut i = 0usize;
    loop {
        let l = 2 * i + 1;
        let r = l + 1;
        let mut m = i;
        if l < n && after(&heap[l], &heap[m]) {
            m = l;
        }
        if r < n && after(&heap[r], &heap[m]) {
            m = r;
        }
        if m == i {
            break;
        }
        heap.swap(i, m);
        i = m;
    }
    true
}

/// The heap's entries in (sort key, arrival) order — what the sorted vector
/// held.
fn topk_sort<T>(heap: &mut [(Vec<Value>, u64, T)], order: &[engram_cypher::stmt::OrderItem]) {
    heap.sort_by(|a, b| cmp_order_keys(order, &a.0, &b.0).then(a.1.cmp(&b.1)));
}

/// Total order for ORDER BY: within-type via the value model; across types by
/// a documented rank; null sorts LAST ascending. (Neo4j's full cross-type
/// orderability is richer; divergence is confined to mixed-type keys.)
/// Compare two ORDER BY key tuples under the projection's directions.
pub(crate) fn cmp_order_keys(
    order: &[engram_cypher::stmt::OrderItem],
    a: &[Value],
    b: &[Value],
) -> std::cmp::Ordering {
    for (o, (ka, kb)) in order.iter().zip(a.iter().zip(b)) {
        let ord = order_cmp(ka, kb);
        let ord = if o.desc { ord.reverse() } else { ord };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

fn order_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Int(_) | Value::Float(_) => 0,
            Value::Str(_) => 1,
            Value::Bool(_) => 2,
            Value::Date(_) => 3,
            Value::Time { .. } => 4,
            Value::LocalTime(_) => 5,
            Value::DateTime { .. } => 6,
            Value::LocalDateTime { .. } => 7,
            Value::Duration { .. } => 8,
            // A PATH sorts among the lists (it is a node/rel trail); sharing
            // List's rank keeps ORDER BY total without a bespoke path order.
            Value::List(_) | Value::Path(_) => 9,
            Value::Map(_) => 10,
            Value::Node { .. } => 11,
            Value::Rel { .. } => 12,
            Value::Null => 13,
        }
    }
    let (ra, rb) = (rank(a), rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    // Two LISTS (or PATHS — node/rel trails) get a TOTAL order — element-wise by
    // openCypher ORDERABILITY, then by length. `lt3` (the scalar 3-valued
    // comparison) returns Unknown for a null- or mixed-type-bearing element and
    // would collapse distinct lists (`[null, 1]` vs `[null, 2]`) to Equal,
    // leaving them unsorted. `minmax_cmp` is the same orderability `min`/`max`
    // use, so ORDER BY and min/max agree.
    if matches!(
        (a, b),
        (
            Value::List(_) | Value::Path(_),
            Value::List(_) | Value::Path(_)
        )
    ) {
        return minmax_cmp(a, b);
    }
    match a.lt3(b) {
        Truth::True => Ordering::Less,
        _ => match b.lt3(a) {
            Truth::True => Ordering::Greater,
            _ => Ordering::Equal,
        },
    }
}

/// openCypher ORDERABILITY for `min`/`max` — a TOTAL order across types in which
/// a Number is the GREATEST value and a Map / Node / List among the least, lists
/// compared element-wise then by length. This is deliberately NOT `order_cmp`
/// (ORDER BY's Neo4j-style order, where a Number sorts first): the openCypher TCK
/// fixes `max([1,'a',[1,2],0.2]) = 1` and `min(...) = [1,2]`. Same-type numerics/
/// strings/bools fall to `lt3`, so a homogeneous min/max is unchanged. `min`/`max`
/// ignore nulls, so null's rank is never consulted.
fn minmax_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Map(_) => 0,
            Value::Node { .. } => 1,
            Value::Rel { .. } => 2,
            Value::List(_) | Value::Path(_) => 3,
            Value::Duration { .. } => 4,
            Value::Date(_) => 5,
            Value::LocalTime(_) => 6,
            Value::Time { .. } => 7,
            Value::LocalDateTime { .. } => 8,
            Value::DateTime { .. } => 9,
            Value::Str(_) => 10,
            Value::Bool(_) => 11,
            Value::Int(_) | Value::Float(_) => 12,
            Value::Null => 13,
        }
    }
    let (ra, rb) = (rank(a), rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    if let (Value::List(la) | Value::Path(la), Value::List(lb) | Value::Path(lb)) = (a, b) {
        for (x, y) in la.iter().zip(lb.iter()) {
            let c = minmax_cmp(x, y);
            if c != Ordering::Equal {
                return c;
            }
        }
        return la.len().cmp(&lb.len());
    }
    match a.lt3(b) {
        Truth::True => Ordering::Less,
        _ => match b.lt3(a) {
            Truth::True => Ordering::Greater,
            _ => Ordering::Equal,
        },
    }
}

pub(crate) fn eval_count(
    graph: &Graph,
    e: Option<&Expr>,
    params: &BTreeMap<String, Value>,
    what: &str,
) -> Result<Option<usize>, RunError> {
    match e {
        None => Ok(None),
        Some(e) => match eval_expr(graph, e, &Row::new(), params)? {
            Value::Int(v) if v >= 0 => Ok(Some(v as usize)),
            other => Err(RunError::Semantic(format!(
                "{what} takes a non-negative integer, got {other:?}"
            ))),
        },
    }
}

/// Evaluate an expression CONTAINING aggregate calls over a group: aggregate
/// subexpressions are computed first and substituted as synthetic parameters,
/// then the rewritten expression evaluates once.
fn eval_aggregate(
    graph: &Graph,
    e: &Expr,
    group: &[Row],
    params: &BTreeMap<String, Value>,
) -> Result<Value, RunError> {
    let mut sites: Vec<AggSite> = Vec::new();
    let rewritten = extract_aggregates(e, &mut sites);
    let mut p = params.clone();
    for (i, site) in sites.iter().enumerate() {
        let v = compute_aggregate(
            graph,
            &site.name,
            site.distinct,
            &site.args,
            site.star,
            group,
            params,
        )?;
        p.insert(format!("__agg{i}"), v);
    }
    let template = group.first().cloned().unwrap_or_default();
    eval_expr(graph, &rewritten, &template, &p)
}

/// One aggregate call site, lifted out of an expression.
#[derive(Clone)]
pub(crate) struct AggSite {
    pub(crate) name: String,
    pub(crate) distinct: bool,
    pub(crate) args: Vec<Expr>,
    pub(crate) star: bool,
}

/// Structurally rewrite aggregate CALLS into `$__aggN` parameter references,
/// recording each site — the single traversal both execution paths use.
pub(crate) fn extract_aggregates(e: &Expr, sites: &mut Vec<AggSite>) -> Expr {
    if let Expr::Call {
        name,
        distinct,
        args,
        star,
    } = e
    {
        if *star || AGG_FNS.contains(&name.as_str()) {
            let idx = sites.len();
            sites.push(AggSite {
                name: name.clone(),
                distinct: *distinct,
                args: args.clone(),
                star: *star,
            });
            return Expr::Param(format!("__agg{idx}"));
        }
    }
    // Structural recursion for the shapes contains_aggregate walks.
    match e {
        Expr::Bin(op, a, b) => Expr::Bin(
            *op,
            Box::new(extract_aggregates(a, sites)),
            Box::new(extract_aggregates(b, sites)),
        ),
        Expr::Call {
            name,
            distinct,
            args,
            star,
        } => Expr::Call {
            name: name.clone(),
            distinct: *distinct,
            args: args.iter().map(|a| extract_aggregates(a, sites)).collect(),
            star: *star,
        },
        Expr::List(items) => {
            Expr::List(items.iter().map(|a| extract_aggregates(a, sites)).collect())
        }
        Expr::Map(entries) => Expr::Map(
            entries
                .iter()
                .map(|(k, v)| (k.clone(), extract_aggregates(v, sites)))
                .collect(),
        ),
        Expr::And(a, b) => Expr::And(
            Box::new(extract_aggregates(a, sites)),
            Box::new(extract_aggregates(b, sites)),
        ),
        Expr::Or(a, b) => Expr::Or(
            Box::new(extract_aggregates(a, sites)),
            Box::new(extract_aggregates(b, sites)),
        ),
        Expr::Xor(a, b) => Expr::Xor(
            Box::new(extract_aggregates(a, sites)),
            Box::new(extract_aggregates(b, sites)),
        ),
        Expr::In(a, b) => Expr::In(
            Box::new(extract_aggregates(a, sites)),
            Box::new(extract_aggregates(b, sites)),
        ),
        Expr::Index(a, b) => Expr::Index(
            Box::new(extract_aggregates(a, sites)),
            Box::new(extract_aggregates(b, sites)),
        ),
        Expr::Not(a) => Expr::Not(Box::new(extract_aggregates(a, sites))),
        Expr::Neg(a) => Expr::Neg(Box::new(extract_aggregates(a, sites))),
        Expr::Prop(a, key) => Expr::Prop(Box::new(extract_aggregates(a, sites)), key.clone()),
        Expr::IsNull { of, negated } => Expr::IsNull {
            of: Box::new(extract_aggregates(of, sites)),
            negated: *negated,
        },
        Expr::Slice { of, from, to } => Expr::Slice {
            of: Box::new(extract_aggregates(of, sites)),
            from: from
                .as_ref()
                .map(|f| Box::new(extract_aggregates(f, sites))),
            to: to.as_ref().map(|t| Box::new(extract_aggregates(t, sites))),
        },
        Expr::Case {
            subject,
            arms,
            otherwise,
        } => Expr::Case {
            subject: subject
                .as_ref()
                .map(|x| Box::new(extract_aggregates(x, sites))),
            arms: arms
                .iter()
                .map(|(w, t)| (extract_aggregates(w, sites), extract_aggregates(t, sites)))
                .collect(),
            otherwise: otherwise
                .as_ref()
                .map(|x| Box::new(extract_aggregates(x, sites))),
        },
        // The comprehension family: the aggregate lives in the SOURCE (the
        // filter/map reference the comprehension variable, which has no
        // meaning at group scope).
        Expr::ListComp {
            var,
            source,
            filter,
            map,
        } => Expr::ListComp {
            var: var.clone(),
            source: Box::new(extract_aggregates(source, sites)),
            filter: filter.clone(),
            map: map.clone(),
        },
        Expr::Reduce {
            acc,
            init,
            var,
            source,
            step,
        } => Expr::Reduce {
            acc: acc.clone(),
            init: Box::new(extract_aggregates(init, sites)),
            var: var.clone(),
            source: Box::new(extract_aggregates(source, sites)),
            step: step.clone(),
        },
        Expr::ListPredicate {
            kind,
            var,
            source,
            filter,
        } => Expr::ListPredicate {
            kind: *kind,
            var: var.clone(),
            source: Box::new(extract_aggregates(source, sites)),
            filter: filter.clone(),
        },
        Expr::HasLabels { of, labels } => Expr::HasLabels {
            of: Box::new(extract_aggregates(of, sites)),
            labels: labels.clone(),
        },
        Expr::MapProjection { of, items } => Expr::MapProjection {
            of: Box::new(extract_aggregates(of, sites)),
            items: items
                .iter()
                .map(|it| match it {
                    engram_cypher::ast::MapProjectionItem::Entry(k, e) => {
                        engram_cypher::ast::MapProjectionItem::Entry(
                            k.clone(),
                            extract_aggregates(e, sites),
                        )
                    }
                    other => other.clone(),
                })
                .collect(),
        },
        other => other.clone(),
    }
}

fn compute_aggregate(
    graph: &Graph,
    name: &str,
    distinct: bool,
    args: &[Expr],
    star: bool,
    group: &[Row],
    params: &BTreeMap<String, Value>,
) -> Result<Value, RunError> {
    if star {
        return Ok(Value::Int(group.len() as i64));
    }
    let arg = args
        .first()
        .ok_or_else(|| RunError::Semantic(format!("{name}() needs an argument")))?;
    let mut values = Vec::with_capacity(group.len());
    for row in group {
        let v = eval_expr(graph, arg, row, params)?;
        if !matches!(v, Value::Null) {
            values.push(v);
        }
    }
    if matches!(name, "percentilecont" | "percentiledisc") {
        let site = AggSite {
            name: name.to_string(),
            distinct,
            args: args.to_vec(),
            star,
        };
        return fold_site(graph, &site, values, params);
    }
    fold_aggregate_values(name, distinct, values)
}

/// Fold evaluated (already null-filtered) aggregate argument values — ONE
/// implementation of the aggregate semantics, shared by the materialising
/// and streaming paths so they cannot drift.
/// The canonical grouping/DISTINCT key — Neo4j's equivalence, measured
/// against the live server 2026-08-21 and pinned in tests:
///
///   - Int and Float that are numerically equal are ONE key
///     (`UNWIND [1, 1.0] … count(*)` → one group of 2);
///   - null is a key like any other (grouping groups nulls; RETURN
///     DISTINCT keeps one null row) — aggregate INPUTS skip nulls before
///     ever reaching a key;
///   - NaN NEVER collapses (two `sqrt(-1)` rows survive DISTINCT) — each
///     NaN takes a nonce so no two are ever the same key;
///   - -0.0 keys as 0 (numeric equality says so);
///   - temporals key FIELD-WISE — measured: same-instant datetimes with
///     different offsets are NOT equal (`=` false, DISTINCT count 2);
///   - nodes and relationships key by id (identity);
///   - no cross-family equivalence (1 and '1' are two keys).
///
/// One encoding, three consumers — the group fast-map, RETURN DISTINCT,
/// and DISTINCT inside aggregates — because the previous three
/// implementations disagreed with each other (strict grouping, strict
/// projection-DISTINCT, eq3 fold-DISTINCT) and two of them with Neo4j.
fn agg_key(v: &Value, nan_nonce: &mut u64, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.push(0),
        Value::Bool(b) => {
            out.push(1);
            out.push(u8::from(*b));
        }
        Value::Int(i) => {
            out.push(2);
            out.extend_from_slice(&i.to_be_bytes());
        }
        Value::Float(f) => {
            if f.is_nan() {
                out.push(3);
                out.extend_from_slice(&nan_nonce.to_be_bytes());
                *nan_nonce += 1;
            } else if f.fract() == 0.0 && *f >= -(2f64.powi(63)) && *f < 2f64.powi(63) {
                // A whole float in i64 range IS its integer — the
                // Int(1)/Float(1.0) unification, and -0.0 lands on 0.
                out.push(2);
                out.extend_from_slice(&(*f as i64).to_be_bytes());
            } else {
                out.push(4);
                out.extend_from_slice(&f.to_bits().to_be_bytes());
            }
        }
        Value::Str(x) => {
            out.push(5);
            out.extend_from_slice(&(x.len() as u64).to_be_bytes());
            out.extend_from_slice(x.as_bytes());
        }
        Value::List(items) => {
            out.push(6);
            out.extend_from_slice(&(items.len() as u64).to_be_bytes());
            for it in items {
                agg_key(it, nan_nonce, out);
            }
        }
        // A PATH keys like its trail but under a DISTINCT tag, so a path never
        // collides with a plain list of the same node/rel values (they are
        // different types and must group separately).
        Value::Path(items) => {
            out.push(16);
            out.extend_from_slice(&(items.len() as u64).to_be_bytes());
            for it in items {
                agg_key(it, nan_nonce, out);
            }
        }
        Value::Map(m) => {
            out.push(7);
            out.extend_from_slice(&(m.len() as u64).to_be_bytes());
            for (k, val) in m {
                out.extend_from_slice(&(k.len() as u64).to_be_bytes());
                out.extend_from_slice(k.as_bytes());
                agg_key(val, nan_nonce, out);
            }
        }
        Value::Node { id, .. } => {
            out.push(8);
            out.extend_from_slice(&id.to_be_bytes());
        }
        Value::Rel { id, .. } => {
            out.push(9);
            out.extend_from_slice(&id.to_be_bytes());
        }
        Value::Date(d) => {
            out.push(10);
            out.extend_from_slice(&d.to_be_bytes());
        }
        Value::Time {
            nanos,
            offset_seconds,
        } => {
            out.push(11);
            out.extend_from_slice(&nanos.to_be_bytes());
            out.extend_from_slice(&offset_seconds.to_be_bytes());
        }
        Value::LocalTime(n) => {
            out.push(12);
            out.extend_from_slice(&n.to_be_bytes());
        }
        Value::DateTime {
            epoch_seconds,
            nanos,
            offset_seconds,
            zone,
        } => {
            out.push(13);
            out.extend_from_slice(&epoch_seconds.to_be_bytes());
            out.extend_from_slice(&nanos.to_be_bytes());
            out.extend_from_slice(&offset_seconds.to_be_bytes());
            match zone {
                Some(z) => {
                    out.push(1);
                    out.extend_from_slice(&(z.len() as u64).to_be_bytes());
                    out.extend_from_slice(z.as_bytes());
                }
                None => out.push(0),
            }
        }
        Value::LocalDateTime {
            epoch_seconds,
            nanos,
        } => {
            out.push(14);
            out.extend_from_slice(&epoch_seconds.to_be_bytes());
            out.extend_from_slice(&nanos.to_be_bytes());
        }
        Value::Duration {
            months,
            days,
            seconds,
            nanos,
        } => {
            out.push(15);
            out.extend_from_slice(&months.to_be_bytes());
            out.extend_from_slice(&days.to_be_bytes());
            out.extend_from_slice(&seconds.to_be_bytes());
            out.extend_from_slice(&nanos.to_be_bytes());
        }
    }
}

/// The key for a whole tuple of values (a group key, a DISTINCT row).
pub(crate) fn agg_key_of(values: &[Value], nan_nonce: &mut u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 12);
    for v in values {
        agg_key(v, nan_nonce, &mut out);
    }
    out
}

fn fold_aggregate_values(
    name: &str,
    distinct: bool,
    mut values: Vec<Value>,
) -> Result<Value, RunError> {
    if distinct {
        let mut nonce = 0u64;
        let mut seen = std::collections::BTreeSet::new();
        values.retain(|v| seen.insert(agg_key_of(std::slice::from_ref(v), &mut nonce)));
    }
    Ok(match name {
        "count" => Value::Int(values.len() as i64),
        "collect" => Value::List(values),
        "sum" => {
            let mut int_sum: i64 = 0;
            let mut float_sum = 0.0;
            let mut any_float = false;
            for v in &values {
                match v {
                    Value::Int(i) => {
                        int_sum = int_sum
                            .checked_add(*i)
                            .ok_or(RunError::Eval(EvalError::Overflow("sum")))?;
                    }
                    Value::Float(f) => {
                        any_float = true;
                        float_sum += f;
                    }
                    other => {
                        return Err(RunError::Semantic(format!(
                            "sum() over a {}",
                            other.type_name()
                        )));
                    }
                }
            }
            if any_float {
                Value::Float(float_sum + int_sum as f64)
            } else {
                Value::Int(int_sum)
            }
        }
        "avg" => {
            if values.is_empty() {
                Value::Null
            } else {
                let mut total = 0.0;
                for v in &values {
                    match v {
                        Value::Int(i) => total += *i as f64,
                        Value::Float(f) => total += f,
                        other => {
                            return Err(RunError::Semantic(format!(
                                "avg() over a {}",
                                other.type_name()
                            )));
                        }
                    }
                }
                Value::Float(total / values.len() as f64)
            }
        }
        "min" | "max" => {
            let mut best: Option<&Value> = None;
            for v in &values {
                best = Some(match best {
                    None => v,
                    Some(b) => {
                        let want = if name == "min" {
                            std::cmp::Ordering::Less
                        } else {
                            std::cmp::Ordering::Greater
                        };
                        if minmax_cmp(v, b) == want { v } else { b }
                    }
                });
            }
            best.cloned().unwrap_or(Value::Null)
        }
        "stdev" | "stdevp" => {
            // Sample (`stdev`, n-1) vs population (`stdevp`, n) standard
            // deviation. Neo4j returns 0.0 for fewer than two values.
            let mut nums = Vec::new();
            for v in &values {
                match v {
                    Value::Int(i) => nums.push(*i as f64),
                    Value::Float(f) => nums.push(*f),
                    Value::Null => {}
                    other => {
                        return Err(RunError::Semantic(format!(
                            "{name}() over a {}",
                            other.type_name()
                        )));
                    }
                }
            }
            let n = nums.len();
            if n < 2 {
                Value::Float(0.0)
            } else {
                let mean = nums.iter().sum::<f64>() / n as f64;
                let ss: f64 = nums
                    .iter()
                    .map(|x| {
                        let d = x - mean;
                        d * d
                    })
                    .sum();
                let denom = if name == "stdev" {
                    (n - 1) as f64
                } else {
                    n as f64
                };
                Value::Float((ss / denom).sqrt())
            }
        }
        other => return Err(RunError::Unsupported(format!("aggregate `{other}`"))),
    })
}

/// `percentileDisc` (a stored value) / `percentileCont` (interpolated) over
/// numeric `values` at fraction `frac`. Neo4j rejects a fraction outside
/// `[0, 1]` (NumberOutOfRange) and returns null over no rows.
fn percentile_value(mut values: Vec<Value>, frac: f64, disc: bool) -> Result<Value, RunError> {
    if !(0.0..=1.0).contains(&frac) {
        return Err(RunError::Semantic(
            "percentile fraction is out of range [0, 1] (NumberOutOfRange)".into(),
        ));
    }
    for v in &values {
        if !matches!(v, Value::Int(_) | Value::Float(_)) {
            return Err(RunError::Semantic(format!(
                "percentile over a {}",
                v.type_name()
            )));
        }
    }
    if values.is_empty() {
        return Ok(Value::Null);
    }
    let key = |v: &Value| match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        _ => 0.0,
    };
    values.sort_by(|a, b| {
        key(a)
            .partial_cmp(&key(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let n = values.len();
    if disc {
        let idx = ((frac * n as f64) as usize).min(n - 1);
        Ok(values[idx].clone())
    } else {
        let rank = frac * (n - 1) as f64;
        let lo = rank.floor() as usize;
        let hi = rank.ceil() as usize;
        let vlo = key(&values[lo]);
        let vhi = key(&values[hi]);
        Ok(Value::Float(vlo + (rank - lo as f64) * (vhi - vlo)))
    }
}

/// Finalize a buffered aggregate SITE. `percentileDisc`/`percentileCont` take a
/// second (fraction) argument, so they need the site + a scope to read it;
/// everything else folds through `fold_aggregate_values`.
fn fold_site(
    graph: &Graph,
    site: &AggSite,
    mut vals: Vec<Value>,
    params: &BTreeMap<String, Value>,
) -> Result<Value, RunError> {
    if matches!(site.name.as_str(), "percentilecont" | "percentiledisc") {
        if site.distinct {
            let mut nonce = 0u64;
            let mut seen = std::collections::BTreeSet::new();
            vals.retain(|v| seen.insert(agg_key_of(std::slice::from_ref(v), &mut nonce)));
        }
        let frac_expr = site
            .args
            .get(1)
            .ok_or_else(|| RunError::Semantic("percentile requires a fraction argument".into()))?;
        let frac = match eval_expr(graph, frac_expr, &Row::new(), params)? {
            Value::Int(i) => i as f64,
            Value::Float(f) => f,
            other => {
                return Err(RunError::Semantic(format!(
                    "percentile fraction must be a number, got {}",
                    other.type_name()
                )));
            }
        };
        return percentile_value(vals, frac, site.name == "percentiledisc");
    }
    fold_aggregate_values(&site.name, site.distinct, vals)
}

// ─── Procedures — the census's two, implemented; everything else refuses ────

fn call_procedure(
    graph: &Graph,
    name: &str,
    args: &[Expr],
    yields: &[(String, Option<String>)],
    where_: Option<&Expr>,
    rows: Vec<Row>,
    params: &BTreeMap<String, Value>,
) -> Result<Vec<Row>, RunError> {
    // ── Introspection procedures (R-5): what a driver or tool asks on
    // connect. Each yields catalog rows per INPUT row, exactly as the data
    // procedures do, so `CALL db.labels() YIELD label WHERE … RETURN …`
    // composes the same way.
    if let Some(rows_out) = introspect_procedure(graph, name, args, yields, where_, &rows, params)?
    {
        return Ok(rows_out);
    }
    if name == "engram.checkpoint" {
        return checkpoint_procedure(graph, args, yields, where_, rows, params);
    }
    let is_vector = name == "db.index.vector.querynodes";
    let is_fulltext = name == "db.index.fulltext.querynodes";
    if !is_vector && !is_fulltext {
        sometimes!("interp.refused an unsupported construct", true);
        return Err(RunError::Unsupported(format!("procedure `{name}`")));
    }
    // Both yield (node, score).
    let bindings: Vec<(String, String)> = if yields.is_empty() {
        vec![
            ("node".into(), "node".into()),
            ("score".into(), "score".into()),
        ]
    } else {
        let mut out = Vec::new();
        for (field, alias) in yields {
            if field != "node" && field != "score" {
                return Err(RunError::Semantic(format!(
                    "`{name}` does not yield `{field}` (node, score)"
                )));
            }
            out.push((
                field.clone(),
                alias.clone().unwrap_or_else(|| field.clone()),
            ));
        }
        out
    };
    let mut out = Vec::new();
    for row in rows {
        let hits = if is_vector {
            if args.len() != 3 {
                return Err(RunError::Semantic(
                    "db.index.vector.queryNodes(name, k, query) takes 3 arguments".into(),
                ));
            }
            let Value::Str(index) = eval_expr(graph, &args[0], &row, params)? else {
                return Err(RunError::Semantic("the index name must be a string".into()));
            };
            let Value::Int(k) = eval_expr(graph, &args[1], &row, params)? else {
                return Err(RunError::Semantic("k must be an integer".into()));
            };
            let q = match eval_expr(graph, &args[2], &row, params)? {
                Value::List(items) => {
                    let mut v = Vec::with_capacity(items.len());
                    for i in items {
                        match i {
                            Value::Float(f) => v.push(f),
                            Value::Int(n) => v.push(n as f64),
                            other => {
                                return Err(RunError::Semantic(format!(
                                    "the query vector must be numeric, got {}",
                                    other.type_name()
                                )));
                            }
                        }
                    }
                    v
                }
                other => {
                    return Err(RunError::Semantic(format!(
                        "the query vector must be a list, got {}",
                        other.type_name()
                    )));
                }
            };
            graph.vector_query(&index, k.max(0) as usize, &q)?.0
        } else {
            if args.len() != 2 {
                return Err(RunError::Semantic(
                    "db.index.fulltext.queryNodes(name, query) takes 2 arguments".into(),
                ));
            }
            let Value::Str(index) = eval_expr(graph, &args[0], &row, params)? else {
                return Err(RunError::Semantic("the index name must be a string".into()));
            };
            let Value::Str(q) = eval_expr(graph, &args[1], &row, params)? else {
                return Err(RunError::Semantic("the query must be a string".into()));
            };
            graph.fulltext_query(&index, &q)?
        };
        for (node, score) in hits {
            let mut r = row.clone();
            for (field, alias) in &bindings {
                let v = if field == "node" {
                    node.clone()
                } else {
                    Value::Float(score)
                };
                r.insert(alias.clone(), v);
            }
            if let Some(w) = where_ {
                let v = eval_expr(graph, w, &r, params)?;
                if v.truth() != Some(Truth::True) {
                    continue;
                }
            }
            out.push(r);
        }
    }
    Ok(out)
}

/// `CALL engram.checkpoint() YIELD spilled, segments, resident, tail` — make
/// the paged store durable NOW and say what is on disk. Runs the server's
/// hook (see [`Graph::set_checkpoint_hook`]); refused where none is
/// installed, because "durable" is not something to answer on a store whose
/// durability lives elsewhere. Yields one row per INPUT row, as the other
/// procedures do, so `CALL engram.checkpoint() YIELD tail RETURN tail`
/// composes the same way.
fn checkpoint_procedure(
    graph: &Graph,
    args: &[Expr],
    yields: &[(String, Option<String>)],
    where_: Option<&Expr>,
    rows: Vec<Row>,
    params: &BTreeMap<String, Value>,
) -> Result<Vec<Row>, RunError> {
    if !args.is_empty() {
        return Err(RunError::Semantic("`engram.checkpoint` takes no arguments".into()));
    }
    let Some(hook) = graph.checkpoint_hook() else {
        sometimes!("interp.checkpoint refused: no paged store behind this graph", true);
        return Err(RunError::Semantic(
            "engram.checkpoint: this graph is not served from a paged store; nothing here \
             decides durability"
                .into(),
        ));
    };
    const FIELDS: [&str; 4] = ["spilled", "segments", "resident", "tail"];
    let bindings: Vec<(String, String)> = if yields.is_empty() {
        FIELDS.iter().map(|f| (f.to_string(), f.to_string())).collect()
    } else {
        let mut out = Vec::new();
        for (field, alias) in yields {
            if !FIELDS.contains(&field.as_str()) {
                return Err(RunError::Semantic(format!(
                    "`engram.checkpoint` does not yield `{field}` ({})",
                    FIELDS.join(", ")
                )));
            }
            out.push((field.clone(), alias.clone().unwrap_or_else(|| field.clone())));
        }
        out
    };
    let report = hook().map_err(|e| RunError::Semantic(format!("engram.checkpoint: {e}")))?;
    counted!("interp.checkpoint ran");
    let values = |f: &str| -> Value {
        Value::Int(match f {
            "spilled" => report.spilled as i64,
            "segments" => report.segments as i64,
            "resident" => report.resident as i64,
            _ => report.tail as i64,
        })
    };
    let mut out = Vec::new();
    for row in rows {
        let mut r = row.clone();
        for (field, alias) in &bindings {
            r.insert(alias.clone(), values(field));
        }
        if let Some(w) = where_ {
            let v = eval_expr(graph, w, &r, params)?;
            if v.truth() != Some(Truth::True) {
                continue;
            }
        }
        out.push(r);
    }
    Ok(out)
}

/// The introspection procedures a driver or tool calls on connect —
/// `db.labels`, `db.relationshipTypes`, `dbms.components` — answered from the
/// maintained stats and the crate version, no scans. Returns `None` for any
/// other name so `call_procedure` continues to its data procedures and its
/// refusal. Names arrive LOWERCASED (the parser's rule for callables); yield
/// FIELD names are identifiers and keep their case, so the Neo4j spellings
/// (`relationshipType`) are matched exactly.
fn introspect_procedure(
    graph: &Graph,
    name: &str,
    args: &[Expr],
    yields: &[(String, Option<String>)],
    where_: Option<&Expr>,
    rows: &[Row],
    params: &BTreeMap<String, Value>,
) -> Result<Option<Vec<Row>>, RunError> {
    // (default yield field, the catalog rows as (field value sets)).
    let catalog: Vec<Vec<(&str, Value)>> = match name {
        "db.labels" => graph
            .label_histogram()
            .map_err(RunError::Graph)?
            .into_iter()
            .map(|(l, _)| vec![("label", Value::Str(l))])
            .collect(),
        "db.relationshiptypes" => graph
            .rel_type_histogram()
            .map_err(RunError::Graph)?
            .into_iter()
            .map(|(t, _)| vec![("relationshipType", Value::Str(t))])
            .collect(),
        "db.propertykeys" => graph
            .property_key_names()
            .map_err(RunError::Graph)?
            .into_iter()
            .map(|k| vec![("propertyKey", Value::Str(k))])
            .collect(),
        "dbms.components" => vec![vec![
            ("name", Value::Str("Engram".into())),
            (
                "versions",
                Value::List(vec![Value::Str(env!("CARGO_PKG_VERSION").into())]),
            ),
            ("edition", Value::Str("engram".into())),
        ]],
        _ => return Ok(None),
    };
    if !args.is_empty() {
        return Err(RunError::Semantic(format!("`{name}` takes no arguments")));
    }
    let fields: Vec<&str> = catalog.first().map_or_else(
        || match name {
            "db.labels" => vec!["label"],
            "db.relationshiptypes" => vec!["relationshipType"],
            "db.propertykeys" => vec!["propertyKey"],
            _ => vec!["name", "versions", "edition"],
        },
        |row| row.iter().map(|(f, _)| *f).collect(),
    );
    let bindings: Vec<(String, String)> = if yields.is_empty() {
        fields.iter().map(|f| (f.to_string(), f.to_string())).collect()
    } else {
        let mut out = Vec::new();
        for (field, alias) in yields {
            if !fields.contains(&field.as_str()) {
                return Err(RunError::Semantic(format!(
                    "`{name}` does not yield `{field}` ({})",
                    fields.join(", ")
                )));
            }
            out.push((
                field.clone(),
                alias.clone().unwrap_or_else(|| field.clone()),
            ));
        }
        out
    };
    let mut out = Vec::new();
    for row in rows {
        for entry in &catalog {
            let mut r = row.clone();
            for (field, alias) in &bindings {
                let v = entry
                    .iter()
                    .find(|(f, _)| f == field)
                    .map(|(_, v)| v.clone())
                    .expect("yield fields validated against the catalog");
                r.insert(alias.clone(), v);
            }
            if let Some(w) = where_ {
                let v = eval_expr(graph, w, &r, params)?;
                if v.truth() != Some(Truth::True) {
                    continue;
                }
            }
            out.push(r);
        }
    }
    Ok(Some(out))
}
