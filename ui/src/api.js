// Public data API for the console.
//
// A thin facade over whichever transport is active (see transport.js). The
// components import from here and never know whether glider is running behind
// an HTTP server or inside this tab as WebAssembly.

import { httpTransport } from './transport.js'

let active = httpTransport()

/** Swap the backend. Called at startup by the standalone (wasm) build. */
export function setTransport(t) {
  active = t
}

export function transportKind() {
  return active.kind
}

export function transportLabel() {
  return active.label
}

/** Whether this backend can load a file the user picks (the wasm build can). */
export function canOpenFiles() {
  return !!active.canOpen
}

/** Name of the file the graph was opened from, if any. */
export function sourceName() {
  return active.source ?? null
}

/** Replace the graph with a `.gldb` or JSON Lines file. wasm only. */
export function openFile(file) {
  if (!active.open) return Promise.reject(new Error('this console cannot open files; start glider on the file instead'))
  return active.open(file)
}

/** The whole graph as JSON Lines. wasm only. */
export function exportJsonl() {
  if (!active.exportJsonl) return Promise.reject(new Error('export is not available here'))
  return active.exportJsonl()
}

/** Run a query. Resolves to {columns, rows, graph:{nodes,edges}, ms, message, touched}. */
export function runQuery(q) {
  return active.query(q)
}

/** Labels, relationship types and property keys with counts, for the sidebar. */
export function fetchSchema() {
  return active.schema()
}

/** Neighbours of one node, for click-to-expand in the graph view. */
export function expandNode(id, limit = 50) {
  return active.expand(id, limit)
}

/**
 * A page of nodes for the explorer: {nodes, next, total}. Cursor-paged: pass
 * the previous page's `next` as `from` to continue. `q` is free text matched
 * server-side against labels, property values and the id.
 */
export function fetchNodes({ label, q, from = 0, limit = 50 } = {}) {
  return active.nodes({ label, q, from, limit })
}

/** A page of relationships with their endpoints: {edges, nodes, next, total}. */
export function fetchEdges({ type, q, from = 0, limit = 50 } = {}) {
  return active.edges({ type, q, from, limit })
}
