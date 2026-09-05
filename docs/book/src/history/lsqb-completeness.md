# LSQB completeness

> **Historical.** The programme that took every LSQB query from "some of these
> do not finish" to all nine completing — and eventually to
> [all nine led](../measurements/index.md).

## The goal

Every query q1–q9 completes at SF1, with answers that are **correct**, not
merely produced.

The second half is why the programme took the shape it did.

## The oracle came first

Before optimising anything, an independent implementation — `lsqbref` — computes
the same nine counts **without going through the engine**.

That is the load-bearing piece. A battery and an engine can agree on a wrong
answer indefinitely: the battery asks, the engine answers, and nothing in the
loop knows the answer is wrong. An oracle built from the corpus by different
code breaks that loop.

It ran in milliseconds and agreed with the engine on every count across three
corpus sizes and both execution paths.

**A fast wrong answer is not a result** — so the batteries take an `--expect`
file, and a count that disagrees fails the run rather than printing a timing.

## Three paths, all agreeing

Every query was checked on **three** paths:

- the default path, with the count fold **on**;
- the same with the fold **off**;
- the general enumerating path.

All three must produce the oracle's answer.

That is more than a correctness check — it is what makes the optimisation
*measurable*. Fold-on against fold-off on the same corpus, with counts asserted
identical, is a real A/B with a real control.

## What the ratios showed

Measured as ratios only, on two corpus sizes — general path ÷ fold-on, and
fold-off ÷ fold-on:

| q | operators on the default path | general ÷ fold | fold-off ÷ fold |
|---|---|---:|---:|
| q1 | count fold + memo | 126.55× | 29.29× |
| q2 | count-only reorder + count fold | 263.24× | 22.86× |
| q3 | count-only reorder + count fold | *(skipped)* | 1.11× |
| q4 | count fold + memo | 25.66× | 1.59× |
| q5 | count fold | 20.31× | 1.60× |
| q6 | count fold + memo | **1,136.77×** | 39.51× |

Two readings:

**The general path is three orders of magnitude off on q6.** That is the count
fold's entire value in one number — enumerating what you only intend to count.

**The `general ÷ fold` and `off ÷ fold` columns differ enormously.** Turning the
fold off is not the same as taking the general path, because other recognisers
still apply. Reporting both is what distinguishes "this optimisation" from "all
optimisations".

## Why ratios and not milliseconds

The measurements were taken on a development machine, and the document says so —
they are reported **only as ratios**, and the absolute timings are not quoted.

A ratio between two arms in the same window on the same machine is meaningful. A
millisecond figure from a laptop, quoted next to a benchmark host's, is not, and
the discipline is to not publish it rather than to caveat it.

## What it became

All nine queries complete, and the current standing is **all nine led**, from
1.94× to 54.2×. See [Measurements](../measurements/index.md).

The narrowest of them, q2, began the wider campaign **14.6× behind** — and the
count-only reorder named in the table above was the first of the three fixes
that reversed it.

## Next

- [Measurements](../measurements/index.md) — the current standing.
- [The planner](../architecture/planner.md) — the count fold and the reorder.
- [Benchmarking](../development/benchmarking.md) — `lsqb` and `lsqbref`.
