/**
 * The engine's `context` extension: graph retrieval for language models.
 *
 * ```ts
 * db.context.install()
 * db.context.index()                                   // optional: makes search an index lookup
 * const card = db.context.schema()                     // for the system prompt
 * const ctx = db.context.retrieve({ query: 'Ada Lovelace', budget: 1500 })
 * ctx.text   // "# context: 4 nodes, 4 edges; seeds n1\nn1:Person name=\"Ada Lovelace\" ..."
 * db.context.retrieve({ seeds: ['n2'], seen: ctx.refs, budget: 800 })   // follow-up
 * ```
 *
 * `nN` in the text is a node id, accepted back by every call. Options are
 * the procedure's arguments (glider's docs/EXTENSIONS.md), sent as query
 * parameters, never spliced into the statement.
 */
import type { GliderDb, Param } from './index.js'

/** A node id, or the `nN` handle the text form uses. */
export type NodeRef = number | string

export interface RetrieveOptions {
  query?: string
  seeds?: NodeRef[]
  budget?: number
  depth?: number
  types?: string[]
  dir?: 'out' | 'in' | 'both'
  labels?: string[]
  props?: string[]
  exclude?: string[]
  /** Nodes already in the model's context: linked to, not described again. */
  seen?: NodeRef[]
  alpha?: number
  normalize?: number
  max_chars?: number
  max_fanout?: number
  epsilon?: number
  /** How many search hits seed a `query`. */
  top?: number
  label?: string
}

export interface ContextText {
  readonly text: string
  readonly tokens: number
  readonly nodes?: number
  readonly edges?: number
  /** The `nN` handles described in `text`, for `seen` or `seeds` next time. */
  readonly refs: string[]
  readonly message?: string | undefined
}

export interface SearchHit {
  readonly id: number
  readonly ref: string
  readonly name: unknown
  readonly score: number
}

export interface RetrievedRow {
  readonly id: number
  readonly ref: string
  readonly score: number
  readonly hops: number
  readonly card: string
}

export interface PathsOptions {
  k?: number
  depth?: number
  types?: string[]
  dir?: 'out' | 'in' | 'both'
  labels?: string[]
  props?: string[]
  exclude?: string[]
  budget?: number
  max_chars?: number
}

const ARG = /^[a-z_][a-z0-9_]*$/

/** The first cell of the first row: what text-valued procedures return. */
function first(rows: unknown[][]): unknown {
  const row = rows[0]
  if (!row) throw new Error('context: the procedure returned no rows')
  return row[0]
}

function refs(text: string): string[] {
  const out = new Set<string>()
  for (const m of text.matchAll(/^(n\d+)[:\s]/gm)) if (m[1]) out.add(m[1])
  return [...out]
}

export class GliderContext {
  constructor(private readonly db: GliderDb) {}

  /** `CALL context.<proc>(k: $k, ...)`, every value a parameter. */
  #call(proc: string, opts: object) {
    const entries = Object.entries(opts).filter(([, v]) => v !== undefined && v !== null)
    for (const [k] of entries) {
      if (!ARG.test(k)) throw new TypeError(`context.${proc}: bad argument name ${JSON.stringify(k)}`)
    }
    const args = entries.map(([k]) => `${k}: $${k}`).join(', ')
    return this.db.query(`CALL context.${proc}(${args})`, Object.fromEntries(entries) as Record<string, Param>)
  }

  /** Install the extension in this graph (no-op if it already is). */
  install(): void {
    this.db.createExtension('context')
  }

  /** Build the full-text index; writes keep it current. Returns the engine's summary. */
  index(opts: { labels?: string[]; keys?: string[] } = {}): string {
    return this.#call('index', opts).message ?? ''
  }

  unindex(): void {
    this.#call('unindex', {})
  }

  /** Nodes whose text best matches `query`, best first. */
  search(query: string, opts: { label?: string; keys?: string[]; top?: number; scan?: number } = {}): SearchHit[] {
    return this.#call('search', { query, ...opts }).rows.map(([id, ref, name, score]) => ({
      id: id as number,
      ref: ref as string,
      name,
      score: score as number,
    }))
  }

  /** A connected, relevance-ranked subgraph that fits `budget` tokens, as compact text. */
  retrieve(opts: RetrieveOptions): ContextText {
    const r = this.#call('retrieve', opts)
    const [text, nodes, edges, tokens] = r.rows[0] as [string, number, number, number]
    return { text, nodes, edges, tokens, refs: refs(text), message: r.message }
  }

  /** The same selection as JSON, budgeted as JSON. */
  retrieveJson(opts: RetrieveOptions): { nodes: unknown[]; edges: unknown[] } {
    const r = this.#call('retrieve', { ...opts, format: 'json' })
    return JSON.parse(first(r.rows) as string)
  }

  /** The same selection as one row per node, with its score and hops. */
  retrieveRows(opts: RetrieveOptions): RetrievedRow[] {
    return this.#call('retrieve', { ...opts, format: 'rows' }).rows.map(([id, ref, score, hops, card]) => ({
      id: id as number,
      ref: ref as string,
      score: score as number,
      hops: hops as number,
      card: card as string,
    }))
  }

  /** Up to `k` (3) shortest paths between two nodes, with the nodes on them. */
  paths(from: NodeRef, to: NodeRef, opts: PathsOptions = {}): ContextText {
    const r = this.#call('paths', { from, to, ...opts })
    const [text, tokens] = r.rows[0] as [string, number]
    return { text, tokens, refs: refs(text), message: r.message }
  }

  /** A compact schema card: labels, property types, relationship patterns. */
  schema(sample?: number): string {
    return first(this.#call('schema', { sample }).rows) as string
  }
}
