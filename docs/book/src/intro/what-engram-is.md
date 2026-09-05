# What Engram is

Engram is a **property-graph database**: it stores nodes and relationships, each
carrying labels and properties, and answers questions about how they connect.
You talk to it in **openCypher**, over the **Bolt** wire protocol, using the
same drivers you would point at Neo4j.

It runs as **one process, one shard**. There is no cluster, no coordinator and
no replica set. A single `engram-server` owns its data directory and serves
every connection.

## What it is good at

- **Graph traversal.** Multi-hop patterns, variable-length paths, and
  aggregations over neighbourhoods are what the engine is built and measured
  on. On the analytical LSQB battery at SF1 it leads Neo4j 5.26-community on
  all nine queries, and on a ten-profile mixed read/write sweep on all twenty
  levels — see [Measurements](../measurements/index.md), which also states what
  those numbers do not cover.
- **Writes.** The storage engine is an append-only MVCC log with adjacency held
  as a derived structure, so relationship creation, hub writes and delete churn
  are where the largest margins are.
- **Standard Cypher.** 3,771 of 3,772 evaluated openCypher TCK scenarios pass,
  and the number is ratcheted in CI so it cannot quietly fall. See
  [Cypher support](../using/cypher-support.md).
- **Graphs larger than memory.** [Paged mode](../architecture/paged-mode.md)
  reads sealed segments block-by-block through a bounded cache, so the working
  set rather than the corpus sets the memory floor.
- **Reproducibility.** The engine crates never read a clock or spawn a thread —
  the adapter injects what they need, and a lint forbids the alternative — so a
  seeded run reproduces exactly, byte for byte, across two processes. A failure
  is a repro, not a story about a bad afternoon.
- **Vector and full-text search** in-process, with no external index to keep in
  sync. See [Indexes](../architecture/indexes.md).

## What it is not

This section is deliberately longer than the one above.

- **Not secured.** No authentication — credentials are accepted and *not
  verified*. No TLS — traffic is plaintext. See
  [Security posture](../using/security.md).
- **Not distributed.** No clustering, no replication, no failover. One process
  is the availability story.
- **Not backed up.** There is no backup or point-in-time-restore tooling. The
  WAL and the segments are the durable artifacts, and there is no supported
  procedure around them yet.
- **Not audited.** Authentication attempts, session lifecycle, DDL and
  authorization denials are not recorded anywhere.
- **Not a Neo4j drop-in**, despite the wire compatibility. The driver
  compatibility matrix is open, not closed. Some Cypher is refused — see
  [Known limits](../known-limits.md).

If any of those absences is disqualifying for your use, it is better to know now
than after a migration. They are a release state rather than a set of design
positions — [Roadmap](../roadmap.md) says which are the next milestone (auth,
authorization and TLS are), which have primitives already built (replication and
restore do), and which were evaluated and deliberately set aside.

## How it compares to what you may know

| if you know | the useful difference |
|---|---|
| **Neo4j** | same wire protocol and query language; one process instead of a cluster; no auth/TLS yet; a different storage engine (MVCC + WAL + sealed segments) |
| **SQLite** | a comparable "one process, one file-ish, embedded-feeling" posture, but a graph model and a network protocol |
| **RocksDB / LSM stores** | Engram's storage layer *is* an MVCC LSM — a sharded memtable, sealed immutable segments, size-tiered compaction — with a graph model on top |
| **An in-memory graph library** | Engram persists, is transactional, and survives restart; it is not a library you link against, it is a server you connect to |

## The three ideas worth knowing early

You do not need these to use Engram, but they explain nearly every design
choice you will meet. Each is covered properly in
[The three decisions](../architecture/three-decisions.md).

1. **Everything ambient is injected.** The engine never reads a clock, spawns a
   thread, or generates a random number directly. On the serving path a lint
   forbids it and the adapter hands in what is needed; in the simulation lane a
   `Runtime` trait supplies all four over a virtual clock. That is what makes
   the simulation real rather than theatre.
2. **Execution within a statement is single-threaded by default**, and
   parallelism is something you install rather than something the engine
   assumes. The server runs `--workers N` engine threads over a shared
   MVCC store, and morsel parallelism inside a statement is opt-in — so the
   deterministic lane can run the same code with parallelism absent.
3. **Every subsystem declares what it can do.** Crash points, `sometimes!`
   events and counters are *registered*, so a fault that never fires is a
   measurable absence rather than an absence of evidence.

## Next

[Getting started](./getting-started.md) — build it, run it, connect to it.
