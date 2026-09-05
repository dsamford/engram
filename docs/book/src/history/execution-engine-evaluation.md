# Execution engine evaluation

> **Historical — written 2026-08-21**, after a full port benchmark. It
> attributes every slow statement to a mechanism in the engine, maps the state
> of the art onto what existed and what was missing, and orders the work by
> leverage over cost.
>
> Every claim about a cost was **verified in the interpreter source**, not
> inferred from the timing.

## Where the engine stood

A row-at-a-time streaming interpreter over an MVCC store with columnar compacted
blocks.

The planner ladder to that point: seed selection (label counts, range-index
point probes, relationship-driven, columnar candidate batches), demand analysis,
sound predicate pushdown, a clause-scan memo, bound-bound `exists()` adjacency
probes, a degree-count fast path, top-k for `ORDER BY … LIMIT`, and count fast
paths.

**The verdict:** the floor was excellent — p50 **4.27 ms**, a third of
statements sub-millisecond — **and the tail was everything**. 56 statements over
one second, totalling **1,455 s against the incumbent's 21.9 s** on the same
data.

That framing is the useful part. A median that looks good and a tail that is
100× worse is not a slow engine; it is a small number of mechanisms behaving
pathologically, and they can be named.

## The tail, attributed

Feature counts across the 56 slow statements: aggregation 40, `ORDER BY` 13,
all-nodes scan 11, anonymous expansion 8, `exists()` 7, `DISTINCT` 5.

### M1 — Aggregation materialised what it should fold

The largest lever. `MATCH (m) WHERE … RETURN count(m)` measured **123.3 s** over
1.79M nodes.

Three compounding facts, each verified in the source:

1. **The accumulator collected, then folded.** Every aggregated value was cloned
   into a `Vec` and folded once at finish — so `count(m)` cloned 1.79M full node
   values *in order to count them*. Only `count(*)` streamed.
2. **A bare variable demanded the full value.** `count(m)` forced a full record
   decode per row — fat text and embedding properties included — where the
   aggregate needs only *presence*.
3. **The group map keyed by JSON serialisation per row**, with a linear equality
   scan as fallback. A heap serialisation per input row.

The fix was one package: streaming accumulators, **aggregate-aware demand**
(`count(var)` needs presence, `count(DISTINCT var)` needs identity), and a
structural group key.

Note the shape of defect 2 — it is *demand analysis being correct but too
coarse*. The conservative rule from
[query planning](./query-planning.md) says a bare variable demands the full
value, which is right in general and wrong for an aggregate that only counts.

### The other five

| mechanism | shape |
|---|---|
| `ORDER BY` | buffered more than the top-k needed |
| all-nodes scan | a pattern with no usable seed |
| anonymous expansion | a hop whose endpoint is never read, still materialised |
| `exists()` | a probe that enumerated rather than short-circuiting |
| `DISTINCT` | a set built over full values rather than identities |

Each has a counterpart in the current engine: the count fold, top-k before
projection, lean binding of unread hop ends, the bound-bound probe.

## The state-of-the-art survey

The document mapped published techniques onto the measured gaps, and the
conclusions worth keeping are the **rejections**:

**Worst-case-optimal joins — deferred.** WCOJ wins on *cyclic* patterns
(triangles, four-cycles). It is substantially slower than binary joins on
acyclic ones. The friends-of-friends shape that dominated these queries is
acyclic.

**Join-order search matters least.** Robust predicate transfer is *provably
robust to arbitrary join order of an acyclic query*, which collapses the
best-versus-worst-plan gap. **Robustness over cardinality-perfection** became
the engine's stated position, and it is why the planner is rule-based and the
operators carry the weight.

**The dominant structural fix was a frontier BFS.** Variable-length expansion
was depth-first path enumeration with no visited set. Every faster system
expands a frontier over a visited set. For `KNOWS*1..2 … WITH DISTINCT friend`,
the frontier visits each node once and the `DISTINCT` *is* the visited set.

That one is now implemented — see
[The query path](../architecture/query-path.md).

**The dominant representation cost** was cloning value tuples at every clause
boundary; the fix is columnar id vectors plus a selection vector, fetching
properties lazily by id. That is the `DataChunk` the pipeline now uses.

## What to take from it

Two things.

**Attribute before you optimise.** Every item here names a statement, a
mechanism, and a line of code. The alternative — optimising what feels slow —
produces the case this document's successors kept hitting: three theories dying
to instruments before the right one was found.

**A technique being state of the art is not a reason to adopt it.** WCOJ was
named, evaluated against the actual query shapes, and declined. That decision
has held.

## Next

- [Tail remediation plan](./remediation-plan.md) — the ordered response.
- [Engine redesign](./engine-redesign.md) — the target architecture.
- [The query path](../architecture/query-path.md) — what was built.
