# Write concurrency ceiling

> **Historical.** An attribution of why the write path did not scale with
> clients, and whether the cause was architectural.
>
> For the model as it stands, see
> [Concurrency and the worker model](../architecture/concurrency.md).

## The question

At LDBC SNB SF1, the comparison engine gained **4.2–4.9×** from one to eight
clients on every write profile. Engram gained **1.2–1.7×**, and two profiles
went *backwards* — `rel-hub` at 0.76×, `delete-churn` at 0.35×.

Yet at **one** client, Engram was **2.0–3.1× faster** on every one of those same
profiles.

So: was the failure to scale **architectural** — the shared graph and the single
commit log — or incremental?

## The answer: incremental, and the evidence is a differential

Turning serialisable autocommit off took `rel-hub` from a **1.63× curve to
5.13×** — past the comparison engine's measured 4.7× — on the same host, corpus
and statements.

The force of that result is what it does *not* change. The OCC-off arm still
goes through every structure the architectural hypothesis blames:

- one shared graph,
- one commit log behind one global lock,
- one id-allocator mutex,
- one global visibility barrier,
- the write fence.

And it pays **more** global-latch acquisitions per relationship than the shipping
configuration — six logged puts against one batched commit — and **still scales
5.13×**.

So the shared-graph, single-log design was **not** the binding constraint at that
CPU count and rate. Something inside OCC was.

## Why this is the right shape of experiment

The architectural hypothesis is the intuitive one, and it is nearly unfalsifiable
by ordinary profiling: every write really does pass through those structures, so
a profile will always show time there.

The differential falsifies it directly. Keep every blamed structure, remove one
mechanism, and watch the curve change. If the architecture were the ceiling, it
could not.

That is the same move as the disjoint-key probe in
[concurrency direction](./concurrency-direction.md), approached from the other
side.

## What it pointed at

Two mechanisms inside OCC validation, each of which became its own measured fix:

- **Guard rows** making two relationship writes to one node conflict over
  nothing — [RC1](./rc1-guard-exemption.md).
- **Validation walking every sealed segment** for every key —
  [RC2](./rc2-sealed-prefix.md).

Neither is architectural. Both were removed without changing the shape of the
engine.

## Next

- [RC1 — guard-row exemption](./rc1-guard-exemption.md)
- [RC2 — the sealed prefix](./rc2-sealed-prefix.md)
- [Transactions and isolation](../using/transactions.md) — what OCC does now.
