# Schema, indexes and constraints

Engram is schema-optional: you can write nodes and relationships without
declaring anything. What you declare buys two things — **faster lookups**
(indexes) and **enforced invariants** (constraints).

The statements on this page are the forms the parser accepts, and the messages
quoted below them are the engine's own text.

## Indexes

### Range indexes

The workhorse. Without one, `MATCH (p:Person {name: 'Ada'})` scans every
`Person`.

```cypher
CREATE INDEX person_name FOR (p:Person) ON (p.name)
```

Composite, conditional, and the relationship form:

```cypher
CREATE INDEX person_name_city IF NOT EXISTS FOR (p:Person) ON (p.name, p.city)
CREATE INDEX rel_since FOR ()-[r:KNOWS]-() ON (r.since)
```

Range indexes serve equality, prefix and range predicates, and `IN` lists.

**Every component of a composite must be a string.** A composite is derived by
joining its components' own indexes into tuple keys, and a component value that
is not a string contributes no tuple at all — so `ON (p.name, p.born)`, with an
integer `born`, is accepted as DDL and then never serves a composite seek. It
does still serve seeks on its leading string property, because a seek looks for
a declared range index whose *first* property is the one it wants. Index a
numeric property on its own.

**Label scoping matters here.** By default an index is scoped to its label, so
`Person.id` and `Company.id` are separate. Unscoped, they would share one index
and a lookup on either would materialise both — a defect that was measured and
fixed. `--no-label-scoped-indexes` restores the unscoped behaviour as the A/B
control.

### Vector indexes

```cypher
CREATE VECTOR INDEX emb FOR (d:Doc) ON (d.v)
```

```cypher
CALL db.index.vector.queryNodes('emb', 2, [1.0, 0.0])
YIELD node, score
RETURN node.t AS t, score
```

```text
t      score
a      1.0
c      0.9938837346736189
```

The metric is **cosine similarity**, so an exact match scores 1.0.

Two things to know:

- **The dimension is inferred from the data**, not declared. Vectors of
  inconsistent length in one index are a data error, not a schema one.
- **`OPTIONS { … }` is parsed and ignored.** You cannot set
  `vector.dimensions` or `vector.similarity_function`. The map is accepted so
  Neo4j-shaped DDL still parses — which means it will silently not do what a
  Neo4j user expects.

Small indexes are answered by an exact scan; past a crossover (2,048 vectors by
default) an HNSW graph serves them. Both are deterministic — the HNSW's level
assignment is seeded from the external id, so the same data builds the same
graph. See [Indexes](../architecture/indexes.md).

### Full-text indexes

```cypher
CREATE FULLTEXT INDEX ft FOR (d:Doc) ON EACH [d.title, d.body]
```

```cypher
CALL db.index.fulltext.queryNodes('ft', 'analytical engine')
YIELD node, score
RETURN node.title AS title, score
```

**Scoring is Okapi BM25** for any index created today — Lucene's `k1 = 1.2` and
`b = 0.75`, with idf, and neither parameter tunable. The stamp travels with the
index rather than with the server: an index created before BM25 existed keeps
term-frequency scoring for ever, and `SHOW INDEXES` does not report which of the
two an index uses. `--no-bm25-by-default` stamps a newly created index for term
frequency instead.

The tokenizer splits on non-alphanumerics and lowercases; there is no stemming,
no stopword list and no configurable analyzer.

### Trigram indexes

```cypher
CREATE TRIGRAM INDEX file_content FOR (f:File) ON (f.content)
```

This is the index that makes four predicates seekable that are otherwise
scans: `=~`, `CONTAINS`, `ENDS WITH`, and `STARTS WITH` where no range index is
declared. The middle two are the ones that matter most, because a range index
cannot answer either at any price: neither is a contiguous span of any sort
order.

Four things to know:

- **Nodes only, one label, one property.** There are no lists here as there are
  in the full-text form: a per-property inverted structure spanning several
  properties would make the write hook read the whole node on every write to any
  of them.
- **The declaration survives a restart; the structure does not.** Only range
  indexes are written to on-disk sidecars, so a trigram index is rebuilt on the
  first query that needs it.
- **One non-string value in the indexed property disables it**, and the scan
  answers instead. This is the one place it diverges from the range index, which
  reports an honest floor over the rows it could order and says so. Here the
  answer is a candidate set, and a candidate set that is a floor is simply a
  wrong answer — the row it skipped might have matched.
- **The answer is a superset.** Candidates are whatever satisfies the trigram
  condition derived from the pattern, and the real predicate is then run against
  every one of them; a condition wider than necessary costs time, and one
  narrower than necessary would lose rows silently, so every rule in the
  analysis returns "scan instead" the moment it is unsure.

On by default; `--no-trigram-indexes` makes those four predicates scan their
label, as the A/B arm. See [Trigram index](../reference/trigram-index.md) for
what the analysis can constrain and when it declines.

## Constraints

### Uniqueness

```cypher
CREATE CONSTRAINT person_id FOR (p:Person) REQUIRE p.id IS UNIQUE
```

Violating it is refused:

```text
Neo.ClientError.Statement.ExecutionFailed
constraint violation: `Person`.(`id`) already exists on node 1
```

The message names the label, the property and **the node that already holds the
value**, so you can go and look at it.

### Node key

Composite uniqueness that also requires the properties to exist:

```cypher
CREATE CONSTRAINT nk FOR (p:P) REQUIRE (p.a, p.b) IS NODE KEY
```

### Existence

```cypher
CREATE CONSTRAINT ex FOR (p:P) REQUIRE p.c IS NOT NULL
```

Relationship forms work too:

```cypher
CREATE CONSTRAINT r_uniq FOR ()-[r:KNOWS]-() REQUIRE r.id IS UNIQUE
```

### How uniqueness is enforced

Worth knowing, because it explains two flags.

A uniqueness constraint writes a **marker row** whose key encodes the
constrained tuple. Two concurrent creates of the same value therefore write the
same key, and one loses at commit — the enforcement is the keyspace, not a
check that could race.

Constrained writes also consult a **schema-epoch key** to learn whether the
constraint set changed. That key is *absent* until the first constraint DDL,
and a sparse index cannot reject an absent key, so probing it descends every
sealed segment. The epoch cache removes that probe;
`--no-constraint-epoch-cache` restores it as the control.

## Inspecting what exists

```cypher
SHOW INDEXES
```

```text
person_name   RANGE   NODE   ["Person"]   ["name"]   ONLINE
```

Also available:

```cypher
SHOW CONSTRAINTS
CALL db.labels()            YIELD label            RETURN label
CALL db.relationshipTypes() YIELD relationshipType RETURN relationshipType
CALL db.propertyKeys()      YIELD propertyKey      RETURN propertyKey
```

`YIELD … RETURN` lets you narrow and rename the columns; a bare `CALL` that
ends the query returns the procedure's declared outputs. See
[Cypher procedures](../reference/procedures.md).

## Dropping

```cypher
DROP INDEX person_name IF EXISTS
DROP CONSTRAINT person_id IF EXISTS
```

## What to index

| pattern | index |
|---|---|
| `MATCH (p:Person {email: $e})` | range on `Person.email` |
| `MATCH (p:Person) WHERE p.born > 1800` | range on `Person.born` |
| `MATCH (p:Person) WHERE p.name STARTS WITH 'A'` | range on `Person.name` |
| `MATCH (d:Doc) WHERE d.body CONTAINS '…'` or `d.body =~ '…'` | trigram index on `Doc.body` |
| semantic similarity over embeddings | vector index |
| keyword search over text | full-text index |
| a business identity that must not duplicate | uniqueness constraint |

An anchored `MATCH` seeks a range index when the planner judges it worthwhile —
gated on the label being large enough (512 nodes), the predicate selective
enough (16×), and the match set small in absolute terms (2,048 rows), because
seeking an index that is not selective is slower than scanning it.

That third gate is the one most people meet. A full node decode costs far more
than reading a column entry, so even a selective predicate over a very large
label is more decodes than the scan it would replace: 5% of a 600,000-node label
passes both of the first two gates and is still refused. The three numbers are the
defaults for the paths that materialise a node per id; a path that walks the
sought ids reading columns costs about a column entry per id and takes a wider
seek. `--no-property-seek` forces the scan, as the control.

## Building on existing data

Creating an index on a populated label builds it immediately. There is **no
online build ladder** — no absent → delete-only → write-only → backfill →
validate sequence — because single-shard atomicity substitutes for one today.
On a large label the DDL will take a while and hold up writes to it.

Constraints are validated against existing data at creation, and creation is
refused if the data already violates them.

## What does not exist

- **No schema migrations.** No versioning, no ordering, no post-apply
  assertion; a failing DDL does not fail a boot.
- **No online constraint build.**
- **No tenancy-class DDL** — a label cannot yet declare how it is owned.
- **No index hints.** You cannot force a plan.

See [Roadmap](../roadmap.md).

## Next

- [Indexes](../architecture/indexes.md) — how each kind is built and read.
- [Transactions and isolation](./transactions.md) — what a violation does to a
  transaction.
- [Tuning guide](../reference/tuning.md) — when an index is not being used.
