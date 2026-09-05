# Counters and observability

Engram's observability is **process-global atomic counters printed to stderr**,
plus a deterministic trace used by tests and the simulation.

There is **no Prometheus exporter, no OpenTelemetry, no `tracing` integration
and no health endpoint.** See [Roadmap](../roadmap.md).

## The two stderr lines

Printed every **30 seconds**, and **only when a value moves**. A quiet system
is silent, so a line appearing means something changed.

### Counters

```text
[engram-server] counters: txn_conflicts=… autocommit_reruns=… won@1=… …
```

Thirty named values. Grouped by what they tell you:

#### Contention

| counter | meaning |
|---|---|
| `txn_conflicts` | transactions that failed validation |
| `autocommit_reruns` | autocommit statements re-run after a conflict |
| `won@1`, `won@2`, `won@3-4`, `won@5-8`, `won@9+` | the attempt distribution — healthy is weighted to `won@1` |
| `max_attempts` | the worst case seen |
| `escalations` | contenders moved onto FIFO locks instead of racing |
| `escalated_losses` | losses after escalation |

A six-way **conflict-class** breakdown is tracked separately, classifying each
conflicting key by its row family. That is what established guard rows as ~48%
of the re-runs on one shape — and therefore what justified the guard exemption.

#### Durability

| counter | meaning |
|---|---|
| `fsyncs` | compare against your write rate to see group commit working: with one client it is one per write, with eight it should be far fewer |

#### Derived structures

| counter | meaning |
|---|---|
| `adj_built` | adjacency tables built **from scratch** — expensive |
| `adj_repaired` | tables caught up incrementally — cheap |
| `derived_refreshed`, `refresh_runs` | the maintenance thread doing its job |
| `stale_served` | single-node reads answered from a table stale as a whole but current for the node asked about |
| `stale_declined` | reads that fell back to a direct span walk because repair would have cost more than a reader should pay |

**`stale_served` versus `stale_declined` is the most useful pair on the line.**
The source says why: the same ops/s can mean the tables are serving *or* that
every read is walking the store, and only this pair distinguishes them.
Throughput cannot.

#### Storage reads

| counter | meaning |
|---|---|
| `span_excl` | span reads that **excluded writers** for their duration |
| `span_free` | span reads that ran latch-free |
| `span_rows_excl` | rows read under latches |

`span_excl` should be near zero on a read-only workload (the tail drains at the
seal) and near zero on a write-only one (no span reads). It dominates in a
*mix*.

#### Indexes and memberships

| counter | meaning |
|---|---|
| `idx_builds` | a **full** range-index build — O(group) |
| `idx_catchups` | a catch-up, which clones and re-sorts the added set |
| `idx_folds` | a fold — O(base) |
| `mem_caught`, `mem_built`, `mem_folds` | the membership equivalents |
| `mem_flat`, `mem_flat_rows` | materialised membership views |
| `mem_probes` | candidate peers a hop's label filter tested |
| `mem_bitmaps` | presence bitmaps built |
| `seed_scan_rows` | rows scanned to seed a pattern |

Divide these by the profile's read count and you get a per-read frequency;
frequency times a known cost is what turns a correlation into a mechanism.

### Memory

```text
[engram-server] memory: cache 3812/4096 MB, adjacency 4894 MB in 318 table(s),
memberships 210 MB in 457 label(s), range indexes 88 MB in 12 index(es),
property columns 512 MB in 40 column(s); rss 11204 MB, unattributed 1688 MB
```

Printed only when a term moves by more than **64 MiB**.

**`unattributed` is the number to watch.** It is process RSS minus everything
the engine can account for. Growth there means memory going somewhere the
accounting does not cover.

RSS is read from `/proc/self/statm` — **Linux only**. On other platforms the
RSS and unattributed terms are unavailable.

## Per-statement tracing

| what | how |
|---|---|
| every statement as received | `ENGRAM_TRACE_STATEMENTS=1` |
| per-statement counters, largest first | `ENGRAM_TRACE_COUNTERS=1` |
| the plan, including fold marks | `ENGRAM_TRACE_PLAN=1` |
| **one** statement only | prefix it with `/* engram:trace */` |

The marker is the one to reach for:

```cypher
/* engram:trace */ MATCH (p:Person)-[:KNOWS]->(f) RETURN count(f)
```

A statement whose execution grows RSS by more than **32 MiB** reports itself by
name regardless. That threshold was chosen deliberately: far above what any
point read or hop costs, far below the transient that had been killing a
process. It was 256 MiB, and at that level only ten statements in a
corpus of hundreds crossed the bar — the climb was everything below it, and the
log could not rank them.

## The determinism trace

A separate instrument, used by tests and the simulation rather than by a
running server.

`engram_observe::with_trace` captures an ordered event trace with a digest.
`cargo xtask determinism` runs one seed in **two processes** and requires the
digests to match:

```text
[PASS] determinism  seed 424242, two processes, digest 5483cf2ea8e8fc46
```

Two processes rather than two runs in one, because a process reuses its
allocator and address-space layout, and a determinism bug hiding behind either
is exactly the kind that ships.

## D3 registration

Every subsystem **declares** its crash points, `sometimes!` events, counters
and gate/canary pairs before it can fire them. Twelve register: `blob`, `bolt`,
`crypto`, `cypher`, `exec-operators`, `graph`, `key-codec`, `commit-log`,
`objstore`, `executor`, `replica`, `store`.

Declaration is what makes absence measurable. If events only came into being by
firing, "never fired" and "does not exist" would be the same observation — and
the simulation sweep's coverage floor, which **fails** when a declared event
never fires, would be unenforceable.

See [The three decisions](../architecture/three-decisions.md).

## What to alert on

Given there is no metrics export, this is what you would scrape if you built
one:

| signal | why |
|---|---|
| `unattributed` growth | unaccounted memory |
| `won@9+` / `max_attempts` rising | contention getting worse |
| `adj_built` rising steadily | tables being rebuilt rather than repaired |
| `stale_declined` >> `stale_served` | reads are walking the store |
| `fsyncs` ≈ writes under concurrency | group commit not batching |
| the process exiting | there is no failover |

## Next

- [Operations](../using/operations.md) — reading these day to day.
- [Tuning guide](./tuning.md) — acting on them.
- [Deterministic simulation](../development/simulation.md) — the coverage floor.
