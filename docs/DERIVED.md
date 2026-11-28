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
  changes nothing, a checkpoint, a reopen, a restore or loading an image; it
  never goes back across saving and loading one; it may also grow when
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
  as the graph is, which is not at all.
- *Loading an image* (`Graph::from_bytes`, how a host without a
  filesystem reopens a database) gives a graph with no lineage at the
  image's revision: a file's image is its state at its last checkpoint and
  carries that checkpoint's revision; a memory graph's snapshot image
  carries the revision it was taken at; a memory graph without a log
  cannot write an image, and once given one its positions carry on from
  what it counted, so its image carries that. Writes on the copy carry on
  above it. (The revision slice decided that such a graph starts at 0; the
  change journal's review showed why that is wrong: a journal or a kept
  colouring inside the image holds the source's positions, and a copy that
  started again below them answered "nothing changed" for a change.
  `a_database_loaded_from_an_image_carries_on_from_the_images_revision`
  in `tests/revision.rs` failed before and passes now.) A legacy image
  is migrated into a new database by `import_legacy`, which counts from 0
  as any new database does.
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

## Budgets for procedures (*built*, G2)

The statement budget (`QueryLimits::max_millis`, `max_steps`) holds for every
`CALL` as it does for a query, and an extension procedure gets the spilling
memory budget a built-in algorithm gets.

**What a step is.** One node or one edge looked at, whether it turns out to
matter or not. In a query: a candidate checked, an edge scanned, a neighbour
looked at, a combination of pattern parts tried (as before). In a `CALL`: a
node an algorithm or walk takes up, each edge it follows, each node a scan
reads, each per-node array entry a pass goes over, each node a result is
written back to, each result row; in the text index a posting read or
written; in `check.run` a record; in label propagation each label compared.
An extension function evaluated for a row is one step. The same call takes
the same steps on the in-memory and the paged tier (tested for every
algorithm): the algorithms count, the views do not, and building a
projection counts its nodes, a page of 4,096 at a time, on both tiers. The
edges the in-memory build reads are not steps, since the paged view reads
none; the build looks at the meter every 4,096 of them, so a time budget
still bounds it, but a step budget bounds it only by its nodes.

**The meter** is `src/meter.rs` (public as `glider::meter`, re-exported as
`glider::ext::{step, steps, spent, check, stopped}`). It is per thread, set
around a statement by `execute_capped` and `Cursor::open`. `steps(n)` is one
thread-local decrement of a credit of at most 256 steps; when the credit is
used up the meter is looked at, the clock read, and more credit handed out.
Tight loops count into a local `meter::Tally`, which passes steps on 256 at
a time and the rest when the loop is done (`flush`), so no step is dropped
however the work is cut up: 50,000 walks of a few steps each count as one
long walk would. Once the budget is spent the statement is *stopped*, and
stays stopped:
every later count fails, so code that could not return the error where it
noticed (a callback, an extension function, an algorithm with no `Result`)
stops at its next count. The statement checks at its end and fails with the
budget's error whatever the procedure returned. The error is the one a query
gets, word for word (`statement stopped: it took more than its budget of N
matching steps`, `... ran past its time budget of N ms`), so callers that
match on it need no change.

**The bound.** The work between two looks at the meter (and the clock) is a
fixed number of steps whatever a node's degree: at most 256 counted steps,
or 4,096 entries of a scan that is not counted step by step. Concretely:
counting goes through a `Tally` (256 at a time); the paged view (which
reads a hub's list from the tree 4,096 entries at a time), `Csr::targets`,
the in-memory projection build,
the reverse projection, the paged degree, a sort of a neighbour list
(`meter::sort`: runs of 4,096 sorted, then merged, looking between), the
intersection of two hubs' lists in `triangles` and k-core's pass over one
node's neighbours all look at the meter every 4,096 entries; passes over
every node (pagerank's sums, the components scan, the topological sorts',
k-core's, label propagation's and MST's setup, closeness's distances, the
normalisation in betweenness) count 4,096 nodes at a time; the walks, the
context walk, `structure`'s scans and `neighbors` count each edge as they
read it and stop the scan. `Csr::each` is the exception that keeps its
loop as it was (a look at the meter in it cost pagerank a tenth, by keeping
the callback from being inlined): the algorithms' callbacks count each edge,
and once the statement is stopped they pass over the rest of a hub's list
doing nothing, a few nanoseconds an entry. A stop noticed inside a callback
sticks to its tally, so the loop around the callback stops at its next
count. Three things
are not cut: allocating or resetting a per-node array (`StateVec::new` and
`fill`, at memory speed, per source in betweenness and closeness), the
external sort's work inside one push when a 64 MB run fills (MST), and the
merge set up by its `finish`. Measured with 50 ms budgets
(`tests/statement_budget.rs`): on a 200,000-node, 1,000,000-edge graph,
50.0 to 51.8 ms for pagerank and betweenness (hours unbounded), a whole-graph
BFS by the local walk and by the paged projection (0.35 s), `context.retrieve`
(2.5 s) and `context.index` (1.5 s); on one hub with 1.5 million edges, every
procedure that reaches it (BFS on both tiers, `neighbors`, `subgraph`, MST on
both tiers, k-core, triangles, clustering, communities, paged pagerank,
degree on both tiers, betweenness, `context.retrieve`, `structure.colors`
and `structure.classes`) within 300 ms, almost all at 50 to 52 ms (MST on
the paged tier took 1.3 s before the hub was cut up).

**Leaving nothing behind.** A stopped statement rolls back like any failed
one: a stopped `CALL pagerank(write: "rank")` has written no `rank` (tested at
every point it can stop, computing or writing), a stopped `context.index()`
leaves no index, and nothing a write hook wrote for the stopped statement
is left (`tests/budget_blocks.rs`). Inside the caller's transaction, a
stopped statement that had written anything is one more failure the engine
rolls back (`Graph::abandon`, the rule G4 set for failed writes and hooks):
a block opened with `BEGIN` or `Graph::begin()` is spent, so the error says
the transaction was rolled back, later statements and writes are refused,
`COMMIT` says nothing was committed, and the next block starts clean; a
transaction opened by assigning `autocommit` goes on in a new one. Only the
outermost statement does this, and it does it for the statements it runs
too (below). A stopped statement that had written nothing leaves the
transaction open. A
stopped algorithm returns early with partial state that nothing uses; a
stopped projection build returns an empty projection; a stopped local walk
ends as if finished, so no whole-graph fallback starts. The memory and spill
files it held are freed when it returns (`tests/procedure_memory.rs`).

**Memory for extensions.** `ext::call` runs the procedure inside
`ooc::with_budget(g.algorithm_memory(), g.temp_dir(), ...)`, so `StateVec`,
`SpillQueue` and `SpillStack` spill to `<db>-tmp/` past `work_mem` exactly as
they do for `CALL pagerank()`. `with_budget` puts the thread's budget and
spill directory back however the procedure ends, a panic included, as the
meter's guard does for the meter: on a pool of threads shared by many
databases, one database's failure changes nothing for the next call.

**Statements inside statements.** A statement a procedure runs (through
`execute_capped` or `execute`) is bounded by the outer statement's budget as
well as by any of its own: the tighter step count and the earlier deadline
hold. When it returns, the outer meter carries on with the inner steps
counted; an inner statement stopped by the outer budget stops the outer one
too. The transaction belongs to the outermost statement: an inner one never
commits or rolls back on its own, and so a statement is all or nothing
however deep it runs. An inner statement that fails after it has written
anything (an error, its own budget, the outer budget, a panic) records what
failed in a thread-local slot as it returns or unwinds; the outermost
statement reads the slot when the procedure returns and, whatever the
procedure did with the error, fails as a whole ("statement failed as a
whole: a statement it ran, `<its text>` failed after writing: <why>") and is
rolled back under the rule of its kind of transaction: in autocommit nothing
remains, a block is spent, a transaction opened by assigning `autocommit`
goes on in a new one. An inner statement that fails before writing anything
leaves nothing to undo, and the procedure may handle its error. A panic is a
failure of the statement it unwinds through, in every kind of transaction:
if the statement had written, it is rolled back (and a block spent) before
the panic goes on, so a caller that catches it cannot commit part of it
(`tests/statement_atomic.rs`). Before this, on the base too, a procedure
that swallowed an inner failure, or a caller that caught a panic inside a
transaction, could commit half a statement.

**Two databases.** Nesting belongs to the database a statement runs on (the
depth and the failure record are fields of the `Graph`, not of the
thread). A statement a procedure on database A runs on database B is B's
outermost statement: it commits or rolls back on B under B's rules for its
kind of transaction, and its failure is B's alone, reported to the
procedure that ran it. A's statement fails as a whole only when a statement
on A itself, run inside it, failed after writing (a procedure on A running
one on B running one on A again counts: that last one is inside A's
statement). The time and step budget is the caller's patience, not a
property of a database, so A's budget bounds the work A's statement starts
on B too: spent on B, it stops B's statement (rolled back on B) and A's
(rolled back on A). Of what is kept per thread, the meter (one budget for
the whole call chain), the memory budget and spill directory (scoped to the
procedure or algorithm running, put back on return and unwind) and the
statement's text (for naming a failed inner statement) are rightly per
thread; the statement nesting and the inner-failure record had to become
per database, and the hook state, autocommit and blocks already were
(`tests/two_databases.rs`, `tests/cross_database.rs`).

**Transaction control inside a statement.** `BEGIN`, `COMMIT` and `ROLLBACK`
run as a statement by a procedure on the database its statement runs on,
and `Graph::begin`, `commit` and `rollback` called on it, are refused with
an error that says so: they would split the statement, which commits or
rolls back as a whole when it ends. On another database they run as usual.

**What `max_steps` does not bound.** Building the in-memory projection
counts only its nodes, so that both tiers count the same steps: a
`max_steps` budget does not limit how many edges that build reads (a test
read all of 3,000,000 edges under `max_steps: 200`). Where `max_steps` is
the only budget, as on wasm32, the build is bounded only by the graph; a
time budget stops it within 4,096 edges.

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
| 200,000 nodes, 1,200,000 edges | −2.2 to −2.9% | +0.1 to +3.1% | +5.0 to +7.9% |

That race runs a copy of the old code inside the new build. Across builds
(the base engine and this one, the same harness, interleaved, best of 20)
the numbers move more than the change does: the old code's copy, the same
source in both, ran pagerank in 129 ms in both builds but BFS in 43 ms in
one and 23 to 31 ms in the other. Read that way, base → head: pagerank over
a `Csr` 120 → 126 ms (+5%), over the projection a `CALL` uses 224 → 231 ms
(+3%), BFS 43 → 23 to 32 ms, components 28 → 27 to 38 ms.

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

*Built (G4). Shown by `tests/write_hooks.rs` and the adopted review tests
in `tests/review_g4.rs`.*

`on_node_write` keeps its meaning. Beside it:

- `wants_edge_writes` / `on_edge_write(g, id, before, after)`, with
  `EdgeImage { from, to, etype, props }`: after every edge add, delete,
  property set and unset, inside the writing transaction. An extension that
  does not ask is not called, and a database where none asks pays one
  check of a cached list per edge write (measured below). One that asks
  pays for two edge images per write and the transaction wrapper: the
  review measured a hook that does nothing at about 17% slower edge adds
  and deletes (200,000 edges, memory graph).
- Deleting a node, when an extension follows edge writes, deletes its edges
  first one by one through the hooked path (logged as `EdgeDel` records),
  a batch at a time, then the node. Without such an extension the node's
  delete detaches its edges inside one `NodeDel` record, as before. The
  whole delete is one transaction, so it grows linearly with the node's
  degree: about 13 log bytes per edge (2.6 MB for a 200,000-edge node,
  where one `NodeDel` is 20 bytes) plus whatever the hooks write per edge,
  and the copy-on-write pages it touches. What bounds it is the database's
  ordinary limits: a memory graph's `max_memory` and a file's disk.
  Deleting frees about as many pages as it copies, so what grows is the
  log (in memory only for a memory graph with a log) and what the hooks
  write: a follower that writes a few bytes per edge deletes a
  225,000-edge hub inside 24 MiB, one that writes a kilobyte per edge runs
  out, and the test asserts that this ends in `Error::Full`, rolled back,
  with the graph, the revision and the hook's keyspace as they were. A file database cannot checkpoint inside the transaction, so its
  log holds the whole delete until it commits. Detach memory itself stays
  at one batch of 4,096 edge ids.
- `on_bulk_write(g, BulkWrite::Clear)`, for an extension that follows node
  or edge writes, after `CLEAR`. `BulkWrite` is `#[non_exhaustive]`: an
  extension treats a kind it does not know as "everything may have
  changed".
- A hook may write only extension keyspaces. A node, edge, label or
  property write, or a `CLEAR`, from inside any hook (node, edge or bulk)
  is refused with an error that says so, before anything is written; a
  hook that passes it on fails the statement, one that swallows it has
  written nothing to the graph. Hooks therefore never nest, and there is
  nothing to bound (before, a hook that wrote the graph ran the hooks again
  on the stack until the process aborted).
- A failing hook fails the write. With autocommit on, the statement rolls
  back. With autocommit off (the API, or a statement inside `BEGIN`) the
  whole open transaction rolls back and the error says so, whatever its
  kind (an I/O error keeps its kind, with the note added): the engine has
  no savepoints, so undoing only the failed write is not possible. A block
  the engine can see start — the `BEGIN` statement, or `Graph::begin()`,
  added beside `commit()` and `rollback()` as the way to open a
  transaction — is then *spent* (`Graph::spent()`): every further write
  and statement in it is refused with the explanation, and `COMMIT` is an
  error ("nothing was committed: ...") that ends the block, so the writes
  made after the failure can never commit as if they were the whole
  transaction. The same rule covers the engine's own rollback inside such
  a block (`Error::Full` past `max_memory`, a storage error), which before
  let the statements after it commit as a partial unit; no existing test
  or document relied on that. A block opened with `begin()` ends only with
  `commit()` or `rollback()` (or `COMMIT`/`ROLLBACK`), and a spent state
  belongs to that block alone: what happens depends only on what the
  caller did, never on whether some other call ran in between. Inside such
  a block nothing commits before its COMMIT, so "nothing was committed"
  is true whenever it is said: every operation that has to commit on its
  own is refused there with an error saying to end the block first, and
  the block's pending writes stay pending. Found by reading every commit
  path: `import_jsonl`, `import_jsonl_from`, `import_legacy` (and so
  `glider <db> migrate` and `Graph::from_bytes` of a legacy image, which
  use a fresh graph and never meet a block), `COMPACT` / `Graph::compact`,
  and `execute_batch` (already refused). `Graph::checkpoint` inside a
  transaction only flushes the log, which recovery ignores without a
  commit marker, so it commits nothing and is allowed. In a transaction
  opened by assigning `autocommit = false` these operations commit what is
  pending, as they always did. Assigning `autocommit = true` in the middle
  of a `begin()` block does not end the block, but makes the next write
  autocommit, and that commit takes the block's earlier writes with it: a
  caller who opens blocks with `begin()` should not assign the field. A
  transaction opened by assigning the public `autocommit` field cannot be
  seen to start or end, so it keeps the behaviour it had before this
  slice: the failing call's error says the transaction was rolled back,
  and the caller carries on in a new one. The embedding binding opens its
  transactions that way and tracks failure itself; it can move to
  `begin()` later.
- A hook that panics: the hook state, autocommit and the statement's
  transaction are restored on the way out (the C interface catches panics
  and keeps the handle, which then stays writable). The transaction is
  rolled back, the caller's earlier pending writes included. Inside a
  `begin()` block the block is then spent with the reason ("a write hook
  panicked"); in a transaction opened by assigning `autocommit = false`
  the caller learns of the rollback only through the panic itself, and
  its next writes start a new transaction.
- With the revision (D2): a write that changes nothing (the same value set
  again, an unset of an absent property, a delete of what is not there) is
  not logged, does not move the revision and is reported to no hook; every
  write a hook is told about was logged, so it has moved the revision by
  the time its transaction commits. `CLEAR` always counts as a change, on
  an empty graph too (it resets the id counters and wipes derived data, as
  the revision slice decided): it moves the revision and the bulk
  notification is sent. The test checks, for every write path, that the
  hooks heard something exactly when the revision moved. The writes that
  move the revision with nothing to report are an index definition (not a
  node or an edge) and a replayed operation on a record that does not
  exist that logs a newly seen key or label (`apply_op` of `NodeSet`,
  `EdgeSet` or `LabelAdd` on a missing id). No hook is ever called with
  nothing before and nothing after. **The revision moving does not imply
  that any hook was called**: a journal must not count on one entry per
  revision step.
- Recovery and replicas replay what the hooks wrote and never run them
  (shown by counting hook calls during `from_replica` and crash recovery,
  and by comparing the replayed keyspace with one made by running the
  hooks again).

Every path that changes a node or an edge, found by reading every caller of
the private `Graph::exec` and every direct write to the pager's trees:

| path | what it does now |
|---|---|
| statements: `CREATE`, `SET`, `REMOVE`, `DELETE`, `DETACH DELETE`, `CALL ... write:` | the graph API, so per-record hooks |
| the graph API: `add_node`, `add_edge`, `delete_node`, `delete_edge`, `set_/unset_node_prop`, `set_/unset_edge_prop`, `add_label`, `remove_label` | per-record hooks |
| `execute_batch`, `BEGIN ... COMMIT` | statements, so per-record hooks |
| `import_jsonl`, `import_jsonl_from` | `add_node` / `add_edge`, so per-record hooks |
| `Graph::apply_op` (replayed operations with explicit ids) | each op goes through the hooked path for its kind: node ops as the node API, edge ops as the edge API, `NodeDel` as `delete_node`, `Clear` as `clear`; `ExtensionAdd`/`Del` as their API calls; any `ExtWrite` forgets the cached hook list |
| `Graph::import_legacy`, `glider <db> migrate`, `Graph::from_bytes` of a legacy image | `apply_op` per record, so per-record hooks (before, they told nobody) |
| `CLEAR`, `Graph::clear` | one `on_bulk_write(Clear)` |
| `Graph::bulk_load` | builds a database that must not exist, so nothing is installed in it and nothing derived can be left behind; an extension installed after it builds from a scan. The test checks that a bulk-loaded database has no extension installed |
| crash recovery, `from_replica`, replica tailing | replay logged records, hook writes included; no hooks (D5 holds because the hooks ran when the records were first written) |
| `Graph::from_bytes` of a paged image, opening a file | a copy of a state, not a change |
| `rebuild_adjacency` (format upgrade at open) | rebuilds the adjacency tree from the edges; no node or edge changes |

The `context` text index keeps its behaviour on every path it handled. On
`apply_op` and `import_legacy`, where it used to drift silently from the
graph, it now stays right (a test that failed before the change). On
`CLEAR` it was already right: its configuration survives and its postings
are wiped with the data.

The proof: a recording extension keeps, for every reported id, the first
image before and the last image after. A file database is bulk-loaded, then
written to by statements of every kind, the API, a batch, JSON Lines from a
string and from a file, `apply_op` of every op kind (new ids and
overwrites), a legacy import over existing ids, `CLEAR` both ways, and
rolled-back writes (`ROLLBACK`, API rollback, failing statements, failing
hooks with autocommit on and off). After each path, every node and edge that
differs between the graph before and after has been reported, every report
agrees with both states, and nothing is recorded for the rolled-back
writes. The change journal (G5) can rely on exactly this.

Cost on edge writes in a database where no extension asks for them
(`tests/write_hooks_cost.rs`, 400,000 edges added then deleted in a memory
graph, commits every 10,000; medians of seven runs each, alternated seven
times between the code before and after on a busy machine):

| | add, before | add, after | delete, before | delete, after |
|---|---|---|---|---|
| no extension | 199k/s | 198k/s | 277k/s | 265k/s |
| `context` with a text index (node hooks only) | 187k/s | 201k/s | 258k/s | 280k/s |

The run-to-run spread on this machine was about 25% and the differences
point both ways: no cost shows above the noise. In the quietest pair the
difference was under 1%.

## `changes`: which nodes and edges changed (*built*)

*Built (G5). Shown by `tests/changes.rs` and `bench/tests/mfg_changes.rs`;
the user-facing description is in `docs/EXTENSIONS.md`.*

`CREATE EXTENSION changes` starts a journal of *which* nodes and edges
changed and at what revision. It does not keep old values: the graph has the
current ones, and that is what a rebuild needs.

```
CALL changes.since(revision: 1042, limit: 1000)   ids changed after a revision, by kind then id
CALL changes.since(revision: 1042, kinds: ["node"], after: "node:7")
CALL changes.status()                             oldest revision still answerable, entries, bytes, cap
CALL changes.release(below: 1500)                 forget changes at or below a revision
CALL changes.cap(entries: 100000)                 bound the journal
```

What is true now, and what this slice decided where the plan was open:

- **Entries.** For every committed change to a node or an edge, by every
  path G4 lists, the journal holds one entry per (kind, id): `node`,
  `edge` or `adjacency`, its latest change (`put`: it exists with what the
  graph holds; `drop`: it is gone) and the revision of that change. An
  edge write gives the edge's entry and an `adjacency` entry for each
  endpoint before and after (four for a replayed `EdgeAdd` that moves an
  edge). A node property or label change is not an adjacency change. An
  adjacency entry is `put` while its node exists and `drop` once it is
  gone: when a node is deleted or appears, its adjacency entry, if it has
  one, is rewritten with it, so a `put` never names a node that is gone.
- **The revision an entry carries.** A transaction's final revision is not
  known while its hooks run, so an entry carries `Graph::log_end()` as the
  hook saw it: the log position just past the record of the write. Every
  record of a transaction lies after the revision committed before it and
  before its commit marker, so a change committed by the transaction that
  took the database from R1 to R2 carries r with R1 < r < R2. Hence, for a
  caller reading `revision` at commit boundaries, `since(R1)` taken at R2
  lists every id written by a transaction committed after R1, at its latest
  change, and none written at or before R1. Rows come in (kind, id)
  order, kinds node < edge < adjacency, the same on both tiers. (The first
  version listed them oldest change first, through an index by revision;
  that index was dropped for write cost, below.)
- **The format: one record per change.** A write adds one extension record
  with one blind put per (kind, id) it changed, and nothing else: no
  revision index, no count, no state rewritten, no read of the entry it
  replaces. Key `<kind><length><id, big-endian without leading zeros>`
  (so keys sort by kind, then id), value `<change><revision, big-endian
  without leading zeros>`: 16-21 bytes an entry stored with its namespace,
  21 in the log for ids below 2^24 and revisions below 2^40.
  Positions within an open transaction are positions, not revisions any
  caller can have read; a caller that reads `log_end()` mid-transaction
  (as `structure` does) gets the same property for that position.
- **The proof** (`since_lists_exactly_what_changed_between_any_two_commit_boundaries`):
  a seeded random workload of 220 steps through statements of every kind,
  the graph API, `begin()` blocks, `BEGIN ... COMMIT`, `execute_batch`,
  JSON Lines, `apply_op` (new ids, an edge moved to other endpoints, node
  deletes, `Clear`), `import_legacy`, `CLEAR`, index definitions, writes
  that change nothing, rolled-back blocks, failing hooks and releases. At
  every boundary, for every earlier boundary, `since` either answers or
  refuses; an answer contains every node, edge and adjacency (a node's
  incident edges, rendered whole) whose exported state differs, nothing
  that a witness extension did not hear of in between, each id once, in
  order, with `put` exactly for what exists, and the Rust questions and
  the statement in slices of 7 and by kind say the same. The same with a
  cap of 40 entries, and on the file tier with the same revisions and a
  byte-identical journal. Mutating the journal (its revision, an endpoint,
  the adjacency rewrite) fails it.
- **Rolled back.** Entries are extension records in the writing
  transaction, so a rollback, a failing statement, a failing hook and a
  block the engine rolled back itself (`Error::Full` past `max_memory`)
  leave none: the journal's keyspace is byte-identical afterwards.
- **The floor.** `since(R)` answers when R is at or above the journal's
  floor and is otherwise an error naming why and the floor ("...; it
  answers for revision F and later. Rebuild what was derived from a scan of
  the graph, then ask CALL changes.since(revision: F) or later"). The
  floor is raised by: installation (to `log_end()` at install, so a
  `CREATE EXTENSION` in a statement of its own answers for the revision
  before it, and one after other writes in a block does not); `release`
  (to `below:`, clamped to the log's end, before anything is forgotten,
  so forgetting can go in slices with `limit:`); the cap (to the newest
  revision it forgets); and `CLEAR` or any other bulk notification (to `log_end()`
  in its hook: everything before must be rebuilt, and the journal answers
  exactly from there on). `CLEAR` keeps the cap.
- **Positions past the database's.** The journal never answers while
  anything it holds lies past `log_end()`: its state records the position
  it was written at, and `since`, `changed` and `changed_any` refuse
  ("cannot answer: it holds positions past this database's ...") when that
  position, the floor or any entry they read is past it, or when the
  revision asked about is (it belongs to another line of history). The
  next write then starts it again: it records nothing and forgets 64
  entries a write (`SWEEP`), answering nothing meanwhile, and once empty
  takes the floor where it is (`why: reset`). Only that end makes it
  answer again: every writer of the state (a write, `release`, `cap`)
  starts from the same check and keeps a restart under way, so `cap` sets
  the cap and `release` the floor without ending it, and every question is
  refused meanwhile (`no_procedure_ends_a_restart_before_it_is_over`; the
  review's `a_cap_call_does_not_end_a_restart_whose_floor_is_not_past_the_database`
  failed before). `CLEAR`, and `DROP` with `CREATE EXTENSION`, make an
  empty journal at the current position, which is what the end of a
  restart is. An entry past the position, with a state that is not, begins
  a restart when a write or call meets it: a sweep, the adjacency entry a
  node delete or creation reads, `release` and `cap` (which walk the
  journal); every question that reads one is refused. Until one of those
  meets it, `changed` answers for ids whose own entries are not past. That
  is safe as far as positions can tell: positions alone cannot tell two
  histories apart (an entry from another database at or below this one's
  position looks like one of its own), and the guarantee against that
  rests on the engine, where a journal is written only by its own
  database's hooks and an image keeps its positions; such a state arises
  only by copying a journal between databases by hand. A kept colouring whose
  position is past the database's is stale. With images keeping their
  revision (D2) this needs a path that moves positions back, which the
  engine no longer has; it is shown by copying a journal and a colouring
  from a graph further along into one that is behind
  (`a_journal_holding_positions_past_the_databases_refuses_and_starts_again`)
  and by the review's image tests (`tests/review_g5_image.rs`).
- **The cap.** In entries, default 1,000,000, at least 64. `CREATE
  EXTENSION` takes no arguments, so "set when it is installed" means
  `CALL changes.cap(entries:)` in the same block as the install; lowering
  it forgets the oldest entries at once (that call does the whole
  journal, as asked). Otherwise no single write pays more than one slice:
  with no count kept, the state holds a log position, `look_at`. Every
  entry costs at least 15 bytes of its own record (`MIN_ENTRY_LOG`) and
  one write makes at most 5 (`MAX_PER_WRITE`), so after counting at most
  n entries at position p the journal cannot pass three quarters of the
  cap before p + 15 (3/4 cap - n - 5). From the first write at or past it,
  each write takes one step of a *sweep*: it looks at the next 64 entries
  (`SWEEP`) in key order, forgets those at or below the floor, counts the
  rest into the state (with a 16-bucket histogram of their revisions),
  and rewrites the state. A sweep over N entries takes about N/64 writes,
  during which at most 5 entries a write are added, so it ends below the
  cap from three quarters of it (64 is the smallest cap for which this
  holds). At its end, if more than half the cap may be held (the count
  plus 5 a write of the sweep), the floor rises to the histogram bucket
  that leaves half or fewer, and a sweep starts again at once to forget
  what is now below it; otherwise `look_at` is set again. Entries at or
  below the floor linger until a sweep reaches them; nothing answers from
  them. So a write pays at most: 64 entries looked at and deleted, one
  state record (about 230 bytes of log) beside its own, and 64 + its own entries in
  steps; the review's write that used to take 133 ms and 1.7 MB of log at
  the default cap now takes under a millisecond and 276 bytes
  (`tests/review_g5_cap.rs`). A write that fails or is stopped during a
  sweep takes its step back with it; the next write takes the same
  bounded step. `a_small_cap_...` checks the cap after every write.
- **Status.** `oldest, why, entries, bytes, cap, revision`, counted by a
  scan. `bytes` is what the keyspace holds: keys with the namespace, and
  values (17.6 bytes an entry on the manufacturing graph).
- **Recovery and replicas** replay the journal's records and never run
  its hooks: after crash recovery and `from_replica` the keyspace is
  byte-identical to the source's and a witness extension's list of hook
  calls is unchanged.
- **For other extensions** (`glider::ext::changes`): `answers(g, R)`,
  `oldest(g)`, `changed(g, R, kind, ids) -> Option<Vec<bool>>`,
  `changed_any(g, R, kind) -> Option<bool>` (a scan of the kind's
  entries until one is newer than R), `entry(g, kind, id)`, `since(g, R,
  kinds, after, limit)` and `status(g)`. `None` means "cannot answer:
  assume everything changed". `structure` uses `changed_any`: a kept
  colouring is stale exactly when a node or an edge changed since its
  position, when the journal answers for it; otherwise the conservative
  answer of before (any write but its own).
- **Check.** The journal implements `Extension::check`: records that do not
  decode, a `put` for an id that does not exist, an entry or a floor past
  the log's end (`log_end()`, so the open transaction's entries are not
  faults), and, over a walk from the start in one slice, more entries
  than the cap. A `put` at or below the floor is not consulted and not
  checked.
- **Bounds (D6).** `since` returns at most 10,000 rows a call, continued
  with `after:` (the message gives it); `release` and `cap` forget 4,096
  entries a write. Every loop passes through `step(n)` in
  `src/ext/changes.rs`, which counts it against the statement budget (G2,
  built); the walks and the status count also look at the meter every
  4,096 entries. A stopped `cap` or `release` forgets nothing, a write
  stopped while its hook is making room leaves the journal as it was and
  the next write makes room as before (`tests/journal_budget.rs`), and the
  hooks inside a statement a procedure runs, on either database, follow the
  same rules as in an outermost one (`tests/two_databases.rs`). A write reads the
  state (one lookup) and writes one record; deleting or recreating a node
  also looks up its adjacency entry; upkeep is as above. `since` scans
  the entries of the kinds asked for in key order, so a full pass in
  slices reads every entry once.
- **Cost** (`bench/KNOWLEDGE.md`, "Change tracking", the first version and
  this one side by side, 10k to 1M nodes, both tiers). In 1,000-write
  transactions, node writes run at 0.38-0.46 of their speed without the
  journal (first version 0.22-0.32), edge adds at 0.48-0.69 (0.23-0.50),
  edge deletes at 0.25-0.56 (0.15-0.33); the log grows 2.5x a node
  write's bytes (7.5x before), 3.5x an edge add's (11x), 6x an edge
  delete's (26x). One write a transaction is in the same range. Node
  writes and edge delete bytes miss the targets of 0.6 and 3x. A hook that
  does nothing already leaves node writes at about 0.70 (G4's images and
  unit), one entry is one extension-tree insert about as dear as the node
  write, and an edge delete logs 13 bytes against an entry's minimum of 15.
  Writing once per transaction would not remove either of the first two, so
  the commit path is unchanged. Reads: `since` for a recent revision now
  scans the journal (8-11 ms at 300,000-400,000 entries, was 0.01 ms), a
  full pass costs what it did, `changed` for 1,000 ids 0.3 ms.
  A database without the journal pays nothing.
- Left out, as planned: old values, named consumers with cursors (a
  consumer keeps its own revision), and vectors (G10).

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
- **Budgets (G2, built).** Every record a check looks at passes through one
  function, `Run::look` in `src/ext/check.rs` (and `Probe::look` for an
  extension's records), and each counts one statement step: `check.run`
  stops on the statement's time and step budget like any procedure
  (`every_algorithm_stops_on_a_step_budget` runs it too).
- **How faults were planted in tests** (`tests/check.rs`), since the graph
  API cannot write one: a bulk load from a source that breaks its contract
  (an edge to a node it never lists) for `endpoints`; for `adjacency`,
  `membership`, `counters` and `indexes`, the database file opened with the
  storage layer below the graph (`storage::db::Db`) and one tree entry or
  counter deleted, added or changed there, then reopened (the same file
  loaded into memory gives the same rows); for `derived`,
  `Graph::ext_write` into the `context` keyspace, and writes made as a
  path below the hooks would make them: since G4 every path runs the hooks
  (D5), so the test makes a write through `Graph::apply_op` and then puts
  the `context` keyspace back as it was before it.
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

## `structure`: sameness and repetition (colours and classes *built*)

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

What is true now (G7: colours and classes; templates, path patterns,
quotient, plan and expand are G8 and G9). The user-facing description is
in `docs/EXTENSIONS.md`.

- **Procedures.** `structure.colors`, `structure.classes`,
  `structure.read`, `structure.stored`, `structure.drop`
  (`CREATE EXTENSION structure`).
- **What a colour is.** Depth 0 hashes a node's label names (sorted) and,
  for each key of `props:` (sorted by name), the key and its value in a
  canonical encoding that equates what the engine equates (`1` and `1.0`)
  or a mark for absent. Depth k+1 hashes the node's depth-k colour, its
  number of followed edges, and the sum modulo 2^128 of one hash per edge
  of (direction, the type's name, the depth-k colour at the other end). A
  sum is a multiset hash: no sort and no memory per node, whatever its
  degree. `dir:` is `out` by default; `both` tags the two directions
  apart. Names and values, never ids, enter, so colours are the same for
  the same data under other ids (D7) and are comparable across databases,
  which G12 (`embed.wl`) can build on.
- **The hash and its odds.** SipHash-2-4 with 128-bit output under a fixed
  key (checked against the reference vectors). Equal surroundings always
  have equal colours. For a billion nodes, the chance that two different
  surroundings share a colour at one depth is under n^2/2^129, 1.5e-21;
  the sum weakens this only for a neighbour repeated about 2^40 times and
  more, where it is still under 1e-9. Classes by neighbours do not rely on
  the odds: hash-equal candidates are compared in full.
- **Depth.** A number (default 3), or `"stable"`: refine until a round
  leaves the number of colours over the whole graph unchanged (refinement
  only splits, so that is the fixed point), at most `max_depth:` rounds,
  32 by default; the message says at which depth it stopped or that the cap
  was reached, and the colours returned are those of the stopping depth,
  the same as asking for that depth.
- **Which nodes.** Colours are computed for every node; `labels:` chooses
  the nodes reported, counted as "selected" and kept. A node's
  surroundings include nodes of every label.
- **Classes.** `by: "neighbours"`: labels, `keys:` values (absent is a
  value) and the multiset of (direction, type, neighbour id), exact. `by:
  "colors"`: a kept colouring or one computed in the call. Largest first;
  ties to the smallest first member (by `identity:` value, then id) for
  neighbours and to the smaller colour for colours, both content-only when
  `identity:` is given, so the rows are the same in two databases holding
  the same data, ids aside. Singletons are not listed (`min_size:` 2)
  but are counted; `summary: true` gives the size distribution.
- **Kept colourings (D3, D4).** Keyspace `structure`: `n<name>\0<id>` ->
  16-byte colour, `m<name>` -> the log position it was computed at, depth,
  depth where it was stable, nodes, colours, the code version
  (`structure-colors/1`) and the arguments as JSON; `w` -> the stretch of
  log that holds only this extension's writes, from its start to the end
  of its last record. Fresh means: that stretch starts at or before the
  colouring's position, and the log ends where the stretch does (inside the
  open transaction) or one commit marker later. Each call that writes ends
  with one write holding the metadata and the stretch; its record's size
  does not depend on the stretch's value, so it can say where it itself
  ends (`Graph::log_end`, a read accessor added here: the log position
  counting the open transaction's records). So keeping or dropping one
  colouring leaves the others fresh, and any other change makes all of them
  stale. A rolled-back keep leaves nothing. A colouring kept inside an open
  transaction is fresh until something else is written in it.
- **Decided here.** Staleness is by position, not by comparison: a
  change that a colouring does not depend on still makes it stale. Since
  G5, when the change journal is installed and answers for the
  colouring's position, only a change to a node or an edge makes it stale
  (an index, another extension's writes, a write that changes nothing do
  not); otherwise the rule above applies. Being
  stale is not a fault for `check.run`'s `derived` check (kept colourings
  are not maintained, and say when they are stale); only records that do not
  decode are. `write:` sets the colour as a text property and refuses a key
  of `props:`. `check`'s `duplicates` is unchanged; classes by neighbours
  are where neighbours tell duplicates apart.
- **Bounds (D6).** Node ids, two colour arrays and the selection (41 bytes
  a node) are `StateVec`s within half of `algorithm_memory()`, spilling
  past it; colours are counted and classes formed by external sorts in a
  quarter of it, runs of a class in spilling queues. At most 10,000 rows a
  call, continued with `after:`. A colouring computed in the call is a
  whole-graph computation, so its slices bound what is returned; a kept
  colouring is read in true slices.
- **Budgets (G2, built).** Every loop over nodes or edges calls one
  function, `step(n)` in `src/ext/structure.rs` (per node and per node's
  edges in a round, per node counted, kept, written or classed), which
  counts it against the statement budget; the scans of one node's edges
  count each edge as they go, so a hub's stops with the statement.
- **On the manufacturing graph** (`bench/KNOWLEDGE.md`, "Structure";
  `bench/tests/mfg_structure.rs`): with the hierarchy and catalogue edge
  types outwards and no properties, depth 4 recovers every design and sets
  apart every recorded deviation at 10k, 100k and 1M nodes on both tiers,
  and depths 1 to 3 do not; classes by neighbours of `Tool`, `Part` and
  `WorkInstruction` are exactly the planted duplicates, none of the
  look-alikes, at every size. A colouring of the 1M-node graph at depth 4
  takes 3.5-4 s and adds about 50 MB.

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
| G5 | `changes` (*built*) | G1, G4 |
| G6 | `check` (*built*) | G1 |
| G7 | `structure`: colours, classes (*built*) | G2 |
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
