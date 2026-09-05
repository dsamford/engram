# Neo4j head to head

> **2026-08-28.** A same-host, sequential comparison against Neo4j 5.26 on an
> SNB-schema corpus.
>
> **Provenance:** the corpus is `snbgen` **synthetic** data on an SNB-like
> schema — **not** official LDBC Datagen output. Nothing here is presentable as
> an LDBC benchmark result.

## Method

| | |
|---|---|
| **Corpus** | synthetic SNB-schema, seed 1, 2,000 persons — **100,099 nodes / 447,093 relationships**, verified on both engines after loading |
| **Host** | a dedicated-CPU Linux server, 16 vCPU / 64 GiB, measured idle at 9% |
| **Containment** | each database in a container limited to **6 CPU / 40 GiB**, on the same node, run **sequentially** |
| **Client** | in-container, 20 s per level, fixed seed |

**Sequentially, not concurrently.** Two benchmarks sharing a node measure each
other. That is the kind of methodological error that produces a number nobody
can reproduce and everybody quotes.

## The answers agree — checked first

**63 of 63 statements returned identical row counts on both engines**, run
against freshly reloaded, pristine corpora on both sides.

This comes first for a reason the document states plainly:

> A speed comparison between engines that disagree is **void**. "Faster" is not
> a property a query has on its own; it is a property of a query that returns
> the right rows, and an engine returning fewer of them is not faster.

An engine can always be made faster by returning less. Establishing agreement
first is what makes every subsequent number mean something.

## What it found

The results this run produced are superseded — the campaign that followed
reversed several of them, and the current standing is on
[How Engram is measured](./index.md).

What is **not** superseded is the shape of the finding, which held: Engram was
ahead at the floor and behind on complex analytical queries, and every
analytical loss had the same query shape. One shape, several losses — a
mechanism rather than general slowness.

That framing is what
[the engine redesign](../history/engine-redesign.md) was built on, and what
eventually reversed the standing.

## Why the containment matters

A container limited to 6 CPU on a 16 vCPU host, with the host measured idle,
gives:

- a **fixed** CPU budget both engines get equally;
- headroom so neither engine is competing with the measurement client;
- a number that is about the engines rather than about the scheduler.

The idle measurement — 9% — is part of the record because a host that was busy
would invalidate the run, and stating it lets a reader judge that rather than
assume it.

## Next

- [How Engram is measured](./index.md) — the current standing.
- [LDBC SNB stress](./ldbc-snb-stress.md) — the mixed read/write suite.
- [Engine redesign](../history/engine-redesign.md) — what this motivated.
