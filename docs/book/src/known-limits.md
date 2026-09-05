# Known limits

Stated as **absences**, because a list of features implies the rest exist.

Read this page before you rely on anything here. It is the honest list, and it
is deliberately the easiest page in the book to find.

**Read it alongside the [Roadmap](./roadmap.md).** Almost everything below is a
release state with a design behind it rather than a design position — the
security absences in particular are the named next milestone. Where the two
pages disagree, this one is about today and that one is about intent.

## Security

- **No authentication.** Credentials are accepted and *not verified*. Any
  username and password — including none — opens a session with full access.
- **No TLS.** All traffic is plaintext, including those unverified credentials.
- **No audit log.** Authentication attempts, session lifecycle, DDL and
  authorization denials are not recorded anywhere.
- **No authorization model.** There are no users, roles or grants to configure.

The practical consequence: an Engram port is an unauthenticated database. Bind
it to a loopback interface or an isolated network segment and nothing else. See
[Security posture](./using/security.md).

## Availability and operations

- **No clustering, no failover.** One process. The availability story is
  "restart it".
- **No replication in service**, though the *primitives* exist and are not
  toys: a replica that recomputes the hash chain against its own head and
  refuses at the entry that broke, restore verification independent of the
  writer, and a log tail for change data capture. What does not exist is a
  second node, log shipping exercised end to end, or any of the machinery
  around them. See [Roadmap](./roadmap.md).
- **No backup or point-in-time-restore tooling.** The write-ahead log and the
  segments are the durable artifacts, and `recover_to` / `verify_restore`
  exist as primitives, but there is no supported procedure around them — no
  `BACKUP`, no snapshot command, no documented restore path beyond copying
  files while the server is stopped.
- **No online schema migration tooling.**
- **No configuration file.** Everything is CLI flags and environment
  variables; a config file is deferred work.

## Durability, per mode

The mode you choose changes what a crash costs. This catches people, so it is
here as well as in [Durability and recovery](./using/durability.md).

| mode | flag | what a crash loses |
|---|---|---|
| In-memory | *(default)* | **everything** — announced loudly at startup |
| WAL-durable | `--data-dir DIR` | nothing acknowledged; the log is `fsync`ed before the ack |
| Paged | `--paged-dir DIR` | **the unsealed tail** — durability is at seal boundaries only |
| Bulk ingest | `--bulk-ingest` | **everything since the load began** — durability is by re-ingest, not replay |

Paged mode is the bigger-than-RAM serving mode, not the durable one. It cannot
be combined with `--data-dir`, and the server refuses at startup if you try.

## Cypher

- **`=~` (regex) parses and then refuses at evaluation.** The grammar accepts
  it; the evaluator returns an error. There is no regex crate in the workspace.
- **`UNION` inside `CALL { }`** is refused.
- **Standalone `CALL` returns no rows.** Unlike Neo4j, a bare `CALL proc()` —
  with or without `YIELD` — produces nothing. Every procedure call needs an
  explicit `YIELD` followed by a `RETURN`:

  ```cypher
  CALL dbms.components() YIELD name, versions, edition
  RETURN name, versions, edition
  ```

- **Full-text search is term-frequency, not BM25.** The tokenizer splits on
  non-alphanumerics and lowercases. No stemming, no stopwords, no configurable
  analyzer.
- **Vector index `OPTIONS` are parsed and uninterpreted.** You cannot set
  `vector.dimensions` or `vector.similarity_function`; the dimension is
  inferred from the data and the metric is cosine.
- **Not a Neo4j drop-in**, despite the wire compatibility. The driver
  compatibility matrix is open, not closed — see [Connecting](./using/connecting.md).

The conformance number is the useful counterweight to this list: 3,772 of 3,773
evaluated openCypher TCK scenarios pass, and the single failure is a time-zone
database expectation where this engine is arguably the more correct of the two.
Broad coverage and specific sharp edges are both true.

## Scale

- **One statement runs on one thread** by default. Morsel-parallel `expand`
  and count-fold exist and are byte-identical to their serial paths, but they
  are opt-in behind `ENGRAM_QUERY_PARALLELISM`. The *server* is multi-threaded
  regardless — `--workers N` engine threads over a shared MVCC store.
- **Results are fully materialised before they are paged to the client.** The
  driver's fetch size does not bound server memory; `--row-budget` does. See
  [Result paging](./using/result-paging.md).
- **`--row-budget` defaults to 20,000,000 rows.** A query that would exceed it
  is refused rather than allowed to reach the OOM killer, which refuses nothing
  and takes every other session with it.
- **The fast operators have admission conditions.** Frontier-BFS
  variable-length expansion needs `min == 1`, no relationship or path variable,
  no relationship-property test, and a `DISTINCT`-only end; BFS `shortestPath`
  needs both endpoints bound and a single hop. A statement outside those
  conditions takes the enumerating path, which is correct but can be
  dramatically slower. Widening admission is active work — see
  [Roadmap](./roadmap.md).
- **Performance is demonstrated at SF1 only.** The published margins are
  measured on one scale factor against one comparison configuration. SF10 and
  SF100 have not been run, so do not read them as scale-free — and the SNB
  Interactive suite has not been re-baselined since the campaign that produced
  them. [Measurements](./measurements/index.md) states this alongside the
  numbers rather than beneath them.

## What is built but not reachable

Four crates are compiled, tested and simulated, but nothing on the Bolt serving
path calls them today: `engram-crypto` (per-tenant keys, AEAD sealing),
`engram-objstore` (the object-storage seam), `engram-blob` (large media
referencing) and `engram-exec` (the bitmap operator seam). They exist because
the decisions they encode are the ones that cannot be retrofitted — see
[Seams not yet on the serving path](./architecture/seams.md). Do not read their
presence as a shipped feature.

## If one of these blocks you

That is useful information, and it is better arrived at from this page than
from a migration. The gaps above are gaps, not disguised design positions —
most have a design written down under [Design history](./history/index.md), and
[Roadmap](./roadmap.md) says which are planned, which are primitives awaiting a
product, and which were evaluated and deliberately deferred.
