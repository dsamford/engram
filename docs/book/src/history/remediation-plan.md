# Tail remediation plan

> **Historical — written 2026-08-21**, as the execution order companion to
> [Execution engine evaluation](./execution-engine-evaluation.md).
>
> Impact figures are the measured seconds each rung removes from the **1,455-second
> tail** that evaluation found across 235 statements.

## The shape of the plan

Impact-first, dependency-ordered, with each rung carrying its **measured**
impact rather than an estimate:

| | work | measured impact | effort |
|---|---|---|---|
| **G0** | pin grouping and `DISTINCT` numeric-equivalence semantics as tests | *a correctness gate* | hours |
| **R1** | adjacency-count primitives — degree and single-hop counts from adjacency **key bytes**, no record decode; type histogram from key tokens | **~970 s** | small–medium |
| R2–R5 | localised operator work | — | — |
| R6–R8 | the architectural half | — | — |
| R9 | the long arc | — | — |

Two things about that table are worth more than the numbers in it.

## A correctness gate above the impact list

**G0 is first, and it removes zero seconds.**

It pins grouping and `DISTINCT` semantics as tests before two of the performance
rungs are allowed to proceed. Those rungs change how grouping is computed, and
an optimisation that is fast and subtly wrong about numeric equivalence is worse
than the slow version.

Ordering a correctness gate above a 970-second win, in a plan whose stated
ordering principle is impact-first, is the plan disagreeing with itself on
purpose.

## The largest rung is a decode that does not happen

R1 is worth roughly **970 of the 1,455 seconds** — two thirds of the tail — and
its content is:

> counts from adjacency **key bytes**, without decoding a record.

The keys already encode what the count needs. The record decode was pure waste:
a full property tuple materialised — fat text and embedding properties included
— to establish that a relationship exists.

This is the same class as the defect in
[relationship-create parity](./rel-create-parity.md): **13.68 full node
materialisations per relationship, where the operation needs zero.** Both are
work done to answer a question the key already answers.

It is worth internalising as a pattern rather than two incidents. In an engine
whose keys are memcomparable and carry structure, *the cheapest read is often
the one that never decodes the value.*

## What actually happened

The plan's ordering was sound, and it was still incomplete in the way these
plans usually are.

The rungs landed and the tail came down. But **the single largest planner win
was not on the list**: first-call cardinality estimation, invisible to every
profile that produced this plan because its cost hid in *planning*, split across
events no counter aggregated.

The plan could only rank what the instrument attributed. That is not a criticism
of the plan — it is the reason the instrument-integrity rules elsewhere in this
codebase exist, and the reason the eventual fix came from adding a counter
rather than from re-reading the list.

## Next

- [Execution engine evaluation](./execution-engine-evaluation.md) — the
  attribution this orders.
- [The planner](../architecture/planner.md) — the count fast paths today.
- [Measurements](../measurements/index.md) — where the tail ended up.
