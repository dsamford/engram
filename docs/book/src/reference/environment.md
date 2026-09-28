# Environment variables

Every `ENGRAM_*` variable, grouped by who reads it. Locations are given so you
can check the behaviour rather than trust this page. For the engine they name
the function that reads the variable rather than a line number: an insert
anywhere above a line invalidates it, and nothing here is gated —
`cargo xtask docs` checks CLI flag names, not environment variables.

Most configuration is CLI flags — see the [CLI reference](./cli.md). There is
**no configuration file**; that is deferred work.

## Runtime — the server and engine

These affect a running server.

| variable | value | effect |
|---|---|---|
| `ENGRAM_SERVER_WORKERS` | usize ≥ 1 | default for `--workers`. The flag wins; this is a *default source* only, so an explicit config always beats ambient process state (`engram-server/src/lib.rs`, `ServerConfig::from_env`) |
| `ENGRAM_QUERY_PARALLELISM` | usize > 1 | installs a morsel-parallel thread pool of that width and arms four things behind it: the process-wide slot budget (`ENGRAM_PARALLEL_SLOTS`), `parallel_expand`, the graph-algorithm fixpoint lane, and `parallel_fold` unless `ENGRAM_NO_PARALLEL_FOLD` is set. The filter is `> 1`, so `=1` installs nothing at all. `--no-algo-parallel` is the fixpoint lane's A/B arm and is inert without this variable. **Off unless set**, and that is expensive: against width 1 on the analytical battery a default install measures 1.3x-5.8x slower on eight of nine LSQB queries, which is what made engram appear to lose q5/q7/q8 to PostgreSQL until the comparison was re-run with the variable set. There is no CLI flag for query parallelism, so this variable is the only way to it. The ninth query goes the other way: q1 is 2.1x *slower* with parallelism on (`engram-server/src/lib.rs`, the parallelism block in `run_server_with_config`) |
| `ENGRAM_PARALLEL_SLOTS` | usize ≥ 1 | the process-wide morsel budget, shared by every statement including graph algorithms. It defaults to the width, so one analytical statement still gets the full parallel fold while a second concurrent statement finds the pool empty and runs serially rather than contending for the same cores. A statement never waits for a slot — it takes what is free and degrades, and taking none means running serially, which is what the engine did before parallelism existed. **Inert on its own**: it is read only inside the `ENGRAM_QUERY_PARALLELISM` block. Setting it to `width x clients` restores the unbounded per-statement behaviour, which is its A/B arm. See the [tuning guide](./tuning.md) for what it measured (`engram-server/src/lib.rs`, the same block) |
| `ENGRAM_NO_PARALLEL_FOLD` | presence | within a parallel run, disables the parallel count fold. The A/B arm inside parallelism, and an env var rather than a flag — there is no `--no-parallel-fold` (`engram-server/src/lib.rs`, the same block) |
| `ENGRAM_CONFLICT_ESCALATION` | `0` or `false` | turns conflict escalation **off**. On by default (`engram-server/src/lib.rs`, the A/B env toggles in `run_server_with_config`) |
| `ENGRAM_ALGO_NODE_CEILING` | u64 | how many nodes a graph-algorithm projection may hold before it is refused. Default 20,000,000 |
| `ENGRAM_ALGO_EDGE_CEILING` | u64 | the same for edges. Default 200,000,000 |
| `ENGRAM_ALGO_BYTE_CEILING` | u64 | the same for the projection's working set in bytes. Default 2 GiB |
| `ENGRAM_ALGO_WORK_CEILING` | u64 | the `V x E` product an ALL-PAIRS algorithm may cost — betweenness, closeness, and Yen's k-shortest paths scaled by `k`. Default 10,000,000,000. Their cost is `O(V x E)` where every other algorithm is `O(V + E)`, so a projection inside the node and edge ceilings can still be days of work |
| `ENGRAM_ALGO_CACHE_BYTES` | u64 | the `mutate` result cache's byte budget. Default 512 MiB |

All five are read in one place — `apply_algo_ceilings` in `engram-server`, which
takes the environment lookup as an argument so that "the variable reaches the
ceiling" is a property a test can assert against a fake environment rather than
against the process environment every other test in the binary shares.

Each of the five names appears in the refusal it governs — "raise
`ENGRAM_ALGO_NODE_CEILING`" — and for one release **none of them was read
anywhere**. An operator following the message exactly would set the variable,
see no change, and have no way to tell the advice was fiction. A value that
does not parse is ignored rather than fatal: these raise a safety ceiling, so
leaving the default in place is the conservative failure.

### Tracing

| variable | value | effect |
|---|---|---|
| `ENGRAM_TRACE_STATEMENTS` | presence | print every statement as it is received (`engram-server/src/lib.rs`, the per-worker session setup in `run_server_with_config`) |
| `ENGRAM_TRACE_COUNTERS` | presence | dump per-statement counters, largest first (same site) |
| `ENGRAM_TRACE_MARKER` | presence | **permit** the per-statement `/* engram:trace */` marker (same site). Off by default: the marker is chosen by the client, and a traced statement costs up to an order of magnitude more and writes its text into the server log. The server says so at boot when it is on |
| `ENGRAM_TRACE_PLAN` | presence | print the plan a statement got, including fold marks (`engram-graph/src/pipeline.rs`, two sites: the fold-mark dump in `recognise_aggregate` and the ordering search's own dump) |

**Prefer the per-statement marker** to the whole-server firehose. On a server
started with `ENGRAM_TRACE_MARKER=1`, prefixing a query with `/* engram:trace */`
traces that one statement:

```cypher
/* engram:trace */ MATCH (p:Person)-[:KNOWS]->(f) RETURN count(f)
```

Without the permission the marker is an ordinary comment: the statement runs
untraced, `bolt.trace marker ignored: tracing not permitted` is counted, and the
server log notes it once, so a trace that did not appear says why.

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
