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
 * `glider` is the loaded module from @glider/wasm and `db` the GliderDb to
 * start with. The calls are synchronous — the engine is right here — but they
 * are wrapped in promises so the UI has one code path for both transports.
 *
 * Unlike HTTP, this transport can swap the graph it serves: `open(file)`
 * replaces it with the contents of a `.gldb` or JSON Lines file the user
 * picked, and `download()` hands the current graph back as JSON Lines. The
 * wasm build has no filesystem, so an opened file is a copy — edits stay in
 * the tab until exported.
 *
 * Timing is measured here with performance.now(): the wasm build reports ms: 0
 * because wasm32-unknown-unknown has no clock of its own.
 */
export function wasmTransport(glider, db, { name = null, onSwap } = {}) {
  const timed = (fn) => {
    const t0 = performance.now()
    const out = fn()
    const ms = performance.now() - t0
    return { ...out, ms }
  }

  let source = name

  return {
    kind: 'wasm',
    get label() {
      return source ? `in-browser · ${source}` : 'in-browser'
    },
    get source() {
      return source
    },
    canOpen: true,
    query: async (q) => timed(() => db.query(q)),
    schema: async () => db.schema(),
    expand: async (id, limit = 50) => ({ graph: db.expand(Number(id), limit) }),
    nodes: async (o) => db.nodes(o),
    edges: async (o) => db.edges(o),

    /** Replace the graph with a file's contents. The old one survives a failure. */
    async open(file) {
      const bytes = new Uint8Array(await file.arrayBuffer())
      const next = isJsonl(file.name, bytes) ? fromJsonl(glider, bytes) : glider.openBytes(bytes)
      db.close()
      db = next
      source = file.name
      onSwap?.(db)
    },

    /** The graph as JSON Lines, which any glider can import. */
    exportJsonl: async () => db.exportJsonl(),
  }
}

/** A glider file starts with its magic; anything else is taken as JSONL. */
function isJsonl(name, bytes) {
  if (/\.(jsonl|ndjson|json)$/i.test(name)) return true
  const head = String.fromCharCode(...bytes.subarray(0, 7))
  return !(head.startsWith('GLIDER') || head.startsWith('GRAPHLT')) && (bytes[0] === 0x7b || bytes[0] === 0x0a)
}

function fromJsonl(glider, bytes) {
  const db = glider.open()
  try {
    db.importJsonl(new TextDecoder().decode(bytes))
    return db
  } catch (e) {
    db.close()
    throw e
  }
}
