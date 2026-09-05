# Query planning

> **Historical — written 2026-08-20**, after the first port benchmark drove the
> first real planner work. Every number here was measured against an incumbent's
> server-side timings on a corpus of ~1.79M nodes and ~5.29M relationships.
>
> For the planner as it is now, see [The planner](../architecture/planner.md).

## The execution model at the time

The interpreter executed read-only chains — `MATCH` / `UNWIND` / `WITH` /
`RETURN` — as **streams**: reading clauses pushed rows one at a time into
chained sinks, aggregation folded into per-group accumulators, `ORDER BY`
buffered projected values only.

The row budget guarded what a caller actually asks the engine to hold — outputs,
sort buffers, group counts, expansion frontiers — never transient intermediates.

Writes, `CALL`, subqueries and `shortestPath` took the original materialising
path unchanged. It remained the correctness reference, and a 94-statement
decoded-values suite read identically through both.

## The five decisions, each with its measured motivation

| decision | trigger | before |
|---|---|---|
| **Smallest-label seed** | `(s:Bio:Species)` — a multi-label pattern seeded from the **first** label | 294k fat nodes scanned to keep a handful; **13.6 s** against the incumbent's 197 ms |
| **Relationship-driven seed** | `()-[r:T]->()` — a single unconstrained-start hop | seeded from all 1.79M nodes; **415 s** against sub-millisecond |
| **Projection pushdown at materialisation** | a pattern variable used only through properties | full property maps cloned per row to read two scalars; **2.7–6.4 s** against 6–40 ms |
| **Trail elision** | paths with no path variable | variable-length walks materialised every node they passed, for a trail nobody read |
| **Count fast path** | a bare `count(n)` / `count(r)` | full-graph counts materialised the database and OOM'd; now O(1)–O(keys) from the count stores |

Each is a *seed* decision or a *demand* decision, and both classes survive into
the current planner — the smallest-label seed and the relationship-driven seed
are `Seed::Label` and `Seed::Rels` today.

## Demand analysis is conservative by construction

The rule that made projection pushdown safe:

A **bare use** of a variable — `RETURN n`, `WITH n AS m`, `count(DISTINCT n)`,
any mention inside a subquery — demands the **full** value.

So projection can only ever **widen** a result, never narrow one. Node equality
is by identity, so a projected node compares exactly like a full one.

That is the shape worth taking from this document: an optimisation whose failure
mode is *doing too much work*, never *returning the wrong answer*. The same
principle appears throughout the engine — a columnar scan that declines past its
budget, a recogniser that refuses an unfamiliar shape, a seek that keeps the
label scan as a runtime fallback.

## What this document got right, and what it missed

**Right:** all five decisions are still in the engine, and the seed-choice
framing became `Seed`.

**Missed:** the single largest planner win was not on this list and was not
foreseeable from these profiles. **First-call cardinality estimation** was a tax
across the whole suite, and no profile attributed it because its cost hid in
*planning* — split across events no counter aggregated. Fixing it moved queries
this document does not mention, by more than the items it ranked first.

That is the recurring lesson of this part of the book: **a profile can only
attribute what it instruments.**

## Next

- [The planner](../architecture/planner.md) — where this ended up.
- [Execution engine evaluation](./execution-engine-evaluation.md) — the deeper
  attribution that followed.
- [Tail remediation plan](./remediation-plan.md) — the ordered response.
