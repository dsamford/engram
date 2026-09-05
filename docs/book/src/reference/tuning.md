# Tuning guide

Organised by the symptom you have, not by the flag you might want. The
[CLI reference](./cli.md) is the field-by-field list; this page is what to
reach for and why.

## Before you change anything

**Measure, then change one thing.** This project's own house rules are worth
borrowing wholesale:

1. One change per measurement — a run that moves two things attributes neither.
2. Use the lever, not a rebuild — nearly every mechanism has a `--no-*` flag, so
   an A/B has a real control.
3. Write the prediction down *before* the run. A number explained afterwards
   explains anything.
4. Compare in the same window, on the same host, from the same snapshot.

The stderr counters are the instrument. They print every 30 s and only when
something moves — see [Operations](../using/operations.md).

---

## "My first query after a restart is slow"

**Check the third startup line.**

```text
[engram-server] warmed in 340 ms: 1482301 nodes, … 318 adjacency table(s) …
```

Engram builds [derived structures](../architecture/derived-structures.md)
before it accepts connections precisely so this does not happen. If warming is
disabled, the first query pays for the whole corpus — measured at **5.85 s on a
1.48M-node graph**.

Warming is on by default and has no CLI flag to disable it; if you are
embedding the server, check `warm_caches` in
[`ServerConfig`](./server-config.md).

If warming itself is too slow, that number tells you what it is spending, and
in paged mode the derived sidecar lets a restart adopt structures from disk
rather than rebuild them.

---

## "Writes stall periodically"

**The most likely cause is the derived-structure refresh.**

Every adjacency table and membership snapshot catches up from a change log. If
that catch-up happens on a *reader*, one unlucky query pays for every write
since the last read. A write-only burst with no reader between produced a
measured **25-second stall** on the first read afterwards.

The maintenance thread exists to prevent that, and it is on by default:

| flag | default | effect |
|---|---|---|
| `--refresh-after-writes N` | 8192 | commit-clock **stamps** between refreshes; a Bolt write statement is about three |
| `--maintenance-tick-secs N` | 5 | the tick, as a floor under refresh frequency |
| `--refresh-pass-rows N` | 250000 | rows one pass may re-read before deferring the rest |

**The trade is real and measured**: the refresh that removed the 25-second
stall cost a **2–3× write tax** on a write-only burst. If your workload is
write-only and nothing reads until later, `--no-derived-refresh` moves the cost
back to the reader, where in that shape it is cheaper.

Watch `derived_refreshed` and `refresh_runs` in the counters.

---

## "My graph is bigger than RAM"

```sh
engram-server 127.0.0.1:7687 --paged-dir ./paged --paged-cache-mb 8192
```

Sealed segments spill to disk and are read block-by-block, so the **working
set** rather than the corpus sets the memory floor.

> **This is not the durable mode.** Durability is at seal boundaries only, and
> a crash loses the unsealed tail. See
> [Durability and recovery](../using/durability.md), which demonstrates the
> loss rather than describing it.

Sizing the cache: the memory line reports `cache resident/budget` alongside
everything else the engine accounts for. If resident sits at budget and
`unattributed` is small, the cache is the constraint and more helps. If
`adjacency` dominates, the cache is not your problem.

`--compact-every S` puts a *time* floor under full compaction. It matters
because a paged compaction **emits the adjacency CSRs and membership bases**,
so it is also how those refresh on a low-write workload.

---

## "Conflicts are high"

Look at the attempt distribution:

```text
won@1=… won@2=… won@3-4=… won@5-8=… won@9+=… max_attempts=…
txn_conflicts=… autocommit_reruns=… escalations=… escalated_losses=…
```

Healthy is heavily weighted to `won@1`.

| what you see | what it means | what to do |
|---|---|---|
| weight in `won@3+` | genuine contention on shared keys | make sure escalation is on (it is by default) |
| high `escalations` | contenders are queueing on FIFO locks rather than racing | this is the system working |
| conflicts on relationship writes to one hub | guard rows | keep the guard exemption on — it is worth 3.7× on that shape |
| conflicts you cannot explain | a wide read set | narrow the statement; validation covers reads as well as writes |

`ENGRAM_CONFLICT_ESCALATION=0` and `--no-guard-exemption` are the A/B controls,
not settings.

---

## "Writes are slower than they should be"

**Check group commit is on.** It batches a worker's inbox, holds every reply,
pays one `fsync`, then releases them. With one client it degrades to one fsync
per write and costs nothing; with eight, the fsync is long enough that all
eight share it.

Before it existed, write throughput was flat at **375 → 380 ops/s from 1 to 8
clients**. `--no-group-commit` restores that, and exists for the A/B only.

Then:

| flag | default | when to change |
|---|---|---|
| `--id-reservation N` | 256 | raise for a bulk-ish write load — it removes a global mutex from N−1 of every N allocations |
| `--workers N` | 1 | raise to use more cores; connections pin by `id % workers` |
| `--seal-after N` | 65536 | a large tail is served from behind the write latch |

And for a genuine corpus load, use `--bulk-ingest` —
see [Loading data at scale](../using/bulk-loading.md).

---

## "Reads are slower than they should be"

**Is the tail sealed?** Every read of a store with a non-empty tail takes the
latch writers hold. A recovered server served its whole history that way —
about **1,000 latch acquisitions per statement** under a balanced load. The
tail seals once at startup and then on `--seal-after`.

**Are derived tables serving, or is every read walking?** This is what
`stale_served` versus `stale_declined` answers, and nothing else does: the same
ops/s can mean either.

**Is an index being used?** An anchored `MATCH` seeks a range index only when
the label is large enough (512 nodes) and the predicate selective enough (16×).
`--no-property-seek` forces the scan, so an A/B tells you whether the seek is
helping.

**Segment count.** A point read walks segments newest-first, so
`--compact-after` (default 8) bounds it.

---

## "A query is refused"

```text
row budget exceeded: the statement materialised more than 20000000
intermediate rows; it would exhaust memory rather than stream
```

Working as intended. The alternative is the OOM killer, which refuses nothing
and takes every other session with it.

Fix the statement — `LIMIT`, or aggregate rather than returning rows, since
`count(*)` is answered by a fold that never materialises what it counts. Only
raise `--row-budget` when you know the statement. See
[Result paging](../using/result-paging.md).

---

## "Connections drop after five minutes"

`--read-timeout-secs` defaults to 300 and reaps a connection **quiet** for that
long. A client waiting on a long analytical query is quiet.

Raise it, or set `0` to disable — accepting that it is the slowloris guard.

---

## "Memory grows without bound"

Read the memory line and find which term moves:

```text
cache 3812/4096 MB, adjacency 4894 MB in 318 table(s), memberships 210 MB …
rss 11204 MB, unattributed 1688 MB
```

| term growing | cause |
|---|---|
| `cache` | bounded by `--paged-cache-mb`; it is meant to reach budget |
| `adjacency` | scales with `(type, direction)` pairs traversed; `--adj-overlay-fold` and `--degree-table-after` govern how much is built |
| `unattributed` | **the one to worry about** — RSS the engine cannot account for |
| none of them, but RSS grows | the in-memory commit log, if `--keep-full-log` is set |

`--keep-full-log` retains the whole commit log — about **150 B per version**,
growing with the corpus, and the term that put one paged load at ~17 GB. It is
needed only by a change-data-capture consumer.

Per-statement RSS growth above 32 MiB is reported by statement, so a single
inflating query names itself.

---

## Parallelism

Off by default. `ENGRAM_QUERY_PARALLELISM=6` installs a morsel pool of that
width and enables parallel `expand` and the parallel count fold.

It is byte-identical to the serial path — partials are concatenated in morsel
order — and it does not apply inside an explicit transaction, because the
read-your-writes overlays and the OCC read set are thread-local.

Measured at width 6 on the analytical battery: q2 5.6×, q9 5.8×, q6 5.0×,
q5 4.0×, q8 3.8×, q4 1.8×, with q1/q3/q7 flat. The win lands where the count
fold *drives*.

`ENGRAM_NO_PARALLEL_FOLD=1` is the A/B arm within a parallel run.

---

## Isolation

`--precision-locking` closes phantoms by validating each transaction's
node-pattern predicates against rows committed since its snapshot.

It is an **isolation upgrade and a behaviour change**: it aborts statements
that currently commit. Expect more conflicts. Turn it on deliberately.

---

## The levers that are not settings

Roughly twenty flags exist so one mechanism can be switched off and measured
against its own control. They are listed separately in the
[CLI reference](./cli.md), and several are documented as *slower* —
`--single-flight-repair` is measured at 40% slower and exists as a control, not
a setting.

If you are reaching for one of those outside an A/B, you probably want a
different flag.

## Next

- [Server CLI](./cli.md) — every flag with its default.
- [Compiled-in constants](./constants.md) — the thresholds you cannot set.
- [Counters and observability](./observability.md) — the instrument.
- [How Engram is measured](../measurements/index.md) — the discipline.
