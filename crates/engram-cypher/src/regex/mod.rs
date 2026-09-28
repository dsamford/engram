//! `=~` — matching, over the `regex` crate, with Cypher's semantics on top.
//!
//! # What this module is
//!
//! Three things the crate does not do for us:
//!
//! 1. **Full-match semantics.** Cypher's `=~` is `java.lang.String.matches`:
//!    the pattern must match the WHOLE string. `'foo' =~ 'oo'` is **false**.
//!    Every pattern is wrapped so that this holds however it was written.
//! 2. **Refusal by name.** Java accepts several constructs this engine cannot
//!    implement — backreferences and lookaround chiefly — and the `regex`
//!    crate rejects them with its own wording. We name them, because a user
//!    who wrote `\1` needs to be told "backreferences", not shown a parse
//!    error about a repetition operator.
//! 3. **A compile cache.** A pattern in a `WHERE` clause is compiled once per
//!    statement, not once per row.
//!
//! # Why a dependency here, when so little else has one
//!
//! The constraint on this path was never "no dependencies" — it is "nothing
//! that can hang, and nothing that can make a run irreproducible". A regex in
//! a filter runs over every row of a scan, on an engine with no clock to time
//! itself out with (time is injected, so `Instant::now` is denied here). A
//! backtracking matcher would make a hostile pattern into a process that stops
//! answering, and "refuse rather than guess" cannot be written for a failure
//! nothing sees coming.
//!
//! `regex` is a finite automaton. It **cannot backtrack**, its cost is linear
//! in the input by construction, and it has no backreferences or lookaround
//! precisely because those are what force a backtracker. So the safety
//! argument is the same one a hand-written Pike VM would give, and the lazy
//! DFA underneath is far faster per row than one. It is pure Rust with no
//! build script, so the one-`cc`-invocation rule is untouched; it spawns
//! nothing and reads no clock, so the determinism gate is untouched.
//!
//! # Absences, stated
//!
//! No backreferences, no lookahead or lookbehind, no atomic groups, no
//! possessive quantifiers, no `\Q…\E`. These are refused **by name** at
//! compile time and never reinterpreted — a `\1` silently read as "a backslash
//! and the digit one" is not an error, it is a pattern that matches a
//! different set of strings, which inside a `WHERE` clause is a wrong answer
//! with nothing anywhere looking wrong.
//!
//! Capture groups parse and are ignored: `=~` returns a boolean, so there is
//! nothing to capture into. Greediness likewise cannot change the answer —
//! lazy quantifiers are accepted and mean the same thing here, because
//! greediness chooses *among* matches and this operator asks only whether one
//! exists.

mod cache;

pub mod prefilter;

use std::sync::Arc;

pub use cache::{compile_cached, set_compile_cache};

/// What a pattern could not be given, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegexError {
    /// A feature this engine does not implement, named.
    Unsupported {
        /// The feature, spelled the way a user would recognise it.
        feature: &'static str,
        /// The character offset in the pattern where it appeared.
        at: usize,
    },
    /// The pattern is not well formed, or is larger than the compiler allows.
    Syntax {
        /// What was wrong, as the underlying engine reported it.
        detail: String,
    },
}

impl std::fmt::Display for RegexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegexError::Unsupported { feature, at } => {
                write!(f, "{feature} are not supported (at offset {at})")
            }
            RegexError::Syntax { detail } => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for RegexError {}

/// The compiled-program size cap, in bytes of automaton.
///
/// **A PATTERN IS A PROGRAM, AND ON AN UNAUTHENTICATED PORT IT IS A PROGRAM AN
/// ATTACKER CHOSE.** `(ab{500}){500}` is fourteen characters to type and a
/// quarter of a million states to compile, so the size is bounded where the
/// cost is known — at compile time, before the allocation happens — rather
/// than discovered by the allocator.
///
/// One mebibyte, and the number is chosen from the whole budget rather than
/// from this pattern alone: the compile cache holds up to `CACHE_CAP` programs
/// per engine thread, so the ceiling this sets is `1 MiB x 64 = 64 MiB` of
/// compiled automata per thread. The crate's own default is ten times this,
/// which would put that ceiling at 640 MiB — a memory budget chosen by whoever
/// sends the statements. Any pattern a person writes by hand compiles to a few
/// kilobytes.
const SIZE_LIMIT: usize = 1024 * 1024;

/// The lazy DFA's cache cap, kept well below [`SIZE_LIMIT`].
///
/// This one is not a correctness bound: exceeding it makes the engine fall
/// back to a slower automaton rather than fail, so it trades a cliff in
/// memory for a slope in speed.
const DFA_SIZE_LIMIT: usize = 256 * 1024;

/// A compiled pattern.
#[derive(Debug, Clone)]
pub struct Regex {
    inner: Arc<regex::Regex>,
}

impl Regex {
    /// Compile `pattern` under Cypher's semantics, or say why it cannot be.
    pub fn compile(pattern: &str) -> Result<Regex, RegexError> {
        refuse_unsupported(pattern)?;
        // THE PATTERN IS VALIDATED ALONE, BEFORE IT IS WRAPPED.
        //
        // Wrapping puts the user's text inside a group this engine opened, so
        // a pattern that is not well formed ON ITS OWN can close that group
        // and escape the anchor. `)|(` wrapped becomes `\A(?:)|()\z`, which
        // compiles happily and whose first branch matches the empty string at
        // position zero — a filter that returns EVERY row. An audit found it;
        // `an_unbalanced_paren_cannot_escape_the_anchor` pins it.
        //
        // Parsing the raw text first makes that unrepresentable: `)|(` is a
        // parse error standalone, so it never reaches the wrapper. This is the
        // same parser the trigram analysis uses, so the two also cannot
        // disagree about what is well formed.
        regex_syntax::parse(pattern).map_err(|e| RegexError::Syntax {
            detail: tidy(&e.to_string()),
        })?;
        // `\A(?:…)\z` is what makes this a full match rather than a search.
        // The non-capturing group is load-bearing: without it, `a|b` would
        // anchor only its first branch and `'b' =~ 'a|b'` would be false.
        let anchored = format!("\\A(?:{pattern})\\z");
        let inner = regex::RegexBuilder::new(&anchored)
            .size_limit(SIZE_LIMIT)
            .dfa_size_limit(DFA_SIZE_LIMIT)
            .build()
            .map_err(|e| RegexError::Syntax {
                detail: tidy(&e.to_string()),
            })?;
        Ok(Regex {
            inner: Arc::new(inner),
        })
    }

    /// Whether `text` matches the pattern **in its entirety**.
    pub fn is_full_match(&self, text: &str) -> bool {
        engram_observe::counted!("cypher.regex evaluations");
        self.inner.is_match(text)
    }

    /// Rebuild from a cached program.
    pub(crate) fn from_inner(inner: Arc<regex::Regex>) -> Regex {
        Regex { inner }
    }

    /// The compiled program, for the cache to hold.
    pub(crate) fn inner_arc(&self) -> Arc<regex::Regex> {
        Arc::clone(&self.inner)
    }
}

/// The underlying engine's message, without the pattern it echoes back.
///
/// `regex` renders an error as several lines including the whole anchored
/// pattern and a caret — useful at a terminal, noise inside a Cypher
/// diagnostic that already quotes the pattern the user wrote.
fn tidy(msg: &str) -> String {
    msg.lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty() && !l.starts_with('^') && !l.starts_with("regex parse error"))
        .unwrap_or("the pattern could not be compiled")
        .to_string()
}

/// Whether the `}` at `at` closes a `{n}` / `{n,}` / `{n,m}` repetition.
///
/// Needed because `}` also closes `\p{L}` and `\x{1F600}`, and a `+` after one
/// of THOSE is an ordinary quantifier rather than a possessive one. The scanner
/// below must refuse `a{2}+` and must not refuse `\p{L}+`; deciding by "the
/// previous character was a brace" gets that wrong, which a test caught.
fn closes_repeat(b: &[char], escaped: &[bool], at: usize) -> bool {
    if b[at] != '}' || escaped[at] {
        return false;
    }
    let mut i = at;
    let mut digits = 0;
    let mut comma = false;
    while i > 0 {
        i -= 1;
        match b[i] {
            c if c.is_ascii_digit() => digits += 1,
            ',' if !comma => comma = true,
            '{' => {
                if escaped[i] || digits == 0 {
                    return false;
                }
                // `\x{1234}` and `\u{1F600}` are all-digit braces too, and the
                // `+` after one of them is an ORDINARY quantifier. Deciding by
                // "digits then a brace" refuses `\x{1234}+`, which is a legal
                // pattern; the character introducing the brace settles it.
                let intro = i.checked_sub(1).map(|k| (b[k], escaped[k]));
                return !matches!(intro, Some(('x' | 'u' | 'p' | 'P', true)));
            }
            _ => return false,
        }
    }
    false
}

/// Refuse, by name, the constructs Java accepts and this engine cannot run.
///
/// **THE POINT IS THAT NOTHING HERE IS REINTERPRETED.** Every one of these has
/// a naive reading in which it is not an error at all — `\1` is "a backslash
/// and a one", `(?=x)` is "a group starting with an equals sign", `a*+` is "a
/// star then a plus". Taken that way, the pattern still compiles and still
/// runs, and it matches a *different set of strings* than the author wrote.
/// In a `WHERE` clause that is a silently wrong answer, so each is named and
/// refused before the engine ever sees it.
///
/// The scan is deliberately textual and deliberately conservative: it walks
/// the pattern honouring escapes and character classes, so a `(?=` inside
/// `[...]` or after a `\` is left alone.
fn refuse_unsupported(pattern: &str) -> Result<(), RegexError> {
    let b: Vec<char> = pattern.chars().collect();
    // Which positions hold the CHARACTER AFTER A BACKSLASH.
    //
    // Without this the scanner reads `\*+` as "a star, then a plus" and
    // refuses a legal pattern as a possessive quantifier — the star is an
    // escaped literal and the plus is an ordinary repetition. Looking only at
    // `b[i - 1]` cannot tell those apart.
    let mut escaped = vec![false; b.len()];
    {
        let mut k = 0;
        while k < b.len() {
            if b[k] == '\\' && k + 1 < b.len() {
                escaped[k + 1] = true;
                k += 2;
            } else {
                k += 1;
            }
        }
    }
    let mut i = 0;
    // Class nesting DEPTH, not a flag. `[a-z&&[^aeiou]]` opens a second class
    // inside the first, and a bool exits on the first `]` — leaving the
    // scanner reading class contents as pattern syntax.
    let mut class_depth = 0usize;
    let mut class_start = usize::MAX;
    while i < b.len() {
        let c = b[i];
        if c == '\\' {
            let next = b.get(i + 1).copied();
            match next {
                // A backreference. The one that matters most: read naively it
                // is a literal digit, and the pattern then matches something
                // else entirely.
                Some('1'..='9') => {
                    return Err(RegexError::Unsupported {
                        feature: "backreferences",
                        at: i,
                    });
                }
                Some('k') => {
                    return Err(RegexError::Unsupported {
                        feature: "named backreferences `\\k<…>`",
                        at: i,
                    });
                }
                Some('Q') | Some('E') => {
                    return Err(RegexError::Unsupported {
                        feature: "literal quoting `\\Q…\\E`",
                        at: i,
                    });
                }
                Some('G') => {
                    return Err(RegexError::Unsupported {
                        feature: "the match-start anchor `\\G`",
                        at: i,
                    });
                }
                _ => {}
            }
            i += 2;
            continue;
        }
        if class_depth > 0 {
            if c == '[' {
                class_depth += 1;
            } else if c == ']' {
                // A `]` in the FIRST position of a class is a literal `]` in
                // most engines, so it does not close anything.
                if i != class_start + 1 {
                    class_depth -= 1;
                }
            }
            i += 1;
            continue;
        }
        if c == '[' {
            class_depth = 1;
            class_start = i;
            i += 1;
            continue;
        }
        // A possessive quantifier: `a*+`, `a++`, `a?+`, `a{2,3}+`. Read
        // naively as two quantifiers, which is a syntax error in some engines
        // and a different meaning in others; either way it is not what was
        // written. `a+` following `)` or a literal is ordinary, so only the
        // sequences that follow another quantifier count.
        if c == '+'
            && i > 0
            && !escaped[i]
            && ((matches!(b[i - 1], '*' | '+' | '?') && !escaped[i - 1])
                || closes_repeat(&b, &escaped, i - 1))
        {
            return Err(RegexError::Unsupported {
                feature: "possessive quantifiers",
                at: i,
            });
        }
        if c == '(' && b.get(i + 1) == Some(&'?') {
            let feature = match b.get(i + 2) {
                Some('=') => Some("lookahead `(?=…)`"),
                Some('!') => Some("negative lookahead `(?!…)`"),
                Some('>') => Some("atomic groups `(?>…)`"),
                Some('#') => Some("comment groups `(?#…)`"),
                Some('(') => Some("conditional groups"),
                // Extended mode makes `#` start a comment that runs to the end
                // of the line — and the pattern is wrapped, so that comment
                // would swallow this engine's own closing `)` and its end
                // anchor. Refused by name rather than mis-handled.
                Some('x') => Some("extended mode `(?x)`"),
                Some('<') => match b.get(i + 3) {
                    Some('=') => Some("lookbehind `(?<=…)`"),
                    Some('!') => Some("negative lookbehind `(?<!…)`"),
                    // `(?<name>…)` is a named CAPTURE, which the crate
                    // supports and which is harmless here — there is nothing
                    // to capture into, so it behaves as a plain group.
                    _ => None,
                },
                _ => None,
            };
            if let Some(feature) = feature {
                return Err(RegexError::Unsupported { feature, at: i });
            }
        }
        i += 1;
    }
    Ok(())
}
