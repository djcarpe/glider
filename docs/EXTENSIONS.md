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
- **Extensions can follow writes.** An extension whose `wants_writes` says so
  has `on_node_write` called after every node write made through the graph
  API (queries included), inside the same transaction, with the node before
  and after. What it writes in response is logged, so recovery and replicas
  replay the result without running the hook again. Bulk loads do not call
  hooks; an extension rebuilds from a scan instead (`CALL context.index()`).

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
`on_node_write` maintain derived state. The built-in `context` extension
(`src/ext/context.rs`, `src/ext/text.rs`) uses all of them and is the
worked example.

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
- Write hooks see node writes only; edge properties are not indexed.
- Bulk loads (`Graph::bulk_load`, `migrate`) bypass hooks: run
  `CALL context.index()` after one.
- `CALL context.index()` builds in one transaction, and its log holds every
  posting until the next checkpoint (about 0.5 GiB for the 8.6M postings in
  the benchmark).
