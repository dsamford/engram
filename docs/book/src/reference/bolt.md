# Bolt and PackStream

Engram's wire protocol is **Bolt**, encoded with **PackStream v2**. This page
is the message-level detail; [Connecting](../using/connecting.md) is what you
need to use a driver.

The protocol machine is **sans-io**: `BoltServer::feed(&[u8]) -> Vec<u8>`. It
never touches a socket. `engram-server` is the adapter that does, and that
separation is why the protocol can be tested without a network.

## The handshake

```text
client → 60 60 B0 17            the magic preamble
client → 16 bytes               four version proposals
server →  4 bytes               the chosen version
```

Engram offers two:

```text
6.0.0    5.8.8
```

**A wrong magic is refused at four bytes**, not after all twenty. Waiting for
the full preamble would leave a misdirected HTTP client hanging rather than
failing legibly.

**Manifest negotiation** is supported: a client proposing `00 00 01 FF` gets a
manifest reply rather than a fallback.

## Messages

### Client → server

| message | tag | |
|---|---|---|
| `HELLO` | `0x01` | opens the session; carries `user_agent` and optional `realm`/`namespace` |
| `LOGON` | `0x6A` | credentials — **accepted, not verified** |
| `LOGOFF` | `0x6B` | |
| `RUN` | `0x10` | a statement, its parameters, and extras |
| `PULL` | `0x3F` | `{n, qid}` — fetch records |
| `DISCARD` | `0x2F` | the same walk with records suppressed |
| `BEGIN` | `0x11` | start an explicit transaction |
| `COMMIT` | `0x12` | |
| `ROLLBACK` | `0x13` | |
| `RESET` | `0x0F` | return the session to a known state |
| `ROUTE` | `0x66` | routing table — answered, but there is one server |
| `TELEMETRY` | `0x54` | accepted and ignored |
| `GOODBYE` | `0x02` | |

### Server → client

| message | tag | |
|---|---|---|
| `SUCCESS` | `0x70` | with metadata: fields, `has_more`, `bookmark`, … |
| `RECORD` | `0x71` | one result row |
| `IGNORED` | `0x7E` | sent while the session is in a failed state until `RESET` |
| `FAILURE` | `0x7F` | the status map — see [Errors](./errors.md) |

### Ordering

`RUN` before `LOGON` is **refused**. That is the part a driver depends on, so
drivers behave normally — but it is sequencing, not authentication. There is no
authentication.

## Paging with `PULL`

```text
PULL {n: 1000}          fetch up to 1000 records from the most recent stream
PULL {n: -1}            fetch all remaining
PULL {n: 1000, qid: 3}  from a specific stream
```

`qid = -1` or absent means the most recent stream — the implicit-stream case
every autocommit session uses. Explicit qids are needed only for concurrent
streams inside one transaction.

The terminating `SUCCESS` carries either:

- **`has_more: true`** — the stream is not drained; pull again; or
- **`bookmark: "eg:<commit_ts>"`** — the stream is finished.

> **A driver's fetch size does not bound server memory.** The rows are
> materialised before the first is sent. See
> [Result paging](../using/result-paging.md).

## PackStream v2

The value encoding. Markers are one byte, with small values packed into the
marker itself.

| type | how |
|---|---|
| null, boolean | single marker bytes |
| integer | tiny ints in the marker, then `INT_8`/`16`/`32`/`64` |
| float | IEEE-754 double |
| string, list, map | length in the marker for small sizes, then 8/16/32-bit length |
| structure | a tag byte plus fields — nodes, relationships, temporals |

Engram encodes `Node` and `Relationship` as the structures drivers expect, and
the full temporal set — date, time, local time, datetime in both the
zone-offset and zone-id forms, local datetime and duration.

Three gaps in that sentence are worth having in writing, because a driver
wired against the list above would otherwise find them at runtime:

- **A `Path` is sent as a plain list** of its alternating nodes and
  relationships, not as the Bolt `Path` structure. The signature is declared
  and emitted nowhere, so a driver does not reconstruct a path object from it.
  The dedicated structure is a follow-up; nothing round-trips a path through
  Bolt today.
- **`Point2D`, `Point3D` and `UnboundRelationship` have signature constants
  and no codec.** Neither an encoder nor a decoder mentions them, so they are
  reserved tags rather than supported types — stated as an absence rather than
  discovered.
- **`Vector` (Bolt 6.0) is decode-only.** A client may send one as a
  parameter, and it becomes a plain list of numbers because the engine has no
  vector-typed value; the server never emits one.

## Limits

These bound what one connection can do to the process. They are policy, not
protocol.

| limit | value | why |
|---|---|---|
| `MAX_MESSAGE_BYTES` | **64 MiB** | the largest single message a session assembles |
| `MAX_DEPTH` | **64** | PackStream nesting — an unbounded recursion used to abort the process |
| `MAX_EXPR_DEPTH` | **64** | Cypher expression nesting |
| `MIN_PARSER_STACK_BYTES` | **4 MiB** | the stack a thread must have for the depth bound to be *reachable* rather than academic |
| per-connection inflight | **8 MiB** | backpressure; the reader parks rather than growing an unbounded queue |
| `RSS_GROWTH_REPORT_BYTES` | **32 MiB** | a statement growing the heap by more than this names itself in the log |

The parser-stack constant is the subtle one: a depth bound you cannot reach
before overflowing the stack is not a bound, so the minimum stack is declared
alongside it.

## Autocommit retries

An autocommit write statement runs as a read-validated transaction and retries
on conflict, up to `RETRIES = 4096`, with an escalated-loss bound of 32.

Counters track the outcome: `AUTOCOMMIT_RERUNS`, `WON_AT[5]` (the attempt
distribution), `ESCALATIONS` and `ESCALATED_LOSSES` — all of them on the
periodic counters line. See [Counters and observability](./observability.md).

A six-way **conflict-class** breakdown is recorded on every conflict as well,
classifying the conflicting key by its row family — but it is not printed
anywhere and no trace exposes it. Today only the tests read it, so reading it
on a running server means adding it to that line first.

## Tracing one statement

On a server started with `ENGRAM_TRACE_MARKER=1`, prefix a query with the trace
marker and only that statement is traced:

```cypher
/* engram:trace */ MATCH (p:Person) RETURN count(p)
```

Better than `ENGRAM_TRACE_COUNTERS`, which turns the firehose on for a whole
server. Without the permission the marker is an ordinary comment: the marker is
the client's choice and a traced statement's cost is the server's, so honouring
it is the operator's decision.

## The server agent

`HELLO`'s response announces `engram/<version>` — deliberately honest rather
than impersonating Neo4j. Drivers negotiate on the handshake version, not on
this string, so answering truthfully costs nothing a driver depends on.

It is overridable (`BoltServer::set_server_agent`) because "no driver reads it"
is a claim about drivers that have been tested, and the compatibility matrix is
open.

## The client

`engram_bolt::client::Client` is a minimal synchronous Bolt client, used by the
benchmark harnesses and the integration tests. It is not a general-purpose
driver — it sends an empty parameter map, for instance — but it is a compact
reference for what a session looks like.

## Next

- [Connecting](../using/connecting.md) — using a driver.
- [Result paging](../using/result-paging.md) — why fetch size is not a memory
  bound.
- [Errors](./errors.md) — the status map.
