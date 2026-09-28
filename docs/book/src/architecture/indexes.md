# Indexes

Five kinds, and one thing they share: **every one is a
[derived structure](./derived-structures.md)** — a cache of rows, rebuildable
from them, never authoritative.

Nothing here is stored in a way that could disagree with the store. The worst a
lost or stale index can do is cost time.

## Range indexes

The workhorse: a sorted structure over property values, serving equality,
prefix, range and `IN`.

### The representation

A shared immutable **base** plus a small **overlay**, folded into a new base at
a threshold — the same shape every derived structure here uses, so a delta costs
O(delta) rather than O(base).

| constant | value | |
|---|---|---|
| `RangeIndex::FOLD_AT` | 4,096 | overlay size before a fold |
| `RangeIndex::RECENT_CAP` | 256 | removals kept in a small sorted bucket |

Three counters distinguish the paths, and the distinction is the point:

| counter | cost |
|---|---|
| `idx_builds` | a **full** build — O(group) |
| `idx_catchups` | clones and re-sorts the added set |
| `idx_folds` | a fold — O(base) |

Divide by the read count and you get per-read frequency; frequency times a known
cost turns a correlation into a mechanism.

### Label scoping

An index is scoped to its label by default, so `Person.id` and `Company.id` are
separate.

Unscoped, they share one index keyed by property name alone — and a per-label
integer `id` then collides across every label family, with **every collision
fully materialised and then discarded**. That lands on relationship ingest
hardest, where every `MATCH` that anchors an endpoint by `id` pays for the
collisions from every other label before the write path does any work.

`--no-label-scoped-indexes` restores the unscoped behaviour as the control.

### Seek admission

An anchored `MATCH` seeks only when the seek is likely to win:

| gate | value |
|---|---|
| `PROPERTY_SEEK_MIN_LABEL` | 512 nodes |
| `PROPERTY_SEEK_SELECTIVITY` | 16× |
| `PROPERTY_SEEK_MAX_PROBE` | 2,048 |

Below those, the label scan is faster and the planner takes it. **The label scan
stays available at runtime and wins whenever it is the smaller candidate set** —
so a bad estimate costs a comparison, not a query.

This is the usual reason an index appears unused.

### Composite indexes

A declared range index over two or more properties of one label is a
**composite**, and it is derived rather than stored: the component single-key
indexes' live entries are joined by body into tuple keys. No record is read —
each component is served however any probe serves it, from its slot, from disk,
or caught up over its change log — so a composite costs no store read and
inherits each component's overlay resolution. It is published in a `Slot` at a
fenced stamp like every other derived structure, and the stamp is read *before*
the components are taken, so a row written after it is re-derived on the next
read rather than missed.

It is **string-only**. A component key that is not a string contributes no
tuple and is counted as **left out**, added to the components' own unindexable
counts — the honest floor a range index is allowed to report, rather than a
guess at an ordering for a value the tuple encoding does not have.

Where a seek's keys are covered by more than one declared composite, the
**widest** one is chosen, so a three-key seek uses the three-key index rather
than leaving a key to re-verification. Two composites of equal width resolve
deterministically from the sorted catalogue.

### Sidecars

Declared range indexes are written to sidecar files on a quiescent paged tick,
so a restart adopts rather than rebuilds. Only **declared** indexes are
persisted — one an ad-hoc query happened to build would otherwise become a
permanent cost at every tick — and the reader discards a sidecar whose vintage
has moved, so a stale file costs a rebuild, never a wrong answer. The growth
interval that paces rewrites belongs to the derived sidecar, not to these; see
[Paged mode](./paged-mode.md#sidecars).

## Vector indexes

Two paths, chosen by size.

### Exact scan

Below 2,048 vectors, a scan answers. It is **int8-quantised** with an f32
rescore over an oversampled candidate set:

```text
scan int8 → keep k × OVERSAMPLE (2) candidates → rescore in f32 → top k
```

The quantised scan narrows, the exact rescore decides. The one place the pair
can part from an exact search is a near-tie closer than the int8 quantisation
error, where the scan may keep either twin.

### HNSW

Above the crossover, a hierarchical navigable small-world graph.

| parameter | value |
|---|---|
| `M` | 24 connections per node above level 0 |
| `M0` | 48 at level 0 |
| `EF_CONSTRUCTION` | 200 |
| `EF_SEARCH_MIN` | 400, effective beam `max(400, 4k)` |
| `LEVEL_NORM` | 1 / ln(M) |

**It is deterministic.** Level assignment comes from a SplitMix64 stream seeded
by the node's external id, not from a thread-local RNG — so the same data builds
the same graph, and a vector search is reproducible in the simulation. That is
unusual for an HNSW and it is a direct consequence of D1.

### Maintenance

Pending ids accumulate to `VECTOR_DELTA_CAP` (4,096) before a rebuild; a bloat
ratio of 0.25 also triggers one. The metric is **cosine**, normalised at insert.

The dimension is **inferred from the data**. `OPTIONS { … }` in the DDL is
parsed and ignored — see [Schema](../using/schema.md).

## Full-text indexes

An inverted index over tokenised text.

**Scoring is Okapi BM25 with Lucene's parameters** — k1 = 1.2, b = 0.75, and
BM25's own idf — for any index created now.

The scoring is stamped into the index's catalogue row **at create** and read
back from it, never consulted at query time. A lever would mean two servers
configured differently ranked the same index differently, and a rolling upgrade
re-ranked mid-query-set. A row with **no** scoring key means term frequency, so
every index written before BM25 existed keeps the scoring it has always had —
which is what makes adding BM25 a change no existing deployment notices. A
scoring this build has never heard of, from a newer one, skips the row rather
than guessing a formula.

Two arms, and they answer different questions. `--no-bm25` stops a BM25 index
serving a query at all; the scan then computes the same scores the slow way, so
it prices the **index**, not the formula. `--no-bm25-by-default` stamps newly
created indexes `Tf` and leaves existing ones alone.

The tokenizer splits on non-alphanumerics and lowercases. No stemming, no
stopwords, no configurable analyzer, and k1 and b are compiled in with no lever.
`SHOW INDEXES` reports `FULLTEXT` for both scorings, so the listing does not say
which an index uses.

## Trigram indexes

What makes `=~`, `CONTAINS` and `ENDS WITH` seekable instead of scans. Four
rules set it apart from every other index on this page:

- **Nodes only, one label and one property.** There is no relationship form.
- **Never persisted.** It lives in a bounded slot cache — `TRIGRAM_CACHE_MAX` is
  **16** slots against the range cache's 256, because a trigram index holds one
  entry per three-character window of every value and is an order of magnitude
  larger per row than a range index over the same property.
- **Built lazily**, on the first probe that needs it rather than at create, on
  the reader's thread and behind the same `SingleFlight` guard every other
  derived structure uses.
- **One non-string value under the indexed property disables it outright.** This
  is the one place it diverges from the range index, and the reason is the shape
  of the answer: a range index that skips an untyped row returns an honest floor
  and says so, while a trigram index returns a *candidate set*, and a candidate
  set that is a floor is simply a wrong answer — the row it omitted might have
  matched.

Every id it returns is re-verified by running the actual predicate, so the
answer is a superset and never an oracle. Every decline path returns "cannot
serve — scan instead" rather than a partial answer.

`--no-trigram-indexes` is the arm: the predicates scan their label.
[The trigram index](../reference/trigram-index.md) carries the thresholds.

## Adjacency and membership

Not user-declared, but the same machinery and by far the hottest.

**Adjacency tables** are CSR — a sorted contiguous neighbour array with a row
directory — one per `(relationship type, direction)`.

The row directory is **sparse**, and that was a fix. A dense one costs O(ids)
*per table*, so across many relationship types the offsets grow with types × ids
while the entries grow only with the relationships. The sparse form is a bitmap
plus a rank directory.

**Membership views** are an immutable base plus added/removed overlays, with an
optional presence bitmap past 4,096 probes and a materialised flat form on
demand.

**Degree tables** are built only after `--degree-table-after` (1,024) direct
probes in an epoch — and the counter resets on the *global* adjacency epoch, so
under a write stream it may never reach the threshold and a table for an
untouched type is never built. `0` admits immediately, as the A/B arm.

## Constraint markers

Not an index exactly, but the same idea used for enforcement: a uniqueness
constraint writes a **marker row** whose key encodes the constrained tuple.

Two concurrent creates of the same value write the same key and one loses at
commit. **The enforcement is the keyspace**, not a check that could race.

## What does not exist

- **No composite index spanning two labels**, and **no composite over non-string
  keys** — a composite is derived from one label's single-key indexes and joins
  string keys only.
- **No partial or filtered indexes.**
- **No index hints** — you cannot force a plan.
- **No online build ladder** — creation builds immediately, holding up writes to
  that label. The exception is a trigram index, which is built on the reader's
  thread at the first probe that needs it.
- **No configurable full-text analyzer**, and no lever on k1 or b.

See [Roadmap](../roadmap.md).

## Next

- [Schema, indexes and constraints](../using/schema.md) — creating them.
- [Derived structures](./derived-structures.md) — the machinery underneath.
- [The planner](./planner.md) — when a seek is chosen.
