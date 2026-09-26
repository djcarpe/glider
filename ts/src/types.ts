/**
 * Result types mirroring glider's typed JSON API (`src/api.rs`).
 *
 * A note on ids. glider ids are `u64`. They arrive here through JSON, so they
 * land as JavaScript numbers and are exact only below 2^53. That is ~9
 * quadrillion nodes, well past the point where a memory-resident graph stops
 * fitting in RAM, so it is not a practical limit — but it is why `id` is
 * `number` here rather than `bigint`.
 */

/** A property value. glider's Value is flat: no nested objects. */
export type PropValue = null | boolean | number | string | Array<null | boolean | number | string>

export interface GliderNode {
  readonly _e: 'node'
  readonly id: number
  readonly labels: string[]
  /** Relationships in either direction. Lets a list show connectivity before expanding. */
  readonly degree: number
  readonly props: Record<string, PropValue>
}

export interface GliderRel {
  readonly _e: 'rel'
  readonly id: number
  readonly type: string
  readonly from: number
  readonly to: number
  readonly props: Record<string, PropValue>
}

/** One cell of a result row: a scalar, or a fully typed entity. */
export type Cell = PropValue | GliderNode | GliderRel

/** The drawable projection: every entity in the result, deduplicated. */
export interface GraphPayload {
  readonly nodes: GliderNode[]
  readonly edges: GliderRel[]
}

export interface QueryResult {
  readonly columns: string[]
  readonly rows: Cell[][]
  readonly graph: GraphPayload
  /** Server-side execution time. Always 0 under wasm, which has no clock. */
  readonly ms: number
  readonly message?: string
  readonly touched: number
}

export interface SchemaEntry {
  readonly name: string
  readonly count: number
}

export interface Schema {
  /** True totals; label counts overlap when a node carries several. */
  readonly nodes: number
  readonly edges: number
  readonly labels: SchemaEntry[]
  readonly edge_types: SchemaEntry[]
  readonly indexes: SchemaEntry[]
  /**
   * Property keys seen on a sample of each label's nodes, sorted. The `""`
   * entry samples nodes regardless of label. A hint, not a census.
   */
  readonly node_keys: Readonly<Record<string, string[]>>
  /** As `node_keys`, per relationship type. */
  readonly edge_keys: Readonly<Record<string, string[]>>
}

/** Options for paging through the graph with `nodes()` / `edges()`. */
export interface PageOptions {
  /** Restrict to one label (`nodes`) or relationship type (`edges`). */
  readonly label?: string
  readonly type?: string
  /** Case-insensitive text matched against labels/type, property values and the id. */
  readonly q?: string
  /** Id cursor: the `next` of the previous page. Omit for the first page. */
  readonly from?: number
  /** Page size, 1–1000. Default 50. */
  readonly limit?: number
}

export interface NodePage {
  readonly nodes: GliderNode[]
  /** Cursor for the next page, or null on the last one. */
  readonly next: number | null
  /** Candidates before the text filter: the label's size, or the whole graph. */
  readonly total: number
}

export interface EdgePage {
  readonly edges: GliderRel[]
  /** The endpoints of every edge on the page. */
  readonly nodes: GliderNode[]
  readonly next: number | null
  readonly total: number
}

/** Narrow a cell to a node. */
export function isNode(c: Cell): c is GliderNode {
  return typeof c === 'object' && c !== null && !Array.isArray(c) && (c as GliderNode)._e === 'node'
}

/** Narrow a cell to a relationship. */
export function isRel(c: Cell): c is GliderRel {
  return typeof c === 'object' && c !== null && !Array.isArray(c) && (c as GliderRel)._e === 'rel'
}

/** Thrown for any failure reported by the engine. */
export class GliderError extends Error {
  constructor(message: string, readonly query?: string) {
    super(message)
    this.name = 'GliderError'
  }
}
