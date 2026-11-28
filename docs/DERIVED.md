# Derived data

A graph is worth more with things computed from it: a full-text index, a
ranking, the communities it falls into, the shapes that repeat in it, a
vector per node to search by likeness. This note is the design for holding
those things inside a glider database without ever confusing them with the
graph.

It is a plan and a contract. Each section says what must be true when it is
built; the sections marked *built* say what is. Slices are listed at the end.

## The line between the graph and what is derived from it

Nodes, edges, labels, edge types and properties are the graph. They are the
only authority on what exists, what it is called and how it is connected.
Everything below is *derived*: it can be thrown away and computed again, and
when it disagrees with the graph the graph is right.

Two kinds of derived data exist, and the difference matters when rebuilding:

- **Computed** data comes from the graph alone: colours, classes, templates,
  communities, a structural embedding. The engine can rebuild it by itself.
- **Supplied** data comes from outside, about something in the graph: a
  vector an application made by running a text model over a node's
  description. The engine cannot recompute it. It can say exactly which
  entries no longer match their source, and the application supplies them
  again.

## Rules

These hold for every procedure in this note. They are numbered because the
tests cite them.

- **D1. The graph is never changed on its own account.** Nothing here adds,
  merges, deletes or relabels a node or an edge. A procedure that finds two
  nodes that look the same reports them. Two things being structurally alike,
  or having vectors that lie close together, is not evidence that they are
  one thing. A procedure writes a node property only when the caller names
  it with `write:`, as the built-in algorithms already do.
- **D2. A database has a revision.** `revision` is a 64-bit number that grows
  with every committed transaction that changed something, and with nothing
  else: not with an empty commit, a checkpoint, a reopen or a restore. It is
  durable. A replica that has replayed the same transactions reports the same
  revision. With `generation` (the lineage) it names one state of one
  database. A build that predates revisions must not be able to leave it
  behind: either that build advances it too, or it refuses the file.
- **D3. Derived data says what it was derived from.** Every derived record
  carries the revision it was computed at, and every named set of them (an
  index, a vector space, a partition) carries its algorithm, its
  configuration and a version of the code that wrote it.
- **D4. Staleness is reported, never hidden.** A read of derived data states
  the revision it reflects and whether its sources have changed since. With
  `fresh: "require"` a stale read is an error instead. The engine never
  answers from derived data it knows to be stale while saying nothing.
- **D5. No write goes unnoticed.** Every committed change to a node or an
  edge, by any path (a query, the graph API, a bulk load, an import, a
  replayed operation, `CLEAR`), either reaches the installed extensions
  inside the writing transaction or marks their derived state as needing a
  rebuild. No path does neither.
- **D6. Everything is bounded and can be cut into pieces.** Every procedure
  obeys the statement's time and step budget, takes its memory from
  `algorithm_memory()` and spills or refuses past it, and returns within the
  result caps. Work over a whole graph can be asked for in slices (`after:`
  and `limit:`, or an explicit list of nodes), so a caller can let other
  statements through between slices and stop when it likes. A stopped
  procedure leaves nothing half-written.
- **D7. The same question has the same answer.** The same database state and
  the same arguments give the same result, byte for byte, on the in-memory
  and paged tiers. Results that are partitions, hashes or orderings
  (colours, classes, templates, duplicates, path patterns) are also the same
  for two databases that hold the same data under different node ids, when
  the caller names the property that identifies a node with `identity:`.
  Floating-point results agree across such databases to a stated tolerance.
  Anything random takes a `seed:` and uses the engine's own generator.
- **D8. The engine's standing rules apply.** No dependencies. No recursion
  over data. No threads. No clock in a result: a time, when one is recorded,
  is supplied by the caller. Everything builds for wasm.
- **D9. A database that uses none of this pays for none of it** and behaves
  as before, with two exceptions that are improvements: `STATS` reports the
  revision, and `CALL` honours the statement budget.

## Revision

`STATS` gains `revision` and `generation`. `Graph::revision()` is the same
number for a library caller.

The write-ahead log's committed position already has most of the properties
D2 asks for on a file: it only moves when a transaction wrote records, it is
durable, and older builds move it as well. An in-memory graph without a log
has no such position. The slice that builds this decides the mechanism; D2 is
the test.

## Budgets for procedures (*built*, G2)

The statement budget (`QueryLimits::max_millis`, `max_steps`) holds for every
`CALL` as it does for a query, and an extension procedure gets the spilling
memory budget a built-in algorithm gets.

**What a step is.** One node or one edge looked at, whether it turns out to
matter or not. In a query: a candidate checked, an edge scanned, a neighbour
looked at, a combination of pattern parts tried (as before). In a `CALL`: a
node an algorithm or walk takes up, each edge it follows, each node a scan
reads, each per-node array entry a reset or a sort goes over, each node a
result is written back to; in the text index a posting read or written. An
extension function evaluated for a row is one step. Algorithms count the same
steps on the in-memory and paged tiers: the views do not count, the
algorithms do.

**The meter** is `src/meter.rs` (public as `glider::meter`, re-exported as
`glider::ext::{step, steps, spent, check, stopped}`). It is per thread, set
around a statement by `execute_capped` and `Cursor::open`. `steps(n)` is one
thread-local decrement of a credit of at most 256 steps; when the credit is
used up the meter is looked at, the clock read, and more credit handed out.
Tight loops count into a local `meter::Tally` and pass steps on 256 at a
time. Once the budget is spent the statement is *stopped*, and stays stopped:
every later count fails, so code that could not return the error where it
noticed (a callback, an extension function, an algorithm with no `Result`)
stops at its next count. The statement checks at its end and fails with the
budget's error whatever the procedure returned. The error is the one a query
gets, word for word (`statement stopped: it took more than its budget of N
matching steps`, `... ran past its time budget of N ms`), so callers that
match on it need no change.

**The bound.** Between two looks at the meter (and the clock) at most 256
steps happen, plus the work of one count: one node's neighbour list, a page
of 4,096 node ids while a projection is built, or, in the few places that go
over every node at once doing a constant amount for each, one such pass
(resetting the arrays per source in betweenness and closeness, pagerank's two
sums per iteration, the components scan for unvisited nodes, k-core's bucket
sort, the topological sorts' and label propagation's setup). A stop therefore comes within one adjacency list or one
array pass of the budget. Measured on a 200,000-node, 1,000,000-edge graph
(`tests/statement_budget.rs`), a 50 ms budget returned at 50.0 to 51.8 ms for
pagerank and betweenness (hours unbounded), a whole-graph BFS by the local
walk and by the paged projection (0.35 s), `context.retrieve` (2.5 s) and
`context.index` (1.5 s).

**Leaving nothing behind.** A stopped statement rolls back like any failed
one: a stopped `CALL pagerank(write: "rank")` has written no `rank` (tested at
every point it can stop, computing or writing), a stopped `context.index()`
leaves no index. Inside `BEGIN` a failed statement does not normally undo
what it wrote; a *stopped* one that had written anything rolls the open
transaction back and says so, so that half of a procedure cannot be
committed. One that had written nothing leaves the transaction open. A
stopped algorithm returns early with partial state that nothing uses; a
stopped projection build returns an empty projection; a stopped local walk
ends as if finished, so no whole-graph fallback starts. The memory and spill
files it held are freed when it returns (`tests/procedure_memory.rs`).

**Memory for extensions.** `ext::call` runs the procedure inside
`ooc::with_budget(g.algorithm_memory(), g.temp_dir(), ...)`, so `StateVec`,
`SpillQueue` and `SpillStack` spill to `<db>-tmp/` past `work_mem` exactly as
they do for `CALL pagerank()`.

**Without a budget** nothing changes: no result differs (every existing test
passes untouched, and every algorithm on every tier answers byte for byte the
same under a budget it does not reach), and counting costs one thread-local
decrement per count. Measured with `bench/src/bin/algobench.rs` on a shared,
busy machine, the algorithms themselves (`CORE=1`: the engine's code against
a copy of the code before this slice, on one in-memory projection,
interleaved run by run, fastest of 40 runs, pinned to one core; the same code
raced against itself differs by under 2%):

| graph | pagerank (20 iterations) | BFS, whole graph | components |
|---|---|---|---|
| 200,000 nodes, 1,200,000 edges | 119 → 115 ms (−3.5%) | 43 → 44 ms (+2 to +4%) | 25 → 25 ms (+1.4%) |
| 1,000,000 nodes, 2,000,000 edges | 721 → 722 ms (+0.2%) | 183 → 188 ms (+2.9%) | 138 → 142 ms (+3.0%) |

End to end through `CALL` (projection build included), the before and after
builds differ by less than the machine's noise (±10% between runs). Two
things the first version got wrong, for whoever counts in a hot loop next:
giving `Tally` a `Drop` that passed on its remainder made pagerank 2.3 times
slower (the drop flag and the call kept the count out of registers), and
counting each node of a pass that does a constant amount per node cost
pagerank 4 to 7%; such passes are counted at once.

**wasm.** There is no clock on wasm32, so `max_millis` is not kept there, for
procedures as for queries; `max_steps` is kept everywhere.

**Not covered.** `CREATE EXTENSION`'s install hook and `on_node_write` are
part of a write and bounded by it, not by the meter. `utils::Digraph` is Rust
API only, not reachable from a statement. Cancelling from another thread,
progress reports and slicing a procedure with `after:`/`limit:` are later
slices.

## Write hooks for edges and bulk paths

`on_node_write` stays as it is. Beside it an extension can ask for edge
writes (the edge before and after, with its endpoints and type) and is told
when the graph is cleared or loaded in bulk. D5 is the test: a database with
the change journal installed, written to by every path the engine has, has a
journal that accounts for every difference between the two states.

## `changes`: which nodes and edges changed

`CREATE EXTENSION changes` starts a journal of *which* nodes and edges
changed and at what revision. It does not keep old values: the graph has the
current ones, and that is what a rebuild needs.

```
CALL changes.since(revision: 1042, limit: 1000)   ids changed after a revision, oldest change first
CALL changes.status()                             oldest revision still answerable, entries, bytes
CALL changes.release(below: 1500)                 forget changes at or below a revision
```

- An id appears once, with the latest revision at which it changed and what
  happened to it last (`put` or `drop`). A node whose edges changed is listed
  as well, as `adjacency`, because much derived data depends on a node's
  surroundings and not on its properties.
- The journal is bounded (`cap:` at install, in entries). When the bound
  forces it to forget, `since` for a revision older than what it still holds
  says so plainly, and the consumer rebuilds from a scan. It never returns a
  partial list as if it were whole.
- Its cost on a write is one extension record. It is measured in the
  benchmark and stated here.

## `check`: is the graph whole

`glider <db> verify` checks pages and trees. Nothing checks the graph those
trees hold. `CALL check.run()` does, and repairs nothing:

| check | what it finds |
|---|---|
| `endpoints` | an edge whose source or target node does not exist |
| `adjacency` | an edge missing from either endpoint's adjacency, or an adjacency entry with no edge |
| `membership` | a label or type membership entry without its record, or a record missing from its membership |
| `counters` | label, type and index counts that differ from what a scan counts; id counters behind an existing id |
| `indexes` | a property index entry that does not match the node, or a node missing from an index it belongs in |
| `derived` | an extension's derived state older than the changes to its sources, or referring to ids that are gone |
| `duplicates` | parallel edges with the same endpoints, type and properties; nodes of a label that agree on the properties named in `keys:` |
| `orphans` | nodes with no edges, optionally of given labels |
| `required` | nodes of a label lacking an edge the caller says they must have (`label:`, `type:`, `dir:`) |

It yields `check, severity, kind, id, detail`, takes `checks:` to run a
subset, `after:` and `limit:` to run in slices, and ends with a summary row
per check. The first five are faults in the database. The rest are facts
about the data that an application may or may not consider faults, so they
run only when asked for.

## `structure`: sameness and repetition

Graphs made by people repeat themselves. A plant has forty stations built
the same way; each has two sides, each side its work locations, each
location its work elements and their instructions. Described node by node,
the description says the same thing forty times. `structure` finds the
repetition so that it can be said once, and it does so without losing which
station is which.

```
CALL structure.colors(depth: 3, types: [...], dir: "out", props: [...])
CALL structure.classes(by: "neighbours" | "colors", ...)
CALL structure.templates(types: [...], dir: "out", min_nodes: 3, min_count: 2)
CALL structure.paths(types: [...], dir: "out", min_count: 2)
CALL structure.quotient(by: <property or stored partition>)
CALL structure.plan(lambda: 1.0, ...)
CALL structure.expand(of: <class, template or pattern>)
```

- **Colours.** Weisfeiler-Lehman refinement over labels, the chosen
  properties and typed, directed edges. Two nodes have the same colour at
  depth *d* exactly when their surroundings to depth *d* cannot be told
  apart by those things. Colours depend on content only (D7).
- **Classes.** Nodes that are interchangeable in the structure: the same
  labels and the same typed neighbours (`by: "neighbours"`), or the same
  colour at a fixed point (`by: "colors"`).
- **Templates.** A shape that occurs many times along a hierarchy, with the
  list of places it occurs. A template has *slots*: the positions where
  instances differ (their own names, numbers, dates). An instance is a root
  node and its values for the slots. Every instance keeps every one of its
  nodes and properties; the template is a way of writing them down, not a
  replacement for them. `expand` gives back exactly the nodes and edges of
  an instance, and a test proves it for every instance (D1).
- **Path patterns.** Sequences of labels and edge types that many paths
  follow (`Plant -HAS-> Shop -HAS-> Line -HAS-> Station ...`), with counts,
  and runs of nodes with one way in and one way out that can be written as
  one hop with its interior listed.
- **Quotient.** The graph of classes: one node per class with its size, one
  edge per pair of classes and edge type with a count of the edges it stands
  for. `expand` lists a class's members. A quotient is a summary, and says
  so; it is never returned in place of the members without the caller asking.
- **Plan.** Whether writing a graph with a given template, pattern or
  quotient is actually shorter. For each candidate it reports the cost of
  describing it once, the cost of each use, and the cost of the plain form it
  replaces, in the units the `context` extension budgets in. A candidate is
  chosen only when the total goes down. For a lossy candidate (a quotient
  used without its members) the properties and edges it leaves out are
  counted and weighed by `lambda`. This is the minimum description length
  principle applied with the real serializer as the code, so the saving it
  reports is the saving a caller sees.

Built-in algorithms gain three names beside the existing ones:
`CALL louvain()` (communities by modularity, deterministic, with levels),
`CALL ppr(seeds: [...])` (the personalised PageRank that `context.retrieve`
already computes, on its own), and `CALL steiner(nodes: [...])` (a small
tree that connects the given nodes, for showing how a set of things relate).

## `vector`: vectors beside the graph

A vector is not a good property. As a list of floats it costs nine bytes an
element on disk and thirty-two in memory, cannot be searched, and says
nothing about where it came from. `vector` stores vectors in the extension
keyspace as packed 32-bit floats, in named *spaces*.

A space records, once, everything needed to know whether two vectors may be
compared and where they came from:

| field | meaning |
|---|---|
| `dims`, `metric`, `normalized` | the shape; `metric` is `cosine`, `dot` or `l2` |
| `of` | what its vectors are about: `node`, `edge` or `set` (a named group of nodes) |
| `algorithm`, `config` | what computed them and with which settings |
| `model`, `model_version` | for supplied vectors, the model the application used |
| `code` | the version of the engine code or of the application that wrote them |
| `revision`, `at` | the revision when the space was last complete; a caller-supplied time |

Each vector records the revision it was derived at and, for supplied
vectors, a hash of its source text that the application passes in.

```
CALL vector.create(space: "text", dims: 384, metric: "cosine", of: "node", model: "...", ...)
CALL vector.put(space: "text", ids: [...], vectors: [[...], ...], hashes: [...])
CALL vector.search(space: "text", vector: [...], top: 10, labels: [...])
CALL vector.similar(space: "text", id: 42, top: 10)
CALL vector.stale(space: "text", limit: 1000)
CALL vector.spaces()        CALL vector.drop(space: "text")
```

- A search refuses a query vector whose length is not the space's, and
  `similar` across two spaces is refused unless their shape, algorithm, model
  and model version agree. Vectors from different models are never compared.
- A deleted node's vectors go with it, in the same transaction.
- `stale` lists the ids whose source changed after their vector was made and
  the ids that should have a vector and do not. It needs `changes`; without
  it, it says that it cannot know (D4).
- Search is exact: every candidate is scored, with a bounded heap for the
  best. An approximate index is a later slice and is built only if the
  benchmark shows exact search is too slow at sizes that matter, with its
  recall against exact search stated beside its speed.
- A vector never stands in for a node. A search returns ids and scores; what
  the node is comes from the graph.

## `embed`: vectors from structure

Procedures that compute a space from the graph, with no model and no
dependency, deterministic for a seed (D7):

| procedure | what it computes | recomputes after a change |
|---|---|---|
| `embed.fastrp` | random projections averaged over neighbours, optionally starting from another space's vectors | the nodes within reach of the change, exactly |
| `embed.propagate` | each node's vector blended with its neighbours' from an existing space, by edge type and distance: structure added to a text space, staying in that space | the nodes within reach, exactly |
| `embed.walks` | random walks and skip-gram training (DeepWalk; node2vec with `p` and `q`; edge types as tokens for typed graphs) | walks from the changed nodes with the rest held still; a full run when drift passes a threshold |
| `embed.ppr` | a sketch of each node's personalised PageRank | the nodes within reach |
| `embed.wl` | a hashed count of Weisfeiler-Lehman subtrees: for a node's surroundings, a set of nodes, a community, or the whole graph | the sets touched |
| `embed.graph2vec` | a trained vector per set of nodes over those subtree counts | as `embed.walks` |
| `embed.pool` | a vector for a set (surroundings, community, graph) from its members' vectors | the sets touched |
| `embed.edges` | a vector per edge from its endpoints and its type | the edges touched |

Networks that are trained by gradient descent over the graph (GraphSAGE,
graph attention) need automatic differentiation and are not built into an
engine with no dependencies. They are compared against the procedures above
on the same retrieval task from outside the engine, and their vectors can be
stored in a space like any other supplied vector. `embed.propagate` is the
untrained core of the mean-aggregating form.

Which of these is worth using is a question for measurement, not for this
note. The benchmark below decides, and its results are recorded in
`docs/BENCHMARKS.md`.

## `context`: what changes

`context.retrieve` is where the rest is put to use for a language model.

- **A scope.** The caller can say which nodes may be seen at all, by label
  and by property value. Nodes outside it are not visited, not counted, not
  scored and not mentioned, and the token budget is spent only on what may be
  seen. A caller that must not see something cannot learn of it from a count
  or an omission line.
- **Weighted seeds and vector seeds.** Seeds can carry weights, and can come
  from a `vector.search` in the same call, beside or instead of the text
  search.
- **Identity.** With `identity:`, references in the output and the order of
  everything in it follow that property instead of node ids (D7).
- **A compact form.** `format: "compact"` writes the selected subgraph with a
  dictionary of templates and path patterns from `structure`, each defined
  once and used by reference, when and only when `structure.plan` says it is
  shorter. A dictionary can be given by the caller so that many answers
  share one that was written out earlier. Whatever is not covered by the
  dictionary is written plainly. Nothing is left out because of the form.
- **Expansion.** Every omission line and every reference to a class or a
  template can be passed back to get the detail.
- **A version.** The output names the serialization version that wrote it.

## Benchmark

`bench/` gains a generator for a manufacturing graph and a harness over it.

The graph: plants, shops, lines, stations, sides, work locations, work
elements and work instructions in a hierarchy; many stations built from a
handful of designs; tools and instructions shared between work elements;
parts and work orders; effectivity (which instruction applies to which model
and from which date); dependencies between work elements. It is generated
from a seed, and the generator knows the truth the algorithms are asked to
find: which stations share a design, which nodes are planted duplicates,
which instructions are about the same thing.

Measured, at several sizes, against that truth:

- structure: bytes and estimated tokens for the plain, templated and
  quotient forms; that `expand` gives back the original exactly; time and
  memory to find templates, classes and communities;
- vectors and embeddings: time to compute, bytes stored, search latency,
  recall and precision at *k* for each `embed` procedure and for supplied
  vectors, and the cost of an update after a small change against a full run;
- change tracking: write throughput with and without `changes`;
- integrity: time for `check.run` and that planted faults are all found.

## Slices

Each is one pull request against the branch that carries this note, with its
tests, and adds its section to `docs/EXTENSIONS.md` and its numbers to
`docs/BENCHMARKS.md`.

| slice | builds | needs |
|---|---|---|
| G1 | revision (D2), in `STATS` and the library | |
| G2 | budgets for procedures (D6) | |
| G3 | the manufacturing generator and the harness's skeleton | |
| G4 | edge and bulk write hooks (D5) | |
| G5 | `changes` | G1, G4 |
| G6 | `check` | G1 |
| G7 | `structure`: colours, classes, duplicates in `check` | G2 |
| G8 | `structure`: templates, path patterns, plan, expand | G7 |
| G9 | `structure`: quotient; `louvain`, `ppr`, `steiner` | G2 |
| G10 | `vector`: spaces, storage, exact search, staleness | G1, G5 |
| G11 | `embed`: fastrp, propagate, pool, edges | G10 |
| G12 | `embed`: wl, for nodes, sets and graphs | G7, G10 |
| G13 | `embed`: walks, ppr, graph2vec | G10 |
| G14 | the embedding comparison, written up | G11, G12, G13 |
| G15 | `context`: scope, weighted and vector seeds, identity | G2, G10 |
| G16 | `context`: compact form, dictionaries, expansion, version | G8, G15 |
| G17 | an approximate vector index, if G14 calls for one | G14 |
