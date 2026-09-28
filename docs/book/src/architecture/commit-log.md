# The commit log

One artifact, four requirements: **replication** ships it, **tamper-evidence**
hashes it, **change data capture** tails it, and **point-in-time restore**
replays it.

It must precede all four, because **a hash chain added later cannot attest to
any history predating it.** There is no retrofit for provenance — which is why
this exists in full while the features that would use it do not.

## The entry

Every entry is a **plaintext routing header** plus an **opaque payload**.

```text
RoutingHeader (22 bytes)          payload
realm │ namespace │ kind │        bytes the log never interprets
partition │ op │ commit_ts
```

The header carries only structural, typed fields — the same closed set the key
encoding's sealed trait allows — so **user data cannot ride in it by
construction**.

### Why the split is the design

> Ciphertext payloads with plaintext routing headers only.

Otherwise destroying a tenant's key does not shred that tenant's data from the
log at any replication site, and **log retention becomes the binding constraint
on shred latency**.

When encryption lands, "payload" means "ciphertext under the tenant key" and
**nothing about this format changes**. The flip from plaintext to ciphertext is
policy, not migration.

The consequence worth spelling out: destroying a key shreds that tenant from
*every copy of the log at once* — local, replicas, archives — because what those
copies hold was never readable without it.

## The hash chain

```text
hash = BLAKE3(prev_hash ‖ seq ‖ header ‖ payload_digest)
payload_digest = BLAKE3(payload_len ‖ payload)
genesis = BLAKE3("engram-log-v1-genesis")
```

```mermaid
flowchart LR
    G["genesis"] --> E0["seq 0<br/>hash₀"]
    E0 --> E1["seq 1<br/>hash₁"]
    E1 --> E2["seq 2<br/>hash₂"]
    E2 --> H["head<br/>publish this"]
```

### Why two hashes and not one

The rule was originally one hash over the whole entry. It was changed **before
any released format existed**, for a specific reason:

The chain makes the log latch the **serialisation point of every write**, and
hashing the payload — the bulk of an entry — *inside* that latch was the largest
serialised cost left after the tail was sharded.

With the digest computed by the writer **outside** the latch, the serialised
section is a 40-byte hash.

The change is pinned by a golden test whose literal was updated exactly once and
which carries the previous value. Logs written under the old rule do not verify
under the new one and were regenerated. After 0.1 the rule is frozen.

## What a chain can and cannot attest

**Can:** any in-place mutation or reorder breaks every subsequent hash.

**Cannot:** **truncation.** A prefix of a valid chain is itself a valid chain.

Detecting truncation requires comparing against an **externally attested head**
— which is why `ChainVerify::Intact`'s `head` is a *value to publish*, not an
internal. Publishing it somewhere the database cannot reach is the whole
mechanism.

## Verification never needs a key

The chain hashes payloads as they stand. A replication site can therefore verify
integrity — every entry, the whole chain — while being **structurally unable to
read a byte of tenant data**.

Integrity and confidentiality do not trade against each other here, and there is
a test pinning exactly that: `verification_requires_no_key`.

## The WAL

The durable sink, in both on-disk layouts: `--data-dir` puts it at the root, and
`--paged-dir DIR` carries `DIR/engram.wal` in front of the paged tail.

`engram.wal` opens with a 64-byte header — this one is a **genesis** file:

```text
00000000: 454e 4752 5741 4c31 0000 0001 0000 0000  ENGRWAL1........
00000010: 0000 0000 114b bb63 f1fd 971c 8735 92bd  .....K.c.....5..
00000020: 6483 b998 fea7 d0db 04e8 5770 29c5 c80b  d.........Wp)...
00000030: 844c 1b8e 0000 0000 0000 0000 0000 0000  .L..............
```

Magic `ENGRWAL1`, format version, then the anchor, then 12 reserved bytes.

### Zero-filled ahead, synced with `fdatasync`

The file runs on past its last record. The WAL writes **zero-filled space ahead
of its end** — 256 KiB when it is opened or rotated, then extensions that double
up to 8 MiB each, written whenever less than a quarter of the last step is left
— and a commit syncs with `fdatasync` rather than a full `fsync`.

The two go together. A sync after an append that grows the file must also
journal the new file size; an append into blocks that were already written and
synced is a data-only sync. The space is **written** with zeros rather than
extended with `set_len` or `fallocate`, because on XFS and ext4 only written
blocks give that. Should an append outrun the space, Linux's `fdatasync` still
flushes the size change, so the durability promise holds either way. The
creation, rotation and open paths keep the full `fsync`.

So the file's size says nothing about how much log it holds. Recovery reads the
zeroed space as the end of the log and **truncates** it, exactly as it
truncates a torn tail — it must, since bytes past the last valid record could
hold a complete, chain-valid record that was never acknowledged — then writes
the space afresh and syncs it before the first append lands. `Wal::logical_len`
answers where the log ends, for tools that used to read the file size.

### The anchor, and the two forms of the file

The two fields between the version and the reserved tail are an **anchor**:
`first_seq`, the sequence of the first record in this file, and `prev_hash`, the
chain hash immediately before it.

A file that has never been checkpointed anchors at genesis — `first_seq` 0 and
the genesis hash, as above — and its chain is verified from there.

A **checkpoint** rotates the file below a sequence and writes the anchor of what
it now starts at, so its records verify against that anchor rather than against
genesis. That is what lets a durable prefix be dropped without breaking chain
verification: the records below the anchor are in sealed segments on disk. The
rotation also swaps the shared group-commit fsync handle to the new file, in the
same critical section as the sink, so a write acknowledged after a rotation
lands in the new file rather than being fsync'd on the old handle.

A rotated file is **refused** by the whole-history open, naming the sequence it
starts at, because replaying a suffix as the database would silently drop
everything before the anchor.

A foreign file is refused rather than touched:

```text
not an engram WAL (expected magic …, found …) — refusing to touch it
```

### Log, then publish

A write appends to the log and **only then** becomes visible in the tail. The
reverse order would let a crash between the two leave a version readable that no
log entry accounts for — divergence rather than data loss, and worse.

A named crash point sits between them, so the simulation can kill the process
exactly there and assert what recovery does.

### Group commit

A worker drains its inbox as a batch, appends every write and **holds every
reply**. No reply leaves before every record appended up to the end of the batch
is on disk — other workers' records included, because a write is visible on
append, before its sync, and a read in the batch may have observed one.

The sync itself belongs to one **flusher thread**, so no worker waits on the
disk:

- If the records the batch needs are already durable, and none of the worker's
  earlier hand-offs is still queued, the replies go at once. That test reads a
  lock-free **durable sequence number** — the log sequence up to which records
  are known to be on stable storage — rather than the mutex a running sync
  holds.
- Otherwise the worker hands the held replies to the flusher and takes its next
  batch. While a hand-off of its is queued, its later replies go through the
  flusher as well, so none overtakes an earlier one on its connection.
- The flusher drains every queued hand-off, pays **one** sync — which flushes
  and syncs everything appended before it runs, from every worker — releases
  the replies in order, and then runs the seal check.

With one client nothing queues during the sync, so it degrades to one sync per
write. With more, the requests that arrive while a sync runs share the next one.
`--no-group-commit` restores a sync inside every write as the A/B arm.

What group commit does **not** do is publish a write to readers only once it is
durable. A write is still visible on append, which is why a read's reply can
wait for a sync it did not ask for.

**A sync failure aborts the process.** The replies for that batch are unsent
and unacknowledged, the writes are already visible to readers on every worker,
and continuing would acknowledge writes that are not durable.

## Recovery

Replay verifies as it goes and **refuses** rather than continuing past damage:

| condition | outcome |
|---|---|
| a hash does not follow from its predecessor | refuses at that sequence, naming it |
| a sequence gap | refuses — an entry was removed or reordered |
| a malformed payload | refuses at that sequence |
| a rotated file opened as the whole history | refuses, naming the sequence it starts at |

The first three are distinct facts with distinct error variants, because "the
chain is broken" and "an entry is missing" call for different responses. The
fourth is not damage at all — the file is intact, and it is refused because it
is only part of the history.

## Truncation: two different things

The section title used to name one mechanism and now names two. Neither loses a
durable byte; both are about how much is kept in front of the segments.

**The in-memory log, released at a seal** — about **150 bytes per version**,
which would otherwise grow with every write for the life of the process.
`--keep-full-log` retains it, and is needed **only** by a change-data-capture
consumer tailing `log_tail`.

**The on-disk WAL, rotated behind a spill.** `Store::checkpoint_wal(seq)` drops
the records below `seq` from the file, because the segments the spill has just
written hold them, fsync'd. This is what bounds `engram.wal` in paged mode. Every
spill the server runs reports that boundary and rotates the file behind it —
the boot spill, the maintenance thread's storage pass, and `CALL
engram.checkpoint()`, which spills every resident segment and checkpoints the WAL
behind them so the next boot replays nothing into a fresh segment. A failed
checkpoint is reported and is not a durability event: the file keeps its prefix
and grows until a later checkpoint succeeds.

## The replica

Not on the serving path, but built, and it sets the standard the rest has to
meet.

`Replica::apply` recomputes the chain **entry by entry against its own head**, so
a tampered entry, a fork or a gap refuses at the entry that broke, with its
sequence named. Retransmitted already-applied entries are skipped idempotently —
catch-up overlaps are normal — but a **future** sequence is a gap and is never
skipped, because *"skip the hole and keep going" is how a replica silently
diverges while reporting healthy.*

`verify_restore` takes the entries and the restored store and checks the chain,
the counts, and — decisively — that the restored log's head equals the head
recomputed from the source entries. **The push's own account of itself is never
the evidence.**

These are primitives. There is no second node, no log shipping exercised end to
end, and no tooling. See [Roadmap](../roadmap.md).

## Next

- [Durability and recovery](../using/durability.md) — what this buys you.
- [The storage engine](./storage-engine.md) — what publishes after the log.
- [Key encoding](./key-encoding.md) — the chain rule as a frozen format.
