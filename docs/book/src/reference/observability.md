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
[engram-server] t=1757430000123 counters: txn_conflicts=… autocommit_reruns=… won@1=… …
```

`t=` is unix milliseconds. Every maintenance line the server prints carries it,
so two lines can be dated against a pod's other logs rather than against their
arrival time — a log parser written from this page must expect the field
between the prefix and the label.

Thirty-one named values. Grouped by what they tell you:

#### Contention

| counter | meaning |
|---|---|
| `txn_conflicts` | transactions that failed validation |
| `autocommit_reruns` | autocommit statements re-run after a conflict |
| `won@1`, `won@2`, `won@3-4`, `won@5-8`, `won@9+` | the attempt distribution — healthy is weighted to `won@1` |
| `max_attempts` | the worst case seen |
| `escalations` | contenders moved onto FIFO locks instead of racing |
| `escalated_losses` | losses after escalation |

A six-way **conflict-class** breakdown is recorded on every conflict,
classifying the conflicting key by its row family. It is not on this line, not
on the memory line, and not reachable through any trace: today only the tests
read it, so reading it on a running server means adding it to the line above
first. It is what established guard rows as a large share of the re-runs on
one shape — and therefore what justified the guard exemption.

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
[engram-server] t=1757430000123 memory: cache 3812/4096 MB, adjacency 4894 MB in 318 table(s),
memberships 210 MB in 457 label(s), range indexes 88 MB in 12 index(es),
property columns 512 MB in 40 column(s); rss 11204 MB, unattributed 1688 MB
```

Printed only when a term moves by more than **64 MiB**.

**`unattributed` is the number to watch.** It is process RSS minus everything
the engine can account for. Growth there means memory going somewhere the
accounting does not cover.

RSS is read from `/proc/self/statm` — **Linux only**. On other platforms both
the `rss` and `unattributed` terms print as `0`, which means the reading is
missing and not that the process is using nothing. The engine-attributed terms
beside them are still real.

## Per-statement tracing

| what | how |
|---|---|
| every statement as received | `ENGRAM_TRACE_STATEMENTS=1` |
| per-statement counters, largest first | `ENGRAM_TRACE_COUNTERS=1` |
| the plan, including fold marks | `ENGRAM_TRACE_PLAN=1` |
| **one** statement only | prefix it with `/* engram:trace */`, on a server started with `ENGRAM_TRACE_MARKER=1` |

The marker is the one to reach for — once the operator has permitted it. It is
off by default, because the client chooses it and the server pays: a traced
statement can cost an order of magnitude more, and its text lands in this log.
Unpermitted, the marker is an ordinary comment, counted in
`engram_bolt::counters::TRACE_MARKER_IGNORED` and noted once in the log:

```cypher
/* engram:trace */ MATCH (p:Person)-[:KNOWS]->(f) RETURN count(f)
```

A statement whose execution grows RSS by more than **32 MiB** reports itself by
name regardless. That threshold was chosen deliberately: far above what any
point read or hop costs, far below the transient that had been killing a
process. It was 256 MiB, and at that level only ten statements in a
corpus of hundreds crossed the bar — the climb was everything below it, and the
log could not rank them.

On a paged server the line also names the block cache's share of that growth,
because a cold statement's resident-set growth there is mostly the cache
filling and a report without that term reads as a leak:

```text
[bolt] t=1757430000123 statement grew rss by 311 MB (cache +290 MB) (9448 -> 9759 MB) on conn 513: MATCH (n) RETURN n
```

The `(cache +…)` term is present only when a cache probe is registered, which
the binary does when it serves `--paged-dir` and not otherwise; the `t=` stamp
is the same clock as the two lines above. Subtract the cache share before
judging the statement.

## Counters the periodic line does not carry

The periodic counters line is a **fixed selection** of thirty-one, accumulated
across every session. `ENGRAM_TRACE_COUNTERS` and the `/* engram:trace */`
marker are a different instrument: they dump every counter *that statement*
recorded, largest first. Two families are reachable only that way, and both
answer questions nothing else on this page can.

### Algorithm counters

| counter | what it distinguishes |
|---|---|
| `algo.refused for all-pairs work` | a projection refused against the `V x E` ceiling, rather than one that ran |
| `algo.fixpoint iterations` vs `algo.fixpoint hit the iteration cap` | converged, or stopped at `maxIterations` |
| `algo.fixpoint parallel` vs `algo.fixpoint below the parallel floor` | which lane ran — the floor swaps in a serial executor rather than narrowing the split |
| `algo.concurrency narrowed the executor` | a `concurrency` key capped the installed width (it can never widen it) |
| `algo.result published` / `served from the cache` / `evicted for budget` | a `mutate` result's whole life |
| `algo.graph built`, `algo.runs`, and one per algorithm (`algo.pagerank runs`, `algo.wcc runs`, …) | how many projections and runs a statement cost |
| `algo.weights defaulted`, `algo.sssp refused a negative weight`, `algo.lpa oscillated` | the per-algorithm caveats |

A refused projection and a converged one are different facts, and so are a
result recomputed and one served from the cache. See
[Graph algorithms](../architecture/graph-algorithms.md) for the refusal
ceilings and the cache's rules — that page explains the semantics and names
none of these counters.

### Text index counters

The trigram, term and full-text families are in the same position. The
periodic line's `idx_*` counters are the **range** index's, so none of this is
on it.

| counter | what it distinguishes |
|---|---|
| `graph.trigram index built` vs `graph.trigram index caught up` vs `graph.trigram index still current` | whether the first query paid for a build, a catch-up, or nothing |
| `graph.trigram index rebuilt for a label change` | a membership move, which cannot be caught up from the property log and forces a rebuild |
| `trigram.overlay folded` | the O(base) fold landing on a reader's thread |
| `interp.seed sought a trigram index` vs `interp.trigram probe declined` | the seed used the index, or declined it because the candidate estimate was over the cap the label's size sets |
| `store.term index built` / `caught up` / `queries` | the BM25 term index's own lifecycle |
| `graph.fulltext answered from the index` vs `graph.fulltext fell back to a scan` | which arm answered a full-text query |
| `cypher.regex evaluations` | how many rows the regex was actually run against, after any seek |

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
