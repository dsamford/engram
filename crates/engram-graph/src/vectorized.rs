//! Layer-4 columnar rewrite, increment 2. Handles
//! `MATCH (a:A)-[:T]->(b:B) WHERE <predicate over b> RETURN count(*)` (and
//! `count(b)`) with a VECTORISED filter: the predicate is evaluated ONCE per
//! distinct `b` over its property COLUMNS (`eval_column`), and the hot loop
//! over `(a,b)` pairs does ONLY a membership check — never a per-tuple
//! `bind_random` + `eval_with`.
//!
//! rev-94 proved the single-comparison case at ~22-35× over the per-tuple path;
//! this generalises the WHERE to an arbitrary predicate over the end variable
//! `b` — compound comparisons, three-valued AND/OR/XOR/NOT, IS NULL — via
//! `eval_column`, the reusable column-at-a-time evaluator that is the seed of
//! the general DataChunk pipeline. Non-const arithmetic and every non-`b`
//! reference DECLINE to the per-tuple path (sound: it answers identically).

use std::collections::{BTreeMap, BTreeSet};

use engram_cypher::ast::{BinOp, Expr};
use engram_cypher::eval::{Scope, apply_scalar_fn, eval_with, is_aggregate_fn};
use engram_cypher::stmt::{Clause, RelDir, SingleQuery};
use engram_cypher::value::{Truth, Value};

use crate::interp::column_name;
use crate::{ColumnFamily, Dir, Graph, QueryResult, RunError};

struct Plan {
    a_labels: Vec<String>,
    dir: Dir,
    types: Vec<String>,
    b_labels: Vec<String>,
    b_var: String,
    where_: Expr,
    props: BTreeSet<String>,
    alias: String,
}

/// The constant-list length from which `needle IN <list>` hashes the list
/// once instead of scanning it per needle (a shorter list scans as fast).
const HASHED_LIST_MIN: usize = 8;

/// Whether a comparison operator (its result is a Bool/Null truth value).
fn is_cmp(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Eq | BinOp::Neq
    )
}

/// A whitelist of expressions that fold to a constant with no bound variable —
/// literals, a parameter (resolved from `params`), and arithmetic/negation over
/// them. Anything else (a `Var`, a property, a call) is NOT constant.
fn is_const(e: &Expr) -> bool {
    match e {
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Param(_) => true,
        Expr::Neg(x) => is_const(x),
        Expr::Bin(_, a, b) => is_const(a) && is_const(b),
        Expr::List(xs) => xs.iter().all(is_const),
        _ => false,
    }
}

/// `count(*)` or `count(v)` for exactly `var`, non-DISTINCT.
fn is_count_of(e: &Expr, var: &str) -> bool {
    match e {
        Expr::Call {
            name,
            distinct: false,
            args,
            star,
        } if name == "count" => {
            (*star && args.is_empty()) || matches!(args.as_slice(), [Expr::Var(v)] if v == var)
        }
        _ => false,
    }
}

/// Walk `e`, collecting the `b_var` properties it reads and returning whether
/// EVERY leaf is something `eval_column` can vectorise over `b` alone. Rejects
/// (→ decline) a bare variable, a property of any other variable, non-const
/// arithmetic, and calls/case/subqueries. Kept in lockstep with `eval_column`
/// so the recogniser never loads columns for a predicate that then declines.
fn where_props(e: &Expr, b_var: &str, out: &mut BTreeSet<String>) -> bool {
    if is_const(e) {
        return true;
    }
    match e {
        Expr::Prop(base, key) => match base.as_ref() {
            Expr::Var(v) if v == b_var => {
                out.insert(key.clone());
                true
            }
            _ => false,
        },
        Expr::Bin(op, l, r) if is_cmp(*op) => {
            where_props(l, b_var, out) && where_props(r, b_var, out)
        }
        Expr::And(l, r) | Expr::Or(l, r) | Expr::Xor(l, r) => {
            where_props(l, b_var, out) && where_props(r, b_var, out)
        }
        Expr::Not(x) => where_props(x, b_var, out),
        Expr::IsNull { of, .. } => where_props(of, b_var, out),
        // `<needle> IN <const list>`: vectorisable over `b` iff the needle is
        // (and the rhs is a constant list — kept in lockstep with `eval_column`).
        // The columns to load are the needle's; the rhs reads none. A non-const
        // rhs falls through to `false` (decline), exactly as `eval_column` does.
        Expr::In(l, r) if is_const(r) => where_props(l, b_var, out),
        _ => false,
    }
}

fn recognise(sq: &SingleQuery) -> Option<Plan> {
    let (pattern, where_, proj) = match sq.clauses.as_slice() {
        [
            Clause::Match {
                optional: false,
                pattern,
                where_: Some(w),
            },
            Clause::Return { proj },
        ] => (pattern, w, proj),
        _ => return None,
    };
    // A projection that is exactly one count, no ordering/paging/DISTINCT.
    let bad_proj = proj.distinct
        || proj.star
        || !proj.order.is_empty()
        || proj.skip.is_some()
        || proj.limit.is_some()
        || proj.items.len() != 1;
    if bad_proj {
        return None;
    }
    // One path, one non-variable-length directed hop, no path/rel variable,
    // no rel props, an unpropertied start with labels, a named unpropertied end.
    if pattern.paths.len() != 1 {
        return None;
    }
    let path = &pattern.paths[0];
    if path.var.is_some() || path.hops.len() != 1 || path.start.props.is_some() {
        return None;
    }
    if path.start.labels.is_empty() {
        return None;
    }
    let (rel, b) = &path.hops[0];
    if rel.var.is_some() || rel.props.is_some() || rel.length.is_some() || rel.types.is_empty() {
        return None;
    }
    let dir = match rel.dir {
        RelDir::Out => Dir::Out,
        RelDir::In => Dir::In,
        RelDir::Undirected => return None,
    };
    let b_var = b.var.as_deref()?.to_string();
    if b.props.is_some() {
        return None;
    }
    // The count must be over `*` or the end variable.
    if !is_count_of(&proj.items[0].expr, &b_var) {
        return None;
    }
    // The WHERE must be a predicate over `b` (and constants) that `eval_column`
    // handles; `where_props` both validates that and collects the columns to load.
    let mut props = BTreeSet::new();
    if !where_props(where_, &b_var, &mut props) {
        return None;
    }
    let alias = proj.items[0]
        .alias
        .clone()
        .or_else(|| proj.items[0].text.clone())
        .unwrap_or_else(|| column_name(&proj.items[0].expr, 0));
    Some(Plan {
        a_labels: path.start.labels.clone(),
        dir,
        types: rel.types.clone(),
        b_labels: b.labels.clone(),
        b_var,
        where_: where_.clone(),
        props,
        alias,
    })
}

/// One comparison, element-wise — EXACTLY `eval::bin`'s comparison arms
/// (`l.eq3(r)`/`l.lt3(r)` → `to_value`), three-valued (Null → Null).
fn compare(op: BinOp, l: &Value, r: &Value) -> Value {
    match op {
        BinOp::Eq => l.eq3(r).to_value(),
        BinOp::Neq => (!l.eq3(r)).to_value(),
        BinOp::Lt => l.lt3(r).to_value(),
        BinOp::Ge => (!l.lt3(r)).to_value(),
        BinOp::Gt => r.lt3(l).to_value(),
        BinOp::Le => (!r.lt3(l)).to_value(),
        _ => Value::Null, // unreachable: only comparison ops reach here
    }
}

/// Combine two boolean/null columns three-valued, EXACTLY as `eval_with`'s
/// AND/OR/XOR arms do (`truth_of(l).and(truth_of(r)).to_value()`). Returns
/// `None` (decline the whole operator) if any element is a non-boolean, which
/// the per-tuple path would raise as a type error.
fn combine3(l: &[Value], r: &[Value], f: fn(Truth, Truth) -> Truth) -> Option<Vec<Value>> {
    let mut out = Vec::with_capacity(l.len());
    for i in 0..l.len() {
        let lt = l[i].truth()?;
        let rt = r[i].truth()?;
        out.push(f(lt, rt).to_value());
    }
    Some(out)
}

/// Evaluate `e` over `n` positions, COLUMN-AT-A-TIME, using the pre-aligned
/// property columns in `cols` (position i is `distinct[i]`'s value). Every
/// sub-expression is evaluated over the whole column — NO per-position
/// `eval_with`/`bind_random`. `None` = a form this cannot vectorise (decline).
/// A column `eval_column` answers: a REFERENCE to an input column when the
/// expression is that column (`n.prop`, a `__col_` local, the identity), an
/// owned vector when it computed one. A column reference used to be
/// `cols.get(..).cloned()` — every value of a 44k-row column cloned per
/// reference, and a column of lists (`$a IN coalesce(g.affectedCountries,
/// [])` over 44k events) cloned three times over: the aligned copy, the
/// reference, the `coalesce`. Derefs to the slice, so every consumer indexes
/// it as before; `into_owned` is the one place a copy is still made, and
/// only for a reference.
pub(crate) enum Col<'a> {
    Borrowed(&'a [Value]),
    Owned(Vec<Value>),
}

impl std::ops::Deref for Col<'_> {
    type Target = [Value];
    fn deref(&self) -> &[Value] {
        match self {
            Col::Borrowed(s) => s,
            Col::Owned(v) => v,
        }
    }
}

impl Col<'_> {
    /// The column as an owned vector — a copy only when it was a reference.
    pub(crate) fn into_owned(self) -> Vec<Value> {
        match self {
            Col::Borrowed(s) => s.to_vec(),
            Col::Owned(v) => v,
        }
    }
}

/// The input columns of `eval_column`, by name, as SLICES — built over
/// whatever the caller holds (a `Vec`, an `Arc<Vec>`) without copying a
/// value.
pub(crate) type ColView<'a> = BTreeMap<String, &'a [Value]>;

/// A [`ColView`] over owned columns.
pub(crate) fn view(cols: &BTreeMap<String, Vec<Value>>) -> ColView<'_> {
    cols.iter().map(|(k, v)| (k.clone(), v.as_slice())).collect()
}

pub(crate) fn eval_column<'a>(
    e: &Expr,
    b_var: &str,
    n: usize,
    cols: &'a ColView<'a>,
    scope: &Scope<'_>,
) -> Option<Col<'a>> {
    // A constant sub-expression (no bound variable): evaluate ONCE, broadcast.
    if is_const(e) {
        let v = eval_with(e, scope, None).ok()?;
        return Some(Col::Owned(vec![v; n]));
    }
    match e {
        Expr::Prop(base, key) => match base.as_ref() {
            Expr::Var(v) if v == b_var => cols.get(key).map(|s| Col::Borrowed(s)),
            _ => None,
        },
        // A column LOCAL of the batch rewrite (`__col_<prop>` — what
        // `batch::rewrite` turns `n.prop` into): its column is keyed by the
        // local's own name. The prefix is the rewrite's, so no pattern
        // variable can collide with it.
        Expr::Var(v) if v.starts_with("__col_") => cols.get(v).map(|s| Col::Borrowed(s)),
        // A bare NODE variable read for its IDENTITY — `country` in `country =
        // countryX` / `country IN [countryX, countryY]` / a CASE over `country =
        // countryX`. Its id-only node column is loaded under NODE_IDENTITY_KEY
        // (NODE vars only; a rel var lacks it → None → the pred declines). `eq3`
        // compares graph entities by id, so this matches a fully-materialised node
        // of the same id.
        Expr::Var(v) if v == b_var => cols
            .get(crate::pipeline::NODE_IDENTITY_KEY)
            .map(|s| Col::Borrowed(s)),
        // A CONSTANT side is evaluated once and compared as a scalar, not
        // broadcast into a column of `n` clones: `s.primaryTopic = $t` over
        // a 21k-row label cloned the parameter string 21k times per
        // statement — half the cost of a one-conjunct count (fix 40).
        Expr::Bin(op, l, r) if is_cmp(*op) && is_const(r) => {
            let lc = eval_column(l, b_var, n, cols, scope)?;
            let rv = eval_with(r, scope, None).ok()?;
            Some(Col::Owned(lc.iter().map(|v| compare(*op, v, &rv)).collect()))
        }
        Expr::Bin(op, l, r) if is_cmp(*op) && is_const(l) => {
            let lv = eval_with(l, scope, None).ok()?;
            let rc = eval_column(r, b_var, n, cols, scope)?;
            Some(Col::Owned(rc.iter().map(|v| compare(*op, &lv, v)).collect()))
        }
        Expr::Bin(op, l, r) if is_cmp(*op) => {
            let lc = eval_column(l, b_var, n, cols, scope)?;
            let rc = eval_column(r, b_var, n, cols, scope)?;
            Some(Col::Owned(
                (0..n).map(|i| compare(*op, &lc[i], &rc[i])).collect(),
            ))
        }
        // The string operators, element by element with `eval::str_op`'s
        // rule: two strings answer the test, anything else (a null, a
        // non-string) is Null. `g.eventId STARTS WITH 'edgar-8k-'` over 44k
        // events walked the label per member for want of this arm.
        Expr::Bin(op @ (BinOp::StartsWith | BinOp::EndsWith | BinOp::Contains), l, r) => {
            let test: fn(&str, &str) -> bool = match op {
                BinOp::StartsWith => |a, b| a.starts_with(b),
                BinOp::EndsWith => |a, b| a.ends_with(b),
                _ => |a, b| a.contains(b),
            };
            let lc = eval_column(l, b_var, n, cols, scope)?;
            // A constant needle is evaluated once (see the comparison arms).
            if is_const(r) {
                let rv = eval_with(r, scope, None).ok()?;
                return Some(Col::Owned(
                    lc.iter()
                        .map(|v| match (v, &rv) {
                            (Value::Str(a), Value::Str(b)) => Value::Bool(test(a, b)),
                            _ => Value::Null,
                        })
                        .collect(),
                ));
            }
            let rc = eval_column(r, b_var, n, cols, scope)?;
            Some(Col::Owned(
                (0..n)
                    .map(|i| match (&lc[i], &rc[i]) {
                        (Value::Str(a), Value::Str(b)) => Value::Bool(test(a, b)),
                        _ => Value::Null,
                    })
                    .collect(),
            ))
        }
        Expr::And(l, r) => {
            let lc = eval_column(l, b_var, n, cols, scope)?;
            let rc = eval_column(r, b_var, n, cols, scope)?;
            combine3(&lc, &rc, Truth::and).map(Col::Owned)
        }
        Expr::Or(l, r) => {
            let lc = eval_column(l, b_var, n, cols, scope)?;
            let rc = eval_column(r, b_var, n, cols, scope)?;
            combine3(&lc, &rc, Truth::or).map(Col::Owned)
        }
        Expr::Xor(l, r) => {
            let lc = eval_column(l, b_var, n, cols, scope)?;
            let rc = eval_column(r, b_var, n, cols, scope)?;
            combine3(&lc, &rc, Truth::xor).map(Col::Owned)
        }
        Expr::Not(x) => {
            let xc = eval_column(x, b_var, n, cols, scope)?;
            let mut out = Vec::with_capacity(n);
            for v in xc.iter() {
                out.push((!v.truth()?).to_value());
            }
            Some(Col::Owned(out))
        }
        Expr::IsNull { of, negated } => {
            let oc = eval_column(of, b_var, n, cols, scope)?;
            Some(Col::Owned(
                oc.iter()
                    .map(|v| Value::Bool(matches!(v, Value::Null) != *negated))
                    .collect(),
            ))
        }
        // `<needle> IN <const list>` — EXACTLY `eval::In`'s three-valued
        // membership, per column element. The needle is `eval_column`-able over
        // the SAME single var (recurse — declines with it). The rhs must be
        // CONSTANT (no bound-var reference): fold it ONCE (the sole eval on a
        // constant, as every other arm folds constants) and broadcast the folded
        // list across the needle column. A non-const rhs (e.g. `friend IN
        // friends` from a collect) DECLINES; a folded rhs that is neither a list
        // nor null DECLINES too (the general path raises the same type error).
        Expr::In(lhs, rhs) => {
            let needle = eval_column(lhs, b_var, n, cols, scope)?;
            // `needle IN coalesce(<column of lists>, <const>)` — the production
            // country-pair scans' `$a IN coalesce(g.affectedCountries, [])` —
            // tests each needle against its OWN list, BORROWED: the row's
            // list where it is present, the constant where it is null. Exactly
            // `coalesce` then `IN`, per element, without the copy of every
            // list the generic scalar-call arm makes to hand `coalesce` owned
            // arguments (44k lists per statement on the mirror).
            if let Expr::Call {
                name,
                args,
                distinct: false,
                star: false,
            } = rhs.as_ref()
            {
                if name == "coalesce" && args.len() == 2 && !is_const(&args[0]) && is_const(&args[1])
                {
                    let lists = eval_column(&args[0], b_var, n, cols, scope)?;
                    let fallback = eval_with(&args[1], scope, None).ok()?;
                    let mut out = Vec::with_capacity(n);
                    for i in 0..n {
                        let list = if matches!(lists[i], Value::Null) {
                            &fallback
                        } else {
                            &lists[i]
                        };
                        out.push(in_list(&needle[i], list)?);
                    }
                    return Some(Col::Owned(out));
                }
            }
            // A COLUMN of lists on the right — evaluates the rhs over the same
            // positions and tests each needle against its own list, with the
            // same three-valued rule; a non-list, non-null element declines
            // (the general path raises the type error).
            if !is_const(rhs) {
                let lists = eval_column(rhs, b_var, n, cols, scope)?;
                let mut out = Vec::with_capacity(n);
                for i in 0..n {
                    out.push(in_list(&needle[i], &lists[i])?);
                }
                return Some(Col::Owned(out));
            }
            let list = match eval_with(rhs, scope, None).ok()? {
                Value::Null => Value::Null, // a null list ⇒ Null for every needle
                l @ Value::List(_) => l,
                _ => return None, // a non-list constant — the general path errors
            };
            // A constant list of STRINGS is indexed once and each string
            // needle is answered by one lookup — `in_list` compared every
            // needle against every item (`NOT s.storyId IN $existingIds`
            // over 21k stories: 40 string compares a member). A null
            // needle against a non-empty list is Unknown, as `in_list`
            // answers it; anything else keeps the three-valued scan.
            let hashed: Option<BTreeSet<&str>> = match &list {
                Value::List(items) if items.len() >= HASHED_LIST_MIN => items
                    .iter()
                    .map(|it| match it {
                        Value::Str(s) => Some(s.as_str()),
                        _ => None,
                    })
                    .collect(),
                _ => None,
            };
            let mut out = Vec::with_capacity(n);
            for v in needle.iter() {
                out.push(match (&hashed, v) {
                    (Some(set), Value::Str(s)) => Value::Bool(set.contains(s.as_str())),
                    (Some(_), Value::Null) => Value::Null,
                    _ => in_list(v, &list)?,
                });
            }
            Some(Col::Owned(out))
        }
        // A SCALAR function over the SAME single var — `toInteger(x.prop)`,
        // `toFloat(x.prop)`, `coalesce(x.a, x.b)`, … (the `toInteger(personId)`
        // ORDER BY keys in IC11/IC12). Every argument must itself be
        // `eval_column`-able over `b_var` (or const); per element, apply the
        // built-in through the SAME registry `eval_with` uses (`apply_scalar_fn`),
        // so the columnar value equals the per-tuple `eval_expr`'s exactly. An
        // aggregate (`count`/`collect`/…), a `*`, a `DISTINCT`, or a NO-arg fn
        // (`rand()`/`timestamp()` — not a pure column function) DECLINES to the
        // general path, and any per-element evaluation error declines too (the
        // general path then reproduces the identical error).
        Expr::Call {
            name,
            args,
            distinct,
            star,
        } if !*distinct && !*star && !args.is_empty() && !is_aggregate_fn(name) => {
            let arg_cols: Vec<Col<'_>> = args
                .iter()
                .map(|a| eval_column(a, b_var, n, cols, scope))
                .collect::<Option<Vec<_>>>()?;
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                let call_args: Vec<Value> = arg_cols.iter().map(|c| c[i].clone()).collect();
                out.push(
                    apply_scalar_fn(name, call_args, scope.now_ms, scope.zones.as_deref()).ok()?,
                );
            }
            Some(Col::Owned(out))
        }
        // `CASE` over the SAME single var — searched (`CASE WHEN <cond> THEN …`)
        // and simple (`CASE <subj> WHEN <val> THEN …`) forms, reproducing
        // `eval_with`'s `Expr::Case` element by element: the first arm whose WHEN
        // FIRES (searched: `truth == True`; simple: `subject.eq3(when) == True`)
        // yields its THEN, else the ELSE, else null. The subject, every WHEN/THEN
        // and the ELSE must each be `eval_column`-able over `b_var` (recurse —
        // each declines with it). Columns are evaluated EAGERLY, so a WHEN/THEN a
        // later short-circuit would have skipped can only DECLINE the whole CASE
        // (never mis-answer): a non-vectorisable or erroring sub-expr returns
        // `None`, and the general path then reproduces the exact value/error.
        // This is the A2 primitive behind IC4's `sum(CASE WHEN <range> THEN 1 ELSE
        // 0 END)` (the range is a chained `a <= x < b`, already `And(Bin,Bin)`).
        Expr::Case {
            subject,
            arms,
            otherwise,
        } => {
            let subj = match subject {
                Some(s) => Some(eval_column(s, b_var, n, cols, scope)?),
                None => None,
            };
            let mut when_cols = Vec::with_capacity(arms.len());
            let mut then_cols = Vec::with_capacity(arms.len());
            for (when, then) in arms {
                when_cols.push(eval_column(when, b_var, n, cols, scope)?);
                then_cols.push(eval_column(then, b_var, n, cols, scope)?);
            }
            let else_col = match otherwise {
                Some(e) => Some(eval_column(e, b_var, n, cols, scope)?),
                None => None,
            };
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                let mut picked: Option<Value> = None;
                for ai in 0..arms.len() {
                    let fired = match &subj {
                        // Simple form: `subject = when` (three-valued; Unknown does
                        // not fire).
                        Some(s) => s[i].eq3(&when_cols[ai][i]) == Truth::True,
                        // Searched form: the WHEN must be boolean-TRUE. A
                        // non-boolean WHEN is a type error the general path raises —
                        // `truth()` is `None` there, so `?` DECLINES.
                        None => when_cols[ai][i].truth()? == Truth::True,
                    };
                    if fired {
                        picked = Some(then_cols[ai][i].clone());
                        break;
                    }
                }
                out.push(match picked {
                    Some(v) => v,
                    None => match &else_col {
                        Some(c) => c[i].clone(),
                        None => Value::Null,
                    },
                });
            }
            Some(Col::Owned(out))
        }
        // Fix 63: a MAP literal over the SAME single var, element by element —
        // exactly `eval_with`'s `Expr::Map` (every entry kept, a null value
        // included). Every value must itself be `eval_column`-able over
        // `b_var` (or const); one that is not declines the whole map. Kept in
        // lockstep with `key_side`'s `Expr::Map` arm.
        Expr::Map(entries) => {
            let value_cols: Vec<Col<'_>> = entries
                .iter()
                .map(|(_, v)| eval_column(v, b_var, n, cols, scope))
                .collect::<Option<Vec<_>>>()?;
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                let mut m = BTreeMap::new();
                for ((k, _), c) in entries.iter().zip(&value_cols) {
                    m.insert(k.clone(), c[i].clone());
                }
                out.push(Value::Map(m));
            }
            Some(Col::Owned(out))
        }
        // A LIST literal, the same way (`eval_with`'s `Expr::List`).
        Expr::List(items) => {
            let item_cols: Vec<Col<'_>> = items
                .iter()
                .map(|v| eval_column(v, b_var, n, cols, scope))
                .collect::<Option<Vec<_>>>()?;
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                out.push(Value::List(item_cols.iter().map(|c| c[i].clone()).collect()));
            }
            Some(Col::Owned(out))
        }
        _ => None,
    }
}

/// `needle IN list`, openCypher's three-valued membership (eval.rs
/// `Expr::In`): any True ⇒ true; else any Unknown ⇒ null; else false. A
/// null list is Null regardless of the needle; a null needle against a
/// nonempty list is Unknown per element (⇒ Null), against `[]` false. A
/// non-list, non-null `list` DECLINES (`None`) — the general path raises
/// the type error.
fn in_list(needle: &Value, list: &Value) -> Option<Value> {
    match list {
        Value::Null => Some(Value::Null),
        Value::List(items) => {
            let mut saw_unknown = false;
            for item in items {
                match needle.eq3(item) {
                    Truth::True => return Some(Value::Bool(true)),
                    Truth::Unknown => saw_unknown = true,
                    Truth::False => {}
                }
            }
            Some(if saw_unknown {
                Value::Null
            } else {
                Value::Bool(false)
            })
        }
        _ => None,
    }
}

/// Align a property column (sorted `(id, value)` over an id-span that may
/// include non-members) to `distinct` (sorted b ids): position i is
/// `distinct[i]`'s value, or Null if the property is absent. O(|column| + |ids|).
pub(crate) fn align(distinct: &[u64], column: &[(u64, Value)]) -> Vec<Value> {
    let mut out = Vec::with_capacity(distinct.len());
    let mut ci = 0usize;
    for &id in distinct {
        while ci < column.len() && column[ci].0 < id {
            ci += 1;
        }
        if ci < column.len() && column[ci].0 == id {
            out.push(column[ci].1.clone());
        } else {
            out.push(Value::Null);
        }
    }
    out
}

fn zero(alias: &str) -> QueryResult {
    QueryResult {
        columns: vec![alias.to_string()],
        rows: vec![vec![Value::Int(0)]],
    }
}

pub(crate) fn try_vectorized_hop_filter_count(
    graph: &Graph,
    q: &SingleQuery,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    if !graph.columnar_scans_enabled() {
        return Ok(None);
    }
    let Some(plan) = recognise(q) else {
        return Ok(None);
    };

    let a_ids = graph.members_all(&plan.a_labels).map_err(RunError::Graph)?;
    if a_ids.is_empty() {
        return Ok(Some(zero(&plan.alias)));
    }
    let b_members = if plan.b_labels.is_empty() {
        None
    } else {
        Some(graph.members_all(&plan.b_labels).map_err(RunError::Graph)?)
    };
    let tokens = graph.type_tokens_peek(&plan.types);
    if matches!(&tokens, Some(v) if v.is_empty()) {
        return Ok(Some(zero(&plan.alias))); // a named type never minted
    }

    // Drive from A's adjacency (NOT the whole rel type). `pairs` is one entry
    // per matching (a,b) edge — b's multiplicity, which count(*) preserves.
    let mut pairs: Vec<u64> = Vec::new();
    let mut distinct_b: BTreeSet<u64> = BTreeSet::new();
    for a in a_ids.iter() {
        // Zero-copy forward adjacency straight from the cached CSR slice.
        graph.adjacent_slim_for_each(a, plan.dir, &tokens, |adj| {
            let b = adj.peer;
            if let Some(m) = &b_members {
                if !m.contains(b) {
                    return;
                }
            }
            pairs.push(b);
            distinct_b.insert(b);
        });
        crate::interp::budget_check(graph, pairs.len())?;
    }
    if distinct_b.is_empty() {
        return Ok(Some(zero(&plan.alias)));
    }
    let distinct: Vec<u64> = distinct_b.into_iter().collect();
    let (lo, hi) = (distinct[0], distinct[distinct.len() - 1]);

    // Load every referenced property as ONE column over the b id-span, aligned
    // to `distinct`. Declines (→ general path) if any span is too wide.
    let budget = graph.columnar_column_budget(distinct.len());
    let mut cols: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for prop in &plan.props {
        // The range scan over `[lo, hi)` DECLINES (Ok(None)) when the distinct
        // hop-end id set is SPARSE — its node-id span holds more `prop` entries
        // than the budget because other node types in the range carry the same
        // prop (every node type carries `id`). Fall back to a POINT-GATHER of
        // exactly `distinct`'s values: O(members) point reads, never a range
        // scan, byte-identical to what the scan would have produced (same token,
        // same tagged bytes, same decode, same settle) so `align` fills the same
        // column — absents included. Mirrors `load_family_columns`.
        let column = match graph
            .column_entries_bounded_in(ColumnFamily::Nodes, prop, lo, hi.checked_add(1), budget)
            .map_err(RunError::Graph)?
        {
            Some(column) => column,
            None => graph
                .column_entries_gather(ColumnFamily::Nodes, prop, &distinct)
                .map_err(RunError::Graph)?,
        };
        cols.insert(prop.clone(), align(&distinct, &column));
    }

    // THE VECTORISED FILTER: the predicate is evaluated column-at-a-time —
    // O(distinct_b), NOT O(pairs) — into a sorted `pass` set. A non-boolean
    // WHERE result (or an unvectorisable form) declines to the general path.
    let empty = engram_cypher::bindings::VarMap::new();
    let scope = Scope::over(params, &empty, graph.wall_ms(), graph.zone_provider());
    let cview = view(&cols);
    let Some(result) = eval_column(&plan.where_, &plan.b_var, distinct.len(), &cview, &scope) else {
        return Ok(None);
    };
    let mut pass: Vec<u64> = Vec::with_capacity(distinct.len());
    for (i, v) in result.iter().enumerate() {
        match v.truth() {
            Some(Truth::True) => pass.push(distinct[i]), // distinct is sorted
            Some(_) => {}
            None => return Ok(None), // non-boolean WHERE — the general path errors
        }
    }

    // HOT LOOP over pairs: a membership check only — no bind, no eval, no row.
    let mut count: i64 = 0;
    for b in &pairs {
        if pass.binary_search(b).is_ok() {
            count += 1;
        }
    }

    Ok(Some(QueryResult {
        columns: vec![plan.alias],
        rows: vec![vec![Value::Int(count)]],
    }))
}

// ─── increment 3: vectorised hop projection + ORDER BY/LIMIT top-k ───────────
//
// `MATCH (a:A)-[:T]->(b:B) [WHERE <pred over b>] RETURN <items over a,b>
//  ORDER BY <keys over a,b> [SKIP s] LIMIT k` — the analytical (IC9-style)
// shape the per-tuple path loses on. The winners are chosen by a VECTORISED
// WHERE filter plus a bounded top-k over id-PAIRS: no `from.row.clone()` per
// edge, no `mat_node` per edge, no `eval_with` per edge. Only the ≤ skip+limit
// survivors are late-materialised and pushed through the SAME projection +
// ORDER BY/SKIP/LIMIT tail (`project_pairs_tail`) the streaming collector
// uses, so the output is byte-identical to the per-tuple path — ORDER-BY ties
// included, because production order (A ascending × REVERSE adjacency, the LIFO
// emission order of `expand_var_length`) is reproduced exactly and used as the
// stable-sort tiebreak.

/// Which single pattern variable an ORDER BY key reads.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Side {
    Const,
    A,
    B,
}

/// Combine the sides of two sub-expressions: a const takes the other's side;
/// same-side stays that side; a mix of `a` and `b` is rejected — a key
/// spanning both variables is not single-column and declines.
fn merge_side(l: Side, r: Side) -> Option<Side> {
    match (l, r) {
        (Side::Const, s) | (s, Side::Const) => Some(s),
        (Side::A, Side::A) => Some(Side::A),
        (Side::B, Side::B) => Some(Side::B),
        _ => None, // A mixed with B
    }
}

/// Walk an ORDER BY key exactly as `eval_column` will, classifying it as
/// reading only `a`, only `b`, or neither (const), and collecting the
/// properties it reads on each side. `None` = a form `eval_column` cannot
/// vectorise, or a key spanning BOTH variables — decline. Kept in lockstep
/// with `eval_column`'s accepted set.
pub(crate) fn key_side(
    e: &Expr,
    a_var: &str,
    b_var: &str,
    a_props: &mut BTreeSet<String>,
    b_props: &mut BTreeSet<String>,
) -> Option<Side> {
    if is_const(e) {
        return Some(Side::Const);
    }
    match e {
        Expr::Prop(base, key) => match base.as_ref() {
            Expr::Var(v) if v == a_var => {
                a_props.insert(key.clone());
                Some(Side::A)
            }
            Expr::Var(v) if v == b_var => {
                b_props.insert(key.clone());
                Some(Side::B)
            }
            _ => None,
        },
        // A bare variable read for its IDENTITY (a node-identity comparison). It
        // reads no property but needs its id-only node column, requested via
        // NODE_IDENTITY_KEY. Kept in lockstep with `eval_column`'s Var arm; a rel
        // var declines downstream (its identity column is not synthesised).
        Expr::Var(v) if v == a_var => {
            a_props.insert(crate::pipeline::NODE_IDENTITY_KEY.to_string());
            Some(Side::A)
        }
        Expr::Var(v) if v == b_var => {
            b_props.insert(crate::pipeline::NODE_IDENTITY_KEY.to_string());
            Some(Side::B)
        }
        Expr::Bin(op, l, r) if is_cmp(*op) => {
            let ls = key_side(l, a_var, b_var, a_props, b_props)?;
            let rs = key_side(r, a_var, b_var, a_props, b_props)?;
            merge_side(ls, rs)
        }
        Expr::And(l, r) | Expr::Or(l, r) | Expr::Xor(l, r) => {
            let ls = key_side(l, a_var, b_var, a_props, b_props)?;
            let rs = key_side(r, a_var, b_var, a_props, b_props)?;
            merge_side(ls, rs)
        }
        Expr::Not(x) => key_side(x, a_var, b_var, a_props, b_props),
        Expr::IsNull { of, .. } => key_side(of, a_var, b_var, a_props, b_props),
        // `<needle> IN <const list>`: the side is the needle's — the rhs is a
        // constant list (folded at eval time), reading no var. A non-const rhs
        // (e.g. `x IN friends`, a non-const list) falls through to decline,
        // exactly as `eval_column` declines it. Kept in lockstep with
        // `eval_column`'s accepted set.
        Expr::In(l, r) if is_const(r) => key_side(l, a_var, b_var, a_props, b_props),
        // A SCALAR function reads the MERGED side of its args — `toInteger(x.prop)`
        // is side A iff `x.prop` is. Kept in lockstep with `eval_column`'s scalar-
        // `Call` arm (which computes the value); an aggregate / `*` / `DISTINCT` /
        // no-arg fn declines, matching that arm exactly.
        Expr::Call {
            name,
            args,
            distinct: false,
            star: false,
        } if !args.is_empty() && !is_aggregate_fn(name) => {
            let mut side = Side::Const;
            for a in args {
                side = merge_side(side, key_side(a, a_var, b_var, a_props, b_props)?)?;
            }
            Some(side)
        }
        // A `CASE` reads the MERGED side of its subject, every WHEN/THEN and its
        // ELSE — single-column iff they all resolve to one side. Kept in lockstep
        // with `eval_column`'s `Expr::Case` arm (which computes the value); a
        // sub-expr `eval_column` cannot vectorise declines here identically.
        Expr::Case {
            subject,
            arms,
            otherwise,
        } => {
            let mut side = Side::Const;
            if let Some(s) = subject {
                side = merge_side(side, key_side(s, a_var, b_var, a_props, b_props)?)?;
            }
            for (when, then) in arms {
                side = merge_side(side, key_side(when, a_var, b_var, a_props, b_props)?)?;
                side = merge_side(side, key_side(then, a_var, b_var, a_props, b_props)?)?;
            }
            if let Some(e) = otherwise {
                side = merge_side(side, key_side(e, a_var, b_var, a_props, b_props)?)?;
            }
            Some(side)
        }
        // Fix 63: a MAP or LIST literal reads the MERGED side of its values —
        // `collect({id: gp.id, type: gp.type, createdAt: gp.createdAt})` is
        // side B iff every value is. Kept in lockstep with `eval_column`'s
        // `Expr::Map` / `Expr::List` arms (which build the value per row).
        // The production orchestrator statement declined the aggregate
        // pipeline for this one argument shape and ran the general path: a
        // projected get per hop end (6.3 ms against Neo4j's 0.9 on the mirror,
        // where the `count(gp)` spelling of the same chain ran in 0.5).
        Expr::Map(entries) => {
            let mut side = Side::Const;
            for (_, v) in entries {
                side = merge_side(side, key_side(v, a_var, b_var, a_props, b_props)?)?;
            }
            Some(side)
        }
        Expr::List(items) => {
            let mut side = Side::Const;
            for v in items {
                side = merge_side(side, key_side(v, a_var, b_var, a_props, b_props)?)?;
            }
            Some(side)
        }
        _ => None,
    }
}

struct TopkPlan {
    a_labels: Vec<String>,
    a_var: String,
    dir: Dir,
    types: Vec<String>,
    b_labels: Vec<String>,
    b_var: String,
    /// The b-only WHERE, if any (else no filter).
    where_: Option<Expr>,
    /// a-side properties read by the ORDER BY keys.
    a_props: BTreeSet<String>,
    /// b-side properties read by the WHERE + ORDER BY keys.
    b_props: BTreeSet<String>,
    /// The full projection (items, order, skip, limit) for the shared tail.
    proj: engram_cypher::stmt::Projection,
}

/// Recognise `MATCH (a:A)-[:T]->(b:B) [WHERE <b-pred>] RETURN <items over a,b>
/// ORDER BY <keys> [SKIP s] LIMIT k` — a fixed directed hop, labelled
/// unpropertied start with a variable, named unpropertied end, no rel
/// variable/props/length, a non-star non-DISTINCT non-aggregating projection
/// with a required LIMIT, whose items read only `a`/`b` and whose ORDER BY
/// keys are single-side `eval_column`-vectorisable. Anything else → `None`.
fn recognise_topk(sq: &SingleQuery) -> Option<TopkPlan> {
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
    // ORDER BY + LIMIT, non-star, non-DISTINCT, at least one item.
    if proj.star
        || proj.distinct
        || proj.order.is_empty()
        || proj.limit.is_none()
        || proj.items.is_empty()
    {
        return None;
    }
    // One path, one fixed directed hop; no path/rel variable, no rel
    // props/length; unpropertied labelled start with a variable; named
    // unpropertied end.
    if pattern.paths.len() != 1 {
        return None;
    }
    let path = &pattern.paths[0];
    if path.var.is_some() || path.hops.len() != 1 || path.start.props.is_some() {
        return None;
    }
    if path.start.labels.is_empty() {
        return None;
    }
    let a_var = path.start.var.as_deref()?.to_string();
    let (rel, b) = &path.hops[0];
    if rel.var.is_some() || rel.props.is_some() || rel.length.is_some() || rel.types.is_empty() {
        return None;
    }
    let dir = match rel.dir {
        RelDir::Out => Dir::Out,
        RelDir::In => Dir::In,
        RelDir::Undirected => return None,
    };
    let b_var = b.var.as_deref()?.to_string();
    if b.props.is_some() || a_var == b_var {
        return None;
    }
    // WHERE (optional): a b-only predicate `eval_column` handles; collect its
    // b-columns.
    let mut a_props = BTreeSet::new();
    let mut b_props = BTreeSet::new();
    if let Some(w) = where_opt {
        if !where_props(w, &b_var, &mut b_props) {
            return None;
        }
    }
    // ORDER BY keys: each a single-side `eval_column`-vectorisable key over `a`
    // or `b` (or const), non-opaque, non-aggregating.
    for o in &proj.order {
        if crate::interp::contains_opaque(&o.expr) || crate::interp::expr_has_aggregate(&o.expr) {
            return None;
        }
        key_side(&o.expr, &a_var, &b_var, &mut a_props, &mut b_props)?;
    }
    // Projection items: read ONLY `a`/`b`, non-aggregating, non-opaque — the
    // shared tail evaluates them per survivor, so any such form is fine.
    for it in &proj.items {
        if crate::interp::contains_opaque(&it.expr) || crate::interp::expr_has_aggregate(&it.expr) {
            return None;
        }
        let mut vars = Vec::new();
        crate::interp::free_vars_of(&it.expr, &mut vars);
        if !vars.iter().all(|v| *v == a_var || *v == b_var) {
            return None;
        }
    }
    Some(TopkPlan {
        a_labels: path.start.labels.clone(),
        a_var,
        dir,
        types: rel.types.clone(),
        b_labels: b.labels.clone(),
        b_var,
        where_: where_opt.cloned(),
        a_props,
        b_props,
        proj: proj.clone(),
    })
}

/// Load each requested property as ONE column over the id-span of `distinct`,
/// aligned positionally to `distinct`. `Ok(None)` = a span too wide for the
/// column budget → decline the whole operator to the general path.
pub(crate) fn load_side_columns(
    graph: &Graph,
    distinct: &[u64],
    props: &BTreeSet<String>,
) -> Result<Option<BTreeMap<String, Vec<Value>>>, RunError> {
    load_family_columns(graph, ColumnFamily::Nodes, distinct, props)
}

/// The relationship-family twin of [`load_side_columns`]: each requested rel
/// property as ONE column over the id-span of `distinct` (RELATIONSHIP ids),
/// aligned positionally to `distinct`, read from `ColumnFamily::Rels`. `Ok(None)`
/// = a span too wide for the column budget → decline. Used when a bound
/// RELATIONSHIP variable's properties are read by a WHERE / ORDER BY / group-by /
/// aggregate over the DataChunk pipeline.
pub(crate) fn load_rel_columns(
    graph: &Graph,
    distinct: &[u64],
    props: &BTreeSet<String>,
) -> Result<Option<BTreeMap<String, Vec<Value>>>, RunError> {
    load_family_columns(graph, ColumnFamily::Rels, distinct, props)
}

/// Load each requested property as ONE aligned column over `distinct` from the
/// given record `family` — the shared body of [`load_side_columns`] (Nodes) and
/// [`load_rel_columns`] (Rels).
fn load_family_columns(
    graph: &Graph,
    family: ColumnFamily,
    distinct: &[u64],
    props: &BTreeSet<String>,
) -> Result<Option<BTreeMap<String, Vec<Value>>>, RunError> {
    let mut cols: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    if props.is_empty() || distinct.is_empty() {
        return Ok(Some(cols));
    }
    let (lo, hi) = (distinct[0], distinct[distinct.len() - 1]);
    let budget = graph.columnar_column_budget(distinct.len());
    for prop in props {
        // The range scan over `[lo, hi)` DECLINES (Ok(None)) when the id set is
        // SPARSE — its span holds more property entries than the budget because
        // other node types in the range carry the same prop (every node type
        // carries `id`). Fall back to a POINT-GATHER of exactly `distinct`'s
        // values: O(members) point reads, never a range scan, byte-identical to
        // what the scan would have produced (same token, same tagged bytes, same
        // decode, same settle) so `align` fills the same column — absents
        // included. The gather reads exactly `members` entries whereas the scan
        // declined only after visiting > budget (≥ 4×members), so it is never
        // more work than the scan it replaces.
        let column = match graph
            .column_entries_bounded_in(family, prop, lo, hi.checked_add(1), budget)
            .map_err(RunError::Graph)?
        {
            Some(column) => column,
            None => graph
                .column_entries_gather(family, prop, distinct)
                .map_err(RunError::Graph)?,
        };
        cols.insert(prop.clone(), align(distinct, &column));
    }
    Ok(Some(cols))
}

/// One ORDER BY key precomputed as a value column over its side's distinct ids
/// (or a single broadcast constant), ready to gather per pair.
enum KeyCol {
    Const(Value),
    A(Vec<Value>),
    B(Vec<Value>),
}

/// The shared columnar top-k TAIL for a fixed-hop project + ORDER BY + LIMIT.
/// Given the produced `(a,b)` id-pairs in PRODUCTION ORDER (the stable-sort
/// tiebreak), the sorted distinct ids per side and the loaded-column budget, it
/// loads each side's columns, evaluates the b-only WHERE column-at-a-time into a
/// `pass` id-set, precomputes every ORDER BY key as a value column, runs the
/// bounded top-k (NO per-pair `eval_with`/node clone/row), and late-materialises
/// the `<= cap` winners through the SHARED `project_pairs_tail` — so value
/// ordering, NULLS, DESC and stability are the general path's own code, not a
/// re-implementation. `Ok(None)` = a budget/type decline; the caller returns
/// None and the general path answers identically. Reused by BOTH the label-scan
/// (`try_vectorized_hop_topk`) and the UNWIND-led
/// (`try_vectorized_unwind_hop_topk`) operators, so this selection/ordering
/// logic exists exactly ONCE. Callers pass NON-EMPTY `pairs`; the empty case is
/// a direct empty projection they handle before calling.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finish_topk(
    graph: &Graph,
    params: &BTreeMap<String, Value>,
    proj: &engram_cypher::stmt::Projection,
    a_var: &str,
    b_var: &str,
    where_: &Option<Expr>,
    a_props: &BTreeSet<String>,
    b_props: &BTreeSet<String>,
    pairs: &[(u64, u64)],
    distinct_a: &[u64],
    distinct_b: &[u64],
    cap: usize,
) -> Result<Option<QueryResult>, RunError> {
    // Load the columns each side needs (a-side ORDER BY keys; b-side WHERE +
    // ORDER BY keys). A budget decline on either → the general path.
    let (Some(a_cols), Some(b_cols)) = (
        load_side_columns(graph, distinct_a, a_props)?,
        load_side_columns(graph, distinct_b, b_props)?,
    ) else {
        return Ok(None);
    };

    let empty_vm = engram_cypher::bindings::VarMap::new();
    let scope = Scope::over(params, &empty_vm, graph.wall_ms(), graph.zone_provider());

    // THE VECTORISED FILTER: evaluate the WHERE column-at-a-time over distinct
    // b (O(distinct_b), not O(pairs)) into a sorted `pass` set — a non-boolean
    // result declines, exactly as the count operator does.
    let a_view = view(&a_cols);
    let b_view = view(&b_cols);
    let pass: Option<Vec<u64>> = match where_ {
        None => None,
        Some(w) => {
            let Some(res) = eval_column(w, b_var, distinct_b.len(), &b_view, &scope) else {
                return Ok(None);
            };
            let mut pass = Vec::with_capacity(distinct_b.len());
            for (i, v) in res.iter().enumerate() {
                match v.truth() {
                    Some(Truth::True) => pass.push(distinct_b[i]), // distinct_b sorted
                    Some(_) => {}
                    None => return Ok(None), // non-boolean WHERE — the general path errors
                }
            }
            Some(pass)
        }
    };

    // Precompute each ORDER BY key as a value column over its side (one
    // `eval_column` per key), broadcasting consts. Divergence from the
    // per-tuple `eval_expr` on the same key is structurally impossible for the
    // accepted forms (props/consts/three-valued booleans) — the same primitive.
    let order = &proj.order;
    let mut keycols: Vec<KeyCol> = Vec::with_capacity(order.len());
    for o in order {
        let (mut ap, mut bp) = (BTreeSet::new(), BTreeSet::new());
        match key_side(&o.expr, a_var, b_var, &mut ap, &mut bp)
            .expect("recogniser validated every ORDER BY key")
        {
            Side::Const => {
                keycols.push(KeyCol::Const(
                    eval_with(&o.expr, &scope, None).map_err(RunError::Eval)?,
                ));
            }
            Side::A => {
                let Some(col) = eval_column(&o.expr, a_var, distinct_a.len(), &a_view, &scope)
                else {
                    return Ok(None);
                };
                keycols.push(KeyCol::A(col.into_owned()));
            }
            Side::B => {
                let Some(col) = eval_column(&o.expr, b_var, distinct_b.len(), &b_view, &scope)
                else {
                    return Ok(None);
                };
                keycols.push(KeyCol::B(col.into_owned()));
            }
        }
    }

    // BOUNDED TOP-K over id-pairs, ordered by (ORDER BY keys, production seq) —
    // the buffer IS the stable sort's first `cap` rows at all times, identical
    // to `StreamProjector`'s top-k. `seq` counts WHERE-survivors in production
    // order, exactly the collector's arrival counter (WHERE is applied before
    // a row reaches the collector). No per-pair `eval_with`, node clone or row.
    let mut topk: Vec<(Vec<Value>, u64, u64, u64)> = Vec::new();
    let mut seq: u64 = 0;
    for &(a, b) in pairs {
        if let Some(p) = &pass {
            if p.binary_search(&b).is_err() {
                continue;
            }
        }
        let ai = distinct_a.binary_search(&a).expect("a produced this pair");
        let bi = distinct_b.binary_search(&b).expect("b produced this pair");
        let mut key: Vec<Value> = Vec::with_capacity(keycols.len());
        for kc in &keycols {
            key.push(match kc {
                KeyCol::Const(v) => v.clone(),
                KeyCol::A(col) => col[ai].clone(),
                KeyCol::B(col) => col[bi].clone(),
            });
        }
        let s = seq;
        seq += 1;
        if cap == 0 {
            continue;
        }
        let pos = topk.partition_point(|(k, ks, _, _)| {
            match crate::interp::cmp_order_keys(order, k, &key) {
                std::cmp::Ordering::Less => true,
                std::cmp::Ordering::Equal => *ks < s,
                std::cmp::Ordering::Greater => false,
            }
        });
        if pos < cap {
            topk.insert(pos, (key, s, a, b));
            topk.truncate(cap);
        }
    }

    // LATE-MATERIALISE only the winners (already in (key, seq) order) through
    // the shared projection tail: it re-sorts stable (same order) and applies
    // SKIP/LIMIT — byte-identical to the per-tuple path.
    let winners: Vec<(u64, u64)> = topk.into_iter().map(|(_, _, a, b)| (a, b)).collect();
    Ok(Some(crate::interp::project_pairs_tail(
        graph, proj, params, a_var, b_var, &winners,
    )?))
}

pub(crate) fn try_vectorized_hop_topk(
    graph: &Graph,
    q: &SingleQuery,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    if !graph.columnar_scans_enabled() {
        return Ok(None);
    }
    let Some(plan) = recognise_topk(q) else {
        return Ok(None);
    };

    // cap = skip + limit — the top-k retains exactly this many, then the tail
    // applies SKIP/LIMIT. A non-const / negative / non-integer bound raises the
    // SAME error the general path would (`eval_count`), never a wrong answer.
    let limit = crate::interp::eval_count(graph, plan.proj.limit.as_ref(), params, "LIMIT")?
        .expect("recognise_topk requires a LIMIT");
    let skip =
        crate::interp::eval_count(graph, plan.proj.skip.as_ref(), params, "SKIP")?.unwrap_or(0);
    let cap = skip.saturating_add(limit);

    let empty_out = |g: &Graph| {
        crate::interp::project_pairs_tail(g, &plan.proj, params, &plan.a_var, &plan.b_var, &[])
    };

    let a_ids = graph.members_all(&plan.a_labels).map_err(RunError::Graph)?;
    if a_ids.is_empty() {
        return Ok(Some(empty_out(graph)?));
    }
    let b_members = if plan.b_labels.is_empty() {
        None
    } else {
        Some(graph.members_all(&plan.b_labels).map_err(RunError::Graph)?)
    };
    let tokens = graph.type_tokens_peek(&plan.types);
    if matches!(&tokens, Some(v) if v.is_empty()) {
        return Ok(Some(empty_out(graph)?)); // a named type never minted
    }

    // PRODUCTION ORDER (the stable-sort tiebreak): A ascending — `members_all`
    // is id-sorted, matching the `Seed::Label` node-driven scan — and each A's
    // neighbours in REVERSE `adjacent_slim` order, the LIFO emission order of
    // the per-tuple `expand_var_length`. `pairs` carries b's multiplicity (one
    // entry per matching edge), exactly as the enumerating path does.
    let mut pairs: Vec<(u64, u64)> = Vec::new();
    let mut distinct_a_set: BTreeSet<u64> = BTreeSet::new();
    let mut distinct_b_set: BTreeSet<u64> = BTreeSet::new();
    for a in a_ids.iter() {
        // REVERSE `adjacent_slim` order (the LIFO emission order), fed zero-copy
        // from the cached CSR slice iterated back-to-front — no per-node Vec.
        graph.adjacent_slim_rev_for_each(a, plan.dir, &tokens, |e| {
            let b = e.peer;
            if let Some(m) = &b_members {
                if !m.contains(b) {
                    return;
                }
            }
            pairs.push((a, b));
            distinct_a_set.insert(a);
            distinct_b_set.insert(b);
        });
        crate::interp::budget_check(graph, pairs.len())?;
    }
    if pairs.is_empty() {
        return Ok(Some(empty_out(graph)?));
    }
    let distinct_a: Vec<u64> = distinct_a_set.into_iter().collect();
    let distinct_b: Vec<u64> = distinct_b_set.into_iter().collect();

    // The shared tail: load columns, vectorised WHERE, ORDER BY key columns,
    // bounded top-k, late-materialise the winners.
    finish_topk(
        graph,
        params,
        &plan.proj,
        &plan.a_var,
        &plan.b_var,
        &plan.where_,
        &plan.a_props,
        &plan.b_props,
        &pairs,
        &distinct_a,
        &distinct_b,
        cap,
    )
}

/// The UNWIND-led counterpart of `TopkPlan`: the source is an UNWIND of a
/// list-of-nodes bound only through params/consts (the LDBC IC9 stage-2 shape,
/// `UNWIND $friends AS f MATCH (f)<-[:HAS_CREATOR]-(m:Message) ...`), rather
/// than a label scan.
struct UnwindTopkPlan {
    /// The UNWIND list expression — a param or a const list (empty-row eval).
    source: Expr,
    /// The UNWIND alias and the hop's (bound) start variable.
    f_var: String,
    dir: Dir,
    types: Vec<String>,
    m_labels: Vec<String>,
    m_var: String,
    /// The m-only WHERE, if any.
    where_: Option<Expr>,
    /// f-side properties read by the ORDER BY keys.
    f_props: BTreeSet<String>,
    /// m-side properties read by the WHERE + ORDER BY keys.
    m_props: BTreeSet<String>,
    proj: engram_cypher::stmt::Projection,
}

/// Recognise `UNWIND <param/const list> AS f MATCH (f)-[:T]->(m:M) [WHERE
/// <m-pred>] RETURN <items over f,m> ORDER BY <keys> [SKIP s] LIMIT k` — the
/// UNWIND-led fixed-hop project + top-k. The UNWIND source must reference only
/// params/consts (it is the first clause, evaluated over an empty row); the hop
/// start `f` is the UNWIND-bound node (var == alias, NO labels/props — it is
/// already a node, not a scan); a fixed directed hop with no rel
/// variable/props/length; a named unpropertied end that may carry a label; a
/// non-star non-DISTINCT non-aggregating projection with a required LIMIT whose
/// items read only `f`/`m` and whose ORDER BY keys are single-side
/// `eval_column`-vectorisable. Anything else → `None`. The collect-fed multi-
/// clause form (`… WITH collect(DISTINCT x) AS friends UNWIND friends …`) is
/// NOT matched here and declines to the general path — that recogniser is the
/// follow-on increment.
fn recognise_unwind_topk(sq: &SingleQuery) -> Option<UnwindTopkPlan> {
    let (source, f_var, pattern, where_opt, proj) = match sq.clauses.as_slice() {
        [
            Clause::Unwind { expr, alias },
            Clause::Match {
                optional: false,
                pattern,
                where_,
            },
            Clause::Return { proj },
        ] => (expr, alias, pattern, where_.as_ref(), proj),
        _ => return None,
    };
    // The list source must fold from params/consts alone: UNWIND is the first
    // clause, so its row is empty and any variable reference is unbound (the
    // general path would error). Restricting to a param or a const list keeps
    // the empty-row eval sound.
    if !(matches!(source, Expr::Param(_)) || is_const(source)) {
        return None;
    }
    unwind_stage2_plan(source, f_var, pattern, where_opt, proj)
}

/// The shared stage-2 recogniser for `UNWIND <src> AS f MATCH (f)-[:T]->(m)
/// [WHERE <m>] RETURN … ORDER BY … [SKIP] LIMIT`: validate the hop, projection
/// and ORDER BY and build the plan, WITHOUT constraining what `src` is. The
/// `$param` operator requires a param/const source (checked by its caller); the
/// collect-fed operator supplies a list it materialised from a prefix. Both then
/// hand the resulting list to `run_unwind_stage2`, so stage-2 is one code path.
fn unwind_stage2_plan(
    source: &Expr,
    f_var: &str,
    pattern: &engram_cypher::stmt::Pattern,
    where_opt: Option<&Expr>,
    proj: &engram_cypher::stmt::Projection,
) -> Option<UnwindTopkPlan> {
    // ORDER BY + LIMIT, non-star, non-DISTINCT, at least one item.
    if proj.star
        || proj.distinct
        || proj.order.is_empty()
        || proj.limit.is_none()
        || proj.items.is_empty()
    {
        return None;
    }
    // One path, one fixed directed hop; the start is the UNWIND-bound node
    // (var == alias, unlabelled, unpropertied); named unpropertied end.
    if pattern.paths.len() != 1 {
        return None;
    }
    let path = &pattern.paths[0];
    if path.var.is_some() || path.hops.len() != 1 {
        return None;
    }
    if path.start.var.as_deref() != Some(f_var)
        || !path.start.labels.is_empty()
        || path.start.props.is_some()
    {
        return None;
    }
    let (rel, m) = &path.hops[0];
    if rel.var.is_some() || rel.props.is_some() || rel.length.is_some() || rel.types.is_empty() {
        return None;
    }
    let dir = match rel.dir {
        RelDir::Out => Dir::Out,
        RelDir::In => Dir::In,
        RelDir::Undirected => return None,
    };
    let m_var = m.var.as_deref()?.to_string();
    if m.props.is_some() || m_var.as_str() == f_var {
        return None;
    }
    // WHERE (optional): an m-only predicate `eval_column` handles; collect its
    // m-columns.
    let mut f_props = BTreeSet::new();
    let mut m_props = BTreeSet::new();
    if let Some(w) = where_opt {
        if !where_props(w, &m_var, &mut m_props) {
            return None;
        }
    }
    // ORDER BY keys: each single-side over `f` (A) or `m` (B) or const.
    for o in &proj.order {
        if crate::interp::contains_opaque(&o.expr) || crate::interp::expr_has_aggregate(&o.expr) {
            return None;
        }
        key_side(&o.expr, f_var, &m_var, &mut f_props, &mut m_props)?;
    }
    // Projection items: read ONLY `f`/`m`, non-aggregating, non-opaque.
    for it in &proj.items {
        if crate::interp::contains_opaque(&it.expr) || crate::interp::expr_has_aggregate(&it.expr) {
            return None;
        }
        let mut vars = Vec::new();
        crate::interp::free_vars_of(&it.expr, &mut vars);
        if !vars.iter().all(|v| v.as_str() == f_var || *v == m_var) {
            return None;
        }
    }
    Some(UnwindTopkPlan {
        source: source.clone(),
        f_var: f_var.to_string(),
        dir,
        types: rel.types.clone(),
        m_labels: m.labels.clone(),
        m_var,
        where_: where_opt.cloned(),
        f_props,
        m_props,
        proj: proj.clone(),
    })
}

pub(crate) fn try_vectorized_unwind_hop_topk(
    graph: &Graph,
    q: &SingleQuery,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    if !graph.columnar_scans_enabled() {
        return Ok(None);
    }
    let Some(plan) = recognise_unwind_topk(q) else {
        return Ok(None);
    };

    // Evaluate the UNWIND source over an EMPTY row (it references only
    // params/consts). Null → no rows (`UNWIND null` yields nothing → an empty
    // list, handled by `run_unwind_stage2`); a non-list → the general path
    // errors, so DECLINE and let it.
    let empty_vm = engram_cypher::bindings::VarMap::new();
    let scope = Scope::over(params, &empty_vm, graph.wall_ms(), graph.zone_provider());
    let src = eval_with(&plan.source, &scope, None).map_err(RunError::Eval)?;
    let items = match src {
        Value::Null => Vec::new(),
        Value::List(items) => items,
        _ => return Ok(None), // a non-list UNWIND — the general path errors identically
    };
    run_unwind_stage2(graph, params, &plan, items)
}

/// Stage-2 execution shared by the `$param` and collect-fed UNWIND operators:
/// given the ALREADY-MATERIALISED UNWIND list, build the (f,m) pairs in list
/// order × REVERSE adjacency and select the top-k through `finish_topk`. The
/// only code path either operator takes once it holds the list. DECLINES
/// (Ok(None)) on a non-node or missing-node element, so the general path answers
/// identically.
fn run_unwind_stage2(
    graph: &Graph,
    params: &BTreeMap<String, Value>,
    plan: &UnwindTopkPlan,
    items: Vec<Value>,
) -> Result<Option<QueryResult>, RunError> {
    // cap = skip + limit — the same bound the general path would raise on
    // (`eval_count`), never a wrong answer.
    let limit = crate::interp::eval_count(graph, plan.proj.limit.as_ref(), params, "LIMIT")?
        .expect("the recogniser requires a LIMIT");
    let skip =
        crate::interp::eval_count(graph, plan.proj.skip.as_ref(), params, "SKIP")?.unwrap_or(0);
    let cap = skip.saturating_add(limit);

    let empty_out = |g: &Graph| {
        crate::interp::project_pairs_tail(g, &plan.proj, params, &plan.f_var, &plan.m_var, &[])
    };

    // The unwound values are the hop's start nodes, IN LIST ORDER (duplicates
    // kept — UNWIND yields a repeated node twice, and each expands). A Null
    // element binds `f` to Null and produces no rows (skip); a non-node element
    // makes the general path error, and a node id absent from the store makes it
    // error too — DECLINE either so it answers identically.
    let mut f_ids: Vec<u64> = Vec::with_capacity(items.len());
    for it in &items {
        match it {
            Value::Node { id, .. } => {
                if graph.node(*id).map_err(RunError::Graph)?.is_none() {
                    return Ok(None); // missing start node — the general path errors
                }
                f_ids.push(*id);
            }
            Value::Null => {}     // `f = null` → no rows for this element
            _ => return Ok(None), // a non-node element — the general path errors
        }
    }

    let m_members = if plan.m_labels.is_empty() {
        None
    } else {
        Some(graph.members_all(&plan.m_labels).map_err(RunError::Graph)?)
    };
    let tokens = graph.type_tokens_peek(&plan.types);
    if matches!(&tokens, Some(v) if v.is_empty()) {
        return Ok(Some(empty_out(graph)?)); // a named type never minted
    }

    // PRODUCTION ORDER (the stable-sort tiebreak): the UNWIND list order over
    // `f` (duplicates kept), and for each `f` its neighbours in REVERSE
    // `adjacent_slim` order — the LIFO emission order of the per-tuple
    // `expand_var_length` from a bound start (verified against interp.rs). One
    // `pairs` entry per matching edge, exactly as the enumerating path emits.
    let mut pairs: Vec<(u64, u64)> = Vec::new();
    let mut distinct_f_set: BTreeSet<u64> = BTreeSet::new();
    let mut distinct_m_set: BTreeSet<u64> = BTreeSet::new();
    for &f in &f_ids {
        // REVERSE `adjacent_slim` order (the LIFO emission order), fed zero-copy
        // from the cached CSR slice iterated back-to-front — no per-node Vec.
        graph.adjacent_slim_rev_for_each(f, plan.dir, &tokens, |e| {
            let m = e.peer;
            if let Some(mm) = &m_members {
                if !mm.contains(m) {
                    return;
                }
            }
            pairs.push((f, m));
            distinct_f_set.insert(f);
            distinct_m_set.insert(m);
        });
        crate::interp::budget_check(graph, pairs.len())?;
    }
    if pairs.is_empty() {
        return Ok(Some(empty_out(graph)?));
    }
    let distinct_f: Vec<u64> = distinct_f_set.into_iter().collect();
    let distinct_m: Vec<u64> = distinct_m_set.into_iter().collect();

    finish_topk(
        graph,
        params,
        &plan.proj,
        &plan.f_var,
        &plan.m_var,
        &plan.where_,
        &plan.f_props,
        &plan.m_props,
        &pairs,
        &distinct_f,
        &distinct_m,
        cap,
    )
}

/// A recognised collect-fed IC9: an arbitrary PREFIX ending in a single
/// `WITH collect(DISTINCT v) AS list`, then the stage-2 `UNWIND list AS f MATCH
/// (f)-[:T]->(m) …` the `$param` operator already handles.
struct CollectIc9Plan {
    /// Every clause up to AND INCLUDING the `WITH collect(...) AS list`.
    prefix: Vec<Clause>,
    /// The collected list variable, read back out of the prefix.
    list_alias: String,
    /// The stage-2 plan (source is `Var(list)`, unused — the list is supplied).
    stage2: UnwindTopkPlan,
}

/// Recognise `<PREFIX> WITH collect(DISTINCT v) AS list UNWIND list AS f MATCH
/// (f)-[:T]->(m:M) [WHERE m] RETURN … ORDER BY … LIMIT` — the production LDBC
/// IC9 shape. The PREFIX (its stage-1 `KNOWS*1..2`) is any clause sequence; it
/// runs on the general path. Only the terminal
/// `WITH collect(DISTINCT …) / UNWIND / MATCH / RETURN` is constrained, so the
/// prefix collapses to exactly one row (no grouping key beside the collect).
fn recognise_collect_ic9(sq: &SingleQuery) -> Option<CollectIc9Plan> {
    let n = sq.clauses.len();
    if n < 4 {
        return None;
    }
    let Clause::Return { proj } = &sq.clauses[n - 1] else {
        return None;
    };
    let Clause::Match {
        optional: false,
        pattern,
        where_,
    } = &sq.clauses[n - 2]
    else {
        return None;
    };
    let Clause::Unwind {
        expr: unwind_expr,
        alias: f_var,
    } = &sq.clauses[n - 3]
    else {
        return None;
    };
    let Clause::With {
        proj: with_proj, ..
    } = &sq.clauses[n - 4]
    else {
        return None;
    };

    // The WITH must be exactly `collect(DISTINCT <v>) AS <list>`: one item, no
    // grouping key beside it (a grouping key makes the prefix multi-row and
    // changes the UNWIND semantics), and no projection DISTINCT/star/ORDER/
    // SKIP/LIMIT on the WITH itself.
    if with_proj.star
        || with_proj.distinct
        || !with_proj.order.is_empty()
        || with_proj.skip.is_some()
        || with_proj.limit.is_some()
        || with_proj.items.len() != 1
    {
        return None;
    }
    let list_alias = with_proj.items[0].alias.as_deref()?;
    match &with_proj.items[0].expr {
        Expr::Call {
            name,
            distinct: true,
            args,
            star: false,
        } if name == "collect" && args.len() == 1 => {}
        _ => return None,
    }
    // The UNWIND source must be exactly that collected list variable.
    match unwind_expr {
        Expr::Var(v) if v == list_alias => {}
        _ => return None,
    }
    // Stage-2 must satisfy the shared recogniser. The source is the bound list
    // var, so the param/const check does NOT apply — that is the `$param`
    // operator's own rule, and here the list is materialised, not folded.
    let stage2 = unwind_stage2_plan(unwind_expr, f_var, pattern, where_.as_ref(), proj)?;
    Some(CollectIc9Plan {
        prefix: sq.clauses[..n - 3].to_vec(),
        list_alias: list_alias.to_string(),
        stage2,
    })
}

/// The collect-fed IC9 operator: run the PREFIX on the general path to
/// materialise `friends`, then hand that node list to the shared stage-2 core.
/// Exact by composition — the prefix produces the identical list the full query
/// would, and stage-2 is byte-identical to the general path GIVEN a list.
pub(crate) fn try_vectorized_collect_ic9_topk(
    graph: &Graph,
    q: &SingleQuery,
    params: &BTreeMap<String, Value>,
) -> Result<Option<QueryResult>, RunError> {
    if !graph.columnar_scans_enabled() {
        return Ok(None);
    }
    let Some(plan) = recognise_collect_ic9(q) else {
        return Ok(None);
    };

    // Run the PREFIX (through `WITH collect(DISTINCT v) AS list`) on the GENERAL
    // path and read `list` back — the exact node list the full query's UNWIND
    // would see. Reusing the engine's own executor introduces no new expansion
    // or ordering logic; `collect(DISTINCT …)` accumulates in first-encounter
    // order here exactly as it would inline.
    let mut prefix_clauses = plan.prefix.clone();
    prefix_clauses.push(Clause::Return {
        proj: engram_cypher::stmt::Projection {
            distinct: false,
            star: false,
            items: vec![engram_cypher::stmt::ProjItem {
                expr: Expr::Var(plan.list_alias.clone()),
                alias: None,
                text: None,
            }],
            order: Vec::new(),
            skip: None,
            limit: None,
        },
    });
    let prefix_q = engram_cypher::stmt::Query::Single(SingleQuery {
        clauses: prefix_clauses,
    });
    let result = crate::interp::run_query(graph, &prefix_q, params.clone())?;

    // The un-grouped collect collapses its input to exactly one row holding the
    // list. Anything else → DECLINE, so the general path runs the whole query.
    if result.rows.len() != 1 || result.rows[0].len() != 1 {
        return Ok(None);
    }
    let items = match &result.rows[0][0] {
        Value::List(items) => items.clone(),
        Value::Null => Vec::new(),
        _ => return Ok(None),
    };

    run_unwind_stage2(graph, params, &plan.stage2, items)
}

#[cfg(test)]
mod sparse_gather_tests {
    //! Site 1 of the IC5-class point-gather widening: the vectorized HOP-FILTER-COUNT
    //! (`try_vectorized_hop_filter_count`). The later, more general
    //! `plan_and_run_columnar` aggregate SHADOWS this operator for the `count(*)`
    //! shape in `run_single`'s dispatch, so no end-to-end query reaches it — these
    //! tests call it DIRECTLY, where its gather-on-decline is observable. SPARSE:
    //! the `b.prop` column's range scan over the scattered hop-ends DECLINES and the
    //! point-gather fires (byte-identical to the general path). DENSE: the range
    //! path is taken (no gather).

    use std::collections::BTreeMap;

    use engram_cypher::parse_statement;
    use engram_cypher::stmt::Query;
    use engram_cypher::value::Value;
    use engram_key::{Namespace, Realm};
    use engram_store::Store;

    use super::try_vectorized_hop_filter_count;
    use crate::Graph;

    /// a0 (no `prop`), b0(prop=5), then 10 filler nodes that ALSO carry `prop`, then
    /// b1(prop=9). The two distinct hop-ends {b0,b1} bracket 12 `prop` entries, over
    /// the 4×2 = 8 budget → the range scan DECLINES and the point-gather fires.
    fn ghop_sparse() -> Graph {
        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        let mk = |label: &str, prop: Option<i64>| {
            let mut m = BTreeMap::new();
            if let Some(v) = prop {
                m.insert("prop".to_string(), Value::Int(v));
            }
            g.create_node(&[label.into()], &m).expect("node")
        };
        let a0 = mk("Aa", None);
        let b0 = mk("Bb", Some(5));
        for k in 0..10 {
            let _ = mk("Filler", Some(1000 + k));
        }
        let b1 = mk("Bb", Some(9));
        g.create_rel(a0, "R", b0, &BTreeMap::new()).expect("R");
        g.create_rel(a0, "R", b1, &BTreeMap::new()).expect("R");
        g
    }

    /// b0, b1 consecutive in id space, no fillers → the `prop` column span holds 2
    /// entries, under budget, so the range path is taken (gather NOT invoked).
    fn ghop_dense() -> Graph {
        let g = Graph::new(Store::new(), Realm(1), Namespace(1));
        let mk = |label: &str, prop: Option<i64>| {
            let mut m = BTreeMap::new();
            if let Some(v) = prop {
                m.insert("prop".to_string(), Value::Int(v));
            }
            g.create_node(&[label.into()], &m).expect("node")
        };
        let a0 = mk("Aa", None);
        let b0 = mk("Bb", Some(5));
        let b1 = mk("Bb", Some(9));
        g.create_rel(a0, "R", b0, &BTreeMap::new()).expect("R");
        g.create_rel(a0, "R", b1, &BTreeMap::new()).expect("R");
        g
    }

    const SRC: &str = "MATCH (a:Aa)-[:R]->(b:Bb) WHERE b.prop < 100 RETURN count(*) AS c";

    /// SPARSE: the operator FIRES (returns `Some`) via the point-gather even though
    /// the `b.prop` range scan declined, and its result is byte-identical to the
    /// general per-tuple path. (Neutralizing the gather → this returns `None` → the
    /// `is_some` assertion fails: the canary.)
    #[test]
    fn hop_filter_count_gathers_on_sparse_decline() {
        let g = ghop_sparse();
        g.set_columnar_column_budget_factor(1); // force the range-scan decline regardless of the default
        let query = parse_statement(SRC).expect("parse");
        let Query::Single(q) = &query else {
            panic!("single query")
        };
        let params = BTreeMap::new();
        g.set_columnar_scans(true);
        let (got, trace) =
            engram_observe::with_trace(|| try_vectorized_hop_filter_count(&g, q, &params));
        let got = got
            .expect("no error")
            .expect("the operator must FIRE via the gather");
        assert_eq!(
            got.rows,
            vec![vec![Value::Int(2)]],
            "sparse hop-filter-count exact"
        );
        assert!(
            trace
                .counters()
                .get("graph.column point-gather")
                .copied()
                .unwrap_or(0)
                > 0,
            "the sparse hop-end column must fall back to the point-gather"
        );
        // Byte-identical to the general per-tuple path (columnar OFF).
        g.set_columnar_scans(false);
        let oracle = crate::interp::run_query(&g, &query, params).expect("oracle");
        assert_eq!(got.rows, oracle.rows, "vectorized vs general disagree");
    }

    /// DENSE control: the range scan FITS its budget, so the operator fires WITHOUT
    /// the gather (point-gather counter 0), and the result still agrees.
    #[test]
    fn hop_filter_count_uses_range_on_dense() {
        let g = ghop_dense();
        let query = parse_statement(SRC).expect("parse");
        let Query::Single(q) = &query else {
            panic!("single query")
        };
        let params = BTreeMap::new();
        g.set_columnar_scans(true);
        let (got, trace) =
            engram_observe::with_trace(|| try_vectorized_hop_filter_count(&g, q, &params));
        let got = got.expect("no error").expect("the operator must fire");
        assert_eq!(
            got.rows,
            vec![vec![Value::Int(2)]],
            "dense hop-filter-count exact"
        );
        assert_eq!(
            trace
                .counters()
                .get("graph.column point-gather")
                .copied()
                .unwrap_or(0),
            0,
            "the dense hop-end column must use the range scan, not the gather"
        );
    }
}
