# Server CLI

```text
engram-server [ADDR] [OPTIONS]
```

`ADDR` is the Bolt listen address, default `127.0.0.1:7687`. `--help` prints
this same list; `cargo xtask docs` fails the build if the two ever disagree.

> **Most of these flags are not settings.** Engram carries a lever per
> mechanism so that a performance change can be measured as an A/B against its
> own control. Those levers are listed separately, under
> [Measurement levers](#measurement-levers), and several are documented as
> *slower*. If you are operating a server, the two sections you want are
> [Storage modes](#storage-modes) and [Operator knobs](#operator-knobs).

## Storage modes

The single most consequential choice, because it decides what a crash costs.
`--data-dir` and `--paged-dir` are mutually exclusive and the server exits 1 if
both are given.

| flag | type | default | what it does |
|---|---|---|---|
| `-d`, `--data-dir DIR` | path | *(none — in-memory)* | **The durable mode.** Every acknowledged write is `fsync`ed before the acknowledgement; a restart replays `DIR/engram.wal`. |
| `--paged-dir DIR` | path | *(none)* | **The bigger-than-RAM mode.** Sealed segments spill to `seg-<seq>.seg` and are read block-by-block. **Durability is at seal boundaries only — a crash loses the unsealed tail.** |
| `--paged-cache-mb N` | MiB | `4096` | Block-cache budget for `--paged-dir`. This, not the corpus, is the steady-state memory floor. |
| `--bulk-ingest` | flag | off | Corpus-load mode: writes skip the commit log (durability by re-ingest, not replay), ids reserve in ranges of 4096, autocommit is not serialisable. **Refused with `--data-dir`.** Restart without it to serve normally. |

With no `--data-dir` and no `--paged-dir` the store is in-memory and a restart
loses everything. That is a legitimate mode for tests, and it is announced
loudly at startup rather than being a silent default.

See [Durability and recovery](../using/durability.md) for what each mode
survives, and [Paged mode](../architecture/paged-mode.md) for how the block
cache behaves.

## Operator knobs

Flags you might reasonably change on a server you are running.

| flag | type | default | what it does |
|---|---|---|---|
| `--workers N` | count | `1` | Engine worker threads. Connections pin to one by `id % workers`. Also settable via `ENGRAM_SERVER_WORKERS`; an explicit flag wins. |
| `--max-connections N` | count | `512` | Concurrent connections accepted. Each costs two OS threads, so an unbounded accept loop is an unbounded thread count. |
| `--row-budget N` | rows, `0` = unlimited | `20000000` | Rows one query may materialise before it is refused. This is the bound that actually protects the process — see [Result paging](../using/result-paging.md). |
| `--read-timeout-secs N` | seconds, `0` = never | `300` | Reap a connection quiet for N seconds (the slowloris guard). **A client waiting on a long analytic query is quiet** — raise or disable this to serve queries past five minutes. |
| `--seal-after N` | versions | `65536` | Seal the write tail into an immutable, lock-free segment once it holds N versions. Reads of a non-empty tail take the latch writers hold, so an unsealed corpus is served from behind the write lock. |
| `--compact-after N` | segments | `8` | Compact the sealed segments into one once there are N. Runs on the maintenance thread; a read walks every segment newest-first, so the count bounds a point read. |
| `--compact-every S` | seconds | *(off)* | **Paged only.** Never go longer than S seconds between full compactions while more than one segment exists. A paged compaction emits the adjacency CSRs and membership bases, so this puts a floor under how often those refresh that does not depend on write volume. |
| `--maintenance-tick-secs N` | seconds | `5` | Maintenance thread tick. |
| `--id-reservation N` | ids, `0`/`1` = one write each | `256` | Ids a session reserves per durable counter write. The allocator holds a global mutex across that write, so a reservation removes it from N−1 of every N allocations. Ids stay dense within a run; a restart abandons the unused tail as a gap. |
| `--refresh-after-writes N` | commit stamps, `0` = tick only | `8192` | Commit-clock **stamps** between maintenance refreshes of derived structures. A Bolt write statement is about three stamps. |
| `--refresh-pass-rows N` | rows, `0` = unbounded | `250000` | Rows one refresh pass may re-read before deferring the rest. |
| `--precision-locking` | flag | off | Validate each transaction's node-pattern **predicates** against rows committed since its snapshot, closing phantoms. **An isolation upgrade and a behaviour change: it aborts statements that currently commit.** |
| `--keep-full-log` | flag | off | Retain the whole in-memory commit log instead of releasing it at a seal. About 150 B per version, growing with the corpus. Needed only by a `log_tail` (CDC/replication) consumer. |

### Derived-structure thresholds

Tunable, but the defaults were measured. Raise them only with a measurement in
hand — see [Derived structures](../architecture/derived-structures.md).

| flag | type | default | what it does |
|---|---|---|---|
| `--adj-overlay-fold N` | rows, `0` = fold every repair | `4096` | Overlay rows a repaired adjacency table may carry before folding. `slice` is the hottest read in the engine and pays a `BTreeMap` descent per hop while an overlay is present. |
| `--degree-table-after N` | probes, `0` = admit immediately | `1024` | Direct adjacency probes tolerated in one epoch before a degree table may be **built**. The counter resets on the global adjacency epoch, so under a write stream it may never reach N and a table for an untouched type is never built. |
| `--members-bitmap-after N` | probes, `0` = never | `4096` | Base probes before a membership base is answered from a presence bitmap. |

## Measurement levers

**These are not production settings.** Each exists so one mechanism can be
switched off and the difference measured against its own control, in the same
window, on the same host. Several are documented as slower. The project's rule
is one change per measurement and a lever per mechanism; this is that rule's
surface.

Every one of these is **on by default** unless stated, so passing the flag
turns the mechanism *off*.

| flag | what turning it off costs |
|---|---|
| `--no-group-commit` | fsync once per **write** instead of once per batch. Slower under concurrent writers; exists for the A/B, not for production. |
| `--no-tail-copyout` | Restores the old span-read path, which holds every tail shard latch for the whole merge and so **excludes every writer** for its duration. |
| `--no-property-seek` | An anchored `MATCH` scans its label instead of seeking a property range index. |
| `--no-label-scoped-indexes` | A property index covers the whole partition rather than the label. |
| `--no-lazy-stale-serve` | A single-node reader repairs the whole change set instead of asking whether its own node moved. |
| `--no-adj-change-filter` | Answers that per-node question under the change log's lock instead of with one atomic load. |
| `--no-single-node-stale-walk` | A reader whose node moved repairs the table rather than walking its own span. The default trades O(change set) for O(degree), so this is the arm for a high-degree corpus. |
| `--single-flight-repair` | *(off by default)* Readers queue on the build guard so a stale table is repaired once between them. **Measured 40% slower** — present as the control, not as a setting. |
| `--no-derived-refresh` | The maintenance thread does not refresh derived structures; the next reader rebuilds instead. The arm for the write-stall the refresh can cause. |
| `--no-guard-exemption` | Two relationship writes touching one node abort each other again (they PUT the same guard row). The default is worth 3.7× on the shared-endpoint shape. |
| `--no-constraint-epoch-cache` | Re-probe the schema-epoch key on every constrained write. The key is absent until the first constraint DDL and the sparse index cannot reject it, so the probe descends every sealed segment. |
| `--no-hop-membership-contains` | A hop's label filter materialises the whole label per published snapshot, then binary-searches it. |
| `--no-hop-count-memo` | Every labelled cardinality estimate walks the smaller label again — 2M nodes for `(:Comment)` at SF1, about 16 walks per LSQB q2 statement. |
| `--no-agg-topk` | An `ORDER BY` + `LIMIT` over groups projects **every** group, then truncates. |
| `--no-const-projection-fold` | `MATCH … RETURN <constants> [LIMIT]` enumerates the pattern as written. |
| `--no-directed-bound-probe` | A directed fold close reads the level var row — a different CSR line every call. |
| `--no-adj-snap-memo` | Every probe rebuilds its `(tag, types)` map key (a heap allocation for a typed hop) and walks the table map, once per row. |
| `--no-order-peak-search` | The count-only reorder keeps its greedy, which scores only the immediate step. |

## Informational

| flag | effect |
|---|---|
| `-h`, `--help` | Print the usage text and exit. |
| `-V`, `--version` | Print the version and exit. |

## Environment

`ENGRAM_SERVER_WORKERS` supplies the default for `--workers`. A number of other
variables affect the engine and the benchmark harness — see
[Environment variables](./environment.md).

## Refusals at startup

The server prefers to exit rather than to start in a state you did not ask for.

| condition | behaviour |
|---|---|
| `--data-dir` **and** `--paged-dir` | exit 1 — the two are different durability models |
| `--bulk-ingest` **and** `--data-dir` | exit 1 — bulk mode's durability is by re-ingest, not replay |
| the data directory is locked by another process | exit 1, naming the holding pid |
| the store cannot be opened | **panic**, deliberately — starting empty over a data directory that was requested would look like an empty database rather than a failed open |

The directory lock is acquired **before the port is bound**, not merely before
serving: a second server that binds and then refuses has, for that moment,
taken the port from the one that legitimately holds the data — which during a
restart race is exactly when it happens, and turns a clean refusal into an
outage.

## Not reachable from the CLI

Nine [`ServerConfig`](./server-config.md) fields have no flag, including
`max_inflight_bytes` (the per-connection backpressure bound), `write_timeout`,
`tombstone_ratio` and `tombstone_min_versions`. They are reachable only when
embedding the server as a library. This is a gap in the CLI rather than a
design position.
