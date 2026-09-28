# Durability and recovery

Engram has **four storage modes**, and they lose different things. Choosing
one is the most consequential flag decision you will make, so this page states
what each survives and then demonstrates it.

| mode | flag | a crash loses |
|---|---|---|
| In-memory | *(default)* | **everything** |
| WAL-durable | `--data-dir DIR` | **nothing acknowledged** |
| Paged | `--paged-dir DIR` | **nothing acknowledged** |
| Bulk ingest | `--bulk-ingest` | **everything since the load began** |

`--data-dir` and `--paged-dir` are two on-disk layouts under one durability
model rather than two durability models. Both carry a write-ahead log, both
`fsync` before they acknowledge, and both lose nothing that was acknowledged;
what differs is how much of the corpus has to be resident. They are mutually
exclusive, and the server exits rather than guess.

`--bulk-ingest` is the exception, and is refused with `--data-dir`. A bulk load
writes no log records at all, so while one is running the paged tail genuinely
is volatile — that is the trade the mode announces.

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
an anchor — the sequence of the first record in the file, and the chain hash
immediately before it. A log that has never been rotated anchors at sequence 0
and the genesis hash; a paged checkpoint moves the anchor forward.

A paged directory holds the same two entries, with its segments beside them:

```text
paged/
  LOCK             the directory lock
  engram.wal       the write-ahead log fronting the unsealed tail
  seg-<seq>.seg    one file per sealed segment
```

The two layouts are not interchangeable even though both hold a file called
`engram.wal`: a paged directory's log is a checkpointed suffix, and pointing
`--data-dir` at one is refused rather than replayed. See
[What recovery refuses](#what-recovery-refuses).

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

## Paged — bigger than RAM, and durable

```sh
engram-server 127.0.0.1:7687 --paged-dir ./paged --paged-cache-mb 4096
```

Sealed segments spill to `seg-<seq>.seg` files and are read block-by-block
through a bounded cache, so the **working set** rather than the corpus sets the
memory floor.

`DIR/engram.wal` fronts the unsealed tail, and it is not something you switch
on: `--paged-dir` opens it unconditionally and no flag selects it. The ordering
is the one `--data-dir` gives — appended and `fsync`ed **before** the
acknowledgement — and the log is replayed into the tail on open. A spill then
checkpoints it behind the segments it wrote.

So paged mode is the combination of a graph larger than memory and the
durability promise of `--data-dir`. The two flags choose a layout, which is to
say how much of the corpus has to be resident. They do not choose what a crash
costs.

The startup line is worth reading rather than skipping. It names the cache
budget, how many segments are on disk, the path of the WAL, and **how many
versions the open replayed out of it** — that last number is the tail the
restart recovered, which is what tells you the log did its job.

What the log does not hold in paged mode is the whole history: a checkpoint
drops the prefix the segments already carry. A paged WAL therefore restores the
tail, not a past point in time — and there is no point-in-time-restore tooling
in either mode, which is stated with the rest of the absences
[below](#what-does-not-exist).

### Demonstrated

The demonstration worth having in this mode is the one where nothing had been
sealed and the data came back regardless, because that is precisely what the WAL
buys. The engine asserts it: 50 acknowledged writes into a paged directory, a
`kill -9`, and then two restarts, each required to answer 50
(`acknowledged_writes_survive_kill_9_in_paged_mode`, in the server's durability
suite). The second restart is not redundant — it is what checks that the replay
is idempotent, that the first restart's replayed tail is still the tail.

The store's own suite pins the pieces underneath it: that writes after a paged
open survive a crash, that a spill checkpoints the WAL behind the segments it
wrote, that a crash between a spill and its checkpoint replays no row twice, and
that a torn tail after a rotation costs the tail rather than the prefix.

Earlier versions of this page showed a paged restart answering `0`, and quoted a
startup banner announcing that the tail was volatile. That was true of a paged
store before the WAL fronted its tail; it is not true of this build, and the
banner is not a line this binary prints. Both are removed rather than re-taken,
because a transcript nobody has re-run is worse than none.

### Forcing a checkpoint

```cypher
CALL engram.checkpoint() YIELD spilled, segments, resident, tail
RETURN spilled, segments, resident, tail
```

Paged mode only; refused otherwise rather than silently doing nothing.

It seals the tail and spills every resident sealed segment into the paged
directory, so `resident` falls and `segments` rises. It does **not** rotate the
WAL: it spills without asking for the durable boundary back, so nothing
checkpoints the log behind it and the next open still replays the same records.

The rotation is the automatic spill's, not this call's — the boot spill and the
maintenance thread's storage pass take the boundary from the spill and
checkpoint below it. When they do, the log is rotated to an anchor — the
sequence of its first surviving record, and the chain hash immediately before
it — so the records it keeps still verify as a chain rather than as a suffix of
one. The group-commit `fsync` handle moves to the successor inside the same
critical section, so a commit racing the rotation fsyncs the old file or the new
one and never the old handle for a record only the new file holds; and the
successor is complete before it is renamed over its predecessor, so a crash
mid-checkpoint leaves one whole file or the other.

This is what a drain-before-shutdown hook calls. It is not what makes paged mode
durable — the WAL already does that, and a hook that never ran costs no
acknowledged write. What it buys is a bounded resident set at shutdown, not a
shorter replay.

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
| the WAL was rotated by a paged checkpoint | refuses — *"WAL was rotated (its chain starts at seq N, not genesis): it fronts a paged store's sealed segments and cannot be opened as a whole-history log"* |
| the directory cannot be opened | **panics** |

That last one is deliberate. Starting empty over a data directory that was
explicitly requested would look like an empty database rather than a failed
open — which is how a restore gets overwritten.

The rotated row is the one an operator meets by accident, because both directory
layouts hold a file called `engram.wal`: pointing `--data-dir` at a directory
that was served with `--paged-dir` reaches exactly this refusal. It refuses
rather than opening partially because the records below the anchor live in the
segments, and replaying the suffix as the whole database would drop them
silently.

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
| `--seal-after N` | 65,536 versions | how much sits in the write tail before it is sealed — what a restart replays, and what a span read may have to merge. Not what a crash costs, except under `--bulk-ingest`, where the tail carries no log records |
| `--compact-after N` | 8 segments | how many segments a point read may walk |
| `--no-group-commit` | *(group commit on)* | one fsync per write instead of per batch |
| `--compact-every S` | off | paged only: a time floor under full compaction |

See the [Tuning guide](../reference/tuning.md).

## Next

- [Operations](./operations.md) — running, sizing, monitoring.
- [Transactions and isolation](./transactions.md) — what a commit means.
- [The storage engine](../architecture/storage-engine.md) — where versions live.
