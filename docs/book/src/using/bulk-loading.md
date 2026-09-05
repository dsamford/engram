# Loading data at scale

Loading a corpus is a different workload from serving one, and Engram has a
mode that says so.

## The short version

| you are | do this |
|---|---|
| loading a few thousand rows | ordinary `CREATE` statements, batched |
| loading millions of rows | `--bulk-ingest`, then restart without it |
| loading for a benchmark | `snbgen` to make a corpus, `snbload` to load it |

## Ordinary loading

Batch your writes. Each Bolt statement is a round trip, and each autocommit
statement is a transaction, so one statement per row is dominated by overhead.

```cypher
UNWIND $rows AS row
CREATE (p:Person {id: row.id, name: row.name})
```

Send parameters rather than inlining values — the engine registers a
statement's predicates from its AST, and a parameterised statement is also the
one you want for injection reasons.

**Create indexes and constraints *after* the load** where you can. Every
constrained write consults the constraint machinery, and an index maintained
during a bulk load is maintained once per row rather than built once at the
end.

## Bulk-ingest mode

```sh
engram-server 127.0.0.1:7687 --paged-dir ./paged --bulk-ingest
```

The server announces what it has traded away:

```text
[engram-server] BULK INGEST ON: writes skip the commit log (durability by
re-ingest, NOT by replay), ids reserve in ranges of 4096, autocommit is not
serialisable. Restart without --bulk-ingest to serve normally.
```

Three changes, each with a reason:

- **Writes skip the commit log.** Durability is by *re-ingest*: if the load
  dies, you run it again. There is no replay because there is no log. This is
  the largest single win, because the log latch is the serialisation point
  every write passes through.
- **Ids reserve in ranges of 4096** instead of 256. The id allocator holds a
  global mutex across a durable counter write; a larger reservation removes
  that from 4,095 of every 4,096 allocations.
- **Autocommit is not serialisable.** The OCC re-run exists for concurrent
  hot-key writers. A loader is one client, so it buys nothing and costs
  throughput.

**Refused with `--data-dir`**, because "durability by re-ingest" and "durability
by replay" are contradictory promises. Restart without the flag to serve
normally.

> Do not serve production traffic in this mode. It is a load mode, and the
> startup banner says so because a silent one would be a footgun.

## After the load

1. **Stop the loader.**
2. **In paged mode, checkpoint** so the tail reaches disk:
   ```cypher
   CALL engram.checkpoint() YIELD spilled, segments, resident, tail
   RETURN spilled, segments, resident, tail
   ```
3. **Restart without `--bulk-ingest`.**
4. **Read the `warmed in` line** to confirm the corpus is what you expect:
   ```text
   [engram-server] warmed in 340 ms: 1482301 nodes, 5290114 out-edges, …
   ```
5. **Create indexes and constraints** now.

## The benchmark loaders

Three binaries in `engram-bench`, useful beyond benchmarking because they are
the reference implementation of a correct load.

### `snbgen` — make a corpus

```sh
cargo run --release -p engram-bench --bin snbgen -- ./corpus 10000 42
```

`snbgen <out_dir> <persons> [seed]` writes `nodes.jsonl`, `rels.jsonl` and
`meta.json`. The person count is the scale knob — roughly 50 nodes and 224
relationships per person.

**It is deterministic**, and that is the point: a SplitMix64 stream seeded by
`seed` drives every choice, so `(persons, seed)` reproduces the graph **byte
for byte**. No wall clock, no system randomness.

> The output is *synthetic data on an SNB-like schema*. It is **not** official
> LDBC Datagen output and must not be presented as LDBC results.

### `snbload` — load it over Bolt

```sh
cargo run --release -p engram-bench --bin snbload -- ./corpus 127.0.0.1:7687
```

Batches of 500. It loads **any** Bolt server, including Neo4j (`--neo4j`), and
that is deliberate: comparing two engines fairly needs both loaded the *same
way*.

The reasoning is worth borrowing. Two engines loaded by two different paths
differ by more than their engines — different property types, different label
sets, an index one side has and the other does not. Every one of those shows up
later as a performance difference and gets attributed to the query engine.
Loading both through one binary removes the load path and the data shape as
variables, leaving the thing actually under test.

### `datagen2jsonl` — convert official LDBC output

For when you have real Datagen output rather than `snbgen`'s.

## Tuning the load

| flag | effect on a load |
|---|---|
| `--bulk-ingest` | the big one; see above |
| `--id-reservation N` | ids per durable counter write; bulk mode sets 4096 |
| `--seal-after N` | how much sits in the volatile tail before sealing |
| `--paged-cache-mb N` | the block-cache budget, if paged |
| `--no-group-commit` | do **not** use during a load; group commit is the win |

## Failure modes

| symptom | cause |
|---|---|
| the load dies and the store is empty | bulk-ingest durability is by re-ingest — run it again |
| the load dies in paged mode and *some* data survives | only sealed segments are durable; the tail was lost |
| "refusing to start" with both flags | `--bulk-ingest` and `--data-dir` are incompatible |
| a constraint violation mid-load | the corpus has duplicates; the message names the offending node |
| memory grows through the load | in resident mode the corpus is memory; use `--paged-dir` |

## What does not exist

- **No `LOAD CSV`.** Loading is over Bolt or through the harness binaries.
- **No offline bulk importer** that writes segments directly without a server.
- **No parallel load coordination.** One loader, one server.

See [Roadmap](../roadmap.md).

## Next

- [Durability and recovery](./durability.md) — what each mode survives.
- [Paged mode](../architecture/paged-mode.md) — serving what you loaded.
- [Benchmarking](../development/benchmarking.md) — the rest of the harness.
