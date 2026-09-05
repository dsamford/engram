# Cypher procedures

Seven procedures. Any other name is refused with
`Neo.ClientError.Statement.NotSupported`.

> **Every call needs `YIELD` *and* `RETURN`.** Engram has no standalone `CALL`:
> a bare call, with or without `YIELD`, produces **no rows** rather than the
> procedure's own columns. This is the most common surprise for anyone arriving
> from Neo4j — see [Cypher support](../using/cypher-support.md).

Procedure names arrive lowercased (the parser's rule for callables), but
`YIELD` field names are identifiers and keep their case, so the Neo4j spellings
like `relationshipType` are matched exactly.

## Data procedures

### `db.index.vector.queryNodes`

```cypher
CALL db.index.vector.queryNodes(indexName, k, queryVector)
YIELD node, score
RETURN node.title AS title, score
```

| argument | type | |
|---|---|---|
| `indexName` | `String` | must name an existing vector index |
| `k` | `Integer` | how many results |
| `queryVector` | `List<Float\|Int>` | the query |

Yields `node` and `score`. The metric is **cosine similarity**, so an exact
match scores 1.0:

```text
t      score
a      1.0
c      0.9938837346736189
```

Small indexes are answered by an exact scan; past 2,048 vectors an HNSW graph
serves them, with an int8-quantised scan and an f32 rescore over an
oversampled candidate set. Both paths are deterministic.

### `db.index.fulltext.queryNodes`

```cypher
CALL db.index.fulltext.queryNodes(indexName, queryString)
YIELD node, score
RETURN node.title AS title, score
```

Yields `node` and `score`. **Scoring is term frequency, not BM25.** The
tokenizer splits on non-alphanumerics and lowercases; there is no stemming, no
stopwords and no configurable analyzer.

## Introspection

Answered from maintained statistics and the crate version — no scans.

### `db.labels`

```cypher
CALL db.labels() YIELD label RETURN label ORDER BY label
```

### `db.relationshipTypes`

```cypher
CALL db.relationshipTypes() YIELD relationshipType RETURN relationshipType
```

### `db.propertyKeys`

```cypher
CALL db.propertyKeys() YIELD propertyKey RETURN propertyKey
```

### `dbms.components`

```cypher
CALL dbms.components() YIELD name, versions, edition
RETURN name, versions, edition
```

```text
name     versions     edition
Engram   ["0.1.0"]    engram
```

Useful as a connectivity check: if this returns, the wire, the parser, the
interpreter and the procedure surface are all working.

## Operational

### `engram.checkpoint`

```cypher
CALL engram.checkpoint()
YIELD spilled, segments, resident, tail
RETURN spilled, segments, resident, tail
```

| field | meaning |
|---|---|
| `spilled` | segments written to disk by this call |
| `segments` | segments on disk afterwards |
| `resident` | segments still in memory |
| `tail` | versions still in the volatile tail |

**Paged mode only** — refused otherwise, rather than silently doing nothing.

This is what a drain-before-shutdown hook calls. In paged mode the unsealed
tail is volatile, so a clean stop means checkpointing first. See
[Durability and recovery](../using/durability.md).

## What is not supported

Calling anything else is refused by name:

```text
Neo.ClientError.Statement.NotSupported
procedure `db.awaitIndexes`
```

Notably absent, and commonly reached for by Neo4j tooling:

- `db.schema.visualization`, `db.schema.nodeTypeProperties`
- `db.awaitIndex`, `db.awaitIndexes`
- `db.stats.*`, `dbms.queryJmx`, `dbms.listQueries`
- `dbms.security.*` — there is no user model
- APOC, in general

Driver-introspection procedures are partial, which is one reason the driver
compatibility matrix is open rather than closed. See [Roadmap](../roadmap.md).

There is also **no user-defined procedure mechanism** — no plugin surface, no
way to register one.

## Next

- [Cypher support](../using/cypher-support.md) — the language, and the
  standalone-`CALL` gap.
- [Schema, indexes and constraints](../using/schema.md) — creating the indexes
  these query.
- [Errors](./errors.md) — the status codes.
