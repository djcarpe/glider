// Shared helpers for rendering nodes and relationships consistently across the
// graph, table and inspector views.

export const LABEL_PALETTE = [
  '#4c8dff', '#f0883e', '#3fb950', '#bc8cff',
  '#e5534b', '#2dd4bf', '#d29922', '#f778ba',
]

const assigned = new Map()

/**
 * Stable colour per label. Assignment is by first-seen order rather than a hash
 * of the name, so the common case — a graph with two or three labels — always
 * gets maximally separated hues instead of whatever the hash happens to collide
 * on.
 */
export function labelColor(label) {
  const key = label || '(none)'
  if (!assigned.has(key)) assigned.set(key, LABEL_PALETTE[assigned.size % LABEL_PALETTE.length])
  return assigned.get(key)
}

// Property names worth showing on a node, best first. Falls back to the first
// short scalar property, then the id.
const CAPTION_KEYS = ['name', 'title', 'label', 'caption', 'email', 'id', 'key', 'username']

export function captionOf(node) {
  const p = node?.props || {}
  for (const k of CAPTION_KEYS) {
    if (p[k] !== undefined && p[k] !== null && p[k] !== '') return String(p[k])
  }
  for (const [, v] of Object.entries(p)) {
    if ((typeof v === 'string' && v.length <= 24) || typeof v === 'number') return String(v)
  }
  return `#${node?.id}`
}

/** True when a cell from /api/query is a node or relationship object. */
export function isEntity(v) {
  return v && typeof v === 'object' && !Array.isArray(v) && (v._e === 'node' || v._e === 'rel')
}
