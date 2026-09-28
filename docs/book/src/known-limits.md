# Known limits

Stated as **absences**, because a list of features implies the rest exist.

Read this page before you rely on anything here. It is the honest list, and it
is deliberately the easiest page in the book to find.

**Read it alongside the [Roadmap](./roadmap.md).** Almost everything below is a
release state with a design behind it rather than a design position — the
security absences in particular are the named next milestone. Where the two
pages disagree, this one is about today and that one is about intent.

## Security

- **No authentication.** Credentials are accepted and *not verified*. Any
  username and password — including none — opens a session with full access.
- **No TLS.** All traffic is plaintext, including those unverified credentials.
- **No audit log.** Authentication attempts, session lifecycle, DDL and
  authorization denials are not recorded anywhere.
- **No authorization model.** There are no users, roles or grants to configure.

The practical consequence: an Engram port is an unauthenticated database. Bind
it to a loopback interface or an isolated network segment and nothing else. See
[Security posture](./using/security.md).

## Availability and operations

- **No clustering, no failover.** One process. The availability story is
  "restart it".
- **No replication in service**, though the *primitives* exist and are not
  toys: a replica that recomputes the hash chain against its own head and
  refuses at the entry that broke, restore verification independent of the
  writer, and a log tail for change data capture. What does not exist is a
  second node, log shipping exercised end to end, or any of the machinery
  around them. See [Roadmap](./roadmap.md).
- **No backup or point-in-time-restore tooling.** The write-ahead log and the
  segments are the durable artifacts, and `recover_to` / `verify_restore`
  exist as primitives, but there is no supported procedure around them — no
  `BACKUP`, no snapshot command, no documented restore path beyond copying
  files while the server is stopped.
- **No online schema migration tooling.**
- **No configuration file.** Everything is CLI flags and environment
  variables; a config file is deferred work.

## Durability, per mode

The mode you choose changes what a crash costs. This catches people, so it is
here as well as in [Durability and recovery](./using/durability.md).

| mode | flag | what a crash loses |
|---|---|---|
| In-memory | *(default)* | **everything** — announced loudly at startup |
| WAL-durable | `--data-dir DIR` | nothing acknowledged; the log is `fsync`ed before the ack |
| Paged | `--paged-dir DIR` | nothing acknowledged; `DIR/engram.wal` fronts the unsealed tail and is `fsync`ed before the ack |
| Bulk ingest | `--bulk-ingest` | **everything since the load began** — durability is by re-ingest, not replay |

Paged mode is the bigger-than-RAM serving mode *and* a durable one. Capacity and
durability used to be two modes and are not any more: `--paged-dir DIR` opens
`DIR/engram.wal` unconditionally — there is no flag that selects it and none
that turns it off — and replays it into the tail on open, so a crash costs
nothing that was acknowledged. That is `Store::open_paged_dir_with_wal`, and it
is exercised by `crates/engram-store/tests/paged_wal.rs` and by
`acknowledged_writes_survive_kill_9_in_paged_mode` in
`crates/engram-server/tests/durability.rs`. Paged mode cannot be combined with
`--data-dir` — two on-disk layouts, one durability contract — and the server
refuses at startup if you try.

What the paged log does **not** hold is the whole history. A spill checkpoints
it behind the segments that spill wrote, so the file carries the unsealed tail
and nothing before it. Replay after a crash is covered; replay to an arbitrary
earlier point is not, and neither is any of the backup tooling absent above.

One combination neither refusal covers: `--bulk-ingest` **is** accepted
alongside `--paged-dir` — only `--data-dir` refuses it. Bulk writes go through
`put_unlogged`, which never reaches the log, so paged plus bulk ingest falls
back to seal-boundary durability and a crash loses everything written since the
last spill.

## Cypher

- **`=~` (regex) evaluates, over a finite automaton.** The engine cannot
  backtrack, so no pattern can hang the server; what it cannot run it **refuses
  by name** rather than reinterpreting — backreferences, lookaround, atomic
  groups, possessive quantifiers, `\Q…\E`. Those are exactly the constructs
  that force a backtracker. Capture groups parse and are inert, because `=~` is
  a full-match boolean. `(?i)` uses Unicode *simple* case folding, so it folds
  `K` to `k` and does not fold `ß` to `ss`. A pattern compiling to more than
  1 MiB of automaton is refused at compile time. See
  [Regular expressions](./reference/regex.md).
- **`UNION` inside `CALL { }`** is refused.
- **A trigram index is not persisted.** It is rebuilt when a query first needs
  it, rather than loaded from disk as a range index can be. It also covers one
  label and one property, indexes nodes only, and is disabled outright by a
  single non-string value under the indexed property — see
  [Trigram index](./reference/trigram-index.md).
- **Full-text search is BM25 for indexes created from now on, and term
  frequency for every index created before.** Scoring is recorded in the
  index's catalogue row and stamped when it is created, so an upgrade never
  re-ranks an existing index; `SHOW INDEXES` does not yet report which. `k1`
  and `b` are Lucene's 1.2 and 0.75 and are not configurable. The analyzer
  still splits on non-alphanumerics and lowercases: **no stemming, no
  stopwords, no synonyms, no phrase or proximity queries, no field boosts, no
  fuzzy matching, and no configurable analyzer.** Scores are per field and
  summed, which is what Lucene does for a multi-field query.
- **Graph algorithm results are not persisted and are never auto-refreshed.**
  A `mutate` result does not survive a restart, and it keeps describing the
  snapshot it was computed at until it is overwritten or dropped — `stale` says
  so rather than the engine recomputing behind a read. Every run recomputes;
  there is no incremental maintenance, and nothing feeds the query optimiser.
  Approximate (sampled) betweenness, eigenvector centrality, ArticleRank, A\*
  and the similarity family are absent; exact betweenness, closeness, strongly
  connected components, Yen's k-shortest paths and `allShortestPaths` are
  present.
  See [Graph algorithms](./architecture/graph-algorithms.md).
- **Vector index `OPTIONS` are parsed and uninterpreted.** You cannot set
  `vector.dimensions` or `vector.similarity_function`; the dimension is
  inferred from the data and the metric is cosine.
- **Not a Neo4j drop-in**, despite the wire compatibility. The driver
  compatibility matrix is open, not closed — see [Connecting](./using/connecting.md).

The conformance number is the useful counterweight to this list: 3,769 of 3,773
evaluated openCypher TCK scenarios pass. Of the four failures, three are
scenarios in which the TCK expects a bare pattern used as a value — in a
`RETURN` or `WITH` projection, or on the right-hand side of a `SET` — to be
refused as a syntax error, and Engram accepts the query instead; the fourth is
a time-zone database expectation where this engine is arguably the more correct
of the two. Broad coverage and specific sharp edges are both true.

## Scale

- **One statement runs on one thread** by default. Morsel-parallel operators,
  the count fold, seed and continuation splitting and the graph-algorithm
  kernels all exist and are byte-identical to their serial paths, but all of
  them are opt-in behind `ENGRAM_QUERY_PARALLELISM` — see
  [Environment variables](./reference/environment.md). There is no CLI flag for
  it. Every Engram figure on the [comparison page](./measurements/three-engines-sf3-sf10.md)
  ran with it set to 40. The *server* is multi-threaded independently of any of
  this — `--workers N` engine threads over a shared MVCC store — but `N`
  defaults to 1, so a default install is single-threaded on both axes.
- **Setting `ENGRAM_QUERY_PARALLELISM` does not guarantee parallelism per
  statement.** The morsel budget is process-wide rather than per-statement and
  defaults to the configured width, so one analytical statement gets the full
  width while a second concurrent one finds the pool empty and runs serially
  instead of contending for the same cores. It degrades; it never queues.
  `ENGRAM_PARALLEL_SLOTS` sets the budget explicitly. What it bounds is the
  amplification — C clients must not become C x width workers — not the worker
  count.
- **Results are fully materialised before they are paged to the client.** The
  driver's fetch size does not bound server memory; `--row-budget` does. See
  [Result paging](./using/result-paging.md).
- **`--row-budget` defaults to a quarter of the process's memory ceiling**, at
  an assumed 96 bytes a row — 391,468,373 rows in a 140 GiB container. A query
  that would exceed it is refused rather than allowed to reach the OOM killer,
  which refuses nothing and takes every other session with it. `--row-budget 0`
  removes the bound; any other value is used as given.
- **The fast operators have admission conditions.** Frontier-BFS
  variable-length expansion needs a lone bounded hop — `min == 1`, a stated
  maximum, and nothing else in the path — with no relationship or path
  variable, no relationship-property test, and a `DISTINCT`-only end; BFS
  `shortestPath` needs both endpoints bound and a single hop. The pipeline's
  multi-path chain, semijoin and count fold also decline a `MATCH` whose
  comma-separated paths share a relationship type, because Cypher keeps those
  relationships distinct across the whole clause and those operators implement
  the rule for separate clauses; such a statement runs on the general path,
  which is correct but slower (BI 14 is the measured case). A statement outside
  these conditions takes the enumerating path. Widening admission is active
  work — see [Roadmap](./roadmap.md).
- **Performance is measured at SF3 and SF10** (FinBench at SF1 and SF10). SF30
  and SF100 have not been run, and the margins are not scale-free: from SF3 to
  SF10, 3.3 times the data, Engram's total time over the BI queries every
  engine answered grew 4.0 times, against Neo4j's 3.5 and PostgreSQL's 3.8, and
  its read throughput at 32 clients fell to about half its SF3 rate. See
  [Three engines at SF3 and SF10](./measurements/three-engines-sf3-sf10.md).
- **Other engines lead in specific places.** Neo4j is faster on a handful of
  short lookups where its per-query work is leaner (BI 5, Interactive IS3 and
  IC7, and IC8 at SF10), on FinBench's transfer-path queries at SF10, and on
  the half-read, half-write "balanced" stress mix at 32 clients and above.
  PostgreSQL is faster at plain inserts, on the heavy BI joins LDBC's SQL is
  tuned for (BI 8, 9, 11, 12, 15 and 17), on the two Interactive aggregations
  over a person's two-step network (IC5, IC6), and on FinBench's transfer
  paths. The common thread on Engram's side is per-row interpretation cost on
  short statements, not traversal.
- **Some figures depend on the server's start.** BI 16 and BI 10 at SF10 each
  have two stable speeds that depend on the start, on every Engram build
  measured; the cause is not yet found.
- **Rapid create-then-delete churn at SF10 exhausted memory.** In the stress
  test the server was killed after its 8-client churn level had completed and
  reconciled: the relationship check the harness runs between levels scans
  every relationship while the churn keeps the typed index stale, and the
  memory it used was not given back.
- **The first writes after an SF10 start are slow**, about a second each for
  roughly the first 40 seconds; the cause is not yet found.

## What is built but not reachable

Four crates are compiled, tested and simulated, but nothing on the Bolt serving
path calls them today: `engram-crypto` (per-tenant keys, AEAD sealing),
`engram-objstore` (the object-storage seam), `engram-blob` (large media
referencing) and `engram-exec` (the bitmap operator seam). They exist because
the decisions they encode are the ones that cannot be retrofitted — see
[Seams not yet on the serving path](./architecture/seams.md). Do not read their
presence as a shipped feature.

## If one of these blocks you

That is useful information, and it is better arrived at from this page than
from a migration. The gaps above are gaps, not disguised design positions —
most have a design written down, and
[Roadmap](./roadmap.md) says which are planned, which are primitives awaiting a
product, and which were evaluated and deliberately deferred.
