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
- [Graph algorithms](./architecture/graph-algorithms.md)
- [Seams not yet on the serving path](./architecture/seams.md)

# Reference

- [Server CLI](./reference/cli.md)
- [ServerConfig](./reference/server-config.md)
- [Tuning guide](./reference/tuning.md)
- [Compiled-in constants](./reference/constants.md)
- [Environment variables](./reference/environment.md)
- [Cypher procedures](./reference/procedures.md)
- [Regular expressions](./reference/regex.md)
- [Trigram index](./reference/trigram-index.md)
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

# Measurements

- [How Engram is measured](./measurements/index.md)
- [Three engines at SF3 and SF10](./measurements/three-engines-sf3-sf10.md)

---

[Known limits](./known-limits.md)
[Roadmap](./roadmap.md)
[Licence and trademarks](./licence.md)
