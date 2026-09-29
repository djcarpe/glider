# glider browser

The React console. Two builds from one source tree:

| build | output | engine runs |
|---|---|---|
| **embedded** | `dist/index.html`, a single inlined file | in the `glider` process, reached over HTTP |
| **standalone** | `dist-standalone/`, HTML + `glider.wasm` | in the browser tab, no server at all |

```sh
npm install
npm run build              # embedded  -> dist/index.html
npm run build:standalone   # standalone -> dist-standalone/
npm run dev                # hot reload against a glider on :7878
```

## How it reaches the engine

`src/transport.js` has the two backends; `src/api.js` is a facade over
whichever is active. No component knows which one it is talking to, because
both doors open onto the same Rust in `src/api.rs`.

```
  App / Frame / GraphView
          │
       api.js            facade
          │
  ┌───────┴────────┐
httpTransport   wasmTransport
  fetch()         @glider/wasm
  /api/*          in-process
```

## Telemetry

Every query response carries the engine's report on the statement as `op`
(see `../docs/OBSERVABILITY.md`). The frame footer shows it as pages read and
the share served from the page cache, next to the time. Over HTTP the time is
the server's; under wasm it is measured in the tab, because the engine has no
clock there.

## Embedding in the binary

The embedded build is inlined to a single `index.html` by
`vite-plugin-singlefile`, copied to `src/console.html`, and pulled into the
binary with `include_str!`. That keeps `glider serve` one file with no static
routing and no runtime dependency on Node.

```sh
npm run build && cp dist/index.html ../src/console.html && (cd .. && cargo build --release)
```

The built file **is committed**. Node is needed only to change the UI — a
plain `cargo build` still works with zero crates and no build script, which is
the point of the project.

Cost: about +260 KB of HTML, taking the release binary from ~1.05 MB to
~1.33 MB.

## Layout

| file | |
|---|---|
| `App.jsx` | shell, Console/Explore switch, sidebar, frame stack, history, opening files |
| `Editor.jsx` | the query textarea: type-ahead list, keyboard handling, history walking |
| `complete.js` | what the type-ahead offers: statements, labels, types, keys, procedures, keywords |
| `Frame.jsx` | one result frame; Graph/Table/JSON tabs, table rendering |
| `GraphView.jsx` | d3-force layout in SVG: drag, zoom, select, expand; controlled mode for the explorer |
| `Explorer.jsx` | the Explore tab: search with label/type suggestions, lazy-loaded list, canvas, edit actions |
| `Inspector.jsx` | the explorer's edit panel: properties, labels, new node / relationship, delete |
| `edit.js` | writes as queries: literal rendering, typed input, create/set/remove/delete |
| `entities.js` | label colours, captions, entity narrowing |
| `transport.js` | the HTTP and wasm backends; the wasm one can swap in an opened file |
| `styles.css` | design tokens and all styling |

## Notes

- The graph view draws at most 300 nodes. Past that a force layout stops
  being informative, and the frame rate goes with it.
- d3 owns node positions and mutates them in place; React re-renders on a tick
  counter rather than holding coordinates in state.
- Query history lives in `localStorage`, wrapped in try/catch so a private
  window or blocked site data does not stop the console from starting.
- The explorer's list is the only thing that touches the whole graph, and it
  does so through `/api/nodes` and `/api/edges`: cursor-paged by id, fifty at
  a time, fetched as a sentinel at the foot of the list scrolls into view. The
  canvas only ever holds what you clicked or expanded, and still stops drawing
  at 300 nodes.
- Explorer edits are queries (`edit.js`), not a separate write API. Values
  typed into the inspector are read like literals — `42` is an int, `4.2` a
  float, `true` a bool, `[..]` a list — with an explicit type override for the
  cases that guess wrong. Names that are not plain identifiers are
  backtick-quoted.
- Type-ahead (`complete.js`) reads the text before the caret rather than
  parsing it, because it has to work on half-written queries. It offers whole
  statements on an empty line, labels after `(n:`, types after `[:`, property
  keys after `n.` or inside `{…}`, procedures after `CALL` and their arguments
  inside `(…)`, and otherwise the keywords, variables and functions that fit
  the current clause. Keys come from `node_keys` / `edge_keys` in the schema —
  a sample of each label, not a scan. Its vocabulary mirrors `HELP` in
  `src/query.rs`; change one, change the other.
- The standalone build can open a file: **Open file…** or drop a `.gldb` or
  `.jsonl` on the page. A `.gldb` is replayed from its bytes
  (`glider.openBytes`); it is a copy, so edits stay in the tab until
  **Export** downloads them as JSON Lines. The embedded build has no such
  button — there the server already has a file open (`glider my.gldb browser`).
