# @glider/wasm

glider — an embeddable property-graph database — compiled to WebAssembly, with
TypeScript types.

The whole engine runs in your process: query planner, pattern matcher, graph
algorithms. There is no server, no network call, and no database to install.

```ts
import { loadGlider } from '@glider/wasm'

const glider = await loadGlider()
const db = glider.open()

db.run(`CREATE (a:Person {name:"Ada"})-[:KNOWS {since:2019}]->(b:Person {name:"Bob"})`)

const r = db.query('MATCH (a)-[r:KNOWS]->(b) RETURN a, r, b')
r.rows[0][0].props.name   // "Ada"  — a real object, not a JSON string
r.graph.nodes             // ready to hand to a renderer

db.close()
```

## Why the module has no imports

```ts
WebAssembly.Module.imports(mod)   // []
```

glider depends on nothing but the Rust standard library, and the parts of
`std` it uses on this target need no host. So the `.wasm` is self-contained:
no WASI shim, no `wasm-bindgen` glue, no JS trampoline. It runs unchanged in
Node, Deno, Bun, browsers, Cloudflare Workers and anything else with a
WebAssembly engine.

That property is asserted in the test suite, not just claimed.

## Install

```sh
npm install @glider/wasm
```

Building from this repo instead:

```sh
rustup target add wasm32-unknown-unknown
cd ts && npm install && ./build.sh      # -> dist/{index.js,index.d.ts,glider.wasm}
npm test
```

## Loading the module

`loadGlider()` with no argument resolves `glider.wasm` next to the module,
which is how the package ships. Pass a source explicitly when your bundler
moves things around:

```ts
await loadGlider(new URL('./glider.wasm', import.meta.url))  // URL
await loadGlider(fetch('/assets/glider.wasm'))               // Response
await loadGlider(await readFile('glider.wasm'))              // bytes
await loadGlider(compiledModule)                             // WebAssembly.Module
```

One `GliderModule` can open many graphs. Each `open()` is an independent,
isolated graph.

## API

| | |
|---|---|
| `glider.open()` | a fresh in-memory graph |
| `glider.version` | engine version |
| `db.query(q)` | typed `QueryResult` — columns, rows, graph payload |
| `db.run(q)` | run for effect, returns entities touched |
| `db.graph(q)` | just the `{nodes, edges}` projection |
| `db.schema()` | labels, relationship types, indexes, with counts |
| `db.expand(id, limit?)` | neighbours of one node, both directions |
| `db.importJsonl(text)` | bulk load |
| `db.exportJsonl()` | dump the whole graph |
| `db.stats()` | node/edge/label/index counts |
| `db.close()` | release it (also via `Symbol.dispose`) |

### Typed results

Cells are scalars or entities, and entities are tagged so you can narrow them:

```ts
import { isNode, isRel } from '@glider/wasm'

for (const row of r.rows) {
  for (const cell of row) {
    if (isNode(cell)) console.log(cell.labels, cell.props)
    else if (isRel(cell)) console.log(cell.type, cell.from, '->', cell.to)
    else console.log(cell)          // null | boolean | number | string | array
  }
}
```

This matters more than it looks. glider's internal `Value` has no entity
variant — a node rendered through the older flat API arrives as a *string*
containing JSON, which is indistinguishable from a text property that happens
to look like an object. The typed API resolves every candidate against the live
graph before tagging it, so a crafted string cannot masquerade as a node. There
is a test for exactly that.

### Resource management

```ts
using db = glider.open()      // TypeScript 5.2+, closed at scope exit
```

## Persistence

`wasm32-unknown-unknown` has no filesystem, so graphs are **in-memory only**.
The file-backed modes — `glider_open`, the WAL, compaction, replication — are
not reachable from this build.

Persist by moving JSONL yourself:

```ts
localStorage.setItem('graph', db.exportJsonl())      // small graphs
// ...later
const db2 = glider.open()
db2.importJsonl(localStorage.getItem('graph')!)
```

For anything sizeable prefer IndexedDB or OPFS; `exportJsonl()` returns a plain
string either way.

## Differences from Cypher worth knowing

glider implements a Cypher-flavoured subset. Three divergences bite most often:

- **`RETURN` needs a `MATCH`.** There is no bare `RETURN 1`.
- **`count()` over zero matches returns zero rows**, not one row holding `0`.
  Use `db.stats()` when you want a count that is always present.
- `count(DISTINCT x)` is not supported; `RETURN DISTINCT` is.

## Timing

`QueryResult.ms` is always `0` under wasm — the target has no clock, so
`Instant::now()` would trap. Time it from the host:

```ts
const t0 = performance.now()
const r = db.query(q)
const ms = performance.now() - t0
```

## Threads

A `GliderDb` is not thread-safe, and wasm linear memory is per-instance
anyway. Give each Worker its own `loadGlider()`.

## Size

The module is 459 KB uncompressed — 168 KB gzipped, 135 KB brotli. That is the
entire database: storage layer, query engine and fifteen graph algorithms.
