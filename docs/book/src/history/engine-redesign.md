# Engine redesign

> **Historical — written 2026-08-22.** The canonical implementation reference
> for the planner and execution overhaul, grounded in a full read of the graph
> and store crates plus the modern graph-database literature.
>
> Its central conclusion has since been reached: the analytical queries it was
> written to fix are now [led on all nine](../measurements/index.md).

## The strategic thesis

> **Robustness beats cardinality-perfection.**

The systems that win on graph workloads do not have flawless cost models — they
have **operators that do not fall off a cliff** when the estimate is wrong.

That single sentence explains most of the engine's subsequent shape: a
rule-based planner, seeds that keep a correct fallback one comparison away,
recognisers that decline cleanly, and effort spent on operators rather than on
an estimator.

## What the measurement said

Measured at SF1 against the comparison engine, on the same corpus:

- **Engram won every short and point-lookup query by 1.3–41×.**
- **The comparison engine won every complex analytical query by 3–26×.**
- Two `shortestPath` queries were **pathological** — over three minutes against
  25 ms.

**Every analytical loser had the same shape:** an acyclic friends-of-friends path
`(:Person {id})-[:KNOWS*1..2]-(friend)`, then `WITH DISTINCT friend`, then a
star-join to messages or forums with a filter and an `ORDER BY … LIMIT`.

One shape, six losses. That is a mechanism, not a general slowness — and it
reorders the plan, because the obvious next step (cost-based join-order search)
is not what that shape needs.

## The survey, and its rejections

Eight techniques were surveyed. The **rejections** are the durable content.

**Worst-case-optimal joins — deferred.** WCOJ wins only on *cyclic* patterns —
triangles and four-cycles, the business-intelligence workload. It is roughly
**25× slower than binary joins on the acyclic benchmark**. These queries are
acyclic.

**Join-order search matters least.** Robust predicate transfer is *provably
robust to arbitrary join order of an acyclic query* — it collapses the
best-versus-worst-plan gap, so a perfect cost model is unnecessary. This is where
the thesis comes from.

**The dominant structural fix is a frontier BFS.** Variable-length expansion was
depth-first path enumeration with no visited set — the fallback the comparison
engine uses only when it cannot rewrite. Every faster system expands a frontier
over a visited set. For `KNOWS*1..2 … WITH DISTINCT friend`, the frontier visits
each node once and **the `DISTINCT` *is* the visited set**.

Single-threaded frontier shortest-path over ~20M edges is 10²–10³ ms; the
depth-first version was over 180,000 ms.

**The dominant representation cost** is cloning value tuples at every clause
boundary. The fix is columnar id vectors plus a selection vector, fetching
properties lazily by id.

## What was deferred, deliberately

- A learned cost model as the **primary** optimizer.
- Full JIT compilation — a prepared-plan cache and a leaner replan first.
- Ripping out the columnar interceptors before the general path is factorized.
- **Distributed execution** — out of scope; single-node fastest first.

Deferrals with reasons are as much a part of a design as the plan.

## Where it landed

Both dominant fixes exist:

- `expand_var_length_bfs` — a frontier BFS with a visited set, admitted for
  `min == 1` with a `DISTINCT`-only end.
- `try_shortest_path_bfs` — bidirectional BFS, which the source records as
  fixing the pathological case named above.
- `DataChunk` — id columns with a selection vector, and id columns never copied.

And the standing reversed: Engram now leads on **all nine** analytical queries.

## The part the document could not predict

Its ordering was by expected leverage, and the largest single win is not on the
list.

**First-call cardinality estimation** was a tax across the whole suite, invisible
to every profile because its cost hid in planning. Fixing it moved q4 from 1,868
to 584 ms and q5 from 3,121 to 2,176 within one window — queries the plan did not
name.

And q2, the narrowest margin today at 1.94× ahead, began **14.6× behind**. Its
path was three defects — a join order driven from the wrong side, a repeated
estimation, an unpriced first estimation — **none of them in the executor**,
which is where a document about execution engines would look.

## Next

- [The planner](../architecture/planner.md) — what was built.
- [The query path](../architecture/query-path.md) — the operators.
- [Measurements](../measurements/index.md) — where it ended up.
