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
| `--paged-dir DIR` | path | *(none)* | **The bigger-than-RAM mode, and it is durable.** Sealed segments spill to `seg-<seq>.seg` and are read block-by-block; `DIR/engram.wal` fronts the unsealed tail, so every acknowledged write is `fsync`ed before the acknowledgement and replayed on open — the same contract as `--data-dir`. The two differ in on-disk layout, not in what a crash costs. |
| `--paged-cache-mb N` | MiB | `4096` | Block-cache budget for `--paged-dir`. This, not the corpus, is the steady-state memory floor. |
| `--bulk-ingest` | flag | off | Corpus-load mode: writes skip the commit log (durability by re-ingest, not replay), ids reserve in ranges of 4096, autocommit is not serialisable. **Refused with `--data-dir`.** Restart without it to serve normally. |

With no `--data-dir` and no `--paged-dir` the store is in-memory and a restart
loses everything. That is a legitimate mode for tests, and it is announced
loudly at startup rather than being a silent default.

What a paged WAL holds is the tail, not the history: a spill checkpoints the file
behind the segments it has just written, so the WAL rotates. Recovery from a
paged directory therefore means the segments and the WAL together — the WAL alone
no longer replays from the beginning, and the whole-history open refuses a
rotated file rather than replaying part of one.

See [Durability and recovery](../using/durability.md) for what each mode
survives, and [Paged mode](../architecture/paged-mode.md) for how the block
cache behaves.

## Operator knobs

Flags you might reasonably change on a server you are running.

| flag | type | default | what it does |
|---|---|---|---|
| `--workers N` | count | `1` | Engine worker threads. Connections pin to one by `id % workers`. Also settable via `ENGRAM_SERVER_WORKERS`; an explicit flag wins. |
| `--max-connections N` | count | `512` | Concurrent connections accepted. Each costs two OS threads, so an unbounded accept loop is an unbounded thread count. |
| `--row-budget N` | rows, `0` = unlimited | derived from the memory ceiling | Rows one query may materialise before it is refused. With no flag the server derives the budget from the memory the process may use — the cgroup limit, else `MemTotal`, else an assumed 8 GiB — as ceiling ÷ 4 ÷ 96 B per row, clamped to 1,000,000–4,000,000,000, and prints the figure and its source at startup. An explicit value wins, and is what a reproducible run should pin. Concurrent statements share it: each gets the budget divided by the statements in flight, never less than 1,000,000 rows (or the whole budget, if that is smaller). This is the bound that protects the process from one statement — see [Result paging](../using/result-paging.md). |
| `--memory-max-mb N` | MiB, `0` = no ceiling | the memory limit (cgroup, else `MemTotal`) | Resident-memory ceiling for the whole process. A sampler reads the resident set every 100 ms; from 90% of the ceiling new statements **queue**, and they are admitted again once it falls to 80%. A statement that has queued for 30 s without memory coming back is refused (`memory ceiling reached`), and the server keeps serving. A statement already running is never aborted. `0` removes the ceiling and leaves the OOM killer as the only limit. It complements `--row-budget` rather than replacing it: the budget bounds one statement between samples, the ceiling bounds the process over time. Enforcement reads `/proc/self/statm`; where that cannot be read the server warns at startup that the ceiling is not enforced. |
| `--read-timeout-secs N` | seconds, `0` = never | `300` | Reap a connection quiet for N seconds (the slowloris guard). **A client waiting on a long analytic query is quiet** — raise or disable this to serve queries past five minutes. |
| `--seal-after N` | versions | `65536` | Seal the write tail into an immutable, lock-free segment once it holds N versions. Reads of a non-empty tail take the latch writers hold, so an unsealed corpus is served from behind the write lock. |
| `--compact-after N` | segments | `8` | Compact the sealed segments into one once there are N. Runs on the maintenance thread; a read walks every segment newest-first, so the count bounds a point read. |
| `--compact-every S` | seconds | *(off)* | **Paged only.** Never go longer than S seconds between full compactions while more than one segment exists. A paged compaction emits the adjacency CSRs and membership bases, so this puts a floor under how often those refresh that does not depend on write volume. |
| `--maintenance-tick-secs N` | seconds | `5` | Maintenance thread tick. |
| `--id-reservation N` | ids, `0`/`1` = one write each | `256` | Ids a session reserves per durable counter write. The allocator holds a global mutex across that write, so a reservation removes it from N−1 of every N allocations. Ids stay dense within a run; a restart abandons the unused tail as a gap. |
| `--refresh-after-writes N` | commit stamps, `0` = tick only | `8192` | Commit-clock **stamps** between maintenance refreshes of derived structures. A Bolt write statement is about three stamps. |
| `--refresh-pass-rows N` | rows, `0` = unbounded | `250000` | Rows one refresh pass may re-read before deferring the rest. |
| `--precision-locking` | flag | off | Validate each transaction's node-pattern **predicates** against rows committed since its snapshot, closing phantoms. **An isolation upgrade and a behaviour change: it aborts statements that currently commit.** |
| `--expand-truncation` | flag | off | Honour LDBC FinBench's `truncationLimit` on variable-length hops: at each step, follow only the `truncationLimit` highest-ranked edges out of each expanding node, ranked by the relationship property `truncationProperty` names (default `timestamp`), most recent first unless `truncationOrder` names an ascending order. The query's own parameters drive it, so a FinBench query needs no rewriting; without this flag those parameters do nothing, so a stray parameter cannot change an answer. **It changes answers by design** — turn it on only for a workload defined with truncation. |
| `--keep-full-log` | flag | off | Retain the whole in-memory commit log instead of releasing it at a seal. About 150 B per version, growing with the corpus. Needed only by a `log_tail` (CDC/replication) consumer. |

**Six flags clamp their argument, and `0` is not a disable for any of them.**
`--workers`, `--max-connections` and `--seal-after` clamp to 1; `--compact-after`
clamps to 2; `--maintenance-tick-secs` and `--compact-every` clamp to one second.
So `--maintenance-tick-secs 0` is a one-second tick rather than a stopped
maintenance thread, `--seal-after 0` seals after every version rather than never,
and `--compact-after 1` is 2. Where `0` does mean something — and it does on
fourteen flags in this reference — the flag's own row says so.

### Derived-structure thresholds

Tunable, but move them only with a measurement in hand — see
[Derived structures](../architecture/derived-structures.md).

| flag | type | default | what it does |
|---|---|---|---|
| `--adj-overlay-fold N` | rows, `0` = fold every repair | `4096` | Overlay rows a repaired adjacency table may carry before folding. `slice` is the hottest read in the engine and pays a `BTreeMap` descent per hop while an overlay is present. |
| `--degree-table-after N` | probes, `0` = admit immediately | `1024` | Direct adjacency probes tolerated in one epoch before a degree table may be **built**. The counter resets on the global adjacency epoch, so under a write stream it may never reach N and a table for an untouched type is never built. |
| `--members-bitmap-after N` | probes, `0` = never | `4096` | Base probes before a membership base is answered from a presence bitmap. |
| `--range-fold-at N` | overlay entries, `0` = the built-in | `4096` | Overlay size past which a range index's catch-up folds the overlay into a new base. Only a **reader** folds a range index — no maintenance pass does — and the fold is O(base) however much overlay it collapses, so a larger N gives proportionally fewer folds at the same cost each. The price is paid on every ordinary read, which merges the base against a larger overlay, so the right value is a measurement on your corpus rather than a constant. |

## Measurement levers

**These are not production settings.** Each exists so one mechanism can be
switched off and the difference measured against its own control, in the same
window, on the same host. Several are documented as slower. The project's rule
is one change per measurement and a lever per mechanism; this is that rule's
surface.

Every one of these is **on by default** unless stated, so passing the flag
turns the mechanism *off* — except the four that take a value
(`--whole-label-read-max`, `--match-start-chunk`, `--fold-hoist-after`,
`--prop-column-budget-mb`), which move a threshold instead, and the four marked
*(off by default)*, which turn a mechanism *on*.

| flag | what turning it off costs |
|---|---|
| `--no-group-commit` | fsync once per **write** instead of once per batch. Slower under concurrent writers; exists for the A/B, not for production. |
| `--no-tail-copyout` | Restores the old span-read path, which holds every tail shard latch for the whole merge and so **excludes every writer** for its duration. |
| `--no-algo-parallel` | **Inert without `ENGRAM_QUERY_PARALLELISM`.** The lane is armed inside that variable's install block and the engine's own cell defaults off, so with no width installed there is nothing to switch. With a pool installed, a graph algorithm runs its fixpoint on one thread. The scores are **bit-identical** either way — morsels partition the output vertex range and merge in morsel order — so this measures the split's cost and nothing else. Expect little from the split: PageRank over a CSR is memory-bandwidth-bound, and threads do not add bandwidth. The lane is kept because it is correct, gated and default-off, and because it is floored at 65,536 vertices so it can cost nothing below that; it is not a speed-up to quote. |
| `--no-trigram-indexes` | `=~`, `CONTAINS`, `STARTS WITH` and `ENDS WITH` scan their label instead of seeking a declared trigram index. The A/B arm for the text seek: the seek declines wherever the label scan is cheaper, so it should cost nothing where it does not pay, and this flag is how that is checked. |
| `--no-bm25` | A BM25 fulltext index is answered by a scan rather than by its term index. The scores are identical — both arms build the same query plan and run the same summation — so this measures the index and not the scoring. |
| `--no-bm25-by-default` | A newly created fulltext index is stamped for term-frequency scoring rather than BM25. Existing indexes are unaffected: scoring is recorded in the catalogue row when the index is created, so no rolling change can re-rank one that already exists. |
| `--no-property-seek` | An anchored `MATCH` scans its label instead of seeking a property range index. |
| `--no-label-scoped-indexes` | A property index covers the whole partition rather than the label. |
| `--no-lazy-stale-serve` | A single-node reader repairs the whole change set instead of asking whether its own node moved. |
| `--no-adj-change-filter` | Answers that per-node question under the change log's lock instead of with one atomic load. |
| `--no-single-node-stale-walk` | A reader whose node moved repairs the table rather than walking its own span. The default trades O(change set) for O(degree), so this is the arm for a high-degree corpus. |
| `--single-flight-repair` | *(off by default)* Readers queue on the build guard so a stale table is repaired once between them. It removes the duplicated repair work and makes the readers wait on a mutex instead, which measured slower than the default — present as the control that shows the duplication was never the cost, not as a setting. |
| `--amortised-reader-repair` | *(off by default)* A single-node reader whose adjacency table is stale repairs it whenever the change log can answer — up to the log's capacity, `ADJ_LOG_CAP` (262,144 rows) — instead of declining past `ADJ_READER_REPAIR_MAX_ROWS` (8,192 rows) and walking its own span. The decline is cached per snapshot, so every reader behind it walks too, and where that walk is expensive each of them pays it on every visit; this arm pays one repair that serves every reader after it. Off because declining and letting the maintenance pass republish is the documented design; the flag lets that trade be measured rather than reversed silently. |
| `--no-subquery-end-gather` | A label past the whole-label read ceiling falls back to one projected record read per subquery hop end, instead of the bounded end gather. |
| `--whole-label-read-max N` | The label size past which a whole-label column read is declined, `WHOLE_LABEL_READ_MAX` [default: 262144]. `0` keeps the built-in. |
| `--match-start-chunk N` | How many start candidates the per-row matcher — the one every **writing** statement takes — binds and carries through a path's hops at once, testing the statement's `WHERE` on each row as its last hop finishes it [default: 4096]. `0` is the A/B arm: the whole candidate set at once and the `WHERE` after collection, the shape in which a `MATCH … WHERE … DETACH DELETE` that deletes nothing can still hold every candidate row in memory. The rows, their order and every read are the same either way. |
| `--no-deferred-reader-fold` | A READER's adjacency repair folds its overlay into a fresh base on the query thread. `AdjTable::folded` is one pass over every row of the table, allocating a whole new base, and under a sustained write stream every table keeps crossing the `--adj-overlay-fold` threshold, so every multi-node read that arrived paid for a fold in latency and resident memory. On (the default), the reader publishes the overlay as it is and the maintenance pass folds it. |
| `--no-unmetered-members-catch-up` | A membership catch-up the label's log covers is metered against the pass's row budget at one row per entry and deferred when it does not fit. A catch-up re-reads no rows — it is an in-memory fold bounded by the log's cap — so metering it that way deferred it until the log overflowed and the pass REBUILT the whole label (`members rebuilt=1`). No row budget prevents that, because a rebuild costs the label's size, not the pass's. |
| `--no-cheap-repair-pricing` | The maintenance refresh prices each stale table's repair by **walking its whole change set** and building a set of every changed node, once per stale table, under the lock writers record into — instead of reading the logs' lengths (O(log n) per log). The two prices lead to the same decision wherever they could differ, so the arms differ in how much work is done under the writers' lock, not in which repairs are taken. |
| `--no-bounded-derived-repair` | The maintenance refresh races for its row budget first-come-first-served and takes ONE unbounded repair, instead of sharing the budget max-min across every stale table and bounding each repair to its slice. Off, the same table is taken and the same tables are deferred on every pass, and the one repair the pass cannot defer is taken whole however long it is — which is how a single refresh stalls the writers. The bound caps the worst pass by spreading the work over more passes; it does not reduce the total. |
| `--no-split-maintenance` | The derived refresh runs at the tail of the storage thread's loop instead of on its own thread. It then cannot start until that loop's spill or compaction returns — and for a paged store a worker asks for storage after **every** batch, so a store past `--compact-after` runs a full compaction with the refresh queued behind it, and `refresh_runs` stops advancing for as long as the merge takes. |
| `--no-derived-refresh` | The maintenance thread does not refresh derived structures; the next reader rebuilds instead. The arm for the write-stall the refresh can cause. |
| `--no-guard-exemption` | Two relationship writes touching one node abort each other again (they PUT the same guard row). The cost falls on shapes where many writers share an endpoint. |
| `--no-constraint-epoch-cache` | Re-probe the schema-epoch key on every constrained write. The key is absent until the first constraint DDL and the sparse index cannot reject it, so the probe descends every sealed segment. |
| `--no-hop-membership-contains` | A hop's label filter materialises the whole label per published snapshot, then binary-searches it. |
| `--no-hop-count-memo` | Every labelled cardinality estimate walks the smaller label again, several times per statement for a multi-hop pattern. |
| `--no-agg-topk` | An `ORDER BY` + `LIMIT` over groups projects **every** group, then truncates. |
| `--no-const-projection-fold` | `MATCH … RETURN <constants> [LIMIT]` enumerates the pattern as written. |
| `--no-directed-bound-probe` | A directed fold close reads the level var row — a different CSR line every call. |
| `--no-adj-snap-memo` | Every probe rebuilds its `(tag, types)` map key (a heap allocation for a typed hop) and walks the table map, once per row. |
| `--no-count-fold` | A `count(*)` over a chain expands every hop instead of folding its unmaterialised suffix into a weight. |
| `--no-count-fold-memo` | A var's level is recomputed per visit even when it is a pure function of the node id. |
| `--no-fold-child-order` | A var's folded children run in pattern order rather than semijoin-first. |
| `--no-count-only-reorder` | A `count(*)` pattern's hops run in the order written rather than in the count-only reorder's order. |
| `--fold-hoist-after N` | The probes a binding of a fold close's bound node answers through the adjacency table before its row is hoisted. Default 8; `0` hoists on the first probe. A hoist costs the row (copied and sorted); a probe costs a lookup, so a binding probed once or twice never pays a row for it. After a hoist the threshold follows the row (max(N, deg / 4)). |
| `--no-fold-hoisted-close` | A fold CLOSE probes the adjacency table through `edges_to_peer_slim` on every call — a snapshot lookup, a transaction-pending check and a key build per probe — instead of a hoisted copy of the bound node's row read once per binding and sorted by peer. It matters where one close dominates a fold's walks, as a triangle close does, since every one of those walks probes the same row. Both arms count the same edges. |
| `--no-fold-symmetry-breaking` | A count fold enumerates EVERY order of an interchangeable var set and multiplies nothing. With it on, a set of node vars whose every transposition the planner proves to be an automorphism of the whole pattern — LSQB q3's `person1`/`person2`/`person3`, each with the same country sub-pattern — is enumerated in one id order (`id(p1) < id(p2) < id(p3)`, an inline `GtBound` per folded member) and the count multiplied by the set's size factorial. Exact only when the members are pairwise ADJACENT through types that carry no self-loop in the data — a self-loop is the one way two of them could bind the same node — so the gate reads the live per-type self-loop count at execution, never a cached plan. Both arms count the same. |
| `--no-rel-predicate-pushdown` | An `all(x IN <var-length rels> WHERE p)` predicate filters the finished paths instead of being pushed into the expansion, where an edge the predicate rejects is never followed. The predicate is applied either way, so both arms return the same rows and differ only in work. |
| `--path-estimate` | *(off by default)* Price a both-ends-bound multi-hop path from a measured first hop and a cached shape tail, instead of taking the written order. It helps some shapes and costs others — it prices a three-hop, both-ends-bound `OPTIONAL` leg worse than the first-hop rule does — so it stays off, and the flag lets the trade be re-measured rather than argued about. A leg shorter than three hops is decided by its first hops either way. |
| `--no-prefix-streaming` | A read-only statement the streaming pipeline refuses as a whole — one with a procedure `CALL` in the middle — runs every clause on the materialising loop, instead of streaming its prefix up to the last `WITH` before that clause. Both arms must return the same answers. |
| `--no-prop-column-epoch-currency` | Any commit anywhere retires every cached property column, instead of only a commit that moved the column's own label epoch or property epoch. This is the arm that says what the property-column cache is worth under a WRITE STREAM: with currency on, a column survives writes to other properties and other labels; with it off, one commit anywhere costs every columnar fast path its cache. |
| `--prop-column-restamp` | *(off by default)* A commit that touched neither a cached property column's label nor its property advances that column's stamp instead of retiring it — giving currency to properties with NO change log. It is the one currency lever that can revive a stale column if some write path is unaccounted for, so it is opt-in until it has a differential of its own. |
| `--prop-column-budget-mb N` | The property-column cache's byte budget in MiB (default 512). `0` turns the cache off entirely, which is the arm that prices the whole columnar family against having no cached columns at all; a small non-zero value is how the eviction path gets exercised against a real corpus rather than only a unit test. Distinct from `--no-prop-column-epoch-currency`, which keeps the cache and changes only what retires an entry. |
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
| `--data-dir` **and** `--paged-dir` | exit 1 — two on-disk layouts cannot both hold one store; the durability contract is the same either way |
| `--data-dir` naming a directory that holds paged segment files (`seg-*.seg`) | exit 1 — resident mode reads only `engram.wal` and would ignore every segment, serving an empty or partial database without an error. Use `--paged-dir` for that directory |
| `--bulk-ingest` **and** `--data-dir` | exit 1 — bulk mode's durability is by re-ingest, not replay |
| the data directory is locked by another process | exit 1, naming the holding pid |
| the store cannot be opened | **panic**, deliberately — starting empty over a data directory that was requested would look like an empty database rather than a failed open |

The directory lock is acquired **before the port is bound**, not merely before
serving: a second server that binds and then refuses has, for that moment,
taken the port from the one that legitimately holds the data — which during a
restart race is exactly when it happens, and turns a clean refusal into an
outage.

### What the CLI does not refuse

Those five are the whole list — four `exit(1)` sites and two deliberate panics,
one for each on-disk mode — and the promise above them is narrower than it
sounds, because the parser does
not validate its arguments at all. It is hand-rolled (this crate has no
dependencies and the flag set is small) and it scans `argv` for exact matches,
with no unknown-argument branch anywhere. Four things are therefore accepted in
silence:

- an unknown or misspelled flag (`--workres 8`), which is ignored;
- the `--flag=value` form, which never matches the exact string, so the field
  keeps whatever it already held;
- a numeric argument that does not parse (`--workers abc`), which falls back the
  same way;
- an `ADDR` anywhere but first — `engram-server --workers 4 0.0.0.0:7687` listens
  on `127.0.0.1:7687` and says nothing.

Read the startup lines rather than the exit code: the server prints what it
actually took.

## Not reachable from the CLI

Ten [`ServerConfig`](./server-config.md) fields have no flag of their own, and
three of those the binary still fills in: `configure_graph` from `--bulk-ingest`;
`paged_spill_cache` from `--paged-dir`, which has to be the same cache handle the
store reads through; and `serving_hint`, from `--paged-dir` and
`--paged-cache-mb`, `--workers` and `ENGRAM_QUERY_PARALLELISM`, which is what the
server reports to a client that asks what it is serving under.

The other seven are reachable only when embedding the server as a library —
`write_timeout`, `max_inflight_bytes` (the per-connection backpressure bound),
`max_message_bytes`, `warm_caches`, `persist_indexes_at_seal`, `tombstone_ratio`
and `tombstone_min_versions`. This is a gap in the CLI rather than a design
position.

Two flags do part or all of their work outside `ServerConfig`. `--memory-max-mb`
sets no field: it starts a process-wide memory governor from `main`, so a server
embedded as a library has no memory ceiling unless it calls
`spawn_memory_governor` itself. `--row-budget` does fill `row_budget`, but the
derivation described above lives
in the binary's call to `resolve_row_budget`: an embedder who never calls it gets
the struct's default of 20,000,000 rows, not a budget sized to the machine.
