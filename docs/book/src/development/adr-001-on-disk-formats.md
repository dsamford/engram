# ADR-001 — The on-disk formats: key encoding and value tags

> **A frozen decision record.** Accepted 2026-08-19 and amended once, on
> 2026-08-29, for the commit-log chain rule. This page is the record as it
> stands; [Key encoding and on-disk formats](../architecture/key-encoding.md)
> is the working explanation of the same material.

**Status:** accepted, frozen.
**Date:** 2026-08-19.
**Fulfils:** FC-14, covering FC-1 through FC-6, FC-10, FC-11 and the keyspace
hygiene rule.

## Context

The key encoding is the on-disk format. It is ranked first in the plan's risk
register by cost of being wrong — *"unfixable once data exists"* — and five
separate requirements are consequences of its shape rather than features built
on top: the namespace component is the overlay model, partition-before-id is
the contiguous range scan, the tenant prefix is both the isolation boundary
and the DEK derivation, and a KIND is how protected properties become
physically unstorable in plaintext.

Spatial support ships no implementation in early milestones. What it gets is
reservations, because the decisions that would preclude it are the ones frozen
first.

## Decision 1 — the key tuple

```
realm:u32 │ namespace:u32 │ KIND:u8 │ partition:u32 │ <body> │ !commit_ts:u64
```

- **Memcomparable**: byte order equals logical order for every pair. All
  fixed-width integers are big-endian. This is asserted by ordering tests, not
  round-trip tests — round-tripping is satisfied by an encoding with no
  ordering property at all.
- **`commit_ts` is inverted** so a newer version sorts first and a point read
  is the head of a prefix scan. The backward-clock-jump test in
  `engram-runtime` exists to prove this survives a clock that moves backwards.
- **Realm first**: the tenant prefix is the isolation boundary. This decides
  the R13-noted conflict between tenant locality and spatial locality —
  tenant wins, and a space-filling curve gets locality *within* a tenant's
  partition, never across tenants. Cross-tenant spatial locality was never
  acceptable; the cost is accepted here deliberately rather than discovered.

## Decision 2 — KIND registry (FC-1, FC-2)

One byte, in frozen blocks: `0x00` invalid · `0x01–0x3F` core · `0x40–0x7F`
reserved · **`0x80–0xBF` protected** · `0xC0–0xFE` extension · `0xFF` escape
(real kind is a u16 at the head of the body).

- **`is_protected` is a range check, never a list.** A list can fall behind
  the registry; a range cannot. It answers for KINDs this build has never
  seen: an unknown byte in the protected block is protected, because the
  other default makes forward compatibility and a plaintext leak the same
  event.
- **Body decoding is dispatched** (`KindRegistry`), never matched inside the
  codec. `split_key` returns the body as bytes. A new KIND therefore ships
  without touching the frozen artifact. `Unregistered`, `Corrupt` and
  `Trailing` are three distinct outcomes: version skew, damage, and a decoder
  that is wrong about its own format are different facts with different
  responses.

## Decision 3 — the keyspace hygiene rule

> A user property value must never appear in a sort-ordered key position.

An LSM sorts by key; a plaintext value in a key **is order-preserving
encryption**, sorting attack included. Enforcement is a **sealed trait**
(`Structural`) — the mistake is unrepresentable, not reviewed for. Because a
compile-time guarantee is still a guard nobody has watched fire,
`cargo xtask hygiene` compiles a violating program and requires the build to
fail *for the seal's reason*, and a legitimate program and requires it to
pass. One direction alone would certify the rule on the strength of an
unrelated build error.

`fixed_bytes<N>` cell ids (FC-3) are written **unescaped** — a Z-order or
Hilbert id carries its locality in its exact bit layout, and escaping would
destroy it. Variable-length escaping (`0x00 → 0x00 0xFF`, terminator
`0x00 0x01`) is shipped and tested although nothing in v1 uses it, because
inventing escaping after data exists is the unfixable class.

## Decision 4 — value tags (FC-4, FC-5, FC-6)

A parallel one-byte registry for property values, in frozen blocks: core
scalars, reserved, **spatial (`0x70–0x9F`)**, **vector (`0xA0–0xCF`)**,
extension, escape.

- **The skip rule is the feature.** Every tag's payload length is computable
  from the tag byte alone (fixed shapes) or from a u32 length prefix that is
  always in the same position. An unknown tag in any valid block is
  length-prefixed *by block rule*, so a reader can step over a value whose
  tag was assigned years after it shipped. The price, accepted in writing:
  **new fixed-width tags are only assignable before any reader ships; after
  that, every new type is length-prefixed.**
- **POINT ≠ VECTOR (FC-6), permanently.** A `Point2D` and a 2-dim vector have
  identical bytes; only the tag separates a coordinate that feeds a curve
  from an embedding that feeds a similarity index. They live in different
  blocks so even an unassigned spatial tag can never collide with an
  unassigned vector tag.
- **POINT payloads are frozen (FC-5):** 2D = `srid:u32 │ x:f64 │ y:f64`
  (20 bytes); 3D adds `z:f64` (28 bytes). The SRID is part of the value —
  4326 and 7203 make the same (x, y) mean different places.
- **Values are little-endian, deliberately breaking symmetry with keys.**
  Values live in ciphertext record payloads and are never sorted; an encoder
  that "helpfully" reuses key components for values fails golden tests
  immediately instead of working by accident.
- **Explicit `NULL` is a tag**, distinct from an absent property. The
  incumbent's `properties(n)` omits null columns and that collapse has cost
  this project repeatedly; the distinction starts on disk.
- **The constraint validator's histogram (FC-10) keys on these same tag
  bytes**, with unknown tags counted individually rather than folded into an
  "other" bucket that hides a rollout.

## Decision 5 — the commit-log chain rule (added 2026-08-29)

Every commit-log entry carries
`hash = BLAKE3(prev_hash ‖ seq ‖ header ‖ payload_digest)` with
`payload_digest = BLAKE3(payload_len ‖ payload)`, and the genesis head is
`BLAKE3("engram-log-v1-genesis")`.

The rule was `BLAKE3(prev_hash ‖ seq ‖ header ‖ payload_len ‖ payload)` —
one hash over the whole entry. It was changed to the two-hash form
**before any released format exists**, for one reason: the chain makes the
log latch the serialisation point of every write, and hashing the payload
(the bulk of an entry) inside that latch was the largest serialised cost
left after the tail was sharded. With the digest computed by the writer
outside the latch, the serialised section is a 40-byte hash. The change is
pinned by `engram-log`'s `GOLDEN_the_hash_rule_is_frozen`,
whose literal was updated once and carries the previous value; logs written
under the old rule do not verify under the new one and are regenerated.
After 0.1 this rule is frozen with the rest of this document.

## Consequences

- Golden byte-level tests pin both formats; any layout change is a visible
  diff plus a failing test, never an accident.
- FC-11 (a range-index entry's payload never migrates into the key) is
  documented on `INDEX_ENTRY` and inherits Decision 3's enforcement; the
  mechanical check lands with the index implementation.
- 45 tests across the crate; every non-structural property is canaried. One
  canary was retired by making its property structural: the list-skip
  arithmetic moved to u64, where a u32 count cannot wrap on any target —
  checked arithmetic there was untestable on a 64-bit host, and an untestable
  guard is not a guard.


## Where this is used

- [Key encoding and on-disk formats](../architecture/key-encoding.md) — the
  working explanation.
- [The storage engine](../architecture/storage-engine.md) — the protected-KIND
  gate in the write path.
- [The commit log](../architecture/commit-log.md) — Decision 5 in use.
- [The gates](./gates.md) — `cargo xtask hygiene`, which enforces Decision 3.
