# RC1 — guard-row exemption

> **Historical.** Measured on LDBC SNB SF1, identical store copies, six workers,
> with only the server binary differing.

## The evidence came first

The design **refused to let the fix be built on a guess**. It was gated on an
observation confirming that the conflicts in question really were guard rows.

A conflict-class counter classifies each conflicting key by its row family. On
the `rel-hub` shape it answered **guard 273 / entity 297** — so guard rows, the
key every session shares purely because it touches one hub, were **~48% of the
re-runs**.

Only then was the fix built.

That ordering is the point of this page. The fix is obvious once you believe the
diagnosis, and the diagnosis was one of several plausible ones.

## What a guard row is for

A relationship write and a node `DELETE` must be a write-write conflict **in
either commit order** — without that, a delete committing second leaves a
dangling edge.

The guard row `'G' | node id` provides it: both sides touch it, so both sides
conflict.

## The defect

`create_rel` and `delete_rel` **both PUT** the guard row.

So two *relationship* writes touching one node aborted each other — for nothing.
Both were valid; neither was deleting the node.

## The fix

The store gained a key-class hook, and the graph declares exactly one class:

> A guard conflict is exempt **only** when the committed version **and** our own
> intent are both PUTs, and **never** for a read-set key.

`delete_node` writes a **tombstone**, so whichever side commits second fails one
of the two tests, and the delete-versus-create conflict is preserved exactly.

## Why the exemption is narrow on purpose

Three conditions, each removing a way it could be wrong:

| condition | what it preserves |
|---|---|
| the committed version is a PUT | a delete's tombstone still conflicts |
| our own intent is a PUT | our delete still conflicts |
| the key is not in our read set | a transaction that *read* the guard still conflicts |

An exemption dropping any one of them would trade a correctness property for
throughput.

## The result

Worth **3.7×** on the shared-endpoint shape.

`--no-guard-exemption` restores the old behaviour as the measurement control,
and the [CLI reference](../reference/cli.md) lists it among the levers rather
than the settings.

## Next

- [The write path](../architecture/write-path.md) — guard rows in context.
- [RC2 — the sealed prefix](./rc2-sealed-prefix.md) — the other OCC fix.
- [Write concurrency ceiling](./write-concurrency-ceiling.md) — what pointed here.
