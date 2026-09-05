# Summary

[Engram](./introduction.md)

# Introduction

- [What Engram is](./intro/what-engram-is.md)
- [Getting started](./intro/getting-started.md)
- [Your first graph](./intro/first-graph.md)
- [Core concepts](./intro/core-concepts.md)
- [How Engram is different](./intro/how-its-different.md)

# Using Engram

- [Connecting](./using/connecting.md)
- [Cypher support](./using/cypher-support.md)
- [Schema, indexes and constraints](./using/schema.md)
- [Transactions and isolation](./using/transactions.md)
- [Durability and recovery](./using/durability.md)
- [Result paging](./using/result-paging.md)
- [Loading data at scale](./using/bulk-loading.md)
- [Operations](./using/operations.md)
- [Security posture](./using/security.md)

# Architecture

- [Architecture overview](./architecture/overview.md)
- [The three decisions](./architecture/three-decisions.md)
- [Crate map and dependency rules](./architecture/crate-map.md)
- [Request lifecycle](./architecture/request-lifecycle.md)
- [The query path](./architecture/query-path.md)
- [The planner](./architecture/planner.md)
- [The write path](./architecture/write-path.md)
- [The storage engine](./architecture/storage-engine.md)
- [Paged mode](./architecture/paged-mode.md)
- [Derived structures](./architecture/derived-structures.md)
- [Key encoding and on-disk formats](./architecture/key-encoding.md)
- [The commit log](./architecture/commit-log.md)
- [Indexes](./architecture/indexes.md)
- [Concurrency and the worker model](./architecture/concurrency.md)
- [Seams not yet on the serving path](./architecture/seams.md)

# Reference

- [Server CLI](./reference/cli.md)
- [ServerConfig](./reference/server-config.md)
- [Tuning guide](./reference/tuning.md)
- [Compiled-in constants](./reference/constants.md)
- [Environment variables](./reference/environment.md)
- [Cypher procedures](./reference/procedures.md)
- [Errors](./reference/errors.md)
- [Bolt and PackStream](./reference/bolt.md)
- [Counters and observability](./reference/observability.md)

# Development

- [Building from source](./development/building.md)
- [The gates](./development/gates.md)
- [Testing](./development/testing.md)
- [Deterministic simulation](./development/simulation.md)
- [Benchmarking](./development/benchmarking.md)
- [Contributing](./development/contributing.md)
- [Architecture decision records](./development/adr.md)
  - [ADR-001 — On-disk formats](./development/adr-001-on-disk-formats.md)

# Design history

- [About these documents](./history/index.md)
- [Engine redesign](./history/engine-redesign.md)
- [Execution engine evaluation](./history/execution-engine-evaluation.md)
- [Query planning](./history/query-planning.md)
- [Tail remediation plan](./history/remediation-plan.md)
- [Concurrency direction](./history/concurrency-direction.md)
- [Derived structures](./history/derived-structures.md)
- [The derived-refresh write tax](./history/derived-refresh-write-tax.md)
- [Write path, phase 0](./history/write-path-phase0.md)
- [Write concurrency ceiling](./history/write-concurrency-ceiling.md)
- [RC1 — guard-row exemption](./history/rc1-guard-exemption.md)
- [RC2 — the sealed prefix](./history/rc2-sealed-prefix.md)
- [Scale and integrity plan](./history/scale-and-integrity-plan.md)
- [LSQB completeness](./history/lsqb-completeness.md)
- [Relationship-create parity](./history/rel-create-parity.md)
- [openCypher conformance strategy](./history/conformance-strategy.md)
- [The SNB proving ground](./history/snb-benchmark.md)
- [Leave nothing on the floor](./history/leave-nothing-on-the-floor.md)

# Measurements

- [How Engram is measured](./measurements/index.md)
- [Neo4j head to head](./measurements/neo4j-head-to-head.md)
- [LDBC SNB stress](./measurements/ldbc-snb-stress.md)
- [Official SF1, paged](./measurements/official-sf1-paged.md)
- [The port benchmark](./measurements/port-benchmark.md)
- [Decoded-value parity](./measurements/decoded-values.md)

---

[Known limits](./known-limits.md)
[Roadmap](./roadmap.md)
[Licence and trademarks](./licence.md)
