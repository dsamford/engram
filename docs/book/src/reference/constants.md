# Compiled-in constants

Thresholds baked into the binary. **None of these has a CLI flag** — changing
one means editing the source and rebuilding.

They are documented because they explain behaviour you will otherwise meet
without an explanation: why a table is not built, why a scan declines, why a
read walks a fixed number of segments. Values were read from the source rather
than from memory.

## Storage

| constant | value | what it governs |
|---|---|---|
| `TAIL_SHARDS` | **64** | shards in the writable tail, each behind its own latch — a key maps to one, so writers to different keys rarely contend |
| `VISIBILITY_RING` | **4096** | in-flight commit timestamps tracked at once |
| `COMMIT_WINDOW_CAP` | **65,536** | the recent-commit window OCC validation consults. Past it, validation falls back to a point lookup per key — which is also the better answer for a long-running transaction, whose window would be enormous |
| `TAIL_COPYOUT_CAP` | **65,536** | rows a span read may copy out per shard. Past it the read **declines** rather than truncating |
| `COLUMN_SCAN_BYTE_BUDGET` | **256 MiB** | bytes one columnar scan may read before declining to the general path |
| `COLUMNAR_MIN_ROWS` | **64** | below this a signature stays in row form; columnar layout does not pay |
| `PRECISION_MAX_DELTA` | **4096** | rows the precision-locking predicate pass examines before giving up |
| `MAX_CAS_RETRIES` | **32** | bounded CAS retries on an adjacency chunk, then `ContentionExhausted` |
| `CHUNK_CAPACITY` | **1024** | peers per adjacency chunk |

### On-disk format

| constant | value | |
|---|---|---|
| `FORMAT_VERSION` | **3** | the segment format |
| `TARGET_BLOCK_BYTES` | **16 KiB** | the SST data-block close threshold — the unit of caching, reading and verification |
| block hash | **32 bytes** | a BLAKE3 trailer per block |
| `BlockCache` shards | **8** | with a probation queue at 10% of each shard's budget (S3-FIFO-lite) |
| `LOCK_FILE` | `"LOCK"` | the directory lock |

A 16 KiB block is the granularity of everything in paged mode: what the cache
holds, what a read faults in, and what a BLAKE3 hash covers.

## Derived structures

These are the ones most likely to explain a surprise.

| constant | value | what it governs |
|---|---|---|
| `ADJ_TABLE_MAX_ENTRIES` | **64 M** | entries one adjacency table may hold; past it the table declines to be built |
| `ADJ_TABLE_CACHE_MAX` | **512** | tables cached at once. It was 32 — exactly one benchmark's need — and evicted its own warm work |
| `ADJ_LOG_CAP` | **262,144** | change-log entries per `(side, type)`, about 4 MB per log |
| `ADJ_OVERLAY_FOLD` | **4096** | overlay rows before folding *(settable: `--adj-overlay-fold`)* |
| `ADJ_REPAIR_MAX` | **4096** | changed nodes one repair may handle |
| `ADJ_READER_REPAIR_MAX_ROWS` | **8192** | rows a *reader* may repair, about 256 changed nodes |
| `ADJ_CHANGE_FILTER_SLOTS` | **262,144** | the per-node staleness filter — 2 MiB per graph |
| `DEGREE_TABLE_AFTER` | **1024** | probes before a degree table may be built *(settable)* |
| `DEGREE_TABLE_MAX_ID` | **268,435,456** | the id ceiling for a degree table |
| `MEMBERS_FOLD_AT` | **4096** | membership overlay rows before folding into a new base |
| `MEMBERS_BITMAP_AFTER` | **4096** | probes before a presence bitmap is built *(settable)* |
| `MEMBERS_BITS_MIN` | **4096** | ids below which a bitmap is not worth it |
| `LABEL_LOG_CAP` / `PROP_LOG_CAP` | **65,536** / **16,384** | change-log caps |
| `MEMBERS_CACHE_MAX` / `RANGE_CACHE_MAX` | **1024** / **256** | cached views |

The `ADJ_TABLE_CACHE_MAX` note is worth reading twice: a cache sized to exactly
one workload's need evicted the very tables it had just built. Sizing a cache
to the measurement is how you get a cache that only helps the measurement.

## Query planning and execution

| constant | value | |
|---|---|---|
| `PROP_COLUMN_BUDGET_BYTES` | **512 MiB** | property-column cache budget |
| `PROPERTY_SEEK_MIN_LABEL` | **512** | a label smaller than this is scanned, not sought |
| `PROPERTY_SEEK_SELECTIVITY` | **16×** | a predicate less selective than this is scanned |
| `PROPERTY_SEEK_MAX_PROBE` | **2048** | probes a seek may make |
| `ESTIMATE_SAMPLE_BUDGET` | **4096** | rows the sampled cardinality estimator reads |
| `COLUMNAR_AGG_BATCH` | **131,072** | rows per columnar aggregate batch |
| `PRED_CHUNK` | **4096** | rows per predicate-evaluation chunk |
| `ENTITY_LATCH_STRIPES` | **1024** | per-entity write-latch stripes |
| `INTERSECT_PROBE_CAP` | **16,384** | probes in a set intersection |
| `MULTISTAGE_TOPK_BATCH` | **1024** | driving rows per top-k stage |
| `ORDER_SEARCH_MAX_PATHS` | **6** | join orderings the peak search evaluates |
| `HOP_COUNT_MEMO_MAX` | **512** | memoised labelled hop counts |
| `CONSTANT_END_CAP` | **65,536** | resolved constant end-set size |
| parallel minimum rows | **256** | driving rows below which a morsel split does not pay *(settable in-process)* |

The two property-seek gates are the reason an index sometimes appears unused:
seeking an index that is *not* selective is slower than scanning the label, so
the planner declines. `--no-property-seek` forces the scan and gives you the
A/B.

## Vector search

| constant | value | |
|---|---|---|
| `M` | **24** | HNSW connections per node above level 0 |
| `M0` | **48** | connections at level 0 |
| `EF_CONSTRUCTION` | **200** | build-time beam width |
| `EF_SEARCH_MIN` | **400** | search beam floor; effective beam is `max(400, 4k)` |
| `LEVEL_NORM` | **0.3606737602222409** | `1 / ln(M)` — the level-assignment constant |
| `OVERSAMPLE` | **2** | the int8 scan keeps `k × 2` candidates for f32 rescore |
| exact-scan crossover | **2048** | below this, an exact scan; above, HNSW *(settable in-process)* |
| `VECTOR_DELTA_CAP` | **4096** | pending ids before a rebuild |

Level assignment is seeded from the external id, so **the same data builds the
same graph** — an HNSW that is deterministic, which is unusual and is what lets
the simulation reproduce a vector search.

## Protocol and parser

| constant | value | |
|---|---|---|
| `MAX_MESSAGE_BYTES` | **64 MiB** | largest single Bolt message |
| `MAX_DEPTH` | **64** | PackStream nesting |
| `MAX_EXPR_DEPTH` | **64** | Cypher expression nesting |
| `MIN_PARSER_STACK_BYTES` | **4 MiB** | the stack needed for that depth to be reachable |
| `RSS_GROWTH_REPORT_BYTES` | **32 MiB** | per-statement RSS growth that names itself |
| autocommit `RETRIES` | **4096** | |
| `ESCALATED_LOSS_BOUND` | **32** | |

## Blobs, log and crypto

Not on the serving path — see [Seams](../architecture/seams.md).

| constant | value | |
|---|---|---|
| `T0_MAX` / `T1_MAX` | **4 KiB** / **1 MiB** | inline, then engine-KV, then external |
| `WAL_MAGIC` | `"ENGRWAL1"` | |
| `WAL_HEADER_LEN` | **64** bytes | magic, version, genesis hash |
| `HEADER_LEN` (log entry) | **22** bytes | the plaintext routing header |
| AEAD | AES-256-GCM | via `ring`; 12-byte nonce |

## Why these are not settable

Two reasons, and they differ.

Some are **format**: `FORMAT_VERSION`, `TARGET_BLOCK_BYTES`, `WAL_MAGIC`, the
key layout. Changing one changes what is on disk, so it is a migration and not
a flag.

The rest are **tuned thresholds** whose defaults were measured. A flag for each
would be forty more flags on a surface that already has forty, most of which
are not settings. Where one has proven worth changing at runtime it has been
promoted — `--adj-overlay-fold`, `--degree-table-after`,
`--members-bitmap-after` all began here.

## Next

- [Server CLI](./cli.md) — what *is* settable.
- [Tuning guide](./tuning.md) — by symptom.
- [Derived structures](../architecture/derived-structures.md) — what most of
  these govern.
