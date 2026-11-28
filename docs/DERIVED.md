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
  with every committed transaction that changed something: it never stays
  still across a change. It does not grow with an empty commit, a write that
  changes nothing, a checkpoint, a reopen or a restore; it may also grow when
  a transaction's operations cancel each other out. It is durable. A replica
  that has replayed the same transactions reports the same revision. With
  `generation` (the lineage) it names one state along one line of history:
  the engine has one writer per lineage, but a copy that is restored and then
  written to is a second line, and can share a pair with its source or with
  another copy. Derived data kept inside the database is restored with its
  copy and carries on with it, so it stays right; a consumer outside the
  database that keys what it keeps by the pair must treat a restore as a new
  source. A build that predates revisions must not be able to leave it
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

## Revision (*built*)

`STATS` reports `revision` (an integer) and `generation` (32 hex digits, all
zeros for a memory graph that has no log). `Graph::revision()` is the same
number for a library caller; it reads a field and costs nothing.

**The revision is the committed position of the write-ahead log**: the byte
offset just past the last committed transaction's marker. No new field is
stored and the file format is unchanged.

- *Grows with every change.* Every change goes through one logged operation,
  and a transaction that logged anything writes a commit marker, so the
  position moves forward. Positions are bytes, so the number is not dense.
  The test is per operation, not per transaction: a statement that sets a
  value and sets it back, or `CALL context.index()` over an index that is
  already current, moves the revision although nothing ends up different.
  The guarantee runs one way: if the state changed, the revision moved.
- *Holds for everything else.* An operation that would change nothing (a
  property set to the value it has, compared exactly, so `1` and `1.0`
  differ; an absent property or label removed; a label or an index that is
  already there; an extension writing what it holds) is no longer written to
  the log, and a node write that changed nothing calls no extension hooks.
  (A property set to its own value still rewrites the record, so the page
  cache sees the same accesses it always did; only the log is spared.)
  A commit that logged nothing writes no marker. A rollback or a failed
  statement cuts the log back to the last commit. Checkpoints, `COMPACT`,
  closing and reopening and reads do not write to the log. `CLEAR` always
  counts as a change.
- *Durable.* A checkpoint records the position in the superblock and reopen
  resumes there, whether or not any log segment is left; recovery after a
  crash ends at the last commit marker.
- *The same on a replica.* Replicas, file or memory, are made of a base taken
  at a log position and the log shipped after it, at the same positions, so
  a restore reports the position it replayed to: the source's revision at
  that point, and an earlier one for an earlier point.
- *Memory graphs.* With a log (`enable_log`), the revision is that log's
  position. Without one, the graph counts the bytes a log would have written
  for the same transactions, so it reports the same revision a logged graph
  or a file reports for the same history, and a log enabled later starts
  there instead of at 0, so enabling a log does not move the revision back. That number lives and dies with the graph object: it is as durable
  as the graph is, which is not at all. A graph loaded from a file's image
  (`Graph::from_bytes`) has no lineage and starts at 0.
- *Bulk load* builds a new file without the log. It checkpoints the loaded
  pages, then logs one record (the id counters it set) and checkpoints
  again, so a loaded database is past 0. A crash between the two reopens the
  loaded data at revision 0, as a load did before revisions (and an
  interrupted load is to be deleted and run again); a crash never leaves the
  empty file past 0.
- *Older builds cannot leave it behind.* They log every operation they
  apply, including the ones that change nothing, commit with a marker, keep
  the position in the superblock across checkpoints, and carry it through
  segment deletion and `CLEAR` exactly as this build does. Their revision
  may grow where this build's would not, which D2 allows. Their migration
  and bulk load create new files with new generations. So no format bump
  was needed.

With `generation`, `(generation, revision)` names one committed state along
one line of history. A restore keeps the source's generation and its log
positions, so a copy restored to a point and then written to carries on at
the source's positions: once it is written to, it is a second line, and it
can reach a pair the source (or another copy) also reaches with different
data. What is guaranteed around a restore:

- the restored copy reports the revision of the point restored to, and its
  first change takes it past that point;
- derived data kept inside the database (an extension's keyspace, anything a
  later slice stores beside the graph) is restored with the copy and follows
  the copy's own writes, so inside one database it agrees with that
  database's graph whatever happens to the source;
- a consumer outside the database that keys what it keeps by the pair must
  treat a restored database as a new source, and must not assume a pair it
  has seen on one copy means the same state on another.

Left open: a restored database could take a new generation at its first
change, which would make the pair unique across copies. That changes what
replication does after a restore and is a decision for the owner, not for
this slice.

## Budgets for procedures

Today the statement budget (`max_millis`, `max_steps`) is checked only while
matching patterns. `CALL pagerank()` and `CALL context.retrieve()` run to
their end whatever the caller asked for, and an extension procedure does not
get the spilling memory budget the built-in algorithms get.

After this slice:

- every loop over nodes or edges in an algorithm, a walk or an extension
  procedure counts steps against the statement budget, and stops with the
  same `statement stopped` error a query gets;
- an extension procedure runs with the same memory budget and spill directory
  as a built-in algorithm, without asking;
- an extension has a public way to count a step.

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

## `check`: is the graph whole (*built*)

`glider <db> verify` checks pages and trees. Nothing checked the graph those
trees hold. `CALL check.run()` (`CREATE EXTENSION check`) does, and repairs
nothing (D1): it writes no node, edge or extension record, and the revision
is the same afterwards, whatever it finds. Its user-facing description is
in `docs/EXTENSIONS.md`.

| check | what it finds |
|---|---|
| `endpoints` | an edge whose source or target node does not exist |
| `adjacency` | an edge missing from either endpoint's adjacency or listed there with the wrong far end; an adjacency entry with no edge, or one that is not its edge's (another node, type or direction) |
| `membership` | a label or type membership entry without its record, or a record missing from its membership |
| `counters` | label, type, node, edge and index counts (entries and distinct values) that differ from what a scan counts; node, edge and index id counters behind an existing id |
| `indexes` | a property index entry that does not match the node (gone, without the label or key, under another value), or a node missing from an index it belongs in |
| `derived` | each installed extension's own check of its derived state: anything that differs from what a rebuild would make, including ids that are gone |
| `duplicates` | parallel edges with the same endpoints, type and properties; nodes of `label:` that agree on every property in `keys:` |
| `orphans` | nodes with no edges, optionally of given `labels:` |
| `required` | nodes of `label:` lacking an edge of `type:` in `dir:` |

What is true now, and what this slice decided where the plan was open:

- **Which run.** With no `checks:`, the first six run: `derived` counts as a
  check of the database, since derived data that disagrees with its graph
  is a fault of the database, not a fact about the data. The last three run
  only when named. Checks always run in the order of the table.
- **Rows.** `check, severity, kind, id, detail`. Severity is `fault` for the
  database's checks, `candidate` for the data's, `info` for an installed
  extension that cannot be checked (not available in this build, or no
  `check` method), and `summary` for the row that ends each check (`kind`
  `done` or `partial`, `detail` `{"records":N,"problems":M}`). `id` is a
  handle (`n12`, `e12`) or the name of the label, type, index, counter or
  term at fault. Within a check, rows come in the order its walks meet
  them: trees in key order, so nodes and edges ascending by id (D7).
- **The extension's half.** `Extension::check(g, probe) -> Result<bool>` is
  the new optional trait method (default: `false`, "cannot check"). The
  `context` text index implements it as a comparison with a rebuild that
  does not rebuild: covered nodes in id order (every posting and length a
  rebuild writes is there and equal), lengths (each a covered node's),
  postings term by term (each term's count is its document frequency, each
  posting's node is covered), document frequencies (each has postings), the
  totals, and only when the counts show extra postings on covered nodes, a
  second walk of the postings to name them. A node is named once where it
  can be: a node deleted below the hooks is named by its length entry, not
  by each of its postings.
- **Staleness by comparison, not by revision.** "Derived state older than
  the changes to its sources" is found by comparing it with the graph,
  which finds staleness of any age. Comparing revisions needs derived
  records that carry one (D3), which the text index does not yet; a later
  slice that adds them can make `derived` cheaper, not more right.
- **Duplicates.** Naming `duplicates` always runs its parallel-edge half;
  the node half runs with `label:` and `keys:`. Edge properties are
  compared exactly, by key; node values as an index compares them (`1`
  equals `1.0`). Nodes lacking any of the keys are not compared. A group of
  more than two is reported as each later member against the first.
- **Slices (D6).** `limit:` caps the records a call looks at and 10,000 the
  rows it returns; past either the call ends with a `partial` summary whose
  `id` is a cursor (`check:phase:key:carry`), which `after:` takes back.
  Running counts (members of the label being counted, entries of the index
  being counted, the text index's totals) travel in the cursor, so a call
  holds nothing between slices, and the slices' rows together are exactly
  one whole run's (`slices_together_report_exactly_what_one_whole_run_reports`,
  with limits of 1, 2, 7, 50 and 1000 records). Memory per call is a few
  batches of records; the parallel edges of one node and the members of a
  label for node duplicates go through an external sort in half of `algorithm_memory()`
  (64 KiB at least), spilling past it. The node half of `duplicates` sorts
  again at every slice, so a slice bounds what it reports, not what it
  reads.
- **Budgets (G2).** Every record a check looks at passes through one
  function, `Run::look` in `src/ext/check.rs` (and `Probe::look` for an
  extension's records); each already returns a `Result`, so counting a
  statement step per record is one line in each.
- **How faults were planted in tests** (`tests/check.rs`), since the graph
  API cannot write one: a bulk load from a source that breaks its contract
  (an edge to a node it never lists) for `endpoints`; for `adjacency`,
  `membership`, `counters` and `indexes`, the database file opened with the
  storage layer below the graph (`storage::db::Db`) and one tree entry or
  counter deleted, added or changed there, then reopened (the same file
  loaded into memory gives the same rows); for `derived`,
  `Graph::ext_write` into the `context` keyspace and writes through
  `Graph::apply_op`, which runs no hooks.
- **On the manufacturing graph** (`bench/tests/mfg_check.rs`, and
  `mfg-bench`'s `check` measurement in `bench/KNOWLEDGE.md`): the database
  checks find nothing; `orphans` and parallel edges are exactly the planted
  faults; `duplicates` of `Tool` and `Part` on the properties the truth
  says their duplicates agree on are exactly the planted pairs and no
  look-alike. For `WorkInstruction` on `text, revision, scope` the planted
  pairs are found among thousands of other candidates: the same design
  element's local instruction at every station of the design is the same
  text, which the truth lists as look-alikes. No choice of keys separates
  them; that is why these are candidates and nothing is merged.

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

*Built (G3): the generator, its truth and questions, and the harness's
skeleton with today's baseline. Described in `bench/KNOWLEDGE.md`.*

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

What is true now:

- `mfg-gen --nodes N --seed S [--faults]` writes `graph.jsonl`,
  `truth.jsonl` and `questions.jsonl`, streaming: the generator holds the
  catalogue, the designs and one record per line, under 10 MB at ten million
  nodes. The output is byte-identical for the same arguments (a checksum is
  pinned in a test, and is the same on Rust 1.74 and current stable).
- Node and edge ids are written in order from 1, so the truth and questions
  name ids that an import into an empty database reproduces; `code` and
  `number` properties identify nodes across databases, for `identity:` (D7).
- Decided here: one station in eight deviates from its design, by one extra
  work element or one missing tool; designs are 3 for a few hundred nodes,
  growing with the square root of the station count to 24; planted
  duplicates are not used by work elements (they hang off the catalogue
  exactly as their originals do), so only their properties and their
  neighbours, not their degree, give them away; look-alikes include the
  same design element's instruction at two stations, which is identical text
  under a different number and must never be merged (D1).
- `mfg-bench` records load, `STATS`, `context.retrieve` from anchors and from
  words at three budgets, `pagerank` and `communities`, on both tiers;
  a measurement is added by adding a function to its `MEASURES` list.

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
| G6 | `check` (*built*) | G1 |
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
