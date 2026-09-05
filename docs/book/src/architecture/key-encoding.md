# Key encoding and on-disk formats

The key encoding **is** the on-disk format, and it is frozen. This project ranks
it first by cost of being wrong — *"unfixable once data exists"* — and five
separate requirements are consequences of its shape rather than features built
on top of it.

The decision record is [ADR-001](../development/adr-001-on-disk-formats.md);
this page is the working explanation.

## The key tuple

```text
realm:u32 │ namespace:u32 │ KIND:u8 │ partition:u32 │ <body> │ !commit_ts:u64
```

Thirteen bytes of prefix, a variable body, and eight bytes of inverted
timestamp.

### Memcomparable

**Byte order equals logical order for every pair.** All fixed-width integers
are big-endian.

This is asserted by **ordering** tests, not round-trip tests — round-tripping is
satisfied by an encoding with no ordering property at all, so a round-trip test
would pass a design that fails at the only thing this encoding is for.

The consequence you feel: "every out-edge of node *n* of type *t*" is a
**contiguous range scan**. The sort order *is* the index, which is why Engram
needs no separate adjacency structure on disk.

### `commit_ts` is inverted

So a newer version sorts **first**, and a point read is the head of a prefix
scan rather than a search.

There is a test in `engram-runtime` for a clock that moves backwards, and it
exists to prove this survives one.

### Realm first

The tenant prefix is the isolation boundary, so every scan is tenant-local by
construction — and a per-tenant encryption key can be derived from the prefix
with **no key table to keep consistent**. The keyspace layout *is* the key
derivation.

This settles a real conflict: tenant locality versus spatial locality. **Tenant
wins.** A space-filling curve gets locality *within* a tenant's partition, never
across tenants. Cross-tenant spatial locality was never acceptable, and the cost
is accepted deliberately rather than discovered later.

## The KIND registry

One byte, in frozen blocks:

| range | block |
|---|---|
| `0x00` | invalid |
| `0x01–0x3F` | core |
| `0x40–0x7F` | reserved |
| **`0x80–0xBF`** | **protected** |
| `0xC0–0xFE` | extension |
| `0xFF` | escape — the real kind is a `u16` at the head of the body |

Two decisions live here.

**`is_protected` is a range check, never a list.** A list can fall behind the
registry; a range cannot. It answers for KINDs this build has never seen: an
unknown byte in the protected block **is protected**, because the other default
makes forward compatibility and a plaintext leak the same event.

**Body decoding is dispatched, never matched inside the codec.** `split_key`
returns the body as bytes and a `KindRegistry` dispatches it, so a new KIND
ships without touching the frozen artifact.

Three distinct outcomes are kept distinct — `Unregistered`, `Corrupt` and
`Trailing` — because version skew, damage, and a decoder that is wrong about its
own format are different facts with different responses.

## The keyspace hygiene rule

> **A user property value must never appear in a sort-ordered key position.**

An LSM sorts by key, so a plaintext value in a key **is order-preserving
encryption**, sorting attack included — whether or not anyone calls it that.

Enforcement is a **sealed trait** (`Structural`): the mistake is
*unrepresentable*, not reviewed for. A property value cannot implement a sealed
trait from outside the crate.

And because a compile-time guarantee is still a guard nobody has watched fire,
`cargo xtask hygiene` compiles a **violating** program and requires the build to
fail *for the seal's reason*, then compiles a **legitimate** one and requires it
to pass:

```text
[PASS] hygiene  violation build=refused (must fail), legitimate build=OK (must pass)
```

One direction alone would certify the rule on the strength of an unrelated build
error.

### Escaping

Variable-length components escape `0x00 → 0x00 0xFF` with terminator
`0x00 0x01`. It is shipped and tested although nothing in v1 uses it, because
**inventing escaping after data exists is the unfixable class.**

Fixed-width cell ids are written **unescaped** — a Z-order or Hilbert id carries
its locality in its exact bit layout, and escaping would destroy it.

## Value tags

A parallel one-byte registry for property values, also in frozen blocks: core
scalars, reserved, **spatial (`0x70–0x9F`)**, **vector (`0xA0–0xCF`)**,
extension, escape.

### The skip rule is the feature

Every tag's payload length is computable from the tag byte alone, or from a
`u32` length prefix always in the same position. An unknown tag in any valid
block is length-prefixed *by block rule*, so a reader can **step over a value
whose tag was assigned years after it shipped**.

The price is written down: **new fixed-width tags are only assignable before any
reader ships.** After that, every new type is length-prefixed.

### POINT is not VECTOR, permanently

A `Point2D` and a 2-dimensional vector have **identical bytes**. Only the tag
separates a coordinate that feeds a space-filling curve from an embedding that
feeds a similarity index.

They live in different blocks so that even an *unassigned* spatial tag can never
collide with an unassigned vector tag.

POINT payloads are frozen: 2D is `srid:u32 │ x:f64 │ y:f64` (20 bytes), 3D adds
`z:f64` (28 bytes). The SRID is part of the value — 4326 and 7203 make the same
`(x, y)` mean different places.

### Values are little-endian, deliberately

Breaking symmetry with keys on purpose. Values live in record payloads and are
**never sorted**, so an encoder that "helpfully" reuses key components for values
fails golden tests immediately instead of working by accident.

### Explicit NULL is a tag

Distinct from an absent property, at the format level. Nothing in Cypher
surfaces the distinction today — setting a property to null removes it, per the
standard — but the format keeps it, because collapsing the two loses information
that cannot be recovered.

## The commit-log chain rule

Every entry carries:

```text
hash = BLAKE3(prev_hash ‖ seq ‖ header ‖ payload_digest)
payload_digest = BLAKE3(payload_len ‖ payload)
```

with genesis `BLAKE3("engram-log-v1-genesis")`.

It was originally one hash over the whole entry, and was changed to this
two-hash form **before any released format existed**, for one reason: the chain
makes the log latch the serialisation point of every write, and hashing the
payload — the bulk of an entry — inside that latch was the largest serialised
cost left. With the digest computed by the writer *outside* the latch, the
serialised section is a 40-byte hash.

See [The commit log](./commit-log.md).

## How it is kept frozen

- **Golden byte-level tests** pin both registries. A layout change is a visible
  diff plus a failing test, never an accident.
- The commit-log hash rule is pinned by a golden test whose literal was updated
  exactly once and which carries the previous value.
- 45 tests across `engram-key`, with every non-structural property canaried.

One canary was **retired by making its property structural**: the list-skip
arithmetic moved to `u64`, where a `u32` count cannot wrap on any target.
Checked arithmetic there was untestable on a 64-bit host — and an untestable
guard is not a guard.

## What this buys, in one list

| property | because |
|---|---|
| tenant-local scans, no filter | realm is the first component |
| per-tenant key derivation, no key table | the prefix *is* the derivation |
| adjacency without a separate structure | memcomparable order is the index |
| point reads as prefix-scan heads | inverted `commit_ts` |
| forward compatibility | the skip rule |
| plaintext leaks unrepresentable | protected KINDs as a range, plus a sealed trait |

## Next

- [ADR-001](../development/adr-001-on-disk-formats.md) — the decision record.
- [The storage engine](./storage-engine.md) — what reads these keys.
- [The commit log](./commit-log.md) — the chain rule in use.
