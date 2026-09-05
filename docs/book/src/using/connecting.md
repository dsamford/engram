# Connecting

Engram speaks **Bolt**, the same wire protocol Neo4j uses, so stock Neo4j
drivers connect without modification.

```text
bolt://host:7687
```

Use the `bolt://` scheme, not `neo4j://`. The latter asks for routing, which a
single-process server does not provide.

## Drivers

Any Bolt 5.x driver should work. These are the ones the project has exercised:

| language | package | notes |
|---|---|---|
| Python | `neo4j` | |
| JavaScript / TypeScript | `neo4j-driver` | |
| Rust | `engram-bolt::client` | a minimal synchronous client, in-tree, used by the benchmarks |

> **The compatibility matrix is open, not closed.** Engram is deliberately not
> claimed to be a Neo4j drop-in. Wire compatibility is demonstrated; full
> driver-feature compatibility is not, and a driver that reaches for something
> unimplemented will find out at runtime. If you exercise a driver Engram has
> not, that is a useful thing to report.

### Python

```python
from neo4j import GraphDatabase

driver = GraphDatabase.driver("bolt://127.0.0.1:7687", auth=("", ""))
with driver.session() as session:
    for record in session.run("MATCH (p:Person) RETURN p.name AS name LIMIT 10"):
        print(record["name"])
driver.close()
```

### JavaScript

```js
import neo4j from 'neo4j-driver'

const driver = neo4j.driver('bolt://127.0.0.1:7687', neo4j.auth.basic('', ''))
const session = driver.session()
const result = await session.run('MATCH (p:Person) RETURN p.name AS name LIMIT 10')
result.records.forEach(r => console.log(r.get('name')))
await session.close()
await driver.close()
```

## Authentication — read this

**Credentials are accepted and not verified.** Any username and password opens
a session with full access, including empty ones.

What *is* enforced is the protocol ordering: a `RUN` before `LOGON` is refused.
That is the part a driver depends on, so drivers behave normally — but it is
sequencing, not authentication.

Do not treat the credentials your driver sends as a control. See
[Security posture](./security.md).

## What the handshake does

Worth knowing if you are debugging a connection rather than using one.

A Bolt connection opens with a four-byte magic preamble followed by sixteen
bytes of version proposals. Engram offers two versions:

```text
6.0.0    and    5.8.8
```

Two details are deliberate:

- **A wrong magic is refused at four bytes**, not after all twenty. Waiting for
  the full preamble would leave a misdirected HTTP client hanging instead of
  failing legibly.
- **Manifest-style negotiation is supported**, so a driver that proposes the
  manifest handshake gets a manifest reply rather than a fallback.

After the handshake, `HELLO` then `LOGON`, and then statements.

## Checking what you connected to

```cypher
CALL dbms.components() YIELD name, versions, edition
RETURN name, versions, edition
```

```text
name     versions     edition
Engram   ["0.1.0"]    engram
```

The server also announces itself in `HELLO`'s response as
`engram/<version>` — deliberately honest rather than impersonating Neo4j.
Drivers negotiate on the protocol version from the handshake, not on this
string, so answering truthfully costs nothing a driver depends on. It is
overridable (`BoltServer::set_server_agent`) for the case where a client is
found that does read it, because "no driver reads it" is a claim about drivers
that have been tested.

> Note the `YIELD … RETURN`. Engram has no standalone `CALL` — see
> [Cypher support](./cypher-support.md).

## Routing and namespaces

`HELLO` accepts `realm` and `namespace` extras, and the server resolves them to
the graph serving that coordinate. Per-namespace id spaces make those graphs
naturally isolated: a node id means nothing outside its own `(realm,
namespace)`, so two namespaces cannot collide.

A default server serves realm 1, namespace 1, and you will not meet this unless
you go looking for it. It is not yet an access-control boundary — with no
authentication there is no identity to bind a realm to. See
[Roadmap](../roadmap.md).

## Connection limits and timeouts

| flag | default | what a client sees |
|---|---|---|
| `--max-connections N` | 512 | connections past the cap are not accepted |
| `--read-timeout-secs N` | 300 | a connection quiet for N seconds is reaped |
| `--row-budget N` | 20,000,000 | a query materialising more is refused |

Each connection costs **two OS threads** — a reader and a writer — which is why
there is a cap at all.

The read timeout catches long analytical queries as well as idle peers, because
a client waiting on a query is quiet. If you run queries past five minutes,
raise it. See [Result paging](./result-paging.md).

## Backpressure

A connection may have a bounded number of unacknowledged bytes queued to the
engine (8 MiB). Past that the reader parks rather than growing the queue, so a
fast client against a slow engine is slowed rather than allowed to consume
memory without limit.

This is invisible in normal use and is the reason a pathological client cannot
push the server over on its own.

## Errors

Failures arrive as standard Bolt `FAILURE` messages carrying Neo4j status
codes, so driver error handling works unchanged. From Bolt 5.7 the failure map
also carries `gql_status`, `description`, `neo4j_code` and a diagnostic record.

See [Errors](../reference/errors.md).

## Next

- [Cypher support](./cypher-support.md) — what the engine accepts and refuses.
- [Result paging](./result-paging.md) — why your fetch size is not a memory
  bound.
- [Bolt and PackStream](../reference/bolt.md) — the message set in detail.
