# Engram

A property-graph database in Rust, speaking openCypher over the Bolt protocol.

> **Pre-release.** There is no authentication and no TLS. Do not expose this to
> a network you do not control. See [SECURITY.md](SECURITY.md) and
> [Known limits](#known-limits) — the latter is a list of absences, written to
> be read before you rely on anything.

## What it is

A single-process graph engine with its own Cypher parser and planner, MVCC
storage over a write-ahead log, paged segments that read block-by-block so a
graph can exceed RAM, vector indexes, and a Bolt v5 listener that stock Neo4j
drivers connect to.

| | |
|---|---|
| **openCypher conformance** | **3,772 of 3,773** evaluated TCK scenarios (99.97%), CI-ratcheted |
| **`unsafe` code** | none — 13 crates `#![forbid(unsafe_code)]`, the workspace denies it |
| **Third-party crates** | 39, every one permissively licensed, no copyleft anywhere |
| **Reproducibility** | two processes, one seed, one identical trace digest — enforced as a gate |

The conformance number is the honest headline. It is measured by running the
vendored openCypher TCK, it is ratcheted in CI so it cannot quietly fall, and
the single failure is a time-zone-database expectation where this engine is
arguably the more correct of the two.

## Design

Three decisions are load-bearing, and each is enforced by a gate rather than by
convention — because a rule nothing checks is a preference.

| | decision | enforced by |
|---|---|---|
| **D1** | time, randomness, task-spawning and I/O arrive through a `Runtime` trait | `clippy.toml` denies `Instant::now`, `thread::spawn`, `HashMap`, … |
| **D2** | single-threaded per shard; concurrency is cooperative tasks | a shared-pool-plus-locks design is not simulable — no seed can name an OS scheduler's interleaving |
| **D3** | every subsystem declares its crash points, `sometimes!` events and counters | `cargo xtask d3`, plus a simulation sweep that fails when a declared state is never reached |

D1 is what makes the simulation lane real: with the clock and the scheduler
injected, a seed reproduces a run exactly, so a failure is a repro rather than a
story about a bad afternoon.

## Layout

```
crates/engram-observe    the assertion vocabulary, counters, the determinism trace
crates/engram-runtime    the Runtime trait, a real simulated executor, a Tokio one
crates/engram-key        key encoding: realm / namespace / kind / partition
crates/engram-store      records, MVCC, entity write locks, native CAS
crates/engram-log        the write-ahead log and its BLAKE3 hash chain
crates/engram-cypher     lexer, parser, expression evaluation, temporal types
crates/engram-graph      the graph model, planner, interpreter, columnar pipeline
crates/engram-bolt       the sans-io Bolt v5 protocol machine
crates/engram-server     the TCP adapter — the one place threads and sockets live
crates/engram-sim        deterministic simulation: seeds, crashes, invariants
crates/engram-tck        the openCypher TCK harness (not published)
xtask                    the durable gates
```

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
cargo xtask all           # d3, c-deps, msrv, determinism, hygiene
cargo xtask scrub <dir>   # no forbidden token survives in a staged tree
cargo xtask public-tree <dir>   # assemble a publishable tree by copy-allowlist
```

Every gate prints what it *scanned*, not just what it found. A gate that walked
zero files reports "no findings" in exactly the same words as one that walked
all of them, and this repository has shipped an audit that skipped the very
site it appeared to clear.

## Measurements

Performance claims live under [`measurements/`](measurements/), each with the
corpus, the host shape and the command that produced it. Nothing is quoted here
that is not reproducible from a file there.

The most recent is a stress and scalability sweep over `snbgen`-generated
corpora (synthetic data on an SNB-like schema — NOT official LDBC Datagen
output, and not presentable as LDBC results) from 25k to 1.5M nodes, run as a
paired before/after on one host in one run —
because a before measured on one machine and an after on another is not a
comparison.

## Known limits

Stated as absences, because a list of features implies the rest exist.

- **No authentication.** Credentials are accepted and not verified.
- **No TLS.** Traffic is plaintext.
- **No clustering, no replication, no failover.** One process, one shard.
- **No backup or point-in-time restore tooling.** The WAL and segments are the
  durable artifacts; there is no supported procedure around them yet.
- **No audit log.** Authentication attempts, session lifecycle, DDL and
  authorization denials are not recorded.
- **Cypher gaps.** Full-text search is term-frequency, not BM25; `UNION` inside
  `CALL {}` is refused; `=~` parses and refuses at evaluation.
- **Not a Neo4j drop-in yet**, despite the wire compatibility. The driver
  compatibility matrix is open, not closed.

## Licence

MIT. See `LICENSE`.

Third-party attributions that travel with a binary are in [NOTICE](NOTICE).

The openCypher TCK vendored under `crates/engram-tck/` is Apache-2.0, copyright
Neo4j Sweden AB, and is redistributed under its own terms; that crate is not
published. Neo4j and Cypher are trademarks of Neo4j, Inc.; this project is not
affiliated with, endorsed by, or derived from Neo4j. See
[TRADEMARKS.md](TRADEMARKS.md), which also records what this project has *not*
cleared for its own name.
