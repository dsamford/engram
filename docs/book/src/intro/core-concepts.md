# Core concepts

[Your first graph](./first-graph.md) used these without naming them. This page
names them, and goes one layer below the query language into how Engram
actually stores what you wrote — enough that the
[Architecture](../architecture/overview.md) part reads as detail rather than as
new material.

## The data model

### Nodes

A node is an entity with an **id**, a set of **labels**, and a map of
**properties**. Ids are assigned by the engine, dense within a run, and stable
for the life of the node.

```cypher
CREATE (p:Person:Employee {name: 'Ada', born: 1815})
```

That is one node with two labels and two properties.

### Relationships

A relationship connects exactly two nodes. It has an id, exactly **one type**,
a **direction**, and its own properties.

```cypher
CREATE (a)-[:KNOWS {since: 1833}]->(b)
```

Two rules that differ from what people often expect:

- **Exactly one type**, never zero and never several. Labels are a set;
  relationship types are not.
- **The direction is always stored**, but a query may ignore it. `-[r]-`
  matches either way; `-[r]->` does not.

### Labels

A label is a set a node belongs to. Labels are how you scan
(`MATCH (p:Person)`) and what indexes and constraints attach to.

They are deliberately cheap to combine. Engram stores a node's labels two ways
at once: as a token set on the record itself, for reading a node you have
already bound, and as **one membership row per label**, for scanning a label
without touching nodes that lack it. That is why `MATCH (p:Person)` does not
degrade as you add unrelated labels elsewhere in the graph.

### Properties

Property values are typed. The full model is in
[Cypher support](../using/cypher-support.md), but the shape is: null, boolean,
integer, float, string, list, map, and the temporal types — date, time, local
time, datetime, local datetime and duration.

Two things worth knowing early:

- **Setting a property to null removes it.** This is standard Cypher, and
  Engram follows it: `CREATE (:T {v: null})` stores no `v` at all, so an
  explicit null and a property that was never set are indistinguishable to a
  query — `properties(n)` omits both, `keys(n)` omits both, and `n.v IS NULL`
  is true for both.

  The *storage format* underneath does carry a distinct NULL value tag, kept
  separate from "absent" on purpose, because collapsing the two loses
  information that cannot be recovered afterwards. Nothing in the query
  language surfaces that distinction today; it is a property of the on-disk
  format, reserved rather than exposed. See
  [Key encoding and on-disk formats](../architecture/key-encoding.md).
- **Three-valued logic is real here.** `null = 'x'` is neither true nor false —
  it is unknown, and a `WHERE` fails closed on it. This is standard Cypher, and
  it is relied on rather than approximated.

## Underneath the model

### Names are tokens

Labels, relationship types and property keys are **interned**. The catalogue
maps each name to a `u32` token once; records then carry tokens, and only the
wire carries strings.

The catalogue is **append-only**. A token, once minted, is never renamed —
renaming one would rewrite the meaning of every record already written with it.

The practical consequence: having many distinct label and property *names*
costs little, because a record pays four bytes per token rather than the length
of the name.

### Realms, namespaces and partitions

Every key in the store begins with the same prefix shape:

```text
realm:u32 │ namespace:u32 │ KIND:u8 │ partition:u32 │ <body> │ !commit_ts:u64
```

- **Realm** is the tenant. It comes first, so every scan is tenant-local by
  construction — and so an encryption key can be derived from the prefix with
  no key table to keep consistent.
- **Namespace** separates logical datasets within a tenant.
- **KIND** says what sort of row this is — a node, an edge, a catalogue entry,
  an index entry — as one byte from a frozen registry.
- **Partition** groups rows that should scan together.

A single-process server serves realm 1, namespace 1 by default, so you will not
meet these unless you go looking. They matter because the *layout* is frozen:
it is the on-disk format, and it is the decision this project ranks first by
cost of being wrong. See
[Key encoding and on-disk formats](../architecture/key-encoding.md).

### Commit timestamps and MVCC

Every write carries a **commit timestamp**, and every version of every row is
stored under a key ending in that timestamp — **inverted**, so the newest
version sorts first and a point read is the head of a prefix scan.

This is what makes Engram multi-version:

- A read takes a **snapshot** — a timestamp — and sees exactly the versions
  committed at or before it. Readers do not block writers, and writers do not
  block readers.
- A transaction's whole write set commits at **one** timestamp. That single
  value is the atomic visibility point: before it none of the writes are
  visible, after it all of them are.
- Old versions are retired by compaction, bounded by the oldest live snapshot.

The `bookmark: "eg:<commit_ts>"` in a Bolt reply is exactly this number, handed
back to the client.

See [Transactions and isolation](../using/transactions.md) for what this means
for concurrent writers, and
[The storage engine](../architecture/storage-engine.md) for where the versions
live.

### Derived structures

Some things are not stored so much as **maintained**: which nodes carry a
label, which nodes a given node is adjacent to by type and direction, how many
neighbours a node has.

Engram calls these **derived structures**, and they follow one rule: a derived
structure is a function of the rows, catches up incrementally from a change
log, and can always be rebuilt from the rows if it is lost. Nothing is
authoritative except the rows.

You meet them in three places:

- The `warmed in … ms: N nodes, … M adjacency table(s)` line at startup, which
  is Engram building them before accepting connections rather than making your
  first query pay for the whole corpus.
- The `--refresh-*` flags, which control how eagerly a background thread keeps
  them current.
- Query latency, when a large write burst has happened and nothing has read
  since.

[Derived structures](../architecture/derived-structures.md) is the full story,
and it is the page to read if write throughput or first-query latency ever
surprises you.

## The vocabulary, in one table

| term | what it means here |
|---|---|
| **node** | an entity: id, labels, properties |
| **relationship** | a typed, directed connection between two nodes, with properties |
| **label** | a set a node belongs to; the unit of scanning, indexing and constraints |
| **type** | a relationship's single kind |
| **property** | a typed key/value on a node or relationship |
| **token** | the `u32` a name interns to; records carry these, the wire carries strings |
| **realm / namespace** | tenant and dataset — the first two key components |
| **KIND** | one byte saying what sort of row a key addresses |
| **commit timestamp** | the version stamp; a transaction's whole write set shares one |
| **snapshot** | the timestamp a read sees the graph as of |
| **tail** | the in-memory, writable part of the store |
| **segment** | an immutable, sealed run of versions; read lock-free |
| **seal** | moving the entire tail into a new segment |
| **compaction** | merging sealed segments and retiring dead versions |
| **derived structure** | a maintained index — memberships, adjacency, degrees — rebuildable from the rows |

## Next

- [How Engram is different](./how-its-different.md) — the three decisions that
  explain the rest of the codebase.
- [Cypher support](../using/cypher-support.md) — the query language in detail.
- [Architecture overview](../architecture/overview.md) — the map of the engine.
