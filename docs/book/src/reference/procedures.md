# Cypher procedures

Sixty-one procedures, declared in one catalogue (`engram-proc`). Eight are the
data, introspection and operational procedures below; the other fifty-three are
the graph algorithms, most of which are named by a scheme rather than listed one
by one — see [Graph algorithms](#graph-algorithms). Any other name is refused with
`Neo.ClientError.Statement.NotSupported`.

> **`YIELD` is required only when the `CALL` is not the last clause.** A `CALL`
> that ends a query returns the procedure's declared output columns, as
> openCypher specifies:
>
> ```cypher
> CALL dbms.components()
> ```
>
> Anywhere else, `YIELD` is required and naming the columns is what binds them:
> `CALL db.labels() RETURN label` is refused, because binding a procedure's
> outputs implicitly would let a later clause capture a variable nobody wrote.

Procedure names arrive lowercased (the parser's rule for callables), but
`YIELD` field names are identifiers and keep their case, so the Neo4j spellings
like `relationshipType` are matched exactly.

## Data procedures

### `db.index.vector.queryNodes`

```cypher
CALL db.index.vector.queryNodes(indexName, numberOfNearestNeighbours, query)
YIELD node, score
RETURN node.title AS title, score
```

| argument | type | |
|---|---|---|
| `indexName` | `String` | must name an existing vector index |
| `numberOfNearestNeighbours` | `Integer` | how many results |
| `query` | `List<Float\|Int>` | the query |

Arguments are positional, so nothing has to spell those names — but they are
the Neo4j spellings and they are what a wrong-arity refusal prints, so a
message quoting `numberOfNearestNeighbours` is quoting this table.

Yields `node` and `score`. The metric is **cosine similarity**, so an exact
match scores 1.0:

```text
t      score
a      1.0
c      0.9938837346736189
```

Small indexes are answered by an exact scan; past 2,048 vectors an HNSW graph
serves them. The graph is searched in `f32` over a beam of `max(400, 4k)`
candidates, and every candidate the search returns is re-scored with the exact
f64 cosine before it is ranked — so the approximation is in which candidates
are considered and never in how they are ordered. Both paths are
deterministic: the graph's level assignment draws from a hash of the element's
own id rather than from ambient randomness, so two builds over one content
produce the same graph.

### `db.index.fulltext.queryNodes`

```cypher
CALL db.index.fulltext.queryNodes(indexName, queryString)
YIELD node, score
RETURN node.title AS title, score
```

Yields `node` and `score`. **Scoring is BM25 for an index created on a stock
server, and term frequency for one created before that field existed.** The
scoring is stamped into the index's catalogue row when the index is created
and never re-derived, so an upgrade cannot re-rank an existing index — an
absent field reads back as term frequency, which is what makes the change
invisible to a deployment that already has indexes. `--no-bm25-by-default`
stamps term frequency into anything created after it. `k1` and `b` are
Lucene's 1.2 and 0.75 and are not configurable, and `SHOW INDEXES` does not
report which scoring an index carries.

The tokenizer splits on non-alphanumerics and lowercases; there is no
stemming, no stopwords and no configurable analyzer.

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
Engram   ["0.2.0"]    engram
```

The version is the crate version the server was built from. Useful as a
connectivity check: if this returns, the wire, the parser, the interpreter and
the procedure surface are all working.

### `db.awaitIndexes`

```cypher
CALL db.awaitIndexes() YIELD ok RETURN ok
```

| argument | type | |
|---|---|---|
| `timeOutSeconds` | `Integer`, optional | accepted and ignored |

Yields `ok`, which is always `true`, returned immediately. Drivers call this on
connect and expect to block until index builds finish; here there is nothing to
wait for, because an index is built single-flight on the read path that first
needs it rather than by a background job, so no build is ever outstanding
between statements. The `true` is therefore a statement that nothing is
pending, not a promise that a job completed.

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
| `tail` | versions still in the unsealed tail |

**Paged mode only** — refused otherwise, rather than silently doing nothing.

This is what a drain-before-shutdown hook calls, and what bounds resident
memory. In order, it:

1. seals whatever the tail holds;
2. writes every resident sealed segment into the paged directory, so
   `resident` falls and `segments` rises, and checkpoints the WAL behind the
   segments it wrote, so the next start has nothing to replay into a fresh
   segment;
3. brings every derived structure current — refresh passes until nothing is
   deferred (at most 64), then a warm of whatever is still stale — and
   persists the derived sidecar, named for the sealed set the next start will
   find, so that start adopts the structures instead of rebuilding them.

Step 3 runs only with headroom: when a memory ceiling is set and the resident
set is over half of it, the drain is skipped and the server logs that it was.
Bringing everything current after a large delete can briefly need a great deal
of memory, and a skipped drain costs only a slower next start. The counts in
the reply are read after all of it, so a write that lands meanwhile shows in
`tail` rather than hiding behind the call's own seal.

It is **not** what keeps acknowledged writes. Paged mode is durable —
`--paged-dir DIR` opens `DIR/engram.wal` unconditionally, and that log is
appended and `fsync`'d before a write is acknowledged and replayed into the
tail on open — so a crash without a checkpoint loses nothing that was
acknowledged. See [Durability and recovery](../using/durability.md).

## Graph algorithms

Fifty-three of the sixty-one catalogue entries are `engram.algo.*`. Most are
not listed here, because they are a grid rather than a list: twelve algorithms
times four modes, plus five names that do not fit the grid.

The shape is `engram.algo.<algorithm>.<mode>`. The modes are `stream` (a row
per node), `stats` (one summary row), `mutate` (publish into the result cache
under a caller-chosen key) and `write` (persist as a node property). The
twelve algorithms are `pageRank`, `wcc`, `scc`, `degree`, `closeness`,
`betweenness`, `bfs`, `sssp`, `triangleCount`,
`localClusteringCoefficient`, `labelPropagation` and `louvain`.

The five outside the grid are `engram.algo.kShortestPaths.stream`, which has a
stream mode only because a route is not a per-node value;
`engram.algo.result.list`, `.stream` and `.drop`, which read and remove what
`mutate` published; and `engram.algo.project`, below.

All fifty-three have a body: the interpreter routes the `engram.algo.` prefix
to the algorithm runner, whose own dispatch covers all twelve algorithms and
all four modes, and the five names above are handled before it. The
catalogue's fallthrough arm exists anyway and refuses with "declared but not
implemented", because a catalogue entry with no body is a mistake made at
compile time and a database that panics on it is the worse outcome.

### `engram.algo.kShortestPaths.stream`

```cypher
MATCH (s:Person {name: 'Ann'}), (t:Person {name: 'Bo'})
CALL engram.algo.kShortestPaths.stream({nodeLabels: ['Person'],
  relationshipTypes: ['KNOWS'], sourceNode: id(s), targetNode: id(t), k: 3})
YIELD index, totalCost, nodeIds
RETURN index, totalCost, nodeIds ORDER BY index
```

The `k` shortest **loopless** routes between two nodes, by Yen's algorithm.
`sourceNode` and `targetNode` are required and are node ids; `k` defaults to 1.
Yields `index`, `sourceNode`, `targetNode`, `totalCost`, `nodeIds` and `asOf`,
one row per route, and fewer than `k` rows when the graph holds fewer distinct
routes.

### `engram.algo.project`

```cypher
MATCH (a:Person)-[r:KNOWS]->(b:Person)
WITH collect({source: id(a), target: id(b), weight: 1.0 / (1.0 + r.years)}) AS edges
CALL engram.algo.project({name: 'knows', nodeLabels: ['Person'], edges: edges})
YIELD projection
MATCH (s:Person {name: 'Ann'}), (t:Person {name: 'Bo'})
CALL engram.algo.kShortestPaths.stream({projection: projection,
  sourceNode: id(s), targetNode: id(t), k: 3})
YIELD index, totalCost
RETURN index, totalCost ORDER BY index
```

Builds an in-memory projection from **rows** rather than from stored
relationships, for an algorithm whose edge weights are computed by the query
rather than read from a property. It writes nothing to the graph.

| config key | | |
|---|---|---|
| `name` | `String`, required | non-empty, and must not contain `#` |
| `nodeLabels` | `String` or `List<String>` | the vertices — so a node with no edges is still in the graph |
| `edges` | `List<Map>`, required | each `{source, target, weight}`; `source` and `target` are nodes or node ids |
| `orientation` | `NATURAL` (default), `REVERSE` or `UNDIRECTED` | |

Yields `projection`, `nodeCount`, `relationshipCount`, `outsideProjection` and
`asOf`. Pass the yielded `projection` handle to an algorithm as its
`projection:` key; it cannot be combined with `nodeLabels`,
`relationshipTypes`, `relationshipWeightProperty` or `orientation`, because the
projection has already fixed all four. Any other config key is refused.

A missing or non-numeric weight is **refused, never defaulted**: a projection
whose every weight the caller computed has no business guessing one. The
projection lives only until the statement that built it ends, and it must be
built once, after an aggregation such as `collect()`, not per row of a
parallel stage. Its size counts against the projection byte ceiling.

See [Graph algorithms](../architecture/graph-algorithms.md) for the modes, the
refusal ceilings and the two caches.

## What is not supported

Calling anything else is refused by name:

```text
Neo.ClientError.Statement.NotSupported
procedure `db.stats.retrieve`
```

`db.awaitIndexes` is supported — see [above](#dbawaitindexes) — and answers
`true` immediately, because saying so is more honest than blocking on nothing.

Notably absent, and commonly reached for by Neo4j tooling:

- `db.schema.visualization`
- `db.awaitIndex`
- `db.stats.*`, `dbms.queryJmx`, `dbms.listQueries`
- `dbms.security.*` — there is no user model
- APOC, in general

Driver-introspection procedures are partial, which is one reason the driver
compatibility matrix is open rather than closed. See [Roadmap](../roadmap.md).

There is also **no user-defined procedure mechanism** — no plugin surface, no
way to register one. Adding a procedure means a catalogue entry and a body, not
an edit to a chain of names.

## Next

- [Cypher support](../using/cypher-support.md) — the language.
- [Schema, indexes and constraints](../using/schema.md) — creating the indexes
  these query.
- [Graph algorithms](../architecture/graph-algorithms.md) — the fifty-three
  `engram.algo.*` procedures, their four modes and their refusal ceilings.
- [Errors](./errors.md) — the status codes.
