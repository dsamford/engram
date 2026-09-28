# Cypher support

Engram implements **openCypher**, and the honest headline is the conformance
number: **3,769 of 3,773 evaluated TCK scenarios pass** — 99.9%. That number
is ratcheted in CI, at a floor of 3,768 passes and at most four failures. The
one pass of margin is deliberate: one scenario rides the harness's five-second
timeout and its verdict can flip run to run. The failure ceiling has no margin
left, so a fifth failure fails CI.

Of the four failures, three are scenarios in which the TCK expects a bare
pattern used as a value — in a `RETURN` or `WITH` projection, or on the
right-hand side of a `SET` — to be refused as a syntax error, and Engram accepts
the query instead. The fourth is a time-zone-database expectation where this
engine is arguably the more correct of the two.

Broad coverage and a few specific sharp edges are both true. This page is the
sharp edges, stated first, and then what works.

## What is refused

The message below is the engine's own text.

### `UNION` inside `CALL { }` — supported

This was refused until SNB BI's Q4 needed it. Each arm runs against the same
seed row and the arms concatenate, exactly as at top level; `UNION` without
`ALL` deduplicates on the same canonical key every other dedup site uses.

```cypher
CALL { RETURN 1 AS x UNION RETURN 2 AS x } RETURN x
```

Arms must project the same columns — the caller binds a subquery's columns back
into the outer row by NAME, so arms that disagree are refused rather than
binding different names on different rows.

Top-level `UNION` works normally:

```cypher
RETURN 1 AS x UNION RETURN 2 AS x
```

```text
x
1
2
```

## Sharp edges that are not refusals

These are accepted, and do something other than what you may be expecting.

### Full-text scoring is BM25, and the analyzer is fixed

A full-text index created today is stamped for **Okapi BM25** — Lucene's
parameters, `k1 = 1.2` and `b = 0.75`, with idf — and
`db.index.fulltext.queryNodes` scores with it. Neither parameter is tunable.

The stamp belongs to the index, not to the server: it is written into the
catalogue row at creation and read back from there. An index created before BM25
existed has no scoring recorded in its row, an absent scoring means term
frequency, and it therefore keeps term-frequency scoring for ever. That is what
made adding BM25 a change no existing deployment could notice, and it is also
why you cannot tell the two apart from the outside — `SHOW INDEXES` reports
name, type, entity type, labels, properties and state, and not the scoring.

`--no-bm25-by-default` stamps a newly created index for term frequency instead;
existing indexes are unaffected either way. `--no-bm25` is a different lever and
changes no score at all — it chooses the path, computing BM25 by a scan rather
than from the term index, so the A/B compares two costs for one answer.

What is still absent is the analyzer. The tokenizer splits on non-alphanumerics,
drops the empties and lowercases, and that is all: no stemming, no stopword
list, no synonyms and no configurable analyzer. There is exactly one of it,
shared by the index build, the incremental catch-up and the fallback scan, so
the three cannot disagree about what a token is.

### `RETURN *` and `WITH *` narrow relationship uniqueness to each path

Relationship uniqueness spans every path of a `MATCH` clause (see
[Patterns](#relationship-uniqueness-is-per-match-clause)), with one exception:
in a statement that projects `*` anywhere — `RETURN *` or `WITH *` — it is
applied within each path but not across a clause's comma-separated paths, so
two paths may bind the same relationship. The engine enforces the rule through
hidden variables that a `*` projection would expose as columns, so it declines
rather than add them. Name the columns you want instead of `*` and the
clause-wide rule applies.

### Vector index options are parsed and ignored

`CREATE VECTOR INDEX … OPTIONS { … }` accepts an options map and does not
interpret it. You cannot set `vector.dimensions` or
`vector.similarity_function`: the dimension is inferred from the data, and the
metric is cosine. See [Schema](./schema.md).

## What works

### Clauses

`MATCH`, `OPTIONAL MATCH`, `WHERE`, `RETURN`, `WITH`, `UNWIND`, `ORDER BY`,
`SKIP`, `LIMIT`, `DISTINCT`, `UNION` / `UNION ALL`, `CREATE`, `MERGE`, `SET`,
`REMOVE`, `DELETE` / `DETACH DELETE`, `FOREACH`, `CALL { }` subqueries,
`CALL … YIELD` procedures, and the schema commands (`CREATE`/`DROP INDEX` —
range, vector, full-text and trigram — `CREATE`/`DROP CONSTRAINT`, `SHOW`). See
[Schema](./schema.md) for what each index kind does.

### Procedure calls

A `CALL` that ends a query returns the procedure's declared output columns, so
a bare call is a complete statement:

```cypher
CALL dbms.components()
```

`YIELD` narrows and renames those columns and a `RETURN` reshapes them further.
Anywhere but the last clause `YIELD` is **required**, because naming the columns
is what binds them: `CALL db.labels() RETURN label` is refused rather than
silently working, since binding a procedure's outputs implicitly would let a
later clause capture a variable nobody wrote. See
[Cypher procedures](../reference/procedures.md).

### Patterns

Node patterns with labels and inline property maps; relationship patterns with
type, direction and properties; multi-hop paths; **variable-length paths**
(`[*]`, `[*1..3]`, `[*..5]`); undirected matching; path variables.

```cypher
MATCH (m:Person {name: 'Mary Somerville'})-[*1..2]->(p:Person)
RETURN DISTINCT p.name AS reached
```

### Relationship uniqueness is per `MATCH` clause

Within one `MATCH` (or `OPTIONAL MATCH`) clause, no relationship binds twice —
across all of its comma-separated paths, not only within each path. That is
openCypher's rule, and it is what makes two paths in one clause mean two
different relationships:

```cypher
MATCH (f:Forum)-[:HAS_MEMBER]->(a:Person), (f)-[:HAS_MEMBER]->(b:Person)
RETURN a.name, b.name
```

Here the two `HAS_MEMBER` relationships must be different relationships, so no
row pairs a member with themselves through one membership. Separate `MATCH`
clauses may reuse a relationship:

```cypher
MATCH (f:Forum)-[:HAS_MEMBER]->(a:Person)
MATCH (f)-[:HAS_MEMBER]->(b:Person)
RETURN a.name, b.name
```

also returns the rows in which both patterns bound the same relationship, and
so `a` and `b` are the same person.

A relationship variable named in two paths of one clause is one relationship
joined, not two, and is left alone. The engine states the rule as extra `WHERE`
conditions between relationship patterns in different paths whose types can
coincide, so patterns whose types cannot, or whose end nodes are already
provably different — a `WHERE` that says so, or inline property maps pinning
one key to two different values — get no extra condition.

### The value model

Null, boolean, integer, float, string, list, map, node, relationship, path, and
the temporal types.

```cypher
RETURN date('2026-09-05') AS d, duration({days: 3}) AS dur
```

Temporal support is real, not a string wrapper: dates, times, local times,
datetimes, local datetimes, durations, and IANA zone resolution — `tz-rs` and
`tzdb` are in the dependency list for exactly this.

### Three-valued logic

This is relied on rather than approximated:

```cypher
RETURN null = 'x' AS eq, null IS NULL AS isn, coalesce(null, 'y') AS c
```

```text
eq      isn     c
null    true    y
```

`null = 'x'` is **unknown**, not false, and a `WHERE` fails closed on it.

Note also that **setting a property to null removes it** (standard Cypher), so
an explicit null and an absent property are indistinguishable to a query —
`properties()` and `keys()` omit both.

### Expressions and comprehensions

Arithmetic and comparison, `AND`/`OR`/`NOT`/`XOR`, `IN`, `IS NULL`/`IS NOT
NULL`, `STARTS WITH`/`ENDS WITH`/`CONTAINS`, `=~`, `CASE`, list indexing and
slicing, map projection, list comprehensions, pattern comprehensions, and
`reduce`:

```cypher
RETURN [x IN range(1,5) WHERE x % 2 = 0 | x * 10] AS lc,
       reduce(a = 0, x IN [1,2,3] | a + x) AS red
```

```text
lc          red
[20, 40]    6
```

`=~` matches a **whole** string against a regular expression — it is
`java.lang.String.matches`, so `'foo' =~ 'oo'` is false and `'foo' =~ '.*oo'` is
true. `null` on either side gives `null`; a non-string operand is a type error
rather than a coercion. The matcher is a finite automaton, which is what makes
it safe to run over every row of a scan on an engine that has no clock to time
itself out with: it cannot backtrack and its cost is linear in the input. The
constructs that would force a backtracker — backreferences and lookaround — are
refused by name. The supported syntax, those refusals and the 1 MiB cap on a
compiled automaton are on [Regular expressions](../reference/regex.md).

### Aggregation

`count`, `sum`, `avg`, `min`, `max`, `collect`, `stDev`, and `count(DISTINCT …)`,
with implicit grouping by the non-aggregated projection items — the usual
Cypher rule.

`count(r)` counts non-null values, which is what makes `OPTIONAL MATCH` +
`count` give 0 rather than 1 for a node with no matches.

## Parameters

Use parameters rather than string interpolation — for the usual injection
reasons, and because the engine registers a statement's predicates from its
AST, which works better when values arrive as parameters.

```cypher
MATCH (p:Person {name: $name}) RETURN p.born AS born
```

## Limits that are bounds, not gaps

These exist to keep a hostile or careless statement from taking the process
down. They are configurable where it makes sense.

| bound | value | why |
|---|---|---|
| expression nesting depth | 64 | a deeply nested expression is a stack overflow otherwise |
| parser stack requirement | 4 MiB | so the depth bound is reachable rather than academic |
| PackStream nesting depth | 64 | the same argument, on the wire |
| single Bolt message | 64 MiB | policy, not protocol |
| rows one query may materialise | derived from the process's memory ceiling and shared by the statements in flight (`--row-budget`); see [Result paging](./result-paging.md) | the alternative is the OOM killer, which refuses nothing and takes every other session with it |

## Error codes

Failures map onto Neo4j's status-code vocabulary, so driver error handling
works unchanged:

| condition | code |
|---|---|
| parse error | `Neo.ClientError.Statement.SyntaxError` |
| unsupported construct | `Neo.ClientError.Statement.NotSupported` |
| semantic error | `Neo.ClientError.Statement.SemanticError` |
| bad argument / evaluation | `Neo.ClientError.Statement.ArgumentError` |
| execution failure | `Neo.ClientError.Statement.ExecutionFailed` |
| transaction start / commit | `Neo.ClientError.Transaction.TransactionStartFailed` / `…CommitFailed` |

From Bolt 5.7 the failure map also carries `gql_status`, `description`,
`neo4j_code` and a diagnostic record. See [Errors](../reference/errors.md).

## How conformance is measured

The vendored openCypher TCK is run scenario by scenario, each in its own thread
with a five-second timeout. The result is ratcheted (`MIN_PASS = 3768`,
`MAX_FAIL = 4`), so a regression fails CI.

The integrity rule is worth stating because it is the part most harnesses get
wrong: **a Skip is a gap in the harness, never a pass.** Pass rate is computed
as `Pass / (Pass + Fail)` and Skips are reported separately, so a scenario the
harness cannot run does not quietly improve the number.

## Next

- [Schema, indexes and constraints](./schema.md) — what to index and how.
- [Transactions and isolation](./transactions.md) — what concurrent writers see.
- [Cypher procedures](../reference/procedures.md) — the full procedure surface.
