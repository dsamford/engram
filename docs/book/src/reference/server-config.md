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

**Nine fields are reachable only from code.** This is a gap in the CLI rather
than a design position, and two of them are knobs an operator would plausibly
want.

| field | type | default | what it controls |
|---|---|---|---|
| `write_timeout` | `Option<Duration>` | 60 s | a peer that stops reading cannot pin a writer thread |
| `max_inflight_bytes` | `usize` | **8 MiB** | per-connection backpressure. The reader used to `send` into an unbounded channel as fast as it could read, so a fast client against a slow engine grew the queue without limit. This is what turns that into a stalled reader |
| `max_message_bytes` | `usize` | 64 MiB | the largest single Bolt message a session will assemble — policy, not protocol |
| `warm_caches` | `bool` | `true` | build derived structures **before** accepting connections. Off, the first query after a restart pays for the whole corpus — measured at 5.85 s on a 1.48M-node graph |
| `persist_indexes_at_seal` | `bool` | `true` | write declared range indexes to sidecars on a quiescent paged tick |
| `tombstone_ratio` | `f64` | **0.2** | the tombstone fraction past which a seal also asks for compaction. `1.0` disables. (Cassandra's default) |
| `tombstone_min_versions` | `u64` | **4096** | the floor below which the ratio is not consulted |
| `configure_graph` | `Option<Arc<dyn Fn(&Graph)>>` | `None` | applied to **every** graph the resolver builds |
| `paged_spill_cache` | `Option<Arc<BlockCache>>` | `None` | required with `paged_dir`; must be the same handle `open_paged_dir` returned |

### `configure_graph` deserves a warning

It exists because a caller can otherwise only configure a graph it builds
*itself* — and the graph a caller builds in `make_store` **is not the graph
that serves queries**, because `make_store` returns a `Store`.

That trap has bitten: a benchmark harness set its A/B toggles on the temporary
loading graph, they were silently discarded, and both arms of a before/after
ran the same engine and produced numbers within 2% of each other — which reads
exactly like "the fix does nothing".

Anything that must hold for every session belongs in `configure_graph`.

### `paged_spill_cache` deserves one too

It must be the **same** cache handle `Store::open_paged_dir` returned, never a
fresh one, so every later spill shares one budget. A cache per spill would grow
the memory bound with uptime.

## Fields that mirror CLI flags

Same defaults, same meanings — see the [CLI reference](./cli.md) for the full
descriptions.

### Core

| field | default | flag |
|---|---|---|
| `workers` | 1 | `--workers` |
| `row_budget` | `Some(20_000_000)` | `--row-budget` |
| `max_connections` | 512 | `--max-connections` |
| `read_timeout` | `Some(300 s)` | `--read-timeout-secs` |
| `paged_dir` | `None` | `--paged-dir` |

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

### Correctness and isolation

| field | default | flag |
|---|---|---|
| `precision_locking` | `false` | `--precision-locking` |
| `guard_put_put_exempt` | `true` | `--no-guard-exemption` |
| `constraint_epoch_cache` | `true` | `--no-constraint-epoch-cache` |

### Measurement levers

All `true` unless noted; each has a `--no-*` flag.

`lazy_stale_serve`, `adj_change_filter`, `single_node_stale_walk`,
`property_seek`, `label_scoped_indexes`, `hop_membership_contains`,
`adj_snap_memo`, `directed_bound_probe`, `agg_topk_before_project`,
`const_projection_fold`, `hop_count_memo`, `order_peak_search`,
`tail_span_copyout` — and `single_flight_repair`, which defaults to **`false`**
because it is measured 40% slower and exists as a control.

## The `Debug` impl is partial

`ServerConfig`'s `Debug` prints a subset — workers, row budget, connection cap,
timeouts, inflight and message bounds, and whether `configure_graph` is set. It
does not print every lever, so do not read a debug dump as a complete
configuration record.

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
