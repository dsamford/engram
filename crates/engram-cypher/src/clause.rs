//! The clause parser — MATCH through CALL, on top of the expression parser.
//!
//! The parser is deliberately permissive about clause ORDER (a RETURN in the
//! middle parses); ordering is a semantic rule the binder owns, where it can
//! be stated once with a good message rather than encoded in a grammar that
//! produces "expected end of input".

use engram_observe::counted;

use crate::ast::Expr;
use crate::parser::{ParseError, Parser};
use crate::stmt::{
    Clause, NodePattern, OrderItem, PathPattern, Pattern, ProjItem, Projection, Query, RelDir,
    RelPattern, RemoveItem, SetItem, SingleQuery, SubqueryBody, VarLength,
};
use crate::token::TokenKind;

/// Parse a whole statement (one query, optional trailing `;`).
pub fn parse_statement(src: &str) -> Result<Query, ParseError> {
    let mut p = Parser::new(src)?;
    let q = p.query()?;
    let _ = p.eat(&TokenKind::Semicolon);
    p.expect_eof()?;
    counted!("cypher.statements parsed");
    Ok(q)
}

/// `(ORDER BY keys, SKIP, LIMIT)` -- the tail every projection may carry.
type ProjectionTail = (Vec<OrderItem>, Option<Expr>, Option<Expr>);

/// Keywords that BEGIN a clause — used to decide whether an `EXISTS {…}`
/// body is a subquery or a bare pattern.
fn starts_clause(k: &TokenKind) -> bool {
    matches!(
        k,
        TokenKind::Keyword(
            "MATCH"
                | "OPTIONAL"
                | "UNWIND"
                | "WITH"
                | "RETURN"
                | "CREATE"
                | "MERGE"
                | "SET"
                | "REMOVE"
                | "DELETE"
                | "DETACH"
                | "FOREACH"
                | "CALL"
        )
    )
}

impl Parser<'_> {
    pub(crate) fn query(&mut self) -> Result<Query, ParseError> {
        let first = self.single_query()?;
        if !matches!(self.peek(), TokenKind::Keyword("UNION")) {
            return Ok(Query::Single(first));
        }
        let mut arms = vec![first];
        let mut all: Option<bool> = None;
        while self.eat_kw("UNION") {
            let this_all = self.eat_kw("ALL");
            match all {
                None => all = Some(this_all),
                Some(prev) if prev != this_all => {
                    return self.refuse("consistent UNION / UNION ALL (mixing them is invalid)");
                }
                Some(_) => {}
            }
            arms.push(self.single_query()?);
        }
        Ok(Query::Union {
            all: all.expect("at least one UNION"),
            arms,
        })
    }

    fn single_query(&mut self) -> Result<SingleQuery, ParseError> {
        let mut clauses = Vec::new();
        loop {
            match self.peek() {
                TokenKind::Eof
                | TokenKind::Semicolon
                | TokenKind::RBrace
                | TokenKind::Keyword("UNION") => break,
                _ => self.clause_into(&mut clauses)?,
            }
        }
        if clauses.is_empty() {
            return self.refuse("a clause");
        }
        Ok(SingleQuery { clauses })
    }

    /// Parse one clause into `out` -- one AST clause, or the two a desugared
    /// form produces (see `Parser::pending`).
    fn clause_into(&mut self, out: &mut Vec<Clause>) -> Result<(), ParseError> {
        let first = self.clause()?;
        out.push(first);
        if let Some(second) = self.pending.take() {
            out.push(second);
        }
        Ok(())
    }

    fn clause(&mut self) -> Result<Clause, ParseError> {
        if self.eat_kw("OPTIONAL") {
            if !self.eat_kw("MATCH") {
                return self.refuse("MATCH after OPTIONAL");
            }
            return self.match_body(true);
        }
        if self.eat_kw("MATCH") {
            return self.match_body(false);
        }
        if self.eat_kw("UNWIND") {
            let expr = self.expr()?;
            if !self.eat_kw("AS") {
                return self.refuse("AS after the UNWIND expression");
            }
            let alias = self.name_token("a variable after AS")?;
            return Ok(Clause::Unwind { expr, alias });
        }
        if self.eat_kw("WITH") {
            let proj = self.projection()?;
            let where_ = if self.eat_kw("WHERE") {
                Some(self.expr()?)
            } else {
                None
            };
            // Neo4j (5.26 verified) also accepts the sub-clauses AFTER the WHERE:
            // `WITH s, count(e) AS n WHERE n >= 1 ORDER BY n DESC LIMIT 5`. openCypher
            // puts ORDER BY / SKIP / LIMIT before WHERE, and the two orders MEAN
            // different things — canonical `WITH … ORDER BY … LIMIT k WHERE p`
            // limits first and filters the k survivors; this form filters first and
            // orders/limits what is left. Its ORDER BY sees only the projected names
            // (Neo4j: "Variable `v` not defined" for a pre-projection variable),
            // which is exactly `WITH * ORDER BY … SKIP … LIMIT …` on the filtered
            // rows — so that is what it parses to: this WITH keeps the filter, and a
            // second, `*` WITH carries the tail. The platform's story-tracker
            // query was the first read the shadow instrument refused for it.
            if where_.is_some()
                && proj.order.is_empty()
                && proj.skip.is_none()
                && proj.limit.is_none()
                && matches!(self.peek(), TokenKind::Keyword("ORDER" | "SKIP" | "LIMIT"))
            {
                let (order, skip, limit) = self.order_skip_limit()?;
                counted!("cypher.with where-first tail desugared");
                self.pending = Some(Clause::With {
                    proj: Projection {
                        distinct: false,
                        star: true,
                        items: Vec::new(),
                        order,
                        skip,
                        limit,
                    },
                    where_: None,
                });
            }
            return Ok(Clause::With { proj, where_ });
        }
        if self.eat_kw("RETURN") {
            return Ok(Clause::Return {
                proj: self.projection()?,
            });
        }
        if self.eat_kw("CREATE") {
            return Ok(Clause::Create {
                pattern: self.pattern()?,
            });
        }
        if self.eat_kw("MERGE") {
            let path = self.path_pattern()?;
            let (mut on_create, mut on_match) = (Vec::new(), Vec::new());
            while self.eat_kw("ON") {
                if self.eat_kw("CREATE") {
                    if !self.eat_kw("SET") {
                        return self.refuse("SET after ON CREATE");
                    }
                    on_create.extend(self.set_items()?);
                } else if self.eat_kw("MATCH") {
                    if !self.eat_kw("SET") {
                        return self.refuse("SET after ON MATCH");
                    }
                    on_match.extend(self.set_items()?);
                } else {
                    return self.refuse("CREATE or MATCH after ON");
                }
            }
            return Ok(Clause::Merge {
                path,
                on_create,
                on_match,
            });
        }
        if self.eat_kw("SET") {
            return Ok(Clause::Set {
                items: self.set_items()?,
            });
        }
        if self.eat_kw("REMOVE") {
            let mut items = vec![self.remove_item()?];
            while self.eat(&TokenKind::Comma) {
                items.push(self.remove_item()?);
            }
            return Ok(Clause::Remove { items });
        }
        if self.eat_kw("DETACH") {
            if !self.eat_kw("DELETE") {
                return self.refuse("DELETE after DETACH");
            }
            return self.delete_body(true);
        }
        if self.eat_kw("DELETE") {
            return self.delete_body(false);
        }
        if self.eat_kw("FOREACH") {
            self.expect(&TokenKind::LParen, "`(` after FOREACH")?;
            let var = self.name_token("the FOREACH variable")?;
            if !self.eat_kw("IN") {
                return self.refuse("IN after the FOREACH variable");
            }
            let source = self.expr()?;
            self.expect(&TokenKind::Pipe, "`|` before the FOREACH updates")?;
            let mut updates = Vec::new();
            while !matches!(self.peek(), TokenKind::RParen | TokenKind::Eof) {
                self.clause_into(&mut updates)?;
            }
            self.expect(&TokenKind::RParen, "`)` closing FOREACH")?;
            if updates.is_empty() {
                return self.refuse("at least one update clause in FOREACH");
            }
            return Ok(Clause::Foreach {
                var,
                source,
                updates,
            });
        }
        if self.eat_kw("CALL") {
            // The Cypher-5 scoped form: CALL (a, b) { … } / CALL (*) { … }.
            if matches!(self.peek(), TokenKind::LParen) {
                self.bump();
                let mut imports = Vec::new();
                if self.eat(&TokenKind::Star) {
                    imports.push("*".to_string());
                } else if !matches!(self.peek(), TokenKind::RParen) {
                    loop {
                        imports.push(self.name_token("an imported variable")?);
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                }
                self.expect(&TokenKind::RParen, "`)` closing the import list")?;
                self.expect(&TokenKind::LBrace, "`{` opening the subquery")?;
                let query = self.query()?;
                self.expect(&TokenKind::RBrace, "`}` closing the subquery")?;
                return Ok(Clause::CallSubquery {
                    query: Box::new(query),
                    in_transactions: false,
                    imports,
                });
            }
            if self.eat(&TokenKind::LBrace) {
                let query = self.query()?;
                self.expect(&TokenKind::RBrace, "`}` closing the subquery")?;
                let mut in_transactions = false;
                if matches!(self.peek(), TokenKind::Keyword("IN"))
                    && self.ident_ahead(1, "TRANSACTIONS")
                {
                    self.bump();
                    self.bump();
                    in_transactions = true;
                    // `OF n ROWS` — accepted, batch size not yet modelled.
                    if self.ident_is("OF") {
                        self.bump();
                        let _ = self.expr()?;
                        if self.ident_is("ROWS") || self.ident_is("ROW") {
                            self.bump();
                        } else {
                            return self.refuse("ROWS after the batch size");
                        }
                    }
                }
                return Ok(Clause::CallSubquery {
                    query: Box::new(query),
                    in_transactions,
                    imports: Vec::new(),
                });
            }
            // A procedure call: dotted name, args, YIELD.
            let mut parts = vec![self.name_token("a procedure name")?];
            while self.eat(&TokenKind::Dot) {
                parts.push(self.name_token("a procedure name segment")?);
            }
            let name = parts.join(".").to_lowercase();
            let mut args = Vec::new();
            self.expect(&TokenKind::LParen, "`(` opening procedure arguments")?;
            if !matches!(self.peek(), TokenKind::RParen) {
                loop {
                    args.push(self.expr()?);
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            self.expect(&TokenKind::RParen, "`)` closing procedure arguments")?;
            let mut yields = Vec::new();
            if self.eat_kw("YIELD") {
                loop {
                    let field = self.name_token("a YIELD field")?;
                    let alias = if self.eat_kw("AS") {
                        Some(self.name_token("an alias")?)
                    } else {
                        None
                    };
                    yields.push((field, alias));
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            let where_ = if self.eat_kw("WHERE") {
                Some(self.expr()?)
            } else {
                None
            };
            return Ok(Clause::CallProcedure {
                name,
                args,
                yields,
                where_,
            });
        }
        self.refuse("a clause (MATCH, RETURN, WITH, CREATE, MERGE, …)")
    }

    fn ident_is(&self, word: &str) -> bool {
        matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case(word))
    }

    fn ident_ahead(&self, n: usize, word: &str) -> bool {
        matches!(self.peek_at(n), TokenKind::Ident(s) if s.eq_ignore_ascii_case(word))
    }

    fn match_body(&mut self, optional: bool) -> Result<Clause, ParseError> {
        let pattern = self.pattern()?;
        let where_ = if self.eat_kw("WHERE") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Clause::Match {
            optional,
            pattern,
            where_,
        })
    }

    fn delete_body(&mut self, detach: bool) -> Result<Clause, ParseError> {
        let mut exprs = vec![self.expr()?];
        while self.eat(&TokenKind::Comma) {
            exprs.push(self.expr()?);
        }
        Ok(Clause::Delete { detach, exprs })
    }

    // ── Projections ─────────────────────────────────────────────────────

    fn projection(&mut self) -> Result<Projection, ParseError> {
        let distinct = self.eat_kw("DISTINCT");
        let mut star = false;
        let mut items = Vec::new();
        if self.eat(&TokenKind::Star) {
            star = true;
            if self.eat(&TokenKind::Comma) {
                self.proj_items(&mut items)?;
            }
        } else {
            self.proj_items(&mut items)?;
        }
        let (order, skip, limit) = self.order_skip_limit()?;
        Ok(Projection {
            distinct,
            star,
            items,
            order,
            skip,
            limit,
        })
    }

    /// The `[ORDER BY …] [SKIP …] [LIMIT …]` tail of a projection. Shared by
    /// `projection()` and by the WITH form that puts them after WHERE.
    fn order_skip_limit(&mut self) -> Result<ProjectionTail, ParseError> {
        let mut order = Vec::new();
        if self.eat_kw("ORDER") {
            if !self.eat_kw("BY") {
                return self.refuse("BY after ORDER");
            }
            loop {
                let expr = self.value_expr()?;
                let desc = if self.eat_kw("DESC") || self.eat_kw("DESCENDING") {
                    true
                } else {
                    let _ = self.eat_kw("ASC") || self.eat_kw("ASCENDING");
                    false
                };
                order.push(OrderItem { expr, desc });
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let skip = if self.eat_kw("SKIP") {
            Some(self.expr()?)
        } else {
            None
        };
        let limit = if self.eat_kw("LIMIT") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok((order, skip, limit))
    }

    fn proj_items(&mut self, items: &mut Vec<ProjItem>) -> Result<(), ParseError> {
        loop {
            // Capture the expression's verbatim source (start-of-expr token to the
            // token that follows it) so an unaliased column keeps its exact text.
            let start = self.pos();
            let expr = self.value_expr()?;
            let text = Some(self.src[start..self.pos()].trim().to_string());
            let alias = if self.eat_kw("AS") {
                Some(self.name_token("an alias")?)
            } else {
                None
            };
            items.push(ProjItem { expr, alias, text });
            if !self.eat(&TokenKind::Comma) {
                return Ok(());
            }
        }
    }

    // ── SET / REMOVE items ──────────────────────────────────────────────

    fn set_items(&mut self) -> Result<Vec<SetItem>, ParseError> {
        let mut items = vec![self.set_item()?];
        while self.eat(&TokenKind::Comma) {
            items.push(self.set_item()?);
        }
        Ok(items)
    }

    fn set_item(&mut self) -> Result<SetItem, ParseError> {
        // A parenthesized base selects the entity by expression: `SET (n).name = …`
        // (openCypher's "simple expression" target). At least one `.prop` follows.
        if matches!(self.peek(), TokenKind::LParen) {
            self.bump();
            let inner = self.expr()?;
            self.expect(&TokenKind::RParen, "`)` after the SET target expression")?;
            let mut keys = Vec::new();
            while self.eat(&TokenKind::Dot) {
                keys.push(self.name_token("a property name")?);
            }
            if keys.is_empty() {
                return self.refuse("`.prop` after `(…)` in SET");
            }
            self.expect(&TokenKind::Eq, "`=` after the property")?;
            let value = self.value_expr()?;
            let key = keys.pop().expect("nonempty");
            let base = keys
                .into_iter()
                .fold(inner, |b, k| Expr::Prop(Box::new(b), k));
            return Ok(SetItem::Prop { base, key, value });
        }
        let var = self.name_token("a variable")?;
        if matches!(self.peek(), TokenKind::Colon) {
            let labels = self.label_list()?;
            return Ok(SetItem::Labels { var, labels });
        }
        // A property chain?
        let mut keys = Vec::new();
        while self.eat(&TokenKind::Dot) {
            keys.push(self.name_token("a property name")?);
        }
        if keys.is_empty() {
            if self.eat(&TokenKind::PlusEq) {
                return Ok(SetItem::Merge {
                    var,
                    value: self.value_expr()?,
                });
            }
            self.expect(&TokenKind::Eq, "`=`, `+=` or `:` after the SET variable")?;
            return Ok(SetItem::Replace {
                var,
                value: self.value_expr()?,
            });
        }
        self.expect(&TokenKind::Eq, "`=` after the property")?;
        let value = self.value_expr()?;
        let key = keys.pop().expect("nonempty");
        let base = keys
            .into_iter()
            .fold(Expr::Var(var), |b, k| Expr::Prop(Box::new(b), k));
        Ok(SetItem::Prop { base, key, value })
    }

    fn remove_item(&mut self) -> Result<RemoveItem, ParseError> {
        let var = self.name_token("a variable")?;
        if matches!(self.peek(), TokenKind::Colon) {
            let labels = self.label_list()?;
            return Ok(RemoveItem::Labels { var, labels });
        }
        let mut keys = Vec::new();
        while self.eat(&TokenKind::Dot) {
            keys.push(self.name_token("a property name")?);
        }
        if keys.is_empty() {
            return self.refuse("`.prop` or `:Label` after the REMOVE variable");
        }
        let key = keys.pop().expect("nonempty");
        let base = keys
            .into_iter()
            .fold(Expr::Var(var), |b, k| Expr::Prop(Box::new(b), k));
        Ok(RemoveItem::Prop { base, key })
    }

    fn label_list(&mut self) -> Result<Vec<String>, ParseError> {
        let mut labels = Vec::new();
        while self.eat(&TokenKind::Colon) {
            labels.push(self.name_token("a label")?);
        }
        Ok(labels)
    }

    // ── Patterns ────────────────────────────────────────────────────────

    pub(crate) fn pattern(&mut self) -> Result<Pattern, ParseError> {
        let mut paths = vec![self.path_pattern()?];
        while self.eat(&TokenKind::Comma) {
            paths.push(self.path_pattern()?);
        }
        Ok(Pattern { paths })
    }

    pub(crate) fn path_pattern(&mut self) -> Result<PathPattern, ParseError> {
        // `p = …`
        let var = if matches!(self.peek(), TokenKind::Ident(_))
            && matches!(self.peek_at(1), TokenKind::Eq)
        {
            let v = self.name_token("a path variable")?;
            self.bump(); // =
            Some(v)
        } else {
            None
        };
        let shortest = if matches!(self.peek(), TokenKind::Keyword("SHORTESTPATH")) {
            self.bump();
            self.expect(&TokenKind::LParen, "`(` after shortestPath")?;
            true
        } else {
            false
        };
        let start = self.node_pattern()?;
        let mut hops = Vec::new();
        while matches!(self.peek(), TokenKind::Minus | TokenKind::ArrowLeft) {
            let rel = self.rel_pattern()?;
            let node = self.node_pattern()?;
            hops.push((rel, node));
        }
        if shortest {
            self.expect(&TokenKind::RParen, "`)` closing shortestPath")?;
        }
        Ok(PathPattern {
            var,
            shortest,
            start,
            hops,
        })
    }

    fn node_pattern(&mut self) -> Result<NodePattern, ParseError> {
        self.expect(&TokenKind::LParen, "`(` opening a node pattern")?;
        let var = match self.peek() {
            TokenKind::Ident(_) => Some(self.name_token("a variable")?),
            // A keyword can name a node variable too (`(count)`), but only
            // when something node-ish follows; leave that rarity unsupported
            // rather than mis-parse `(TRUE)`.
            _ => None,
        };
        let labels = self.label_list()?;
        let props = match self.peek() {
            TokenKind::LBrace => Some(self.expr()?),
            TokenKind::Param(_) => Some(self.expr()?),
            _ => None,
        };
        self.expect(&TokenKind::RParen, "`)` closing a node pattern")?;
        Ok(NodePattern { var, labels, props })
    }

    fn rel_pattern(&mut self) -> Result<RelPattern, ParseError> {
        let left_arrow = if self.eat(&TokenKind::ArrowLeft) {
            true
        } else {
            self.expect(&TokenKind::Minus, "`-` or `<-` starting a relationship")?;
            false
        };
        let (mut var, mut types, mut length, mut props) = (None, Vec::new(), None, None);
        if self.eat(&TokenKind::LBracket) {
            if matches!(self.peek(), TokenKind::Ident(_)) {
                var = Some(self.name_token("a relationship variable")?);
            }
            if matches!(self.peek(), TokenKind::Colon) {
                self.bump();
                types.push(self.name_token("a relationship type")?);
                while self.eat(&TokenKind::Pipe) {
                    let _ = self.eat(&TokenKind::Colon); // `|:T` and `|T` both occur
                    types.push(self.name_token("a relationship type")?);
                }
            }
            if self.eat(&TokenKind::Star) {
                let min = match self.peek() {
                    TokenKind::Int(v) => {
                        let v = *v;
                        self.bump();
                        Some(self.non_negative(v)?)
                    }
                    _ => None,
                };
                if self.eat(&TokenKind::DotDot) {
                    let max = match self.peek() {
                        TokenKind::Int(v) => {
                            let v = *v;
                            self.bump();
                            Some(self.non_negative(v)?)
                        }
                        _ => None,
                    };
                    length = Some(VarLength { min, max });
                } else {
                    // `*2` is EXACTLY two; bare `*` is unbounded.
                    length = Some(VarLength { min, max: min });
                }
            }
            if matches!(self.peek(), TokenKind::LBrace | TokenKind::Param(_)) {
                props = Some(self.expr()?);
            }
            self.expect(&TokenKind::RBracket, "`]` closing a relationship")?;
        }
        let dir = if self.eat(&TokenKind::ArrowRight) {
            if left_arrow {
                // Arrowheads on BOTH ends (`<-[…]->`, `<-->`) are openCypher
                // UNDIRECTED — a redundant-but-legal spelling that matches an
                // edge in either direction, NOT a syntax error.
                RelDir::Undirected
            } else {
                RelDir::Out
            }
        } else {
            self.expect(&TokenKind::Minus, "`-` or `->` closing a relationship")?;
            if left_arrow {
                RelDir::In
            } else {
                RelDir::Undirected
            }
        };
        Ok(RelPattern {
            var,
            types,
            dir,
            props,
            length,
        })
    }

    fn non_negative(&self, v: i64) -> Result<u64, ParseError> {
        u64::try_from(v).map_err(|_| ParseError::Unexpected {
            expected: "a non-negative length".to_string(),
            found: v.to_string(),
            at: self.pos(),
        })
    }

    // ── Subquery-expression bodies (EXISTS { … } / COUNT { … }) ─────────

    pub(crate) fn subquery_body(&mut self) -> Result<SubqueryBody, ParseError> {
        if starts_clause(self.peek()) {
            let q = self.single_query()?;
            // An EXISTS / COUNT subquery is a PREDICATE — read-only by the
            // language (Neo4j refuses an updating clause inside one too).
            // The refusal is also load-bearing here: the server decides
            // whether a statement runs inside a transaction from its syntax
            // (`Stmt::may_write`), which does not descend into expressions —
            // an update hidden in a predicate would write outside the
            // transaction the rest of the statement runs in.
            if q.may_write() {
                return self.refuse("an updating clause inside an EXISTS / COUNT subquery");
            }
            return Ok(SubqueryBody::Query(q));
        }
        let pattern = self.pattern()?;
        let where_ = if self.eat_kw("WHERE") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(SubqueryBody::Pattern { pattern, where_ })
    }
}

// ─── DDL ────────────────────────────────────────────────────────────────────

use crate::stmt::{ConstraintKind, SchemaCmd, Stmt};

/// Parse any statement — a query or a schema command.
pub fn parse_any(src: &str) -> Result<Stmt, ParseError> {
    let mut p = Parser::new(src)?;
    let is_schema = matches!(p.peek(), TokenKind::Keyword("DROP" | "SHOW"))
        || (matches!(p.peek(), TokenKind::Keyword("CREATE"))
            && matches!(
                p.peek_at(1),
                TokenKind::Keyword("CONSTRAINT" | "VECTOR" | "FULLTEXT" | "INDEX")
            ));
    if !is_schema {
        let q = p.query()?;
        let _ = p.eat(&TokenKind::Semicolon);
        p.expect_eof()?;
        counted!("cypher.statements parsed");
        return Ok(Stmt::Query(q));
    }
    let cmd = p.schema_cmd()?;
    let _ = p.eat(&TokenKind::Semicolon);
    p.expect_eof()?;
    counted!("cypher.statements parsed");
    Ok(Stmt::Schema(cmd))
}

impl Parser<'_> {
    fn if_not_exists(&mut self) -> Result<bool, ParseError> {
        if self.eat_kw("IF") {
            if !(self.eat_kw("NOT") && self.eat_kw("EXISTS")) {
                return self.refuse("NOT EXISTS after IF");
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// `FOR (n:Label)` — ONE label; the two-label form is the measured
    /// silent-failure defect and refuses with its shape named.
    fn for_single_label(&mut self) -> Result<(String, String), ParseError> {
        if !self.eat_kw("FOR") {
            return self.refuse("FOR");
        }
        self.expect(&TokenKind::LParen, "`(` after FOR")?;
        let var = self.name_token("a variable")?;
        self.expect(&TokenKind::Colon, "`:` before the label")?;
        let label = self.name_token("a label")?;
        if matches!(self.peek(), TokenKind::Colon) {
            return self.refuse(
                "`)` — a two-label FOR clause is a Cypher syntax error (one label per index/constraint)",
            );
        }
        self.expect(&TokenKind::RParen, "`)` closing FOR")?;
        Ok((var, label))
    }

    /// The `FOR` target of a constraint: the node form `(n:Label)` or the
    /// relationship form `()-[r:TYPE]-()`. Returns the pattern variable, the
    /// label (node) or type (rel), and whether it was the relationship form.
    fn for_node_or_rel(&mut self) -> Result<(String, String, bool), ParseError> {
        if !self.eat_kw("FOR") {
            return self.refuse("FOR");
        }
        self.expect(&TokenKind::LParen, "`(` after FOR")?;
        // The relationship form opens with an EMPTY node `()`; the node form's
        // first token inside the paren is the variable.
        if self.eat(&TokenKind::RParen) {
            self.expect(&TokenKind::Minus, "`-` starting the relationship")?;
            self.expect(&TokenKind::LBracket, "`[`")?;
            let var = self.name_token("a relationship variable")?;
            self.expect(&TokenKind::Colon, "`:` before the relationship type")?;
            let rel_type = self.name_token("a relationship type")?;
            self.expect(&TokenKind::RBracket, "`]`")?;
            self.expect(&TokenKind::Minus, "`-` after the relationship")?;
            self.expect(&TokenKind::LParen, "`(`")?;
            self.expect(&TokenKind::RParen, "`)` closing the relationship pattern")?;
            return Ok((var, rel_type, true));
        }
        let var = self.name_token("a variable")?;
        self.expect(&TokenKind::Colon, "`:` before the label")?;
        let label = self.name_token("a label")?;
        if matches!(self.peek(), TokenKind::Colon) {
            return self.refuse(
                "`)` — a two-label FOR clause is a Cypher syntax error (one label per index/constraint)",
            );
        }
        self.expect(&TokenKind::RParen, "`)` closing FOR")?;
        Ok((var, label, false))
    }

    fn prop_ref(&mut self, var: &str) -> Result<String, ParseError> {
        let v = self.name_token("the bound variable")?;
        if v != var {
            return self.refuse("the FOR clause variable");
        }
        self.expect(&TokenKind::Dot, "`.` before the property")?;
        self.name_token("a property name")
    }

    fn schema_cmd(&mut self) -> Result<SchemaCmd, ParseError> {
        if self.eat_kw("SHOW") {
            // SHOW parses loosely (subject + the rest consumed unvalidated) —
            // it is the language, so a parse-rate measurement must not count
            // it as a grammar gap. Whether a tail existed is RECORDED: the
            // executor answers implemented subjects, but must refuse when a
            // tail was swallowed rather than pretend the projection ran.
            let subject = self.name_token("a SHOW subject")?;
            let tail = !matches!(self.peek(), TokenKind::Eof | TokenKind::Semicolon);
            while !matches!(self.peek(), TokenKind::Eof | TokenKind::Semicolon) {
                self.bump();
            }
            return Ok(SchemaCmd::Show { subject, tail });
        }
        if self.eat_kw("DROP") {
            let is_constraint = if self.eat_kw("CONSTRAINT") {
                true
            } else if self.eat_kw("INDEX") {
                false
            } else {
                return self.refuse("INDEX or CONSTRAINT after DROP");
            };
            let name = self.name_token("a name")?;
            let if_exists = if self.eat_kw("IF") {
                if !self.eat_kw("EXISTS") {
                    return self.refuse("EXISTS after IF");
                }
                true
            } else {
                false
            };
            return Ok(if is_constraint {
                SchemaCmd::DropConstraint { name, if_exists }
            } else {
                SchemaCmd::DropIndex { name, if_exists }
            });
        }
        if !self.eat_kw("CREATE") {
            return self.refuse("CREATE or DROP");
        }
        if self.eat_kw("CONSTRAINT") {
            let name = match self.peek() {
                TokenKind::Keyword("IF" | "FOR") => None,
                _ => Some(self.name_token("a constraint name")?),
            };
            let if_not_exists = self.if_not_exists()?;
            let (var, label, on_relationships) = self.for_node_or_rel()?;
            if !self.eat_kw("REQUIRE") {
                return self.refuse("REQUIRE");
            }
            // One property, or the composite `(n.a, n.b)` form.
            let props = if self.eat(&TokenKind::LParen) {
                let mut ps = vec![self.prop_ref(&var)?];
                while self.eat(&TokenKind::Comma) {
                    ps.push(self.prop_ref(&var)?);
                }
                self.expect(&TokenKind::RParen, "`)` closing the property list")?;
                ps
            } else {
                vec![self.prop_ref(&var)?]
            };
            if !self.eat_kw("IS") {
                return self.refuse("IS after the property");
            }
            let kind = if self.eat_kw("UNIQUE") {
                ConstraintKind::Unique
            } else if self.eat_kw("NOT") {
                if !self.eat_kw("NULL") {
                    return self.refuse("NULL after IS NOT");
                }
                if props.len() > 1 {
                    return self.refuse("a single property (IS NOT NULL is per-property)");
                }
                ConstraintKind::NotNull
            } else if matches!(self.peek(), TokenKind::Ident(w)
                if w.eq_ignore_ascii_case("node")
                    || w.eq_ignore_ascii_case("relationship")
                    || w.eq_ignore_ascii_case("rel"))
            {
                // `IS NODE KEY` / `IS RELATIONSHIP KEY` — same kind (every
                // property required AND the tuple unique); the node-vs-rel
                // scope is carried by `on_relationships`, not by the kind.
                self.bump();
                match self.peek() {
                    TokenKind::Ident(w) if w.eq_ignore_ascii_case("key") => {
                        self.bump();
                        ConstraintKind::NodeKey
                    }
                    _ => return self.refuse("KEY after NODE/RELATIONSHIP"),
                }
            } else {
                return self.refuse("UNIQUE, NOT NULL or NODE/RELATIONSHIP KEY");
            };
            return Ok(SchemaCmd::CreateConstraint {
                name,
                if_not_exists,
                label,
                props,
                kind,
                on_relationships,
            });
        }
        if self.eat_kw("VECTOR") {
            if !self.eat_kw("INDEX") {
                return self.refuse("INDEX after VECTOR");
            }
            let name = self.name_token("an index name")?;
            let if_not_exists = self.if_not_exists()?;
            let (var, label) = self.for_single_label()?;
            if !self.eat_kw("ON") {
                return self.refuse("ON");
            }
            self.expect(&TokenKind::LParen, "`(` after ON")?;
            let prop = self.prop_ref(&var)?;
            self.expect(&TokenKind::RParen, "`)` closing ON")?;
            let options = if self.eat_kw("OPTIONS") {
                Some(self.expr()?)
            } else {
                None
            };
            return Ok(SchemaCmd::CreateVectorIndex {
                name,
                if_not_exists,
                label,
                prop,
                options,
            });
        }
        if self.eat_kw("FULLTEXT") {
            if !self.eat_kw("INDEX") {
                return self.refuse("INDEX after FULLTEXT");
            }
            let name = self.name_token("an index name")?;
            let if_not_exists = self.if_not_exists()?;
            if !self.eat_kw("FOR") {
                return self.refuse("FOR");
            }
            self.expect(&TokenKind::LParen, "`(` after FOR")?;
            let var = self.name_token("a variable")?;
            self.expect(&TokenKind::Colon, "`:` before the label")?;
            let mut labels = vec![self.name_token("a label")?];
            while self.eat(&TokenKind::Pipe) {
                labels.push(self.name_token("a label")?);
            }
            self.expect(&TokenKind::RParen, "`)` closing FOR")?;
            if !self.eat_kw("ON") {
                return self.refuse("ON");
            }
            if !self.eat_kw("EACH") {
                return self.refuse("EACH after ON");
            }
            self.expect(&TokenKind::LBracket, "`[` opening the property list")?;
            let mut props = vec![self.prop_ref(&var)?];
            while self.eat(&TokenKind::Comma) {
                props.push(self.prop_ref(&var)?);
            }
            self.expect(&TokenKind::RBracket, "`]` closing the property list")?;
            return Ok(SchemaCmd::CreateFulltextIndex {
                name,
                if_not_exists,
                labels,
                props,
            });
        }
        if self.eat_kw("INDEX") {
            let name = match self.peek() {
                TokenKind::Keyword("IF" | "FOR") => None,
                _ => Some(self.name_token("an index name")?),
            };
            let if_not_exists = self.if_not_exists()?;
            // The relationship form: FOR ()-[r:TYPE]-().
            if matches!(self.peek(), TokenKind::Keyword("FOR"))
                && matches!(self.peek_at(1), TokenKind::LParen)
                && matches!(self.peek_at(2), TokenKind::RParen)
            {
                self.bump(); // FOR
                self.expect(&TokenKind::LParen, "`(`")?;
                self.expect(&TokenKind::RParen, "`)`")?;
                self.expect(&TokenKind::Minus, "`-`")?;
                self.expect(&TokenKind::LBracket, "`[`")?;
                let var = self.name_token("a relationship variable")?;
                self.expect(&TokenKind::Colon, "`:`")?;
                let rel_type = self.name_token("a relationship type")?;
                self.expect(&TokenKind::RBracket, "`]`")?;
                self.expect(&TokenKind::Minus, "`-`")?;
                self.expect(&TokenKind::LParen, "`(`")?;
                self.expect(&TokenKind::RParen, "`)`")?;
                if !self.eat_kw("ON") {
                    return self.refuse("ON");
                }
                self.expect(&TokenKind::LParen, "`(` after ON")?;
                let mut props = vec![self.prop_ref(&var)?];
                while self.eat(&TokenKind::Comma) {
                    props.push(self.prop_ref(&var)?);
                }
                self.expect(&TokenKind::RParen, "`)` closing ON")?;
                let name = name.unwrap_or_else(|| format!("rel_range_{rel_type}"));
                return Ok(SchemaCmd::CreateRangeIndex {
                    name: Some(name),
                    if_not_exists,
                    label: rel_type,
                    props,
                    on_relationships: true,
                });
            }
            let (var, label) = self.for_single_label()?;
            if !self.eat_kw("ON") {
                return self.refuse("ON");
            }
            self.expect(&TokenKind::LParen, "`(` after ON")?;
            let mut props = vec![self.prop_ref(&var)?];
            while self.eat(&TokenKind::Comma) {
                props.push(self.prop_ref(&var)?);
            }
            self.expect(&TokenKind::RParen, "`)` closing ON")?;
            return Ok(SchemaCmd::CreateRangeIndex {
                name,
                if_not_exists,
                label,
                props,
                on_relationships: false,
            });
        }
        self.refuse("CONSTRAINT, INDEX, VECTOR INDEX or FULLTEXT INDEX")
    }
}
