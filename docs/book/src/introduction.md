# Engram

A property-graph database in Rust, speaking openCypher over the Bolt protocol.

> **Pre-release.** There is no authentication and no TLS. Do not expose an
> Engram server to a network you do not control. [Known limits](./known-limits.md)
> is a list of *absences*, written to be read before you rely on anything.

Engram is a single-process graph engine with its own Cypher parser and planner,
MVCC storage over a write-ahead log, paged segments that read block-by-block so
a graph can exceed RAM, vector indexes, graph algorithms as `engram.algo.*`
procedures, and a Bolt listener (protocol 5.0–5.8 and 6.0) that stock Neo4j
drivers connect to.

|  |  |
|---|---|
| **Measured standing** | against Neo4j 5.26 Community and PostgreSQL 17 on LSQB, SNB BI and SNB Interactive at SF3 and SF10 and FinBench at SF1 and SF10: the only one of the three to answer every query, faster than Neo4j on **103 of the 119** queries both answered and faster than PostgreSQL on **90 of 126**. PostgreSQL stays ahead on a set of heavy analytical joins and on FinBench's transfer-path queries |
| **openCypher conformance** | 3,769 of 3,773 evaluated TCK scenarios (99.9%), CI-ratcheted |
| **`unsafe` code** | none — the workspace denies it outright |
| **Third-party crates** | 44, every one permissively licensed, no copyleft |
| **Reproducibility** | two processes, one seed, one identical trace digest — enforced as a gate |

The performance line is the one most worth checking rather than believing:
[Three engines at SF3 and SF10](./measurements/three-engines-sf3-sf10.md)
carries the per-query tables, the rig and the protocol, and — at equal
prominence — what they do not say. These are not official LDBC results; that
page says what that means.

## Where to start

**New here?** [What Engram is](./intro/what-engram-is.md) sets expectations in
about five minutes, then [Getting started](./intro/getting-started.md) has a
server running and a driver connected.

**Evaluating it?** Read [Known limits](./known-limits.md) first — it is the
honest list — then [Cypher support](./using/cypher-support.md) and
[Durability and recovery](./using/durability.md).

**Running it?** [Operations](./using/operations.md), the
[Server CLI reference](./reference/cli.md), and the
[Tuning guide](./reference/tuning.md), which is organised by the symptom you
have rather than by the flag you might want.

**Reading the source?** [Architecture overview](./architecture/overview.md)
is the map, and [The three decisions](./architecture/three-decisions.md)
explains why the code looks the way it does. The generated API documentation
lives at [`/api`](../api/engram_graph/index.html).

## The shape of this book

The book runs from newcomer to expert in order, and each part stands alone:

| part | for |
|---|---|
| [Introduction](./intro/what-engram-is.md) | first contact — concepts, a running server, a first query |
| [Using Engram](./using/connecting.md) | connecting, Cypher, schema, transactions, durability, loading, operating |
| [Architecture](./architecture/overview.md) | how it works inside, with diagrams |
| [Reference](./reference/cli.md) | every flag, field, constant, variable, procedure and error |
| [Development](./development/building.md) | building, the gates, testing, simulation, benchmarking |
| [Measurements](./measurements/index.md) | how performance is measured, and the current comparison with its rig, data sizes and protocol |

## A note on how this project states things

Engram's documentation tries to state absences as loudly as features, because a
list of features implies the rest exist. Where a performance number appears, it
comes from the current comparison under [Measurements](./measurements/index.md),
which names the rig, the data size and the protocol it was taken under. Where a
rule is described, it is usually enforced by a gate rather than by convention —
the project's own phrasing is that *a rule nothing checks is a preference*.

If you find a page that promises something the code does not do, that is a bug
in the page, and it is worth reporting as one.
