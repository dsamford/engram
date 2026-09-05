# Cypher support

Engram implements **openCypher**, and the honest headline is the conformance
number: **3,772 of 3,773 evaluated TCK scenarios pass** — 99.97%. That number
is ratcheted in CI so it cannot quietly fall, and the single failure is a
time-zone-database expectation where this engine is arguably the more correct
of the two.

Broad coverage and a few specific sharp edges are both true. This page is the
sharp edges, stated first, and then what works.

## What is refused

Each of these was run against a live server; the messages are verbatim.

### `=~` — regular expressions

Parses, then refuses at evaluation:

```cypher
RETURN 'abc' =~ 'a.*' AS m
```

```text
Neo.ClientError.Statement.ArgumentError
`=~` is not yet supported
```

There is no regex crate anywhere in the workspace — the engine is deliberately
zero-dependency on this axis, and adding one is an open decision rather than an
oversight. Use `STARTS WITH`, `ENDS WITH` or `CONTAINS`, which are supported.

### `UNION` inside `CALL { }`

```cypher
CALL { RETURN 1 AS x UNION RETURN 2 AS x } RETURN x
```

```text
Neo.ClientError.Statement.NotSupported
not supported yet: UNION inside CALL {}
```

Top-level `UNION` works normally:

```cypher
RETURN 1 AS x UNION RETURN 2 AS x
```

```text
x
1
2
```

### Standalone `CALL`

This one catches people migrating from Neo4j, because it fails *quietly* rather
than with an error. A bare procedure call — with or without `YIELD` — returns
**no rows**:

```cypher
CALL dbms.components()                     -- returns nothing
CALL dbms.components() YIELD name          -- also returns nothing
```

Every procedure call needs an explicit `YIELD` **and** a `RETURN`:

```cypher
CALL dbms.components() YIELD name, versions, edition
RETURN name, versions, edition
```

```text
name     versions     edition
Engram   ["0.1.0"]    engram
```

See [Cypher procedures](../reference/procedures.md) for the full list.

### Full-text search is term frequency, not BM25

`db.index.fulltext.queryNodes` scores by term frequency. The tokenizer splits
on non-alphanumerics and lowercases. There is no stemming, no stopword list and
no configurable analyzer. Scores are therefore comparable *within* a query and
should not be read as BM25-like relevance.

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
`CALL … YIELD` procedures, and the schema commands (`CREATE`/`DROP INDEX`,
`CREATE`/`DROP CONSTRAINT`, `SHOW`).

### Patterns

Node patterns with labels and inline property maps; relationship patterns with
type, direction and properties; multi-hop paths; **variable-length paths**
(`[*]`, `[*1..3]`, `[*..5]`); undirected matching; path variables.

```cypher
MATCH (m:Person {name: 'Mary Somerville'})-[*1..2]->(p:Person)
RETURN DISTINCT p.name AS reached
```

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
NULL`, `STARTS WITH`/`ENDS WITH`/`CONTAINS`, `CASE`, list indexing and slicing,
map projection, list comprehensions, pattern comprehensions, and `reduce`:

```cypher
RETURN [x IN range(1,5) WHERE x % 2 = 0 | x * 10] AS lc,
       reduce(a = 0, x IN [1,2,3] | a + x) AS red
```

```text
lc          red
[20, 40]    6
```

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
| rows one query may materialise | 20,000,000 (`--row-budget`) | the alternative is the OOM killer, which refuses nothing and takes every other session with it |

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
