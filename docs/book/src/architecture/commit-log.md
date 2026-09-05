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

The durable sink. `engram.wal` opens with a 64-byte header:

```text
00000000: 454e 4752 5741 4c31 0000 0001 0000 0000  ENGRWAL1........
00000010: 0000 0000 114b bb63 f1fd 971c 8735 92bd  .....K.c.....5..
00000020: 6483 b998 fea7 d0db 04e8 5770 29c5 c80b  d.........Wp)...
00000030: 844c 1b8e 0000 0000 0000 0000 0000 0000  .L..............
```

Magic `ENGRWAL1`, format version, and the genesis hash.

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

A worker drains its inbox as a batch, appends every write, **holds every reply**,
pays one `fsync`, and only then releases them.

With one client nothing queues during the fsync, so it degrades to one fsync per
write. With eight, the fsync is long enough that all eight send their next
request during it, so the next batch shares one fsync eight ways.

**An `fsync` failure aborts the process.** The replies for that batch are unsent
and unacknowledged; continuing would acknowledge writes that are not durable.

## Recovery

Replay verifies as it goes and **refuses** rather than continuing past damage:

| condition | outcome |
|---|---|
| a hash does not follow from its predecessor | refuses at that sequence, naming it |
| a sequence gap | refuses — an entry was removed or reordered |
| a malformed payload | refuses at that sequence |

Three distinct facts with three distinct variants, because "the chain is broken"
and "an entry is missing" call for different responses.

## Truncation at a seal

The in-memory log is released at a seal — about **150 bytes per version**,
growing with the corpus, and the term that put one paged load at ~17 GB.

`--keep-full-log` retains it, and is needed **only** by a change-data-capture
consumer tailing `log_tail`.

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

- [Durability and recovery](../using/durability.md) — what this buys you,
  demonstrated.
- [The storage engine](./storage-engine.md) — what publishes after the log.
- [Key encoding](./key-encoding.md) — the chain rule as a frozen format.
