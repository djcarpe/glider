# glider

An embeddable property-graph database in a single binary, in the shape SQLite
took for relational data: one file on disk, no server to run, no dependencies to
install, and a library you can link straight into your process.

**Zero external crates.** Nothing but `std`. That is the portability story — it
builds for any target rustc supports, with no C toolchain, no build scripts, no
vendored native code. The x86-64 Linux release binary is 820 KB dynamic, 963 KB
as a fully static musl build.

```
cargo build --release
cargo build --release --target x86_64-unknown-linux-musl   # static, runs anywhere
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-pc-windows-msvc
```

## Quick start

```sh
glider social.gldb                     # interactive shell
glider social.gldb -c "MATCH (n) RETURN n LIMIT 5"
glider social.gldb -f schema.gql       # run a script
glider social.gldb serve               # HTTP on 127.0.0.1:7878, plus a browser console
glider social.gldb browser             # same, and open the console in your browser
glider :memory:                        # throwaway graph, nothing hits disk
```

```
» CREATE (ada:Person {name:"Ada", city:"London"})
» CREATE (bob:Person {name:"Bob", city:"Paris"})
» MATCH (a:Person),(b:Person) WHERE a.name="Ada" AND b.name="Bob"
    CREATE (a)-[:KNOWS {since:2019}]->(b)
» MATCH (a:Person {name:"Ada"})-[:KNOWS*1..3]->(b) RETURN b.name, b.city
» CALL pagerank(iterations: 30, write: "rank", top: 10)
» MATCH (p:Person) RETURN p.name, p.rank ORDER BY p.rank DESC LIMIT 5
```

## Data model

A labelled property graph.

- **Node** — id, any number of labels, a flat property map.
- **Edge** — id, one type, from, to, a flat property map. Directed, with
  traversal available in either direction or both.
- **Value** — null, bool, int, float, text, or a list of those.

Labels, edge types and property keys are interned to `u32`, so a node costs two
small vectors rather than a pile of `String`s.

## Architecture

Three layers, each independently usable:

| module | what it does |
|---|---|
| `store` | append-only log: CRC32 records, transactions delimited by a commit marker, atomic compaction |
| `graph` | in-memory graph, interning, label and property indexes, CSR projection |
| `algo` | algorithms over the CSR view — all iterative, no recursion |
| `query` | lexer, parser, pattern matcher, expression evaluator |
| `server` | ~150 lines of `std::net` HTTP |
| `api` | typed JSON for the console and the wasm bindings |

The graph is **memory-resident**; the file is the write-ahead log and the
persistent form at once. Reads never touch disk, which is what makes whole-graph
algorithms fast. Writes append. `COMPACT` rewrites the file as the minimal set of
records reproducing current state, reclaiming space from deletes and overwrites.
The trade: your graph must fit in RAM. For the knowledge-graph and
context-graph sizes this is aimed at, that's the right trade; for a
billion-edge graph it is not.

### Durability

`--sync always | normal | off`

- `always` — fsync every commit. Survives power loss.
- `normal` *(default)* — hand bytes to the OS each commit. Survives process
  crash, not power loss.
- `off` — buffer aggressively. For bulk load.

A transaction is a run of records followed by a commit marker. On open, the log
replays; a torn tail (half-written record, or records with no commit marker) is
discarded and the file truncated to the last committed offset. You never see a
partial transaction. Compaction writes a temp file, fsyncs it, then renames —
atomic, so a crash mid-compaction leaves the original intact. Both paths are
covered by tests.

## Query language

A Cypher-flavoured subset, chosen so that everything listed here actually works
rather than approximately works.

```
MATCH (a:Person {name:"Ada"})-[r:KNOWS]->(b) WHERE b.age > 30
      RETURN b.name, count(r) AS n ORDER BY n DESC LIMIT 10
MATCH (a)-[:KNOWS*1..3]->(b) RETURN DISTINCT b          -- variable-length hops
MATCH (a)<-[:REPORTS_TO]-(b) RETURN b                   -- either direction
CREATE (a:Person {name:"Ada"})-[:KNOWS {since:2020}]->(b:Person {name:"Bob"})
MATCH (n:Person) WHERE n.age IS NULL SET n.age = 0, n:Unknown
MATCH (n) WHERE id(n) = 4 DETACH DELETE n
INDEX ON :Person(email)
STATS   SCHEMA   COMPACT   CLEAR   BEGIN   COMMIT   HELP
```

- **operators** `= <> < <= > >= AND OR NOT IN [..] IS NULL CONTAINS STARTS WITH ENDS WITH + - * /`
- **functions** `id labels type degree indegree outdegree keys length lower upper trim abs round floor ceil sqrt toInt toFloat toString coalesce`
- **aggregates** `count sum avg min max collect`, with implicit grouping on the
  non-aggregate return items, as Cypher does.

`BEGIN` turns off autocommit so a batch of statements becomes one durable
transaction; `COMMIT` flushes and turns it back on.

### Indexes

`INDEX ON :Person(email)` builds a sorted index over that label/property pair.
The planner uses it when a pattern supplies a literal for an indexed property,
and falls back to a label scan, then a full scan. Indexes are maintained through
property updates, label changes and deletes, and are recorded in the log so they
survive reopen.

## Browser console

`glider <db> browser` opens a React console in the shape Neo4j Browser
established: a query editor, stacked result frames, and a force-directed graph
view with click-to-expand, alongside table and JSON tabs and a live schema
sidebar.

It is compiled into the binary — one file, nothing to serve, nothing to
install. `glider <db> serve` hosts the same console without opening a browser.

```
glider social.gldb browser                    # opens a browser
glider social.gldb browser --no-open          # headless box: just print the URL
glider social.gldb serve --addr 0.0.0.0:7878  # bind elsewhere
```

The console talks to `/api/*`, which differs from `/query` in one important
way: nodes and relationships come back as real JSON objects plus a
deduplicated `graph:{nodes,edges}` payload. `/query` renders entities to JSON
*strings*, which a client cannot reliably tell apart from a text property that
happens to look like an object — so it cannot be drawn from. `/query` is
unchanged for existing callers.

Source and build instructions: `ui/README.md`.

## TypeScript and WebAssembly

The same engine compiles to `wasm32-unknown-unknown` and runs in a browser,
Node, Deno, Bun or a Worker.

```ts
import { loadGlider } from '@glider/wasm'

const glider = await loadGlider()
const db = glider.open()
db.run(`CREATE (a:Person {name:"Ada"})-[:KNOWS]->(b:Person {name:"Bob"})`)
const r = db.query('MATCH (a)-[r]->(b) RETURN a, r, b')
```

The module imports **nothing** — no WASI, no `wasm-bindgen` glue. That falls
straight out of the zero-dependency rule: there is nothing in glider that wants
an operating system. 459 KB, 135 KB brotli, for the entire database.

Under wasm there is no filesystem, so graphs are in-memory; persist with
`exportJsonl()` / `importJsonl()`. Details and the full API: `ts/README.md`.

## Algorithms

`CALL name(arg: value, ...)`

| | |
|---|---|
| `pagerank` | damping, iterations, tolerance; dangling mass redistributed |
| `betweenness` | Brandes; `samples: n` for an approximation |
| `closeness` | Wasserman-Faust scaling, weighted or not |
| `degree` | in / out / both |
| `triangles`, `clustering` | triangle count, local clustering coefficient |
| `kcore` | core numbers (Batagelj-Zaveršnik) |
| `components`, `scc` | connected, strongly connected (iterative Tarjan) |
| `communities` | label propagation, deterministic |
| `shortestpath`, `sssp` | BFS when unweighted, Dijkstra when given `weight:` |
| `bfs`, `dfs`, `subgraph`, `neighbors` | traversal with depth limits |
| `toposort`, `cycle` | topological order, cycle witness |
| `mst` | Kruskal minimum spanning forest |

Common arguments: `dir: "out"|"in"|"both"`, `type: "KNOWS"` to restrict to one
edge type, `weight: "cost"` to read weights from an edge property, `top: 10`,
and `write: "score"` to **store the result back as a node property**, which lets
you compute once and then query against the scores.

```
CALL pagerank(write: "rank")
MATCH (p:Person) WHERE p.rank > 0.01 RETURN p.name ORDER BY p.rank DESC
```

Everything is iterative. A 100k-node chain runs Tarjan and topological sort
without touching the stack — that's a test, not a hope.

## Performance

Measured on one core of the container this was built in (`glider bench
200000`): 200k nodes, 800k edges, default sync.

```
nodes      0.19s   1,044,000/s
edges      1.61s     498,000/s
pagerank   0.285s  (20 iterations, converged at 16)
components 0.664s
kcore      0.386s
triangles  0.587s
file       31 MB
```

Run `glider bench <n>` on your own hardware.

## Embedding

```rust
use glider::{Graph, Sync, Value};

let mut g = Graph::open(Path::new("kg.gldb"), Sync::Normal)?;

g.autocommit = false;                       // one transaction for the batch
let ada = g.add_node(&["Person".into()], vec![("name".into(), Value::from("Ada"))])?;
let bob = g.add_node(&["Person".into()], vec![("name".into(), Value::from("Bob"))])?;
g.add_edge(ada, bob, "KNOWS", vec![("since".into(), Value::Int(2020))])?;
g.commit()?;

let result = glider::query("CALL pagerank(top: 10)", &mut g)?;
for row in &result.rows { println!("{:?}", row); }

// Or drop to the engine directly:
let csr = g.csr(glider::Dir::Both, None, None);
let (comp, n) = glider::algo::components(&csr);
```

`Graph` is `Send`; wrap it in a `Mutex` for shared access, which is what the
server does.

## HTTP

```
POST /query    body is the query text   -> {"columns":[...],"rows":[...],"message":"..."}
GET  /stats
GET  /health
GET  /                                  -> the browser console

POST /api/query                         -> typed result + graph:{nodes,edges}
GET  /api/schema                        -> labels, rel types, indexes with counts
GET  /api/expand?id=N&limit=K           -> neighbours of one node
```

`/query` returns entities as JSON *strings*; `/api/query` returns them as
objects tagged `"_e":"node"` / `"_e":"rel"` and adds the drawable graph
payload. Use `/api/*` for anything that renders a graph, `/query` for anything
already written against it.

## Import / export

JSON Lines, one object per line, re-importable:

```json
{"type":"node","key":"ada","labels":["Person"],"props":{"name":"Ada"}}
{"type":"edge","from":"ada","to":"bob","label":"KNOWS","props":{"since":2020}}
```

Edges reference endpoints by the `key` or `id` of a node earlier in the file.

```sh
glider kg.gldb import people.jsonl
glider kg.gldb export backup.jsonl
```

## What this is not

Honest limits, so you find them here rather than in production:

- **Single process, single writer.** One `Mutex`, no MVCC, no concurrent
  readers during a write. Same shape as SQLite's default mode.
- **Memory-resident.** Capacity is bounded by RAM, roughly 150–250 bytes per
  node plus property size.
- **Not full Cypher.** No `WITH`, `UNWIND`, `OPTIONAL MATCH`, `MERGE`, or
  multi-part queries. What's documented above is what exists.
- **Pattern results are materialised.** A pattern that matches millions of rows
  builds millions of binding vectors — the 3.2M-result two-hop match in the
  benchmark takes 11s. Filter earlier or use `LIMIT`.
- **Variable-length patterns return distinct endpoints, not distinct paths.**
  Deliberate, to avoid combinatorial blowup.
- **`betweenness` is O(n·m).** Use `samples:` above a few thousand nodes.
- **No authentication on the HTTP server.** Bind it to localhost.

## Tests

```sh
cargo test
```

18 integration tests covering reopen, torn-tail recovery, uncommitted-transaction
rollback, compaction correctness, index maintenance through updates and deletes,
each query form, algorithm results against hand-computed values, deep-chain
recursion safety, and that malformed queries return errors instead of panicking.

## Licence

MIT OR Apache-2.0.
