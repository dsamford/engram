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
| Hash-chained commit log, verifiable without a key | **done** |
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
| Morsel-parallel `expand` and count fold | **done**, opt-in behind `ENGRAM_QUERY_PARALLELISM` |
| Frontier-BFS variable-length expansion with a visited set | **done** (`expand_var_length_bfs`) — admitted for `min == 1`, no rel/path variable, no rel-property test, `DISTINCT`-only end |
| BFS `shortestPath` with a visited-node set | **done** (`try_shortest_path_bfs`) — bidirectional for unbounded `*`, memoised forward tree for `*..max`; handles the bound-endpoint shape, others fall back |
| **Widening what those operators admit** | the remaining work — every unadmitted shape takes the enumerating path |
| Columnar id vectors with a selection vector | **done** — the pipeline's `DataChunk` |
| Predicate pushdown into expansion, top-k early termination | planned |
| Factorized intermediates for projection | planned — counting already factorizes via count-fold weights |
| Prepared-plan cache for the short-query floor | planned |
| Incrementally-maintained statistics and sketches | planned |
| Worst-case-optimal joins | **deliberately deferred** — WCOJ wins on cyclic patterns and loses on acyclic ones, which is the shape that dominates here |
| Full JIT compilation | **deliberately deferred** |
| A learned cost model as the primary optimizer | **deliberately deferred** |
| Distributed / multi-node execution | **out of scope** — single node, fastest first |

The deferrals are as much a part of the roadmap as the plans. Three of them are
techniques a reader might reasonably expect and which were evaluated and
rejected for this workload.

## Query language

| item | status |
|---|---|
| `=~` regular expressions | refused at evaluation; adding a regex dependency is an open decision, since the engine is currently zero-dependency on this axis |
| BM25 scoring for full-text | not started — scoring is term frequency today |
| `UNION` inside `CALL { }` | not started |
| Standalone `CALL` (no `YIELD`/`RETURN`) | not started |
| Configurable vector index options (`dimensions`, `similarity_function`) | parsed and uninterpreted today |
| Native graph algorithms — PageRank, betweenness, community detection | not started; only `shortestPath` exists |
| Driver-introspection procedures (`db.schema.*`, `SHOW INDEXES`) | partial |

## Protocol and compatibility

| item | status |
|---|---|
| Bolt 5.x | **done** — versions 5.8.8 and 6.0.0 are offered at handshake |
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
| Nine `ServerConfig` fields reachable only when embedding | a gap in the CLI, not a design position — see [ServerConfig](./reference/server-config.md) |

## What this page is not

It is not a schedule, and it is not a promise. The project's own convention is
that a claim should be checkable, so where this page says *done* you should be
able to find the page or the gate that demonstrates it, and where it says
*primitive exists* you should read that as "the hard part is built and the
product around it is not".

If an item here is load-bearing for you, the useful question is not when it will
land but what evidence would show it had.
