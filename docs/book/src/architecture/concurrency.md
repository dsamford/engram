# Concurrency and the worker model

Engram is a **multi-threaded server over a shared MVCC store**, in which the
engine crates themselves never spawn a thread.

That distinction is the whole design, and it is easy to state wrongly. The rule
is not "single-threaded" — `--workers N` runs N engine threads and a statement
can split across morsels. The rule is that **every thread in the process is
created in one file.**

## Where the threads are

All in `engram-server`, the only crate carrying
`#![allow(clippy::disallowed_methods, disallowed_types)]`:

| thread | count |
|---|---|
| accept loop | 1 (the main thread) |
| reader | 1 per connection |
| writer | 1 per connection |
| engine worker | `--workers`, default 1 |
| maintenance | 1 |
| counters | 1 |

`std::thread::spawn` is denied workspace-wide by `clippy.toml`. The adapter is
where the real world is allowed to exist, and it is one file deep.

## The D2 revision

D2 was originally *"one shard, one thread, cooperative tasks; the store is
`!Sync` on purpose."*

It was **revised on 2026-08-25**. The store is now `Send + Sync`, because
morsel-driven parallel execution and MVCC-OCC require it to cross threads.

The original argument for one thread was simulability — a shared-pool-plus-locks
design cannot be simulated, because the interleavings belong to the OS scheduler.
Losing that meant the integrity had to come from somewhere stronger, and the
source names the replacements: **result-determinism, interleaving search
(Loom/shuttle), and a serializability checker.**

## Connection pinning

A connection pins to worker `id % workers` and stays there. Consequences:

- A worker's session map is **its own** — no cross-worker map, no lock around it.
- One connection's statements are serialised with respect to each other, which
  is what a client expects.
- Load balance depends on connection distribution, not on work stealing. Eight
  connections across two workers is four each, however unequal their work.

## Isolation between workers

Workers share one store. What keeps them honest:

| mechanism | protects |
|---|---|
| **MVCC snapshots** | readers never block writers, writers never block readers |
| **OCC validation** at commit | both read set and write set, against the commit window |
| **Per-entity write latches** | 1,024 stripes — record-level lost updates |
| **Per-entity FIFO locks** | CAS ordering, and escalation targets |
| **64 sharded tail latches** | writers to different keys rarely contend |
| **The write fence** | a derived publish cannot claim a stamp above an in-flight writer |

Store state itself is an `Arc<RwLock<State>>` taken through one place — a
**coarse latch and a deliberate stepping stone**, in the source's own words.
Refining it so a reader on a snapshot never blocks a writer is later work; the
immutable sealed segments never need locking, only the mutable tail, the
timestamp counter, the pins and the locks do.

## Conflict escalation

Under sustained contention on one key, optimistic re-running is wasteful — every
contender burns a full execution to lose.

Escalation moves those contenders onto **FIFO entity locks** so they queue
instead of racing. On by default; `ENGRAM_CONFLICT_ESCALATION=0` turns it off.

The counters distinguish the outcomes: `escalations`, `escalated_losses`, and
the `won@N` attempt distribution.

## Morsel parallelism

Parallelism *inside* a statement enters through one trait:

```rust
pub trait ScopedExec: Send + Sync {
    fn width(&self) -> usize;
    fn for_each(&self, n: usize, f: &(dyn Fn(usize) + Sync));
}
```

Three implementors, and the middle one is the point:

| implementor | what runs |
|---|---|
| **absent** — the default, and always in the simulation lane | operators take their serial paths; one lever check on the hot path |
| `SerialExec` | width 1, inline — the *parallel machinery* (split, slot collection, ordered merge) run **deterministically** |
| the server's thread-scope pool | real OS threads, work-stealing off an atomic cursor, behind `ENGRAM_QUERY_PARALLELISM` |

Because the engine **asks** for parallelism rather than owning it, the morsel
machinery can be exercised single-threaded through the *same code path* the
parallel pool uses. The deterministic simulation therefore covers parallel
execution logic without threads. A database that owns its thread pool cannot
make that separation.

### The merge discipline

Partials are collected per morsel and concatenated **in morsel order**, so a
parallel run reproduces the serial output byte-identically — proven per operator
by an A/B differential with a fired-counter canary.

The discipline is the *operator's* obligation, not the trait's. The trait
promises only that every index in `[0, n)` has run before it returns.

### Admission

Five gates, each load-bearing:

1. the lever is on;
2. an executor is installed;
3. **no active transaction on this thread**;
4. enough driving rows to beat the split's overhead (256);
5. no fold weights on the driving rows.

Gate 3 is the sharp one: the read-your-writes overlays and the OCC read set are
**thread-local**, so a morsel worker would silently read committed state and
record nothing.

### Read semantics, stated rather than assumed

A morsel worker reads exactly as the serial loop it replaces: read-committed per
row against the visible clock. A commit landing mid-statement can be seen by
later rows and not earlier ones **in either mode**. Parallelism changes which
rows are "later", not the anomaly class.

### What it measured

At width 6, N=3 medians, interleaved arms, counts identical across all 54
measurements: **q2 5.6×, q9 5.8×, q6 5.0×, q5 4.0×, q8 3.8×, q4 1.8×**, with
q1/q3/q7 flat.

The win landed where the count fold *drives* — not where the prediction put it.
The prediction was recorded before the run, which is how anyone can tell.

### A defect it surfaced

Enabling it OOM-killed a benchmark host, and the cause was **not** the new code:
the columnar recogniser's parallel expand materialised every worker partial
*before* its row-budget check, where the serial loop refuses incrementally. The
shipping binary exploded under the same cap. Latent since the seam was written.

The fix: workers share a produced-rows account and stop where the serial loop
would.

## What is not concurrent

- **A statement inside an explicit transaction** never parallelises.
- **Sealing** happens inline, under the log latch.
- **The commit log latch** is the serialisation point of every write — which is
  why the payload digest is computed outside it.
- **Compaction** is one at a time.

## Next

- [Request lifecycle](./request-lifecycle.md) — the threads in motion.
- [The three decisions](./three-decisions.md) — D2 and its revision.
- [Transactions and isolation](../using/transactions.md) — what workers
  guarantee each other.
