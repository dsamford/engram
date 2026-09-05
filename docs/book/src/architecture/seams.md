# Seams not yet on the serving path

Four crates are compiled, tested and simulated, and **nothing on the Bolt
serving path calls them**:

| crate | what it is |
|---|---|
| `engram-crypto` | per-tenant keys, AEAD sealing, a redaction type |
| `engram-objstore` | the object-storage seam |
| `engram-blob` | large-media referencing |
| `engram-exec` | the bitmap operator seam |

Verified rather than assumed: `engram_crypto::`, `engram_objstore::`,
`engram_blob::` and `engram_exec::` appear **nowhere** in `engram-graph`,
`engram-server` or `engram-bolt`. Only `engram-sim` and their own tests
reference them.

**Do not read their presence as a shipped feature.** This page exists so that
nobody does.

## Why build a seam before the feature

Because some decisions cannot be retrofitted, and each of these encodes one.

> A hash chain added later cannot attest to any history predating it.

The same shape applies four times:

- A **key derived from the keyspace prefix** cannot be added after data exists
  under a different derivation.
- **Crypto-shredding** requires that the bytes were never readable without the
  key — a property you cannot acquire later.
- A **content-addressed blob manifest** cannot be reconstructed for blobs
  already referenced by other means.
- An **error taxonomy** at a network boundary must exist before the boundary is
  crossed, or the first adapter's error shapes become the interface.

So the seams ship, and the features that use them do not exist yet.

## `engram-crypto`

Per-tenant data keys, AEAD sealing (AES-256-GCM via `ring`), and a redaction
type.

**The key is derived from the key prefix.** Because the tenant's realm is the
first component of every key, `derive_dek(master, realm)` gives one key per
tenant **with no key table to keep consistent** — the keyspace layout *is* the
key derivation.

Destroying a realm's key shreds that tenant from every copy of every byte sealed
under it. That is the shred-latency story the [commit log](./commit-log.md)'s
header/payload split sets up.

**Nonces are deterministic** — a per-key monotonic counter. GCM requires nonce
*uniqueness* per key, not unpredictability, so this is safe here and lets the
simulation reproduce ciphertexts from a seed. Two sealers on one key would
repeat nonces, so `Sealer` is not `Clone` and owns its key.

**`Secret` is the redaction type**: no `Display`, and a `Debug` that does not
print the value.

## `engram-objstore`

A trait, an error taxonomy, and fault injection.

**The boundary exists because real backends live outside the simulation
envelope.** S3 clients and gRPC stacks construct their own runtimes, so the
engine's seam is a trait, the simulation runs against an in-memory tier plus a
fault tier, and network adapters would implement the same trait in adapter
crates. **No engine crate may depend on `reqwest` or `tonic`** — the `c-deps`
gate enforces the dependency-graph half of that.

**The error taxonomy is the seam's real content.** Neither of the obvious
crates has an `Unavailable` variant; both collapse 500, 503, timeout and DNS
into a generic error. The wrapper supplies the taxonomy by classifying
**status**, never by string-matching a `Display` impl, with a fail-closed
default:

> anything unrecognised becomes `Unavailable`, **never** `NotFound`

because the fatal direction is a network hiccup reading as "the object does not
exist". `Indeterminate` is first-class, for a backend that answers UNKNOWN.

`TierPolicy::new` **refuses at construction** to place vectors on a tier that
cannot serve them — a boot refusal rather than a runtime surprise.

## `engram-blob`

Large media, referenced rather than embedded. The graph stores a `BlobRef` of a
few hundred bytes; the bytes live in a content-addressed store; between them
sits a **manifest entry** — an MVCC row in the engine's own transactional
domain, owning the refcount, locator and verification state.

**The split that matters is in the schema, not in a comment:**

- `node → manifest entry` is **engine-enforced** — transactional, exact, zero
  object-store I/O.
- `manifest entry → bytes` is **measured, never enforced** — an object-store
  outage must not fail a graph write.

That second relationship lives in a `VerifyState` where **`NotAttempted` is a
distinct state that never reads as `Intact`**.

**Convergent encryption is rejected** — fatal, not a trade-off. A key derived
from the plaintext is a key nothing can destroy, so crypto-shredding and
message-locked keys are mutually exclusive. Instead: a per-object random key,
wrapped under the realm's sealer, with dedup at the manifest.

Placement is by size: inline below 4 KiB, engine-KV below 1 MiB, external above.

## `engram-exec`

The bitmap operator seam. `RowIdSet`, a row directory mapping ids to dense
offsets, and `semi_mask`.

The argument for shipping it as an operator rather than an inline `&`:

> Tenant scoping, vector-candidate scoping and selective-predicate scoping are
> the same bitmap-AND on dense offsets. Shipping it as an operator makes scope a
> **measurement**; deferring it makes scope an **estimate** — and every
> published result says graph cardinality estimators are badly inaccurate.

So `semi_mask` does two things, and the second is why it exists: the bitmap AND,
**and it returns the measurement** — how many candidates entered, how many
survived, how many the mask removed. Scope stops being a number the planner
guessed and becomes one the execution produced, on every query, for free.

The mask side arrives as a `CandidateSource`, the trait a spatial index, an ANN
index, a tenant filter and a predicate scan would all implement — so a spatial
predicate composes with everything else for free, with no new execution path.

## How to read this page

These are **not** vapour: they are built, tested, and exercised by the
deterministic simulation, which is why they carry crash points and `sometimes!`
events like everything else.

They are also **not features**. Engram today has no encryption at rest in the
serving path, no object-storage tiering, no blob handling and no bitmap
operators in query execution.

The honest summary is the one the roadmap uses: **the hard part is built and the
product around it is not.**

## Next

- [Roadmap](../roadmap.md) — what would turn these into features.
- [Key encoding](./key-encoding.md) — the derivation and the protected KINDs.
- [The commit log](./commit-log.md) — the header/payload split these depend on.
