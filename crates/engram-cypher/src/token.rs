//! The Cypher tokenizer.
//!
//! Hand-written, position-carrying, and COMPLETE for the corpus's surface —
//! the tokenizer is the one layer everything above re-uses, so a gap here is
//! a gap in every clause. Keywords are recognised case-insensitively but
//! carried as their canonical spelling; identifiers keep theirs (Cypher
//! variables are case-sensitive, keywords are not).

use engram_observe::sometimes;

/// A token with its byte offset — every refusal upstream names a position,
/// and this is where positions come from.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    /// The token.
    pub kind: TokenKind,
    /// Byte offset of the token's first character in the source.
    pub at: usize,
}

/// The token kinds.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// An identifier or non-keyword word (case preserved). Backtick-quoted
    /// identifiers arrive here with the backticks removed.
    Ident(String),
    /// A reserved word, canonicalised to upper case.
    Keyword(&'static str),
    /// A string literal, unescaped.
    Str(String),
    /// An integer literal.
    Int(i64),
    /// A float literal.
    Float(f64),
    /// A parameter: `$name` or `$0`.
    Param(String),

    // Punctuation and operators.
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `,`
    Comma,
    /// `:`
    Colon,
    /// `;`
    Semicolon,
    /// `.`
    Dot,
    /// `..`
    DotDot,
    /// `|`
    Pipe,
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*`
    Star,
    /// `/`
    Slash,
    /// `%`
    Percent,
    /// `^`
    Caret,
    /// `=`
    Eq,
    /// `<>`
    Neq,
    /// `<`
    Lt,
    /// `>`
    Gt,
    /// `<=`
    Le,
    /// `>=`
    Ge,
    /// `=~`
    RegexMatch,
    /// `+=`
    PlusEq,
    /// `->`
    ArrowRight,
    /// `<-`
    ArrowLeft,
    /// End of input.
    Eof,
}

/// The reserved words — everything the parser dispatches on. Not every SQL
/// keyword: a word absent here is a legal identifier, which is how Cypher
/// treats e.g. `count` in `count(x)` (function names are identifiers).
const KEYWORDS: &[&str] = &[
    "MATCH",
    "OPTIONAL",
    "WHERE",
    "RETURN",
    "WITH",
    "UNWIND",
    "CREATE",
    "MERGE",
    "SET",
    "REMOVE",
    "DELETE",
    "DETACH",
    "ORDER",
    "BY",
    "SKIP",
    "LIMIT",
    "ASC",
    "ASCENDING",
    "DESC",
    "DESCENDING",
    "AND",
    "OR",
    "XOR",
    "NOT",
    "IN",
    "STARTS",
    "ENDS",
    "CONTAINS",
    "IS",
    "NULL",
    "TRUE",
    "FALSE",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "DISTINCT",
    "AS",
    "UNION",
    "ALL",
    "FOREACH",
    "CALL",
    "YIELD",
    "EXISTS",
    "COUNT",
    "ON",
    "USING",
    "INDEX",
    "CONSTRAINT",
    "FOR",
    "REQUIRE",
    "UNIQUE",
    "SHORTESTPATH",
    "DROP",
    "IF",
    "VECTOR",
    "FULLTEXT",
    "EACH",
    "OPTIONS",
    "SHOW",
];

/// Why the tokenizer refused. Every variant carries the byte offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LexError {
    /// A string or backtick quote that never closes.
    Unterminated {
        /// What was open.
        what: &'static str,
        /// Where it opened.
        at: usize,
    },
    /// A number that does not parse (overflow, malformed exponent…).
    BadNumber {
        /// Where it started.
        at: usize,
        /// The offending text.
        text: String,
    },
    /// A character no token starts with.
    BadChar {
        /// The character.
        ch: char,
        /// Where.
        at: usize,
    },
    /// A `$` with no parameter name after it.
    EmptyParam {
        /// Where.
        at: usize,
    },
}

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LexError::Unterminated { what, at } => {
                write!(f, "unterminated {what} starting at byte {at}")
            }
            LexError::BadNumber { at, text } => write!(f, "malformed number `{text}` at byte {at}"),
            LexError::BadChar { ch, at } => write!(f, "unexpected character `{ch}` at byte {at}"),
            LexError::EmptyParam { at } => write!(f, "`$` with no parameter name at byte {at}"),
        }
    }
}

impl std::error::Error for LexError {}

/// Tokenize a whole source string.
pub fn tokenize(src: &str) -> Result<Vec<Token>, LexError> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let at = i;
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let open = i;
                i += 2;
                loop {
                    if i + 1 >= b.len() {
                        return Err(LexError::Unterminated {
                            what: "block comment",
                            at: open,
                        });
                    }
                    if b[i] == b'*' && b[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            b'\'' | b'"' => {
                let quote = c;
                let open = i;
                i += 1;
                let mut s = String::new();
                loop {
                    let Some(&ch) = b.get(i) else {
                        return Err(LexError::Unterminated {
                            what: "string",
                            at: open,
                        });
                    };
                    if ch == quote {
                        i += 1;
                        break;
                    }
                    if ch == b'\\' {
                        let Some(&esc) = b.get(i + 1) else {
                            return Err(LexError::Unterminated {
                                what: "string",
                                at: open,
                            });
                        };
                        // The escape set the corpus uses. An unknown escape is
                        // carried through VERBATIM (backslash kept) rather
                        // than guessed at — openCypher's behaviour, and the
                        // one that never silently rewrites a regex.
                        match esc {
                            b'\\' => s.push('\\'),
                            b'\'' => s.push('\''),
                            b'"' => s.push('"'),
                            b'n' => s.push('\n'),
                            b'r' => s.push('\r'),
                            b't' => s.push('\t'),
                            b'b' => s.push('\u{0008}'),
                            b'f' => s.push('\u{000C}'),
                            // `\uXXXX` (4 hex) and `\UXXXXXXXX` (8 hex) — a
                            // Unicode code point. A malformed one is an error, not
                            // a verbatim carry-through (unlike an unknown letter).
                            b'u' | b'U' => {
                                let digits = if esc == b'u' { 4 } else { 8 };
                                let hex = b.get(i + 2..i + 2 + digits);
                                let ch = hex
                                    .and_then(|h| std::str::from_utf8(h).ok())
                                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                                    .and_then(char::from_u32);
                                match ch {
                                    Some(c) => {
                                        s.push(c);
                                        i += 2 + digits;
                                        continue;
                                    }
                                    None => {
                                        return Err(LexError::BadNumber {
                                            at: i,
                                            text: format!(
                                                "\\{}{}",
                                                esc as char,
                                                hex.map(|h| String::from_utf8_lossy(h).into_owned())
                                                    .unwrap_or_default()
                                            ),
                                        });
                                    }
                                }
                            }
                            other => {
                                s.push('\\');
                                s.push(other as char);
                            }
                        }
                        i += 2;
                        continue;
                    }
                    // Multi-byte UTF-8 passes through byte-wise; re-slice at
                    // the end of the run for correctness.
                    let start = i;
                    while i < b.len() && b[i] != quote && b[i] != b'\\' {
                        i += 1;
                    }
                    s.push_str(std::str::from_utf8(&b[start..i]).expect("source was a str"));
                }
                out.push(Token {
                    kind: TokenKind::Str(s),
                    at,
                });
            }
            b'`' => {
                let open = i;
                i += 1;
                let start = i;
                while i < b.len() && b[i] != b'`' {
                    i += 1;
                }
                if i >= b.len() {
                    return Err(LexError::Unterminated {
                        what: "backtick identifier",
                        at: open,
                    });
                }
                let name = std::str::from_utf8(&b[start..i])
                    .expect("source was a str")
                    .to_string();
                i += 1;
                out.push(Token {
                    kind: TokenKind::Ident(name),
                    at,
                });
            }
            b'$' => {
                i += 1;
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                if start == i {
                    return Err(LexError::EmptyParam { at });
                }
                let name = std::str::from_utf8(&b[start..i])
                    .expect("ascii")
                    .to_string();
                out.push(Token {
                    kind: TokenKind::Param(name),
                    at,
                });
            }
            b'0'..=b'9' => {
                let (kind, next) = lex_number(src, i)?;
                out.push(Token { kind, at });
                i = next;
            }
            b'.' if b.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                let (kind, next) = lex_number(src, i)?;
                out.push(Token { kind, at });
                i = next;
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                let word = std::str::from_utf8(&b[start..i]).expect("ascii");
                let upper = word.to_ascii_uppercase();
                match KEYWORDS.iter().find(|k| **k == upper) {
                    Some(k) => out.push(Token {
                        kind: TokenKind::Keyword(k),
                        at,
                    }),
                    None => out.push(Token {
                        kind: TokenKind::Ident(word.to_string()),
                        at,
                    }),
                }
            }
            _ => {
                let (kind, len) = match (c, b.get(i + 1).copied()) {
                    (b'<', Some(b'=')) => (TokenKind::Le, 2),
                    (b'<', Some(b'>')) => (TokenKind::Neq, 2),
                    (b'<', Some(b'-')) => (TokenKind::ArrowLeft, 2),
                    (b'>', Some(b'=')) => (TokenKind::Ge, 2),
                    (b'=', Some(b'~')) => (TokenKind::RegexMatch, 2),
                    (b'+', Some(b'=')) => (TokenKind::PlusEq, 2),
                    (b'-', Some(b'>')) => (TokenKind::ArrowRight, 2),
                    (b'.', Some(b'.')) => (TokenKind::DotDot, 2),
                    (b'(', _) => (TokenKind::LParen, 1),
                    (b')', _) => (TokenKind::RParen, 1),
                    (b'[', _) => (TokenKind::LBracket, 1),
                    (b']', _) => (TokenKind::RBracket, 1),
                    (b'{', _) => (TokenKind::LBrace, 1),
                    (b'}', _) => (TokenKind::RBrace, 1),
                    (b',', _) => (TokenKind::Comma, 1),
                    (b':', _) => (TokenKind::Colon, 1),
                    (b';', _) => (TokenKind::Semicolon, 1),
                    (b'.', _) => (TokenKind::Dot, 1),
                    (b'|', _) => (TokenKind::Pipe, 1),
                    (b'+', _) => (TokenKind::Plus, 1),
                    (b'-', _) => (TokenKind::Minus, 1),
                    (b'*', _) => (TokenKind::Star, 1),
                    (b'/', _) => (TokenKind::Slash, 1),
                    (b'%', _) => (TokenKind::Percent, 1),
                    (b'^', _) => (TokenKind::Caret, 1),
                    (b'=', _) => (TokenKind::Eq, 1),
                    (b'<', _) => (TokenKind::Lt, 1),
                    (b'>', _) => (TokenKind::Gt, 1),
                    _ => {
                        sometimes!("cypher.lex refused", true);
                        return Err(LexError::BadChar {
                            ch: src[i..].chars().next().expect("in bounds"),
                            at,
                        });
                    }
                };
                out.push(Token { kind, at });
                i += len;
            }
        }
    }
    out.push(Token {
        kind: TokenKind::Eof,
        at: b.len(),
    });
    Ok(out)
}

/// Lex a number starting at `start`. Handles ints, floats, exponents, hex
/// (`0x…`) and octal (`0o…`). Returns the token and the index after it.
fn lex_number(src: &str, start: usize) -> Result<(TokenKind, usize), LexError> {
    let b = src.as_bytes();
    let mut i = start;

    if b[i] == b'0' && matches!(b.get(i + 1), Some(b'x') | Some(b'X')) {
        i += 2;
        let digits = i;
        while i < b.len() && b[i].is_ascii_hexdigit() {
            i += 1;
        }
        let text = &src[digits..i];
        let v = i64::from_str_radix(text, 16).map_err(|_| LexError::BadNumber {
            at: start,
            text: src[start..i].to_string(),
        })?;
        return Ok((TokenKind::Int(v), i));
    }
    if b[i] == b'0' && matches!(b.get(i + 1), Some(b'o') | Some(b'O')) {
        i += 2;
        let digits = i;
        while i < b.len() && (b'0'..=b'7').contains(&b[i]) {
            i += 1;
        }
        let text = &src[digits..i];
        let v = i64::from_str_radix(text, 8).map_err(|_| LexError::BadNumber {
            at: start,
            text: src[start..i].to_string(),
        })?;
        return Ok((TokenKind::Int(v), i));
    }

    let mut is_float = false;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    // A fractional part — but NOT `..` (the range operator) and not a
    // property access like `1 .foo` (which is a lex-then-parse error anyway;
    // digits after the dot are what commits us).
    if i < b.len() && b[i] == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
        is_float = true;
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            is_float = true;
            i = j;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    let text = &src[start..i];
    if is_float {
        let v: f64 = text.parse().map_err(|_| LexError::BadNumber {
            at: start,
            text: text.to_string(),
        })?;
        // Rust parses a too-large magnitude to ±inf without erroring; openCypher
        // refuses it (FloatingPointOverflow) — the source has no `inf` literal.
        if v.is_infinite() {
            return Err(LexError::BadNumber {
                at: start,
                text: text.to_string(),
            });
        }
        Ok((TokenKind::Float(v), i))
    } else {
        // Integer overflow is a REFUSAL, not a silent float: a literal that
        // does not fit i64 quietly becoming imprecise is how ids corrupt.
        let v: i64 = text.parse().map_err(|_| LexError::BadNumber {
            at: start,
            text: text.to_string(),
        })?;
        Ok((TokenKind::Int(v), i))
    }
}
