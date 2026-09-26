// Where the console gets its data.
//
// Two backends, one interface. The UI never learns which it is talking to.
//
//   http  — `glider serve` / `glider browser`. The graph lives in the server
//           process; this browser is a client.
//   wasm  — the whole engine compiled to WebAssembly and running in this tab.
//           No server, no network, nothing leaves the page.
//
// The shapes match because both ultimately come from the same Rust in api.rs:
// the HTTP routes and the wasm exports are two doors onto one implementation.

/** Talk to a glider HTTP server on the same origin. */
export function httpTransport() {
  async function req(path, init) {
    const res = await fetch(path, init)
    const text = await res.text()
    let data
    try {
      data = JSON.parse(text)
    } catch {
      throw new Error(text.slice(0, 400) || `HTTP ${res.status}`)
    }
    if (data.error) throw new Error(data.error)
    if (!res.ok) throw new Error(`HTTP ${res.status}`)
    return data
  }

  return {
    kind: 'http',
    label: 'connected',
    query: (q) =>
      req('/api/query', {
        method: 'POST',
        headers: { 'Content-Type': 'text/plain;charset=utf-8' },
        body: q,
      }),
    schema: () => req('/api/schema'),
    expand: (id, limit = 50) =>
      req(`/api/expand?id=${encodeURIComponent(id)}&limit=${limit}`),
    nodes: (o) => req(`/api/nodes?${qs({ label: o.label, q: o.q, from: o.from, limit: o.limit })}`),
    edges: (o) => req(`/api/edges?${qs({ type: o.type, q: o.q, from: o.from, limit: o.limit })}`),
  }
}

/** Encode the defined entries of an object as a query string. */
function qs(params) {
  const p = new URLSearchParams()
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== null && v !== '') p.set(k, String(v))
  }
  return p.toString()
}

/**
 * Run glider in this tab via the WebAssembly build.
 *
 * `db` is a GliderDb from @glider/wasm. The calls are synchronous — the engine
 * is right here — but they are wrapped in promises so the UI has one code path
 * for both transports.
 *
 * Timing is measured here with performance.now(): the wasm build reports ms: 0
 * because wasm32-unknown-unknown has no clock of its own.
 */
export function wasmTransport(db) {
  const timed = (fn) => {
    const t0 = performance.now()
    const out = fn()
    const ms = performance.now() - t0
    return { ...out, ms }
  }

  return {
    kind: 'wasm',
    label: 'in-browser',
    query: async (q) => timed(() => db.query(q)),
    schema: async () => db.schema(),
    expand: async (id, limit = 50) => ({ graph: db.expand(Number(id), limit) }),
    nodes: async (o) => db.nodes(o),
    edges: async (o) => db.edges(o),
  }
}
