# Indexes

Four kinds, and one thing they share: **every one is a
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
fully materialised and then discarded**. That was measured as the dominant cost
of relationship ingest: the write path was 22% of it, and 78% was the read side
of the `MATCH`, most of that this defect.

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

### Sidecars

Declared range indexes are written to sidecar files on a quiescent paged tick,
so a restart adopts rather than rebuilds. Rate-limited by a growth interval
(default 600 s): without it, one build rewrote a 1.3 GB file nine times in four
minutes.

## Vector indexes

Two paths, chosen by size.

### Exact scan

Below 2,048 vectors, a scan answers. It is **int8-quantised** with an f32
rescore over an oversampled candidate set:

```text
scan int8 → keep k × OVERSAMPLE (2) candidates → rescore in f32 → top k
```

Measured recall at k=10: **1.0**. The quantised scan narrows, the exact rescore
decides.

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

**Scoring is term frequency, not BM25.** The tokenizer splits on
non-alphanumerics and lowercases. No stemming, no stopwords, no configurable
analyzer.

Scores are comparable within one query and should not be read as relevance in
the BM25 sense.

## Adjacency and membership

Not user-declared, but the same machinery and by far the hottest.

**Adjacency tables** are CSR — a sorted contiguous neighbour array with a row
directory — one per `(relationship type, direction)`.

The row directory is **sparse**, and that was a fix. A dense one costs O(ids)
*per table*: on one corpus with 159 relationship types and ~3.4M ids named, that
was **4.36 GB of offsets carrying 540 MB of entries**. The sparse form is a
bitmap plus a rank directory.

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

- **No composite index across labels.**
- **No partial or filtered indexes.**
- **No index hints** — you cannot force a plan.
- **No online build ladder** — creation builds immediately, holding up writes to
  that label.
- **No configurable full-text analyzer.**

See [Roadmap](../roadmap.md).

## Next

- [Schema, indexes and constraints](../using/schema.md) — creating them.
- [Derived structures](./derived-structures.md) — the machinery underneath.
- [The planner](./planner.md) — when a seek is chosen.
