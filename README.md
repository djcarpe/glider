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
glider :memory: --max-memory 4G        # ... that may use at most 4 GiB
glider big.gldb --cache-size 256M      # a file of any size, in about 256 MiB of RAM
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

The SQLite model: **one engine, one page format, two places for the pages.**

| | `:memory:` | a file on disk |
|---|---|---|
| where pages live | in RAM | the database file, plus 64 GiB segment files in `<db>-data/` |
| how big the graph can get | `--max-memory` (default: physical RAM) | the disk. Nothing about capacity depends on RAM |
| what RAM holds | the graph | a page cache (`--cache-size`, default 1 GiB) and bounded working memory |
| when it is full | `Error::Full` — the transaction rolls back, the graph stays usable | the same, for a full disk |
| durability | none | write-ahead log + checkpoints |

| module | what it does |
|---|---|
| `storage::pager` | fixed-size pages (16 KiB default), copy-on-write by transaction epoch, CLOCK cache, free list, alternating superblocks |
| `storage::btree` | copy-on-write B+trees over byte keys, overflow chains, a bottom-up bulk builder |
| `storage::log`, `storage::db` | the logical write-ahead log, checkpoints, recovery, locking |
| `storage::extsort`, `ooc` | external merge sort; per-node arrays, queues and stacks that spill to disk |
| `graph` | nodes, edges, adjacency, label, type and property-index trees; one mutation path shared by live writes and recovery |
| `algo` | algorithms, written once over an `Adjacency` view: an in-memory CSR or the pages themselves |
| `query` | lexer, parser, streaming pattern matcher, expression evaluator |
| `server`, `api` | ~200 lines of `std::net` HTTP; typed JSON for the console and wasm |
| `replica`, `wal` | replication: base snapshots plus the shipped log |

The graph is seven B+trees: nodes, edges, adjacency (`(node, direction, edge)`,
so a node's neighbours are one range scan), label and type membership, property
indexes (keys encoded so byte order equals value order), and a catalog. A point
lookup reads a handful of pages — about 1.4 disk reads cold at 100 GB — and
opening any database takes milliseconds: it reads a superblock, not the data.

Queries stream: a match is consumed as it is produced, `count(*)` never
builds rows, `ORDER BY … LIMIT` keeps only the top k, and the writes of a
`MATCH … SET/DELETE/CREATE` spool their match set to a temp file past the
working memory. Algorithms run over an in-memory projection when it fits the
working memory (`--work-mem`, default 256 MiB; `:memory:` uses the headroom
under `--max-memory`) and straight over the pages when it does not, with
their per-node state spilling to disk — same code, same answers.

To make a database too large to build through the graph API, implement
`glider::legacy::image::ImageSource` over your data and call
`Graph::bulk_load`: external sorts feed bottom-up tree builds, so memory
stays at the working-memory budget however large the result.
`bench/src/bin/scale-gen.rs` builds a 25 GiB graph that way; a 100 GB B+tree
built the same way (`pagebench`) took 15 minutes in 1.1 GB of RAM.

Databases from before paged storage (a snapshot image followed by a log) are
converted with `glider <db> migrate`, which keeps the original as
`<db>.legacy.bak`. `Graph::from_bytes` still reads them, for the browser.

### Durability

`--sync always | normal | off`

- `always` — fsync every commit. Survives power loss.
- `normal` *(default)* — hand bytes to the OS each commit. Survives process
  crash, not power loss.
- `off` — buffer aggressively. For bulk load.

A commit appends the transaction's operations and a commit marker to the
write-ahead log (`<db>-wal/`). Pages are copy-on-write: a transaction never
overwrites a page the last checkpoint refers to, so the file always holds a
consistent state. A checkpoint (every 256 MiB of log, `--checkpoint SIZE|off`,
`COMPACT`, and on close) writes the dirty pages, fsyncs, then flips to the
other of two superblocks; the log it covers is deleted. Recovery opens the
newest valid superblock and replays committed transactions from the log
through the same code that ran them; a torn tail is discarded, so you never
see a partial transaction. `ROLLBACK`, and any failed statement, drops the
transaction's pages and cuts the log back.

Every page carries a CRC and its own page number. Damage found while reading
is recorded as the graph's integrity error: queries then fail instead of
returning results. `glider <db> verify` walks every tree. A kill -9 loop
(`pagebench crashloop`) recovers cleanly every time; that and the other
recovery paths are tests.

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
STATS   SCHEMA   COMPACT   CLEAR   BEGIN   COMMIT   ROLLBACK   HELP
```

- **operators** `= <> < <= > >= AND OR NOT IN [..] IS NULL CONTAINS STARTS WITH ENDS WITH + - * /`
- **functions** `id labels type degree indegree outdegree keys length lower upper trim abs round floor ceil sqrt toInt toFloat toString coalesce`
- **aggregates** `count sum avg min max collect`, with implicit grouping on the
  non-aggregate return items, as Cypher does.

Each statement is a transaction: it commits whole, or on an error leaves no
trace. `BEGIN` groups the statements that follow into one transaction;
`COMMIT` makes it durable, `ROLLBACK` discards it. `COMPACT` checkpoints.

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

The **Explore** tab browses a graph without queries. A search bar lists nodes
or relationships — filtered by label or type, matched against any property —
and loads them a page at a time as you scroll, so a graph of any size opens
instantly. Click a result to put it on the canvas, double-click a node to pull
in its neighbours, and edit whatever is selected in place: properties (typed —
text, int, float, bool, null, list), labels, new nodes, new relationships,
deletes. Every edit is issued as an ordinary `SET` / `REMOVE` / `CREATE` /
`DELETE` statement, so it is logged, indexed and committed exactly as if you
had typed it.

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
an operating system. 824 KB for the entire database, storage engine included.

Under wasm there is no filesystem, so graphs are in-memory page stores;
persist with `exportJsonl()` / `importJsonl()`. An existing database file can
be loaded from its bytes with `glider.openBytes()` (edits stay in memory).
Details and the full API: `ts/README.md`.

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

**Any size.** Each algorithm is written once against an adjacency interface
with two implementations: an in-memory CSR projection, used when it fits the
working memory (`--work-mem`, default 256 MiB; for `:memory:` the headroom
under `--max-memory`), and a view that reads adjacency straight from the
pages. Per-node state (ranks, distances, component ids), BFS queues and DFS
stacks live in arrays that spill to temp files past the budget; Kruskal's
edge list is sorted externally. Results are identical either way — a test
runs every algorithm both ways, and against the previous engine's
implementation bit for bit. `tier: "mem"` or `tier: "ooc"` forces one.

## Performance

[`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) compares glider with SQLite (a
normalized schema with foreign keys and indexes) and Memgraph (in memory) at
500 MiB, 1, 5, 10 and 25 GiB: 30 reads, 5 graph algorithms and 6 write
workloads, every answer checked across engines, with the methodology, raw
results and the Grafana/eBPF observability setup used to profile them.
glider was the fastest of the three on 18 to 21 of the 30 reads at every size,
by orders of magnitude on shortest paths, traversals and counts; SQLite stays
faster on whole-label scans, aggregates and bulk writes.

Opening takes milliseconds at any size and point queries cost a few page
reads. Reads through the paged engine keep the page cache bounded, though
peak memory still grows with the graph (8.6 GB while reading the 25 GiB
graph with a 1 GiB cache), which is being tracked down.
[`bench/SCALE.md`](bench/SCALE.md) has the earlier snapshot-image engine's
numbers. A 100 GB B+tree (`pagebench`) was built in 15 minutes at 1.1 GB of
RAM, answered cold point lookups in 0.5 ms (1.35 disk reads each), scanned at
450 MB/s, and recovered from 50 kill -9s in a row.

`glider bench <n>` runs a quick synthetic benchmark on your own hardware.

### Test data

`scale-gen` builds graphs of a given size — sixteen relationship types with
power-law hubs, cliques, trees, chains, DAGs, self-loops and multi-edges —
directly as paged databases (`--paged`, a bulk load) or through the
transactional API (`--paged-insert`). `stress-gen` writes the commerce graph
from `scripts/mundane_graph.py` as a log in the pre-paged format, in constant
memory; convert its output with `glider <file> migrate`.

```sh
cargo build --release --workspace
./target/release/scale-gen --size 10GiB --paged --out big.gldb
```

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

`Graph::open_opts` takes `OpenOptions` for the page cache, working memory,
page size, checkpoint interval and sync mode; `Graph::memory_with_limit(bytes)`
caps an in-memory graph, which then reports `Error::Full` (and rolls the
transaction back) instead of growing past it.

`Graph` is `Send`; wrap it in a `Mutex` for shared access, which is what the
server does.

## HTTP

```
POST /query    body is the query text   -> {"columns":[...],"rows":[...],"message":"..."}
GET  /stats
GET  /health
GET  /metrics                           -> Prometheus text
GET  /                                  -> the browser console

POST /api/query                         -> typed result + graph:{nodes,edges} + op (engine report)
GET  /api/schema                        -> node/edge totals; labels, rel types, indexes with counts
GET  /api/expand?id=N&limit=K           -> neighbours of one node
GET  /api/nodes?label=&q=&from=&limit=  -> a page of nodes:  {nodes, next, total}
GET  /api/edges?type=&q=&from=&limit=   -> a page of edges:  {edges, nodes, next, total}
```

The page endpoints are cursor-paged by id: pass a page's `next` as the next
request's `from`. That keeps a walk through a million nodes O(page) per
request rather than O(offset), and means a node created or deleted between
pages shifts nothing. `q` is case-insensitive free text matched server-side
against labels (or the relationship type), every property value, and the id.

`/query` returns entities as JSON *strings*; `/api/query` returns them as
objects tagged `"_e":"node"` / `"_e":"rel"` and adds the drawable graph
payload. Use `/api/*` for anything that renders a graph, `/query` for anything
already written against it.

## Observability

Every runtime — native, wasm, BEAM — reports the same metrics and spans under
the same names (`glider.queries`, `glider.query.duration`, `glider MATCH`
spans with `db.operation.name`, rows and page-cache hits). Natively, set
`OTEL_EXPORTER_OTLP_ENDPOINT` and glider pushes OTLP itself, std only;
`glider serve` also honours `traceparent` and serves `/metrics`. The wasm
wrapper takes an OpenTelemetry tracer and meter; glider_ex emits `:telemetry`
events and has `Glider.OpenTelemetry.setup/0`. See
[docs/OBSERVABILITY.md](docs/OBSERVABILITY.md).

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
- **Bigger than the old format.** A paged file is about 1.4× the snapshot
  image the previous engine wrote for the same graph.
- **Not full Cypher.** No `WITH`, `UNWIND`, `OPTIONAL MATCH`, `MERGE`, or
  multi-part queries. What's documented above is what exists.
- **Results are returned whole.** Matching streams, and `count`, `LIMIT` and
  `ORDER BY … LIMIT` hold only what they return, but a query that *returns*
  millions of rows builds them all. Aggregate, filter or page.
- **Variable-length patterns return distinct endpoints, not distinct paths.**
  Deliberate, to avoid combinatorial blowup.
- **`betweenness` is O(n·m).** Use `samples:` above a few thousand nodes.
- **No authentication on the HTTP server.** Bind it to localhost.

## Tests

```sh
cargo test
```

About 120 tests. The central one is differential: random operations run
against the previous engine as an oracle and against the paged engine with
tiny pages and a tiny cache — in memory, on disk, reopened between batches,
and filling up to `max_memory` — and every observable read is compared.
Others cover B+trees against `BTreeMap` through commits and rollbacks, crash
recovery, checkpoint holds under a live writer, replication end to end,
every algorithm in memory against out of core, write statements whose match
sets spill, each query form, and that malformed queries return errors
instead of panicking.

## Repository layout

```
src/            the library and both binaries
src/storage/    pages, B+trees, write-ahead log, external sort
src/legacy/     the previous engine: test oracle, and `migrate`
src/stream/     glider-stream: S3/HTTP replication of pre-paged files
tests/          integration tests
bench/          benchmark harness (workspace member; ./bench/run.sh)
include/        glider.h, the C ABI header
ts/             TypeScript/wasm bindings
ui/             browser console (React + Vite)
scripts/        cross-compilation helpers
docs/           design notes: MOBILE, REPLICATION, STREAM, REVIEW, TODO
```

## Licence

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
