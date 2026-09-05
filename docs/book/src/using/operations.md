# Operations

Running an Engram server day to day: what it tells you, what to watch, and what
to do when something looks wrong.

> Before anything else: there is no authentication and no TLS, so operating
> Engram safely is mostly about the boundary you put around it. See
> [Security posture](./security.md).

## Starting

```sh
engram-server 127.0.0.1:7687 --data-dir ./data
```

Three lines on startup, each worth reading:

```text
[engram-server] listening on bolt://127.0.0.1:7687
[engram-server] durable: ./data/engram.wal
[engram-server] warmed in 340 ms: 1482301 nodes, 5290114 out-edges, 5290114 in-edges,
                318 adjacency table(s) holding 4894 MB in 7640 MB allocated
```

The third is the one to keep an eye on. Engram builds its
[derived structures](../architecture/derived-structures.md) **before** the
listener accepts, so your first query does not pay for the whole corpus.
Deferring that was measured at 5.85 s on a 1.48M-node graph, with the first ten
seconds of a run producing almost nothing.

A server that starts fast and then stalls its first user is worse than one that
takes a few more seconds to say it is ready. If startup is slower than you
want, that number tells you what it is spending.

## What it prints while running

Two lines, every 30 seconds, and **only when something moved**. An unchanging
system is silent.

### Counters

```text
[engram-server] counters: txn_conflicts=… autocommit_reruns=… won@1=… won@2=…
won@3-4=… won@5-8=… won@9+=… max_attempts=… escalations=… escalated_losses=…
fsyncs=… adj_built=… adj_repaired=… derived_refreshed=… refresh_runs=…
span_excl=… span_free=… span_rows_excl=… stale_served=… stale_declined=…
idx_builds=… idx_catchups=… idx_folds=… mem_caught=… mem_built=… mem_folds=…
mem_flat=… mem_flat_rows=… mem_probes=… mem_bitmaps=… seed_scan_rows=…
```

The ones worth knowing:

| counter | what a high value means |
|---|---|
| `txn_conflicts`, `autocommit_reruns` | contention; statements are re-running |
| `won@1` vs `won@9+` | the attempt distribution — healthy is weighted to `won@1` |
| `escalations`, `escalated_losses` | contenders were moved onto FIFO locks instead of racing |
| `fsyncs` | durability cost; compare against your write rate to see group commit working |
| `adj_built` vs `adj_repaired` | rebuilding adjacency from scratch versus catching it up incrementally — repairs are cheap, builds are not |
| `stale_served` vs `stale_declined` | whether readers are being served from derived tables or falling back to walking the store |
| `span_excl`, `span_free` | span reads that excluded writers versus those that ran latch-free |

That `stale_served` / `stale_declined` pair is the most useful diagnostic on
the line, and the source says why: the same ops/s can mean the tables are
serving *or* that every read is walking the store, and only this pair
distinguishes them. Throughput alone cannot.

### Memory

```text
[engram-server] memory: cache 3812/4096 MB, adjacency 4894 MB in 318 table(s),
memberships 210 MB in 457 label(s), range indexes 88 MB in 12 index(es),
property columns 512 MB in 40 column(s); rss 11204 MB, unattributed 1688 MB
```

Printed only when a term moves by more than 64 MiB.

**`unattributed` is the number to watch.** It is process RSS minus everything
the engine can account for. A large or growing unattributed figure means memory
is going somewhere the accounting does not cover — allocator fragmentation, or
a leak worth investigating.

Per-statement RSS growth above 32 MiB is separately reported by statement, so a
single query that inflates the heap names itself.

## Tracing a specific problem

| what you want | how |
|---|---|
| see every statement as it arrives | `ENGRAM_TRACE_STATEMENTS=1` |
| dump per-statement counters, biggest first | `ENGRAM_TRACE_COUNTERS=1` |
| see the plan a statement got | `ENGRAM_TRACE_PLAN=1` |
| trace **one** statement only | prefix it with `/* engram:trace */` |

That last one is the good one — it traces a single query without turning the
firehose on for a whole server.

## The maintenance thread

One background thread does the work that must not happen on a query's critical
path:

- **compaction**, when segments reach `--compact-after` (default 8) or the
  tombstone ratio crosses its threshold;
- **spilling** sealed segments to disk in paged mode;
- **log truncation**, releasing the in-memory commit log at a seal;
- **derived-structure refresh**, so readers find current structures rather than
  one unlucky reader paying to catch everything up.

It ticks every `--maintenance-tick-secs` (default 5) and is additionally asked
after `--refresh-after-writes` commit stamps (default 8192, roughly 2,700 Bolt
write statements).

That refresh is why a write-only burst followed by a read is *usually* fine.
`--no-derived-refresh` turns it off and makes the next reader pay — it is the
A/B arm for the write stall the refresh can itself cause.

## Checkpointing

Paged mode only:

```cypher
CALL engram.checkpoint() YIELD spilled, segments, resident, tail
RETURN spilled, segments, resident, tail
```

This is what a drain-before-shutdown hook calls. In paged mode the unsealed
tail is volatile, so a clean shutdown means checkpointing first — see
[Durability and recovery](./durability.md).

Refused outside paged mode rather than silently doing nothing.

## Restarting

1. Stop the process. In paged mode, checkpoint first.
2. Start it with the **same** `--data-dir` or `--paged-dir`.
3. Read the third startup line to confirm what came back.

**A stale `LOCK` after a hard kill is expected.** The message names the pid
that held it:

```text
[engram-server] the data directory is locked by another process (pid 78972).
If that process is NOT running, the lock is stale — remove ./data/LOCK and
start again.
```

Confirm the pid is gone before removing it. Two servers on one store interleave
their log records and leave a hash chain no recovery can verify.

## Sizing

| dimension | what sets it |
|---|---|
| Memory, paged mode | `--paged-cache-mb` plus the derived structures — the memory line breaks this down |
| Memory, resident mode | the corpus, plus derived structures |
| Threads | `2 × connections + --workers + 2` |
| Connections | `--max-connections`, default 512 — each costs two OS threads |

The adjacency figure in the memory line is often the largest single term, and
it scales with the number of `(relationship type, direction)` pairs actually
traversed, not with the corpus alone.

## When something looks wrong

| symptom | first thing to check |
|---|---|
| first query after restart is slow | the `warmed in` line — is warming being skipped? |
| writes stall periodically | `--refresh-after-writes`, and the `derived_refreshed` counter |
| conflicts are high | `won@N` distribution; consider whether escalation is on |
| memory grows without bound | the `unattributed` term; `--keep-full-log` retains the whole commit log |
| a query is refused | the row budget — see [Result paging](./result-paging.md) |
| connections drop after 5 minutes | `--read-timeout-secs`; a client waiting on a long query is quiet |

The [Tuning guide](../reference/tuning.md) is organised by symptom and goes
further on each.

## What does not exist yet

- **No Prometheus or OpenTelemetry export.** Observability is the stderr lines
  above plus the trace instruments.
- **No health endpoint.** Liveness is "the port accepts a connection"; readiness
  is "a query answers".
- **No backup tooling.** Stop the server and copy the directory.
- **No audit log.** Nothing records who did what.

See [Roadmap](../roadmap.md).

## Next

- [Tuning guide](../reference/tuning.md) — symptom-first.
- [Counters and observability](../reference/observability.md) — every counter.
- [Durability and recovery](./durability.md) — what survives a crash.
