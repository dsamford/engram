# Result paging

**"Paging" means two unrelated things in Engram, and conflating them is the
single most expensive misreading available.**

| | |
|---|---|
| **Result paging** — this page | how many rows a driver pulls per round trip |
| **Paged storage** — [Paged mode](../architecture/paged-mode.md) | serving a graph larger than memory from block-addressed segments on disk |

They share a word and nothing else. `--paged-cache-mb` has nothing to do with
result size; a driver's `fetch_size` has nothing to do with disk.

## The thing to know

**A query's rows are fully materialised before any of them are sent.**

When `RUN` executes a statement, the whole `QueryResult` is built and held. A
`PULL` then walks it with a cursor:

```rust
struct Stream {
    result: QueryResult,   // every row, already built
    at: usize,             // how far the client has read
}
```

The consequence follows directly, and it is the opposite of what most people
assume:

> **Your driver's fetch size does not bound the server's memory.**
> Setting `fetch_size=100` on a query that matches ten million rows does not
> make the server hold a hundred rows. It holds ten million, and hands you a
> hundred at a time.

What *does* bound server memory is **`--row-budget`**, and that is why it
defaults to 20,000,000 rather than to unlimited.

## How the wire protocol pages

Standard Bolt, and drivers do this for you.

`PULL {n, qid}`:

- **`n`** — how many records to send. `n < 0` means "all remaining"; drivers
  send `n = -1` when you consume a result eagerly, and a positive `n` when you
  have set a fetch size.
- **`qid`** — which stream. `-1` or absent means the most recent, which is what
  every autocommit session uses. Explicit qids matter only for concurrent
  streams inside one transaction.

The reply is one `RECORD` per row, then a `SUCCESS` whose metadata says which
situation you are in:

| metadata | meaning |
|---|---|
| `has_more: true` | the stream is not drained; pull again |
| `bookmark: "eg:<commit_ts>"` | the stream is finished, and this is the commit timestamp you saw |

`DISCARD` is the same walk with the records suppressed — it advances the cursor
and drops the rows, which is how a driver abandons a result without paying to
encode it.

## Bookmarks

The bookmark is the store's commit clock at the moment the stream drained,
formatted `eg:<n>`. It is monotonic, so it is directly comparable.

Today it is a value you can hold and reason about on one instance. It exists in
this form because it is what a replica would need to wait on — free to produce
now, load-bearing later. See [Roadmap](../roadmap.md).

## Bounding a result properly

Since fetch size does not bound the server, bound the query.

**Use `LIMIT`.** It is the only thing that reduces what the engine builds:

```cypher
MATCH (p:Person)-[:KNOWS]->(f:Person)
RETURN p.name, f.name
ORDER BY p.name
LIMIT 1000
```

**Aggregate instead of returning rows** where you can. `count(*)` is answered
by a **fold that never materialises the rows it counts**, which makes counting
dramatically cheaper than returning rows and counting client-side.

That is demonstrable rather than merely claimed. On a server started with
`--row-budget 100`, over fifty nodes — so a self-join produces 2,500 rows:

```cypher
MATCH (a:N), (b:N) RETURN a.i, b.i        -- refused: 2500 > 100
MATCH (a:N), (b:N) RETURN count(*) AS c   -- returns 2500
```

The second answers correctly under a budget twenty-five times smaller than its
own result set, because those rows are never built.

**Keep `--row-budget` set.** The default of 20,000,000 refuses a statement that
would materialise more:

```text
Neo.ClientError.Statement.SemanticError
row budget exceeded: the statement materialised more than 20000000
intermediate rows; it would exhaust memory rather than stream
```

The alternative to refusing is the OOM killer, which refuses nothing and takes
every other session on the process with it. Note the classification: a *client*
error, not a database one, because the statement asked for more than the server
will build and rewriting it is the fix.

`--row-budget 0` disables the bound. Do that only when you know the statement.

## Timeouts and long results

`--read-timeout-secs` defaults to 300 and reaps a connection that has been
**quiet** for that long. A client waiting on a long analytical query *is*
quiet, so a query that runs past five minutes can have its connection reaped
mid-flight.

If you serve long analytical queries, raise it or set it to `0`. The flag
exists as a slowloris guard — a peer that opens a connection and says nothing
holds a reader and a writer thread forever — so disabling it trades that
protection away.

## What is not supported

- **No server-side cursors that outlive a session.** A stream belongs to its
  connection.
- **No result streaming in the engine sense.** The rows exist before the first
  one is sent. Making the engine stream a result — producing rows lazily as the
  client pulls — is not implemented, and it is what would make fetch size a
  real memory bound.
- **No default fetch size on the server.** The driver's `n` is authoritative.

## Next

- [Server CLI](../reference/cli.md) — `--row-budget`, `--read-timeout-secs`.
- [Tuning guide](../reference/tuning.md) — what to change when a query is too
  large.
- [Bolt and PackStream](../reference/bolt.md) — the message-level detail.
