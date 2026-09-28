# Graph algorithms

```cypher
CALL engram.algo.pageRank.stream({nodeLabels: ['Person'], relationshipTypes: ['KNOWS']})
YIELD node, score
RETURN node.name, score ORDER BY score DESC LIMIT 10
```

PageRank, weakly connected components, **strongly** connected components,
degree centrality, BFS and weighted SSSP, triangle count and local clustering,
label propagation, Louvain, exact betweenness centrality and closeness
centrality. Each runs in four modes.

Two pairs there answer questions that are easy to confuse, and both are kept
separate deliberately:

- **WCC and SCC.** WCC asks which nodes are joined ignoring arrow direction;
  SCC asks which can REACH each other following them. On a directed acyclic
  chain WCC reports one component and SCC reports one per node — both correct,
  to different questions. Components are labelled by their smallest node id in
  both, so the answers are directly comparable.
- **Degree and closeness.** Degree is local. Closeness is global, and takes
  the Wasserman-Faust form — scaled by the fraction of the graph a node
  reaches — because the plain definition divides by an infinite distance on a
  disconnected graph. Without that scaling a node in a tight pair scores a
  perfect 1 and outranks the centre of a much larger component.

Two PATH procedures sit beside them and have a stream mode only, because a
route is not a per-node value — there is nothing to write onto a node and no
distribution to summarise:

```cypher
CALL engram.algo.kShortestPaths.stream({sourceNode: $a, targetNode: $b, k: 5})
YIELD index, totalCost, nodeIds RETURN index, totalCost, nodeIds
```

and in the query language itself, `allShortestPaths(…)` beside `shortestPath(…)`
— the first returning EVERY route of minimum length, the second one of them.
That is not a `LIMIT` relationship: a caller cannot recover the set from the
single answer.

`kShortestPaths` is Yen's algorithm, returning loopless routes ordered by
`(cost, node sequence)` so equal-cost routes always come back in the same
order. With `k = 1` it is a single Dijkstra from source to target — Yen's spur
searches exist only to find routes 2 to `k` — and it is priced as one (see
[the ceilings](#it-refuses-rather-than-tries)).

## The rule that makes `write` safe

**The procedure never writes; the statement does.**

Compute runs against a read snapshot, outside any write transaction, producing
`(node, value)` plus an `asOf` vintage. `mutate` publishes into a cache and
touches no keyspace row. `write` is an **ordinary bulk property set through the
normal write path**, in a separate short transaction, *after* the computation
has returned.

That is not a convention to be careful about — it is the shape. A write mode
that wrote *during* the fixpoint would hold entity locks for the whole run, and
no amount of care inside the procedure could avoid it.

The consequence is that a written result is stale by construction: the values
describe snapshot `S` and land at commit `C`. Both stamps are reported:

```cypher
CALL engram.algo.pageRank.write({writeProperty: 'pagerank'})
YIELD nodesWritten, asOf, committedAt
```

GDS returns one write summary and never tells you the scores describe a graph
that no longer exists.

`write` **refuses inside an open transaction**, naming why: an enclosing
transaction would either see its own uncommitted writes in the snapshot, or
hold locks across the computation.

## The four modes

| mode | returns | writes |
|---|---|---|
| `stream` | one row per node, ascending by node id | nothing |
| `stats` | one summary row with a distribution | nothing |
| `mutate` | a receipt; the result goes in the cache | nothing durable |
| `write` | a receipt with both stamps | a node property |

`stream` is ordered by node id rather than by score, because sorting by score
needs a tie-break the algorithm cannot supply — and `ORDER BY score DESC`
composes on top for free.

`mutate` is the composition mechanism, equivalent to GDS's mode of the same
name: run PageRank once, then read it back by name.

```cypher
CALL engram.algo.pageRank.mutate({mutateKey: 'pr1'}) YIELD mutateKey
CALL engram.algo.result.stream({mutateKey: 'pr1'}) YIELD node, value
CALL engram.algo.result.list() YIELD mutateKey, asOf, stale
CALL engram.algo.result.drop({mutateKey: 'pr1'}) YIELD dropped
```

## LDBC Graphalytics conformance

Five of the six LDBC Graphalytics kernels, as the engine answers them by
default, diverge from the published specification in ways that are defensible
as engine behaviour and wrong as conformance. `graphalytics: true` in the
configuration map selects the specification's semantics:

| kernel | default | `graphalytics: true` |
|---|---|---|
| BFS | unreachable is `null` | `9223372036854775807` |
| SSSP | unreachable is `null` | `Infinity` |
| CDLP (label propagation) | in- and out-neighbours deduplicated; a detected two-cycle stops the run | counted separately, so a reciprocal edge votes twice; no two-cycle escape |
| PageRank | stops at `tolerance` | exactly `maxIterations` rounds |
| LCC | the triangle test symmetrised | direction kept in the edge test |

It is gated rather than switched because both readings are legitimate: a
`null` unreachable distance is the better answer for a query, and the sentinel
is the one the benchmark validates. Changing the default would silently alter
every existing caller's answers; offering no way to conform would make the
benchmark unrunnable. The flag travels on the result rather than being re-read
from the configuration, so the rows are rendered with the sentinels of the run
that computed them.

The directed LCC is the specification's rule: `N(v)` is `v`'s in- and
out-neighbours as a set, and the numerator counts the ordered pairs `(u, w)`
from `N(v)` joined by a directed edge `u -> w`, over `d(d - 1)`. Such a pair
makes `{v, u, w}` a triangle of the symmetrised graph, so the numerator is
counted per triangle: each triangle is found once and each corner credited the
directed edges between the other two, read from a two-bit direction record on
each adjacency entry. Self-loops never count and a multi-edge is one edge.

Both clustering coefficients and the triangle count find their triangles the
same way: the **forward algorithm over a degree-oriented CSR**. Each undirected
edge is kept once, from its lower-`(degree, id)` end, which bounds any vertex's
oriented out-degree by `O(√E)`; a triangle is then found exactly once, at its
lowest-ranked corner, as the common neighbour of two sorted oriented rows. A
hub with a million neighbours costs `O(√E)` rather than `O(d²)` — the
difference between a kernel that scales with the graph and one that scales
with its largest vertex.

The kernels are pinned against LDBC's own validation graphs and published
output, with the published parameters, and all six conform under the flag. The
current results on the S-size graphs are on
[Three engines at SF3 and SF10](../measurements/three-engines-sf3-sf10.md#graphalytics-engram).

## A projection built from rows

An algorithm normally projects stored nodes and relationships from its
configuration. `engram.algo.project` builds one from rows instead, so a
statement can run an algorithm over weights it has just computed without
writing a relationship per edge:

```cypher
MATCH (a:Person)-[k:KNOWS]->(b:Person)
WITH collect({source: id(a), target: id(b), weight: 1.0 / (1.0 + k.replies)}) AS edges
CALL engram.algo.project({name: 'g', nodeLabels: ['Person'], edges: edges,
                          orientation: 'UNDIRECTED'}) YIELD projection
MATCH (s:Person {id: $a}), (t:Person {id: $b})
CALL engram.algo.kShortestPaths.stream({projection: projection,
       sourceNode: id(s), targetNode: id(t), k: 1})
YIELD totalCost
RETURN totalCost
```

The vertices are the members of `nodeLabels`, not the edges' endpoints, so a
node with no edge is still in the graph and a route to it is "none" rather
than "not in this projection". Every weight must be a finite number — a
projection whose weights the caller computed has no business guessing one —
and an edge with an endpoint outside `nodeLabels` is left out and reported in
`outsideProjection`, never silently kept. The
numbering is the same determinism argument as a stored projection's: vertices
by ascending id, each row's edges sorted by target and then by weight, so the
arrays are a function of the edge set whatever order the rows arrived in.

It is priced before it is built against the same node, edge and byte ceilings,
and every live row-built projection shares one budget, the byte ceiling. It
lives only as
long as its statement: `YIELD projection` returns a handle (`name#generation`)
that the same statement passes back as `projection`, which is exclusive with
`nodeLabels`, `relationshipTypes`, `relationshipWeightProperty` and
`orientation`, because the projection already fixed all four. It must be
called once on the statement's own thread — after an aggregation such as
`collect()`, not per row of a parallel stage.

## Determinism, in four links

This is the non-obvious part, and it will otherwise be re-derived wrongly.

1. **The dense numbering is a pure function of the ascending member id set.**
   Vertices are numbered by their position in the sorted id vector.
2. **Every adjacency row is sorted** by dense id. The store's own row order
   depends on the relationship type layout and on which cached table served —
   neither of which is a property of the graph.
3. **Kernels pull.** `next[v]` is computed from `v`'s in-neighbours, so no
   floating-point sum is ever owned by two vertices.
4. **Morsels partition the output range** and merge in morsel order, so a
   parallel run is **bit-identical** to a serial one rather than merely
   equivalent. `pagerank_is_bit_identical_at_every_scoped_exec_width` pins it.

The other kernels that split keep the same guarantee by their own routes. A
label-propagation round reads only the previous round's labels, so each
vertex's new label is a pure function of the last round and the morsels
concatenate in order. Triangle counts and both clustering numerators are
integers added atomically, exact whatever the width or the order of the adds.
The symmetrised and degree-oriented adjacency they build is written row by row,
each row from its own vertex alone, and concatenated in morsel order, so it is
the serial build byte for byte. `a_graph_kernel_splits_across_the_executor`
asserts that each of them answers on four threads exactly as on one.

### What that test had to be given before it proved anything

Worth recording, because the first version of this layer satisfied every word
above and still tested none of it. Three separate things each made the
differential vacuous, and each was invisible on its own:

- **The seam was never crossed.** The driver sized its morsels from
  `exec.width()` and then evaluated them with a plain iterator chain;
  `ScopedExec::for_each` was not called. Varying the width proved the
  *partitioning arithmetic* was deterministic — which is true of a driver that
  never parallelises. Fixed by `the_fixpoint_actually_hands_its_morsels_to_the_executor`,
  which asserts one `for_each` per iteration and `width` morsels per call, and
  which counts **on the test's side**: a `counted!` inside a morsel records
  onto a worker thread and is dropped, so it could never have fired.
- **The corpus could not discriminate.** The graph was built from
  `i -> 7i+3` and `i -> 13i+5` mod 37 — two *bijections*, so every vertex had
  in- and out-degree exactly two and PageRank converged to the uniform vector.
  A uniform vector is invariant under permutation, so a merge that reassembled
  partials in completion order rather than morsel order passed at every width,
  on real threads, bit for bit. The corpus now uses degrees one to five and
  asserts its own spread before trusting what it proves.
- **The scheduler was too well behaved.** Each morsel is microseconds of work,
  so the first worker drains the cursor before its siblings are scheduled and
  completion order comes out ascending anyway. A deliberately hostile
  `ReverseExec` — a legal implementor, since the trait lets the implementor
  choose scheduling — turns a race the test has to be lucky to observe into
  arithmetic it observes on every run.

The generalisable rule, and the reason this is in the architecture doc rather
than a commit message: **a differential is evidence only once you have shown
the path it compares actually ran, and that its inputs can tell the two arms
apart.** Neither half is implied by the test being green.

A fourth arrived while fixing the first three, from the other direction: the
size floor below (`Graph::algo_min_vertices`) was added afterwards, and it put
the test's 37-vertex corpus under the threshold where every width collapses to
one morsel. A correct performance decision silently disarmed a correctness
test. The corpus is now 4,999 vertices — prime, so no width divides it evenly
— and the two files that assert the seam is crossed both build above the
floor and say why at the site.

### Reaching the lane

Parallelism is armed by `ENGRAM_QUERY_PARALLELISM`, which installs the
executor, and disarmed for algorithms alone by `--no-algo-parallel`, which is
the A/B arm within one binary.

Which kernels split: PageRank's fixpoint; each round of label propagation;
the triangle count and both clustering coefficients, whose symmetrised
adjacency, degree orientation and triangle enumeration are all built across
the executor (`vertex_morsels`: sixteen morsels a worker, at least 64 vertices
each, so a morsel of hubs does not hold the run up). BFS and SSSP walk a
frontier and WCC's union-find is order-dependent in its structure, so those
three, with Louvain, SCC, degree, closeness and betweenness, run on one thread.

Four things narrow the split:

- **an installed executor**, so a run with no width configured is byte for
  byte the run that shipped;
- **the size floor.** Below `Graph::algo_min_vertices` = 65,536 vertices the
  executor is not entered at all — unless the projection's edge count reaches
  sixteen times that floor (1,048,576 at the default), because a kernel's work
  is its vertices *and* its edges, and a small, dense graph (a Graphalytics
  graph of 61,170 vertices carries over fifty million edges) is heavy work
  under a vertex-only floor. Note *not entered*, rather than entered and asked
  for one morsel: a thread pool honours `for_each(1, ..)` by spawning a scope
  to run one closure, so a floor implemented that way would pay the very spawn
  it exists to avoid.

  The answer is identical either side of the floor, which is what makes it a
  cost decision rather than a semantic one.

- **the process-wide morsel budget.** The lane takes the same installed
  executor every other morsel body takes, so its `for_each` draws from the same
  slot budget. The budget defaults to the configured width, so an algorithm
  running alongside a query can be granted nothing and run its fixpoint
  serially. The answer is the same either way; only the schedule changes. See
  [Concurrency](./concurrency.md).

- **`concurrency` in the config**, which narrows the installed width through a
  capped view of the executor and can never widen it — the engine spawns
  nothing, so the workers available are whatever the server installed. Only
  `width()` is narrowed; `for_each` is forwarded untouched, because the
  operator sizes its morsels from the reported width.

**And the honest part, for PageRank.** PageRank pulling over a materialised
CSR is memory-bandwidth-bound, and threads do not add bandwidth; the `O(V+E)`
instruction count that suggests otherwise is not the bottleneck. What remains
is capped by the two O(V) serial passes each iteration keeps — the morsel-order
merge and the width-independent convergence fold — and neither may be
parallelised without giving up bit-identity, which is not for sale. The vertex
floor was set by a width sweep of PageRank on a sparse graph, at the size where
the split stops LOSING, not where a gain starts, and this book quotes no
speed-up for PageRank's split.

The lane is kept because it is correct, gated, off unless an executor is
installed, and free below the floor — and because a compute-bound kernel
inherits it. Triangle enumeration and label propagation are those kernels:
they do real work per vertex, and they took the lane's seam, budget and floor
when they joined it, the floor gaining its edge term for them.

Unlike `expand`'s dispatch, the split here is **not** gated on `in_txn`. That
gate exists because expand's workers read the store, and overlays and the OCC
read-set are thread-local. A fixpoint's morsel body calls `VertexProgram::pull`,
which reads the already-materialised CSR and nothing else — no store, no
overlay, no thread-local — and the other kernels' morsel bodies likewise read
the projection alone, so the hazard cannot arise, and copying the condition
anyway would imply a risk that had been weighed and found real.

The global scalars — PageRank's dangling mass, the convergence delta,
modularity — are folded **serially, in ascending vertex order, over the
finished array**, never from per-morsel partials. A partial-based reduction
regroups float addition with the executor's width, and the delta feeds the loop
condition, so the *number of iterations* would depend on how many threads were
available. The extra pass is deliberate and will look like waste in a profile.

**Louvain is strictly sequential and will never have a parallel variant.** Its
local-moving phase is Gauss-Seidel by definition — the gain of moving a vertex
depends on where its neighbours are *right now*, including moves made earlier
in the same sweep — so the sweep order (ascending node id) is part of the
specification. A parallel variant added later would not be an optimisation; it
would silently change every result already published.

## It refuses rather than tries

`shortestPath` declines to a slower correct path. **An algorithm has no slower
path** — there is no other way to compute PageRank — so a projection it cannot
afford produces a typed error naming the numbers and the lever, raised *before*
anything is allocated:

| ceiling | default | lever |
|---|---|---|
| nodes | 20,000,000 | `ENGRAM_ALGO_NODE_CEILING` |
| edges | 200,000,000 | `ENGRAM_ALGO_EDGE_CEILING` |
| working set | 2 GiB | `ENGRAM_ALGO_BYTE_CEILING` |
| all-pairs work (`V x E`) | 10,000,000,000 | `ENGRAM_ALGO_WORK_CEILING` |
| result cache | 512 MiB | `ENGRAM_ALGO_CACHE_BYTES` |
| iterations | 100 | `maxIterations` in the config |

The rejected alternative is "build it and let the allocator decide", which is
what GDS does, and which ends at the out-of-memory killer — taking every other
session with it — rather than at an error.

**The all-pairs row exists because the other two cannot fire for betweenness.**
Its cost is `O(V x E)` where everything else here is `O(V + E)`, so a million
nodes and four million edges — comfortably inside both — is four times ten to
the twelve units of work, which is days. Closeness is priced the same way.
Yen's k-shortest paths is too, with `k` as a further factor, from `k = 2`: with
`k = 1` it is one Dijkstra, `E + V log V`, and only the node, edge and byte
ceilings apply.

### Two ways these guards were dead, and how

Worth recording, because both had shipped and neither was visible:

- **The levers were fiction.** Every refusal above names an environment
  variable, and for a release *nothing anywhere read them*. An operator
  following the message exactly would set the variable, see no change, and
  have no way to tell the advice was wrong. They are settable cells now, and a
  test lowers each one and asserts the refusal names it.
- **The edge estimate was always zero.** The pre-flight cost was priced with
  `edge_count_slim(id, dir, types, u64::MAX)` — whose last parameter is a peer
  NODE ID, not a cap. So it asked how many edges ran from each node to node
  18,446,744,073,709,551,615, which is none. The edge ceiling, the edge term of
  the byte ceiling, and the all-pairs work ceiling that multiplies by it were
  all reading `e = 0`: three guards that could not fire, presenting as three
  guards that had never needed to. The price now comes first from an O(1)
  upper bound read off the maintained per-type edge counts — if the bound
  clears every ceiling, the exact figure does too — and otherwise from each
  node's degree by `count_adjacent_memo`. The ceiling tests set a ceiling low
  enough to demand a refusal — which is the only kind of test that could have
  caught it.

An **unknown config key is an error with a suggestion**, not a silent default:
a misspelled `tolerance` that is ignored converges somewhere else and says
nothing.

A **negative relationship weight is refused** by SSSP rather than mis-answered.
Dijkstra settles a vertex permanently on first pop, which is sound only while
no later edge can shorten it; over negative weights it returns a plausible
wrong answer.

## The two caches have opposite rules

The **projection** is a pure function of committed state, so a stale projection
is a *wrong* one and it rebuilds.

Which also says exactly when one may be kept. A projection built at commit
clock `t` is exact for every statement that runs while the clock still reads
`t`, so it is **kept between statements while nothing commits**: the next
statement over the same projection key reuses it, and any commit makes the
next one rebuild. The clock is read *before* the build, so a commit landing
during it leaves the kept copy already stale. A transaction with buffered
writes neither reads nor keeps one, since its own writes could change the
answer. At most two are kept per graph (typically its unweighted and weighted
projections), within twice the algorithm byte ceiling, in a process-wide table
keyed by the graph's never-reused id and cleared when the graph is dropped.
Within one statement the projection is also memoised, so a query that calls an
algorithm once per row builds it once. The reverse adjacency is built only when
a kernel first reads it, and kept with the projection; a kernel that never
reads it — BFS, SSSP, degree — never builds it.

A weighted projection reads each edge's weight in one sorted gather of the
weight property across the whole projection, off each relationship's bytes
through a shared block cursor, rather than a full record read per edge. Each
edge takes its own weight, so parallel edges between one pair keep theirs
apart.

A **`mutate` result** is a function of the state at its `asOf` — a measurement
of a past graph, not an approximation of the present one. Auto-invalidating it
on an epoch change would make it vanish under any concurrent write, destroying
the compute-once-read-many workflow it exists for, and the only available
"refresh" would be a silent multi-minute recompute inside what the user wrote
as a read. So it is **explicitly versioned and never implicitly refreshed**:
`result.list` reports `stale`, every row carries `asOf`, `mutate` overwrites,
`drop` removes.

An eviction under the memory budget makes the next read an **error naming the
lever**, never an empty answer — "the algorithm found nothing" and "that result
is gone" are different facts, and only one of them is about the graph.

## What is absent

- **No results are persisted.** A cached result does not survive a restart. It
  is a session-scoped measurement with a user-chosen name, not a derived
  structure of the store.
- **No incremental maintenance.** Every run recomputes its kernel. A projection
  is reused only while nothing has committed since it was built, and the first
  commit after that rebuilds it whole; nothing patches a kept projection with a
  delta.
- **Nothing feeds the query optimiser.** `stats` emits a distribution, which is
  an input to manual tuning rather than to plan choice. Wiring a cached
  histogram into cardinality estimation would make the same query plan
  differently on two identical databases depending on whether an unrelated
  procedure had been called — a determinism violation dressed as an
  optimisation. The right shape is a *maintained* degree histogram, always
  present and never user-triggered, and that is separate work.
- **No SAMPLED betweenness.** The exact Brandes computation is present;
  GDS's RA-Brandes sampling, which trades accuracy for a lower cost on graphs
  over the work ceiling, is not. The ceiling refuses rather than approximating,
  because an approximation nobody asked for is a wrong answer that looks right.
- **No eigenvector centrality, no ArticleRank, no A\*, and none of the
  similarity family** (node similarity, k-nearest neighbours). Those are a
  different kind of question from anything here — a pairwise score rather than
  a per-node one — and would need a result shape this layer does not have.
- **SCC is Kosaraju's, not Tarjan's.** Tarjan is one pass rather than two and
  is the usual choice; it is also naturally recursive, and a recursive descent
  over a projection the node ceiling permits overflows the stack long before
  it finishes. An iterative Tarjan is possible and genuinely fiddly — the
  low-link update on return from a child has to be replayed by hand, and
  getting it subtly wrong yields components that are merely plausible.
  Kosaraju's two passes are each a plain iterative DFS over structures that
  are already resident.
- **Betweenness is serial and will stay so.** Brandes parallelises over
  sources, and every vertex's score is a sum over source contributions — so a
  parallel run would add them in thread-completion order, and floating point
  addition is not associative. Summing per-source partials in a fixed order
  would restore determinism, and is rejected on memory: a partial is `O(V)`
  f64 per source in flight, so eight workers over a million-vertex projection
  would hold 64 MB of partials. The serial loop is the honest choice.
- **Triangle counting treats the projection as undirected**, since a directed
  triangle count is a different quantity, and so does the clustering
  coefficient by default. Under `graphalytics: true` the coefficient keeps
  direction in its edge test, as the specification defines it (above).
