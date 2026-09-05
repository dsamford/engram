# The SNB proving ground

> **Historical.** Phase 0 of the engine-redesign programme — the benchmark stood
> up **before** the engine work, so every change would be scored on a hard bar
> rather than on the old corpus alone.

## Why it came first

That ordering is the whole point.

A team that optimises against the corpus it already has will optimise for the
corpus it already has. A standard, scalable, comparable benchmark — chosen
before the work — is the difference between "faster than we were" and "fast".

It also made the head-to-head possible: an engine-neutral corpus that can be
loaded into **either** database.

## The pieces

### `snbgen` — a deterministic generator

`snbgen <out_dir> <persons> [seed]` writes an SNB-schema social graph sized by
the person count, with SNB-like power-law fan-outs on `KNOWS`, `HAS_MEMBER`,
`LIKES` and reply threads.

The schema covers Person, Forum, Post and Comment as `:Message`, Tag and
TagClass, Place as City/Country/Continent, and Organisation as
University/Company.

**Determinism is the whole point**, because the corpus is loaded into two
engines for a comparison: a SplitMix64 stream seeded by `seed` drives every
choice, so `(persons, seed)` reproduces the graph **byte for byte**. No wall
clock, no system randomness. Dates are computed from a base epoch, and ids are
dense per label.

Scale: roughly 50 nodes and 224 relationships per person, so the person count is
the knob.

> **It is not official LDBC Datagen output** and must not be presented as LDBC
> results. `datagen2jsonl` exists to convert real Datagen output when that is
> what you have.

### The Interactive query set

The LDBC Interactive workload, each entry carrying its LDBC **choke points** —
the query-processing challenge each query is designed to exercise.

Carrying the choke point with the query is what makes a slow result
*diagnostic*: a query is not merely slow, it is slow at a named thing.

### The harness

The existing port harness, reused: drop the query set in as the corpus's
statements, time each query as a median over N iterations, and record parse and
run errors separately from timings.

**Errors separate from timings** matters. A query that fails fast is not a fast
query, and a battery that averages them together reports an improvement for a
regression.

## Loading both engines the same way

The rule that makes a comparison mean anything:

> Load the **same generated corpus** into each engine, through the **same
> loader**.

Two engines loaded by two different paths differ by more than their engines —
property types, label sets, an index one side has and the other does not. Every
one of those shows up later as a performance difference and gets attributed to
the query engine.

`snbload` therefore speaks Bolt and takes a `--neo4j` flag, so the load path and
the data shape stop being variables.

The comparison instance must also be **clean** — a scratch database, never one
holding real data.

## What it became

This is the foundation the current numbers rest on. The batteries it introduced
— LSQB for analytical queries, `stress` for mixed read/write profiles — are the
ones reporting **19 workloads led** in
[Measurements](../measurements/index.md).

And the caveat it set is still the operative one: **SF1 only**. The document's
own plan called for SF1 → SF100 as the proving ground, and only SF1 has run, so
the margins are not demonstrated to be scale-free.

## Next

- [How Engram is measured](../measurements/index.md) — the rules and the
  standing.
- [Benchmarking](../development/benchmarking.md) — the harness today.
- [LSQB completeness](./lsqb-completeness.md) — making all nine queries finish.
