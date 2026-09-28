# Changelog

All notable changes to Engram. Dates are UTC.

## 0.2.1 — 2026-09-28

A build and CI release. The 0.2.0 tag did not build on its declared minimum
Rust version, and its public CI was red. Engine behaviour is unchanged, and so
is every measured figure.

### Fixed

- **Builds on the declared MSRV, Rust 1.85, again.** `engram-server` and the
  `jsonl2neo4j` converter used `let` chains, which 1.85 does not accept.
- **The API documentation builds under `-D warnings`.** Seven doc links
  pointed at private items or at a function that had been renamed.
- **The simulator's coverage floor holds.** 0.2.0's LOGOFF fix declared the
  event "bolt.logoff rolled back an open transaction" but shipped without the
  simulator scenario that reaches it. That scenario is now included: LOGOFF
  inside an open transaction, then a count showing that the buffered write is
  gone.
- **A test that depended on the scheduler.**
  `an_algorithm_under_a_write_stream_answers_from_one_snapshot` failed on
  hosted runners when every read ran before the writer's first write. The
  writer now starts after the first read, and the reader keeps running until
  it has seen the writes.
- **The harness smoke job** built the benchmark harness but not the server
  binary it starts.
- **Messages with a run of spaces mid-sentence.** 337 wrapped string literals
  in 66 files had lost their line continuations, so the affected log lines and
  errors printed a long gap mid-sentence. Examples include the server's
  memory-ceiling and parallelism notices, the derived-sidecar refusals and the
  harness errors. The continuations are restored.
- A `clippy::type_complexity` error in a test.

## 0.2.0 — 2026-09-28

The release that measured Engram against Neo4j and PostgreSQL on every
LDBC-derived family at two data sizes, and fixed what those runs found.

### Highlights

- **Measured against Neo4j 5.26.31 Community and PostgreSQL 17.11** on LSQB and
  SNB Interactive and BI at SF3 and SF10, FinBench at SF1 and SF10, the LDBC
  Graphalytics kernels on the S-size graphs, and a ten-profile concurrent
  stress test — each engine alone in the same 40-CPU, 140 GiB container. Engram
  answered every query of every family at both sizes, the only one of the three
  engines to do so. The tables, the method and the gaps are in the book's
  [Three engines at SF3 and SF10](docs/book/src/measurements/three-engines-sf3-sf10.md).
  These are not official LDBC results.
- **Graphalytics conformance.** All six kernels validate against LDBC's
  reference output on the ten S-size graphs, 57 of 57 jobs, in the
  specification's conformance mode (`graphalytics: true`).

### Added

- **Graph algorithms** (`engram.algo.*`): PageRank, weakly and strongly
  connected components, degree, BFS and weighted shortest paths, triangle
  count, local clustering coefficient, label propagation, Louvain, exact
  betweenness and closeness, Yen's k-shortest paths and `allShortestPaths`,
  with stream, stats, mutate and write modes over a result cache
  (`engram.algo.result.*`), and the Graphalytics conformance mode.
- **`=~` regular expressions**, evaluated over a finite automaton; the
  constructs that need a backtracker are refused by name.
- **A trigram index** for `=~`, `CONTAINS` and `ENDS WITH`.
- **BM25 scoring** for full-text indexes created from this release on;
  existing indexes keep term-frequency scoring.
- **Standalone `CALL`** returns the procedure's declared output columns.
- **A procedure catalogue** (`engram-proc`) in place of a hard-coded dispatch
  chain.
- **A process-wide morsel budget** (`ENGRAM_PARALLEL_SLOTS`), so concurrent
  statements under `ENGRAM_QUERY_PARALLELISM` cannot multiply into
  clients × width threads.

### Correctness

Wrong answers found by the benchmark runs, each fixed and pinned by a test that
compares against the unoptimised path or another engine:

- **Relationship uniqueness is scoped to the whole `MATCH` clause**, as
  openCypher requires, not to one path. Engram let one relationship be matched
  twice across a clause's comma-separated paths; SNB BI query 17 counted a
  person in two roles Cypher keeps apart. Its complete answer now matches
  PostgreSQL's value for value at SF3 and SF10.
- A `WITH`'s own `WHERE` or `ORDER BY` lost what it read through an alias.
- A columnar stage returned a node leaving at `RETURN` with no properties.
- A frontier walk counted rows for `count(*)` beside `count(DISTINCT end)`.
- An undirected frontier walk re-reached its start through the edge it left by.
- A per-creator date index, cached by commit epoch alone, answered a statement
  about another label.
- A pattern predicate, `EXISTS {}`, a list comprehension or `COUNT {}` over a
  lean-bound seed answered 0: the subquery matcher trusted the bound value's
  label list.
- Column-bound seeds carried only the pattern's labels, so a later `a:Other`
  test read false.

### Security

- **Persisted range-index sidecars are keyed by realm, namespace and
  property.** They were keyed by property alone, so after a restart one tenant
  could be served another tenant's index. Files in the old naming are counted
  and rebuilt, never adopted.
- **`LOGOFF` releases open result streams and the explicit transaction**, as
  `RESET` does; the next principal on the connection can no longer pull or
  commit the previous one's work.
- **The `/* engram:trace */` statement marker is honoured only when the server
  sets `ENGRAM_TRACE_MARKER=1`.** Any client could previously switch on a
  roughly tenfold per-statement tracing cost that also wrote statement text to
  the log.
- **Engine and morsel worker threads run on a stack sized for the deepest
  statement the parser accepts.** Such a statement could overflow a worker and
  abort the whole server before authentication.

### Engine

- **Parallel execution** (behind `ENGRAM_QUERY_PARALLELISM`): a first-stage
  seed set and a stage's continuation are split across the executor; a
  read-only statement streams its prefix ahead of an unstreamable `CALL`;
  aggregation runs on the workers, with `DISTINCT` partials merged by union;
  the executor's calling thread works and helpers start on demand.
- **Planning**: a path bound only in its middle is walked both ways from that
  node; a both-bound path whose end repeats across rows is answered as a join
  at its middle; a MATCH's connected paths are ordered by the rows they add;
  a relationship-property memo; typed date indexes; a grouping finishes only
  the groups a downstream `DISTINCT … LIMIT k` reads; relationship predicates
  and `WHERE` conjuncts prune the walk at their hop.
- **Write path**: a dedicated flusher thread owns the group-commit fsync; the
  WAL is zero-filled ahead and flushed with `fdatasync`; the derived refresh
  copies under the change-log locks and computes outside them; a writing
  `MATCH` binds its start candidates in chunks (`--match-start-chunk`), so a
  statement that deletes nothing no longer holds every candidate in memory.
- **Derived structures on disk**: `CALL engram.checkpoint()` drains —
  refreshes, warms and persists the derived sidecar — so a restart adopts
  instead of rebuilding, when the resident set is below half the memory
  ceiling; one sidecar writer at a time; adoption holds the persist lock and
  records the adopted file's vintage, so a maintenance tick during a slow
  adoption can no longer write a partial file over the whole one; the drain
  is not paced by the growth interval.
- **Graph algorithms**: PageRank, label propagation, the triangle count and
  LCC split across the executor, including on dense graphs below the vertex
  floor; LCC counts each triangle once on a degree-oriented CSR; projections are kept between statements while the
  commit clock is unchanged, with weights read in one column gather;
  `engram.algo.project` builds a weighted projection from rows inside a
  statement; `engram.algo.kshortestpaths`.
- **The row budget** defaults to a quarter of the process's memory ceiling
  rather than a fixed row count.

### Benchmark harness (`crates/engram-bench`)

- The `harness` binary runs LSQB, SNB Interactive, SNB BI, FinBench and the
  stress workload from one statement catalogue with per-family digests, and
  writes one result document per pass carrying its rig, fairness settings and
  catalogue digests; `harness report` compares documents and gates regressions
  (`--baseline`, `--max-regression`, `--reproduce`, `--min-regression-ms`).
- A `graphalytics` runner implementing LDBC's protocol and validation rules.
- Checked-in regression baselines under `measurements/baselines/`, gated by a
  test.
- The frozen LSQB and stress catalogue's prose notes were reworded; its
  statements are unchanged, and the old and new digests are declared
  equivalent, so earlier result documents still compare.

### Documentation

- The book's measurement part now holds only the current comparison; earlier
  measurement pages and the design-history section were removed. Architecture
  and reference pages were brought up to date, and the CLI reference lists
  every flag the server parses.
- This changelog.

### Known issues

- **openCypher TCK: 3,769 of 3,773** evaluated scenarios pass (0.1.0: 3,772).
  Three scenarios expect a bare pattern used as a value — in `RETURN`, `WITH`
  or on the right-hand side of `SET` — to be a syntax error, and Engram accepts
  the query.
- The pipeline's fast operators decline a `MATCH` whose comma-separated paths
  share a relationship type; such statements run on the general path, correctly
  and more slowly.
- Rapid create-then-delete churn at SF10 exhausted memory in the stress test.
- The first writes after an SF10 start take about a second each for roughly the
  first 40 seconds.
- BI 16 and BI 10 at SF10 each have two stable speeds that depend on the
  server's start.

## 0.1.0 — 2026-09-05

The first public snapshot: the engine, its gates, the openCypher TCK harness,
and the documentation book.
