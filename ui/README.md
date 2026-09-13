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
| `App.jsx` | shell, editor, sidebar, frame stack, history |
| `Frame.jsx` | one result frame; Graph/Table/JSON tabs, table rendering |
| `GraphView.jsx` | d3-force layout in SVG: drag, zoom, select, expand |
| `entities.js` | label colours, captions, entity narrowing |
| `transport.js` | the HTTP and wasm backends |
| `styles.css` | design tokens and all styling |

## Notes

- The graph view draws at most 300 nodes. Past that a force layout stops
  being informative, and the frame rate goes with it.
- d3 owns node positions and mutates them in place; React re-renders on a tick
  counter rather than holding coordinates in state.
- Query history lives in `localStorage`, wrapped in try/catch so a private
  window or blocked site data does not stop the console from starting.
