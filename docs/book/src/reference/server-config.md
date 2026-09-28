# ServerConfig

`engram_server::ServerConfig` is the programmatic configuration surface, used
when you embed the server as a library rather than running the binary.

```rust
use engram_server::{ServerConfig, run_server_with_config};

let mut cfg = ServerConfig::default();       // or ::from_env()
cfg.workers = 4;
cfg.row_budget = Some(5_000_000);

run_server_with_config(listener, make_store, cfg)?;
```

Two constructors:

- **`ServerConfig::default()`** — the values below.
- **`ServerConfig::from_env()`** — the same, with `workers` taken from
  `ENGRAM_SERVER_WORKERS` when set. The variable predates the struct and is
  kept as a *default source* only, so an explicit config always wins over
  ambient process state.

## Fields with no CLI flag

**Ten fields have no flag of their own**, and three of those the binary still
fills in: `configure_graph` from `--bulk-ingest`, `paged_spill_cache` from
`--paged-dir`, and `serving_hint` from the flags and variable it reports. The
other seven are reachable only from code. That is a gap in the CLI rather than a
design position, and two of the seven are knobs an operator would plausibly
want.

| field | type | default | what it controls |
|---|---|---|---|
| `write_timeout` | `Option<Duration>` | 60 s | a peer that stops reading cannot pin a writer thread |
| `max_inflight_bytes` | `usize` | **8 MiB** | per-connection backpressure. The reader used to `send` into an unbounded channel as fast as it could read, so a fast client against a slow engine grew the queue without limit. This is what turns that into a stalled reader |
| `max_message_bytes` | `usize` | 64 MiB | the largest single Bolt message a session will assemble — policy, not protocol |
| `warm_caches` | `bool` | `true` | build derived structures **before** accepting connections. Off, the first query after a restart pays for building them over the whole corpus, inline |
| `persist_indexes_at_seal` | `bool` | `true` | write declared range indexes to sidecars on a quiescent paged tick |
| `tombstone_ratio` | `f64` | **0.2** | the tombstone fraction past which a seal also asks for compaction. `1.0` disables. (Cassandra's default) |
| `tombstone_min_versions` | `u64` | **4096** | the floor below which the ratio is not consulted |
| `configure_graph` | `Option<Arc<dyn Fn(&Graph)>>` | `None` | applied to **every** graph the resolver builds. The binary sets it from `--bulk-ingest` |
| `paged_spill_cache` | `Option<Arc<BlockCache>>` | `None` | required with `paged_dir`; must be the same handle `open_paged_dir` returned. The binary sets it from `--paged-dir` |
| `serving_hint` | `Option<ServingHint>` | `None`, which sends nothing | what the server answers in `HELLO` when a client asks what it is serving under: the block-cache budget, the intra-query width and the worker count. The binary fills it with the numbers actually in circuit — a cache budget only under `--paged-dir`, the width `ENGRAM_QUERY_PARALLELISM` installed (1 when none), and `--workers` — so a benchmark's fairness stamp can be checked against the running server rather than against a command line |

### `configure_graph` deserves a warning

It exists because a caller can otherwise only configure a graph it builds
*itself* — and the graph a caller builds in `make_store` **is not the graph
that serves queries**, because `make_store` returns a `Store`.

That trap has bitten: a benchmark harness set its A/B toggles on the temporary
loading graph, they were silently discarded, and both arms of a before/after
ran the same engine and produced the same numbers — which reads exactly like
"the fix does nothing".

Anything that must hold for every session belongs in `configure_graph`.

### `paged_spill_cache` deserves one too

It must be the **same** cache handle `Store::open_paged_dir` returned, never a
fresh one, so every later spill shares one budget. A cache per spill would grow
the memory bound with uptime.

## Fields that mirror CLI flags

Same meanings, and the same defaults with one exception, `row_budget` — see the
[CLI reference](./cli.md) for the full descriptions.

### Core

| field | default | flag |
|---|---|---|
| `workers` | 1 | `--workers` |
| `row_budget` | `Some(20_000_000)` | `--row-budget` |
| `max_connections` | 512 | `--max-connections` |
| `read_timeout` | `Some(300 s)` | `--read-timeout-secs` |
| `paged_dir` | `None` | `--paged-dir` |

`row_budget` is the exception. The struct's default is a fixed 20,000,000 rows;
the binary does not use it, and instead calls `resolve_row_budget`, which derives
a budget from the process's memory ceiling when `--row-budget` is not given. An
embedder who wants the same behaviour calls it too:

```rust
let (budget, why) = engram_server::resolve_row_budget(None); // `why` names the source
cfg.row_budget = budget;
```

The binary's memory ceiling, `--memory-max-mb`, is not a field at all: it is a
process-wide governor started by `spawn_memory_governor`, which an embedding
process has to call itself.

### Storage and durability

| field | default | flag |
|---|---|---|
| `group_commit` | `true` | `--no-group-commit` |
| `seal_after_versions` | 65,536 | `--seal-after` |
| `compact_after_segments` | 8 | `--compact-after` |
| `compact_max_interval` | `None` | `--compact-every` |
| `truncate_log_at_seal` | `true` | `--keep-full-log` |
| `id_reservation` | 256 | `--id-reservation` |

### Derived structures

| field | default | flag |
|---|---|---|
| `derived_refresh` | `true` | `--no-derived-refresh` |
| `refresh_after_writes` | 8,192 | `--refresh-after-writes` |
| `maintenance_tick` | 5 s | `--maintenance-tick-secs` |
| `refresh_pass_rows` | 250,000 | `--refresh-pass-rows` |
| `degree_table_after` | 1,024 | `--degree-table-after` |
| `adj_overlay_fold` | 4,096 | `--adj-overlay-fold` |
| `members_bitmap_after` | 4,096 | `--members-bitmap-after` |
| `range_fold_at` | **`0`**, which keeps the built-in 4,096 | `--range-fold-at` |
| `split_maintenance` | `true` | `--no-split-maintenance` |
| `bounded_derived_repair` | `true` | `--no-bounded-derived-repair` |
| `cheap_repair_pricing` | `true` | `--no-cheap-repair-pricing` |
| `members_unmetered_catch_up` | `true` | `--no-unmetered-members-catch-up` |
| `deferred_reader_fold` | `true` | `--no-deferred-reader-fold` |

The last five are the mechanisms that closed the write stall. They are controls
rather than settings — see [Tuning](./tuning.md).

### Correctness and isolation

| field | default | flag |
|---|---|---|
| `precision_locking` | `false` | `--precision-locking` |
| `guard_put_put_exempt` | `true` | `--no-guard-exemption` |
| `constraint_epoch_cache` | `true` | `--no-constraint-epoch-cache` |
| `expand_truncation` | `false` — it changes answers, by design | `--expand-truncation` |

### Text and indexes

| field | default | flag |
|---|---|---|
| `bm25_scoring` | `true` | `--no-bm25` |
| `bm25_by_default` | `true` | `--no-bm25-by-default` |
| `trigram_indexes` | `true` | `--no-trigram-indexes` |

### The count fold

| field | type | default | flag |
|---|---|---|---|
| `count_fold` | `bool` | `true` | `--no-count-fold` |
| `count_fold_memo` | `bool` | `true` | `--no-count-fold-memo` |
| `count_only_reorder` | `bool` | `true` | `--no-count-only-reorder` |
| `fold_child_order` | `bool` | `true` | `--no-fold-child-order` |
| `fold_hoisted_close` | `bool` | `true` | `--no-fold-hoisted-close` |
| `fold_symmetry_breaking` | `bool` | `true` | `--no-fold-symmetry-breaking` |
| `fold_hoist_after` | `usize` | `FOLD_HOIST_AFTER_DEFAULT` — **8** | `--fold-hoist-after N` |
| `subquery_end_gather` | `bool` | `true` | `--no-subquery-end-gather` |
| `whole_label_read_max` | `u64` | **`0`**, which keeps the built-in `WHOLE_LABEL_READ_MAX` (262,144) | `--whole-label-read-max N` |

`whole_label_read_max`'s default reads as the opposite of its effect: the name
ends `_max`, so `0` looks like "no ceiling" and means "the built-in ceiling". The
CLI documents the effective default, which is the right answer for an operator;
this table gives the field value, which is what an embedder inspecting `cfg`
will see.

### Measurement levers

Each has a flag, and each exists so one mechanism can be switched off and
measured against its own control rather than because it is a setting. Two are
not booleans and four default to `false`, which is why this is a table rather
than a sentence.

| field | type | default | flag |
|---|---|---|---|
| `lazy_stale_serve` | `bool` | `true` | `--no-lazy-stale-serve` |
| `adj_change_filter` | `bool` | `true` | `--no-adj-change-filter` |
| `single_node_stale_walk` | `bool` | `true` | `--no-single-node-stale-walk` |
| `single_flight_repair` | `bool` | **`false`** — measured slower, kept as a control | `--single-flight-repair` |
| `amortised_reader_repair` | `bool` | **`false`** — the shipped design declines past 8,192 rows | `--amortised-reader-repair` |
| `match_start_chunk` | `Option<usize>` | **`None`**, which keeps the built-in 4,096; `Some(0)` is the A/B arm | `--match-start-chunk N` |
| `rel_predicate_pushdown` | `bool` | `true` | `--no-rel-predicate-pushdown` |
| `path_estimate` | `bool` | **`false`** — it helps some shapes and costs others | `--path-estimate` |
| `prefix_streaming` | `bool` | `true` | `--no-prefix-streaming` |
| `property_seek` | `bool` | `true` | `--no-property-seek` |
| `label_scoped_indexes` | `bool` | `true` | `--no-label-scoped-indexes` |
| `hop_membership_contains` | `bool` | `true` | `--no-hop-membership-contains` |
| `adj_snap_memo` | `bool` | `true` | `--no-adj-snap-memo` |
| `directed_bound_probe` | `bool` | `true` | `--no-directed-bound-probe` |
| `agg_topk_before_project` | `bool` | `true` | `--no-agg-topk` |
| `const_projection_fold` | `bool` | `true` | `--no-const-projection-fold` |
| `hop_count_memo` | `bool` | `true` | `--no-hop-count-memo` |
| `order_peak_search` | `bool` | `true` | `--no-order-peak-search` |
| `tail_span_copyout` | `bool` | `true` | `--no-tail-copyout` |
| `algo_parallel` | `bool` | `true` | `--no-algo-parallel` |
| `prop_column_epoch_currency` | `bool` | `true` | `--no-prop-column-epoch-currency` |
| `prop_column_restamp` | `bool` | **`false`** — opt-in until it has a differential of its own | `--prop-column-restamp` |
| `prop_column_budget_mb` | `Option<usize>` | **`None`**, which keeps the built-in 512 MiB; `Some(0)` turns the cache off | `--prop-column-budget-mb N` |

`algo_parallel` is inert on its own: the graph-algorithm lane is armed inside the
`ENGRAM_QUERY_PARALLELISM` block, so with no width installed there is nothing for
this field to switch. See [Environment variables](./environment.md).

`prop_column_budget_mb` has the same shape, sharper: `None` keeps the built-in
budget and `Some(0)` turns the cache off entirely, so its two adjacent values are
maximally different. The `> 0 means set` shape the other numeric levers use
cannot express that, which is why this one is an `Option`. `match_start_chunk`
is an `Option` for the same reason: `None` keeps the built-in chunk and
`Some(0)` is the arm that turns chunking off.

These tables and the one above cover every public field of `ServerConfig` — 73
of them. Nothing gates that correspondence: `cargo xtask docs` checks CLI flag
names against `main.rs`, not struct fields against this page, so a field added
without a row here goes unnoticed until a reader misses it.

## The `Debug` impl is partial

`ServerConfig`'s `Debug` prints 17 of the 73 fields — workers, row budget,
connection cap, both timeouts, the inflight and message bounds, whether
`configure_graph` and `paged_spill_cache` are set, `warm_caches`, group commit,
the seal and compaction thresholds, `paged_dir`, and the three derived-refresh
settings (`derived_refresh`, `refresh_after_writes`, `maintenance_tick`). It
prints none of the measurement levers, so do not read a debug dump as a
complete configuration record.

## Entry points

```rust
pub fn run_server(
    listener: TcpListener,
    make_store: impl FnOnce() -> (Store, Realm, Namespace) + Send + 'static,
) -> std::io::Result<()>;

pub fn run_server_with_workers(listener, make_store, workers: usize) -> io::Result<()>;
pub fn run_server_with_config(listener, make_store, cfg: ServerConfig) -> io::Result<()>;
```

`make_store` is a closure rather than a value because the store is built **on
the engine thread** — it is deliberately not `Send` in its construction path,
so the open, and any refusal, happens there.

## Next

- [Server CLI](./cli.md) — the flags, and which fields they reach.
- [Tuning guide](./tuning.md) — what to change, by symptom.
- [Compiled-in constants](./constants.md) — the thresholds nothing reaches.
