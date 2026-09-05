# Environment variables

Every `ENGRAM_*` variable, grouped by who reads it. Locations are given so you
can check the behaviour rather than trust this page.

Most configuration is CLI flags — see the [CLI reference](./cli.md). There is
**no configuration file**; that is deferred work.

## Runtime — the server and engine

These affect a running server.

| variable | value | effect |
|---|---|---|
| `ENGRAM_SERVER_WORKERS` | usize ≥ 1 | default for `--workers`. The flag wins; this is a *default source* only, so an explicit config always beats ambient process state (`engram-server/src/lib.rs:556`) |
| `ENGRAM_QUERY_PARALLELISM` | usize > 1 | installs a morsel-parallel thread pool of that width and enables `parallel_expand` and `parallel_fold`. **Off unless set** (`:745`) |
| `ENGRAM_NO_PARALLEL_FOLD` | presence | within a parallel run, disables the parallel count fold. The A/B arm inside parallelism (`:755`) |
| `ENGRAM_CONFLICT_ESCALATION` | `0` or `false` | turns conflict escalation **off**. On by default (`:763`) |

### Tracing

| variable | value | effect |
|---|---|---|
| `ENGRAM_TRACE_STATEMENTS` | presence | print every statement as it is received (`:1349`) |
| `ENGRAM_TRACE_COUNTERS` | presence | dump per-statement counters, largest first (`:1354`) |
| `ENGRAM_TRACE_PLAN` | presence | print the plan a statement got, including fold marks (`engram-graph/src/pipeline.rs:2547`) |

**Prefer the per-statement marker** to the whole-server firehose. Prefixing a
query with `/* engram:trace */` traces that one statement:

```cypher
/* engram:trace */ MATCH (p:Person)-[:KNOWS]->(f) RETURN count(f)
```

## Test and gate variables

These gate or parameterise tests. They do not affect a server.

| variable | default | effect |
|---|---|---|
| `ENGRAM_SEED` | 424242 | the seed the determinism test runs (`engram-runtime/tests/determinism.rs:49`) |
| `ENGRAM_EXPECTED_DIGEST` | unset | pin the expected trace digest; unset means the two runs are compared to each other only (`:106`) |
| `ENGRAM_SWEEP_SEEDS` | 48 | seeds the simulation sweep runs. CI uses the default; nightly runs more (`engram-sim/tests/sweep.rs:9`) |
| `ENGRAM_TCK_PRECISION_LOCKING` | unset | `"1"` runs the TCK with precision locking on — the second conformance arm (`engram-tck/src/lib.rs:710`) |

Several integration tests need an external corpus and **skip cleanly** when
their directory variable is unset, rather than failing or silently passing:

`ENGRAM_LSQB_ALL_NINE_DIRS`, `ENGRAM_LSQB_ALL_NINE_SKIP`,
`ENGRAM_LSQB_ALL_NINE_SKIP_GENERAL`, `ENGRAM_LSQB_ATTRIB_DIR`,
`ENGRAM_LSQBREF_AGREEMENT`.

## Benchmark harness variables

Read only by `engram-bench` binaries. They exist so a mechanism can be switched
off and measured against its own control — the same reasoning as the `--no-*`
flags.

### The port benchmark (`portbench`)

`ENGRAM_PORT_OPEN_PAGED`, `ENGRAM_PORT_PAGED`, `ENGRAM_PORT_PERSIST_IDX`,
`ENGRAM_PORT_ROW_BUDGET`, `ENGRAM_PORT_COLUMN_BUDGET_FACTOR`,
`ENGRAM_PORT_DEGREE_TABLE_AFTER`, `ENGRAM_PORT_BISECT`,
`ENGRAM_PORT_BISECT_SQL`, and the mechanism arms `ENGRAM_PORT_NO_LATE`,
`_NO_SEEK`, `_NO_FRONTIER`, `_NO_COLUMNAR`, `_NO_DEGREE`, `_NO_COMPACT`,
`_NO_MS_BATCH`, and the per-query arms `_NO_IC2`, `_NO_IC3`, `_NO_IC11`,
`_NO_BI7`.

### Attribution harnesses

`ENGRAM_BURST_ATTRIB_*` (post-burst rebuild attribution — `DIR`, `N`, `PAGED`,
`SET_FIRST`, `CHURN_THREADS`, `CHURN_PAD`, `LEVERS`, `MAINT`),
`ENGRAM_RELATTRIB_*` (relationship ingest — `PLAN`, `BATCH`, `BULK`, `LIMIT`,
`MODES`), `ENGRAM_BENCH_BASELINE`.

## Standard variables Engram respects

| variable | effect |
|---|---|
| `CARGO_HOME` | where the `c-deps` gate looks for extracted package sources |
| `ENGRAM_CDEPS_SRC_DIRS` | additional directories for that gate — CI fills it from `cargo vendor` so the gate sees the full lock file on any host |
| `RUST_BACKTRACE` | standard |

## What is not configurable by environment

Deliberately: **storage mode, durability and the listen address**. Those are
CLI arguments, because a server that silently changed its durability model
based on ambient process state would be a bad server.

## Next

- [Server CLI](./cli.md) — the flags.
- [Tuning guide](./tuning.md) — what to change, by symptom.
- [Counters and observability](./observability.md) — what tracing produces.
