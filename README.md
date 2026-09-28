# Engram

A property-graph database in Rust, speaking openCypher over the Bolt protocol.

> **Pre-release.** There is no authentication and no TLS. Do not expose this to
> a network you do not control. See [SECURITY.md](SECURITY.md) and
> [Known limits](#known-limits) — the latter is a list of absences, written to
> be read before you rely on anything.

## What it is

A single-process graph engine with its own Cypher parser and planner, MVCC
storage over a write-ahead log, paged segments that read block-by-block so a
graph can exceed RAM, vector indexes, and a Bolt listener that stock Neo4j
drivers connect to. It offers Bolt 6.0 and 5.0–5.8, in that order of
preference, through the Manifest v1 handshake.

| | |
|---|---|
| **openCypher conformance** | **3,769 of 3,773** evaluated TCK scenarios (99.9%), CI-ratcheted |
| **`unsafe` code** | none — the workspace lint denies it outright, and the engine crates forbid it at the crate root |
| **Third-party crates** | every one permissively licensed, no copyleft anywhere — `cargo xtask c-deps` prints the count and what it inspected |
| **Reproducibility** | two processes, one seed, one identical trace digest — enforced as a gate |

The conformance number is the honest headline. It is measured by running the
vendored openCypher TCK and ratcheted in CI so it cannot quietly fall below its
floor. Of the four failures, three expect a bare pattern used as a value — in a
`RETURN` or `WITH` projection, or on the right-hand side of a `SET` — to be
refused as a syntax error, and Engram accepts the query; the fourth is a
time-zone-database expectation where this engine is arguably the more correct of
the two.

## Design

Three decisions are load-bearing, and each is enforced by a gate rather than by
convention — because a rule nothing checks is a preference.

| | decision | enforced by |
|---|---|---|
| **D1** | time, randomness, task-spawning and I/O arrive through a `Runtime` trait | `clippy.toml` denies `Instant::now`, `thread::spawn`, `HashMap`, … |
| **D2** | single-threaded per shard; concurrency is cooperative tasks, and one statement runs on one thread by default | a shared-pool-plus-locks design is not simulable — no seed can name an OS scheduler's interleaving |
| **D3** | every subsystem declares its crash points, `sometimes!` events and counters | `cargo xtask d3`, plus a simulation sweep that fails when a declared state is never reached |

D1 is what makes the simulation lane real: with the clock and the scheduler
injected, a seed reproduces a run exactly, so a failure is a repro rather than a
story about a bad afternoon.

"By default" in D2 is the whole of the qualifier. Morsel-parallel execution
exists and is opt-in: setting `ENGRAM_QUERY_PARALLELISM` installs a thread pool
in `engram-server`, which is the only production implementor of the engine's
scoped-execution seam. The engine crates still never spawn, so the simulation
lane still sees one thread.

## Layout

```
crates/engram-observe    the assertion vocabulary, counters, the determinism trace
crates/engram-runtime    the Runtime trait, a real simulated executor, a Tokio one
crates/engram-key        key encoding: realm / namespace / kind / partition
crates/engram-store      records, MVCC, entity write locks, native CAS
crates/engram-log        the write-ahead log and its BLAKE3 hash chain
crates/engram-proc       the procedure catalogue — one sorted table, read by cypher and graph
crates/engram-cypher     lexer, parser, expression evaluation, temporal types
crates/engram-graph      the graph model, planner, interpreter, columnar pipeline
crates/engram-bolt       the sans-io Bolt protocol machine — 6.0 and 5.0–5.8
crates/engram-server     the TCP adapter — the one place threads and sockets live
crates/engram-sim        deterministic simulation: seeds, crashes, invariants
crates/engram-tck        the openCypher TCK harness (not published)
crates/engram-bench      benchmark and loader binaries (not published)
xtask                    the durable gates
```

Four more crates are compiled, tested and simulated and are **not on the
serving path**: `engram-exec` (a bitmap operator seam), `engram-crypto`,
`engram-objstore` and `engram-blob`. None of them is named by `engram-cypher`,
`engram-graph`, `engram-bolt` or `engram-server`, in a manifest or in a source
file. Do not read their presence as a shipped feature.

## Running it

```sh
cargo build --release -p engram-server
./target/release/engram-server 127.0.0.1:7687 --data-dir ./data
```

Without `--data-dir` the store is in-memory and a restart loses everything; the
server says so loudly at startup. With it, every acknowledged write is `fsync`ed
before the acknowledgement and a restart replays the log.

`--help` lists the flags.

## The gates

```sh
cargo xtask all           # d3, c-deps, msrv, determinism, hygiene, docs
cargo xtask docs          # no orphan pages, no dead intra-book links, and the
                          # CLI reference names exactly the flags the CLI parses
cargo xtask scrub <dir>   # no forbidden token survives in a staged tree
cargo xtask public-tree <dir>   # assemble a publishable tree by copy-allowlist
```

Every gate prints what it *scanned*, not just what it found. A gate that walked
zero files reports "no findings" in exactly the same words as one that walked
all of them, and this repository has shipped an audit that skipped the very
site it appeared to clear.

## Measurements

Performance claims are in the book's
[Measurements](docs/book/src/measurements/index.md) part, with the rules every
run is held to and the method behind each number. The current standing compares
Engram with Neo4j 5.26.31 Community and PostgreSQL 17.11 on workloads derived
from the LDBC benchmarks — LSQB and SNB Interactive and BI at SF3 and SF10,
FinBench at SF1 and SF10 — plus the LDBC Graphalytics kernels and a concurrent
stress test, each engine alone in the same 40-CPU container on one 48-core
server, with identical answers asserted per query before any time is compared.
These are not official LDBC results.

The benchmark harness, its statement catalogue and the checked-in regression
baselines (`measurements/baselines/`) ship with the source; the rest of
`measurements/` and the corpora stay in the development tree, for the reason
`xtask/src/public_tree.rs` gives. No ratio is quoted here, because a number in
this file is a copy no gate checks.

## Known limits

Stated as absences, because a list of features implies the rest exist.

- **No authentication.** Credentials are accepted and not verified.
- **No TLS.** Traffic is plaintext.
- **No clustering, no replication, no failover.** One process, one shard.
- **No backup or point-in-time restore tooling.** The WAL and segments are the
  durable artifacts; there is no supported procedure around them yet.
- **No audit log.** Authentication attempts, session lifecycle, DDL and
  authorization denials are not recorded.
- **Cypher gaps.** `=~` evaluates, over a
  finite automaton — the constructs that force a backtracker (backreferences,
  lookaround, atomic groups, possessive quantifiers) are refused by name rather
  than reinterpreted. Full-text scoring is BM25 for indexes created from now on
  and term frequency for every index created before, because the scoring is
  stamped into the index's catalogue row when it is created; there is no
  stemming, no stopwords, no phrase or proximity queries and no fuzzy matching.
  Vector index `OPTIONS` are parsed and uninterpreted — the dimension is
  inferred from the data and the metric is cosine.
- **Not a Neo4j drop-in yet**, despite the wire compatibility. The driver
  compatibility matrix is open, not closed.

The whole list, with the reasoning behind each absence, is the book's
[Known limits](docs/book/src/known-limits.md). What is above is a subset of that
page and is meant to stay one: the two drifted apart once already, on BM25 and
on `=~`, and nothing checks them against each other.

## Licence

MIT. See `LICENSE`.

Third-party attributions that travel with a binary are in [NOTICE](NOTICE).

The openCypher TCK vendored under `crates/engram-tck/` is Apache-2.0, copyright
Neo4j Sweden AB, and is redistributed under its own terms; that crate is not
published. Neo4j and Cypher are trademarks of Neo4j, Inc.; this project is not
affiliated with, endorsed by, or derived from Neo4j. See
[TRADEMARKS.md](TRADEMARKS.md), which also records what this project has *not*
cleared for its own name.
