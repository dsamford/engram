# Relationship-create parity

> **Historical.** An attribution of where relationship ingest spent its time.
> One of the cleanest examples in this part of the book of a measurement
> overturning the intuitive answer.

## The question

Loading SF1's 17,256,038 relationships, the comparison engine sustained
**11,759/s**. Engram did **6,395/s**.

Indexed `MATCH` + `CREATE` is the most load-bearing write shape a graph database
has. What closes the gap?

The sustained figure matters: sampling stayed flat at 12.1–12.4k/s through the
back half, so the comparison was not an early-phase artifact.

## The answer, measured

**The write path is 22% of the cost. 78% is the read/bind side of the `MATCH`.**

| term | µs/rel | share |
|---|---|---|
| parse + inline list + `UNWIND` | 1.07 | 2.4% |
| **`MATCH` seeks + candidate materialisation + row binding** | **31.67** | **69.7%** |
| `create_rel` — commit log | 5.08 | 11.2% |
| `create_rel` — puts, gets, fence, stats, adjacency log | 7.63 | 16.8% |

Nobody looking at "relationship creation is slow" starts by profiling the
`MATCH`.

## The defect underneath it

Most of that 78% was **one** thing: the range index was keyed **by property name
alone**, so a per-label integer `id` collided across roughly ten label families —
and **every collision was fully materialised and then discarded.**

The operation counts show it exactly:

| counter | the loader's plan, integer `id` | the same, with a distinct key | `create_rel` alone |
|---|---|---|---|
| store puts | 6.000 | 6.000 | 6.000 |
| commit-log appends | 6.000 | 6.000 | 6.000 |
| store gets | **18.686** | 9.000 | 4.000 |
| **full node materialisations** | **13.681** | 4.000 | **0** |
| range-index probes | 4.000 | 2.000 | 0 |

**13.68 full node materialisations per relationship**, where the write itself
needs zero. Each one decodes a whole record — fat text and embedding properties
included — to discover it is the wrong label and throw it away.

That is what became **label-scoped indexes**. See
[Indexes](../architecture/indexes.md).

## Why the counts are the argument

The timing says *where*; the counts say *what*.

"31.67 µs in the MATCH" invites a dozen theories. **18.686 gets and 13.681 full
materialisations per relationship, against 4.000 and 0 for the write** names the
mechanism, and the fix follows from the number rather than from a hypothesis.

Notice also what is **flat**: puts and commit-log appends are 6.000 across every
plan. The write path was never the variable, and the counts prove it before any
optimisation is attempted.

## The methodological note

The in-process driver **replays the loader's own plan** — a dump mode emits the
exact statements the loader would send — so the attribution cannot drift from
what the loader really does.

A profile of a *reconstruction* of a workload measures the reconstruction. This
is the same discipline as loading both engines through one loader: remove
everything that is not the thing under test.

## Next

- [Indexes](../architecture/indexes.md) — label scoping, and its gates.
- [Loading data at scale](../using/bulk-loading.md) — the load path today.
- [The write path](../architecture/write-path.md) — the 22%.
