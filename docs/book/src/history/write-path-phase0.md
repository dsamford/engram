# Write path, phase 0

> **Historical.** What landed on the write path, why, and — for the items
> carrying a correctness argument — **the canary that proves the test is
> load-bearing**.
>
> This is the longest document in the engineering record, and it is where the
> campaign's final verdict was written.

## The recurring pattern

Nearly every entry has the same three parts:

1. a defect, named precisely;
2. a fix;
3. **a canary** — the deliberate break that makes the test fail, proving the
   test was checking what it claimed.

That third part is the discipline this codebase is built on. A test added
alongside a fix will pass. Whether it would have *failed* before the fix is a
separate question, and it is the only one that matters.

## The fix that a previous fix caused

The clearest entry, and a general lesson.

[RC2](./rc2-sealed-prefix.md) taught OCC validation to stop walking sealed
segments once it reached one sealed at or below the snapshot. The bound it
consults is a segment's `max_commit_ts`.

On the **paged** path that is a stored footer field. On the **resident** path it
was **recomputed from scratch** — a flat map over every version of every entry,
plus every column block.

Two things made it worse than it sounds:

1. **The early exit was evaluated *after* the call.** The newest segment was
   interrogated unconditionally — on every validated key, on every commit,
   including the happy case the bound exists to make cheap.
2. It ran **inside the global commit latch**. At the default seal threshold and
   about nine validated keys for a relationship create, that is on the order of
   **590,000 iterations per commit** at the one serialisation point that cannot
   be parallelised.

The paged path was not exempt either: a seal produces a resident segment.

**A performance fix can be slower than what it replaced**, when the bound is
cheap on one representation and expensive on the other and only one is measured.

## The same-window verdict

The document ends with the campaign's standing, measured as one script, one
window, N=3 medians, identical answer counts on every query:

**Ahead on all nine analytical queries**, from **1.94× to 54.2×**. Combined with
the stress suite's twenty levels: **ahead on all nineteen workloads.**

### q2 is the entry worth reading

The narrowest margin on the board, at 1.94× — and it began at **14.6× behind**.

Its path, end to end:

| stage | standing |
|---|---|
| a cyclic join driven from the wrong side | 14.6× **behind** |
| after the peak-ordered plan | 1.76× behind |
| after the hop-count memo | 1.43× ahead |
| after the sampled estimator | **1.94× ahead** |

**Three different defects, none of them in the executor**: a join order, a
repeated estimation, and an unpriced first estimation.

A document about a *write path* ending on a query-planning result is not a
digression — it is what happens when attribution is followed rather than
assumed.

### The estimator overshot its prediction

The sampled estimator was predicted to land q2 at 1,300–1,400 ms. It landed at
**886**, and moved queries it was not aimed at: q4 fell 1,868 → 584 and q5
3,121 → 2,176 **within one window**.

> First-call cardinality estimation had been a tax across the whole suite; **no
> profile attributed it** because its cost hid in planning, split across events
> no counter aggregated.

The prediction being recorded *before* the run is what makes "it overshot" a
statement rather than a rationalisation.

## The instrument that was the problem

One entry is worth carrying on its own.

Two runs sat on q3 for between ten and sixty-six minutes. Three single batteries
and three q3 singles did not reproduce it. A static review found nothing.

**The instrument was the problem.** The battery ran an **existence probe** —
`… RETURN 1 LIMIT 1` — before every count, and reported **only the count's**
milliseconds. So a probe taking three minutes printed as "q3 4.5 s".

The mechanism, once traced: the count reached the planner and folded nine hops;
**the probe never reached the planner at all** and fell to the enumerating
general path, which walked the pattern as written. Ten times the data cost the
pipeline 2.4× and that path 90×.

Two further corrections in the same entry:

- A thread dump showing every worker idle was a dump of the **wrapper process** —
  a pattern that matched itself.
- "Counters unchanged" never meant idle: the counter printer suppresses
  unchanged lines, and the probe's path touched none of the counters being
  watched.

**Three wrong readings of the same event, each individually reasonable.**

## The gate that caught a silent failure

The first run of the head-to-head script caught its own copy bug: a destination
path with a drive colon parsed as a remote path and **wrote nothing**.

What surfaced it was **the guard that refuses to measure phase 2 without phase
1's artifact** — described as the sixth silent-instrument failure the campaign
converted into a loud one.

Six, in one campaign, on a project that is unusually careful about this. That is
the base rate to expect.

## Next

- [Measurements](../measurements/index.md) — the standing, with its caveats.
- [RC2 — the sealed prefix](./rc2-sealed-prefix.md) — the fix this corrected.
- [Benchmarking](../development/benchmarking.md) — the silent-instrument rules.
