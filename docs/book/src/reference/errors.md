# Errors

Engram maps its internal error types onto Neo4j's status-code vocabulary, so
driver error handling works unchanged.

## What a client sees

A failure arrives as a Bolt `FAILURE` message carrying a map:

```json
{
  "code": "Neo.ClientError.Statement.SemanticError",
  "message": "row budget exceeded: the statement materialised more than 100 …",
  "gql_status": "50N42",
  "description": "error: general processing exception - unexpected error",
  "neo4j_code": "Neo.ClientError.Statement.SemanticError",
  "diagnostic_record": { "CURRENT_SCHEMA": "/", "OPERATION": "", … }
}
```

`gql_status`, `description`, `neo4j_code` and `diagnostic_record` are present
from **Bolt 5.7** onward. Classification derives from the code's second
segment, so `Neo.ClientError.*` is a client error and `Neo.DatabaseError.*` is
not.

## Status codes

### Statement execution

| condition | code |
|---|---|
| parse failure | `Neo.ClientError.Statement.SyntaxError` |
| an unsupported construct | `Neo.ClientError.Statement.NotSupported` |
| a semantic error, **including the row budget and the memory ceiling** | `Neo.ClientError.Statement.SemanticError` |
| a bad argument or evaluation failure | `Neo.ClientError.Statement.ArgumentError` |
| a graph-level failure, **including constraint violations** | `Neo.ClientError.Statement.ExecutionFailed` |
| an internal saturation marker leaking | `Neo.DatabaseError.Statement.ExecutionFailed` |

That last row is the only `DatabaseError` the statement path produces. The
saturation marker is stage-internal control flow, and one reaching a client is
an engine bug reported as such.

Two classifications worth noting because they are not obvious:

- **The row budget is a `SemanticError`**, not a database error. The statement
  asked for more than the server will build, and rewriting it is the fix. A
  refusal at the memory ceiling (`memory ceiling reached`) carries the same
  code, although there the statement is usually not at fault: it queued for
  memory and none came back, and the server is still serving.
- **A constraint violation is `ExecutionFailed`**, and the message names the
  label, property and the node that already holds the value.

### Transactions

| condition | code |
|---|---|
| the transaction could not start | `Neo.ClientError.Transaction.TransactionStartFailed` |
| the commit failed | `Neo.ClientError.Transaction.TransactionCommitFailed` |

### Streams

| condition | code |
|---|---|
| `PULL`/`DISCARD` with no open stream | `Neo.ClientError.Statement.InvalidUsage` |
| `PULL`/`DISCARD` naming an unknown `qid` | `Neo.ClientError.Statement.InvalidUsage` |

## Internal error types

Useful if you are embedding Engram as a library, or reading a message and
wondering where it came from.

### `engram-graph`

| type | variants |
|---|---|
| `RunError` | `Eval`, `Graph`, `Unsupported`, `Semantic`, `Saturated` (internal — must never surface) |
| `GraphError` | `Corrupt`, `BadPropertyValue`, `Store`, `Missing`, `StillConnected`, `SchemaConflict`, `ConstraintViolation`, `Txn`, `TxnConflict` |

`StillConnected` is the one behind "cannot delete a node that still has
relationships" — use `DETACH DELETE`.

### `engram-store`

| type | variants |
|---|---|
| `StoreError` | `ProtectedKindPlaintext`, `InvalidKind`, `Durability`, `CasMismatch`, `Conflict` |
| `RecoverError` | `BrokenChain{seq}`, `SequenceGap{expected,found}`, `MalformedPayload{seq}` |
| `OpenWalError` | `Io`, `Format`, `Recover` |
| `LockError` | `Held{path,holder}`, `Io` |
| `AdjacencyError` | `ContentionExhausted`, `CorruptChunk`, `Store` |
| `SstError` | `Truncated`, `BadMagic`, `UnsupportedVersion`, `HashMismatch`, `Corrupt` |

`AdjacencyError::Store` is how a store refusal reaches an adjacency caller;
the other two are the adjacency layer's own.

Three of these carry design decisions:

- **`ProtectedKindPlaintext` has no override.** The storage layer refuses a
  plaintext write to a protected KIND unconditionally, including KINDs this
  build has never heard of, because the check is a range check rather than a
  list. There is no flag and no privileged caller.
- **`Durability` means the write was unwound**, not that it half-happened. If
  the `fsync` failed, nothing was acknowledged.
- **`CasMismatch` carries what was actually current**, so a retry loop does not
  need a second read.

### Recovery

`RecoverError`'s three variants are three different facts: a chain that does
not verify, an entry that is missing, and a payload that does not decode.
Recovery refuses at the named sequence rather than continuing, because
continuing past damage is how a database quietly serves a fork.

### `engram-cypher`, `engram-bolt`

`ParseError`, `LexError`, `EvalError`; `WireError`, `PackError`.

### The seam crates

`TierError` (object storage) is worth one note even though it is not on the
serving path: its default for anything unrecognised is **`Unavailable`, never
`NotFound`** — because the fatal direction is a network hiccup reading as "the
object does not exist".

## Failures that are not errors

Some conditions take the process down instead, deliberately:

| condition | behaviour | why |
|---|---|---|
| the requested data or paged directory cannot be opened | **panic** | starting empty over a directory that was asked for would look like an empty database rather than a failed open — that is how a restore gets overwritten |
| an `fsync` fails during group commit | **abort** | replies for that batch are unsent and unacknowledged; continuing would acknowledge writes that are not durable |
| the data directory is locked | exit 1, naming the pid | two writers on one WAL leave an unverifiable chain |
| `--data-dir` with `--paged-dir` | exit 1 | one store, two on-disk layouts — the durability model is the same either way |
| `--data-dir` naming a paged directory (one holding `seg-*.seg` files) | exit 1 | resident mode reads only `engram.wal`, so it would ignore every segment and serve an empty or partial database without an error |
| `--bulk-ingest` with `--data-dir` | exit 1 | durability by re-ingest versus by replay |

The `--data-dir` with `--paged-dir` refusal is not a durability one, though it
reads like one. Since `--paged-dir` gained its WAL the two modes have the same
durability model and differ only in on-disk layout — the binary's usage text
says so, "two layouts, one durability". A store has one backing layout, and
cannot be both a resident WAL directory and a paged segment directory at once.
The row after it is the same point made about a directory rather than a pair
of flags. All three argument refusals happen before the directory is locked or
the port is bound, so a refused start leaves no stale lock behind. The
`--bulk-ingest` row is a different case and its reason still holds:
bulk writes go through `put_unlogged` and never reach the log, so a WAL
directory served in bulk mode would replay to a partial database.

A panic **inside a session** is different: it is caught, the session is
dropped, and the worker survives. Before that, one bad statement killed a
worker and silently stranded every connection pinned to it.

## Next

- [Cypher support](../using/cypher-support.md) — what is refused, with the
  messages.
- [Durability and recovery](../using/durability.md) — what recovery refuses.
- [Bolt and PackStream](./bolt.md) — where the failure map is built.
