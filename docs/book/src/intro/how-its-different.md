# How Engram is different

Engram is a **single-process graph server** with an **MVCC log-structured
store**, a **columnar pipeline with injected morsel parallelism**, and
**adjacency held as a derived structure rather than as storage**. Every one of
those is a choice with a live alternative, and this page is about what each
buys and costs against the systems you have probably already used.

The short version: Engram is architecturally closest to an LSM key-value store
with a graph model welded on, serving a wire protocol. That shape makes writes
and point lookups cheap by construction; the analytical traversals were won
back operator by operator, and — as of the most recent same-window
measurement — they were won. See [the measured standing](#the-measured-standing).

## The comparison set

| system | shape | storage | execution |
|---|---|---|---|
| **Neo4j** | JVM server, clusterable | fixed-size record store, doubly-linked relationship lists, page cache | row-at-a-time pipelined runtime |
| **Kuzu / LadybugDB** | embedded library | columnar, CSR adjacency as join indices | vectorized + factorized, WCOJ |
| **DuckDB** | embedded library | columnar, MVCC | vectorized push-based, morsel-parallel |
| **SQLite** | embedded library | B-tree pages, row store | row-at-a-time VM |
| **Engram** | single-process server | MVCC LSM: sharded memtable + immutable sealed segments | columnar `DataChunk` pipeline with morsel parallelism, row-at-a-time fallback |

Kùzu Inc. archived the Kuzu repository in October 2025; **LadybugDB** is the
community continuation, with the same `Database`/`Connection` API, Cypher
dialect and columnar storage. Where this page says "Kuzu-style" it means the
architecture both share.

## Axis 1 — deployment shape

Engram is a **server**, not a library. It binds a port and speaks Bolt, so
stock Neo4j drivers connect and your application talks to it over a socket.

That puts it on Neo4j's side of the biggest fork in this space. Kuzu,
LadybugDB, DuckDB and SQLite are **embedded**: they run inside your process,
there is no wire protocol, and a query result crosses a function-call boundary
rather than a socket.

The trade is the usual one, and neither side is subtle about it:

- **Embedded** wins on latency floor (no serialisation, no syscall), on
  operational simplicity (no daemon), and on analytical throughput (the engine
  can hand you Arrow buffers directly).
- **A server** wins when more than one process, machine or language needs the
  same graph concurrently, and when you want the database's lifetime to be
  independent of any one application's.

Engram takes the server side but keeps the **single-process** part: one shard,
one process, no coordinator. It is closer to "SQLite with a socket" than to a
distributed database, and [Known limits](../known-limits.md) is explicit that
there is no clustering, replication or failover.

## Axis 2 — how a graph is actually stored

This is the deepest architectural difference, and the one most consequences
follow from.

**Neo4j** stores fixed-size records in dedicated stores, with relationships as
doubly-linked lists threaded through both endpoint nodes. Traversal is pointer
chasing: read a node record, follow its relationship chain, read the next node.
It is very good at exactly that and it is why Neo4j's traversal has a low
constant factor.

**Kuzu-style systems** store node and relationship properties columnar, and
adjacency as **CSR** (compressed sparse row) — a sorted, contiguous array of
neighbours per node, with an offset directory. CSR is the right shape for
set-at-a-time traversal: a hop is a slice, and slices vectorize.

**Engram stores neither.** The authoritative representation is an **MVCC
log-structured store** of versioned key-value rows:

```text
realm:u32 │ namespace:u32 │ KIND:u8 │ partition:u32 │ <body> │ !commit_ts:u64
```

A node is a record row. A relationship is a record row **plus two half-edge
index rows** — one under the source, one under the destination:

```text
'O' ‖ src:u64 ‖ type:u32 ‖ dst:u64 ‖ rel:u64     out half-edge
'I' ‖ dst:u64 ‖ type:u32 ‖ src:u64 ‖ rel:u64     in  half-edge
'L' ‖ label:u32 ‖ node:u64                        label membership
```

Because the key encoding is memcomparable, "every out-edge of node *n* of type
*t*" is a **contiguous range scan** — the sort order is the index. There is no
separate adjacency structure on disk at all.

### Adjacency is derived, not stored

The CSR tables Kuzu keeps as *storage*, Engram builds as a **derived
structure** — a cache over those half-edge rows, rebuildable from them at any
time, caught up incrementally from a change log.

That is a real architectural position with real consequences:

- **Writes are cheap and uniform.** Adding an edge appends rows. There is no
  CSR to rebuild, no adjacency list to splice, no page to rewrite in place.
  This is the LSM bargain, and it is why Engram's write path and point lookups
  are its strongest suit.
- **Reads pay for freshness.** A traversal wants a CSR, and the CSR may be
  stale after a write burst. Engram's answer is a maintenance thread that
  refreshes derived structures in the background, plus a startup warm that
  builds them before the listener opens — which is what the
  `warmed in … ms: … adjacency table(s)` line at startup is doing.
- **The failure mode is a first-query stall**, not corruption. If a write-only
  burst happens with no reader between, the next reader can find a large change
  set to catch up on. That is the single most important operational
  characteristic of this design, and it is why
  [Derived structures](../architecture/derived-structures.md) exists as a page
  and `--refresh-after-writes` exists as a knob.

Neo4j and Kuzu do not have this failure mode, because their adjacency *is* the
storage. Engram trades that for write-path simplicity and for the ability to
serve a graph larger than memory out of immutable, block-addressed segments.

### Immutability, and bigger-than-RAM

Sealed segments are **immutable** and read **lock-free**. That is the LSM
property Neo4j's mutable page cache does not have, and it is what makes
[paged mode](../architecture/paged-mode.md) possible: sealed segments spill to
disk and are read block-by-block through a bounded cache, so the **working
set** rather than the corpus sets the memory floor.

DuckDB reaches a similar place from the other direction — columnar blocks,
read-mostly — but for analytical scans rather than for graph traversal.

## Axis 3 — execution model

All four are converging on the same vocabulary — vectors, morsels, ordered
merges — and Engram has most of it. Where it is still behind is in the
*operators*, not the framework.

- **DuckDB** is the reference implementation of vectorized push-based execution
  with morsel-driven parallelism: operators exchange batches of ~2048 values,
  and work is split into morsels across threads.
- **Kuzu** adds **factorization** — keeping intermediate results in a
  compressed, un-flattened form so a many-to-many join does not materialise the
  cross product — plus worst-case-optimal joins for cyclic patterns.
- **Neo4j** is row-at-a-time but with a mature runtime and, crucially,
  **set-at-a-time variable-length expansion**: a bounded `*1..n` followed by
  `DISTINCT` becomes a frontier BFS over a visited set.
- **Engram** has a columnar **`DataChunk`** pipeline — the same vector model
  Kuzu and GraphflowDB use — carrying id vectors with a selection vector, plus
  **morsel-parallel** `expand` and count-fold operators. Underneath it sits a
  row-at-a-time streaming interpreter as the general fallback for shapes the
  pipeline declines.

### The parallelism seam is unusual, and worth its own paragraph

Engram's morsel parallelism is **injected through a trait**, not built in:

```rust
pub trait ScopedExec: Send + Sync {
    fn width(&self) -> usize;
    fn for_each(&self, n: usize, f: &(dyn Fn(usize) + Sync));
}
```

`DataChunk::expand` splits its driving rows into morsels, runs them through
whatever `ScopedExec` is installed, and concatenates the partials **in morsel
order** — so a parallel run is byte-identical to the serial one, proven per
operator by an A/B differential with a fired-counter canary.

Who implements the trait decides everything, and there are three answers:

| implementor | what runs |
|---|---|
| **absent** (default, and always in the simulation lane) | operators take their serial paths; one lever check on the hot path |
| `SerialExec` | width 1, inline — the *parallel machinery* (morsel split, slot collection, ordered merge) exercised **deterministically** |
| the server's thread-scope pool | real OS threads, work-stealing off an atomic cursor, behind `ENGRAM_QUERY_PARALLELISM` |

That middle row is the architecturally interesting one. Because the engine
itself never spawns a thread — a house rule with a lint behind it — the
morsel machinery can be run single-threaded through the *same code path* the
parallel pool uses. The deterministic simulation therefore covers parallel
execution logic without threads, and the determinism digest is unaffected by
whether parallelism is available. DuckDB and Kuzu, which own their thread
pools, cannot make that separation.

The dispatch gates are stated rather than assumed, and each is load-bearing:
the lever is off by default; an executor must be installed; there must be **no
active transaction on the thread** (the read-your-writes overlays and the OCC
read-set are thread-local, so a worker would silently read committed state and
record nothing); there must be enough driving rows to beat the split's
overhead; and weighted chunks stay serial.

Read semantics are stated too: a morsel worker reads exactly as the serial loop
it replaces — read-committed per row against the visible clock. Parallelism
changes which rows are "later", not the anomaly class.

### Two paths, and which one a statement gets

Engram keeps **both** an enumerating and a set-at-a-time implementation of the
expensive operators, and admits a statement to the faster one only when it can
prove the answer is unchanged.

**Variable-length expansion** has both. `expand_var_length` is depth-first over
rel-distinct walks; `expand_var_length_bfs` is a **frontier BFS with a visited
set**, where each reachable node is produced once at its shortest depth — so
"the O(paths) flat rows the enumerating path builds and then collapses at the
`DISTINCT` never exist." The admission conditions are stated in the source:
`min == 1`, no relationship or path variable, no relationship-property test,
and an end the breaker consumes `DISTINCT`-only. Anything else takes the
enumerating path, unchanged.

**`shortestPath`** likewise. `try_shortest_path_bfs` computes it by BFS over a
visited-node set — bidirectional for an unbounded `*`, a memoised forward BFS
tree for a bounded `*..max` — and the source records exactly why: it replaces
an enumeration that "exhausts the process on an unbounded `(a)-[:KNOWS*]-(b)`
— the LDBC IC13 OOM". It handles the shape IC1 and IC13 take, with both
endpoints bound; every other shape falls back, so no accepted behaviour
changes.

**Intermediate representation.** The pipeline's `DataChunk` is the Kuzu-style
vector model — one id column per variable, a selection vector that filters
shrink while **id columns are never copied**, and properties fetched lazily by
id. What it does not have is general *factorization* for projection. It does
have a targeted one for counting: count-fold weights, where a folded hop
multiplies a row's weight instead of materialising rows, and the product is the
same count.

So the honest statement is not "Engram lacks these operators" but "Engram has
them behind admission conditions, and the general path underneath is the
fallback when a statement does not qualify." The remaining work is widening
what qualifies.

Notably, the survey **rejected** worst-case-optimal joins for this workload:
WCOJ wins on *cyclic* patterns and is substantially slower than binary joins on
acyclic ones, and the friends-of-friends shape that dominates these queries is
acyclic. Deciding that a celebrated technique does not apply is as
architectural as adopting one.

## The measured standing

Architecture arguments are cheap, so here is what was actually measured.

**Against Neo4j 5.26-community at LDBC SNB SF1, in same-window paired runs on
one host, N=3 medians, with identical answer counts on every query: Engram
leads on all nineteen workloads.**

LSQB — the Labelled Subgraph Query Benchmark, which is *complex analytical
joins*, not point lookups:

| query | engram (ms) | Neo4j (ms) | ahead |
|---|---:|---:|---|
| q1 | 266 | 9,284 | **34.9×** |
| q2 | 886 | 1,716 | **1.94×** |
| q3 | 5,614 | 14,171 | **2.5×** |
| q4 | 584 | 14,948 | **25.6×** |
| q5 | 2,176 | 10,588 | **4.9×** |
| q6 | 658 | 35,646 | **54.2×** |
| q7 | 3,328 | 16,477 | **5.0×** |
| q8 | 2,647 | 37,825 | **14.3×** |
| q9 | 5,086 | 53,282 | **10.5×** |

The stress suite, ten mixed read/write profiles at one and eight clients —
twenty levels, ahead on every one:

| profile | @1 | @8 |
|---|---|---|
| read-only | 1.46× | 1.35× |
| read-heavy | 1.35× | 1.45× |
| balanced | 1.82× | 1.58× |
| write-heavy | 3.94× | 1.58× |
| write-only | 5.64× | 1.41× |
| contention | 1.39× | 1.46× |
| rel-create | 8.21× | 6.71× |
| rel-hub | 9.00× | 7.03× |
| unique-create | 10.76× | 5.54× |
| delete-churn | 8.25× | 4.88× |

The write-side margins are the LSM bargain from
[Axis 2](#adjacency-is-derived-not-stored) showing up as numbers: relationship
creation, hub writes, unique-constrained creates and delete churn are where an
append-only store with derived adjacency should win, and it does.

### The q2 story, because it is the honest one

q2 is the smallest margin on the board at 1.94×. It began the campaign at
**14.6× behind**, and the path from there is the useful part: a join order
driven from the wrong side, then a repeated estimation, then an unpriced first
estimation. **Three defects, none of them in the executor.**

That is worth knowing before concluding anything from architecture alone. The
analytical gap this page's execution-model section describes was real, and most
of what closed it was planning rather than operators.

### What these numbers do *not* say

The measurement discipline here is strict, so the caveats travel with the
result:

- **SF1 only.** SF10 and SF100 have not been run. Do not read these margins as
  scale-free; the project's own plan says so in as many words.
- **Neo4j 5.26-community**, not Enterprise.
- **The SNB Interactive suite has not been re-baselined.** Its last numbers
  predate the memos, the estimator and clause fusion, and several were losses.
  Until it is re-run, treat IC as unmeasured rather than as won or lost.
- **Kuzu and LadybugDB have not been measured head to head.** Only a smoke pass
  exists, explicitly not a measurement, and this page does not quote it.
- **`shortestPath` is fast only for the bound-endpoint shape.** Other shapes —
  multi-hop, `min > 1`, an unbounded endpoint — still fall back to enumeration.

So the defensible claim is precise: *against Neo4j community at SF1, on these
nineteen workloads, in the same window, Engram leads on all of them.* It is not
"Engram is faster than every graph database", and the project's own house rules
would refuse that sentence.

## Axis 4 — concurrency and isolation

| system | writers | isolation |
|---|---|---|
| Neo4j | multi-writer, locks | read committed by default, serializable available |
| DuckDB / Kuzu | effectively single-writer | snapshot |
| Engram | multi-writer, MVCC + OCC | reads are **read committed**; a transaction validates its **read set as well as its writes** |

Engram's choice is deliberate and argued at length in the store's own source,
and it is two decisions rather than one.

**Individual reads are read committed**, because the correctness of a
compare-and-swap depends on **write-lock-then-read** ordering — take the lock,
read the *current* committed value, compare, write. Under snapshot isolation
the read returns the transaction's snapshot instead, **every contender's guard
passes**, and ownership or governance writes lose updates silently: no error,
no log line, just the last writer winning a race nobody knew had run. `cas` is
the supported primitive and its body *is* that ordering.

**A transaction validates its read set as well as its write set** at commit.
The source is explicit that this is what "lifts this above bare snapshot
isolation toward serializability: a stale READ that fed a decision aborts
rather than commits." Autocommit statements get the same treatment by default —
the Bolt adapter runs each one as a read-validated transaction, with a canary
test that demonstrates the lost increments when it is turned off.

What is *not* closed by default is phantoms; `--precision-locking` closes them
and is documented as a behaviour change, because it aborts statements that
currently commit.

So `Store::cas` is the supported primitive and its read happens after lock
acquisition *by construction*. Phantom protection is available as an opt-in
isolation upgrade (`--precision-locking`), stated as a behaviour change because
it aborts statements that currently commit.

See [Transactions and isolation](../using/transactions.md).

## Axis 5 — the dependency and safety posture

Less glamorous, occasionally decisive:

- **No `unsafe`.** The workspace denies it outright.
- **39 third-party crates**, all permissively licensed, no copyleft anywhere.
- **A native dependency may need one `cc` invocation** — no cmake, no bindgen,
  no external shared library, no build-time process. Enforced by a gate that
  keys on `links`/build-script presence rather than on crate names.
- **No JVM**, no GC pauses, and a single static binary.

Against Neo4j that is the sharpest practical contrast: no JVM to tune, no heap
sizing, no GC pause at the 99th percentile.

## So when would you actually choose it?

Honestly:

- **Choose Kuzu/LadybugDB or DuckDB** if your workload is analytical, embedded
  suits you, and you want the best vectorized execution available today.
- **Choose Neo4j** if you need clustering, authentication, a mature operational
  story, or heavy analytical traversal right now.
- **Engram is interesting** when you want a graph over a *wire protocol*
  without a JVM or a cluster, your workload is write-heavy or point-lookup
  heavy, you need a graph larger than memory from a single process, or you
  value the reproducibility properties below.

And it is **pre-release**: no authentication, no TLS, no backup tooling. Read
[Known limits](../known-limits.md) before that list of trade-offs matters to
you at all.

## The reproducibility property, briefly

One architectural consequence is not about performance. Because time,
randomness, scheduling and I/O are injected rather than reached for, and
because the engine never spawns — parallelism is installed rather than
assumed — **a seed reproduces a run exactly**: the same seed in two processes
yields a byte-identical event trace.

That is unusual in this company and it is why the codebase looks the way it
does. It has its own page: [The three decisions](../architecture/three-decisions.md).

## Next

- [Architecture overview](../architecture/overview.md) — the map of the engine.
- [The storage engine](../architecture/storage-engine.md) — the LSM in detail.
- [Derived structures](../architecture/derived-structures.md) — the adjacency
  design, and its operational consequences.
- [Measurements](../measurements/index.md) — the numbers, each with its corpus
  and command.
