# Building from source

## Prerequisites

- **Rust 1.95.0.** Pinned in `rust-toolchain.toml`, so `rustup` selects it
  automatically on the first build.
- **A C compiler.** Two dependencies need exactly one `cc` invocation each —
  `ring` and `blake3`. No cmake, no bindgen, no system libraries.
- About **2 GB** of disk for the build.

## Build and run

```sh
cargo build --release -p engram-server
./target/release/engram-server 127.0.0.1:7687 --data-dir ./data
```

The whole workspace:

```sh
cargo build --workspace
cargo test --workspace
```

Be aware that `cargo test --workspace` links **276 integration test binaries**
and runs about 1,700 test functions. At the dev profile's default optimisation
level that is slow, because this engine's tests move real data — one repairs a
table of a fifth of a million entries. CI raises the optimisation level while
keeping debug assertions on:

```sh
CARGO_PROFILE_TEST_OPT_LEVEL=2 CARGO_PROFILE_TEST_DEBUG_ASSERTIONS=true \
  cargo test --workspace
```

That is not a correctness compromise: `debug-assertions` stays on, so every
`always!`, `debug_assert!` and overflow check still fires.

## The toolchain pin is not the MSRV

Two different numbers, deliberately:

| | value | meaning |
|---|---|---|
| `rust-toolchain.toml` `channel` | **1.95.0** | what the code is developed and tested against |
| `Cargo.toml` `rust-version` | **1.85** | the oldest toolchain it must build on |

Conflating them is how a declared minimum stops being true: everyone builds on
something newer, a newer language feature slips in, and the break surfaces only
for whoever actually pinned the minimum.

`rust-version` is also **advisory in most build paths** — cargo only errors on
it when resolving a dependency that declares a higher one, so a crate in this
workspace using a newer feature compiles happily on a newer toolchain.

So the number is enforced twice:

- **`cargo xtask msrv`** checks every member *declares* the same value;
- **CI builds on a toolchain pinned to it**, which is the only thing that makes
  the declaration true.

That second half found a real defect on its first run: a dead binding that 1.85
warns about and 1.95 does not, which meant the declared minimum did not
actually build.

## The workspace

Seventeen members sharing **one version** (`version.workspace = true`). It was a
literal in all seventeen manifests, which makes a release bump seventeen edits
and a missed one a crate published at the wrong version — permanent on
crates.io.

Every internal dependency carries **both a path and a version**, because
`cargo publish` refuses a dependency with no version requirement — and it does
so at publish time, so a tree that builds, tests and gates perfectly is
nonetheless unpublishable, with nothing saying so until the first release
attempt.

## Lints

```toml
[workspace.lints.rust]
unsafe_code = "deny"
missing_docs = "deny"

[workspace.lints.rustdoc]
broken_intra_doc_links = "deny"
private_intra_doc_links = "deny"

[workspace.lints.clippy]
disallowed_methods = "deny"
disallowed_types = "deny"
```

`missing_docs` is `deny` at zero cost — public-item coverage was already
complete when it was raised, so it only keeps it that way.

The clippy denials are the determinism first line; see
[The gates](./gates.md).

## Dependencies

**39 third-party crates**, all permissively licensed, no copyleft. Direct
dependencies are only:

| crate | used by | for |
|---|---|---|
| `blake3` | store, log, graph, blob, objstore | hash chain, block integrity, content keys |
| `arc-swap` | store, graph | lock-free publish of sealed sets and derived slots |
| `ring` | crypto | AES-256-GCM and HKDF |
| `tz-rs`, `tzdb` | cypher | IANA zone resolution for temporal values |
| `tokio` | runtime (optional feature) | the non-simulated `Runtime` |
| `mimalloc` | server, bench, **musl only** | musl's single-arena allocator serialises concurrent allocation |

### The one-`cc`-invocation rule

> A native dependency may need **one** `cc` invocation. It may not need cmake,
> bindgen, an external shared library, or a process at build time.

`ring` and `blake3` satisfy it; `aws-lc-rs` (cmake plus AWS-LC) does not, and is
banned by name in `deny.toml` — which records what that costs, since AWS-LC is
FIPS-validated and `ring` is not.

The rule lives in the workspace manifest because it is a property of the whole
build: cargo unifies features per crate-version across the entire graph, so one
member enabling a cmake-driven backend changes what every other member links.

```sh
cargo xtask c-deps
cargo deny check
```

## Platform notes

**musl** builds link `mimalloc`. The multi-worker server would otherwise
serialise concurrent allocation-heavy queries on the single-arena system
allocator's lock; one measured workload went 73 → 324 queries/s. Native builds
keep the system allocator the determinism baselines run on.

**Linux** is the only platform where RSS reporting and stale-lock takeover work:
RSS comes from `/proc/self/statm`, and liveness from `/proc/<pid>`. Elsewhere a
stale lock is refused with instructions — the conservative direction.

## Building the documentation

```sh
cargo install mdbook mdbook-mermaid --locked
mdbook build docs/book
cargo xtask docs        # orphans, dead links, CLI-reference parity

cargo doc --workspace --no-deps
```

## Common problems

| symptom | cause |
|---|---|
| a `cc` not found error | no C compiler for `ring`/`blake3` |
| the MSRV job fails, local builds pass | 1.85 warns where 1.95 does not; `-D warnings` turns that into an error |
| the linker dies with a bus error | linking 276 test binaries with full debug info; use `CARGO_PROFILE_TEST_DEBUG=line-tables-only` |
| a golden byte test fails on a fresh clone | CRLF checkout — `.gitattributes` normalises, so check your git config |

## Next

- [The gates](./gates.md) — what must pass.
- [Testing](./testing.md) — how the suite is organised.
- [Contributing](./contributing.md) — the conventions.
