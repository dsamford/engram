# Concurrency direction

> **Historical — written 2026-08-29**, on what serialises writes and what true
> scale would need. Two of its three workstreams were executed; the third was
> not, and later was.
>
> For the model as it stands, see
> [Concurrency and the worker model](../architecture/concurrency.md).

## The question

> Is there a reason to change the single-threaded engine model? Does it not
> give clean CPU balance and no contention?

The answer required naming what was actually running, and it was neither of the
two things people assumed.

The D2 model — one thread per shard, no shared mutable state — **would** give
clean balance *if the data were sharded*. It was not: the partition was
hardcoded, and a Cypher traversal has no notion of crossing a shard.

So what ran was neither pure D2 nor a finished multi-threaded engine: **N worker
threads sharing one graph over one store, where everything around the store
parallelises and the store itself does not.**

## Finding 1 — the store's write path had *negative* scaling

A probe of N threads doing `put` on **disjoint** keys — in-memory store, fsync
deferred, no disk, no graph, no Bolt. Throughput relative to one thread:

| variant | 1 | 2 | 4 | 6 | 8 threads |
|---|---|---|---|---|---|
| `put` (latch + chain hash + log buffer + memtable) | 1.00 | 0.86 | 0.55 | 0.57 | **0.51** |
| `put_unlogged` (latch + memtable only) | 1.00 | 0.61 | 0.49 | 0.44 | 0.49 |
| a transaction of 16 puts (OCC validate + publish) | 1.00 | 0.86 | 0.66 | 0.68 | 0.65 |

**Eight threads did half the work of one.**

The second row is what makes the diagnosis precise. `put_unlogged` skips the
chain hash entirely and scales *no better*, so **the BLAKE3 hash was not the
cost — the latch was.**

Every write held one hot lock across timestamp allocation, log append and the
memtable insert, and every reader of a non-empty tail took the same lock.

That is the "thread-**safety**, not thread-**scalability**" interim latch the D2
revision had flagged, now quantified.

### Why disjoint keys matter

The probe deliberately used keys that do not conflict. If throughput collapses
with *no logical contention*, the cause is structural rather than a property of
the workload — which rules out a large class of theories before they are
proposed.

## What was executed

Two workstreams landed:

**Sharding the tail.** The memtable became 64 shards, each behind its own latch,
so writers to different keys stop contending. This is the tail described in
[the storage engine](../architecture/storage-engine.md).

**Narrowing the serialised section.** The commit-log hash rule was split into
two hashes so the payload digest is computed by the writer *outside* the log
latch, leaving a 40-byte hash in the serialised section. See
[the commit log](../architecture/commit-log.md).

## What was not

**Morsel-parallel execution.** The document concluded it was not the constraint
at the corpus scale then in use, and it was explicitly not implemented.

**That conclusion was later overturned by measurement.** Morsel-parallel
`expand` and count fold now exist behind `ENGRAM_QUERY_PARALLELISM`, and at
width 6 measured q2 5.6×, q9 5.8×, q6 5.0×, q5 4.0×, q8 3.8×, q4 1.8×.

The document was right about its own scale and wrong as a general conclusion,
which is the ordinary way a scoped finding goes wrong. It is kept because the
*probe* — disjoint keys, layers stripped one at a time — is the transferable
part, not the verdict.

## The correction worth carrying

The document's own follow-up contains one of the more useful notes in this part
of the book:

> **The determinism digest does not gate this work.**
> `cargo xtask determinism` runs a `SimRuntime`-only workload that never touches
> the store, graph or Bolt, and the gate compares two fresh runs to *each
> other*, not to a pinned constant. Every "digest unchanged" acceptance clause
> in the raw designs was **vacuous**.

The real instruments for store behaviour are the simulation sweep, A/B
differential tests with fired-counter canaries, and seeded one-worker-versus-six
equivalence runs.

A gate cited as evidence for something it does not exercise is a gate being used
as decoration — and noticing that about your own acceptance criteria is harder
than noticing it about someone else's.

## Next

- [Concurrency and the worker model](../architecture/concurrency.md) — the model
  now.
- [Write concurrency ceiling](./write-concurrency-ceiling.md) — the deeper probe.
- [Write path, phase 0](./write-path-phase0.md) — what landed.
