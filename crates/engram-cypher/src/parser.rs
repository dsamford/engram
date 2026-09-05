//! The expression parser — recursive descent, one function per precedence
//! level, positions on every refusal.
//!
//! The level ladder follows openCypher's grammar (loosest first):
//! `OR` → `XOR` → `AND` → `NOT` → comparison (CHAINED: `1 < x < 10` is the
//! conjunction) → string/list/null operators (`STARTS WITH`, `IN`,
//! `IS NULL`, `=~` — these bind TIGHTER than `=`) → `+ -` → `* / %` → `^` →
//! unary `-` → postfix (`.prop`, `[i]`, `[a..b]`) → atoms.

use engram_observe::{counted, sometimes};

use crate::ast::{BinOp, Expr};
use crate::token::{LexError, Token, TokenKind, tokenize};

/// Why a parse refused. Always positioned, always names what was expected —
/// "expected X, found Y at byte N" is the whole contract.
#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    /// The tokenizer refused.
    Lex(LexError),
    /// The parser refused.
    Unexpected {
        /// What the grammar wanted here.
        expected: String,
        /// What it found.
        found: String,
        /// Byte offset.
        at: usize,
    },
    /// A map literal with one key twice — refused rather than last-wins,
    /// because `{a: 1, a: 2}` is always a bug at the call site.
    DuplicateMapKey {
        /// The key.
        key: String,
        /// Byte offset of the second occurrence.
        at: usize,
    },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Lex(e) => write!(f, "{e}"),
            ParseError::Unexpected {
                expected,
                found,
                at,
            } => {
                write!(f, "expected {expected}, found {found} at byte {at}")
            }
            ParseError::DuplicateMapKey { key, at } => {
                write!(f, "duplicate map key `{key}` at byte {at}")
            }
        }
    }
}

impl std::error::Error for ParseError {}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        ParseError::Lex(e)
    }
}

/// Parse one expression; the whole input must be consumed.
pub fn parse_expression(src: &str) -> Result<Expr, ParseError> {
    let mut p = Parser::new(src)?;
    let e = p.expr()?;
    p.expect_eof()?;
    counted!("cypher.expressions parsed");
    Ok(e)
}

/// The deepest expression nesting the parser will build.
///
/// # Why 64, measured rather than guessed
///
/// Two numbers bound this from opposite sides, and both were measured:
///
/// - **The floor is 40.** `expressions/literals/Literals7.feature` and
///   `Literals8.feature` in the vendored openCypher TCK nest list literals 40
///   deep. A limit at or below that costs conformance, so anything under ~48 is
///   not available. (For contrast, the deepest expression in a 3,547-statement
///   real application corpus is **5** — the TCK is deliberately adversarial and
///   real queries are nowhere near it.)
/// - **The ceiling is the stack.** The descent ladder is ~12 frames per nesting
///   level, and an unoptimised build's frames are large: on a 1 MiB stack a
///   DEBUG build overflows between 30 and 40 levels — i.e. before a limit of 128
///   could ever fire. A release build clears 128 comfortably.
///
/// 64 sits above the conformance floor with margin and below anything
/// pathological. But note what that arithmetic means: the limit is only a
/// defence if the stack is big enough for it to be reached. In debug that is
/// ~2 MiB. **A caller that parses on a thread it spawned must give that thread
/// an explicit stack size** — the server does, precisely so this limit is a
/// guarantee rather than a coincidence of whatever the platform defaulted to.
pub(crate) const MAX_EXPR_DEPTH: u32 = 64;

/// The stack a thread must have for `MAX_EXPR_DEPTH` to be reachable rather
/// than academic. Derived from the measurement above (~12 frames per level,
/// unoptimised) with a factor of two of headroom.
pub const MIN_PARSER_STACK_BYTES: usize = 4 * 1024 * 1024;

pub(crate) struct Parser<'a> {
    pub(crate) src: &'a str,
    pub(crate) tokens: Vec<Token>,
    pub(crate) at: usize,
    /// Whether a BARE relationship pattern (`(a)-[:R]->(b)`) is a legal value in
    /// the position currently being parsed. It is a predicate, so it is allowed
    /// in WHERE and other predicate spots (the default) but NOT as a RETURN/WITH
    /// projection item, an ORDER BY / SKIP / LIMIT key, or a SET right-hand side
    /// — openCypher raises `UnexpectedSyntax` there. `exists((a)-[:R]->(b))` and
    /// `exists { … }` stay legal everywhere (their own parser branches don't
    /// consult this flag).
    pub(crate) allow_bare_pattern: bool,
    /// Expression nesting currently open. See [`MAX_EXPR_DEPTH`].
    pub(crate) depth: u32,
    /// A clause the one just parsed DESUGARED into two: the WITH form that puts
    /// ORDER BY / SKIP / LIMIT after its WHERE parses as that WITH plus a `WITH *`
    /// carrying the tail. `clause()` returns the first and leaves the second here;
    /// every clause-sequence loop drains it (`clause_into`).
    pub(crate) pending: Option<crate::stmt::Clause>,
}

impl<'a> Parser<'a> {
    pub(crate) fn new(src: &'a str) -> Result<Parser<'a>, ParseError> {
        Ok(Parser {
            src,
            tokens: tokenize(src)?,
            at: 0,
            allow_bare_pattern: true,
            depth: 0,
            pending: None,
        })
    }

    /// Parse an expression in a VALUE position, where a bare relationship
    /// pattern is illegal (a projection item, ORDER BY key, or SET RHS). The
    /// previous setting is restored so a nested predicate (a comprehension /
    /// subquery WHERE) can re-permit it.
    pub(crate) fn value_expr(&mut self) -> Result<Expr, ParseError> {
        let saved = self.allow_bare_pattern;
        self.allow_bare_pattern = false;
        let r = self.expr();
        self.allow_bare_pattern = saved;
        r
    }

    pub(crate) fn peek(&self) -> &TokenKind {
        &self.tokens[self.at].kind
    }

    pub(crate) fn peek_at(&self, n: usize) -> &TokenKind {
        &self.tokens[(self.at + n).min(self.tokens.len() - 1)].kind
    }

    pub(crate) fn pos(&self) -> usize {
        self.tokens[self.at].at
    }

    pub(crate) fn bump(&mut self) -> TokenKind {
        let t = self.tokens[self.at].kind.clone();
        if self.at + 1 < self.tokens.len() {
            self.at += 1;
        }
        t
    }

    pub(crate) fn eat(&mut self, k: &TokenKind) -> bool {
        if self.peek() == k {
            self.bump();
            true
        } else {
            false
        }
    }

    pub(crate) fn eat_kw(&mut self, kw: &str) -> bool {
        if matches!(self.peek(), TokenKind::Keyword(k) if *k == kw) {
            self.bump();
            true
        } else {
            false
        }
    }

    pub(crate) fn refuse<T>(&self, expected: &str) -> Result<T, ParseError> {
        sometimes!("cypher.parse refused", true);
        Err(ParseError::Unexpected {
            expected: expected.to_string(),
            found: format!("{:?}", self.peek()),
            at: self.pos(),
        })
    }

    pub(crate) fn expect(&mut self, k: &TokenKind, expected: &str) -> Result<(), ParseError> {
        if self.eat(k) {
            Ok(())
        } else {
            self.refuse(expected)
        }
    }

    pub(crate) fn expect_eof(&self) -> Result<(), ParseError> {
        if matches!(self.peek(), TokenKind::Eof) {
            Ok(())
        } else {
            self.refuse("end of input")
        }
    }

    /// A name-position token as text: identifiers as written; keywords by
    /// re-slicing the SOURCE, so `{count: 1}` keeps the user's spelling
    /// rather than the canonical upper-case.
    pub(crate) fn name_token(&mut self, expected: &str) -> Result<String, ParseError> {
        let at = self.pos();
        match self.peek().clone() {
            TokenKind::Ident(s) => {
                self.bump();
                Ok(s)
            }
            TokenKind::Keyword(k) => {
                self.bump();
                Ok(self.src[at..at + k.len()].to_string())
            }
            _ => self.refuse(expected),
        }
    }

    // ── The ladder ──────────────────────────────────────────────────────

    /// Parse an expression, bounded in nesting depth.
    ///
    /// This is the ONE choke point for expression recursion: every nested
    /// construct — a parenthesised expression, a list or map literal, function
    /// arguments, `CASE`, a comprehension, a subquery predicate — re-enters the
    /// grammar here. Guarding it therefore bounds the whole cycle
    /// (`expr → … → atom → expr`), where guarding each call site individually
    /// would leave a hole the first time a construct is added.
    ///
    /// Without this, `RETURN ((((…1…))))` at 200k parens recursed 200k frames
    /// before reaching a token that could even be rejected, and a Rust stack
    /// overflow is `abort()`, not a catchable panic — so an unauthenticated
    /// client killed the whole server, and every other session on it, with one
    /// string. The query text arrives from the wire with no length limit.
    pub(crate) fn expr(&mut self) -> Result<Expr, ParseError> {
        self.deeper(Self::or_expr)
    }

    /// Run one grammar rule one level deeper, refusing past [`MAX_EXPR_DEPTH`].
    ///
    /// EVERY recursive entry point goes through here. That is not tidiness: the
    /// first version of this guard sat only in `expr`, on the reasoning that
    /// every nested construct re-enters the grammar there — which is true of
    /// brackets and false of the ladder's own rungs. `not_expr` and `unary`
    /// recurse into THEMSELVES without passing `expr`, so `RETURN ----…1` and
    /// `RETURN NOT NOT …` sailed past the limit and still overflowed the stack,
    /// at one byte per level — cheaper than the bracket attack the guard was
    /// written for. A single helper makes the next rung that recurses fail
    /// loudly rather than silently reopening the hole.
    fn deeper<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, ParseError>,
    ) -> Result<T, ParseError> {
        if self.depth >= MAX_EXPR_DEPTH {
            return self.refuse("an expression nested no deeper than the limit");
        }
        self.depth += 1;
        let r = f(self);
        // Decrement on BOTH paths. An early `?` here would poison the parser's
        // depth for the rest of the statement, turning one refused expression
        // into a spurious refusal of every later one.
        self.depth -= 1;
        r
    }

    fn or_expr(&mut self) -> Result<Expr, ParseError> {
        let mut e = self.xor_expr()?;
        while self.eat_kw("OR") {
            e = Expr::Or(Box::new(e), Box::new(self.xor_expr()?));
        }
        Ok(e)
    }

    fn xor_expr(&mut self) -> Result<Expr, ParseError> {
        let mut e = self.and_expr()?;
        while self.eat_kw("XOR") {
            e = Expr::Xor(Box::new(e), Box::new(self.and_expr()?));
        }
        Ok(e)
    }

    fn and_expr(&mut self) -> Result<Expr, ParseError> {
        let mut e = self.not_expr()?;
        while self.eat_kw("AND") {
            e = Expr::And(Box::new(e), Box::new(self.not_expr()?));
        }
        Ok(e)
    }

    fn not_expr(&mut self) -> Result<Expr, ParseError> {
        if self.eat_kw("NOT") {
            // Self-recursive rung: costs one keyword per level and never
            // re-enters `expr`, so it needs its own depth accounting.
            return Ok(Expr::Not(Box::new(self.deeper(Self::not_expr)?)));
        }
        self.comparison()
    }

    /// Chained comparison: `a < b <= c` is `a < b AND b <= c` — openCypher's
    /// reading, with the middle expression duplicated rather than re-evaluated
    /// lazily (expressions here are pure).
    fn comparison(&mut self) -> Result<Expr, ParseError> {
        let first = self.string_list_null()?;
        let mut legs: Vec<(BinOp, Expr)> = Vec::new();
        loop {
            let op = match self.peek() {
                TokenKind::Eq => BinOp::Eq,
                TokenKind::Neq => BinOp::Neq,
                TokenKind::Lt => BinOp::Lt,
                TokenKind::Le => BinOp::Le,
                TokenKind::Gt => BinOp::Gt,
                TokenKind::Ge => BinOp::Ge,
                _ => break,
            };
            self.bump();
            legs.push((op, self.string_list_null()?));
        }
        match legs.len() {
            0 => Ok(first),
            1 => {
                let (op, rhs) = legs.pop().expect("one leg");
                Ok(Expr::Bin(op, Box::new(first), Box::new(rhs)))
            }
            _ => {
                let mut lhs = first;
                let mut conj: Option<Expr> = None;
                for (op, rhs) in legs {
                    let leg = Expr::Bin(op, Box::new(lhs.clone()), Box::new(rhs.clone()));
                    conj = Some(match conj {
                        None => leg,
                        Some(c) => Expr::And(Box::new(c), Box::new(leg)),
                    });
                    lhs = rhs;
                }
                Ok(conj.expect("legs nonempty"))
            }
        }
    }

    fn string_list_null(&mut self) -> Result<Expr, ParseError> {
        let mut e = self.additive()?;
        loop {
            if self.eat_kw("IN") {
                e = Expr::In(Box::new(e), Box::new(self.additive()?));
            } else if self.eat_kw("STARTS") {
                if !self.eat_kw("WITH") {
                    return self.refuse("WITH after STARTS");
                }
                e = Expr::Bin(BinOp::StartsWith, Box::new(e), Box::new(self.additive()?));
            } else if self.eat_kw("ENDS") {
                if !self.eat_kw("WITH") {
                    return self.refuse("WITH after ENDS");
                }
                e = Expr::Bin(BinOp::EndsWith, Box::new(e), Box::new(self.additive()?));
            } else if self.eat_kw("CONTAINS") {
                e = Expr::Bin(BinOp::Contains, Box::new(e), Box::new(self.additive()?));
            } else if self.eat(&TokenKind::RegexMatch) {
                e = Expr::Bin(BinOp::Regex, Box::new(e), Box::new(self.additive()?));
            } else if self.eat_kw("IS") {
                let negated = self.eat_kw("NOT");
                if !self.eat_kw("NULL") {
                    return self.refuse("NULL after IS");
                }
                e = Expr::IsNull {
                    of: Box::new(e),
                    negated,
                };
            } else {
                return Ok(e);
            }
        }
    }

    fn additive(&mut self) -> Result<Expr, ParseError> {
        let mut e = self.multiplicative()?;
        loop {
            let op = match self.peek() {
                TokenKind::Plus => BinOp::Add,
                TokenKind::Minus => BinOp::Sub,
                _ => return Ok(e),
            };
            self.bump();
            e = Expr::Bin(op, Box::new(e), Box::new(self.multiplicative()?));
        }
    }

    fn multiplicative(&mut self) -> Result<Expr, ParseError> {
        let mut e = self.power()?;
        loop {
            let op = match self.peek() {
                TokenKind::Star => BinOp::Mul,
                TokenKind::Slash => BinOp::Div,
                TokenKind::Percent => BinOp::Mod,
                _ => return Ok(e),
            };
            self.bump();
            e = Expr::Bin(op, Box::new(e), Box::new(self.power()?));
        }
    }

    fn power(&mut self) -> Result<Expr, ParseError> {
        let mut e = self.unary()?;
        while self.eat(&TokenKind::Caret) {
            // Left-associative, matching the openCypher grammar's flat list.
            e = Expr::Bin(BinOp::Pow, Box::new(e), Box::new(self.unary()?));
        }
        Ok(e)
    }

    fn unary(&mut self) -> Result<Expr, ParseError> {
        if self.eat(&TokenKind::Minus) {
            // Self-recursive rung: ONE BYTE per level, the cheapest stack
            // attack in the grammar. See `deeper`.
            return Ok(Expr::Neg(Box::new(self.deeper(Self::unary)?)));
        }
        if self.eat(&TokenKind::Plus) {
            return self.deeper(Self::unary);
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expr, ParseError> {
        let mut e = self.atom()?;
        loop {
            // `n:Label` — the label predicate, tightest-binding postfix.
            if matches!(self.peek(), TokenKind::Colon)
                && matches!(self.peek_at(1), TokenKind::Ident(_) | TokenKind::Keyword(_))
            {
                let mut labels = Vec::new();
                while self.eat(&TokenKind::Colon) {
                    labels.push(self.name_token("a label")?);
                }
                e = Expr::HasLabels {
                    of: Box::new(e),
                    labels,
                };
                continue;
            }
            // `n {.a, .*, k: v, var}` — a map projection.
            if matches!(self.peek(), TokenKind::LBrace) {
                e = self.map_projection(e)?;
                continue;
            }
            if self.eat(&TokenKind::Dot) {
                let key = self.name_token("a property name after `.`")?;
                e = Expr::Prop(Box::new(e), key);
            } else if self.eat(&TokenKind::LBracket) {
                if self.eat(&TokenKind::DotDot) {
                    // [..b] or [..]
                    let to = if matches!(self.peek(), TokenKind::RBracket) {
                        None
                    } else {
                        Some(Box::new(self.expr()?))
                    };
                    self.expect(&TokenKind::RBracket, "`]` closing a slice")?;
                    e = Expr::Slice {
                        of: Box::new(e),
                        from: None,
                        to,
                    };
                } else {
                    let first = self.expr()?;
                    if self.eat(&TokenKind::DotDot) {
                        let to = if matches!(self.peek(), TokenKind::RBracket) {
                            None
                        } else {
                            Some(Box::new(self.expr()?))
                        };
                        self.expect(&TokenKind::RBracket, "`]` closing a slice")?;
                        e = Expr::Slice {
                            of: Box::new(e),
                            from: Some(Box::new(first)),
                            to,
                        };
                    } else {
                        self.expect(&TokenKind::RBracket, "`]` closing an index")?;
                        e = Expr::Index(Box::new(e), Box::new(first));
                    }
                }
            } else {
                return Ok(e);
            }
        }
    }

    fn atom(&mut self) -> Result<Expr, ParseError> {
        match self.peek().clone() {
            TokenKind::Int(v) => {
                self.bump();
                Ok(Expr::Int(v))
            }
            TokenKind::Float(v) => {
                self.bump();
                Ok(Expr::Float(v))
            }
            TokenKind::Str(s) => {
                self.bump();
                Ok(Expr::Str(s))
            }
            TokenKind::Param(p) => {
                self.bump();
                Ok(Expr::Param(p))
            }
            TokenKind::Keyword("TRUE") => {
                self.bump();
                Ok(Expr::Bool(true))
            }
            TokenKind::Keyword("FALSE") => {
                self.bump();
                Ok(Expr::Bool(false))
            }
            TokenKind::Keyword("NULL") => {
                self.bump();
                Ok(Expr::Null)
            }
            TokenKind::Keyword("CASE") => self.case_expr(),
            TokenKind::Keyword("ALL")
                if matches!(self.peek_at(1), TokenKind::LParen)
                    && matches!(self.peek_at(2), TokenKind::Ident(_))
                    && matches!(self.peek_at(3), TokenKind::Keyword("IN")) =>
            {
                self.list_predicate(crate::ast::ListPredicateKind::All)
            }
            TokenKind::Keyword("COUNT") if matches!(self.peek_at(1), TokenKind::LParen) => {
                // count(...) — the keyword doubling as the aggregate's name.
                self.bump();
                self.call_args("count".to_string())
            }
            TokenKind::Keyword("EXISTS") if matches!(self.peek_at(1), TokenKind::LParen) => {
                self.bump();
                // `exists((a)-[:R]->(b))` is the PATTERN predicate, not the
                // property-null test — rewritten here so the evaluator never
                // has to guess which one a boolean argument meant.
                let saved = self.at;
                self.bump(); // (
                if let Ok(path) = self.path_pattern() {
                    if !path.hops.is_empty() && self.eat(&TokenKind::RParen) {
                        return Ok(Expr::PatternPredicate(Box::new(path)));
                    }
                }
                self.at = saved;
                self.call_args("exists".to_string())
            }
            TokenKind::Keyword("EXISTS") if matches!(self.peek_at(1), TokenKind::LBrace) => {
                self.bump();
                self.bump();
                let body = self.subquery_body()?;
                self.expect(&TokenKind::RBrace, "`}` closing EXISTS")?;
                Ok(Expr::ExistsSub(Box::new(body)))
            }
            TokenKind::Keyword("COUNT") if matches!(self.peek_at(1), TokenKind::LBrace) => {
                self.bump();
                self.bump();
                let body = self.subquery_body()?;
                self.expect(&TokenKind::RBrace, "`}` closing COUNT")?;
                Ok(Expr::CountSub(Box::new(body)))
            }
            // SOFT keywords fall back to plain variables when their
            // structural form does not follow — the corpus aliases
            // `count(*) AS count` and then writes `WHERE count > 1`, which
            // is legal Cypher (only clause words are truly reserved).
            TokenKind::Keyword(
                "COUNT" | "EXISTS" | "SHOW" | "VECTOR" | "FULLTEXT" | "OPTIONS" | "EACH" | "INDEX"
                | "CONSTRAINT" | "REQUIRE" | "YIELD" | "BY" | "ON" | "KEY" | "ALL",
            ) => {
                let name = self.name_token("a variable")?;
                Ok(Expr::Var(name))
            }
            TokenKind::LParen => {
                // A bare pattern is a PREDICATE (`WHERE (a)-[:R]->()`). Try a
                // path parse; a RELATIONSHIP commits it, anything else
                // backtracks to a parenthesised expression — the same rule
                // pattern comprehensions use.
                let saved = self.at;
                if let Ok(path) = self.path_pattern() {
                    if !path.hops.is_empty() {
                        if !self.allow_bare_pattern {
                            // A pattern is not a value here (RETURN/WITH/ORDER
                            // BY/SET) — openCypher `UnexpectedSyntax`. Wrap it in
                            // `exists(...)` to test existence.
                            return self.refuse(
                                "a relationship pattern is not a value here (UnexpectedSyntax); use exists(...)",
                            );
                        }
                        return Ok(Expr::PatternPredicate(Box::new(path)));
                    }
                }
                self.at = saved;
                self.bump();
                let e = self.expr()?;
                self.expect(&TokenKind::RParen, "`)`")?;
                Ok(e)
            }
            TokenKind::LBracket => self.list_or_comprehension(),
            TokenKind::LBrace => self.map_literal(),
            TokenKind::Ident(name) => {
                if name.eq_ignore_ascii_case("reduce")
                    && matches!(self.peek_at(1), TokenKind::LParen)
                {
                    return self.reduce_expr();
                }
                let lp = match name.to_ascii_lowercase().as_str() {
                    "any" => Some(crate::ast::ListPredicateKind::Any),
                    "none" => Some(crate::ast::ListPredicateKind::None),
                    "single" => Some(crate::ast::ListPredicateKind::Single),
                    _ => None,
                };
                if let Some(kind) = lp {
                    if matches!(self.peek_at(1), TokenKind::LParen)
                        && matches!(self.peek_at(2), TokenKind::Ident(_))
                        && matches!(self.peek_at(3), TokenKind::Keyword("IN"))
                    {
                        return self.list_predicate(kind);
                    }
                }
                // A dotted function name? Scan Ident (. Ident)* `(` without
                // consuming unless it lands on a call.
                let mut lookahead = 1;
                let mut parts = vec![name.clone()];
                while let (TokenKind::Dot, TokenKind::Ident(next)) =
                    (self.peek_at(lookahead), self.peek_at(lookahead + 1))
                {
                    parts.push(next.clone());
                    lookahead += 2;
                }
                if matches!(self.peek_at(lookahead), TokenKind::LParen) {
                    for _ in 0..lookahead {
                        self.bump();
                    }
                    return self.call_args(parts.join(".").to_lowercase());
                }
                self.bump();
                Ok(Expr::Var(name))
            }
            _ => self.refuse("an expression"),
        }
    }

    /// Arguments after the name; the caller has consumed up to (not
    /// including) `(`.
    fn call_args(&mut self, name: String) -> Result<Expr, ParseError> {
        self.expect(&TokenKind::LParen, "`(` opening arguments")?;
        if self.eat(&TokenKind::Star) {
            self.expect(&TokenKind::RParen, "`)` after `*`")?;
            return Ok(Expr::Call {
                name,
                distinct: false,
                args: vec![],
                star: true,
            });
        }
        let distinct = self.eat_kw("DISTINCT");
        let mut args = Vec::new();
        if !matches!(self.peek(), TokenKind::RParen) {
            loop {
                args.push(self.expr()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RParen, "`)` closing arguments")?;
        Ok(Expr::Call {
            name,
            distinct,
            args,
            star: false,
        })
    }

    fn case_expr(&mut self) -> Result<Expr, ParseError> {
        self.bump(); // CASE
        let subject = if matches!(self.peek(), TokenKind::Keyword("WHEN")) {
            None
        } else {
            Some(Box::new(self.expr()?))
        };
        let mut arms = Vec::new();
        while self.eat_kw("WHEN") {
            let when = self.expr()?;
            if !self.eat_kw("THEN") {
                return self.refuse("THEN after WHEN");
            }
            arms.push((when, self.expr()?));
        }
        if arms.is_empty() {
            return self.refuse("at least one WHEN arm");
        }
        let otherwise = if self.eat_kw("ELSE") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        if !self.eat_kw("END") {
            return self.refuse("END closing CASE");
        }
        Ok(Expr::Case {
            subject,
            arms,
            otherwise,
        })
    }

    fn list_or_comprehension(&mut self) -> Result<Expr, ParseError> {
        self.bump(); // [
        // A pattern comprehension `[ (n)-[:T]->(m) … | e ]`? Try a path parse
        // with backtracking; commit only if a RELATIONSHIP followed (that is
        // what distinguishes it from a parenthesized expression in a list).
        if matches!(self.peek(), TokenKind::LParen) {
            let saved = self.at;
            if let Ok(path) = self.path_pattern() {
                if !path.hops.is_empty()
                    && matches!(self.peek(), TokenKind::Pipe | TokenKind::Keyword("WHERE"))
                {
                    let filter = if self.eat_kw("WHERE") {
                        Some(Box::new(self.expr()?))
                    } else {
                        None
                    };
                    self.expect(&TokenKind::Pipe, "`|` before the comprehension expression")?;
                    let map = Box::new(self.expr()?);
                    self.expect(&TokenKind::RBracket, "`]` closing a pattern comprehension")?;
                    return Ok(Expr::PatternComp {
                        path: Box::new(path),
                        filter,
                        map,
                    });
                }
            }
            self.at = saved;
        }
        // A pattern comprehension that BINDS the path: `[p = (n)-[:T]->() | e]`.
        if let (TokenKind::Ident(pv), TokenKind::Eq) =
            (self.peek().clone(), self.peek_at(1).clone())
        {
            let saved = self.at;
            self.bump(); // p
            self.bump(); // =
            if let Ok(mut path) = self.path_pattern() {
                if !path.hops.is_empty()
                    && matches!(self.peek(), TokenKind::Pipe | TokenKind::Keyword("WHERE"))
                {
                    path.var = Some(pv);
                    let filter = if self.eat_kw("WHERE") {
                        Some(Box::new(self.expr()?))
                    } else {
                        None
                    };
                    self.expect(&TokenKind::Pipe, "`|` before the comprehension expression")?;
                    let map = Box::new(self.expr()?);
                    self.expect(&TokenKind::RBracket, "`]` closing a pattern comprehension")?;
                    return Ok(Expr::PatternComp {
                        path: Box::new(path),
                        filter,
                        map,
                    });
                }
            }
            self.at = saved;
        }
        // `[x IN xs …]` — two-token lookahead decides.
        if let (TokenKind::Ident(var), TokenKind::Keyword("IN")) =
            (self.peek().clone(), self.peek_at(1).clone())
        {
            self.bump();
            self.bump();
            let source = self.expr()?;
            let filter = if self.eat_kw("WHERE") {
                Some(Box::new(self.expr()?))
            } else {
                None
            };
            let map = if self.eat(&TokenKind::Pipe) {
                Some(Box::new(self.expr()?))
            } else {
                None
            };
            self.expect(&TokenKind::RBracket, "`]` closing a comprehension")?;
            return Ok(Expr::ListComp {
                var,
                source: Box::new(source),
                filter,
                map,
            });
        }
        let mut items = Vec::new();
        if !matches!(self.peek(), TokenKind::RBracket) {
            loop {
                items.push(self.expr()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RBracket, "`]` closing a list")?;
        Ok(Expr::List(items))
    }

    fn map_literal(&mut self) -> Result<Expr, ParseError> {
        self.bump(); // {
        let mut entries: Vec<(String, Expr)> = Vec::new();
        if !matches!(self.peek(), TokenKind::RBrace) {
            loop {
                let at = self.pos();
                let key = self.name_token("a map key")?;
                if entries.iter().any(|(k, _)| *k == key) {
                    return Err(ParseError::DuplicateMapKey { key, at });
                }
                self.expect(&TokenKind::Colon, "`:` after a map key")?;
                entries.push((key, self.expr()?));
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RBrace, "`}` closing a map")?;
        Ok(Expr::Map(entries))
    }

    fn reduce_expr(&mut self) -> Result<Expr, ParseError> {
        self.bump(); // reduce
        self.expect(&TokenKind::LParen, "`(` after reduce")?;
        let acc = self.name_token("the accumulator variable")?;
        self.expect(&TokenKind::Eq, "`=` after the accumulator")?;
        let init = self.expr()?;
        self.expect(&TokenKind::Comma, "`,` after the initial value")?;
        let var = self.name_token("the bound variable")?;
        if !self.eat_kw("IN") {
            return self.refuse("IN after the bound variable");
        }
        let source = self.expr()?;
        self.expect(&TokenKind::Pipe, "`|` before the step expression")?;
        let step = self.expr()?;
        self.expect(&TokenKind::RParen, "`)` closing reduce")?;
        Ok(Expr::Reduce {
            acc,
            init: Box::new(init),
            var,
            source: Box::new(source),
            step: Box::new(step),
        })
    }

    /// `any/all/none/single(var IN list WHERE predicate)`.
    fn list_predicate(&mut self, kind: crate::ast::ListPredicateKind) -> Result<Expr, ParseError> {
        self.bump(); // the quantifier word
        self.expect(&TokenKind::LParen, "`(`")?;
        let var = self.name_token("the bound variable")?;
        if !self.eat_kw("IN") {
            return self.refuse("IN after the bound variable");
        }
        let source = self.expr()?;
        if !self.eat_kw("WHERE") {
            return self.refuse("WHERE (a list predicate needs one)");
        }
        let filter = self.expr()?;
        self.expect(&TokenKind::RParen, "`)` closing the predicate")?;
        Ok(Expr::ListPredicate {
            kind,
            var,
            source: Box::new(source),
            filter: Box::new(filter),
        })
    }

    /// `expr {.a, .*, k: v, var}` — the caller has NOT consumed the brace.
    fn map_projection(&mut self, of: Expr) -> Result<Expr, ParseError> {
        use crate::ast::MapProjectionItem as Item;
        self.bump(); // {
        let mut items = Vec::new();
        if !matches!(self.peek(), TokenKind::RBrace) {
            loop {
                if self.eat(&TokenKind::Dot) {
                    if self.eat(&TokenKind::Star) {
                        items.push(Item::AllProperties);
                    } else {
                        items.push(Item::Property(self.name_token("a property name")?));
                    }
                } else {
                    let name = self.name_token("a projection item")?;
                    if self.eat(&TokenKind::Colon) {
                        items.push(Item::Entry(name, self.expr()?));
                    } else {
                        items.push(Item::Variable(name));
                    }
                }
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RBrace, "`}` closing the map projection")?;
        Ok(Expr::MapProjection {
            of: Box::new(of),
            items,
        })
    }
}
