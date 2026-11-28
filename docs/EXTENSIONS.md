# Extensions

glider databases opt into extra capability per database, the way a
PostgreSQL database opts into `timescaledb` or `vector`:

```
SHOW EXTENSIONS                          what this build has, what this database installed
CREATE EXTENSION context                 install (CREATE EXTENSION IF NOT EXISTS context)
CALL context.help()                      the extension's procedures and functions
ALTER EXTENSION context UPDATE           record the running code's version, running its upgrade
DROP EXTENSION context                   uninstall, deleting everything it stored (IF EXISTS too)
```

Procedures are called as `CALL <extension>.<procedure>(name: value, ...)`;
functions work in any expression as `<extension>.<function>(...)`.

## The model

- **Code is compiled in; installation is per database.** glider has no
  dependencies and no unsafe code, so there is no `dlopen`. Every extension
  in the binary is *available*; a database that never runs `CREATE EXTENSION`
  never exposes or pays for it. Calling a procedure of an extension that is
  not installed is an error that says how to install it, and a statement
  using an unknown extension function fails before it runs rather than
  returning a column of nulls.
- **Installation is data.** `CREATE EXTENSION` writes a catalog entry through
  the write-ahead log, in the statement's transaction: it rolls back with
  `ROLLBACK`, is replayed by crash recovery and ships to replicas. `CLEAR`
  keeps it, as it keeps index definitions.
- **Extensions get a keyspace of their own.** A B+tree beside the graph's
  (`ext` in `src/graph.rs`), written with `Graph::ext_write`, read with
  `ext_get` and `ext_scan`. Writes are logged records like any other, so
  whatever an extension stores is transactional, recovered and replicated.
  Keys starting with `!` are configuration and survive `CLEAR`; everything
  else counts as derived from the data and is wiped with it. `DROP EXTENSION`
  deletes the whole keyspace.
- **Extensions can follow writes.** No change to a node or an edge goes
  unnoticed by an extension that asks:
  - `wants_writes` → `on_node_write(id, before, after)` after every node
    write (labels and properties before and after; `None` for absent);
  - `wants_edge_writes` → `on_edge_write(id, before, after)` after every
    edge add, delete, property set and unset (endpoints, type and
    properties before and after). Deleting a node reports each of its edges
    first, then the node;
  - either → `on_bulk_write(BulkWrite::Clear)` after every `CLEAR`, an
    empty graph's too (it resets the id counters, so it always counts as a
    change). Keys not starting with `!` are already wiped.

  Every path calls them: statements (including `CALL` procedures that
  write, `BEGIN` blocks and `execute_batch`), the graph API, JSON Lines
  imports, `Graph::apply_op` (replayed operations with explicit ids),
  `Graph::import_legacy` and `migrate`. They run inside the writing
  transaction. What a hook writes is logged, so crash recovery and replicas
  replay the result without running the hook again.

  A hook may write only extension keyspaces (`Graph::ext_write`). A node,
  edge, label or property write, or a `CLEAR`, attempted from inside any
  hook is refused with an error saying so, which fails the statement like
  any hook failure; so a hook never sets off hooks again.

  A hook that returns an error fails the write: with autocommit on the
  statement rolls back; with autocommit off the whole open transaction
  rolls back, and the error ends with "(the transaction was rolled back)"
  whatever its kind. Open a transaction with `BEGIN` or `Graph::begin()`
  and end it with `COMMIT`/`ROLLBACK` or `commit()`/`rollback()`: such a
  block is then spent (`Graph::spent()`): every further write and
  statement in it is refused with that explanation, and `COMMIT` answers
  "nothing was committed: ..." and ends the block, so nothing run after the
  failure can commit as if it were the whole transaction. The same holds
  when the engine itself rolls back an open block (a memory graph past
  `max_memory`, a storage error). A spent state never reaches the next
  block. A transaction opened by assigning `autocommit = false` keeps the
  older behaviour: the failing call's error says the transaction was
  rolled back, and the caller carries on in a new one. Inside a `begin()`
  block, operations that commit on their own (`import_jsonl`,
  `import_jsonl_from`, `import_legacy`, `COMPACT`, `execute_batch`) are
  refused until the block ends; in a field-opened transaction they commit
  what is pending, as before. Do not assign `autocommit` inside a
  `begin()` block: it does not end the block, but the next write then
  autocommits, taking the block's earlier writes with it.

  A hook that panics leaves the handle usable: the open transaction is
  rolled back (the caller's earlier pending writes too) and autocommit and
  the hook state restored before the panic goes on, so an embedder that
  catches panics can keep the handle. Inside a `begin()` block the block
  is spent with the reason; in a field-opened transaction the panic is the
  only report.

  An extension that does not ask is never called, and costs nothing. A
  write that changes nothing (a property set to the value it has, an unset
  of an absent property, a label it already has, a delete of what is not
  there) is not logged, does not move the revision and calls no hook. A
  hook is never called with nothing before and nothing after. The revision
  moving does not mean a hook was called: a replayed operation on a record
  that does not exist may still log a newly seen name.

  `Graph::bulk_load` only builds a new database, so no extension is
  installed in what it writes; one installed afterwards builds from a scan
  (`CALL context.index()`).
- **Extensions can tell whether the graph has moved.** `Graph::revision()`
  (and `revision` in `STATS`) grows with every committed change, including
  an extension's own writes, and with nothing else. Record it beside derived
  state; if it is the same later, nothing has changed since
  ([`DERIVED.md`](DERIVED.md), "Revision").

### Writing one

Implement `glider::ext::Extension` and register it once per process:

```rust
use std::sync::Arc;
use glider::ext::{self, Arg, Args, Extension, Spec};
use glider::{Graph, QueryResult, Result, Value};

struct Geo;

impl Extension for Geo {
    fn name(&self) -> &str { "geo" }
    fn version(&self) -> &str { "1.0" }
    fn summary(&self) -> &str { "distances between nodes with lat/lon" }
    fn procedures(&self) -> &[Spec] {
        &[Spec { name: "near", args: "(from, km)", doc: "nodes within km of a node" }]
    }
    fn functions(&self) -> &[Spec] {
        &[Spec { name: "km", args: "(a, b)", doc: "great-circle distance" }]
    }
    fn call(&self, g: &mut Graph, procedure: &str, args: &Args) -> Result<QueryResult> {
        // runs inside the statement's transaction
        todo!()
    }
    fn eval(&self, g: &Graph, function: &str, args: &[Arg]) -> Value {
        // Arg::Node(id) / Arg::Edge(id) for bound variables, Arg::Value otherwise
        todo!()
    }
}

ext::register(Arc::new(Geo));
```

Optional hooks: `install` / `uninstall` / `upgrade(from)` run in the
transaction of `CREATE` / `DROP` / `ALTER EXTENSION`; `wants_writes` and
`on_node_write`, `wants_edge_writes` and `on_edge_write`, and
`on_bulk_write` maintain derived state; `check(g, probe)` compares that
state with the graph for `CALL check.run()` (below). The built-in `context`
extension (`src/ext/context.rs`, `src/ext/text.rs`) uses all of them and is
the worked example.

## `context`: graph retrieval for language models

A model asked about a knowledge graph needs a small, relevant, faithful slice
of it in as few tokens as possible. Query results as JSON spend most of a
budget on punctuation and repeated keys, and "the neighbours, then their
neighbours" fills it with whatever hangs off a hub. `context` does that work
in the database.

```
CREATE EXTENSION context;
CALL context.index();                                    -- optional: full-text index for search
CALL context.schema();                                   -- a schema card for the system prompt
CALL context.retrieve(query: "Ada Lovelace", budget: 1500);
```

```
# context: 4 nodes, 4 edges; seeds n1
n1:Person name="Ada Lovelace" born=1815 bio="English mathematician and writer, chiefly known for…"
n2:Person name="Charles Babbage" born=1791
n3:Machine name="Analytical Engine" status="design" embedding=<vector 768>
n5:Country name="United Kingdom"
n1 -KNEW{since=1833}-> n2
n1 -WROTE_ABOUT{year=1843}-> n3
n2 -DESIGNED-> n3
n1 -CITIZEN_OF-> n5
# more: n2 +1 DESIGNED +1 CITIZEN_OF
```

`nN` is the node id. It is stable, and every procedure accepts it back
(`seeds: ["n2"]`), so a model can cite what it was shown and ask to expand it.

### Procedures

| | |
|---|---|
| `retrieve(query \| seeds, budget, …)` | A connected, relevance-ranked subgraph that fits `budget` tokens (default 1000). |
| `search(query, label, keys, top, scan)` | BM25 over node text, for finding seeds. Uses the text index when there is one, a streaming scan otherwise (`scan:` bounds it). |
| `index(labels, keys)` / `unindex()` | Build or drop the full-text index (all labels and text keys by default). Writes keep it current. |
| `paths(from, to, k, depth, types, dir, budget)` | Up to `k` shortest paths, specific ones first, with the nodes on them. |
| `schema(sample)` | Labels with property types, enum-like values (`status:text in {"open","closed"}`), optional keys (`?`), vectors, relationship patterns with counts, indexes. |
| `help()` | Every procedure and function with its arguments. |

Functions: `context.card(n [, max_chars])` (one node, or edge, as a line)
and `context.tokens(x)` (the token estimate).

### How `retrieve` chooses

1. **Seeds**: `seeds:` (ids or `"nN"`), and/or the `top:` (default 3) best
   `search` hits for `query:`, weighted by score.
2. **Relevance**: personalised PageRank from the seeds, computed by local
   push. Mass moves from a node to its neighbours only while it exceeds a
   tolerance tied to the budget, so the work is bounded by what the budget
   can show, not by the graph's size. State is kept only for nodes that
   receive mass, and the walk stops at a memory cap derived from the
   database's algorithm memory.
3. **Hubs**: a node with more than `max_fanout` (500) neighbours, or `depth`
   (3) hops out, absorbs the mass that reaches it instead of passing it on.
   Scores are divided by degree (`normalize: 1.0`; 0 turns it off), so a
   node reached because it is adjacent to everything ranks below one the
   seeds reach by several routes.
4. **Selection**: a connected subgraph grows from the seeds, highest-scoring
   frontier node first, each node joining with the edge that reached it plus
   every edge to nodes already chosen, until the budget is spent. Every
   shown node is therefore explained by shown edges back to a seed.
5. **What was left out** goes last (`# more: n2 +12 KNEW`), with room kept
   for it, so the model knows the picture is partial and can ask for more
   instead of concluding something is absent.

Options: `types:` and `dir:` restrict the walk, `labels:` the nodes; `props:`
/ `exclude:` choose properties (keys starting with `_` are never shown);
`max_chars:` (160) cuts long text at a word with `…`; lists of eight or
more numbers are shown as `<vector N>`; `seen: [...]` lists nodes already
in the model's context, which are linked to but not described again;
`format: "json"` (budgeted as JSON) or `"rows"` (one row per node with its
score and hop count).

### Using it from an agent

Give the model the schema card once and three tools:

| tool | statement |
|---|---|
| `graph_context(question, budget)` | `CALL context.retrieve(query: $question, budget: $budget, seen: $seen)` |
| `expand(node, budget)` | `CALL context.retrieve(seeds: [$node], budget: $budget, seen: $seen)` |
| `how_related(a, b)` | `CALL context.paths(from: $a, to: $b)` |

Keep `seen` as the list of `nN` the conversation has already shown, so
follow-ups spend their budget on new facts. Everything goes through the
ordinary query path: `POST /api/query` over HTTP, the C ABI, wasm and the
Elixir binding all work unchanged.

### Measured

`bench/CONTEXT.md`. On a graph with planted communities and only one edge
type, at equal token budgets, retrieve's precision is 0.77 / 0.87 at 500 /
1,500 tokens against 0.63 / 0.62 for breadth-first expansion, and at 4,000
tokens it finds 99.9% of the seed's community against 69%; a Cypher 1-hop
answer as JSON costs ~10× the tokens for the same relevant nodes. On a
1.2M-node, 8M-edge database with a 256 MiB cache, `retrieve` takes 11–16 ms
warm and 41–73 ms cold, an indexed search 18 ms, and the process stays
under 300 MB.

### Limits

- The token estimate is a heuristic (one token per four characters of a
  word, one per symbol), not a tokenizer. It errs high on symbols.
- The text index is lexical (lowercased words, plural `s` dropped, no other
  stemming, a short English stopword list). Semantic search over embeddings
  is not part of it; pass the ids from your own vector search as `seeds:`.
- The text index follows node writes only; edge properties are not indexed.
- A database made by `Graph::bulk_load` has nothing installed: install the
  extension and run `CALL context.index()` after it. `migrate`, imports
  and replayed operations keep the index current like any other write.
- `CALL context.index()` builds in one transaction, and its log holds every
  posting until the next checkpoint (about 0.5 GiB for the 8.6M postings in
  the benchmark).

## `changes`: which nodes and edges changed

Things derived from a graph and kept (a text index, a colouring, vectors an
application paid a model to make) need to know what to recompute after the
graph changes. The revision only says that *something* did. `changes`
says exactly which nodes and edges did, since a revision.

```
CREATE EXTENSION changes
CALL changes.since(revision: 1042)                         ids changed after it, by kind then id
CALL changes.since(revision: 1042, kinds: ["node"], limit: 1000, after: "node:7")
CALL changes.status()                                      oldest, why, entries, bytes, cap, revision
CALL changes.release(below: 1500)                          forget changes at or below a revision
CALL changes.cap(entries: 100000)                          the most entries it holds (default 1,000,000)
```

Rows of `since` are `revision, kind, id, change`: `kind` is `node`, `edge`
or `adjacency` (the edges at a node: one added, removed or altered at either
end), `change` is what happened to it last, `put` (it exists, with what the
graph holds now) or `drop` (it is gone). An id appears once per kind, with
its latest change; old values are not kept. Rows come by kind (node,
edge, adjacency), then id; at most 10,000 a call (`limit:` lowers it), and the
message gives the `after:` to continue with.

**How to use it.** Read `revision` (from `STATS` or `Graph::revision()`)
when you derive something, and keep it beside what you derived. Later, ask
`since` for that revision: what it lists is exactly what was written by the
transactions committed since, by any path (statements, the API, imports,
replayed operations), and nothing else. Recompute those ids, and keep the
revision you did it at. Rolled-back work never appears.

**When it cannot answer.** The journal is bounded: the cap forgets the
oldest entries, `release` forgets entries at or below a revision, and it
knows nothing from before it was installed or before a `CLEAR`. Asking for
an older revision is an error that says why and names the oldest revision
it answers for: rebuild from a scan of the graph, then carry on from that
revision. It never returns a partial list as if it were whole. To set the
cap from the start, run `CALL changes.cap(entries: N)` in the same
`BEGIN ... COMMIT` as `CREATE EXTENSION changes`.

**From another extension** (`glider::ext::changes`): `changed(g, since,
Kind::Node, &ids)` says which of the ids changed, `changed_any(g, since,
kind)` whether anything of that kind did, `answers(g, since)` whether the
journal can say; each returns `None` when it cannot, which means "assume
everything changed". `structure` uses it: with the journal installed, a
kept colouring is stale only when a node or an edge changed since it was
computed.

**What it costs**: one extension record a write (an edge write updates the
edge and its two endpoints), 16-21 bytes an entry in the keyspace, and the
write throughput measured in `bench/KNOWLEDGE.md`, "Change tracking".
`check.run` checks it (records that do not decode, a `put` for an id that
does not exist, an entry past the log's end). Recovery and replicas hold
the same journal as the source.

### Limits

- A revision is a position in the log, not a counter: entries of one
  transaction carry different positions between the revision before it and
  the one it commits at. Ask with revisions read at commit boundaries.
- A replayed `CLEAR` or any bulk write ends what it can answer for: ids
  are not listed for it.
- `since` reads every entry of the kinds asked for, whatever the
  revision: about 50 ms for 300,000 entries. `changed` for given ids is
  one lookup an id. `changed_any` (and so a kept colouring's freshness)
  reads every entry of the kind when nothing changed: a few milliseconds
  for a few hundred thousand entries.
- Past three quarters of the cap, each write also looks at 64 entries of
  the journal and rewrites its state, until the sweep ends; that is the
  cap's whole cost, spread over writes.
- The journal refuses to answer while it holds a position past the
  database's (a revision from another line of history, or a copy of a
  later state), and starts again with the next writes.

## `check`: is the graph whole

`glider <db> verify` proves that pages and trees are sound. `check` proves
that the graph those trees hold agrees with itself, and that what
extensions derived from it still matches it. It reports and never repairs:
no node, edge or extension record is written, and the revision is the same
afterwards.

```
CREATE EXTENSION check
CALL check.run()                                            the database's own checks
CALL check.run(checks: ["orphans"], labels: ["Tool", "Part"])
CALL check.run(checks: ["duplicates"], label: "Tool", keys: ["kind", "maker", "model", "spec"])
CALL check.run(checks: ["required"], label: "Station", type: "HAS_SIDE", dir: "out")
CALL check.run(limit: 100000)                               a slice; continue with after:
```

| check | finds | severity |
|---|---|---|
| `endpoints` | an edge whose source or target node does not exist | fault |
| `adjacency` | an edge missing from an endpoint's adjacency, an entry naming the wrong far end, an entry with no edge or another edge's | fault |
| `membership` | a node or edge not listed under its label or type, a label or type entry without its record | fault |
| `counters` | a label, type, node, edge or index count that differs from a scan; a node, edge or index id counter that would hand out an id in use | fault |
| `indexes` | an index entry that does not match its node (gone, relabelled, another value), a covered node missing from an index | fault |
| `derived` | each installed extension's own check of its derived data (`context`: its text index against a rebuild) | fault |
| `duplicates` | parallel edges with the same endpoints, type and properties; with `label:` and `keys:`, nodes of the label equal on every key | candidate |
| `orphans` | nodes with no edges, of `labels:` when given | candidate |
| `required` | nodes of `label:` with no edge of `type:` in `dir:` (`out`, `in`, `both`; default `out`) | candidate |

With no `checks:` the first six run; the last three run only when named,
because what they find are facts about the data that an application may or
may not call faults. Candidates are for a person or a rule to decide on:
two tools that agree on kind, maker, model and specification may be one
tool entered twice or two of the same tool.

Rows are `check, severity, kind, id, detail`. `id` is `n12` for a node,
`e12` for an edge (the handles `context` uses and accepts back), or names
the label (`:Tool`), type, index (`:Tool(code)`), counter or term at fault;
`detail` says what is wrong in a sentence. An extension's findings have its
name in `kind` (`context.stale_node`). An extension that does not implement
`check` gets one `info` row saying so. Each check ends with a summary row:
`severity: "summary"`, `kind: "done"` (or `"partial"`), `detail`
`{"records":N,"problems":M}`. Checks run in the order of the table, each
walking its trees in key order, so the same database gives the same rows
every time and on both tiers.

**Slices.** `limit:` caps the records one call looks at; a call also stops
once it holds 10,000 rows. A call that stops ends with a `partial` summary
whose `id` is a cursor: call again with `after:` set to it and the same
other arguments, and the next call carries on from the next record. Slices
together report exactly the rows of one whole run, and their counts add up
to it. A call holds a few batches of records, plus an external sort within
`algorithm_memory()` for the parallel edges of a hub and for node
duplicates. A cursor is a position, not a snapshot: a caller who writes
between slices gets an answer about a mix of states.

**For extensions.** `Extension::check(&self, g, probe) -> Result<bool>`
compares what the extension keeps with what a rebuild would make: call
`probe.look()?` before each record (false: the slice is full; call
`probe.stop_at(cursor)` and return), `probe.found(kind, id, detail)` for
each difference, and resume from `probe.after()`. Return `false` when there
is nothing the extension knows how to check. The `context` text index
checks, without rebuilding: every covered node's postings and length are
there, every length belongs to a covered node, each term's postings match
its document frequency and belong to covered nodes, the totals match, and
only when the counts show postings a rebuild would not make for nodes it
does cover, walks the postings again to name those nodes.

### Limits

- The node half of `duplicates` sorts the label's members by their key
  values again at every slice: a slice bounds the rows and records it
  reports, not the reading. Nodes missing any key are not compared; values
  compare as the index compares them (`1` equals `1.0`).
- It checks what the caller names. Constraints declared in a schema
  (uniqueness, required properties) are not part of it.
- Staleness of derived data is found by comparison with the graph, not by
  revisions: the text index records none yet (D3 is a later slice).
- Measured on the manufacturing graph: `bench/KNOWLEDGE.md`, "Integrity".

## `structure`: which nodes are the same shape

Graphs made by people repeat themselves: forty stations built to one
design, each with the same sides, locations, work elements and tools.
`structure` says which nodes have the same surroundings. It reports and
never merges: two stations of one design are two stations, and nothing
here adds, deletes, connects or relabels a node.

```
CREATE EXTENSION structure
CALL structure.colors(depth: 4, types: ["HAS_SIDE", "HAS_LOCATION", "HAS_ELEMENT", "USES_TOOL"], dir: "out")
CALL structure.colors(depth: "stable", labels: ["Station"], summary: true)
CALL structure.colors(depth: 4, labels: ["Station"], store: "designs")
CALL structure.read(name: "designs", fresh: "require", after: 0, limit: 1000)
CALL structure.classes(by: "neighbours", labels: ["Tool"], keys: ["kind", "maker", "model", "spec"], types: ["SUPPLIED_BY"])
CALL structure.classes(by: "colors", colors: "designs")
CALL structure.stored()
CALL structure.drop(name: "designs")
```

**Colours.** `structure.colors` gives every node (of `labels:`, when given)
a colour, Weisfeiler-Lehman style: two nodes have the same colour at
`depth: d` exactly when their surroundings out to `d` hops cannot be told
apart by their labels, the values of the properties named in `props:`
(none by default, so names and codes do not make every node unique), and
the types, directions and number of their edges of `types:` (every type
by default) in `dir:` (`out` by default, `in`, or `both`, which counts the
two directions apart). `depth: 0` is labels and properties alone; the
default is 3. `depth: "stable"` refines until the partition stops
changing, at most `max_depth:` rounds (32 by default), and says where it
stopped or that it hit the cap. Rows are `id, color` (and `identity` with
`identity: "<key>"`), in id order; `summary: true` gives instead one row
per depth: `depth, colors` (over every node) `, selected` (over the nodes
asked about).

A colour depends on content only: labels, types and property keys count by
name and values by content (`1` equals `1.0`), never by id, so the same data
loaded under other ids gets the same colours, matched through `identity:`.
Colours are 128-bit hashes (SipHash-2-4 under a fixed key) written as 32 hex
digits. Equal surroundings always have equal colours. Different ones share
a colour only by a hash collision: under 1.5e-21 for any two of a billion
nodes at one depth (below 1e-9 if some node has a single neighbour repeated
about 2^40 times).

**Classes.** `structure.classes` groups interchangeable nodes, largest
class first. `by: "neighbours"` (the default) groups nodes with the same
labels, the same values on `keys:` (an absent key counts as a value) and
exactly the same neighbours: the same multiset of (direction, edge type,
node) over `types:` in `dir:`. It is exact: candidates found by hashing are
compared in full. `by: "colors"` groups by colour, from a kept colouring
(`colors: "<name>"`) or one computed in the call from the colour arguments.
Rows are `class, size, first, members` (and `identities` with
`identity:`), members by id or by identity value, at most `members:` (1,000)
listed per class; ties between classes of one size go to the smallest first
member (`neighbours`) or the smaller colour (`colors`). Only classes of
`min_size:` (2) or more are listed; `summary: true` gives the size
distribution instead (`size, classes`). The message says it plainly: the
members are distinct nodes that the structure cannot tell apart, not one
thing.

What `keys:` and `types:` to give decides what "the same" means. On the
manufacturing benchmark a planted duplicate tool hangs off the same supplier
as its original but no work element uses it, so the edges that come in from
users are left out (`types: ["SUPPLIED_BY"], dir: "out"`); a work
element's own instruction is told apart from the same text at another
station of the design by the element it belongs to
(`types: ["APPLIES_TO", "ISSUED_BY", "HAS_INSTRUCTION"], dir: "both"`).

**Kept colourings.** `store: "<name>"` keeps a colouring in the extension's
keyspace with the revision it was computed at, its arguments and the
version of the code that computed it (`CODE`, `structure-colors/1`).
`structure.read(name:)` reads it back in slices and says whether the
database has moved since; with `fresh: "require"` a moved database is an
error (`classes(by: "colors", colors:)` takes `fresh:` too).
`structure.stored()` lists them (`name, revision, fresh, depth, stable,
nodes, colors, code, args`), `structure.drop(name:)` forgets one. Only the
extension's own writes do not count as a move, so keeping or dropping one
colouring leaves the others fresh; a write to any node, edge, index or other
extension makes them all stale, whether or not it touched what they
describe; with `changes` installed (and able to answer for the
colouring's revision), only a change to a node or an edge does. `write: "<key>"` also sets the colour as a text property on each
node (refused for a key in `props:`, which would change what it records).
Without `store:` or `write:` nothing is written and the revision does not
move. A kept colouring is derived data: `CLEAR` and `DROP EXTENSION` remove
it, and `check.run` reports only records of it that do not decode (going
stale is expected, and reported by `read`).

**Slices and memory.** At most 10,000 rows a call (`limit:` lowers it);
continue with `after:` (the last id for colours, the last class number for
classes), as the message says. Colours are a whole-graph computation, so a
slice of a colouring computed in the call bounds what it returns, not the
work; read a kept colouring to slice the work too. Per-node state (41 bytes
a node) is held within half of `algorithm_memory()` and spills to temporary
files past it; distinct colours are counted and classes formed by external
sorts in a quarter of it. The same database and arguments give the same
rows, byte for byte, in memory and from a file.

### Limits

- Colours are computed for every node, also when `labels:` asks about a
  few; `labels:` chooses what is reported and kept.
- Staleness is by revision, not by comparison: any change makes a kept
  colouring stale, including one it does not depend on, unless `changes`
  is installed, when only node and edge changes do (one to a node or an
  edge the colouring does not reach still does).
- The edges of one node are read once per round; a colouring at depth `d`
  reads the adjacency `d` times.
- Measured on the manufacturing graph: `bench/KNOWLEDGE.md`, "Structure".

### Templates: a repeated shape said once

A model given a plant as text spends its budget on the same station written
out forty times. Templates write each repeated shape once and each
occurrence as a short reference with only what is its own. A template is a
way of writing nodes down, never a replacement for them: every instance
keeps every node, edge and value it has, and `expand` gives them back.

```
CALL structure.templates(types: ["HAS_LINE", "HAS_STATION", "HAS_SIDE", "HAS_LOCATION", "HAS_ELEMENT", "HAS_INSTRUCTION"], labels: ["Station"])
CALL structure.templates(types: [...], instances: "<template id>", after: 0, limit: 100)
CALL structure.templates(types: [...], instances: "*")             every instance of every template
CALL structure.expand(of: "<template id>", root: [242, 282], types: [...])
CALL structure.paths(types: [...])                                 label and type sequences, with counts
CALL structure.paths(types: [...], kind: "chains")                 runs with one way in and one way out
CALL structure.expand(of: "chain:1705", types: [...])
CALL structure.plan(types: [...], text: "compact", store: "plant")
CALL structure.plan(dictionary: "plant", nodes: [242, 243, ...], text: "compact")
CALL structure.dictionaries()
CALL structure.drop(dictionary: "plant")
```

**What a template is.** The edges of `types:`, followed in `dir:` (`out`,
from parent to child, by default; or `in`), make a hierarchy: a node
belongs to the node that one such edge comes from, when exactly one does.
The *shape* under a node is the node and everything that belongs to it,
down the hierarchy, with every edge going out of those nodes. An edge to a
node of the shape is internal and is written by position (`.4`); any other
is a reference to a shared node and is written by its handle (`n83`), so a
tool used by forty stations is referred to, not copied. Roots are nodes of
`labels:` (every node when not given) whose shape has `min_nodes:` (2) to
`max_nodes:` (4096) nodes; a template is a shape that `min_count:` (2) or
more roots have exactly: the same labels, property keys, positions, edges
and referred nodes, compared in full. Across its instances, a property that
always has the same value is a constant; a text value that is always the
root's value of the same key (or its first N bytes) followed by the same
text is derived (`code=~"-WE01"`); anything else is a slot (`$`), which each
use fills, in the order the definition is read. Values are compared exactly
(`1` and `1.0` differ, unlike colours): a template gives back what was
stored.

`structure.templates` rows: `rank, template, root` (its root's label),
`nodes, edges, slots, instances, plain_tokens` (of its instances written
plainly), `saving_bytes, saving_tokens` (estimated, as if it were used for
all its instances alone) and `definition`, largest estimated saving first.
`instances:` lists a template's instances (`template, root, handle,
slots`), or with `"*"` every template's (with `grouped: true`, one row
per template, `template, instances, roots`, up to 100,000 roots a call);
`dictionary:` lists a kept
dictionary's templates as the graph has them now. A template id is a hash
of its definition, with referred nodes by identity value or content, so the
same data under other ids has the same templates.

**Expansion.** `structure.expand(of: <template>, root: <id or list>)`
rebuilds each instance from the definition and its slot values alone,
after checking in full that the root has exactly the template's shape,
constants and derivations; a root that does not is refused ("is not an
instance of template ..."). Rows are `kind` (`node` or `edge`), `id`,
`labels`, `type`, `from`, `to`, `props` (a list of `[key, value]`) and
`root`. Without `dictionary:` the templates are found again with the same
arguments as `templates`; with it, they are the kept dictionary's. At most
1,000 roots a call.

**Deviations.** A station that differs from its design (a tool missing,
an element more) has another shape, so it is never an instance of the
design's template. It is written plainly, with the smaller templates of
the parts it shares with its design (a side, a location, an element) used
where they make it shorter. Stations that deviate in the same way are
instances of a template of their own.

**Paths.** `structure.paths` counts the label and type sequences that
paths follow from the nodes of `labels:` (or from the nodes no followed
edge reaches), up to `max_hops:` (12): rows `rank, pattern, path, hops,
paths, ends, maximal`, longest maximal patterns first. Counting merges the
paths that reach one node along one sequence, so it costs nodes times
sequences, not paths. `kind: "chains"` lists runs of nodes that each have
one followed edge in and one out, between two that do not: `chain` (named
`chain:<first interior id>`), `pattern, path, hops, from, to, interior,
nodes`; `expand(of: "chain:<id>")` gives the interior nodes and the run's
edges.

**The notation, `glider-compact/1`.** This is the whole of it: a reader
with a text and, when the text names one, its dictionary needs nothing
else (`tests/structure_notation.rs` decodes from this description alone).

*Lines.* A text is lines. The first is the header:
`# glider-compact/1`, then ` dictionary <hash>` when the text is written
with a dictionary, then ` identity <k1>, <k2>` when nodes are named by
identity. Other lines starting with `#` are comments (the legend, `# body`).
A dictionary is a legend line, definitions and patterns; a text that
carries its dictionary has it after the header and before `# body`, and a
text written with a kept dictionary has only the header and the body.

*Names and values.* A label, key or type is bare when it is `[A-Za-z_]`
then `[A-Za-z0-9_]*`, otherwise in backticks with a backtick doubled. A
value is `null`, `true`, `false`, an integer in digits, a float with a point
or an exponent (`1.0`, `-0.5`, `1e300`, `NaN`, `inf`, `-inf`), a JSON string,
or a list `[a, b]` (comma and one space).

*Handles.* `n12` is the node with id 12. With `identity` in the header,
`@code="P01-S1"` is the one node whose identity key `code` has that value:
its identity key is the first of the header's keys it has, the handle names
it with its value, and its card leaves that property off (the handle
carries it). A node that has none of the keys, or whose value another node
of the text has under the same key, is written `n<id>` and keeps every
property on its card, so a handle names exactly one node.

*Plain blocks.* One per node: a card `handle:Label:Other k=v ...` (labels
sorted, properties by key rank then name), then its outgoing edges, by
type: `handle -TYPE-> h1, h2` for the edges without properties (targets
sorted by handle), then `handle -TYPE{k=v, ...}-> h` one line per edge with
properties. Parallel edges are listed as often as they exist.

*Definitions.* `Tn #<16 hex>` and then lines that start with a space. A
node line is ` .k:Label k=spec ...` (positions in preorder, `.1` the root,
numbered consecutively). A spec is a value (a constant), `$` (a slot), or a
derivation `~"x"` (the value of the same key at the root of the definition
the line belongs to, then `x`) or `~N"x"` (its first N bytes, then `x`). A
nested line ` .k=Tm` says that positions `.k` to `.k+size(Tm)-1` are `Tm`'s
nodes in `Tm`'s order, with `Tm`'s edges between them and from them (its
targets shifted by `k-1`); `k=spec` pairs after it give values of `Tm`'s
root where `Tm` has a slot, and a `~` there refers to the root of the
definition the line is in, while every derivation inside `Tm` refers to
`Tm`'s root (here, `.k`). Edge lines ` .k -TYPE-> t1, t2` and
` .k -TYPE{k=spec, ...}-> t` follow the node lines; a target is a position
`.m` or a handle. A definition writes every edge of its nodes that no nested
template it uses has: under `dir: "in"` that includes a nested root's edge
to its owner.

*Slots.* A definition's slots, in order: its node lines top to bottom, each
line's `$` left to right, where a nested line contributes the nested
template's slots in that template's own order, less those the line gives;
then its edge lines top to bottom, each line's `$` left to right.

*Uses.* `Tn ids v1 v2 ...`: the template's name, then which nodes these
are, then one value per slot, separated by single spaces. Without
`identity`, ids are every position's node id in position order, as `n`
followed by comma-separated ids and runs (`n242..244,247` is 242, 243, 244,
247). With `identity`, only the positions whose node is written by id are
listed, as `.k=n<id>` separated by commas (`.5=n246,.7=n248`), or nothing
when there are none; every other node's handle is `@key=value` from its
values as the use gives them. Edges of the use's nodes that are not the
template's (under `dir: "in"`, the root's edge to its owner) are written
after the use line as plain edge lines.

*Chains and patterns.* `Pn #<16 hex> -T1-> (:L1) -T2-> ... -Tk->` (or
`<-T-` steps under `dir: "in"`) is a pattern: edge types with the labels
of the nodes between them. `a -Pn-> b [x y]` in a node's block is the run
`a -T1-> x -T2-> y -T3-> b` (for a `<-T-` step the edge goes the other
way), every edge without properties; those edges are not written again.

```
# glider-compact/1 dictionary 0f472ec37f752451c3de9ff727ef1bcb
# .k: a template's k-th node in preorder, ...
T1 #1a78f86e1f72f9c0
 .1:WorkElement name="Check tailgate fit" code=$ seq=12 std_time_seconds=$
 .2:WorkInstruction title="Examine tailgate fit" number=$ revision="A" scope="local" text="Examine the tailgate fit to 9 mm. ..."
 .1 -FOLLOWS-> n179
 .1 -HAS_INSTRUCTION{from=$}-> .2
 .1 -USES_TOOL-> n24, n59
T36 #ea11ab640dbe7e2c
 .1:Side name="Left" code=$
 .2=T31 code=~"1"
 .11:WorkLocation name="Tailgate post" code=~"3"
 .12=T11 code=~16"WE08"
 ...
P1 #3b1c... -HAS_ELEMENT-> (:WorkElement) -HAS_INSTRUCTION->
# body
n241:Line name="Line 1 Battery tray" code="P01-S1-L1" rate_per_hour=26
n241 -HAS_STATION-> n242, n282, n322
T39 n242..244,247..248,... "Station 010 Battery tray torque cell" "P01-S1-L1-ST010" "2024-01-30" 1 50 22 "WI-000001-02A" ...
n1459 -P1-> n1707 [n1705 n1706]
```

Here `.2=T31 code=~"1"` says positions `.2` on are `T31`'s and gives
`T31`'s root `code` as `T36`'s root code followed by `1`. With every node
identified, a text is the same for the same data under other ids.

**The plan.** `structure.plan` writes the part of the graph asked for (the
whole graph, or `nodes:`) plainly and with templates and chain patterns, and
keeps what makes it shorter in tokens (`by: "bytes"` to count bytes).
Larger templates are chosen first, each when its uses save more than its
definition costs; then templates are nested in chosen definitions where
that is shorter; then further uses are added where a use is shorter than
what it replaces; then chains. Both texts are then written and measured,
and when the compact one is not shorter nothing is chosen. Rows: first the
`total` (`id` the dictionary's hash; `define_*` the dictionary,
`use_*` the compact text, `plain_*` the plain text, `saving_*` their
difference, exactly; `text` the text asked for with `text: "compact"`,
`"plain"` or `"dictionary"`), then each candidate (`kind` `template` or
`pattern`, `chosen`, `uses`, `nested_in`, the cost of defining it once,
of its uses, of the plain text they replace, and its saving). `lambda:` is
accepted for candidates that leave something out; there are none, so it
weighs nothing and nothing is ever left out.

**Kept dictionaries.** `store: "<name>"` keeps the chosen dictionary in the
extension's keyspace: its definitions, its text, the revision it was made
at, the arguments, the code version (`structure-templates/1`) and the hash
that names its text. `plan(dictionary: "<name>")` writes with it without
repeating it (the text's first line names it by hash), so many answers can
share one dictionary given to a model once. An occurrence is written as a
use only when it agrees with the definition in everything the definition
says, including the handles of the nodes it refers to, so a stale
dictionary costs uses, never truth. `structure.dictionaries()` lists them (`name,
revision, fresh, hash, templates, patterns, bytes, tokens, code, args`);
`fresh: "require"` refuses a stale one; `structure.drop(dictionary:)`
forgets one; `stored_bytes` counts every record kept for one. Freshness
works as for colourings: with the `changes` journal installed, a
dictionary is stale only when a node or an edge changed since; without it,
any write but the extension's own makes it stale. Keeping or dropping
either leaves the other fresh. With nothing chosen, `store:` keeps a
dictionary of the legend alone, and `text: "dictionary"` returns it.

Which nodes are instances is never kept: a selection works it out from the
graph as it is now (ownership from each node's own edges, each candidate
root's shape hashed and matched in full against the kept definition), so a
node changed since the dictionary was made is written plainly unless it
still matches exactly, and nothing is written from a membership that may
no longer hold. A definition also says how it names the nodes it refers
to; an occurrence whose referred node is named otherwise now (a tool whose
code changed) is written plainly. Beside the definitions a dictionary keeps an index of each
template's root shape and id (about 50 bytes a template), so a selection
reads only the definitions its shapes can use.

**Bounds.** The hierarchy's state is 56 bytes a node, within half of
`algorithm_memory()` and spilling past it; candidates are grouped by
external sorts; only roots whose shape repeats are walked, each at most
`max_nodes:` nodes. At most 100,000 templates a call (narrow with
`labels:`, `types:` or larger `min_count:`/`min_nodes:`), and 10,000 rows,
continued with `after:`.

### Limits of templates

- Finding templates reads the whole graph (a template's constants are what
  is the same across all its instances), so `templates`, `plan` without a
  kept dictionary, `plan(dictionary:)` without `nodes:`, `expand` without
  `dictionary:` and `templates(dictionary:)` are whole-graph calls. A
  selection written with a kept dictionary, `plan(dictionary:, nodes:)`,
  reads only the selection, the owners above it and the definitions it can
  use: milliseconds at a million nodes. `expand(dictionary:)` reads only the
  shapes under its roots. `find: "graph"` makes a selection's plan find its
  instances over the whole graph instead (the same text, for checking).
- A deviating station is written plainly with its parts' templates; a
  template plus a list of differences is not built.
- Sibling nodes are numbered by content, not by id, so a use's id list
  folds into runs only where the two orders agree.
- Measured on the manufacturing graph: `bench/KNOWLEDGE.md`, "Templates".
