# Engram

A property-graph database in Rust, speaking openCypher over the Bolt protocol.

> **Pre-release.** There is no authentication and no TLS. Do not expose an
> Engram server to a network you do not control. [Known limits](./known-limits.md)
> is a list of *absences*, written to be read before you rely on anything.

Engram is a single-process graph engine with its own Cypher parser and planner,
MVCC storage over a write-ahead log, paged segments that read block-by-block so
a graph can exceed RAM, vector indexes, and a Bolt v5 listener that stock Neo4j
drivers connect to.

|  |  |
|---|---|
| **Measured standing** | ahead of Neo4j 5.26-community on **all 19 workloads** measured at LDBC SNB SF1 — LSQB 9/9 and the stress suite's 20 levels, same-window pairs |
| **openCypher conformance** | 3,771 of 3,772 evaluated TCK scenarios (99.97%), CI-ratcheted |
| **`unsafe` code** | none — the workspace denies it outright |
| **Third-party crates** | 39, every one permissively licensed, no copyleft |
| **Reproducibility** | two processes, one seed, one identical trace digest — enforced as a gate |

The performance line is the one most worth checking rather than believing:
[Measurements](./measurements/index.md) carries the per-query tables, the
methodology, the commands to reproduce them, and — at equal prominence — what
they do not say.

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
| [Design history](./history/index.md) | the engineering record — dated, superseded in places, kept because the reasoning is the value |
| [Measurements](./measurements/index.md) | performance claims, each with its corpus, host shape and command |

## A note on how this project states things

Engram's documentation tries to state absences as loudly as features, because a
list of features implies the rest exist. Where a number appears, it is
reproducible from a file under [Measurements](./measurements/index.md). Where a
rule is described, it is usually enforced by a gate rather than by convention —
the project's own phrasing is that *a rule nothing checks is a preference*.

If you find a page that promises something the code does not do, that is a bug
in the page, and it is worth reporting as one.
