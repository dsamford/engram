# The port benchmark

> **2026-08-21.** A large real-world corpus, ported into Engram, with all of an
> application's statements run against both engines on the same hardware class.
>
> The corpus and the statements come from a private application and are not
> published. What is published is the **method** and the **classification** —
> which is the part that transfers.

## Method

| | |
|---|---|
| **Corpus** | ~1.79M nodes / 5.29M relationships, exported at the driver level |
| **Statements** | 235, extracted from a real application rather than written for a benchmark |
| **Comparison** | server-side timings on one side, in-process wall clock on the other, same hardware class |
| **The run** | one uninterrupted pass |

**Statements extracted from an application** is the distinguishing feature. A
benchmark suite is written by someone who knows what the engine does well. A
corpus of statements a real application actually issues is not, and it is the
only kind that finds the shapes nobody anticipated.

## Correctness is the headline

Of 235 statements: **196 comparable, 180 verified matching, 14 diverged — every
divergence classified, and none against the engine.**

Three things about that sentence.

**"Comparable" is smaller than the total.** 39 statements could not be compared
at all, and saying so is what keeps the denominator honest. A pass rate over the
comparable subset, reported as a pass rate over the corpus, is the standard way
this number gets inflated.

**"Every divergence classified."** Fourteen disagreements, each assigned a
cause. An unclassified divergence is an unknown, and a benchmark with unknowns
in it is not evidence.

**"None against the engine"** is a claim that only means something *because* all
fourteen were classified. Without the classification it would be an assertion.

## The finding

**The floor was excellent and the tail was everything**: a median of 4.27 ms
with a third of statements sub-millisecond, and 56 statements over one second
totalling **1,455 s against the comparison engine's 21.9 s**.

That framing drove everything that followed. A good median and a catastrophic
tail is not a slow engine — it is a small number of mechanisms behaving
pathologically, and they can be named individually.

They were:
[Execution engine evaluation](../history/execution-engine-evaluation.md)
attributed all 56 to six mechanisms, and
[the remediation plan](../history/remediation-plan.md) ordered the response by
measured impact.

## Why this one is not quoted for performance

The numbers here are superseded, and the corpus is private, so this page
contributes no figures to the [current standing](./index.md).

It is kept for the method:

- statements taken from an application, not written for the engine;
- correctness established before speed;
- a comparable subset stated honestly;
- every divergence classified before any of it counted as evidence.

## Next

- [Execution engine evaluation](../history/execution-engine-evaluation.md) — the
  attribution.
- [Tail remediation plan](../history/remediation-plan.md) — the response.
- [How Engram is measured](./index.md) — the current standing.
