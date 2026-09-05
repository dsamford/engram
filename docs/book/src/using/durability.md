# Durability and recovery

Engram has **three storage modes**, and they lose different things. Choosing
one is the most consequential flag decision you will make, so this page states
what each survives and then demonstrates it.

| mode | flag | a crash loses |
|---|---|---|
| In-memory | *(default)* | **everything** |
| WAL-durable | `--data-dir DIR` | **nothing acknowledged** |
| Paged | `--paged-dir DIR` | **the unsealed tail** |
| Bulk ingest | `--bulk-ingest` | **everything since the load began** |

`--data-dir` and `--paged-dir` are mutually exclusive, and the server exits
rather than guess. `--bulk-ingest` is refused with `--data-dir`.

## WAL-durable — the mode you want

```sh
engram-server 127.0.0.1:7687 --data-dir ./data
```

Every acknowledged write is `fsync`ed **before** the acknowledgement, and a
restart replays the log. That ordering is the whole guarantee: if the client
saw success, the bytes were on disk first.

### Demonstrated

Three nodes written, then the process killed outright — `taskkill /F`, no
graceful shutdown, no chance to flush:

```text
before kill:  MATCH (d:Durable) RETURN count(d)  →  3
=== hard kill ===
after restart: MATCH (d:Durable) RETURN count(d)  →  3

[engram-server] durable: ./data/engram.wal
[engram-server] warmed in 1 ms: 3 nodes, 0 out-edges, 0 in-edges, …
```

The startup line counts the replayed corpus, so a restart tells you what it
recovered rather than leaving you to check.

### What is in the directory

```text
data/
  LOCK          the directory lock, held for the process lifetime
  engram.wal    the write-ahead log — 64 bytes when empty
```

The empty WAL is a 64-byte header: the magic `ENGRWAL1`, a format version, and
the genesis hash of the chain.

### The rule underneath it

**Log, then publish.** A write appends to the log and only then becomes visible
in the tail. The reverse order — publish, then log — would let a crash between
the two leave a version readable that no log entry accounts for, which is
divergence rather than data loss and is worse.

Between them sits a named crash point, so the deterministic simulation can kill
the process exactly there and assert what recovery does.

### Group commit

By default a worker drains its inbox as a batch, appends every write, **holds
every reply**, pays one `fsync`, and only then releases the replies.

With one client nothing queues during the fsync, so it degrades to exactly one
fsync per write — no regression. With eight, the fsync is long enough that all
eight send their next request during it, so the next batch shares one fsync
eight ways.

`--no-group-commit` restores one fsync per write. It exists for A/B measurement
and is slower under concurrent writers.

## Paged — bigger than RAM, and not the durable mode

```sh
engram-server 127.0.0.1:7687 --paged-dir ./paged --paged-cache-mb 4096
```

Sealed segments spill to `seg-<seq>.seg` files and are read block-by-block
through a bounded cache, so the **working set** rather than the corpus sets the
memory floor.

**Durability is at seal boundaries only.** A version reaches disk when its
segment is spilled; the unsealed tail is volatile. The server says so at
startup, unprompted:

```text
[engram-server] paged: ./paged — cache 4096 MiB, 0 segment(s) on disk.
                Durability at seal boundaries — the unsealed tail is LOST on
                crash; not the durable mode.
```

### Demonstrated

The same three-node test, in paged mode, killed the same way:

```text
before kill:   MATCH (v:Vol) RETURN count(v)  →  3
seg files on disk: 0
=== hard kill ===
after restart: MATCH (v:Vol) RETURN count(v)  →  0
```

Nothing had been sealed — three nodes is far below `--seal-after`'s default of
65,536 versions — so nothing was on disk, and the restart came back empty. This
is the documented behaviour working exactly as specified, and it is why paged
mode is the benchmark and bulk-serving mode rather than the durable one.

If you need both durability and a graph larger than memory, that combination
does not exist today. See [Roadmap](../roadmap.md).

### Forcing a checkpoint

```cypher
CALL engram.checkpoint() YIELD spilled, segments, resident, tail
RETURN spilled, segments, resident, tail
```

Paged mode only; refused otherwise. This is what a drain-before-shutdown hook
calls.

## In-memory — the default, announced loudly

With neither flag the store is in-memory:

```text
[engram-server] WARNING: in-memory only — a restart LOSES ALL DATA.
                Pass --data-dir DIR for durability.
```

A legitimate mode for tests and comparison runs, and a footgun as a silent
default — hence the warning.

## Bulk ingest

```sh
engram-server 127.0.0.1:7687 --paged-dir ./paged --bulk-ingest
```

For loading a corpus. Writes skip the commit log, ids reserve in ranges of
4096, and autocommit is not serialisable. **Durability is by re-ingest, not by
replay**: if the load dies, you start it again.

Refused with `--data-dir`, and you restart without it to serve normally.

## The directory lock

Taken **before the port is bound**:

```text
[engram-server] the data directory is locked by another process (pid 78972).
Two servers writing one store interleave their log records and leave a hash
chain that no recovery can verify.
If that process is NOT running, the lock is stale — remove ./data/LOCK and
start again.
```

Before the bind rather than merely before serving: a second server that binds
and *then* refuses has, for that moment, taken the port from the one that
legitimately holds the data — which during a restart race is exactly when it
happens, and turns a clean refusal into an outage.

A stale `LOCK` after a hard kill is expected. Confirm the pid is gone, then
remove it.

## What recovery refuses

Replay is not best-effort. It verifies the hash chain as it goes and refuses
rather than continue past damage:

| condition | outcome |
|---|---|
| an entry's hash does not follow from its predecessor | refuses at that sequence, naming it |
| a sequence gap | refuses — an entry was removed or reordered |
| a malformed payload | refuses at that sequence |
| the file is not an Engram WAL | refuses — *"not an engram WAL (expected magic …, found …) — refusing to touch it"* |
| the directory cannot be opened | **panics** |

That last one is deliberate. Starting empty over a data directory that was
explicitly requested would look like an empty database rather than a failed
open — which is how a restore gets overwritten.

## Integrity beyond replay

The commit log is a **BLAKE3 hash chain**: each entry hashes its predecessor's
hash together with its sequence, header and payload digest, from a genesis
constant.

Two properties follow, and they are why the chain exists rather than a
checksum:

- **Truncation is detectable only against an external attestation.** A prefix
  of a valid chain is itself valid, so the head hash is a value to publish
  somewhere else, not merely to keep.
- **Verification never needs a key.** The chain hashes payloads as they stand,
  so a replication site can verify integrity — every entry, the whole chain —
  while being structurally unable to read the contents.

The replica primitive recomputes the chain against its **own** head as it
consumes, refusing at the entry that broke, and never skipping a future
sequence — because "skip the hole and keep going" is how a replica silently
diverges while reporting healthy. It is a primitive, not a product; see
[Roadmap](../roadmap.md).

## What does not exist

- **No backup tooling.** No `BACKUP`, no snapshot command. The WAL and the
  segments are the durable artifacts, and the supported procedure around them
  is to stop the server and copy the directory.
- **No point-in-time-restore tooling**, though `recover_to` and
  `verify_restore` exist as primitives.
- **No replication in service.** See [Known limits](../known-limits.md).

## Tuning

| flag | default | effect |
|---|---|---|
| `--seal-after N` | 65,536 versions | how much sits in the volatile tail — in paged mode, how much a crash can lose |
| `--compact-after N` | 8 segments | how many segments a point read may walk |
| `--no-group-commit` | *(group commit on)* | one fsync per write instead of per batch |
| `--compact-every S` | off | paged only: a time floor under full compaction |

See the [Tuning guide](../reference/tuning.md).

## Next

- [Operations](./operations.md) — running, sizing, monitoring.
- [Transactions and isolation](./transactions.md) — what a commit means.
- [The storage engine](../architecture/storage-engine.md) — where versions live.
