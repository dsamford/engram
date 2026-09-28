# Regular expressions

`=~` matches a string against a pattern:

```cypher
MATCH (f:File) WHERE f.path =~ '.*\\.rs' RETURN f.path
```

The engine is the `regex` crate, wrapped so that it means what Cypher says it
means. The constraint that governs this path is not "no dependencies" but
"nothing that can hang and nothing that can make a run irreproducible", and a
finite automaton satisfies both: it cannot backtrack, its cost is linear in the
input by construction, and it reads no clock and spawns nothing.

## It is a full match, not a search

`=~` is `java.lang.String.matches` — **the pattern must match the whole
string**. This is the thing people get wrong:

```cypher
RETURN 'foo' =~ 'oo'      // false
RETURN 'foo' =~ '.*oo'    // true
RETURN 'foo' =~ 'foo'     // true
```

Two consequences follow, and both are deliberate:

- **Capture groups do nothing.** `=~` returns a boolean, so there is nothing to
  capture into; a group is just a group.
- **Greediness cannot change the answer.** Lazy quantifiers (`a*?`) are
  accepted and mean the same thing here, because greediness chooses *among*
  matches and this operator asks only whether one exists. Possessive
  quantifiers (`a*+`) are refused, because those can remove matches.

`null` on either side gives `null`. A non-string operand is a type error, not a
silent coercion: `3 =~ '3'` fails rather than answering `true`.

## Supported syntax

| | |
|---|---|
| literals | `abc` |
| any character | `.` (not a newline unless `(?s)`) |
| classes | `[abc]`, `[a-z]`, `[^a-z]`, ranges, negation |
| shorthands | `\d \D \w \W \s \S` |
| escapes | `\\ \. \+ \* \? \( \) \[ \] \{ \} \| \^ \$ \/ \n \r \t \f \v \a \xHH \uHHHH` |
| alternation | `a\|b` |
| grouping | `(…)`, `(?:…)`, `(?<name>…)` — captures are inert |
| quantifiers | `* + ? {n} {n,} {n,m}`, and the lazy forms |
| anchors | `^ $ \A \z` (redundant — the match is already anchored) |
| word boundaries | `\b \B` |
| flags | `(?i)`, `(?s)`, `(?m)` and their scoped forms |
| Unicode classes | `\p{L}`, `\p{Han}`, `\P{…}` |
| POSIX classes | `[[:alpha:]]`, `[[:digit:]]` … |
| class set operations | `[a-z&&[^aeiou]]` |

## What is refused, and why by name

Anything below is **refused with the feature named**, never reinterpreted:

backreferences (`\1`, `\k<n>`) · lookahead and lookbehind (`(?=` `(?!` `(?<=`
`(?<!`) · atomic groups (`(?>`) · possessive quantifiers (`*+`) · `\Q…\E` ·
`\G` · extended mode `(?x)` · comment groups `(?#…)` · conditional groups.

Most of those are exactly the constructs a finite automaton cannot run: they
are the features that force a backtracker, which is why an engine guaranteeing
linear time does not have them. Named capture groups (`(?<n>…)`) are accepted
and behave as plain groups, since there is nothing to capture into.

The last three are refused for a different reason, and are listed with the rest
because what matters to a caller is the same either way: the pattern is refused
by name rather than reinterpreted. Comment groups and conditional groups are
caught by the scanner before the pattern reaches the engine at all. Extended
mode is refused because of the wrapper: `=~` compiles `\A(?:<pattern>)\z`, and
under `(?x)` a `#` comment runs to the end of the line — so it would swallow
this engine's own closing `)` and its end anchor, and the pattern would mean
something other than what was written.

**That refusal is not complete, and the gap is worth knowing about.** The
scanner reads the one character after `(?`, so it sees `(?x)` and does not see
a combined flag group. `(?ix)` therefore passes, and extended mode really is
on: `'ab' =~ '(?ix)a b'` is true because the space was dropped, and
`'a b' =~ '(?ix)a b'` is false. A `#` in such a pattern reaches the swallowed
anchor and the compile fails with `unclosed group`. Until the scanner reads the
whole flag group, `(?x)` is refused and a combined form carrying `x` is not.

The refusal is the point. A naive parser reads `\1` as "a backslash, then the
digit one" — which is not an error, it is a pattern that matches a *different
set of strings* than the author wrote. In a `WHERE` clause that is a wrong
answer with nothing anywhere looking wrong. Refusing by name is the difference
between a database that cannot do something and one that quietly does something
else.

One divergence from Java is worth stating: `a{2}{2}` is a syntax error in Java
and reads here as `a{4}`. The composition is unambiguous and bounded, and the
bomb version of it is refused by the size limit below.

`(?m)` deserves a note because it looks like a way to widen a filter. It makes
`^` and `$` mean "line start" *inside* the pattern, as it does in Java — but the
match as a whole is still against the WHOLE value, so a two-line value does not
match `'(?m)^secret$'` just because its second line is `secret`. A pattern an
author believed was anchored to the value cannot be made to match one line of it
that someone else controls.

## Characters, not bytes

The matcher steps over decoded characters, so `.` cannot match half of a UTF-8
sequence:

```cypher
RETURN 'é' =~ '.'    // true  — one character
RETURN 'é' =~ '..'   // false — not two, despite being two bytes
```

`(?i)` uses Unicode simple case folding: it folds the Kelvin sign to `k`, and
it does **not** fold `ß` to `ss` — simple folding is one scalar to one scalar,
and full folding, which expands one character into several, is not applied.
There is no locale. Stated as an absence rather than discovered.

The [trigram index](./trigram-index.md) folds values with the same rule — one
scalar to its lowercase — written out twice, once on each side of the crate
boundary, because `engram-store` sits below `engram-cypher` and may not reach
up to it. That rule is *lowercasing*, not the case folding `(?i)` does, and the
two are not the same relation: case folding equates the final sigma `ς` with
`σ`, lowercasing does not.

What keeps a query from asking for trigrams the index never stored is therefore
not that identity. It is that `(?i)` arrives at the trigram analysis already
expanded into a character class, whose members are enumerated and ORed rather
than folded together, and that every rule in that analysis widens to a scan
when it is unsure. The two copies are asserted to agree by a comment and not by
a test — the Basic-Multilingual-Plane agreement test the source names does not
exist.

## It cannot hang, and what it does bound instead

The matcher is a finite automaton, so its per-character cost does not depend on
the pattern's structure at all. The classic catastrophic patterns are ordinary
here:

```cypher
RETURN 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' =~ '(a+)+b'   // false, immediately
```

A backtracking engine takes exponential time on that. The reason this matters
more than usual: a regex in a `WHERE` clause runs over every row of a scan, and
the engine has no clock to time itself out with — time is injected, so
`Instant::now` is not available on this path. A pattern that hangs would be a
process that stops answering, with nothing in the trace to say why. "Refuse
rather than guess" cannot be written for a failure nothing sees coming, so the
failure is made impossible instead.

What a pattern *can* still do is be expensive to **compile**: `(ab{500}){500}`
is fourteen characters to type and a quarter of a million states to build. A
pattern is a program, and on an unauthenticated port it is a program someone
else chose, so the size is capped at **1 MiB of compiled automaton** and a
pattern over that is refused at compile time — before the allocation, not after
it. With a per-thread compile cache of 64 entries, that puts the ceiling on
compiled automata at 64 MiB per engine thread. The lazy DFA's working cache is
capped separately, at 256 KiB per pattern, and a pattern that outgrows it falls
back to a slower automaton rather than failing.

Refused by that cap, for example: `a{99999}`, `a{999}{999}`, `(a{1000}){1000}`,
`(ab{500}){500}`. Accepted: anything a person writes by hand, which compiles to
a few kilobytes.

A dangling quantifier is a syntax error: `{2}` and `*` are refused. So is a
brace that is not a repetition — `a{foo}` is refused rather than read as the six
characters it looks like, because any `{` after a repeatable atom enters the
repetition parser and a non-decimal body has nowhere to fall back to. This is a
divergence from the engines patterns get copied from. Write `a\\{foo\\}` for the
literal, which is what a pattern over JSON-ish text needs.

## Next

- [Trigram index](./trigram-index.md) — what makes `=~` seekable rather than a
  scan.
- [Cypher support](../using/cypher-support.md) — the language `=~` is an
  operator in.
- [Known limits](../known-limits.md) — where this sits among the rest.
