//! The constant evaluator — the expression semantics, independent of any
//! graph. The clause layer will call this with graph values bound; every
//! rule about null, type refusal and overflow is decided HERE, once.

use std::collections::BTreeMap;

use engram_observe::{counted, sometimes};

use crate::ast::{BinOp, Expr};
use crate::json;
use crate::value::{Truth, Value};

/// Why an evaluation refused. Named, positioned at the operator, and never a
/// silent null: null is a VALUE with specified propagation, not an error
/// sink.
#[derive(Debug, Clone, PartialEq)]
pub enum EvalError {
    /// A parameter the call did not supply. Distinct from a parameter
    /// supplied as null — absence is not a value.
    UnknownParam(String),
    /// A variable with no binding in scope.
    UnknownVar(String),
    /// A function the registry does not know.
    UnknownFunction(String),
    /// An aggregate function used where only scalars can appear.
    AggregateInScalarContext(String),
    /// The operands' types do not fit the operator.
    Type {
        /// What was being evaluated.
        what: String,
        /// The offending type(s).
        got: String,
    },
    /// Integer arithmetic overflowed — refused, never wrapped or silently
    /// floated, because a wrapped id is a different id.
    Overflow(&'static str),
    /// Integer division or modulo by zero.
    DivisionByZero,
    /// `=~` — parsed but not yet evaluable (no regex engine in the core).
    RegexUnsupported,
    /// A graph-dependent expression (`EXISTS {}`, `COUNT {}`, a pattern
    /// comprehension) in a scalar context — the clause interpreter owns
    /// these; refusing by name keeps "unsupported" and "unknown" apart.
    GraphDependent(&'static str),
    /// A function refused its input (wrong arity, malformed JSON…).
    Function {
        /// The function.
        name: String,
        /// Why.
        detail: String,
    },
    /// A property or label read on an entity DELETEd earlier in the same
    /// statement — openCypher `DeletedEntityAccess`. `id()`/`elementId()` stay
    /// legal on a deleted entity, so only the data reads raise this.
    DeletedEntity,
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvalError::UnknownParam(p) => write!(f, "parameter ${p} was not supplied"),
            EvalError::UnknownVar(v) => write!(f, "variable `{v}` is not in scope"),
            EvalError::UnknownFunction(n) => write!(f, "unknown function `{n}()`"),
            EvalError::AggregateInScalarContext(n) => {
                write!(f, "aggregate `{n}()` is not valid in a scalar context")
            }
            EvalError::Type { what, got } => write!(f, "type error in {what}: got {got}"),
            EvalError::Overflow(op) => write!(f, "integer overflow in {op}"),
            EvalError::DivisionByZero => write!(f, "division by zero"),
            EvalError::RegexUnsupported => write!(f, "`=~` is not yet supported"),
            EvalError::GraphDependent(what) => write!(f, "{what} requires a graph context"),
            EvalError::Function { name, detail } => write!(f, "{name}(): {detail}"),
            EvalError::DeletedEntity => {
                write!(f, "cannot read a deleted entity (DeletedEntityAccess)")
            }
        }
    }
}

impl std::error::Error for EvalError {}

/// An evaluation scope: parameters (fixed for a statement) and variables
/// (grown by comprehensions, reduce, and — later — the clause pipeline).
#[derive(Clone)]
pub struct Scope<'a> {
    /// `$param` values — BORROWED. The interpreter evaluated every pushed
    /// conjunct, projection item and order key through a scope that
    /// cloned the whole row AND the whole params map first; at 1.16M
    /// per-pair evaluations on one production statement that clone was a
    /// measurable share of the statement. One evaluator, zero clones.
    pub params: &'a BTreeMap<String, Value>,
    /// The row's variable bindings — BORROWED.
    pub vars: &'a crate::bindings::VarMap,
    /// Bindings introduced by the expression itself (comprehension,
    /// reduce and list-predicate variables), layered OVER `vars`: the last
    /// binding of a name wins, so a re-bound accumulator reads correctly.
    pub locals: Vec<(String, Value)>,
    /// Wall-clock epoch milliseconds for `datetime()`/`timestamp()`.
    /// INJECTED (D1: time is a dependency) — absent means the constructors
    /// refuse by name rather than reaching for an ambient clock.
    pub now_ms: Option<i64>,
    /// Timezone rules, injected the same way. Absent, only what the built-in
    /// fixed table resolves works; named IANA zones refuse by name.
    pub zones: Option<std::sync::Arc<dyn crate::temporal::ZoneProvider>>,
}

static EMPTY_PARAMS: BTreeMap<String, Value> = BTreeMap::new();
static EMPTY_VARS: crate::bindings::VarMap = crate::bindings::VarMap::new();

impl Default for Scope<'static> {
    fn default() -> Self {
        Scope {
            params: &EMPTY_PARAMS,
            vars: &EMPTY_VARS,
            locals: Vec::new(),
            now_ms: None,
            zones: None,
        }
    }
}

impl<'a> Scope<'a> {
    /// A scope over a row and its params, nothing local yet.
    pub fn over(
        params: &'a BTreeMap<String, Value>,
        vars: &'a crate::bindings::VarMap,
        now_ms: Option<i64>,
        zones: Option<std::sync::Arc<dyn crate::temporal::ZoneProvider>>,
    ) -> Self {
        Scope {
            params,
            vars,
            locals: Vec::new(),
            now_ms,
            zones,
        }
    }

    /// A variable: the innermost local of that name, else the row's.
    pub fn var(&self, name: &str) -> Option<&Value> {
        self.locals
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v)
            .or_else(|| self.vars.get(name))
    }

    /// A child scope sharing the row and params — what a comprehension
    /// evaluates its body in. Only the locals vector copies.
    pub fn child(&self) -> Scope<'a> {
        Scope {
            params: self.params,
            vars: self.vars,
            locals: self.locals.clone(),
            now_ms: self.now_ms,
            zones: self.zones.clone(),
        }
    }

    /// Bind (or re-bind) a local.
    pub fn bind(&mut self, name: &str, value: Value) {
        if let Some(slot) = self.locals.iter_mut().rev().find(|(n, _)| n == name) {
            slot.1 = value;
        } else {
            self.locals.push((name.to_string(), value));
        }
    }

    /// Every visible binding as an owned map — locals over the row. This
    /// is the ONE place a clone happens, and only a subquery re-entering
    /// the interpreter (which binds a fresh row) asks for it.
    pub fn materialise(&self) -> crate::bindings::VarMap {
        let mut out = self.vars.clone();
        for (n, v) in &self.locals {
            out.insert(n.clone(), v.clone());
        }
        out
    }
}

impl std::fmt::Debug for Scope<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scope")
            .field("params", &self.params)
            .field("vars", &self.vars)
            .field("locals", &self.locals)
            .field("now_ms", &self.now_ms)
            .field("zones", &self.zones.as_ref().map(|_| "<provider>"))
            .finish()
    }
}

/// Aggregate names — refused in scalar evaluation so `count(x)` outside an
/// aggregation is a NAMED error rather than an unknown function.
const AGGREGATES: &[&str] = &[
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

/// Evaluate an expression in a scope.
pub fn eval(expr: &Expr, scope: &Scope<'_>) -> Result<Value, EvalError> {
    eval_with(expr, scope, None)
}

/// The graph hooks: an interpreter supplies these so `EXISTS {}`,
/// `COUNT {}` and pattern comprehensions evaluate against a graph. Without
/// them those forms refuse BY NAME.
pub trait GraphHooks {
    /// Evaluate `EXISTS { body }` under `scope`.
    fn exists(
        &self,
        body: &crate::stmt::SubqueryBody,
        scope: &Scope<'_>,
    ) -> Result<Value, EvalError>;
    /// Evaluate `COUNT { body }` under `scope`.
    fn count(
        &self,
        body: &crate::stmt::SubqueryBody,
        scope: &Scope<'_>,
    ) -> Result<Value, EvalError>;
    /// Evaluate a pattern comprehension under `scope`.
    fn pattern_comp(
        &self,
        path: &crate::stmt::PathPattern,
        filter: Option<&Expr>,
        map: &Expr,
        scope: &Scope<'_>,
    ) -> Result<Value, EvalError>;
    /// The node with the given id (backs `startNode`/`endNode`, which must yield
    /// the whole node so a chained `.prop` works, not a bare id).
    fn node_by_id(&self, id: u64) -> Result<Value, EvalError>;
    /// Whether entity `id` (a node when `is_rel` is false, else a relationship)
    /// was DELETEd earlier in the running statement — a subsequent property/label
    /// read on it must raise [`EvalError::DeletedEntity`]. Node and relationship
    /// id spaces are independent, hence `is_rel`. Defaults to `false` (no
    /// deletion tracking).
    fn is_deleted(&self, _id: u64, _is_rel: bool) -> bool {
        false
    }
}

/// Evaluate with optional graph hooks.
pub fn eval_with(
    expr: &Expr,
    scope: &Scope<'_>,
    hooks: Option<&dyn GraphHooks>,
) -> Result<Value, EvalError> {
    counted!("cypher.expressions evaluated");
    eval_inner(expr, scope, hooks)
}

fn eval_inner(
    expr: &Expr,
    scope: &Scope<'_>,
    hooks: Option<&dyn GraphHooks>,
) -> Result<Value, EvalError> {
    match expr {
        Expr::Null => Ok(Value::Null),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Int(i) => Ok(Value::Int(*i)),
        Expr::Float(f) => Ok(Value::Float(*f)),
        Expr::Str(s) => Ok(Value::Str(s.clone())),
        Expr::Param(p) => scope
            .params
            .get(p)
            .cloned()
            .ok_or_else(|| EvalError::UnknownParam(p.clone())),
        Expr::Var(v) => scope
            .var(v)
            .cloned()
            .ok_or_else(|| EvalError::UnknownVar(v.clone())),
        Expr::List(items) => Ok(Value::List(
            items
                .iter()
                .map(|e| eval_inner(e, scope, hooks))
                .collect::<Result<_, _>>()?,
        )),
        Expr::Map(entries) => {
            let mut m = BTreeMap::new();
            for (k, e) in entries {
                m.insert(k.clone(), eval_inner(e, scope, hooks)?);
            }
            Ok(Value::Map(m))
        }
        Expr::Prop(of, key) => match eval_inner(of, scope, hooks)? {
            Value::Null => Ok(Value::Null),
            Value::Map(m) => Ok(m.get(key).cloned().unwrap_or(Value::Null)),
            v @ (Value::Node { .. } | Value::Rel { .. }) => {
                let (id, props, is_rel) = match &v {
                    Value::Node { id, props, .. } => (*id, props, false),
                    Value::Rel { id, props, .. } => (*id, props, true),
                    _ => unreachable!(),
                };
                if hooks.is_some_and(|h| h.is_deleted(id, is_rel)) {
                    return Err(EvalError::DeletedEntity);
                }
                Ok(props.get(key).cloned().unwrap_or(Value::Null))
            }
            t @ (Value::Date(_)
            | Value::Time { .. }
            | Value::LocalTime(_)
            | Value::DateTime { .. }
            | Value::LocalDateTime { .. }
            | Value::Duration { .. }) => temporal_component(&t, key),
            other => Err(EvalError::Type {
                what: format!("property access `.{key}`"),
                got: other.type_name().to_string(),
            }),
        },
        Expr::Index(of, idx) => {
            let of = eval_inner(of, scope, hooks)?;
            let idx = eval_inner(idx, scope, hooks)?;
            match (&of, &idx) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::List(items), Value::Int(i)) => {
                    let n = items.len() as i64;
                    let i = if *i < 0 { *i + n } else { *i };
                    if (0..n).contains(&i) {
                        Ok(items[i as usize].clone())
                    } else {
                        Ok(Value::Null)
                    }
                }
                (Value::Map(m), Value::Str(k)) => Ok(m.get(k).cloned().unwrap_or(Value::Null)),
                // Dynamic property access: `n['prop']` on a node/relationship.
                (Value::Node { props, .. } | Value::Rel { props, .. }, Value::Str(k)) => {
                    Ok(props.get(k).cloned().unwrap_or(Value::Null))
                }
                _ => Err(EvalError::Type {
                    what: "indexing".to_string(),
                    got: format!("{}[{}]", of.type_name(), idx.type_name()),
                }),
            }
        }
        Expr::Slice { of, from, to } => {
            let of = eval_inner(of, scope, hooks)?;
            let from = from
                .as_ref()
                .map(|e| eval_inner(e, scope, hooks))
                .transpose()?;
            let to = to
                .as_ref()
                .map(|e| eval_inner(e, scope, hooks))
                .transpose()?;
            slice(of, from, to)
        }
        Expr::Bin(op, l, r) => bin(
            *op,
            eval_inner(l, scope, hooks)?,
            eval_inner(r, scope, hooks)?,
        ),
        // Fix 53: a GRAPH-DEPENDENT right operand (a subquery, a pattern
        // predicate, a pattern comprehension) is evaluated only when the
        // left side has not already decided the connective — `false AND
        // EXISTS {…}` is false and `true OR EXISTS {…}` is true without
        // running the body, the SelectOrSemiApply Neo4j plans for the same
        // shape. Only such an operand is skipped: a scalar one still
        // evaluates, so `false AND 1` raises exactly as it always did. The
        // planner orders the cheap operands first (`subqueries_last`), so
        // the body is on the right whenever it can be. The viewer-visibility
        // listings (`scope-test OR owner-test OR EXISTS {…} OR EXISTS {…}`)
        // ran both membership bodies for every row the first disjunct had
        // already admitted.
        Expr::And(l, r) => {
            let lt = truth_of(&eval_inner(l, scope, hooks)?, "AND")?;
            if lt == Truth::False && r.has_subquery() {
                counted!("cypher.subquery operand skipped by a decided connective");
                Ok(Truth::False.to_value())
            } else {
                Ok(lt.and(truth_of(&eval_inner(r, scope, hooks)?, "AND")?).to_value())
            }
        }
        Expr::Or(l, r) => {
            let lt = truth_of(&eval_inner(l, scope, hooks)?, "OR")?;
            if lt == Truth::True && r.has_subquery() {
                counted!("cypher.subquery operand skipped by a decided connective");
                Ok(Truth::True.to_value())
            } else {
                Ok(lt.or(truth_of(&eval_inner(r, scope, hooks)?, "OR")?).to_value())
            }
        }
        Expr::Xor(l, r) => Ok(truth_of(&eval_inner(l, scope, hooks)?, "XOR")?
            .xor(truth_of(&eval_inner(r, scope, hooks)?, "XOR")?)
            .to_value()),
        Expr::Not(e) => Ok((!truth_of(&eval_inner(e, scope, hooks)?, "NOT")?).to_value()),
        Expr::Neg(e) => match eval_inner(e, scope, hooks)? {
            Value::Null => Ok(Value::Null),
            Value::Int(i) => i
                .checked_neg()
                .map(Value::Int)
                .ok_or(EvalError::Overflow("negation")),
            Value::Float(f) => Ok(Value::Float(-f)),
            Value::Duration {
                months,
                days,
                seconds,
                nanos,
            } => Ok(Value::Duration {
                months: -months,
                days: -days,
                seconds: -seconds,
                nanos: -nanos,
            }),
            other => Err(EvalError::Type {
                what: "unary minus".to_string(),
                got: other.type_name().to_string(),
            }),
        },
        Expr::IsNull { of, negated } => {
            // The one operator that looks AT null instead of through it.
            let is_null = matches!(eval_inner(of, scope, hooks)?, Value::Null);
            Ok(Value::Bool(is_null != *negated))
        }
        Expr::In(l, r) => {
            let needle = eval_inner(l, scope, hooks)?;
            match eval_inner(r, scope, hooks)? {
                Value::Null => Ok(Value::Null),
                Value::List(items) => {
                    // openCypher: any True wins; else any Unknown → null;
                    // else false. A null needle against a nonempty list is
                    // Unknown per element, so null — and against [] is false.
                    let mut saw_unknown = false;
                    for item in &items {
                        match needle.eq3(item) {
                            Truth::True => return Ok(Value::Bool(true)),
                            Truth::Unknown => saw_unknown = true,
                            Truth::False => {}
                        }
                    }
                    if saw_unknown {
                        sometimes!("cypher.null propagated through a predicate", true);
                        Ok(Value::Null)
                    } else {
                        Ok(Value::Bool(false))
                    }
                }
                other => Err(EvalError::Type {
                    what: "IN".to_string(),
                    got: format!("right side is {}", other.type_name()),
                }),
            }
        }
        Expr::Call {
            name,
            distinct: _,
            args,
            star,
        } => {
            if AGGREGATES.contains(&name.as_str()) || *star {
                return Err(EvalError::AggregateInScalarContext(name.clone()));
            }
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(eval_inner(a, scope, hooks)?);
            }
            // startNode/endNode yield the whole node (so `.prop` chains work),
            // which needs the graph — resolve via hooks when present.
            if matches!(name.as_str(), "startnode" | "endnode") {
                match (hooks, vals.first()) {
                    (_, Some(Value::Null)) => return Ok(Value::Null),
                    (Some(h), Some(Value::Rel { src, dst, .. })) => {
                        let id = if name == "startnode" { *src } else { *dst };
                        return h.node_by_id(id);
                    }
                    _ => {}
                }
            }
            // A DATA read (labels/keys/properties) on an entity DELETEd earlier
            // in the statement raises DeletedEntityAccess. `id()`/`elementId()`
            // AND `type()` stay legal on a deleted entity (openCypher keeps the
            // relationship TYPE readable after DELETE) — all excluded here.
            if matches!(name.as_str(), "labels" | "keys" | "properties") {
                let deleted = match (hooks, vals.first()) {
                    (Some(h), Some(Value::Node { id, .. })) => h.is_deleted(*id, false),
                    (Some(h), Some(Value::Rel { id, .. })) => h.is_deleted(*id, true),
                    _ => false,
                };
                if deleted {
                    return Err(EvalError::DeletedEntity);
                }
            }
            call_function(name, vals, scope.now_ms, scope.zones.as_deref())
        }
        Expr::Case {
            subject,
            arms,
            otherwise,
        } => {
            let subject = subject
                .as_ref()
                .map(|e| eval_inner(e, scope, hooks))
                .transpose()?;
            for (when, then) in arms {
                let fired = match &subject {
                    // The simple form matches by = (three-valued; Unknown
                    // does not fire the arm).
                    Some(s) => s.eq3(&eval_inner(when, scope, hooks)?) == Truth::True,
                    None => truth_of(&eval_inner(when, scope, hooks)?, "CASE WHEN")? == Truth::True,
                };
                if fired {
                    return eval_inner(then, scope, hooks);
                }
            }
            match otherwise {
                Some(e) => eval_inner(e, scope, hooks),
                None => Ok(Value::Null),
            }
        }
        Expr::ListComp {
            var,
            source,
            filter,
            map,
        } => {
            let source = match eval_inner(source, scope, hooks)? {
                Value::Null => return Ok(Value::Null),
                Value::List(items) => items,
                other => {
                    return Err(EvalError::Type {
                        what: "list comprehension source".to_string(),
                        got: other.type_name().to_string(),
                    });
                }
            };
            let mut out = Vec::new();
            let mut inner = scope.child();
            for item in source {
                inner.bind(var, item.clone());
                if let Some(f) = filter {
                    if truth_of(&eval_inner(f, &inner, hooks)?, "comprehension WHERE")?
                        != Truth::True
                    {
                        continue;
                    }
                }
                out.push(match map {
                    Some(m) => eval_inner(m, &inner, hooks)?,
                    None => item,
                });
            }
            Ok(Value::List(out))
        }
        Expr::HasLabels { of, labels } => match eval_inner(of, scope, hooks)? {
            Value::Null => Ok(Value::Null),
            Value::Node { labels: have, .. } => {
                Ok(Value::Bool(labels.iter().all(|l| have.contains(l))))
            }
            // `r:T` on a relationship tests its single TYPE (openCypher shares the
            // `:X` syntax); a rel can match only a one-element, equal predicate.
            Value::Rel { rel_type, .. } => Ok(Value::Bool(labels.iter().all(|l| *l == rel_type))),
            other => Err(EvalError::Type {
                what: "a label predicate".to_string(),
                got: other.type_name().to_string(),
            }),
        },
        Expr::PatternPredicate(path) => match hooks {
            Some(h) => h.exists(
                &crate::stmt::SubqueryBody::Pattern {
                    pattern: crate::stmt::Pattern {
                        paths: vec![(**path).clone()],
                    },
                    where_: None,
                },
                scope,
            ),
            None => Err(EvalError::GraphDependent("a pattern predicate")),
        },
        Expr::ListPredicate {
            kind,
            var,
            source,
            filter,
        } => {
            use crate::ast::ListPredicateKind as K;
            let items = match eval_inner(source, scope, hooks)? {
                Value::Null => return Ok(Value::Null),
                Value::List(items) => items,
                other => {
                    return Err(EvalError::Type {
                        what: "a list predicate".to_string(),
                        got: other.type_name().to_string(),
                    });
                }
            };
            let mut inner = scope.child();
            let (mut trues, mut unknowns, mut falses) = (0usize, 0usize, 0usize);
            for item in items {
                inner.bind(var, item);
                match truth_of(&eval_inner(filter, &inner, hooks)?, "a list predicate")? {
                    Truth::True => trues += 1,
                    Truth::Unknown => unknowns += 1,
                    Truth::False => falses += 1,
                }
            }
            // openCypher's three-valued quantifiers.
            Ok(match kind {
                K::Any => {
                    if trues > 0 {
                        Value::Bool(true)
                    } else if unknowns > 0 {
                        Value::Null
                    } else {
                        Value::Bool(false)
                    }
                }
                K::All => {
                    if falses > 0 {
                        Value::Bool(false)
                    } else if unknowns > 0 {
                        Value::Null
                    } else {
                        Value::Bool(true)
                    }
                }
                K::None => {
                    if trues > 0 {
                        Value::Bool(false)
                    } else if unknowns > 0 {
                        Value::Null
                    } else {
                        Value::Bool(true)
                    }
                }
                K::Single => {
                    // Two definite matches already make `single` FALSE — no unknown
                    // can change that. Only when trues ∈ {0,1} can an unknown tip
                    // the count, leaving the result NULL.
                    if trues > 1 {
                        Value::Bool(false)
                    } else if unknowns > 0 {
                        Value::Null
                    } else {
                        Value::Bool(trues == 1)
                    }
                }
            })
        }
        Expr::MapProjection { of, items } => {
            use crate::ast::MapProjectionItem as Item;
            let base = eval_inner(of, scope, hooks)?;
            let props: BTreeMap<String, Value> = match &base {
                Value::Null => return Ok(Value::Null),
                Value::Node { props, .. } | Value::Rel { props, .. } => props.clone(),
                Value::Map(m) => m.clone(),
                other => {
                    return Err(EvalError::Type {
                        what: "a map projection".to_string(),
                        got: other.type_name().to_string(),
                    });
                }
            };
            let mut out = BTreeMap::new();
            for item in items {
                match item {
                    Item::AllProperties => {
                        for (k, v) in &props {
                            out.insert(k.clone(), v.clone());
                        }
                    }
                    Item::Property(k) => {
                        out.insert(k.clone(), props.get(k).cloned().unwrap_or(Value::Null));
                    }
                    Item::Entry(k, e) => {
                        out.insert(k.clone(), eval_inner(e, scope, hooks)?);
                    }
                    Item::Variable(v) => {
                        let val = scope
                            .var(v)
                            .cloned()
                            .ok_or_else(|| EvalError::UnknownVar(v.clone()))?;
                        out.insert(v.clone(), val);
                    }
                }
            }
            Ok(Value::Map(out))
        }
        Expr::ExistsSub(body) => match hooks {
            Some(h) => h.exists(body, scope),
            None => Err(EvalError::GraphDependent("EXISTS {}")),
        },
        Expr::CountSub(body) => match hooks {
            Some(h) => h.count(body, scope),
            None => Err(EvalError::GraphDependent("COUNT {}")),
        },
        Expr::PatternComp { path, filter, map } => match hooks {
            Some(h) => h.pattern_comp(path, filter.as_deref(), map, scope),
            None => Err(EvalError::GraphDependent("a pattern comprehension")),
        },
        Expr::Reduce {
            acc,
            init,
            var,
            source,
            step,
        } => {
            let source = match eval_inner(source, scope, hooks)? {
                Value::Null => return Ok(Value::Null),
                Value::List(items) => items,
                other => {
                    return Err(EvalError::Type {
                        what: "reduce source".to_string(),
                        got: other.type_name().to_string(),
                    });
                }
            };
            let mut inner = scope.child();
            let mut accum = eval_inner(init, scope, hooks)?;
            for item in source {
                inner.bind(acc, accum);
                inner.bind(var, item);
                accum = eval_inner(step, &inner, hooks)?;
            }
            Ok(accum)
        }
    }
}

fn truth_of(v: &Value, what: &str) -> Result<Truth, EvalError> {
    v.truth().ok_or_else(|| EvalError::Type {
        what: what.to_string(),
        got: v.type_name().to_string(),
    })
}

fn slice(of: Value, from: Option<Value>, to: Option<Value>) -> Result<Value, EvalError> {
    let items = match of {
        Value::Null => return Ok(Value::Null),
        Value::List(items) => items,
        other => {
            return Err(EvalError::Type {
                what: "slice".to_string(),
                got: other.type_name().to_string(),
            });
        }
    };
    let n = items.len() as i64;
    let clamp = |v: Option<Value>, default: i64| -> Result<Option<i64>, EvalError> {
        match v {
            None => Ok(Some(default)),
            Some(Value::Null) => Ok(None),
            Some(Value::Int(i)) => {
                let i = if i < 0 { i + n } else { i };
                Ok(Some(i.clamp(0, n)))
            }
            Some(other) => Err(EvalError::Type {
                what: "slice bound".to_string(),
                got: other.type_name().to_string(),
            }),
        }
    };
    let (Some(lo), Some(hi)) = (clamp(from, 0)?, clamp(to, n)?) else {
        // A null bound nulls the slice — openCypher.
        return Ok(Value::Null);
    };
    if lo >= hi {
        return Ok(Value::List(Vec::new()));
    }
    Ok(Value::List(items[lo as usize..hi as usize].to_vec()))
}

fn bin(op: BinOp, l: Value, r: Value) -> Result<Value, EvalError> {
    use BinOp::*;
    // NaN is UNORDERED: `<`, `<=`, `>`, `>=` between a NaN and another NUMBER are
    // all FALSE (IEEE / openCypher), not the boolean negation a total order would
    // give — `NaN >= 1` must not become `!(NaN < 1)` = true. A NaN against a
    // NON-number stays a cross-type comparison (→ null), so both sides must be
    // numeric for the NaN rule to apply.
    let numeric = |v: &Value| matches!(v, Value::Int(_) | Value::Float(_));
    let nan = numeric(&l)
        && numeric(&r)
        && (matches!(l, Value::Float(f) if f.is_nan())
            || matches!(r, Value::Float(f) if f.is_nan()));
    // Comparisons first: they have their own null story.
    match op {
        Eq => return Ok(l.eq3(&r).to_value()),
        Neq => return Ok((!l.eq3(&r)).to_value()),
        Lt | Ge | Gt | Le if nan => return Ok(Value::Bool(false)),
        Lt => return Ok(l.lt3(&r).to_value()),
        Ge => return Ok((!l.lt3(&r)).to_value()),
        Gt => return Ok(r.lt3(&l).to_value()),
        Le => return Ok((!r.lt3(&l)).to_value()),
        Regex => {
            if matches!(l, Value::Null) || matches!(r, Value::Null) {
                return Ok(Value::Null);
            }
            return Err(EvalError::RegexUnsupported);
        }
        _ => {}
    }
    // Everything else propagates null.
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    if is_temporal(&l) || is_temporal(&r) {
        return temporal_bin(op, l, r);
    }
    match op {
        Add => match (l, r) {
            (Value::Int(a), Value::Int(b)) => a
                .checked_add(b)
                .map(Value::Int)
                .ok_or(EvalError::Overflow("+")),
            (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a + b)),
            (Value::Int(a), Value::Float(b)) => Ok(Value::Float(a as f64 + b)),
            (Value::Float(a), Value::Int(b)) => Ok(Value::Float(a + b as f64)),
            (Value::Str(a), Value::Str(b)) => Ok(Value::Str(a + &b)),
            (Value::Str(a), Value::Int(b)) => Ok(Value::Str(format!("{a}{b}"))),
            (Value::Str(a), Value::Float(b)) => Ok(Value::Str(format!("{a}{b}"))),
            (Value::Int(a), Value::Str(b)) => Ok(Value::Str(format!("{a}{b}"))),
            (Value::Float(a), Value::Str(b)) => Ok(Value::Str(format!("{a}{b}"))),
            (Value::List(mut a), Value::List(b)) => {
                a.extend(b);
                Ok(Value::List(a))
            }
            (Value::List(mut a), b) => {
                a.push(b);
                Ok(Value::List(a))
            }
            // scalar + list prepends (openCypher list concatenation is symmetric:
            // `x + [a]` = `[x, a]`), mirroring the list-append arm above.
            (b, Value::List(mut a)) => {
                a.insert(0, b);
                Ok(Value::List(a))
            }
            (a, b) => Err(EvalError::Type {
                what: "+".to_string(),
                got: format!("{} + {}", a.type_name(), b.type_name()),
            }),
        },
        Sub => num_op(l, r, "-", i64::checked_sub, |a, b| a - b),
        Mul => num_op(l, r, "*", i64::checked_mul, |a, b| a * b),
        Div => match (l, r) {
            (Value::Int(_), Value::Int(0)) => Err(EvalError::DivisionByZero),
            (Value::Int(a), Value::Int(b)) => a
                .checked_div(b)
                .map(Value::Int)
                .ok_or(EvalError::Overflow("/")),
            (a, b) => float_op(a, b, "/", |a, b| a / b),
        },
        Mod => match (l, r) {
            (Value::Int(_), Value::Int(0)) => Err(EvalError::DivisionByZero),
            (Value::Int(a), Value::Int(b)) => a
                .checked_rem(b)
                .map(Value::Int)
                .ok_or(EvalError::Overflow("%")),
            (a, b) => float_op(a, b, "%", |a, b| a % b),
        },
        Pow => float_op(l, r, "^", f64::powf),
        StartsWith => str_op(l, r, "STARTS WITH", |a, b| a.starts_with(b)),
        EndsWith => str_op(l, r, "ENDS WITH", |a, b| a.ends_with(b)),
        Contains => str_op(l, r, "CONTAINS", |a, b| a.contains(b)),
        Eq | Neq | Lt | Le | Gt | Ge | Regex => unreachable!("handled above"),
    }
}

fn num_op(
    l: Value,
    r: Value,
    name: &'static str,
    int: impl Fn(i64, i64) -> Option<i64>,
    float: impl Fn(f64, f64) -> f64,
) -> Result<Value, EvalError> {
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => {
            int(a, b).map(Value::Int).ok_or(EvalError::Overflow(name))
        }
        (a, b) => float_op(a, b, name, float),
    }
}

fn float_op(
    l: Value,
    r: Value,
    name: &'static str,
    f: impl Fn(f64, f64) -> f64,
) -> Result<Value, EvalError> {
    let as_f = |v: &Value| match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(x) => Some(*x),
        _ => None,
    };
    match (as_f(&l), as_f(&r)) {
        (Some(a), Some(b)) => Ok(Value::Float(f(a, b))),
        _ => Err(EvalError::Type {
            what: name.to_string(),
            got: format!("{} {name} {}", l.type_name(), r.type_name()),
        }),
    }
}

fn str_op(
    l: Value,
    r: Value,
    name: &'static str,
    f: impl Fn(&str, &str) -> bool,
) -> Result<Value, EvalError> {
    match (l, r) {
        (Value::Str(a), Value::Str(b)) => Ok(Value::Bool(f(&a, &b))),
        // A non-string operand yields NULL (openCypher), not a type error — null
        // operands were already short-circuited by the caller. `let _ = name;`
        // keeps the label for callers that still format it.
        _ => {
            let _ = name;
            Ok(Value::Null)
        }
    }
}

// ─── The function registry ──────────────────────────────────────────────────

thread_local! {
    /// Per-thread xorshift state for `rand()`. Cypher's `rand()` is a genuine
    /// non-deterministic source; a fixed seed just makes a fresh thread's first
    /// draw reproducible. No benchmark / determinism query calls it, so the
    /// two-process digest is unaffected.
    static RAND_STATE: std::cell::Cell<u64> = const { std::cell::Cell::new(0x2545_F491_4F6C_DD1D) };
}

/// A uniform pseudo-random `f64` in `[0, 1)`.
fn next_rand() -> f64 {
    RAND_STATE.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        (x >> 11) as f64 / (1u64 << 53) as f64
    })
}

fn arity(name: &str, args: &[Value], n: usize) -> Result<(), EvalError> {
    if args.len() != n {
        return Err(EvalError::Function {
            name: name.to_string(),
            detail: format!("takes {n} argument(s), got {}", args.len()),
        });
    }
    Ok(())
}

/// Is `name` an AGGREGATE function (`count`/`collect`/`sum`/…)? Aggregates fold
/// across rows and are never per-element scalar calls — the columnar evaluator
/// uses this to keep them out of its single-var scalar-`Call` path.
pub fn is_aggregate_fn(name: &str) -> bool {
    AGGREGATES.contains(&name)
}

/// Is `name` a KNOWN scalar or aggregate function? A STATIC predicate so
/// `RETURN foo(x)` raises `UnknownFunction` at validation time even when no row
/// ever evaluates the call (an empty match). It probes the scalar registry with
/// no arguments: ONLY a genuinely unknown name yields `UnknownFunction` — a real
/// function returns an arity/type error (or a value), which still proves it is
/// registered. `now_ms` is pinned so a temporal builtin cannot fault on a
/// missing clock. The only builtin that mutates state is `rand` (it advances a
/// thread-local RNG); it is non-deterministic by contract and absent from the
/// determinism / benchmark corpora, so probing it does not move the digest.
pub fn is_known_function(name: &str) -> bool {
    is_aggregate_fn(name)
        || !matches!(
            call_function(name, Vec::new(), Some(0), None),
            Err(EvalError::UnknownFunction(_))
        )
}

/// Apply a SCALAR built-in to already-evaluated argument values — the per-element
/// seam the columnar evaluator (`eval_column`) uses to compute a scalar `Call`
/// over a column (e.g. `toInteger(x.prop)` as an ORDER BY key), reusing the EXACT
/// registry `eval_with` uses so the columnar and per-tuple paths stay
/// byte-identical. Refuses an aggregate (`count`/`collect`/…) — not a scalar fn.
pub fn apply_scalar_fn(
    name: &str,
    args: Vec<Value>,
    now_ms: Option<i64>,
    zones: Option<&dyn crate::temporal::ZoneProvider>,
) -> Result<Value, EvalError> {
    if AGGREGATES.contains(&name) {
        return Err(EvalError::AggregateInScalarContext(name.to_string()));
    }
    call_function(name, args, now_ms, zones)
}

/// The ordinal day of the Monday that starts the ISO week-year containing
/// `(y, mo, d)`. The ISO week-year is the year of that week's Thursday; its
/// week 1 always contains Jan 4, so the start is the Monday on/before Jan 4.
fn iso_weekyear_start(y: i64, mo: u32, d: u32) -> i64 {
    let days = crate::temporal::days_from_civil(y, mo, d);
    let thursday = days - (days + 3).rem_euclid(7) + 3;
    let iso_wy = crate::temporal::civil_from_days(thursday).0;
    let jan4 = crate::temporal::days_from_civil(iso_wy, 1, 4);
    jan4 - (jan4 + 3).rem_euclid(7)
}

/// The LOCAL `(year, month, day)` of a temporal value — a date's own date, or a
/// datetime's date read in its own offset (a named-zone datetime uses its stored
/// offset). `None` for a value that carries no date (a time).
fn local_date_of(v: &Value) -> Option<(i64, u32, u32)> {
    use crate::temporal::civil_from_days;
    match v {
        Value::Date(days) => Some(civil_from_days(*days)),
        Value::LocalDateTime { epoch_seconds, .. } => {
            Some(civil_from_days(epoch_seconds.div_euclid(86_400)))
        }
        Value::DateTime {
            epoch_seconds,
            offset_seconds,
            ..
        } => {
            let local = epoch_seconds + *offset_seconds as i64;
            Some(civil_from_days(local.div_euclid(86_400)))
        }
        _ => None,
    }
}

/// The full LOCAL wall-clock breakdown of a temporal, for truncation.
struct DtParts {
    y: i64,
    mo: u32,
    d: u32,
    h: u32,
    mi: u32,
    s: u32,
    nanos: u32,
    offset: i32,
    zone: Option<String>,
}

/// Decompose a `Date`/`DateTime`/`LocalDateTime` into local wall-clock parts.
fn local_datetime_parts(v: &Value) -> Option<DtParts> {
    let (local, nanos, offset, zone) = match v {
        Value::DateTime {
            epoch_seconds,
            nanos,
            offset_seconds,
            zone,
        } => (
            epoch_seconds + *offset_seconds as i64,
            *nanos,
            *offset_seconds,
            zone.clone(),
        ),
        Value::LocalDateTime {
            epoch_seconds,
            nanos,
        } => (*epoch_seconds, *nanos, 0, None),
        Value::Date(days) => (days * 86_400, 0, 0, None),
        _ => return None,
    };
    let days = local.div_euclid(86_400);
    let tod = local.rem_euclid(86_400);
    let (y, mo, d) = crate::temporal::civil_from_days(days);
    Some(DtParts {
        y,
        mo,
        d,
        h: (tod / 3600) as u32,
        mi: ((tod % 3600) / 60) as u32,
        s: (tod % 60) as u32,
        nanos,
        offset,
        zone,
    })
}

/// Truncate `p` in place to `unit` (date units zero the time; time units keep it).
fn truncate_dt(unit: &str, p: &mut DtParts) -> Result<(), String> {
    let zero_time = |p: &mut DtParts| {
        p.h = 0;
        p.mi = 0;
        p.s = 0;
        p.nanos = 0;
    };
    match unit {
        "millennium" => {
            p.y = p.y.div_euclid(1000) * 1000;
            p.mo = 1;
            p.d = 1;
            zero_time(p);
        }
        "century" => {
            p.y = p.y.div_euclid(100) * 100;
            p.mo = 1;
            p.d = 1;
            zero_time(p);
        }
        "decade" => {
            p.y = p.y.div_euclid(10) * 10;
            p.mo = 1;
            p.d = 1;
            zero_time(p);
        }
        "year" => {
            p.mo = 1;
            p.d = 1;
            zero_time(p);
        }
        "quarter" => {
            p.mo = ((p.mo - 1) / 3) * 3 + 1;
            p.d = 1;
            zero_time(p);
        }
        "month" => {
            p.d = 1;
            zero_time(p);
        }
        "week" => {
            let days = crate::temporal::days_from_civil(p.y, p.mo, p.d);
            let dow = (days + 3).rem_euclid(7);
            let (y, m, d) = crate::temporal::civil_from_days(days - dow);
            p.y = y;
            p.mo = m;
            p.d = d;
            zero_time(p);
        }
        "weekyear" => {
            let (y, m, d) = crate::temporal::civil_from_days(iso_weekyear_start(p.y, p.mo, p.d));
            p.y = y;
            p.mo = m;
            p.d = d;
            zero_time(p);
        }
        "day" => zero_time(p),
        "hour" => {
            p.mi = 0;
            p.s = 0;
            p.nanos = 0;
        }
        "minute" => {
            p.s = 0;
            p.nanos = 0;
        }
        "second" => p.nanos = 0,
        "millisecond" => p.nanos = p.nanos / 1_000_000 * 1_000_000,
        "microsecond" => p.nanos = p.nanos / 1_000 * 1_000,
        other => return Err(format!("unknown truncation unit `{other}`")),
    }
    Ok(())
}

/// The local wall-clock seconds of the (y..s) parts.
fn dt_local_seconds(p: &DtParts) -> i64 {
    crate::temporal::days_from_civil(p.y, p.mo, p.d) * 86_400
        + p.h as i64 * 3600
        + p.mi as i64 * 60
        + p.s as i64
}

/// The LOCAL time-of-day (nanoseconds since midnight) and offset of a temporal.
fn time_of_day_of(v: &Value) -> Option<(i64, i32)> {
    match v {
        Value::Time {
            nanos,
            offset_seconds,
        } => Some((*nanos, *offset_seconds)),
        Value::LocalTime(nanos) => Some((*nanos, 0)),
        Value::DateTime {
            epoch_seconds,
            nanos,
            offset_seconds,
            ..
        } => {
            let local = epoch_seconds + *offset_seconds as i64;
            Some((
                local.rem_euclid(86_400) * 1_000_000_000 + *nanos as i64,
                *offset_seconds,
            ))
        }
        Value::LocalDateTime {
            epoch_seconds,
            nanos,
        } => Some((
            epoch_seconds.rem_euclid(86_400) * 1_000_000_000 + *nanos as i64,
            0,
        )),
        Value::Date(_) => Some((0, 0)),
        _ => None,
    }
}

/// Truncate a time-of-day (nanos since midnight) to `unit` — date units go to
/// midnight; time units floor within the day.
fn truncate_tod(unit: &str, nanos: i64) -> Result<i64, String> {
    Ok(match unit {
        "millennium" | "century" | "decade" | "year" | "quarter" | "month" | "week" | "day" => 0,
        "hour" => nanos / 3_600_000_000_000 * 3_600_000_000_000,
        "minute" => nanos / 60_000_000_000 * 60_000_000_000,
        "second" => nanos / 1_000_000_000 * 1_000_000_000,
        "millisecond" => nanos / 1_000_000 * 1_000_000,
        "microsecond" => nanos / 1_000 * 1_000,
        other => return Err(format!("unknown truncation unit `{other}`")),
    })
}

const NANOS_PER_DAY: i128 = 86_400 * 1_000_000_000;

/// A temporal decomposed for `duration.between`: whether it carries a date, its
/// local ordinal day, its local wall-clock time-of-day (ns since midnight), and
/// its offset (if any).
struct TParts {
    has_date: bool,
    days: i64,
    tod: i128,
    has_offset: bool,
    offset: i64,
    /// The NAMED zone (a zoned `datetime` only) — lent to a local operand so a
    /// duration ACROSS a DST boundary counts the real elapsed time.
    zone: Option<String>,
}

fn tparts(v: &Value) -> Option<TParts> {
    Some(match v {
        Value::Date(d) => TParts {
            has_date: true,
            days: *d,
            tod: 0,
            has_offset: false,
            offset: 0,
            zone: None,
        },
        Value::LocalDateTime {
            epoch_seconds,
            nanos,
        } => TParts {
            has_date: true,
            days: epoch_seconds.div_euclid(86_400),
            tod: epoch_seconds.rem_euclid(86_400) as i128 * 1_000_000_000 + *nanos as i128,
            has_offset: false,
            offset: 0,
            zone: None,
        },
        Value::DateTime {
            epoch_seconds,
            nanos,
            offset_seconds,
            zone,
        } => {
            let local = epoch_seconds + *offset_seconds as i64;
            TParts {
                has_date: true,
                days: local.div_euclid(86_400),
                tod: local.rem_euclid(86_400) as i128 * 1_000_000_000 + *nanos as i128,
                has_offset: true,
                offset: *offset_seconds as i64,
                zone: zone.clone(),
            }
        }
        Value::LocalTime(ns) => TParts {
            has_date: false,
            days: 0,
            tod: *ns as i128,
            has_offset: false,
            offset: 0,
            zone: None,
        },
        Value::Time {
            nanos,
            offset_seconds,
        } => TParts {
            has_date: false,
            days: 0,
            tod: *nanos as i128,
            has_offset: true,
            offset: *offset_seconds as i64,
            zone: None,
        },
        _ => return None,
    })
}

/// Cross-type zone borrow for `duration.between`: a LOCAL operand (`target`) with
/// no offset takes the OTHER operand's NAMED zone, resolved at the target's own
/// wall clock (borrowing the other's date when the target has none). This makes a
/// duration between a zoned datetime and a local value count the true elapsed
/// seconds across a DST transition.
fn borrow_zone(
    target: &mut TParts,
    other: &TParts,
    zones: Option<&dyn crate::temporal::ZoneProvider>,
) {
    if target.has_offset {
        return;
    }
    let Some(z) = &other.zone else { return };
    let date = if target.has_date {
        target.days
    } else {
        other.days
    };
    let local_secs = date * 86_400 + (target.tod / 1_000_000_000) as i64;
    if let Some(off) = resolve_zone(z, local_secs, zones) {
        target.offset = i64::from(off);
        target.has_offset = true;
    }
}

/// The shared frame for a `duration.between`: the date difference is counted only
/// when BOTH sides have a date; the offset is applied to the time-of-day whenever
/// BOTH sides carry one — including a pure time-to-time difference, where the zones
/// still matter (`time('14:30')` vs `time('16:30+0100')` is 1h, not 2h).
fn between_components(a: &TParts, b: &TParts) -> (bool, i128, i128) {
    let use_date = a.has_date && b.has_date;
    let apply_off = a.has_offset && b.has_offset;
    let adj = |t: &TParts| {
        t.tod
            - if apply_off {
                t.offset as i128 * 1_000_000_000
            } else {
                0
            }
    };
    (use_date, adj(a), adj(b))
}

/// `(y, mo, d)` advanced by `months`, the day clamped to the target month length.
fn add_months(y: i64, mo: u32, d: u32, months: i64) -> i64 {
    use crate::temporal::days_from_civil;
    let total = y * 12 + (mo as i64 - 1) + months;
    let ny = total.div_euclid(12);
    let nm = (total.rem_euclid(12) + 1) as u32;
    let first = days_from_civil(ny, nm, 1);
    let next = if nm == 12 {
        days_from_civil(ny + 1, 1, 1)
    } else {
        days_from_civil(ny, nm + 1, 1)
    };
    let mlen = (next - first) as u32;
    days_from_civil(ny, nm, d.min(mlen))
}

/// The raw difference `b - a` in nanoseconds, date folded to 86_400 s/day. Used
/// by `inSeconds`/`inDays`, which express the whole span in one fixed-length
/// unit (months, being variable-length, never appear here).
fn duration_total_ns(a: &TParts, b: &TParts) -> i128 {
    let (use_date, toa, tob) = between_components(a, b);
    let date_ns = if use_date {
        (b.days - a.days) as i128 * NANOS_PER_DAY
    } else {
        0
    };
    date_ns + (tob - toa)
}

/// The full `(months, days, seconds, nanos)` breakdown between two temporals:
/// calendar months first, then whole days, then the sub-day remainder.
fn duration_between(a: &TParts, b: &TParts) -> (i64, i64, i64, i32) {
    use crate::temporal::civil_from_days;
    let (use_date, toa, tob) = between_components(a, b);
    if !use_date {
        let total = tob - toa;
        return (
            0,
            0,
            total.div_euclid(1_000_000_000) as i64,
            total.rem_euclid(1_000_000_000) as i32,
        );
    }
    let (ay, am, ad) = civil_from_days(a.days);
    let (by, bm, bd) = civil_from_days(b.days);
    let mut months = (by - ay) * 12 + (bm as i64 - am as i64);
    let a_rem = (ad as i64, toa);
    let b_rem = (bd as i64, tob);
    if months > 0 && b_rem < a_rem {
        months -= 1;
    } else if months < 0 && b_rem > a_rem {
        months += 1;
    }
    let a2 = add_months(ay, am, ad, months);
    let mut days = b.days - a2;
    let mut time_rem = tob - toa;
    if time_rem < 0 && days > 0 {
        days -= 1;
        time_rem += NANOS_PER_DAY;
    } else if time_rem > 0 && days < 0 {
        days += 1;
        time_rem -= NANOS_PER_DAY;
    }
    (
        months,
        days,
        time_rem.div_euclid(1_000_000_000) as i64,
        time_rem.rem_euclid(1_000_000_000) as i32,
    )
}

fn call_function(
    name: &str,
    mut args: Vec<Value>,
    now_ms: Option<i64>,
    zones: Option<&dyn crate::temporal::ZoneProvider>,
) -> Result<Value, EvalError> {
    match name {
        // coalesce is the OTHER operator that looks at null.
        "coalesce" => Ok(args
            .into_iter()
            .find(|v| !matches!(v, Value::Null))
            .unwrap_or(Value::Null)),
        "size" => {
            arity(name, &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
                Value::List(l) => Ok(Value::Int(l.len() as i64)),
                other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes a string or list, got {}", other.type_name()),
                }),
            }
        }
        "length" => {
            arity(name, &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                // A PATH is a trail `[node, rel, node, …]`; its length is the
                // number of RELATIONSHIPS it contains (LDBC IC1/IC13's
                // `length(shortestPath(...))`), not its element count (`size`).
                // A bare list is accepted too (a rel-free list has length 0).
                Value::List(l) | Value::Path(l) => Ok(Value::Int(
                    l.iter().filter(|v| matches!(v, Value::Rel { .. })).count() as i64,
                )),
                other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes a path, got {}", other.type_name()),
                }),
            }
        }
        "nodes" | "relationships" => {
            arity(name, &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                // A PATH is a trail `[node, rel, node, …]`; `nodes` returns its
                // nodes in order, `relationships` its relationships.
                Value::List(l) | Value::Path(l) => {
                    let want_node = name == "nodes";
                    Ok(Value::List(
                        l.iter()
                            .filter(|v| match v {
                                Value::Node { .. } => want_node,
                                Value::Rel { .. } => !want_node,
                                _ => false,
                            })
                            .cloned()
                            .collect(),
                    ))
                }
                other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes a path, got {}", other.type_name()),
                }),
            }
        }
        "head" | "last" => {
            arity(name, &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::List(l) => Ok(match (name, l.first(), l.last()) {
                    ("head", Some(v), _) | ("last", _, Some(v)) => v.clone(),
                    _ => Value::Null,
                }),
                other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes a list, got {}", other.type_name()),
                }),
            }
        }
        "reverse" => {
            arity(name, &args, 1)?;
            match args.pop().expect("arity") {
                Value::Null => Ok(Value::Null),
                Value::List(mut l) => {
                    l.reverse();
                    Ok(Value::List(l))
                }
                Value::Str(s) => Ok(Value::Str(s.chars().rev().collect())),
                other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes a list or string, got {}", other.type_name()),
                }),
            }
        }
        "abs" => {
            arity(name, &args, 1)?;
            match args[0] {
                Value::Null => Ok(Value::Null),
                Value::Int(i) => i
                    .checked_abs()
                    .map(Value::Int)
                    .ok_or(EvalError::Overflow("abs")),
                Value::Float(f) => Ok(Value::Float(f.abs())),
                ref other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes a number, got {}", other.type_name()),
                }),
            }
        }
        // ── math: unary, → float ──────────────────────────────────────────
        "ceil" | "floor" | "round" | "sqrt" | "sin" | "cos" | "tan" | "asin" | "acos" | "atan"
        | "cot" | "exp" | "log" | "log10" | "degrees" | "radians" | "haversin" => {
            arity(name, &args, 1)?;
            let x = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Int(i) => *i as f64,
                Value::Float(f) => *f,
                other => {
                    return Err(EvalError::Function {
                        name: name.to_string(),
                        detail: format!("takes a number, got {}", other.type_name()),
                    });
                }
            };
            let y = match name {
                "ceil" => x.ceil(),
                "floor" => x.floor(),
                "round" => x.round(), // ties away from zero — Neo4j HALF_UP
                "sqrt" => x.sqrt(),
                "sin" => x.sin(),
                "cos" => x.cos(),
                "tan" => x.tan(),
                "asin" => x.asin(),
                "acos" => x.acos(),
                "atan" => x.atan(),
                "cot" => 1.0 / x.tan(),
                "exp" => x.exp(),
                "log" => x.ln(),
                "log10" => x.log10(),
                "degrees" => x.to_degrees(),
                "radians" => x.to_radians(),
                "haversin" => (1.0 - x.cos()) / 2.0,
                _ => unreachable!(),
            };
            Ok(Value::Float(y))
        }
        "sign" => {
            arity(name, &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Int(i) => Ok(Value::Int(i.signum())),
                Value::Float(f) => Ok(Value::Int(if *f > 0.0 {
                    1
                } else if *f < 0.0 {
                    -1
                } else {
                    0
                })),
                other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes a number, got {}", other.type_name()),
                }),
            }
        }
        "atan2" => {
            arity(name, &args, 2)?;
            let num = |v: &Value| match v {
                Value::Int(i) => Some(*i as f64),
                Value::Float(f) => Some(*f),
                _ => None,
            };
            match (&args[0], &args[1]) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (a, b) => match (num(a), num(b)) {
                    (Some(y), Some(x)) => Ok(Value::Float(y.atan2(x))),
                    _ => Err(EvalError::Function {
                        name: name.to_string(),
                        detail: "takes two numbers".to_string(),
                    }),
                },
            }
        }
        "e" => {
            arity(name, &args, 0)?;
            Ok(Value::Float(std::f64::consts::E))
        }
        "pi" => {
            arity(name, &args, 0)?;
            Ok(Value::Float(std::f64::consts::PI))
        }
        "rand" => {
            arity(name, &args, 0)?;
            Ok(Value::Float(next_rand()))
        }
        // ── string ────────────────────────────────────────────────────────
        "left" | "right" => {
            arity(name, &args, 2)?;
            match (&args[0], &args[1]) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Int(n)) if *n >= 0 => {
                    let chars: Vec<char> = s.chars().collect();
                    let n = (*n as usize).min(chars.len());
                    let out: String = if name == "left" {
                        chars[..n].iter().collect()
                    } else {
                        chars[chars.len() - n..].iter().collect()
                    };
                    Ok(Value::Str(out))
                }
                _ => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: "takes (string, non-negative integer)".to_string(),
                }),
            }
        }
        "substring" => {
            if args.len() != 2 && args.len() != 3 {
                return Err(EvalError::Function {
                    name: name.to_string(),
                    detail: "takes (string, start[, length])".to_string(),
                });
            }
            let s = match &args[0] {
                Value::Null => return Ok(Value::Null),
                Value::Str(s) => s,
                other => {
                    return Err(EvalError::Function {
                        name: name.to_string(),
                        detail: format!("takes a string, got {}", other.type_name()),
                    });
                }
            };
            let chars: Vec<char> = s.chars().collect();
            let start = match &args[1] {
                Value::Null => return Ok(Value::Null),
                Value::Int(i) if *i >= 0 => (*i as usize).min(chars.len()),
                _ => {
                    return Err(EvalError::Function {
                        name: name.to_string(),
                        detail: "start must be a non-negative integer".to_string(),
                    });
                }
            };
            let end = if args.len() == 3 {
                match &args[2] {
                    Value::Null => return Ok(Value::Null),
                    Value::Int(l) if *l >= 0 => (start + *l as usize).min(chars.len()),
                    _ => {
                        return Err(EvalError::Function {
                            name: name.to_string(),
                            detail: "length must be a non-negative integer".to_string(),
                        });
                    }
                }
            } else {
                chars.len()
            };
            Ok(Value::Str(chars[start..end].iter().collect()))
        }
        "ltrim" | "rtrim" => {
            arity(name, &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Str(s) => Ok(Value::Str(
                    if name == "ltrim" {
                        s.trim_start()
                    } else {
                        s.trim_end()
                    }
                    .to_string(),
                )),
                other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes a string, got {}", other.type_name()),
                }),
            }
        }
        "replace" => {
            arity(name, &args, 3)?;
            match (&args[0], &args[1], &args[2]) {
                (Value::Null, _, _) | (_, Value::Null, _) | (_, _, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Str(from), Value::Str(to)) => {
                    Ok(Value::Str(s.replace(from.as_str(), to)))
                }
                _ => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: "takes three strings".to_string(),
                }),
            }
        }
        // ── scalar conversions + list ─────────────────────────────────────
        "toboolean" | "tobooleanornull" => {
            arity(name, &args, 1)?;
            let or_null = name.ends_with("ornull");
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Bool(b) => Ok(Value::Bool(*b)),
                Value::Str(s) => Ok(match s.trim().to_lowercase().as_str() {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    _ => Value::Null,
                }),
                other if or_null => {
                    let _ = other;
                    Ok(Value::Null)
                }
                other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes a boolean or string, got {}", other.type_name()),
                }),
            }
        }
        "tointegerornull" => {
            arity(name, &args, 1)?;
            Ok(match &args[0] {
                Value::Int(i) => Value::Int(*i),
                Value::Float(f) => Value::Int(*f as i64),
                Value::Str(s) => s
                    .trim()
                    .parse::<i64>()
                    .map(Value::Int)
                    .unwrap_or(Value::Null),
                _ => Value::Null,
            })
        }
        "tofloatornull" => {
            arity(name, &args, 1)?;
            Ok(match &args[0] {
                Value::Int(i) => Value::Float(*i as f64),
                Value::Float(f) => Value::Float(*f),
                Value::Str(s) => s
                    .trim()
                    .parse::<f64>()
                    .map(Value::Float)
                    .unwrap_or(Value::Null),
                _ => Value::Null,
            })
        }
        "tostringornull" => {
            arity(name, &args, 1)?;
            Ok(match &args[0] {
                Value::Null => Value::Null,
                Value::Str(s) => Value::Str(s.clone()),
                Value::Int(i) => Value::Str(i.to_string()),
                Value::Float(f) => Value::Str(format!("{f:?}")),
                Value::Bool(b) => Value::Str(b.to_string()),
                _ => Value::Null,
            })
        }
        "tail" => {
            arity(name, &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::List(xs) => Ok(Value::List(xs.iter().skip(1).cloned().collect())),
                other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes a list, got {}", other.type_name()),
                }),
            }
        }
        "isempty" => {
            arity(name, &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::List(xs) => Ok(Value::Bool(xs.is_empty())),
                Value::Str(s) => Ok(Value::Bool(s.is_empty())),
                Value::Map(m) => Ok(Value::Bool(m.is_empty())),
                other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes a list, string or map, got {}", other.type_name()),
                }),
            }
        }
        "datetime.fromepoch" => {
            if args.len() != 2 {
                return Err(EvalError::Function {
                    name: name.to_string(),
                    detail: "takes (seconds, nanoseconds)".to_string(),
                });
            }
            if args.iter().any(|a| matches!(a, Value::Null)) {
                return Ok(Value::Null);
            }
            let (Value::Int(secs), Value::Int(ns)) = (&args[0], &args[1]) else {
                return Err(EvalError::Function {
                    name: name.to_string(),
                    detail: "seconds and nanoseconds must be integers".to_string(),
                });
            };
            let total = secs * 1_000_000_000 + ns;
            Ok(Value::DateTime {
                epoch_seconds: total.div_euclid(1_000_000_000),
                nanos: total.rem_euclid(1_000_000_000) as u32,
                offset_seconds: 0,
                zone: None,
            })
        }
        "datetime.fromepochmillis" => {
            if args.len() != 1 {
                return Err(EvalError::Function {
                    name: name.to_string(),
                    detail: "takes (milliseconds)".to_string(),
                });
            }
            if matches!(args[0], Value::Null) {
                return Ok(Value::Null);
            }
            let Value::Int(ms) = &args[0] else {
                return Err(EvalError::Function {
                    name: name.to_string(),
                    detail: "milliseconds must be an integer".to_string(),
                });
            };
            Ok(Value::DateTime {
                epoch_seconds: ms.div_euclid(1000),
                nanos: (ms.rem_euclid(1000) * 1_000_000) as u32,
                offset_seconds: 0,
                zone: None,
            })
        }
        "date.truncate" => {
            if args.len() != 2 && args.len() != 3 {
                return Err(EvalError::Function {
                    name: name.to_string(),
                    detail: "takes (unit, temporal[, map])".to_string(),
                });
            }
            if matches!(args[1], Value::Null) {
                return Ok(Value::Null);
            }
            let unit = match &args[0] {
                Value::Str(s) => s.to_lowercase(),
                other => {
                    return Err(EvalError::Function {
                        name: name.to_string(),
                        detail: format!("unit must be a string, got {}", other.type_name()),
                    });
                }
            };
            let Some((mut y, mut mo, mut d)) = local_date_of(&args[1]) else {
                return Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("cannot truncate a {} to a date", args[1].type_name()),
                });
            };
            match unit.as_str() {
                "millennium" => {
                    y = y.div_euclid(1000) * 1000;
                    mo = 1;
                    d = 1;
                }
                "century" => {
                    y = y.div_euclid(100) * 100;
                    mo = 1;
                    d = 1;
                }
                "decade" => {
                    y = y.div_euclid(10) * 10;
                    mo = 1;
                    d = 1;
                }
                "year" => {
                    mo = 1;
                    d = 1;
                }
                "quarter" => {
                    mo = ((mo - 1) / 3) * 3 + 1;
                    d = 1;
                }
                "month" => d = 1,
                "week" => {
                    // Monday of the ISO week. 1970-01-01 is a Thursday, so
                    // `(days + 3) mod 7` gives the day index with Monday = 0.
                    let days = crate::temporal::days_from_civil(y, mo, d);
                    let dow = (days + 3).rem_euclid(7);
                    let (yy, mm, dd) = crate::temporal::civil_from_days(days - dow);
                    y = yy;
                    mo = mm;
                    d = dd;
                }
                "weekyear" => {
                    let (yy, mm, dd) =
                        crate::temporal::civil_from_days(iso_weekyear_start(y, mo, d));
                    y = yy;
                    mo = mm;
                    d = dd;
                }
                "day" => {}
                other => {
                    return Err(EvalError::Function {
                        name: name.to_string(),
                        detail: format!("unknown truncation unit `{other}`"),
                    });
                }
            }
            let truncated = crate::temporal::days_from_civil(y, mo, d);
            // The override map replaces components after truncation — the same
            // selector-group grammar as construction, over the truncated base.
            match args.get(2) {
                Some(Value::Null) => Ok(Value::Null),
                Some(Value::Map(m)) => {
                    let mut merged = m.clone();
                    merged.insert("date".to_string(), Value::Date(truncated));
                    temporal_map_construct("date", &merged, zones, false)
                }
                _ => Ok(Value::Date(truncated)),
            }
        }
        "datetime.truncate" | "localdatetime.truncate" => {
            if args.len() != 2 && args.len() != 3 {
                return Err(EvalError::Function {
                    name: name.to_string(),
                    detail: "takes (unit, temporal[, map])".to_string(),
                });
            }
            if matches!(args[1], Value::Null) {
                return Ok(Value::Null);
            }
            let unit = match &args[0] {
                Value::Str(s) => s.to_lowercase(),
                other => {
                    return Err(EvalError::Function {
                        name: name.to_string(),
                        detail: format!("unit must be a string, got {}", other.type_name()),
                    });
                }
            };
            let Some(mut p) = local_datetime_parts(&args[1]) else {
                return Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("cannot truncate a {} to a datetime", args[1].type_name()),
                });
            };
            truncate_dt(&unit, &mut p).map_err(|detail| EvalError::Function {
                name: name.to_string(),
                detail,
            })?;
            let local = dt_local_seconds(&p);
            let truncated = if name == "datetime.truncate" {
                Value::DateTime {
                    epoch_seconds: local - p.offset as i64,
                    nanos: p.nanos,
                    offset_seconds: p.offset,
                    zone: p.zone,
                }
            } else {
                Value::LocalDateTime {
                    epoch_seconds: local,
                    nanos: p.nanos,
                }
            };
            // Overrides after truncation reuse the construction grammar over the
            // truncated value as a `datetime` base (handles the calendar selectors
            // and sub-second keys, not just y/m/d/h/m/s).
            match args.get(2) {
                Some(Value::Null) => Ok(Value::Null),
                Some(Value::Map(m)) => {
                    let mut merged = m.clone();
                    merged.insert("datetime".to_string(), truncated);
                    temporal_map_construct(
                        name.strip_suffix(".truncate").unwrap_or(name),
                        &merged,
                        zones,
                        false,
                    )
                }
                _ => Ok(truncated),
            }
        }
        "time.truncate" | "localtime.truncate" => {
            if args.len() != 2 && args.len() != 3 {
                return Err(EvalError::Function {
                    name: name.to_string(),
                    detail: "takes (unit, temporal[, map])".to_string(),
                });
            }
            if matches!(args[1], Value::Null) {
                return Ok(Value::Null);
            }
            let unit = match &args[0] {
                Value::Str(s) => s.to_lowercase(),
                other => {
                    return Err(EvalError::Function {
                        name: name.to_string(),
                        detail: format!("unit must be a string, got {}", other.type_name()),
                    });
                }
            };
            let Some((mut nanos, offset)) = time_of_day_of(&args[1]) else {
                return Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("cannot truncate a {} to a time", args[1].type_name()),
                });
            };
            nanos = truncate_tod(&unit, nanos).map_err(|detail| EvalError::Function {
                name: name.to_string(),
                detail,
            })?;
            let truncated = if name == "time.truncate" {
                Value::Time {
                    nanos,
                    offset_seconds: offset,
                }
            } else {
                Value::LocalTime(nanos)
            };
            match args.get(2) {
                Some(Value::Null) => Ok(Value::Null),
                Some(Value::Map(m)) => {
                    let mut merged = m.clone();
                    merged.insert("time".to_string(), truncated);
                    temporal_map_construct(
                        name.strip_suffix(".truncate").unwrap_or(name),
                        &merged,
                        zones,
                        false,
                    )
                }
                _ => Ok(truncated),
            }
        }
        "duration.between" | "duration.inmonths" | "duration.indays" | "duration.inseconds" => {
            arity(name, &args, 2)?;
            if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
                return Ok(Value::Null);
            }
            let (Some(mut a), Some(mut b)) = (tparts(&args[0]), tparts(&args[1])) else {
                return Err(EvalError::Function {
                    name: name.to_string(),
                    detail: "takes two temporal values".to_string(),
                });
            };
            // A local operand paired with a zoned one borrows its zone (DST-aware).
            borrow_zone(&mut a, &b, zones);
            borrow_zone(&mut b, &a, zones);
            Ok(match name {
                "duration.between" => {
                    let (months, days, seconds, nanos) = duration_between(&a, &b);
                    Value::Duration {
                        months,
                        days,
                        seconds,
                        nanos,
                    }
                }
                "duration.inmonths" => Value::Duration {
                    months: duration_between(&a, &b).0,
                    days: 0,
                    seconds: 0,
                    nanos: 0,
                },
                "duration.indays" => Value::Duration {
                    months: 0,
                    days: (duration_total_ns(&a, &b) / NANOS_PER_DAY) as i64,
                    seconds: 0,
                    nanos: 0,
                },
                // inseconds: the whole span in seconds+nanos.
                _ => {
                    let all = duration_total_ns(&a, &b);
                    Value::Duration {
                        months: 0,
                        days: 0,
                        seconds: (all / 1_000_000_000) as i64,
                        nanos: (all % 1_000_000_000) as i32,
                    }
                }
            })
        }
        "tostring" => {
            arity(name, &args, 1)?;
            Ok(match &args[0] {
                Value::Null => Value::Null,
                Value::Str(s) => Value::Str(s.clone()),
                Value::Int(i) => Value::Str(i.to_string()),
                Value::Float(f) => Value::Str(format!("{f}")),
                Value::Bool(b) => Value::Str(b.to_string()),
                t @ (Value::Date(_)
                | Value::Time { .. }
                | Value::LocalTime(_)
                | Value::DateTime { .. }
                | Value::LocalDateTime { .. }
                | Value::Duration { .. }) => Value::Str(crate::temporal_to_string(t)),
                other => {
                    return Err(EvalError::Function {
                        name: name.to_string(),
                        detail: format!("takes a scalar, got {}", other.type_name()),
                    });
                }
            })
        }
        "tointeger" => {
            arity(name, &args, 1)?;
            Ok(match &args[0] {
                Value::Null => Value::Null,
                Value::Int(i) => Value::Int(*i),
                Value::Float(f) => Value::Int(*f as i64),
                // An unparseable string is NULL, not an error — openCypher's
                // choice. A float-shaped string (`'2.9'`, `'1.7'`) parses then
                // truncates toward zero (→ 2, 1), matching Neo4j.
                Value::Str(s) => {
                    let t = s.trim();
                    t.parse::<i64>()
                        .map(Value::Int)
                        .or_else(|_| t.parse::<f64>().map(|f| Value::Int(f as i64)))
                        .unwrap_or(Value::Null)
                }
                Value::Bool(b) => Value::Int(i64::from(*b)),
                other => {
                    return Err(EvalError::Function {
                        name: name.to_string(),
                        detail: format!("takes a scalar, got {}", other.type_name()),
                    });
                }
            })
        }
        "tofloat" => {
            arity(name, &args, 1)?;
            Ok(match &args[0] {
                Value::Null => Value::Null,
                Value::Int(i) => Value::Float(*i as f64),
                Value::Float(f) => Value::Float(*f),
                Value::Str(s) => s.trim().parse().map(Value::Float).unwrap_or(Value::Null),
                other => {
                    return Err(EvalError::Function {
                        name: name.to_string(),
                        detail: format!("takes a number or string, got {}", other.type_name()),
                    });
                }
            })
        }
        "toupper" | "tolower" | "trim" => {
            arity(name, &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Str(s) => Ok(Value::Str(match name {
                    "toupper" => s.to_uppercase(),
                    "tolower" => s.to_lowercase(),
                    _ => s.trim().to_string(),
                })),
                other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes a string, got {}", other.type_name()),
                }),
            }
        }
        "split" => {
            arity(name, &args, 2)?;
            match (&args[0], &args[1]) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Str(sep)) => Ok(Value::List(
                    s.split(sep.as_str())
                        .map(|p| Value::Str(p.to_string()))
                        .collect(),
                )),
                (a, b) => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!(
                        "takes two strings, got {}, {}",
                        a.type_name(),
                        b.type_name()
                    ),
                }),
            }
        }
        "range" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes 2 or 3 arguments, got {}", args.len()),
                });
            }
            let as_int = |v: &Value| match v {
                Value::Int(i) => Some(*i),
                _ => None,
            };
            let (Some(start), Some(end)) = (as_int(&args[0]), as_int(&args[1])) else {
                return Err(EvalError::Function {
                    name: name.to_string(),
                    detail: "bounds must be integers".to_string(),
                });
            };
            let step = match args.get(2) {
                None => 1,
                Some(v) => as_int(v).ok_or_else(|| EvalError::Function {
                    name: name.to_string(),
                    detail: "step must be an integer".to_string(),
                })?,
            };
            if step == 0 {
                return Err(EvalError::Function {
                    name: name.to_string(),
                    detail: "step must not be zero".to_string(),
                });
            }
            let mut out = Vec::new();
            let mut v = start;
            // range() is INCLUSIVE of the end — a fence-post the corpus
            // depends on (`range(0, n-1)` idioms everywhere).
            while (step > 0 && v <= end) || (step < 0 && v >= end) {
                out.push(Value::Int(v));
                v = match v.checked_add(step) {
                    Some(v) => v,
                    None => break,
                };
            }
            Ok(Value::List(out))
        }
        "keys" => {
            arity(name, &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Map(m) | Value::Node { props: m, .. } | Value::Rel { props: m, .. } => Ok(
                    Value::List(m.keys().map(|k| Value::Str(k.clone())).collect()),
                ),
                other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes a map, got {}", other.type_name()),
                }),
            }
        }
        "labels" => {
            arity(name, &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Node { labels, .. } => Ok(Value::List(
                    labels.iter().map(|l| Value::Str(l.clone())).collect(),
                )),
                other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes a node, got {}", other.type_name()),
                }),
            }
        }
        "type" => {
            arity(name, &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Rel { rel_type, .. } => Ok(Value::Str(rel_type.clone())),
                other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes a relationship, got {}", other.type_name()),
                }),
            }
        }
        "id" => {
            arity(name, &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Node { id, .. } | Value::Rel { id, .. } => i64::try_from(*id)
                    .map(Value::Int)
                    .map_err(|_| EvalError::Overflow("id")),
                other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes a node or relationship, got {}", other.type_name()),
                }),
            }
        }
        "elementid" => {
            // elementId is a STRING in Bolt 5 — the corpus's primary
            // identity, 65 call sites.
            arity(name, &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Node { id, .. } => Ok(Value::Str(format!("n:{id}"))),
                Value::Rel { id, .. } => Ok(Value::Str(format!("r:{id}"))),
                other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes a node or relationship, got {}", other.type_name()),
                }),
            }
        }
        "properties" => {
            arity(name, &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Node { props, .. } | Value::Rel { props, .. } => {
                    Ok(Value::Map(props.clone()))
                }
                Value::Map(m) => Ok(Value::Map(m.clone())),
                other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!(
                        "takes a node, relationship or map, got {}",
                        other.type_name()
                    ),
                }),
            }
        }
        "startnode" | "endnode" => {
            // Returns the node ID as an integer in this slice; the
            // interpreter substitutes the full node where it has the graph.
            arity(name, &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Rel { src, dst, .. } => {
                    let v = if name == "startnode" { *src } else { *dst };
                    i64::try_from(v)
                        .map(Value::Int)
                        .map_err(|_| EvalError::Overflow("node id"))
                }
                other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes a relationship, got {}", other.type_name()),
                }),
            }
        }
        "exists" => {
            // The legacy exists(n.prop) form: null-ness of the argument.
            arity(name, &args, 1)?;
            Ok(Value::Bool(!matches!(args[0], Value::Null)))
        }

        // ── The five live APOC functions ────────────────────────────────
        "apoc.convert.tojson" => {
            arity(name, &args, 1)?;
            Ok(Value::Str(json::to_json(&args[0])))
        }
        "apoc.convert.fromjsonlist" => {
            arity(name, &args, 1)?;
            let Value::Str(s) = &args[0] else {
                return match &args[0] {
                    Value::Null => Ok(Value::Null),
                    other => Err(EvalError::Function {
                        name: name.to_string(),
                        detail: format!("takes a string, got {}", other.type_name()),
                    }),
                };
            };
            match json::from_json(s) {
                Ok(Value::List(l)) => Ok(Value::List(l)),
                Ok(other) => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("JSON was a {}, not a list", other.type_name()),
                }),
                Err(e) => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: e,
                }),
            }
        }
        "apoc.convert.fromjsonmap" => {
            arity(name, &args, 1)?;
            let Value::Str(s) = &args[0] else {
                return match &args[0] {
                    Value::Null => Ok(Value::Null),
                    other => Err(EvalError::Function {
                        name: name.to_string(),
                        detail: format!("takes a string, got {}", other.type_name()),
                    }),
                };
            };
            match json::from_json(s) {
                Ok(Value::Map(m)) => Ok(Value::Map(m)),
                Ok(other) => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("JSON was a {}, not a map", other.type_name()),
                }),
                Err(e) => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: e,
                }),
            }
        }
        "apoc.coll.toset" => {
            arity(name, &args, 1)?;
            match args.pop().expect("arity") {
                Value::Null => Ok(Value::Null),
                Value::List(items) => {
                    // First occurrence wins, order preserved — APOC's
                    // behaviour, and the one a dedup caller expects.
                    let mut out: Vec<Value> = Vec::new();
                    for item in items {
                        if !out.iter().any(|v| v.eq3(&item) == Truth::True) {
                            out.push(item);
                        }
                    }
                    Ok(Value::List(out))
                }
                other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes a list, got {}", other.type_name()),
                }),
            }
        }
        "apoc.coll.min" => {
            arity(name, &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::List(items) => {
                    let mut best: Option<&Value> = None;
                    for item in items {
                        if matches!(item, Value::Null) {
                            continue; // nulls are skipped, as the aggregate does
                        }
                        best = Some(match best {
                            None => item,
                            Some(b) => {
                                if item.lt3(b) == Truth::True {
                                    item
                                } else {
                                    b
                                }
                            }
                        });
                    }
                    Ok(best.cloned().unwrap_or(Value::Null))
                }
                other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes a list, got {}", other.type_name()),
                }),
            }
        }
        "timestamp" => {
            arity(name, &args, 0)?;
            match now_ms {
                Some(ms) => Ok(Value::Int(ms)),
                None => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: "no wall clock is configured (time is injected, never ambient)".into(),
                }),
            }
        }
        "date" | "datetime" | "localdatetime" | "time" | "localtime" => {
            temporal_construct(name, args, now_ms, zones)
        }
        // The realtime/statement/transaction clocks all read the single injected
        // `now_ms` (Engram's time is injected, never ambient), so they behave exactly
        // like the base constructor — same null/now/string/map argument handling.
        n if n
            .strip_suffix(".realtime")
            .or_else(|| n.strip_suffix(".statement"))
            .or_else(|| n.strip_suffix(".transaction"))
            .is_some_and(|b| {
                matches!(
                    b,
                    "date" | "datetime" | "localdatetime" | "time" | "localtime"
                )
            }) =>
        {
            let base = n.split('.').next().expect("has a base component");
            temporal_construct(base, args, now_ms, zones)
        }
        "duration" => {
            arity(name, &args, 1)?;
            match args.pop().expect("arity") {
                Value::Null => Ok(Value::Null),
                Value::Str(s) => match crate::temporal::parse_duration(&s) {
                    Some((months, days, seconds, nanos)) => Ok(Value::Duration {
                        months,
                        days,
                        seconds,
                        nanos,
                    }),
                    None => Err(EvalError::Function {
                        name: name.to_string(),
                        detail: format!("`{s}` is not an ISO-8601 duration"),
                    }),
                },
                Value::Map(m) => {
                    let is_component = |k: &str| {
                        matches!(
                            k,
                            "years"
                                | "quarters"
                                | "months"
                                | "weeks"
                                | "days"
                                | "hours"
                                | "minutes"
                                | "seconds"
                                | "milliseconds"
                                | "microseconds"
                                | "nanoseconds"
                        )
                    };
                    // Fractional components cascade (Neo4j): a fractional month is
                    // the AVERAGE Gregorian month (2 629 746 s), a fractional day
                    // is 86 400 s; the seconds they produce carry WHOLE days into
                    // the day field, the rest into seconds. Integer maps keep the
                    // exact path below (f64 would lose precision on large spans).
                    if m.iter()
                        .any(|(k, v)| is_component(k) && matches!(v, Value::Float(_)))
                    {
                        let getf = |k: &str| -> Result<f64, EvalError> {
                            match m.get(k) {
                                None | Some(Value::Null) => Ok(0.0),
                                Some(Value::Int(v)) => Ok(*v as f64),
                                Some(Value::Float(v)) => Ok(*v),
                                Some(other) => Err(EvalError::Function {
                                    name: "duration".to_string(),
                                    detail: format!(
                                        "`{k}` must be a number, got {}",
                                        other.type_name()
                                    ),
                                }),
                            }
                        };
                        let months_total =
                            getf("years")? * 12.0 + getf("quarters")? * 3.0 + getf("months")?;
                        let days_total = getf("weeks")? * 7.0 + getf("days")?;
                        let time_secs = getf("hours")? * 3600.0
                            + getf("minutes")? * 60.0
                            + getf("seconds")?
                            + getf("milliseconds")? / 1e3
                            + getf("microseconds")? / 1e6
                            + getf("nanoseconds")? / 1e9;
                        let months = months_total.trunc() as i64;
                        let days_i = days_total.trunc() as i64;
                        let cascade = (months_total - months as f64) * 2_629_746.0
                            + (days_total - days_i as f64) * 86_400.0;
                        let cascade_days = (cascade / 86_400.0).trunc() as i64;
                        let total = time_secs + (cascade - cascade_days as f64 * 86_400.0);
                        let seconds = total.trunc() as i64;
                        let nanos = ((total - seconds as f64) * 1e9).round() as i32;
                        return Ok(Value::Duration {
                            months,
                            days: days_i + cascade_days,
                            seconds,
                            nanos,
                        });
                    }
                    let get = |k: &str| -> Result<i64, EvalError> {
                        match m.get(k) {
                            None => Ok(0),
                            Some(Value::Int(v)) => Ok(*v),
                            Some(other) => Err(EvalError::Function {
                                name: "duration".to_string(),
                                detail: format!(
                                    "`{k}` must be an integer, got {}",
                                    other.type_name()
                                ),
                            }),
                        }
                    };
                    let months = get("years")? * 12 + get("quarters")? * 3 + get("months")?;
                    let days = get("weeks")? * 7 + get("days")?;
                    let seconds = get("hours")? * 3600 + get("minutes")? * 60 + get("seconds")?;
                    let total_nanos = get("milliseconds")? * 1_000_000
                        + get("microseconds")? * 1_000
                        + get("nanoseconds")?;
                    let seconds = seconds + total_nanos.div_euclid(1_000_000_000);
                    let nanos = total_nanos.rem_euclid(1_000_000_000) as i32;
                    Ok(Value::Duration {
                        months,
                        days,
                        seconds,
                        nanos,
                    })
                }
                other => Err(EvalError::Function {
                    name: name.to_string(),
                    detail: format!("takes a string or map, got {}", other.type_name()),
                }),
            }
        }
        _ => {
            sometimes!("cypher.unknown function refused", true);
            Err(EvalError::UnknownFunction(name.to_string()))
        }
    }
}

// ─── Temporal semantics ─────────────────────────────────────────────────────

fn is_temporal(v: &Value) -> bool {
    matches!(
        v,
        Value::Date(_)
            | Value::Time { .. }
            | Value::LocalTime(_)
            | Value::DateTime { .. }
            | Value::LocalDateTime { .. }
            | Value::Duration { .. }
    )
}

/// Temporal arithmetic: instant ± duration, duration ± duration,
/// duration × integer. Calendar components apply in LOCAL time (a month
/// added across a DST-free fixed offset is still a calendar month).
fn temporal_bin(op: BinOp, l: Value, r: Value) -> Result<Value, EvalError> {
    use Value::*;
    let type_err = |l: &Value, r: &Value| EvalError::Type {
        what: format!("{op:?}"),
        got: format!("{} vs {}", l.type_name(), r.type_name()),
    };
    match op {
        BinOp::Add => match (l, r) {
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
            ) => Ok(norm_duration(
                ma + mb,
                da + db,
                sa + sb,
                i64::from(na) + i64::from(nb),
            )),
            (d @ Duration { .. }, t) if is_temporal(&t) => temporal_bin(BinOp::Add, t, d),
            (
                t,
                Duration {
                    months,
                    days,
                    seconds,
                    nanos,
                },
            ) if is_temporal(&t) => shift_temporal(t, months, days, seconds, i64::from(nanos)),
            (l, r) => Err(type_err(&l, &r)),
        },
        BinOp::Sub => match (l, r) {
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
            ) => Ok(norm_duration(
                ma - mb,
                da - db,
                sa - sb,
                i64::from(na) - i64::from(nb),
            )),
            (
                t,
                Duration {
                    months,
                    days,
                    seconds,
                    nanos,
                },
            ) if is_temporal(&t) => shift_temporal(t, -months, -days, -seconds, -i64::from(nanos)),
            (l, r) => Err(type_err(&l, &r)),
        },
        BinOp::Mul => match (l, r) {
            (
                Duration {
                    months,
                    days,
                    seconds,
                    nanos,
                },
                Int(k),
            )
            | (
                Int(k),
                Duration {
                    months,
                    days,
                    seconds,
                    nanos,
                },
            ) => Ok(norm_duration(
                months * k,
                days * k,
                seconds * k,
                i64::from(nanos) * k,
            )),
            // A FRACTIONAL factor cascades (as construction does): keep the
            // integer months/days, spill the fractions through the average
            // Gregorian month / 86 400 s into days + seconds.
            (
                Duration {
                    months,
                    days,
                    seconds,
                    nanos,
                },
                Float(f),
            )
            | (
                Float(f),
                Duration {
                    months,
                    days,
                    seconds,
                    nanos,
                },
            ) => Ok(scale_duration(months, days, seconds, nanos, f)),
            (l, r) => Err(type_err(&l, &r)),
        },
        BinOp::Div => match (l, r) {
            (
                Duration {
                    months,
                    days,
                    seconds,
                    nanos,
                },
                Int(k),
            ) if k != 0 => Ok(scale_duration(months, days, seconds, nanos, 1.0 / k as f64)),
            (
                Duration {
                    months,
                    days,
                    seconds,
                    nanos,
                },
                Float(f),
            ) => Ok(scale_duration(months, days, seconds, nanos, 1.0 / f)),
            (l, r) => Err(type_err(&l, &r)),
        },
        _ => Err(type_err(&l, &r)),
    }
}

/// Scale a duration by a real factor, cascading fractional months (the AVERAGE
/// Gregorian month, 2 629 746 s) and fractional days (86 400 s) down into days
/// and seconds — the same rule `duration({…})` uses for fractional components.
fn scale_duration(months: i64, days: i64, seconds: i64, nanos: i32, factor: f64) -> Value {
    let months_f = months as f64 * factor;
    let days_f = days as f64 * factor;
    let secs_f = (seconds as f64 + f64::from(nanos) / 1e9) * factor;
    let m = months_f.trunc() as i64;
    let d = days_f.trunc() as i64;
    let cascade = (months_f - m as f64) * 2_629_746.0 + (days_f - d as f64) * 86_400.0;
    let cd = (cascade / 86_400.0).trunc() as i64;
    let total = secs_f + (cascade - cd as f64 * 86_400.0);
    let s = total.trunc() as i64;
    let n = ((total - s as f64) * 1e9).round() as i64;
    norm_duration(m, d + cd, s, n)
}

fn norm_duration(months: i64, days: i64, seconds: i64, nanos: i64) -> Value {
    let seconds = seconds + nanos.div_euclid(1_000_000_000);
    let nanos = nanos.rem_euclid(1_000_000_000) as i32;
    Value::Duration {
        months,
        days,
        seconds,
        nanos,
    }
}

/// Shift an instant/date/time by duration components.
fn shift_temporal(
    t: Value,
    months: i64,
    days: i64,
    seconds: i64,
    nanos: i64,
) -> Result<Value, EvalError> {
    use crate::temporal::{civil_from_days, days_from_civil};
    let add_months = |day0: i64, months: i64| -> i64 {
        if months == 0 {
            return day0;
        }
        let (y, m, d) = civil_from_days(day0);
        let total = y * 12 + i64::from(m) - 1 + months;
        let (ny, nm) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
        // Clamp the day into the target month (Jan 31 + 1 month = Feb 28/29).
        let next_month_start = if nm == 12 {
            days_from_civil(ny + 1, 1, 1)
        } else {
            days_from_civil(ny, nm + 1, 1)
        };
        let month_len = (next_month_start - days_from_civil(ny, nm, 1)) as u32;
        days_from_civil(ny, nm, d.min(month_len))
    };
    match t {
        Value::Date(day0) => {
            // A date has day accuracy: whole days in the duration's seconds count,
            // but the sub-day remainder is TRUNCATED toward zero — `div_euclid`
            // floors, so it wrongly pulls `date - duration` back a day when the
            // negated seconds are a fraction of a day (`-70 s / 86400` must be 0).
            let shifted = add_months(day0, months) + days + seconds / 86_400;
            Ok(Value::Date(shifted))
        }
        Value::LocalTime(n) => {
            let total = (n + seconds * 1_000_000_000 + nanos).rem_euclid(86_400_000_000_000);
            Ok(Value::LocalTime(total))
        }
        Value::Time {
            nanos: n,
            offset_seconds,
        } => {
            let total = (n + seconds * 1_000_000_000 + nanos).rem_euclid(86_400_000_000_000);
            Ok(Value::Time {
                nanos: total,
                offset_seconds,
            })
        }
        Value::LocalDateTime {
            epoch_seconds,
            nanos: n,
        } => {
            let (s2, n2) = shift_wall(
                epoch_seconds,
                n,
                0,
                months,
                days,
                seconds,
                nanos,
                &add_months,
            );
            Ok(Value::LocalDateTime {
                epoch_seconds: s2,
                nanos: n2,
            })
        }
        Value::DateTime {
            epoch_seconds,
            nanos: n,
            offset_seconds,
            zone,
        } => {
            let (s2, n2) = shift_wall(
                epoch_seconds,
                n,
                offset_seconds,
                months,
                days,
                seconds,
                nanos,
                &add_months,
            );
            Ok(Value::DateTime {
                epoch_seconds: s2,
                nanos: n2,
                offset_seconds,
                zone,
            })
        }
        other => Err(EvalError::Type {
            what: "temporal arithmetic".to_string(),
            got: other.type_name().to_string(),
        }),
    }
}

#[allow(clippy::too_many_arguments)]
fn shift_wall(
    epoch_seconds: i64,
    sub_nanos: u32,
    offset: i32,
    months: i64,
    days: i64,
    seconds: i64,
    nanos: i64,
    add_months: &dyn Fn(i64, i64) -> i64,
) -> (i64, u32) {
    // Calendar components in LOCAL wall time; clock components as raw span.
    let local = epoch_seconds + i64::from(offset);
    let day0 = local.div_euclid(86_400);
    let tod = local.rem_euclid(86_400);
    let day1 = add_months(day0, months) + days;
    let mut total_nanos = i64::from(sub_nanos) + nanos;
    let mut local2 = day1 * 86_400 + tod + seconds + total_nanos.div_euclid(1_000_000_000);
    total_nanos = total_nanos.rem_euclid(1_000_000_000);
    local2 -= i64::from(offset);
    (local2, total_nanos as u32)
}

/// A temporal value's component, via property access (`d.year`).
fn temporal_component(t: &Value, key: &str) -> Result<Value, EvalError> {
    use crate::temporal::civil_from_days;
    let unknown = || EvalError::Function {
        name: format!(".{key}"),
        detail: format!("no such component on a {}", t.type_name()),
    };
    let date_parts = |days: i64| civil_from_days(days);
    let ymd_component = |days: i64, key: &str| -> Option<i64> {
        let (y, m, d) = date_parts(days);
        Some(match key {
            "year" => y,
            "month" => i64::from(m),
            "day" => i64::from(d),
            "quarter" => i64::from((m - 1) / 3 + 1),
            "week" => {
                // ISO week number, via the Thursday rule.
                let dow = (days + 3).rem_euclid(7); // 0 = Monday
                let thursday = days - dow + 3;
                let (ty, _, _) = date_parts(thursday);
                let jan1 = crate::temporal::days_from_civil(ty, 1, 1);
                (thursday - jan1) / 7 + 1
            }
            "dayOfWeek" | "weekDay" => (days + 3).rem_euclid(7) + 1, // 1 = Monday, Neo4j's rule
            "ordinalDay" => {
                let (y, _, _) = date_parts(days);
                days - crate::temporal::days_from_civil(y, 1, 1) + 1
            }
            "dayOfQuarter" => {
                let (y, m, _) = date_parts(days);
                let q_start_month = (m - 1) / 3 * 3 + 1;
                days - crate::temporal::days_from_civil(y, q_start_month, 1) + 1
            }
            "weekYear" => {
                let thursday = days - (days + 3).rem_euclid(7) + 3;
                date_parts(thursday).0
            }
            _ => return None,
        })
    };
    let tod_component = |nanos: i64, key: &str| -> Option<i64> {
        Some(match key {
            "hour" => nanos / 3_600_000_000_000,
            "minute" => (nanos / 60_000_000_000) % 60,
            "second" => (nanos / 1_000_000_000) % 60,
            "millisecond" => (nanos / 1_000_000) % 1_000,
            "microsecond" => (nanos / 1_000) % 1_000_000,
            "nanosecond" => nanos % 1_000_000_000,
            _ => return None,
        })
    };
    match t {
        Value::Date(days) => ymd_component(*days, key)
            .map(Value::Int)
            .ok_or_else(unknown),
        Value::LocalTime(nanos) => tod_component(*nanos, key)
            .map(Value::Int)
            .ok_or_else(unknown),
        Value::Time {
            nanos,
            offset_seconds,
        } => match key {
            "offsetSeconds" => Ok(Value::Int(i64::from(*offset_seconds))),
            "offsetMinutes" => Ok(Value::Int(i64::from(*offset_seconds) / 60)),
            // A time carries no NAMED zone, so `timezone` is its offset spelling.
            "offset" | "timezone" => {
                Ok(Value::Str(crate::temporal::format_offset(*offset_seconds)))
            }
            _ => tod_component(*nanos, key)
                .map(Value::Int)
                .ok_or_else(unknown),
        },
        Value::LocalDateTime {
            epoch_seconds,
            nanos,
        } => {
            let days = epoch_seconds.div_euclid(86_400);
            let tod = epoch_seconds.rem_euclid(86_400) * 1_000_000_000 + i64::from(*nanos);
            match key {
                "epochSeconds" => Ok(Value::Int(*epoch_seconds)),
                "epochMillis" => Ok(Value::Int(
                    epoch_seconds * 1_000 + i64::from(*nanos) / 1_000_000,
                )),
                _ => ymd_component(days, key)
                    .or_else(|| tod_component(tod, key))
                    .map(Value::Int)
                    .ok_or_else(unknown),
            }
        }
        Value::DateTime {
            epoch_seconds,
            nanos,
            offset_seconds,
            zone,
        } => {
            let local = epoch_seconds + i64::from(*offset_seconds);
            let days = local.div_euclid(86_400);
            let tod = local.rem_euclid(86_400) * 1_000_000_000 + i64::from(*nanos);
            match key {
                "epochSeconds" => Ok(Value::Int(*epoch_seconds)),
                "epochMillis" => Ok(Value::Int(
                    epoch_seconds * 1_000 + i64::from(*nanos) / 1_000_000,
                )),
                "offsetSeconds" => Ok(Value::Int(i64::from(*offset_seconds))),
                "offsetMinutes" => Ok(Value::Int(i64::from(*offset_seconds) / 60)),
                "offset" => Ok(Value::Str(crate::temporal::format_offset(*offset_seconds))),
                "timezone" => Ok(match zone {
                    Some(z) => Value::Str(z.clone()),
                    None => Value::Str(crate::temporal::format_offset(*offset_seconds)),
                }),
                _ => ymd_component(days, key)
                    .or_else(|| tod_component(tod, key))
                    .map(Value::Int)
                    .ok_or_else(unknown),
            }
        }
        Value::Duration {
            months,
            days,
            seconds,
            nanos,
        } => Ok(Value::Int(match key {
            "months" => *months,
            "years" => *months / 12,
            "monthsOfYear" => *months % 12,
            "quarters" => *months / 3,
            "quartersOfYear" => *months / 3 % 4,
            "monthsOfQuarter" => *months % 3,
            "days" => *days,
            "weeks" => *days / 7,
            "daysOfWeek" => *days % 7,
            "seconds" => *seconds,
            "hours" => *seconds / 3600,
            "hoursOfDay" => *seconds % 86_400 / 3600,
            "minutes" => *seconds / 60,
            "minutesOfHour" => *seconds / 60 % 60,
            "secondsOfMinute" => *seconds % 60,
            "milliseconds" => *seconds * 1_000 + i64::from(*nanos) / 1_000_000,
            "millisecondsOfSecond" => i64::from(*nanos) / 1_000_000,
            "microseconds" => *seconds * 1_000_000 + i64::from(*nanos) / 1_000,
            "microsecondsOfSecond" => i64::from(*nanos) / 1_000,
            "nanoseconds" => *seconds * 1_000_000_000 + i64::from(*nanos),
            "nanosecondsOfSecond" => i64::from(*nanos),
            _ => return Err(unknown()),
        })),
        _ => Err(unknown()),
    }
}

/// The five point-temporal constructors.
/// The ISO-8601 week number (1..53) of an ordinal day.
fn iso_week_number(days: i64) -> i64 {
    let thursday = days - (days + 3).rem_euclid(7) + 3;
    let (ty, _, _) = crate::temporal::civil_from_days(thursday);
    (thursday - iso_weekyear_start(ty, 1, 4)) / 7 + 1
}

/// Build a temporal from a component map: an optional base temporal
/// (`date`/`time`/`datetime`) supplies defaults, then the finest explicitly
/// present date-selector group (quarter, else month/day, else week, else
/// ordinal) and the time components override it. This is the openCypher
/// "create/select from a map" grammar (Temporal1 / Temporal3).
fn temporal_map_construct(
    name: &str,
    m: &BTreeMap<String, Value>,
    zones: Option<&dyn crate::temporal::ZoneProvider>,
    // `true` for the projection constructors (`datetime({datetime: x, timezone})`
    // CONVERTS x to the new zone); `false` for a truncate's override map, where
    // the timezone REINTERPRETS the already-computed local value.
    convert: bool,
) -> Result<Value, EvalError> {
    use crate::temporal::{days_from_civil, parse_zone};
    let func_err = |detail: String| EvalError::Function {
        name: name.to_string(),
        detail,
    };
    let geti = |k: &str| -> Result<Option<i64>, EvalError> {
        match m.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Int(v)) => Ok(Some(*v)),
            Some(other) => Err(func_err(format!(
                "`{k}` must be an integer, got {}",
                other.type_name()
            ))),
        }
    };
    let has = |k: &str| matches!(m.get(k), Some(v) if !matches!(v, Value::Null));

    // A `date`/`datetime` base supplies date components; a `time`/`datetime`
    // base supplies the time-of-day and offset. The DATE always comes from the
    // date base (never the time base), so `{date: D, time: <datetime>}` keeps D's
    // date; the timezone convert/shift is applied AFTER date + tod are known.
    let base_dmy = m
        .get("date")
        .or_else(|| m.get("datetime"))
        .and_then(local_date_of);
    let base_days = base_dmy.map(|(y, mo, d)| days_from_civil(y, mo, d));
    let base_tod = m
        .get("time")
        .or_else(|| m.get("datetime"))
        .and_then(time_of_day_of);
    // Only a base that CARRIES a real offset (a `time`/`datetime`) is CONVERTED
    // (instant-preserving) by an explicit timezone; a local* base is re-interpreted
    // (its wall clock is kept). Deferring the decision lets a named-zone base be
    // re-resolved at the FINAL date — Stockholm is +02:00 on 1984-03-28.
    let base_has_offset = matches!(
        m.get("time").or_else(|| m.get("datetime")),
        Some(Value::Time { .. } | Value::DateTime { .. })
    );

    // ---- DATE ----
    let (by, bmo, bd) = base_dmy.unwrap_or((1970, 1, 1));
    let year = geti("year")?.unwrap_or(by);
    let days = if has("quarter") || has("dayOfQuarter") {
        let base_q = (bmo as i64 - 1) / 3 + 1;
        let base_q_start = days_from_civil(by, (((bmo as i64 - 1) / 3) * 3 + 1) as u32, 1);
        let base_doq = base_days.map(|d| d - base_q_start + 1).unwrap_or(1);
        let quarter = geti("quarter")?.unwrap_or(base_q);
        let doq = geti("dayOfQuarter")?.unwrap_or(base_doq);
        days_from_civil(year, ((quarter - 1) * 3 + 1) as u32, 1) + (doq - 1)
    } else if has("month") || has("day") {
        let month = geti("month")?.unwrap_or(bmo as i64);
        let day = geti("day")?.unwrap_or(bd as i64);
        days_from_civil(year, month as u32, day as u32)
    } else if has("week") || has("dayOfWeek") {
        // A week-date's default year is the base's ISO WEEK-year, not its calendar
        // year: `date({date: date('1816-12-31'), week: 2})` sits in week-year 1817
        // (1816-12-31 is a Tuesday of ISO week 1 of 1817), so it resolves to
        // 1817-01-07, not into calendar 1816.
        let base_weekyear = base_days
            .map(|d| {
                let thursday = d - (d + 3).rem_euclid(7) + 3;
                crate::temporal::civil_from_days(thursday).0
            })
            .unwrap_or(by);
        let year = geti("year")?.unwrap_or(base_weekyear);
        let base_week = base_days.map(iso_week_number).unwrap_or(1);
        let base_dow = base_days.map(|d| (d + 3).rem_euclid(7) + 1).unwrap_or(1);
        let week = geti("week")?.unwrap_or(base_week);
        let dow = geti("dayOfWeek")?.unwrap_or(base_dow);
        iso_weekyear_start(year, 1, 4) + (week - 1) * 7 + (dow - 1)
    } else if has("ordinalDay") {
        let base_ord = base_days
            .map(|d| d - days_from_civil(by, 1, 1) + 1)
            .unwrap_or(1);
        let ord = geti("ordinalDay")?.unwrap_or(base_ord);
        days_from_civil(year, 1, 1) + (ord - 1)
    } else {
        days_from_civil(year, bmo, bd)
    };

    // ---- TIME ----
    let (btod, boff) = base_tod.unwrap_or((0, 0));
    let bh = btod / 3_600_000_000_000;
    let bmi = (btod / 60_000_000_000) % 60;
    let bs = (btod / 1_000_000_000) % 60;
    let bsub = btod % 1_000_000_000;
    let hour = geti("hour")?.unwrap_or(bh);
    let minute = geti("minute")?.unwrap_or(bmi);
    let second = geti("second")?.unwrap_or(bs);
    // Each sub-second field overrides only its OWN component and keeps the rest
    // from the base: `truncate('millisecond', …, {nanosecond: 2})` sets the ns
    // digit on the already-truncated 645 ms → .645000002, not .000000002. (In
    // pure construction the base is zero, so a lone `nanosecond: N` is N ns.)
    let sub = geti("millisecond")?.unwrap_or(bsub / 1_000_000) * 1_000_000
        + geti("microsecond")?.unwrap_or(bsub / 1_000 % 1_000) * 1_000
        + geti("nanosecond")?.unwrap_or(bsub % 1_000);
    let tod_nanos =
        hour * 3_600_000_000_000 + minute * 60_000_000_000 + second * 1_000_000_000 + sub;

    // ---- OFFSET / ZONE ----
    // The named zone follows the SAME base that supplies the time-of-day and
    // offset (`time`, else `datetime`): `{date: D, time: <zoned datetime>}` inherits
    // the time base's `[Europe/Stockholm]`. A localtime/localdatetime time base
    // carries no zone, so the result is unzoned even if the `date` base had one.
    let base_zone = match m.get("time").or_else(|| m.get("datetime")) {
        Some(Value::DateTime { zone, .. }) => zone.clone(),
        _ => None,
    };
    // Wall-clock seconds/sub-seconds of the value as constructed, before any convert.
    let local_secs = days * 86_400 + tod_nanos.div_euclid(1_000_000_000);
    let sub_nanos = tod_nanos.rem_euclid(1_000_000_000);
    // A named-zone base is re-resolved at the CONSTRUCTED date (DST depends on it);
    // a fixed-offset base keeps its stored offset.
    let boff = match &base_zone {
        Some(z) => resolve_zone(z, local_secs, zones).unwrap_or(boff),
        None => boff,
    };
    let (offset_seconds, zone, days, tod_nanos) = match m.get("timezone") {
        None => (boff, base_zone, days, tod_nanos),
        Some(Value::Str(z)) => {
            let (target_off, target_name) = match parse_zone(z) {
                Some((Some(o), _, "")) => (o, None),
                _ => {
                    // Resolve at the UTC instant when converting an offset-bearing
                    // base (we hold the instant); at the wall clock otherwise.
                    let at = if convert && base_has_offset {
                        local_secs - boff as i64
                    } else {
                        local_secs
                    };
                    let o = resolve_zone(z, at, zones).ok_or_else(|| {
                        func_err(format!(
                            "zone `{z}` needs tzdata to resolve; give an explicit offset or install a ZoneProvider"
                        ))
                    })?;
                    (o, Some(z.clone()))
                }
            };
            if convert && base_has_offset {
                // CONVERT preserving the instant: shift the wall clock by the offset
                // difference, carrying the date across midnight.
                let shifted = local_secs - boff as i64 + target_off as i64;
                (
                    target_off,
                    target_name,
                    shifted.div_euclid(86_400),
                    shifted.rem_euclid(86_400) * 1_000_000_000 + sub_nanos,
                )
            } else {
                // REINTERPRET: keep the wall clock, adopt the override's offset/zone.
                (target_off, target_name, days, tod_nanos)
            }
        }
        Some(other) => {
            return Err(func_err(format!(
                "timezone must be a string, got {}",
                other.type_name()
            )));
        }
    };

    let local_seconds = days * 86_400 + tod_nanos.div_euclid(1_000_000_000);
    let nanos = tod_nanos.rem_euclid(1_000_000_000) as u32;
    Ok(match name {
        "date" => Value::Date(days),
        "localdatetime" => Value::LocalDateTime {
            epoch_seconds: local_seconds,
            nanos,
        },
        "datetime" => Value::DateTime {
            epoch_seconds: local_seconds - i64::from(offset_seconds),
            nanos,
            offset_seconds,
            zone,
        },
        "time" => Value::Time {
            nanos: tod_nanos,
            offset_seconds,
        },
        "localtime" => Value::LocalTime(tod_nanos),
        _ => unreachable!("caller matched the name"),
    })
}

fn temporal_construct(
    name: &str,
    mut args: Vec<Value>,
    now_ms: Option<i64>,
    zones: Option<&dyn crate::temporal::ZoneProvider>,
) -> Result<Value, EvalError> {
    use crate::temporal::{parse_date, parse_time_of_day, parse_zone};
    let func_err = |detail: String| EvalError::Function {
        name: name.to_string(),
        detail,
    };
    if args.len() > 1 {
        return Err(func_err(format!(
            "takes 0 or 1 arguments, got {}",
            args.len()
        )));
    }
    // No argument: NOW, from the injected clock, in UTC.
    if args.is_empty() {
        let Some(ms) = now_ms else {
            return Err(func_err(
                "no wall clock is configured (time is injected, never ambient)".into(),
            ));
        };
        let epoch_seconds = ms.div_euclid(1_000);
        let nanos = (ms.rem_euclid(1_000) * 1_000_000) as u32;
        return Ok(match name {
            "date" => Value::Date(epoch_seconds.div_euclid(86_400)),
            "datetime" => Value::DateTime {
                epoch_seconds,
                nanos,
                offset_seconds: 0,
                zone: None,
            },
            "localdatetime" => Value::LocalDateTime {
                epoch_seconds,
                nanos,
            },
            "time" => Value::Time {
                nanos: epoch_seconds.rem_euclid(86_400) * 1_000_000_000 + i64::from(nanos),
                offset_seconds: 0,
            },
            "localtime" => Value::LocalTime(
                epoch_seconds.rem_euclid(86_400) * 1_000_000_000 + i64::from(nanos),
            ),
            _ => unreachable!("caller matched the name"),
        });
    }
    match args.pop().expect("one argument") {
        Value::Null => Ok(Value::Null),
        Value::Str(s) => match name {
            "date" => parse_date(&s)
                .map(Value::Date)
                .ok_or_else(|| func_err(format!("`{s}` is not an ISO date"))),
            "localtime" => match parse_time_of_day(&s) {
                Some((n, "")) => Ok(Value::LocalTime(n)),
                _ => Err(func_err(format!("`{s}` is not an ISO time"))),
            },
            "time" => {
                let (n, rest) = parse_time_of_day(&s)
                    .ok_or_else(|| func_err(format!("`{s}` is not an ISO time")))?;
                let (offset, _zone, rest) =
                    parse_zone(rest).ok_or_else(|| func_err(format!("`{s}` has a bad zone")))?;
                if !rest.is_empty() {
                    return Err(func_err(format!("trailing input in `{s}`")));
                }
                Ok(Value::Time {
                    nanos: n,
                    offset_seconds: offset.unwrap_or(0),
                })
            }
            "datetime" | "localdatetime" => {
                // A date-only string is midnight of that date — Neo4j reads
                // `datetime('2015-07-21')` as 2015-07-21T00:00:00Z. The
                // production port refused `datetime(coalesce(e.eventTime,
                // e.startAt))` over date-only values with `lacks a T`.
                let (days, tod, rest) = match s.find('T') {
                    Some(t_at) => {
                        let days = parse_date(&s[..t_at])
                            .ok_or_else(|| func_err(format!("`{s}` has a bad date part")))?;
                        let (tod, rest) = parse_time_of_day(&s[t_at + 1..])
                            .ok_or_else(|| func_err(format!("`{s}` has a bad time part")))?;
                        (days, tod, rest)
                    }
                    None => {
                        let days = parse_date(&s)
                            .ok_or_else(|| func_err(format!("`{s}` has a bad date part")))?;
                        (days, 0i64, "")
                    }
                };
                let (offset, zone, rest) =
                    parse_zone(rest).ok_or_else(|| func_err(format!("`{s}` has a bad zone")))?;
                if !rest.is_empty() {
                    return Err(func_err(format!("trailing input in `{s}`")));
                }
                let local_seconds = days * 86_400 + tod.div_euclid(1_000_000_000);
                let nanos = tod.rem_euclid(1_000_000_000) as u32;
                if name == "localdatetime" {
                    return Ok(Value::LocalDateTime {
                        epoch_seconds: local_seconds,
                        nanos,
                    });
                }
                // A NAMED zone without an offset would need tzdata to
                // resolve — refused by name, never guessed.
                let offset_seconds = match (offset, &zone) {
                    (Some(o), _) => o,
                    (None, None) => 0,
                    (None, Some(z)) => resolve_zone(z, local_seconds, zones)
                        .ok_or_else(|| func_err(format!(
                            "zone `{z}` needs tzdata to resolve; give an explicit offset or install a ZoneProvider"
                        )))?,
                };
                Ok(Value::DateTime {
                    epoch_seconds: local_seconds - i64::from(offset_seconds),
                    nanos,
                    offset_seconds,
                    zone,
                })
            }
            _ => unreachable!("caller matched the name"),
        },
        Value::Map(m) => {
            if let Some(v) = m.get("epochMillis") {
                let Value::Int(ms) = v else {
                    return Err(func_err("epochMillis must be an integer".into()));
                };
                return temporal_construct(name, vec![], Some(*ms), zones);
            }
            if let Some(v) = m.get("epochSeconds") {
                let Value::Int(sec) = v else {
                    return Err(func_err("epochSeconds must be an integer".into()));
                };
                return temporal_construct(name, vec![], Some(sec * 1_000), zones);
            }
            temporal_map_construct(name, &m, zones, true)
        }
        // The single-arg PROJECTION form `date(other)` / `time(other)` / … —
        // build from the source temporal as a base (its date, or its
        // time-of-day + offset), reusing the map grammar.
        v @ (Value::Date(_)
        | Value::Time { .. }
        | Value::LocalTime(_)
        | Value::DateTime { .. }
        | Value::LocalDateTime { .. }) => {
            let key = if local_date_of(&v).is_some() {
                "datetime"
            } else {
                "time"
            };
            let mut m = BTreeMap::new();
            m.insert(key.to_string(), v);
            temporal_map_construct(name, &m, zones, true)
        }
        other => Err(func_err(format!(
            "takes a string or map, got {}",
            other.type_name()
        ))),
    }
}

/// The zone resolution chain: the built-in fixed table first, then the
/// injected provider. The fixed table going FIRST means an embedder cannot
/// accidentally redefine UTC.
/// The UTC offset of a NAMED IANA zone at a LOCAL wall-clock time, from the
/// bundled tz database. `find_local_time_type` takes a UTC instant, so the
/// local time is resolved in two steps: a first lookup treating it as UTC gives
/// an approximate offset, then the true instant (`local − offset`) gives the
/// offset that actually applies. Off a transition boundary this is exact.
fn tzdb_resolve(zone: &str, local_seconds: i64) -> Option<i32> {
    let tz = tzdb::tz_by_name(zone)?;
    let approx = tz.find_local_time_type(local_seconds).ok()?.ut_offset();
    let utc = local_seconds - i64::from(approx);
    Some(tz.find_local_time_type(utc).ok()?.ut_offset())
}

fn resolve_zone(
    zone: &str,
    local_seconds: i64,
    zones: Option<&dyn crate::temporal::ZoneProvider>,
) -> Option<i32> {
    use crate::temporal::ZoneProvider as _;
    // Fixed spellings first (never let anything redefine UTC), then the full
    // bundled IANA database, then the built-in approximation and any injected
    // provider as fallbacks for a name the database somehow lacks.
    crate::temporal::FixedZones
        .resolve(zone, local_seconds)
        .or_else(|| tzdb_resolve(zone, local_seconds))
        .or_else(|| crate::temporal::IanaZones.resolve(zone, local_seconds))
        .or_else(|| zones.and_then(|p| p.resolve(zone, local_seconds)))
}
