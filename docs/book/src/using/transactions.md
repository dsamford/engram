# Transactions and isolation

Engram is multi-version and multi-writer. Readers do not block writers and
writers do not block readers; conflicts are detected at commit rather than
prevented by locking, and a losing transaction aborts and re-runs.

The short version of what you get:

| | |
|---|---|
| A single read | **read committed** — the newest committed value, right now |
| A transaction | validates its **read set as well as its write set** at commit |
| An autocommit statement | the same, by default — it runs as a read-validated transaction |
| `cas` | lock, then read the **current** value, then compare |
| Phantoms | **not** closed by default; `--precision-locking` closes them |

## What a commit timestamp means

Every write carries a commit timestamp, and a transaction's **whole write set
commits at one**. That single value is the atomic visibility point: before it,
none of the writes are visible; after it, all of them are. There is no window
in which half a transaction can be observed.

A read takes a **snapshot** — a timestamp — and sees exactly the versions
committed at or before it.

The `bookmark: "eg:<commit_ts>"` a Bolt reply carries is this number.

## Why reads are read committed, not snapshot

This is a deliberate choice with a specific argument behind it, and it is the
one place Engram departs from what you might expect.

The correctness of a compare-and-swap depends on **write-lock-then-read**
ordering:

1. take the entity's lock (FIFO, awaiting if held);
2. read the **current committed value** — never a snapshot;
3. compare with what you expected;
4. write and publish, or fail with what was actually current.

Under snapshot isolation, step 2 returns *the transaction's snapshot* instead.
Every contender's guard then passes, and ownership, tenancy or governance
writes lose updates **silently** — no error, no log line, just the last writer
winning a race nobody knew had run.

So `Store::cas` is the supported primitive and its body *is* that ordering. The
lock is released on every path including the error paths, because a CAS that
leaks its lock on mismatch turns every retry loop into a deadlock that presents
as a hang.

## What lifts a transaction above snapshot isolation

At commit, validation asks one question per key: **did anyone commit to this
key after my snapshot?** — and it asks it for every key the transaction *read*
as well as every key it *wrote*.

Validating the read set is the part that matters. The source puts it plainly:
this is what "lifts this above bare snapshot isolation toward serializability:
a stale READ that fed a decision aborts rather than commits."

If any key moved, the transaction returns a conflict and is re-run.

## Two kinds of lost update, and what stops each

This distinction is worth internalising, because the two need different
mechanisms and only one of them is a latch.

**Record-level.** `SET n.name = 'x'` reads the node's record, changes one
property, and writes the record back. Two sessions setting *different*
properties must not each write back a record missing the other's. A per-entity
write latch gives this.

**Statement-level.** `SET n.hits = n.hits + 1` reads `n.hits` when `MATCH`
materialises the node — *before* the write. Two sessions can both read 5 and
both write 6. **No latch inside the write path can cover a read that happened
before it.** This needs the statement to run as a transaction whose commit
validates its read set.

That is exactly what **serialisable autocommit** is, and it is **on by
default**. Every autocommit write statement runs as a read-validated
transaction, retrying on conflict.

Both guarantees have a test and a paired negative canary — one asserting every
concurrent increment lands, one demonstrating the loss with the lever turned
off. A guarantee whose test would pass either way is not a guarantee.

## Conflicts, retries and escalation

On conflict the adapter re-runs the statement. Counters track how it went, and
they are printed every 30 seconds when they move:

```text
txn_conflicts, autocommit_reruns, won@1, won@2, won@3-4, won@5-8, won@9+,
max_attempts, escalations, escalated_losses
```

`won@N` is the distribution of how many attempts a statement needed. A healthy
system is heavily weighted to `won@1`.

Under sustained contention on one key, re-running optimistically is wasteful —
every contender burns a full execution to lose. **Conflict escalation** switches
those contenders onto FIFO entity locks so they queue instead of racing. It is
on by default and `ENGRAM_CONFLICT_ESCALATION=0` turns it off.

### Guard rows, and why two relationship writes used to conflict

Creating a relationship writes a **guard row** under each endpoint. Two writes
touching one hub node therefore wrote the same guard row and aborted each
other, even though they conflicted over nothing real.

The **guard exemption** recognises that case: two *puts* of the same guard row,
neither in a read set, do not conflict. It is on by default and worth 3.7× on
the shared-endpoint shape. `--no-guard-exemption` restores the old behaviour as
the measurement control.

## Explicit transactions

Standard Bolt: `BEGIN`, statements, `COMMIT` or `ROLLBACK`. Drivers expose this
as their transaction API.

Two properties to know:

- **A read-only transaction never validates and never aborts.** If the write
  set is empty, commit returns the snapshot timestamp immediately.
- **Statements inside an explicit transaction never parallelise.** The
  read-your-writes overlays and the OCC read set are thread-local, so a morsel
  worker would silently read committed state and record nothing. The dispatch
  gates enforce this.

## Phantoms and `--precision-locking`

By default, Engram does **not** close phantoms: a transaction whose `MATCH`
would have matched a row another transaction created after its snapshot can
still commit.

`--precision-locking` validates each transaction's node-pattern **predicates**
against the rows committed since its snapshot, closing that class.

It is documented as an **isolation upgrade and a behaviour change**: it aborts
statements that currently commit. Turn it on deliberately, not as a default,
and expect more conflicts.

## Bulk ingest changes the rules

`--bulk-ingest` turns serialisable autocommit **off** along with the commit
log. A loader is one client, and the OCC re-run exists for concurrent hot-key
writers, so it buys nothing there and costs throughput. Do not serve
concurrent traffic in this mode.

## Errors you may see

| condition | code |
|---|---|
| transaction could not start | `Neo.ClientError.Transaction.TransactionStartFailed` |
| commit failed | `Neo.ClientError.Transaction.TransactionCommitFailed` |
| CAS mismatch | carries the value that was actually current |
| durability failure | the write is unwound and **not** acknowledged |

A durability failure is worth calling out: if the `fsync` fails, the write is
unwound rather than acknowledged. You never get a success for something that
did not reach disk.

## Next

- [Durability and recovery](./durability.md) — what a commit survives.
- [The write path](../architecture/write-path.md) — validation, logging and
  publication in detail.
- [Tuning guide](../reference/tuning.md) — what to do when conflicts are high.
