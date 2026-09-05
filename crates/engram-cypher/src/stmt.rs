//! The statement AST — patterns and clauses.
//!
//! Shapes follow the corpus census: multi-label nodes, multi-type
//! relationships (`[:A|B|C]`), undirected matches, variable-length paths
//! (mostly bounded, four unbounded), one `shortestPath`, `MERGE` with both
//! `ON` arms, `FOREACH` as the conditional-write idiom (212 sites),
//! `EXISTS {}` / `COUNT {}` subquery expressions, `CALL {}` subqueries.

use crate::ast::Expr;

/// A node pattern: `(var:Label1:Label2 {props})`.
#[derive(Debug, Clone, PartialEq)]
pub struct NodePattern {
    /// The bound variable, if named.
    pub var: Option<String>,
    /// Labels, in source order. Multi-label is an AND in MATCH.
    pub labels: Vec<String>,
    /// The property map — a map literal or a parameter.
    pub props: Option<Expr>,
}

/// Relationship direction, from the source text's arrows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelDir {
    /// `-[]->`
    Out,
    /// `<-[]-`
    In,
    /// `-[]-`
    Undirected,
}

/// A variable-length specifier: `*`, `*2`, `*1..3`, `*..5`, `*2..`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VarLength {
    /// Lower bound, if written.
    pub min: Option<u64>,
    /// Upper bound, if written.
    pub max: Option<u64>,
}

/// A relationship pattern: `-[var:TYPE|OTHER*1..2 {props}]->`.
#[derive(Debug, Clone, PartialEq)]
pub struct RelPattern {
    /// The bound variable, if named.
    pub var: Option<String>,
    /// Types, in source order. Multi-type is an OR.
    pub types: Vec<String>,
    /// Direction.
    pub dir: RelDir,
    /// The property map.
    pub props: Option<Expr>,
    /// Variable-length, if any. `Some(VarLength{None,None})` is bare `*`.
    pub length: Option<VarLength>,
}

/// One path: a node, then rel-node hops; possibly named, possibly wrapped in
/// `shortestPath(…)`.
#[derive(Debug, Clone, PartialEq)]
pub struct PathPattern {
    /// `p = …`
    pub var: Option<String>,
    /// Wrapped in `shortestPath(…)`.
    pub shortest: bool,
    /// The first node.
    pub start: NodePattern,
    /// The hops.
    pub hops: Vec<(RelPattern, NodePattern)>,
}

/// A comma-separated pattern list (one MATCH/CREATE's worth).
#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    /// The paths.
    pub paths: Vec<PathPattern>,
}

/// A projection item: `expr AS alias` (alias mandatory in WITH for
/// non-variables — enforced by the parser, because an unnamed column is
/// unreferenceable downstream).
#[derive(Debug, Clone, PartialEq)]
pub struct ProjItem {
    /// The expression.
    pub expr: Expr,
    /// The alias, if written.
    pub alias: Option<String>,
    /// The VERBATIM source text of the expression, captured by the parser and
    /// used as the column name when there is no alias (openCypher names an
    /// unaliased column by its exact source, preserving case/spacing/parens that
    /// a re-render cannot reproduce). `None` for synthetic items built by the
    /// engine's own rewrites, which fall back to a rendered name.
    pub text: Option<String>,
}

impl ProjItem {
    /// A synthetic projection item (no captured source text) — the constructor
    /// engine rewrites use so they need not spell out `text: None`.
    pub fn synthetic(expr: Expr, alias: Option<String>) -> ProjItem {
        ProjItem {
            expr,
            alias,
            text: None,
        }
    }
}

/// One ORDER BY key.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    /// The key expression.
    pub expr: Expr,
    /// Descending?
    pub desc: bool,
}

/// The shared projection body of RETURN and WITH.
#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    /// `DISTINCT`.
    pub distinct: bool,
    /// `*` — carry every variable. May be combined with items (`RETURN *, x`).
    pub star: bool,
    /// The items.
    pub items: Vec<ProjItem>,
    /// ORDER BY keys.
    pub order: Vec<OrderItem>,
    /// SKIP.
    pub skip: Option<Expr>,
    /// LIMIT.
    pub limit: Option<Expr>,
}

/// A SET clause item.
#[derive(Debug, Clone, PartialEq)]
pub enum SetItem {
    /// `n.prop = expr` — the target is the property chain's base + key.
    Prop {
        /// The base expression (usually a variable).
        base: Expr,
        /// The property name.
        key: String,
        /// The value.
        value: Expr,
    },
    /// `n = expr` — replace all properties.
    Replace {
        /// The variable.
        var: String,
        /// The map.
        value: Expr,
    },
    /// `n += expr` — merge properties.
    Merge {
        /// The variable.
        var: String,
        /// The map.
        value: Expr,
    },
    /// `n:Label1:Label2`.
    Labels {
        /// The variable.
        var: String,
        /// The labels.
        labels: Vec<String>,
    },
}

/// A REMOVE clause item.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoveItem {
    /// `n.prop`.
    Prop {
        /// The base expression.
        base: Expr,
        /// The property name.
        key: String,
    },
    /// `n:Label`.
    Labels {
        /// The variable.
        var: String,
        /// The labels.
        labels: Vec<String>,
    },
}

/// One clause.
#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    /// MATCH / OPTIONAL MATCH.
    Match {
        /// OPTIONAL?
        optional: bool,
        /// The pattern.
        pattern: Pattern,
        /// WHERE, if present.
        where_: Option<Expr>,
    },
    /// UNWIND expr AS var.
    Unwind {
        /// The list expression.
        expr: Expr,
        /// The introduced variable.
        alias: String,
    },
    /// WITH — projection plus optional WHERE.
    With {
        /// The projection.
        proj: Projection,
        /// WHERE after the projection.
        where_: Option<Expr>,
    },
    /// RETURN.
    Return {
        /// The projection.
        proj: Projection,
    },
    /// CREATE.
    Create {
        /// The pattern.
        pattern: Pattern,
    },
    /// MERGE, with its ON arms.
    Merge {
        /// The single path to merge.
        path: PathPattern,
        /// ON CREATE SET items.
        on_create: Vec<SetItem>,
        /// ON MATCH SET items.
        on_match: Vec<SetItem>,
    },
    /// SET.
    Set {
        /// The items.
        items: Vec<SetItem>,
    },
    /// REMOVE.
    Remove {
        /// The items.
        items: Vec<RemoveItem>,
    },
    /// DELETE / DETACH DELETE.
    Delete {
        /// DETACH?
        detach: bool,
        /// What to delete.
        exprs: Vec<Expr>,
    },
    /// FOREACH (var IN expr | updates).
    Foreach {
        /// The bound variable.
        var: String,
        /// The list.
        source: Expr,
        /// The update clauses.
        updates: Vec<Clause>,
    },
    /// CALL [(imports)] { subquery } [IN TRANSACTIONS].
    CallSubquery {
        /// The subquery.
        query: Box<Query>,
        /// `IN TRANSACTIONS`.
        in_transactions: bool,
        /// The Cypher-5 scoped import list (`CALL (a, b) { … }`); empty for
        /// the classic form, which imports via WITH inside.
        imports: Vec<String>,
    },
    /// CALL proc.name(args) [YIELD a, b AS c] [WHERE …].
    CallProcedure {
        /// Dot-joined lower-cased name.
        name: String,
        /// Arguments.
        args: Vec<Expr>,
        /// YIELD items as (name, alias).
        yields: Vec<(String, Option<String>)>,
        /// WHERE after YIELD.
        where_: Option<Expr>,
    },
}

/// The body of an `EXISTS { … }` / `COUNT { … }` subquery expression.
#[derive(Debug, Clone, PartialEq)]
pub enum SubqueryBody {
    /// A bare pattern with an optional WHERE.
    Pattern {
        /// The pattern.
        pattern: Pattern,
        /// The filter.
        where_: Option<Expr>,
    },
    /// A full subquery.
    Query(SingleQuery),
}

/// A single (non-UNION) query: clauses in order.
#[derive(Debug, Clone, PartialEq)]
pub struct SingleQuery {
    /// The clauses.
    pub clauses: Vec<Clause>,
}

/// A whole query.
#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    /// No UNION.
    Single(SingleQuery),
    /// UNION arms. `all` applies uniformly — mixing `UNION` and `UNION ALL`
    /// is a parse refusal, as in Neo4j.
    Union {
        /// Keep duplicates?
        all: bool,
        /// The arms, in order.
        arms: Vec<SingleQuery>,
    },
}

/// A schema command — the DDL half of the surface (294 constraint sites in
/// the corpus, ~44 vector indexes).
#[derive(Debug, Clone, PartialEq)]
pub enum SchemaCmd {
    /// `CREATE VECTOR INDEX name FOR (n:Label) ON (n.prop) [OPTIONS {…}]`.
    CreateVectorIndex {
        /// The index name.
        name: String,
        /// `IF NOT EXISTS`.
        if_not_exists: bool,
        /// The single label — a two-label FOR is a SYNTAX error in Cypher,
        /// and a real deployment's vector indexes silently failed on exactly
        /// that for months, so the refusal is load-bearing.
        label: String,
        /// The property.
        prop: String,
        /// The OPTIONS map, uninterpreted here.
        options: Option<Expr>,
    },
    /// `CREATE FULLTEXT INDEX name FOR (n:L1|L2) ON EACH [n.p1, n.p2]`.
    CreateFulltextIndex {
        /// The index name.
        name: String,
        /// `IF NOT EXISTS`.
        if_not_exists: bool,
        /// The labels (fulltext allows several, OR-combined).
        labels: Vec<String>,
        /// The properties.
        props: Vec<String>,
    },
    /// `CREATE [RANGE] INDEX [name] FOR (n:Label) ON (n.p…)` — or the
    /// relationship form `FOR ()-[r:TYPE]-() ON (r.p…)`.
    CreateRangeIndex {
        /// The index name, if given.
        name: Option<String>,
        /// `IF NOT EXISTS`.
        if_not_exists: bool,
        /// The label — or the relationship TYPE for the rel form.
        label: String,
        /// The properties.
        props: Vec<String>,
        /// Whether this indexes relationships rather than nodes.
        on_relationships: bool,
    },
    /// `CREATE CONSTRAINT [name] FOR (n:Label) REQUIRE n.prop IS UNIQUE|IS NOT NULL`,
    /// or the relationship form `FOR ()-[r:TYPE]-() REQUIRE r.prop IS …`.
    CreateConstraint {
        /// The constraint name, if given.
        name: Option<String>,
        /// `IF NOT EXISTS`.
        if_not_exists: bool,
        /// The label — or the relationship TYPE for the rel form.
        label: String,
        /// The properties — one for the plain forms, several for the
        /// composite `REQUIRE (a, b) IS UNIQUE` / `IS NODE KEY` forms.
        props: Vec<String>,
        /// What is required.
        kind: ConstraintKind,
        /// Whether this constrains relationships rather than nodes. The scope
        /// is carried all the way to enforcement — a rel constraint validated
        /// against the node population (or the reverse) would silently never
        /// fire, certifying an integrity rule that holds over the wrong set.
        on_relationships: bool,
    },
    /// `SHOW <subject> …` — parsed (it IS the language). INDEXES and
    /// CONSTRAINTS answer from the catalogue; every other subject refuses by
    /// name at execution until implemented.
    Show {
        /// The first word after SHOW (CONSTRAINTS, INDEXES, PROCEDURES…).
        subject: String,
        /// Whether anything followed the subject (YIELD/WHERE/RETURN…). The
        /// tail is consumed unvalidated, so an executor must refuse when it
        /// was present rather than answer with the projection silently
        /// ignored.
        tail: bool,
    },
    /// `DROP INDEX name [IF EXISTS]`.
    DropIndex {
        /// The name.
        name: String,
        /// `IF EXISTS`.
        if_exists: bool,
    },
    /// `DROP CONSTRAINT name [IF EXISTS]`.
    DropConstraint {
        /// The name.
        name: String,
        /// `IF EXISTS`.
        if_exists: bool,
    },
}

/// What a constraint requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintKind {
    /// `IS UNIQUE` — null components exempt a row (Neo4j's rule).
    Unique,
    /// `IS NOT NULL` (existence).
    NotNull,
    /// `IS NODE KEY` — every property required AND the tuple unique.
    NodeKey,
}

/// Any statement: a query, or a schema command.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// A query.
    Query(Query),
    /// A schema command.
    Schema(SchemaCmd),
}

impl Stmt {
    /// Whether this statement CAN write — decided from its syntax, before
    /// it runs. `false` is a promise: no clause in it, at any nesting, is an
    /// updating clause, and no procedure it calls is one that mutates.
    /// Anything the predicate cannot vouch for answers `true`; the cost of a
    /// wrong `true` is a transaction begin, the cost of a wrong `false` is a
    /// write outside the transaction that should have carried it.
    pub fn may_write(&self) -> bool {
        match self {
            Stmt::Query(q) => q.may_write(),
            // DDL writes through its own path, directly, whether or not a
            // transaction is open — a transaction wrapped around it would
            // carry an empty write-set (nothing to validate, nothing a
            // ROLLBACK could undo) and buy only overhead. `false` here means
            // "run it plainly", exactly as it ran before statements became
            // transactions; it does NOT claim DDL is a read.
            Stmt::Schema(_) => false,
        }
    }
}

impl Query {
    /// See [`Stmt::may_write`].
    pub fn may_write(&self) -> bool {
        match self {
            Query::Single(q) => q.may_write(),
            Query::Union { arms, .. } => arms.iter().any(SingleQuery::may_write),
        }
    }
}

impl SingleQuery {
    /// See [`Stmt::may_write`].
    pub fn may_write(&self) -> bool {
        self.clauses.iter().any(Clause::may_write)
    }
}

impl Clause {
    /// See [`Stmt::may_write`]. A `CALL { … }` subquery answers for its
    /// body; a procedure call answers `false` only for the read-only
    /// families (`db.index.*` lookups, the schema listings) — every other
    /// procedure is treated as a writer, including all of `apoc.*`.
    pub fn may_write(&self) -> bool {
        match self {
            Clause::Match { .. } | Clause::Unwind { .. } | Clause::With { .. } | Clause::Return { .. } => {
                false
            }
            Clause::Create { .. }
            | Clause::Merge { .. }
            | Clause::Set { .. }
            | Clause::Remove { .. }
            | Clause::Delete { .. }
            | Clause::Foreach { .. } => true,
            Clause::CallSubquery { query, .. } => query.may_write(),
            Clause::CallProcedure { name, .. } => !procedure_is_read_only(name),
        }
    }
}

/// The procedures known not to mutate the graph, by (lower-cased,
/// dot-joined) name. Deliberately a short allow-list rather than a deny-list:
/// a procedure missing from it merely runs inside a transaction.
fn procedure_is_read_only(name: &str) -> bool {
    name.starts_with("db.index.")
        || name.starts_with("db.schema.")
        || matches!(
            name,
            "db.labels" | "db.relationshiptypes" | "db.propertykeys" | "db.info" | "dbms.components"
        )
}
