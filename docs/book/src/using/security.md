# Security posture

**This release has no authentication and no TLS.**

`LOGON` accepts credentials and does not verify them, and the Bolt listener
speaks plaintext. Any client that can reach the port can run any statement.
There is no user model, no roles and no authorization check to bypass — there
is nothing there yet.

Authentication (OIDC), namespace-aware authorization and TLS are the **named
next milestone** — see [Roadmap](../roadmap.md). Until they land, the
deployment boundary is the only boundary, and this page is about making that
boundary real.

## How to deploy it safely today

Because the database has no boundary of its own, everything below is about the
one you put around it.

- **Bind to loopback** unless you have a specific reason not to.
  `engram-server 127.0.0.1:7687` is the default for exactly this reason;
  `0.0.0.0:7687` publishes an unauthenticated database.
- **Put it inside a network boundary you control** — a private subnet, a
  container network, a WireGuard interface. Treat reachability as equivalent to
  full access, because it is.
- **Do not rely on obscurity.** An unpublished address, a non-standard port and
  the absence of a DNS record are not controls.
- **Terminate TLS in front of it** if traffic must cross a network — a
  reverse proxy or an mTLS sidecar. Bolt is a TCP protocol and proxies fine.
- **Give it its own host or container.** There is no per-tenant isolation to
  rely on inside one server.
- **Assume every statement is trusted**, and make sure the only things that can
  send one are things you trust.

## What *is* hardened

"We hardened it" is not a claim anyone should accept unenumerated, so here is
the list. Each of these was a way an unauthenticated client could previously
harm the process.

| | |
|---|---|
| PackStream nesting | bounded (`MAX_DEPTH`, 64); an unbounded recursion used to abort the process |
| Cypher expression nesting | bounded (64), with a declared minimum parser stack (4 MiB) so the bound is reachable rather than academic |
| Message size | bounded — `MAX_MESSAGE_BYTES`, 64 MiB |
| Per-connection queued bytes | bounded, with real backpressure: the reader parks rather than growing an unbounded queue |
| Concurrent connections | bounded (`--max-connections`, default 512), because each costs two OS threads |
| Idle and write timeouts | set (`--read-timeout-secs`, default 300), so a silent peer cannot hold a thread pair — the slowloris shape |
| Query row budget | set by the server (`--row-budget`, default 20M), not left unbounded |
| A panic in one session | contained; it no longer kills the worker and silently strands every connection pinned to it |

Note the fourth and last rows in particular: both were cases where one
misbehaving connection could degrade or kill service for every other connection
on the same worker.

## What is still absent

Load-bearing for an exposed deployment, and not present:

- **Authentication.** Credentials are accepted, not verified.
- **TLS.** Traffic is plaintext, including those unverified credentials.
- **Authorization.** No users, no roles, no grants.
- **Rate limiting.** The row budget bounds one query's materialisation; it does
  not make the engine fair between clients.
- **An audit log.** Session lifecycle, authentication attempts, DDL and
  authorization denials are **not recorded anywhere**. If you need to know who
  did what, that record does not exist to be recovered.

## What the engine does protect

Two things are enforced at a level that does not depend on the query layer, and
they are worth knowing because they will still hold when authorization arrives.

**The keyspace is the gate, not the planner.** Property values that must never
appear in plaintext are addressed by a *protected* KIND, and the storage layer
refuses a plaintext write to a protected KIND unconditionally — including KINDs
this build has never heard of, because the check is a range check rather than a
list. There is no flag to disable it and no privileged caller.

**A user property value can never become a key component.** An LSM sorts by
key, so a plaintext value in a key *is* order-preserving encryption, sorting
attack included. This is enforced by a sealed trait, so the mistake is
unrepresentable rather than reviewed for — and `cargo xtask hygiene` compiles a
deliberately violating program and requires the build to fail *for the seal's
reason*, then compiles a legitimate one and requires it to pass. One direction
alone would certify the rule on the strength of an unrelated build error.

## Tenancy, honestly

Every key begins with a realm and a namespace, so scans are tenant-local **by
construction** and a per-tenant key can be derived from the prefix with no key
table to keep consistent. That is real, and it is why the layout was frozen
early.

But it is a *layout* property, not an access-control one. With no
authentication, there is no identity to bind a realm to, so today the tenancy
machinery isolates data rather than users. Cross-tenant reports become
meaningful when authentication exists — which is exactly how the
[security policy](#reporting-a-vulnerability) scopes them.

## Memory safety and the supply chain

- **No `unsafe`.** The workspace denies it, and there is none. A memory-safety
  fault would therefore be in a dependency or in the compiler, and either is
  worth knowing about.
- **39 dependencies**, gated by `cargo deny check`: an allow-list of licences,
  a deny-list of specific crates, and advisory checks. All permissive, no
  copyleft.
- **`ring` is the only cryptographic dependency.** `aws-lc-rs` and `aws-lc-sys`
  are banned by name, and `deny.toml` records what that costs — they are
  FIPS-validated and `ring` is not.
- **One `cc` invocation, at most**, for any native dependency. No cmake, no
  bindgen, no external shared library, no build-time process. Enforced by
  `cargo xtask c-deps`, which keys on `links`/build-script presence rather than
  on crate names.

## Reporting a vulnerability

Open a **private security advisory** through the repository's GitHub Security
tab. Please do not open a public issue for an unfixed vulnerability.

Include what you need to reproduce it: the version or commit, the statement or
byte sequence, and what you observed. A crash is useful even without an
analysis.

### What to expect

The project has **one maintainer**, and these commitments are deliberately
modest so that they can actually be met — an unmet published SLA is worse than
a small one.

- **Acknowledgement within 7 days.** If you have not heard back, assume the
  report was lost and send it again.
- **An assessment within 30 days**, saying whether it is accepted and, if so, a
  rough severity and intended timeline.
- **No bounty programme.** Credit in the advisory and changelog if you want it,
  none if you would rather not.

There is no embargo policy yet, because there is no release cadence to
coordinate with. If a fix will take longer than you are willing to wait, say so
and a date can be agreed rather than left to drift.

### Scope

**In scope:** the engine, the Cypher implementation, the Bolt protocol
handling, the storage and WAL formats, and the server.

**Out of scope for now**, because they are documented absences rather than
defects:

- Anything an unauthenticated client can do. "An unauthenticated client can
  read data" is the documented state. Reports of what an *authenticated* user
  could do to another tenant become in scope when authentication exists.
- Plaintext traffic. There is no TLS.
- Denial of service through legitimate expensive queries. The row budget bounds
  one query's materialisation; it does not make the engine fair.

## Next

- [Known limits](../known-limits.md) — the absences, in one place.
- [Roadmap](../roadmap.md) — what is planned, and what is already built but not
  yet a product.
- [Operations](./operations.md) — running it day to day.
