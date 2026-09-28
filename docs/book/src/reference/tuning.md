# Tuning guide

Organised by the symptom you have, not by the flag you might want. The
[CLI reference](./cli.md) is the field-by-field list; this page is what to
reach for and why.

## Before you change anything

**Measure, then change one thing.** This project's own house rules are worth
borrowing wholesale:

1. One change per measurement — a run that moves two things attributes neither.
2. Use the lever, not a rebuild — most mechanisms have a `--no-*` flag, so an
   A/B has a real control. Not all do: a ratchet test freezes the remaining
   debt at 40 levers the server cannot set, and the list may only shrink. If
   the mechanism you want to test has no flag, comparing two binaries credits
   the whole delta between them to one change.
3. Write the prediction down *before* the run. A number explained afterwards
   explains anything.
4. Compare in the same window, on the same host, from the same snapshot.

The stderr counters are the instrument. They print every 30 s and only when
something moves — see [Operations](../using/operations.md).

---

## "My first query after a restart is slow"

**Check the `warmed in` startup line.**

```text
[engram-server] warmed in <ms> ms: <n> nodes, <n> out-edges, <n> in-edges, <n> adjacency table(s) holding <MB> MB …
```

Engram builds [derived structures](../architecture/derived-structures.md)
before it accepts connections precisely so this does not happen. If warming is
disabled, the first query pays for building them over the whole corpus.

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
since the last read. After a write-only burst with no reader between, the first
read afterwards stalls for as long as the whole catch-up takes.

The maintenance thread exists to prevent that, and it is on by default:

| flag | default | effect |
|---|---|---|
| `--refresh-after-writes N` | 8192 | commit-clock **stamps** between refreshes; a Bolt write statement is about three |
| `--maintenance-tick-secs N` | 5 | the tick, as a floor under refresh frequency |
| `--refresh-pass-rows N` | 250000 | rows one pass may re-read before deferring the rest |

The pass is now bounded rather than open-ended. It shares its row budget max-min
across every stale table instead of taking one unbounded repair; it runs on its
own thread rather than at the tail of the storage loop, where a spill or a
compaction could hold it up; it prices a repair from its change logs' lengths
rather than by walking them; it does not meter a membership catch-up the label's
log already covers, because that catch-up re-reads no rows; and a reader
publishes its overlay for the pass to fold rather than folding it on the query
thread. Five `--no-*` arms cover those five, and they are listed in the
[CLI reference](./cli.md) — A/B controls rather than settings, with the default
being the measured configuration. What each one does is on
[Derived structures](../architecture/derived-structures.md).

Two more mechanisms have no arm at all: past its cap a change log drops the
oldest half rather than the whole log, and a property log's cap is set in
proportion to the index it protects.

**The trade is real**: the refresh that spares the reader its stall is work the
writers pay for during a write burst, whether or not anything reads. If your
workload is write-only and nothing reads until later, `--no-derived-refresh`
moves the cost back to the reader, where in that shape it is cheaper.

Watch `derived_refreshed` and `refresh_runs` in the counters.

---

## "My graph is bigger than RAM"

```sh
engram-server 127.0.0.1:7687 --paged-dir ./paged --paged-cache-mb 8192
```

Sealed segments spill to disk and are read block-by-block, so the **working
set** rather than the corpus sets the memory floor.

> **This is the bigger-than-RAM mode, and it is durable.** `DIR/engram.wal`
> fronts the unsealed tail — every acknowledged write is `fsync`ed before the
> acknowledgement and replayed on open — so paged mode trades layout against
> `--data-dir`, not durability. What the file holds is the tail rather than the
> history: a spill checkpoints it behind the segments it wrote, so the WAL
> rotates and recovery means the segments and the WAL together. See
> [Durability and recovery](../using/durability.md).

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
| conflicts on relationship writes to one hub | guard rows | keep the guard exemption on — that shape is what it exists for |
| conflicts you cannot explain | a wide read set | narrow the statement; validation covers reads as well as writes |

`ENGRAM_CONFLICT_ESCALATION=0` and `--no-guard-exemption` are the A/B controls,
not settings.

---

## "Writes are slower than they should be"

**Check group commit is on.** It batches a worker's inbox, holds every reply,
pays one `fsync`, then releases them. With one client it degrades to one fsync
per write and costs nothing; with eight, the fsync is long enough that all
eight share it.

Without it every write pays its own fsync, so write throughput stays flat as
writers are added. `--no-group-commit` restores that, and exists for the A/B
only.

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
latch writers hold, so an unsealed corpus is served from behind the write
lock. The tail seals once at startup and then on
`--seal-after`.

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
row budget exceeded: the statement materialised more than <share> intermediate
rows (its share of <budget> across <n> statement(s) in flight); it would
exhaust memory rather than stream
```

Working as intended. The alternative is the OOM killer, which refuses nothing
and takes every other session with it.

The budget is derived from the process's memory ceiling unless `--row-budget`
names one (the startup line says which, and from what), and it is shared:
each statement in flight gets an equal part of it, never less than 1,000,000
rows (or the whole budget, if that is smaller). So a statement that passes
alone can be refused under concurrency.

Fix the statement — `LIMIT`, or aggregate rather than returning rows, since
`count(*)` is answered by a fold that never materialises what it counts. Only
raise `--row-budget` when you know the statement. See
[Result paging](../using/result-paging.md).

A different refusal comes from the memory ceiling, `--memory-max-mb`:

```text
memory ceiling reached: the process holds <MiB> MiB of a <MiB> MiB ceiling. …
```

That statement did nothing wrong: it waited 30 s for resident memory to fall
below the ceiling and it did not. The pressure is somewhere else — caches,
derived structures, or a corpus larger than the ceiling — and the memory line
below says which.

---

## "Connections drop after five minutes"

`--read-timeout-secs` defaults to 300 and reaps a connection **quiet** for that
long. A client waiting on a long analytical query is quiet.

Raise it, or set `0` to disable — accepting that it is the slowloris guard.

---

## "Memory grows without bound"

Read the memory line and find which term moves:

```text
memory: cache <n>/<budget> MB, adjacency <n> MB in <n> table(s), memberships <n> MB in <n> label(s),
range indexes <n> MB in <n> index(es), property columns <n> MB in <n> column(s); rss <n> MB, unattributed <n> MB
```

| term growing | cause |
|---|---|
| `cache` | bounded by `--paged-cache-mb`; it is meant to reach budget |
| `adjacency` | scales with `(type, direction)` pairs traversed; `--adj-overlay-fold` and `--degree-table-after` govern how much is built |
| `property columns` | bounded by `--prop-column-budget-mb` (512 MiB by default) |
| `unattributed` | **the one to worry about** — RSS the engine cannot account for |
| none of them, but RSS grows | the in-memory commit log, if `--keep-full-log` is set |

`--keep-full-log` retains the whole commit log — about **150 B per version**,
growing with the corpus. It is needed only by a change-data-capture consumer.

Per-statement RSS growth above 32 MiB is reported by statement, so a single
inflating query names itself.

---

## Parallelism

Off by default. `ENGRAM_QUERY_PARALLELISM=6` installs a morsel pool of that
width and enables parallel `expand`, the parallel count fold and the
graph-algorithm fixpoint lane; the full list of what it arms, and what `=1`
does, is on [Environment variables](./environment.md).

It is byte-identical to the serial path — partials are concatenated in morsel
order — and query execution does not parallelise inside an explicit
transaction, because the read-your-writes overlays and the OCC read set are
thread-local. The graph-algorithm fixpoint is the deliberate exception: its
morsel body reads the already-materialised projection and touches neither the
store nor an overlay, so that hazard cannot arise and the gate is not copied.

Within a parallel run, the parallel **count fold** changes only the statements
the count fold drives; a statement it does not drive runs the same either way.
`ENGRAM_NO_PARALLEL_FOLD=1` is the A/B arm within a parallel run.

Turning the whole layer on or off is a different comparison, and it is the one
`ENGRAM_QUERY_PARALLELISM` decides. Measure it on your own workload against
width 1 rather than assuming it helps: a statement can be slower in parallel
than serially, and one that is is worth reporting.

### Width is not the only knob — the slot budget is

The width is per statement, so C concurrent clients used to mean up to
`C × width` morsel workers against whatever CPU the process actually has. Once
those exceed the cores, adding clients makes every statement slower — throughput
peaks and falls, and the tail lengthens. Writes do not parallelise the same way,
so write profiles never multiply.

`ENGRAM_PARALLEL_SLOTS` bounds the total: a process-wide budget, defaulting to
the width, that a statement draws from and never waits for. Finding it empty
means running serially, which is what the engine did before parallelism existed,
so one analytical statement is unaffected and the second concurrent one stops
contending. Setting it to `width × clients` restores the old behaviour and is
its A/B arm.

When you test it, look for throughput up **and** tail latency down at the same
client count: that is what separates removing contention from trading one for
the other. And expect nothing from it with one client — a lone statement always
finds the whole budget free, so the slot budget is a concurrency mechanism and
not a speed-up.

---

## Isolation

`--precision-locking` closes phantoms by validating each transaction's
node-pattern predicates against rows committed since its snapshot.

It is an **isolation upgrade and a behaviour change**: it aborts statements
that currently commit. Expect more conflicts. Turn it on deliberately.

---

## The levers that are not settings

Most of the flag surface exists so one mechanism can be switched off and
measured against its own control. They are listed separately in the
[CLI reference](./cli.md), and several are documented as *slower* —
`--single-flight-repair` measured slower than the default and exists as a
control, not a setting.

If you are reaching for one of those outside an A/B, you probably want a
different flag.

## Next

- [Server CLI](./cli.md) — every flag with its default.
- [Compiled-in constants](./constants.md) — the thresholds you cannot set.
- [Counters and observability](./observability.md) — the instrument.
- [How Engram is measured](../measurements/index.md) — the discipline.
