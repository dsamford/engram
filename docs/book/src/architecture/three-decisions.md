# The three decisions

Three decisions shape this codebase, and each is enforced by a gate rather than
by convention — because, in the project's own phrasing, *a rule nothing checks
is a preference*.

[How Engram is different](../intro/how-its-different.md) is the short version.
This page is the mechanics: what each decision actually requires, what enforces
it, and what it would cost to have decided otherwise.

| | decision | enforced by |
|---|---|---|
| **D1** | Nothing ambient is reached for: the adapter injects time on the serving path, a `Runtime` trait supplies it in the simulation lane | `clippy.toml` denies `Instant::now`, `thread::spawn`, `HashMap`, …; `cargo xtask determinism` |
| **D2** | The engine crates never spawn a thread; parallelism is installed through a seam (*revised 2026-08-25 — see below*) | `clippy.toml` denies `thread::spawn` everywhere but the adapter; result-determinism and a serializability checker replaced single-threading |
| **D3** | Every subsystem declares its crash points, `sometimes!` events and counters | `cargo xtask d3`, plus a simulation sweep that fails when a declared state is never reached |

## D1 — everything ambient is injected

### What it requires

The engine reads no clock, draws no randomness, spawns nothing and performs no
I/O directly.

**Two different mechanisms deliver that, and it is worth knowing which is
which**, because the trait below is not the one the serving path uses.

| lane | how the ambient arrives |
|---|---|
| **simulation** | the `Runtime` trait — a real executor with a virtual clock |
| **serving** | the lints forbid ambient access, and the adapter *injects* what the engine needs |

`engram-runtime` is a **dev-dependency** of every engine crate; only
`engram-sim` depends on it for real. On the serving path the engine gets wall
time because `engram-server` calls `graph.set_wall_ms(now)` into a cell the
engine reads, and gets *logical* time from the store's commit clock
(`now_ts`) — which is a monotonic counter, not a clock at all.

So D1 is enforced on the serving path by prohibition plus injection, and
demonstrated in the simulation lane by the trait:

```rust
pub trait Runtime: Clone {
    fn now(&self) -> SimInstant;
    fn rand_u64(&self) -> u64;
    fn sleep(&self, d: Duration) -> impl Future<Output = ()>;
    fn spawn(&self, fut: impl Future<Output = ()> + 'static);
    fn yield_now(&self) -> impl Future<Output = ()>;
}
```

Two signatures encode decisions of their own:

- **`rand_u64(&self)`, not `&mut self`.** Randomness is drawn from the runtime
  the whole shard shares, so the *draw order* is part of the run and is
  reproduced with it. A per-caller generator would make two runs diverge
  whenever scheduling shifted which task drew first — precisely the divergence
  the determinism gate exists to catch.
- **`spawn` has no `Send` bound.** A cooperative task on a shard never leaves
  its thread, so requiring `Send` would be a constraint bought for nothing.
  (The *store* is `Send + Sync` — that is a different axis, and D2 below has
  the history.)

### What enforces it

First, the compiler, via `clippy.toml`:

```toml
disallowed-types = [
  { path = "std::collections::HashMap", reason = "iteration order is not deterministic;
    use BTreeMap, or IndexMap when insertion order is what you mean" },
  { path = "std::collections::HashSet", reason = "iteration order is not deterministic;
    use BTreeSet" },
]
disallowed-methods = [
  { path = "std::time::Instant::now",   reason = "take time from Runtime::now()" },
  { path = "std::time::SystemTime::now", reason = "take time from Runtime::now()" },
  { path = "std::thread::spawn",  reason = "spawn cooperative tasks through Runtime::spawn" },
  { path = "std::thread::sleep",  reason = "blocks the shard's thread and stops virtual time" },
]
```

But that file's own header calls itself *a convenience, not the enforcement* —
clippy config is bypassable with an `#[allow]`, a changed lint level, or a build
that never runs clippy. So the real gate is:

```sh
cargo xtask determinism
# [PASS] determinism  seed 424242, two processes, digest 5483cf2ea8e8fc46
```

**Two processes, one seed, one identical digest over the whole event trace.**
Two processes rather than two runs in one, because a process reuses its
allocator and its address-space layout, and a determinism bug that hides behind
either is exactly the kind that ships.

That catches what no lint can see: hash iteration order, float reduction order,
a dependency quietly spawning its own thread pool.

### Why, and what the alternative costs

So that a seed reproduces a run exactly, and a failure is a repro rather than a
story about a bad afternoon.

The source has a word for the alternative: **theatre**. A simulation lane built
over direct clock reads and a real thread pool still runs, still goes green,
and still produces a coverage report. It just is not reproducing anything — and
a green run from a harness that explored one interleaving looks exactly like a
green run from one that explored thousands.

The cost of deferring D1 is what makes it a *decision* rather than a practice:
adding it after twenty thousand lines of `tokio::spawn` and `Instant::now()` is
not a refactor, it is a rewrite.

### Where the clock actually enters

Exactly one place. `engram-server` calls `graph.set_wall_ms(now)` immediately
before feeding a batch's bytes to the protocol machine. Everything below reads
time from what it was handed, and everything that needs *ordering* rather than
wall time uses the commit clock instead, which is a counter the store owns.

`engram-server` is also the only crate in the workspace carrying
`#![allow(clippy::disallowed_methods, clippy::disallowed_types)]`. That is not
an exemption so much as a boundary: the adapter is where the real world is
allowed to exist, and it is one file deep.

## D2 — the engine never spawns

### What it requires today

**This is the decision that has changed the most, so read the revision before
the original.**

As written, D2 was "one shard, one thread, cooperative tasks; the store is
`!Sync` on purpose." It was **revised on 2026-08-25** for the concurrent-write
program. The store is now `Send + Sync`, because morsel-driven parallel
execution and MVCC-OCC require it to cross threads.

So the server is **not** single-threaded, and describing it that way is wrong:

- `--workers N` runs N engine threads over one shared store, with connections
  pinned to a worker by `id % workers`.
- Each connection additionally has a reader and a writer thread.
- A maintenance thread and a counters thread run alongside.
- Inside a statement, **morsel-parallel** `expand` and count-fold operators can
  split work across a thread pool.

What survives, and is the actual rule, is narrower and sharper: **the engine
crates never spawn a thread.** `std::thread::spawn` is denied workspace-wide,
and `engram-server` is the only crate that opts out. Every thread in the process
is created in one file.

### What replaced single-threading as the integrity mechanism

The original argument for one thread was simulability. Losing it meant the
integrity had to come from somewhere stronger, and the store's own
documentation names the replacements: **result-determinism**, interleaving
search (Loom/shuttle), and a serializability checker.

`SimRuntime` is still a **real executor** — a ready queue, a timer heap and a
virtual clock — rather than a thin wrapper deferring to a runtime it does not
control. If the simulation did not own scheduling, it could not reproduce it.

### The seam that keeps determinism possible

Parallelism inside a statement enters through one trait, `ScopedExec`, and this
is the part worth understanding:

| implementor | what runs |
|---|---|
| **absent** (default, and always in the simulation lane) | operators take their serial paths; one lever check on the hot path |
| `SerialExec` | width 1, inline — the *parallel machinery* run **deterministically** |
| the server's thread-scope pool | real OS threads, behind `ENGRAM_QUERY_PARALLELISM` |

Because the engine asks for parallelism rather than owning it, the morsel
machinery — split, slot collection, ordered merge — can be exercised
single-threaded through the *same code path*. Partials are concatenated in
morsel order, so a parallel run is byte-identical to a serial one, proven per
operator by an A/B differential with a fired-counter canary.

That is what a database owning its own thread pool cannot do.

### Interior state, and the honest stepping stone

Store state is an `Arc<RwLock<State>>` taken through one place. The source calls
this what it is: a **coarse latch and a deliberate stepping stone** — it makes
the type thread-safe with no behaviour change, and the refinement into
fine-grained latches (the immutable sealed segments never need locking; only the
mutable tail, the timestamp counter, the pins and the locks do) is later work.

A decision page that only recorded decisions which held would be a marketing
document. This one changed, and the shape it changed into is less elegant than
the original and more useful.

## D3 — every subsystem declares what it can do

### What it requires

Before a subsystem may fire an event, it registers it. Four kinds:

| kind | what it buys |
|---|---|
| crash points | a place the harness can kill the process, named so a seed can name it |
| `sometimes!` events | the coverage floor, below |
| counters | a measurable quantity the sweep can assert on |
| gates + canaries | a claim, paired with proof the check for it still bites |

```sh
cargo xtask d3
# [PASS] d3  367 file(s), 12 Subsystem impl(s) checked, 2 test fixture(s) excluded,
#            canary detected 3 of 3
```

Twelve subsystems register: `blob`, `bolt`, `crypto`, `cypher`,
`exec-operators`, `graph`, `key-codec`, `commit-log`, `objstore`, `executor`,
`replica`, `store`.

### Why registration, rather than just calling the macros

This is the subtle one, and the crate's own documentation makes the argument
better than a summary can:

> If events only ever come into being by firing, then "never fired" and "does
> not exist" are the same observation — an empty coverage report reads as a
> clean run, and a simulation that never injected a fault passes everything.
> Declaring the event at construction is what makes its absence a *measurable*
> fact rather than an absence of facts.

With declaration, the simulation sweep can enforce a **coverage floor**: any
declared `sometimes!` event that never fires across a sweep *fails the sweep*.
A fault that is never injected is a build failure, not neutral.

### It has already paid

The first sweep failed with seven never-fired events, and every one was a real
gap in the harness — tamper checks running outside the trace, a workload that
never constructed a contended CAS, a codec path nothing exercised, and two
executor states no scenario reached.

The crate's doc calls this *the house defect, in the layer built to detect the
house defect*, which is the honest description.

### The gate/canary rule, in the type system

`Gate::new(name, first_canary)` is the only constructor. A gate without at
least one canary is **unrepresentable** — not discouraged, not reviewed for.
The same move appears in `engram-key`, where the sealed `Structural` trait makes
it impossible for a user property value to become a key component, and
`cargo xtask hygiene` then compiles a deliberately violating program and
requires the build to fail *for the seal's reason*:

```sh
cargo xtask hygiene
# [PASS] hygiene  violation build=refused (must fail), legitimate build=OK (must pass)
```

Both directions, because one alone would certify the rule on the strength of an
unrelated build error.

## The pattern underneath all three

Each decision follows the same three-step shape:

1. **Make the wrong thing hard** — a trait, a sealed type, a denied lint.
2. **Make the check unable to lie** — report what was *scanned*, and fail when
   your own reach is implausible.
3. **Prove the check still bites** — a canary that must fire, and a negative
   canary proving it has not started matching everything.

You will meet step 2 constantly. `[PASS] c-deps 39 package(s), 39 inspected`
carries that second number because a gate which walked zero files prints "no
findings" in exactly the same words as one that walked all of them — and this
repository has shipped an audit that skipped the very thing it appeared to
clear.

## Next

- [Crate map and dependency rules](./crate-map.md) — the boundaries these
  decisions create.
- [Deterministic simulation](../development/simulation.md) — the sweep, the
  swarm config and the coverage floor in practice.
- [The gates](../development/gates.md) — what each one checks and how to run it.
