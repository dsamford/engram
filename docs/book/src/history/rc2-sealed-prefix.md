# RC2 — the sealed prefix

> **Historical.** Measured on LDBC SNB SF1, identical store copies, six workers,
> with only the server binary differing.

## The change

OCC validation asks one question per key: **did anyone commit to this key after
my snapshot?**

A sealed segment can answer that from its **footer**. `max_commit_ts` is the
greatest commit timestamp any version in it carries, and segments are sealed in
timestamp order — so a newest-first walk can **stop at the first segment sealed
at or below the snapshot**. Every older one is older still.

Three properties make it safe:

- **No format change.** The footer already carried the field.
- **The bound is exact.** It never skips a segment that could hold a conflict.
- **Behaviour is unchanged.** Validation refuses exactly the transactions it
  refused before.

## What it was doing instead

Validation walked **every sealed segment** for **every** read-set and write-set
key, **under the global commit latch**.

And a freshly minted id is never rejected early by the sparse index — it sorts
above every node key but below the segment's edge region, so the index cannot
exclude it.

At SF1 that was ~100 segments per key, **growing without bound**, because the
paged path never compacts.

## The shape of the defect

This is the instructive part. The cost was:

- **per key**, so it scaled with the transaction's footprint;
- **per segment**, so it grew with uptime;
- **under the one latch that cannot be parallelised**, so it converted directly
  into a scaling ceiling.

Three multipliers, each individually reasonable. A bound already stored in the
footer removed all three.

## The follow-on defect

Worth recording, because the fix introduced one.

The bound RC2 consults is a segment's `max_commit_ts`. On the paged path that is
a stored footer field; on the **resident** path it was **recomputed from
scratch** — a flat map over every version of every entry, plus every column
block.

Two things made that worse than it sounds:

1. **The early exit was evaluated *after* the call**, so the newest segment was
   interrogated unconditionally — on every validated key, on every commit,
   including the happy case the bound exists to make cheap.
2. It ran **inside the global commit latch**. At the default seal threshold, and
   about nine validated keys for a `CREATE (a)-[:R]->(b)`, that is on the order
   of 590,000 iterations per commit at the one serialisation point that cannot
   be parallelised.

A performance fix whose fast path is more expensive than the slow path it
replaced is not rare. It is what happens when a bound is cheap on one
representation and not on the other, and only one of them is measured.

## Next

- [The storage engine](../architecture/storage-engine.md) — segments and their
  footers.
- [RC1 — guard-row exemption](./rc1-guard-exemption.md) — the other OCC fix.
- [Write path, phase 0](./write-path-phase0.md) — where the follow-on was caught.
