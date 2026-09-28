# Roadmap

[Known limits](./known-limits.md) states what is absent. This page states what
is *planned*, so that the absences read as a release state rather than as
design positions.

**How to read it.** These are directions with designs behind them, not dated
commitments. Where an item says *primitives exist*, that is a claim about code
in this tree, and the relevant page says which. Where it says *not started*,
nothing has been built and the design may still change.

## Security — the named next milestone

The security absences are the ones most likely to be mistaken for permanent, so
they are first.

| item | status |
|---|---|
| **Authentication (OIDC)** | the next milestone |
| **Namespace-aware authorization** | the next milestone |
| **TLS** | the next milestone |
| Audit log — session lifecycle, DDL, authorization denials | not started |
| Rate limiting | not started |
| Key management, capability-typed executor, typed leakage classes | **partial — seam only.** The at-rest AEAD envelope and the protected-KIND gate exist; the key-management machinery does not |
| Searchable-encryption tiers | not started |

Until authentication and TLS land, **the deployment boundary is the only
boundary**, and [Security posture](./using/security.md) says so at length. What
*is* already hardened — bounded message size, bounded nesting on the wire and in
the parser, connection caps, timeouts, backpressure, per-session panic
containment — is enumerated there rather than summarised, because "we hardened
it" is not a claim anyone should accept unenumerated.

## Availability and durability

This is the area where the gap between *primitives* and *product* is widest,
and worth stating precisely.

| item | status |
|---|---|
| Write-ahead log, `fsync`-before-ack, replay on restart | **done** — see [Durability and recovery](./using/durability.md) |
| Hash-chained commit log, verifiable without a key | **done** — see [The commit log](./architecture/commit-log.md) |
| Replica that verifies as it consumes | **primitive exists** (`engram-store::replica`) |
| Point-in-time restore, restore verification | **primitives exist** — `recover_to`, `verify_restore` |
| Change-data-capture feed | **primitive exists** — the log tail; relays, predicates and delivery do not |
| A second node, log shipping end to end | **not exercised** |
| Clustering, failover, leader election | **not started** |
| Backup and restore *tooling* | **not started** — the WAL and segments are the durable artifacts, but there is no supported procedure around them |

The replica primitive is worth one sentence of detail because it sets the
standard the rest has to meet: `Replica::apply` recomputes the hash chain entry
by entry against its **own** head, so a tampered entry, a fork or a gap refuses
at the entry that broke, with its sequence named. A future sequence is a gap and
is never skipped, because "skip the hole and keep going" is how a replica
silently diverges while reporting healthy. Restore verification is independent
of the writer: the push's own account of itself is never the evidence.

So the honest summary is: **the integrity machinery for replication exists and
the distribution machinery does not.**

## Execution engine

The [architectural comparison](./intro/how-its-different.md) explains why these
are the items that matter. The framework — a columnar `DataChunk` pipeline and
an injected morsel-parallelism seam — is in place; these are operators.

| item | status |
|---|---|
| Morsel-parallel `expand`, count fold and algorithm fixpoint | **done** — all three opt-in behind `ENGRAM_QUERY_PARALLELISM`, and byte-identical to their serial paths (`crates/engram-graph/tests/parallel_fold_is_byte_identical.rs`, `parallel_expand.rs`); see [Concurrency](./architecture/concurrency.md). The fold carries its own driving-row floor of 2 against `expand`'s 256, because a fold's driving row is a whole nested walk rather than a cheap probe |
| A bounded, process-wide morsel budget | **done** — `ENGRAM_PARALLEL_SLOTS`, defaulting to the width. It bounds the *amplification* — C clients must not become C x width workers — not the worker count, and a statement granted nothing runs serially rather than queueing. It exists because a fixed per-statement width scaled negatively with the client count — C clients at width W spawned up to C x W workers against a fixed CPU quota. The properties the throughput numbers cannot pin are unit-tested beside the pool in `engram-server/src/lib.rs` — that a statement granted nothing still visits every morsel, and that slots are returned even when a body panics |
| Frontier-BFS variable-length expansion with a visited set | **done** (`expand_var_length_bfs`) — admitted for a lone bounded hop (`min == 1` **and** a stated maximum, nothing else in the path), no rel/path variable, no rel-property test, `DISTINCT`-only end |
| BFS `shortestPath` with a visited-node set | **done** (`try_shortest_path_bfs`) — bidirectional for unbounded `*`, memoised forward tree for `*..max`; handles the bound-endpoint shape, others fall back |
| **Widening what those operators admit** | the remaining work — every unadmitted shape takes the enumerating path |
| Columnar id vectors with a selection vector | **done** — the pipeline's `DataChunk`, see [The query path](./architecture/query-path.md) |
| Predicate pushdown into expansion, top-k early termination | **done** — relationship predicates and `WHERE` conjuncts are applied at their hop rather than after the walk (`a_where_conjunct_prunes_the_walk_at_its_hop.rs`; lever `--no-rel-predicate-pushdown`), and `ORDER BY … LIMIT` keeps a bounded heap, including before projection and over an index (`agg_topk_before_projection_is_byte_identical.rs`, `pipeline_ic9_index_topk.rs`) |
| The clause-wide relationship rule in the fast operators | **next** — the multi-path chain, semijoin and count fold implement the separate-clause rule, so a `MATCH` whose comma-separated paths share a relationship type runs on the general path. The design: drop the engine's hidden uniqueness conjuncts, carry the used-relationship set across one clause's comma paths, keep fusion from merging clauses that carry them, and let the fold decline until it can price the cross-path correction |
| Per-row interpretation cost on short statements | open — what Engram's losses to other engines have in common (BI 5, Interactive IS3, FinBench's transfer paths): no hot spot, a cost per row per stage boundary |
| Factorized intermediates for projection | planned — counting already factorizes via count-fold weights |
| Prepared-plan cache for the short-query floor | planned |
| Incrementally-maintained statistics and sketches | planned |
| Columnar batch execution of a hop's bindings against a batched adjacency reader | **the largest untested idea** — an architectural change to the fold rather than an optimisation of it |
| Worst-case-optimal joins | **deliberately deferred** — WCOJ wins on cyclic patterns and loses on acyclic ones, which is the shape that dominates here |
| Full JIT compilation | **deliberately deferred** |
| A learned cost model as the primary optimizer | **deliberately deferred** |
| Distributed / multi-node execution | **out of scope** — single node, fastest first |

The deferrals are as much a part of the roadmap as the plans. Three of them —
worst-case-optimal joins, full JIT and a learned cost model — are techniques a
reader might reasonably expect, and each was evaluated and rejected for this
workload. Distributed execution is a different case: out of scope, not
deferred.

### Measured and removed

Four approaches to the fold's memory-access cost were built in this tree,
measured against an A/B on one binary, and taken back out.

| approach | why it was removed |
|---|---|
| An adjacency-memo last-match hint | No measurable effect across interleaved rounds on a clean base, with arm identity checked in the binaries. The direction was the finding: it helped the query it targeted and slowed two others, the nested-recursion cost that was predicted. Testing the target alone would have shown a win and hidden two regressions |
| Sorted fold bindings | Substantially slower, with disjoint distributions. The outer chunk is columnar and read by row index, so sorting the row order to make one CSR sequential randomises reads across every other outer column at once. The design priced the locality it bought and not the locality it spent |
| A fold lookahead, or warming read | No measurable effect at any lookahead distance tried. The safe form performs the full index descent rather than issuing a hint, so it repeats roughly the work of the miss it warms |
| Prefetch intrinsics | **Refused on architecture, not deferred.** `_mm_prefetch` is `unsafe` and `engram-graph` is `#![forbid(unsafe_code)]`. Lifting a crate-wide safety invariant for a speculative optimisation is the worse trade |

They are listed because a rejected change that is not written down gets
re-proposed. Anyone picking up the fold's memory-access cost should start at
columnar batch execution and should not re-run reordering, prefetching or
warming.

## Query language

| item | status |
|---|---|
| `=~` regular expressions | **done** — over the `regex` crate, wrapped for Cypher full-match semantics; the constructs a finite automaton cannot run are refused by name, see [Regular expressions](./reference/regex.md) |
| BM25 scoring for full-text | **done** — over a maintained term index; per-index and stamped at create, so an existing index keeps term frequency. See [Indexes](./architecture/indexes.md); the gate is `crates/engram-graph/tests/a_bm25_scored_index_ranks_a_rare_term_above_a_common_one.rs`. WAND top-k not built: the procedure takes no `k` |
| `UNION` inside `CALL { }` | not started |
| Standalone `CALL` (no `YIELD`/`RETURN`) | **done** (`procedure_result_columns`) — a `CALL` that ends a query returns its declared output columns, per openCypher. `YIELD` is still required when the `CALL` is not the last clause. The gate is the vendored TCK's `clauses/call/` features, 41 scenarios |
| Configurable vector index options (`dimensions`, `similarity_function`) | parsed and uninterpreted today |
| Native graph algorithms — PageRank, betweenness, community detection | **done** for PageRank, WCC, degree, BFS/SSSP, triangle count, local clustering, label propagation, Louvain, strongly connected components, exact betweenness and closeness in four modes, plus Yen's k-shortest paths and `allShortestPaths` — see [Graph algorithms](./architecture/graph-algorithms.md). Sampled betweenness, eigenvector centrality, ArticleRank, A\* and the similarity family not started; results are not persisted and nothing feeds the optimiser |
| Trigram index for `=~`, `CONTAINS`, `ENDS WITH` | **done** — see [Trigram index](./reference/trigram-index.md); not persisted, and built lazily on the first query that needs it rather than at startup, so that query pays for the build |
| Driver-introspection procedures (`db.schema.*`, `SHOW INDEXES`) | partial — `db.awaitIndexes` added; the procedure surface is now a catalogue (`engram-proc`) rather than a hard-coded chain |

## Protocol and compatibility

| item | status |
|---|---|
| Bolt 5.x | **done** — versions 5.8.8 and 6.0.0 are offered at handshake (`OFFERED`, `engram-bolt/src/server.rs`); see [Bolt and PackStream](./reference/bolt.md) |
| Bolt 6.x server semantics | not started |
| A closed driver compatibility matrix | open — see [Connecting](./using/connecting.md) |

## Multi-tenancy and federation

The key encoding already carries realm and namespace in every key, so tenancy
is present in the *layout* well ahead of the features that use it.

| item | status |
|---|---|
| Realm/namespace key prefix, tenant-local scans by construction | **done** — see [Key encoding](./architecture/key-encoding.md) |
| Per-tenant key derivation from the key prefix | **seam exists** |
| Namespace-qualified node references | **not started — the key architectural change.** Node ids are per-namespace, so ids collide across namespaces; cross-namespace work needs `(namespace, id)` threaded through the interpreter, adjacency and results |
| Overlay reads across a tenant and a shared namespace | not started; depends on the above |
| Cross-namespace traversal and vector search | not started; depends on the above |

## Operations

| item | status |
|---|---|
| A configuration file | not started — configuration is CLI flags and environment variables |
| Versioned, ordered, verified schema migrations | not started |
| Online constraint-build ladder | not started — single-shard atomicity substitutes today |
| Prometheus / OpenTelemetry export | not started — observability is stderr counters and the trace, see [Counters and observability](./reference/observability.md) |
| Nine `ServerConfig` fields have no flag of their own; seven have no route from the binary at all | a gap in the CLI, not a design position. The other two are set by the binary without a flag — `configure_graph` under `--bulk-ingest`, `paged_spill_cache` under `--paged-dir`. See [ServerConfig](./reference/server-config.md) |

## What this page is not

It is not a schedule, and it is not a promise. The project's own convention is
that a claim should be checkable, so where this page says *done* you should be
able to find the page or the gate that demonstrates it, and where it says
*primitive exists* you should read that as "the hard part is built and the
product around it is not".

If an item here is load-bearing for you, the useful question is not when it will
land but what evidence would show it had.
