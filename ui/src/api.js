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
