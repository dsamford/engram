# Derived structures — the one rule

> **Historical — implemented 2026-08-28**, replacing six special-case caches.
>
> For the mechanism as it stands, see
> [Derived structures](../architecture/derived-structures.md). This page is the
> diagnosis that produced it.

## The finding

A graph keeps many structures *derived* from its store: label memberships,
range indexes, adjacency tables, degree tables, BFS memos, relationship
populations, per-node adjacency snapshots.

Every one had been written as its own special case. A balanced-load head-to-head
— **739 ops/s against 2,624 at one worker, 344 at six** — turned out to be **the
same defect six times over.**

| # | structure | what it did wrong |
|---|---|---|
| 1 | adjacency probe gate | Keyed on the **global commit clock**. A `CREATE (:Message)` — a *node* write — reset it, so the first 1,024 hops of every following traversal bypassed a current table and opened a k-way visitor scan each. |
| 2 | label memberships | Catch-up by **copying the whole label** — O(label) per read after any write to it. |
| 3 | range indexes | Deltas **consumed by the first reader**. A concurrent reader on another worker found none and fell to a full rebuild under the store's read lock, stalling every writer. |
| 4 | range indexes, memberships | Publish was **not monotone**. A rebuild at an older epoch could overwrite a newer catch-up and **lose a row**. Reachable. |
| 5 | every structure | A change inside a **transaction** was stamped at note time, *before* the commit. A snapshot rebuilt in that window carried a stamp at or past a change it did not contain — and was judged current for ever. |
| 6 | the store tail | Recovery replays the whole log into the tail, and every read of a non-empty tail takes the hot latch writers hold — ~1,000 latch acquisitions per statement. **Nothing in the server ever sealed it.** |

Also verified and **refuted** as causes: the group-commit mutex (on/off arms
flat) and syncer lock nesting. Recording what was ruled out is as useful as
recording what was found.

## Why one rule rather than six fixes

Every one had already been fixed as a special case, and the next cache had it
again. Defect 1 is the clearest: keying validity on the global commit clock is
an obvious mistake once stated, and it was made independently in several places
because nothing made it hard to make.

So the six fixes were replaced by a mechanism with four parts — a change log
stamped with commit timestamps, a monotonically published slot, a single-flight
guard around builds only, and an O(delta) snapshot.

[The architecture page](../architecture/derived-structures.md) has the mechanism
and the reader protocol.

## The two subtle ones

**Defect 4 — non-monotone publish** is a correctness fault, not a performance
one. A slower path overwriting a faster one's result *loses data*. The fix is
that a slot only accepts a newer epoch, which is cheap and would never have been
added without this diagnosis.

**Defect 5 — the stale stamp** is the one that generalises. Reading the clock
separately from the entries means a reader can stamp a snapshot as newer than a
change it does not contain, and then be judged current for ever. The fix is that
a catch-up's epoch is read **under the log's lock, in the same critical section
as the entries**, and a build's stamp is taken *before* its scan.

## The correction this document forced

The document is careful about something worth repeating: **it names the causes
it refuted.** Two plausible theories — the group-commit mutex and syncer lock
nesting — were tested with arms and came out flat.

That matters because both were more intuitive than the real answer. A balanced
workload losing to a competitor *looks* like lock contention. It was six
independent cache-invalidation bugs.

## Next

- [Derived structures](../architecture/derived-structures.md) — the mechanism.
- [The derived-refresh write tax](./derived-refresh-write-tax.md) — what the fix
  then cost.
- [Concurrency direction](./concurrency-direction.md) — the write-side
  investigation alongside it.
