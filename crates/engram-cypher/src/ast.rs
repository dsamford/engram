//! The expression AST.
//!
//! Expressions only, in this revision — patterns and clauses build on top.
//! The shape favours the EVALUATOR: operators that share null-propagation
//! rules share a node, and the ones with bespoke semantics (`AND`/`OR`'s
//! three-valued logic, `IN`'s null-element rule, `IS NULL`'s refusal to
//! propagate) get their own.

/// A binary operator with ordinary null propagation (null in → null out).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    /// `+` — numbers, strings (concat), lists (concat).
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/` — integer division for two integers, float otherwise.
    Div,
    /// `%`
    Mod,
    /// `^` — always float.
    Pow,
    /// `=`
    Eq,
    /// `<>`
    Neq,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `STARTS WITH`
    StartsWith,
    /// `ENDS WITH`
    EndsWith,
    /// `CONTAINS`
    Contains,
    /// `=~` — regex match. Parsed; evaluation is deferred (no regex engine in
    /// the zero-dep core yet) and refuses with a named error, never a guess.
    Regex,
}

/// The expression tree.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A literal `null`.
    Null,
    /// A boolean literal.
    Bool(bool),
    /// An integer literal.
    Int(i64),
    /// A float literal.
    Float(f64),
    /// A string literal.
    Str(String),
    /// A parameter reference.
    Param(String),
    /// A variable reference.
    Var(String),
    /// A list literal.
    List(Vec<Expr>),
    /// A map literal, in source order (duplicate keys are a parse refusal).
    Map(Vec<(String, Expr)>),
    /// Property access: `expr.key`.
    Prop(Box<Expr>, String),
    /// Index: `expr[e]`.
    Index(Box<Expr>, Box<Expr>),
    /// Slice: `expr[a..b]`, either end optional.
    Slice {
        /// The sliced expression.
        of: Box<Expr>,
        /// The start, if given.
        from: Option<Box<Expr>>,
        /// The end, if given.
        to: Option<Box<Expr>>,
    },
    /// A binary operation with ordinary null propagation.
    Bin(BinOp, Box<Expr>, Box<Expr>),
    /// `AND` — three-valued.
    And(Box<Expr>, Box<Expr>),
    /// `OR` — three-valued.
    Or(Box<Expr>, Box<Expr>),
    /// `XOR` — three-valued (null if either side is null).
    Xor(Box<Expr>, Box<Expr>),
    /// `NOT` — three-valued.
    Not(Box<Expr>),
    /// Unary minus.
    Neg(Box<Expr>),
    /// `IS NULL` / `IS NOT NULL` — the operators that do NOT propagate null.
    IsNull {
        /// The tested expression.
        of: Box<Expr>,
        /// True for `IS NOT NULL`.
        negated: bool,
    },
    /// `lhs IN list`.
    In(Box<Expr>, Box<Expr>),
    /// A function call, name kept as written (dotted namespaces joined).
    Call {
        /// Lower-cased, dot-joined name (`apoc.coll.min`).
        name: String,
        /// `DISTINCT` inside an aggregate call.
        distinct: bool,
        /// Arguments; `count(*)` is the empty-args `count` with `star: true`.
        args: Vec<Expr>,
        /// True for `count(*)`.
        star: bool,
    },
    /// `CASE` — both forms; the simple form carries the subject.
    Case {
        /// The subject of a simple CASE, absent for the searched form.
        subject: Option<Box<Expr>>,
        /// `WHEN … THEN …` arms, in order.
        arms: Vec<(Expr, Expr)>,
        /// The `ELSE`, if given (absent means null).
        otherwise: Option<Box<Expr>>,
    },
    /// A list comprehension: `[x IN xs WHERE p | e]`. Filter and map both
    /// optional (`[x IN xs]` copies).
    ListComp {
        /// The bound variable.
        var: String,
        /// The source list.
        source: Box<Expr>,
        /// The filter, if any.
        filter: Option<Box<Expr>>,
        /// The map expression, if any.
        map: Option<Box<Expr>>,
    },
    /// `reduce(acc = init, x IN xs | expr)`.
    Reduce {
        /// The accumulator variable.
        acc: String,
        /// Its initial value.
        init: Box<Expr>,
        /// The bound variable.
        var: String,
        /// The source list.
        source: Box<Expr>,
        /// The step expression.
        step: Box<Expr>,
    },
    /// A label predicate: `n:Label1:Label2` as a boolean expression —
    /// evaluable WITHOUT a graph (the bound node carries its labels).
    HasLabels {
        /// The tested expression (must bind to a node).
        of: Box<Expr>,
        /// The labels, all required (AND).
        labels: Vec<String>,
    },
    /// A bare pattern as a predicate: `WHERE (a)-[:R]->()`. Graph-dependent.
    PatternPredicate(Box<crate::stmt::PathPattern>),
    /// A list predicate: `any(x IN xs WHERE p)` / all / none / single.
    ListPredicate {
        /// Which quantifier.
        kind: ListPredicateKind,
        /// The bound variable.
        var: String,
        /// The list.
        source: Box<Expr>,
        /// The predicate.
        filter: Box<Expr>,
    },
    /// A map projection: `n {.a, .*, k: expr, var}`.
    MapProjection {
        /// The projected expression (node, relationship or map).
        of: Box<Expr>,
        /// The items, in order.
        items: Vec<MapProjectionItem>,
    },
    /// `EXISTS { … }` — a subquery predicate. Graph-dependent; the scalar
    /// evaluator refuses it by name, the clause interpreter will own it.
    ExistsSub(Box<crate::stmt::SubqueryBody>),
    /// `COUNT { … }`.
    CountSub(Box<crate::stmt::SubqueryBody>),
    /// A pattern comprehension: `[ (n)-[:T]->(m) WHERE p | expr ]`.
    PatternComp {
        /// The path (relationship required — that is what disambiguates it
        /// from a parenthesized expression in a list).
        path: Box<crate::stmt::PathPattern>,
        /// The filter.
        filter: Option<Box<Expr>>,
        /// The map expression (mandatory in this form).
        map: Box<Expr>,
    },
}

impl Expr {
    /// Whether the tree holds a GRAPH-DEPENDENT shape anywhere — a subquery
    /// (`EXISTS { … }`, `COUNT { … }`), a bare pattern predicate or a
    /// pattern comprehension. The scalar evaluator cannot answer one without
    /// the graph's hooks, and the planner treats it as OPAQUE: never pushed
    /// ahead of the clause that binds its variables, never vectorised, and
    /// (fix 53) evaluated only when the connective it sits in is still
    /// undecided by its cheap side. Exhaustive on purpose: a new variant must
    /// say which side of that line it falls on.
    pub fn has_subquery(&self) -> bool {
        match self {
            Expr::ExistsSub(_)
            | Expr::CountSub(_)
            | Expr::PatternPredicate(_)
            | Expr::PatternComp { .. } => true,
            Expr::Call { args, .. } | Expr::List(args) => args.iter().any(Expr::has_subquery),
            Expr::Map(entries) => entries.iter().any(|(_, v)| v.has_subquery()),
            Expr::Bin(_, a, b)
            | Expr::And(a, b)
            | Expr::Or(a, b)
            | Expr::Xor(a, b)
            | Expr::In(a, b)
            | Expr::Index(a, b) => a.has_subquery() || b.has_subquery(),
            Expr::Not(a) | Expr::Neg(a) | Expr::Prop(a, _) => a.has_subquery(),
            Expr::IsNull { of, .. } | Expr::HasLabels { of, .. } => of.has_subquery(),
            Expr::Slice { of, from, to } => {
                of.has_subquery()
                    || from.as_deref().is_some_and(Expr::has_subquery)
                    || to.as_deref().is_some_and(Expr::has_subquery)
            }
            Expr::Case {
                subject,
                arms,
                otherwise,
            } => {
                subject.as_deref().is_some_and(Expr::has_subquery)
                    || arms
                        .iter()
                        .any(|(w, t)| w.has_subquery() || t.has_subquery())
                    || otherwise.as_deref().is_some_and(Expr::has_subquery)
            }
            Expr::ListComp {
                source,
                filter,
                map,
                ..
            } => {
                source.has_subquery()
                    || filter.as_deref().is_some_and(Expr::has_subquery)
                    || map.as_deref().is_some_and(Expr::has_subquery)
            }
            Expr::Reduce {
                init, source, step, ..
            } => init.has_subquery() || source.has_subquery() || step.has_subquery(),
            Expr::ListPredicate { source, filter, .. } => {
                source.has_subquery() || filter.has_subquery()
            }
            Expr::MapProjection { of, items } => {
                of.has_subquery()
                    || items
                        .iter()
                        .any(|it| matches!(it, MapProjectionItem::Entry(_, x) if x.has_subquery()))
            }
            Expr::Null
            | Expr::Bool(_)
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Str(_)
            | Expr::Param(_)
            | Expr::Var(_) => false,
        }
    }
}

/// The list-predicate quantifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListPredicateKind {
    /// At least one element satisfies.
    Any,
    /// Every element satisfies.
    All,
    /// No element satisfies.
    None,
    /// Exactly one element satisfies.
    Single,
}

/// One item of a map projection.
#[derive(Debug, Clone, PartialEq)]
pub enum MapProjectionItem {
    /// `.key` — copy one property.
    Property(String),
    /// `.*` — copy every property.
    AllProperties,
    /// `key: expr` — a computed entry.
    Entry(String, Expr),
    /// `var` — shorthand for `var: var`.
    Variable(String),
}
