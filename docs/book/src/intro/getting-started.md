# Getting started

This page takes you from a clean checkout to a running server with a client
connected. It should take about ten minutes, most of which is the compile.

## Prerequisites

- **Rust 1.95.0.** The repository pins it in `rust-toolchain.toml`, so if you
  have `rustup`, the right toolchain is selected automatically the first time
  you build. (The *minimum* supported version is 1.85 — see
  [Building from source](../development/building.md) for why those differ.)
- **A C compiler**, for the two dependencies that need exactly one `cc`
  invocation (`ring` and `blake3`). No cmake, no bindgen, no system libraries.
- Roughly 2 GB of disk for the build.

## Build the server

```sh
cargo build --release -p engram-server
```

The binary lands at `target/release/engram-server`.

## Run it

```sh
./target/release/engram-server 127.0.0.1:7687 --data-dir ./data
```

It says what it did:

```text
[engram-server] listening on bolt://127.0.0.1:7687
[engram-server] durable: ./data/engram.wal
[engram-server] warmed in 0 ms: 0 nodes, 0 out-edges, 0 in-edges, 0 adjacency table(s) holding 0 MB in 0 MB allocated
```

Three lines, and each is worth reading:

- **`listening on`** — the address it actually bound, not the one you asked
  for, so `:0` or a shorthand resolves visibly.
- **`durable:`** — the write-ahead log's path. Every acknowledged write is
  `fsync`ed *before* the acknowledgement, and a restart replays this file.
- **`warmed in`** — Engram builds its [derived structures](../architecture/derived-structures.md)
  *before* it accepts connections. On an empty graph that is instant; on a
  large one it can be seconds, and the alternative is making your first query
  pay for the whole corpus.

Look in `./data` and you will find two files:

```text
LOCK          the directory lock, held for the process lifetime
engram.wal    the write-ahead log — 64 bytes when empty (magic + genesis hash)
```

The lock is taken **before the port is bound**, deliberately: two servers on one
WAL both append and interleave their records, leaving a hash chain no recovery
can verify — after both have already acknowledged writes.

### Without `--data-dir`

```text
[engram-server] WARNING: in-memory only — a restart LOSES ALL DATA.
                Pass --data-dir DIR for durability.
```

In-memory is a legitimate mode for tests and for comparison runs, but it is a
footgun as a silent default, so it is announced loudly. See
[Durability and recovery](../using/durability.md) for the three storage modes
and what each one loses on a crash.

## Connect

Engram speaks **Bolt**, so stock Neo4j drivers work. Authentication is not
verified, so any credentials are accepted — including none.

### Python

```sh
pip install neo4j
```

```python
from neo4j import GraphDatabase

driver = GraphDatabase.driver("bolt://127.0.0.1:7687", auth=("", ""))

with driver.session() as session:
    result = session.run("RETURN 'hello from Engram' AS greeting")
    print(result.single()["greeting"])

driver.close()
```

### JavaScript

```sh
npm install neo4j-driver
```

```js
import neo4j from 'neo4j-driver'

const driver = neo4j.driver('bolt://127.0.0.1:7687', neo4j.auth.basic('', ''))
const session = driver.session()

const result = await session.run("RETURN 'hello from Engram' AS greeting")
console.log(result.records[0].get('greeting'))

await session.close()
await driver.close()
```

### Check what you connected to

```cypher
CALL dbms.components() YIELD name, versions, edition
RETURN name, versions, edition
```

```text
name     versions     edition
Engram   ["0.1.0"]    engram
```

If that returns, the wire, the parser, the interpreter and the procedure
surface are all working.

> **Note the `YIELD … RETURN`.** Unlike Neo4j, Engram does not support a
> *standalone* `CALL proc()` — a bare call, with or without `YIELD`, produces
> no rows rather than the procedure's own columns. Every procedure call needs
> an explicit `YIELD` followed by a `RETURN`. See
> [Cypher support](../using/cypher-support.md#procedure-calls).

## Write something and prove it survives

```cypher
CREATE (a:Person {name: 'Ada', born: 1815})
CREATE (b:Person {name: 'Charles', born: 1791})
CREATE (a)-[:KNOWS {since: 1833}]->(b)
```

```cypher
MATCH (p:Person)-[k:KNOWS]->(q:Person)
RETURN p.name AS from, q.name AS to, k.since AS since
```

```text
from   to        since
Ada    Charles   1833
```

Now stop the server with `Ctrl-C`, start it again with the same `--data-dir`,
and run the `MATCH` a second time. The row is still there: the write was
`fsync`ed before it was acknowledged, and the restart replayed the log.

That round trip — write, kill, restart, read — is the one worth doing yourself
before trusting any database, including this one.

## Where to go next

- [Your first graph](./first-graph.md) — a longer worked example that actually
  uses the graph shape.
- [Core concepts](./core-concepts.md) — what a label, a token and a commit
  timestamp are here.
- [Connecting](../using/connecting.md) — driver compatibility, version
  negotiation, and what Engram announces about itself.
- [Server CLI](../reference/cli.md) — every flag. `--help` lists them too.

> **Before you put this anywhere shared:** read
> [Security posture](../using/security.md). There is no authentication and no
> TLS, and binding to `0.0.0.0` publishes an unauthenticated database.
