/**
 * glider for TypeScript — the whole database compiled to WebAssembly.
 *
 *   import { loadGlider } from '@glider/wasm'
 *
 *   const glider = await loadGlider()
 *   const db = glider.open()
 *   db.run('CREATE (a:Person {name:"Ada"})-[:KNOWS]->(b:Person {name:"Bob"})')
 *   const r = db.query('MATCH (a)-[r]->(b) RETURN a, r, b')
 *   db.close()
 *
 * The module has **no imports** — no WASI, no JS glue injected by a bindgen.
 * That falls out of glider being std-only: there is nothing in it that wants
 * an operating system. The consequence is that it runs unchanged in Node,
 * Deno, Bun, browsers, and edge runtimes.
 *
 * Telemetry: pass an OpenTelemetry tracer and meter to `loadGlider` and every
 * call becomes a span with the engine's per-statement report on it, and the
 * engine's counters become metrics. Or push the engine's own OTLP metrics
 * with `exportOtlp`. See telemetry.ts.
 *
 * What does not work under wasm: anything file-backed. `wasm32-unknown-unknown`
 * has no filesystem, so graphs are in-memory only. Persist by exporting JSONL
 * and storing that yourself (IndexedDB, OPFS, a fetch to your server).
 */

import {
  GliderError,
  type Cell,
  type EdgePage,
  type GliderNode,
  type GliderRel,
  type NodePage,
  type PageOptions,
  type QueryResult,
  type Schema,
} from './types.js'

import {
  DURATION_BOUNDS,
  SPAN_KIND_CLIENT,
  STATUS_ERROR,
  monoMs,
  nowMs,
  queryAttributes,
  type Attributes,
  type DbMetrics,
  type HistogramLike,
  type MeterLike,
  type OpReport,
  type OtlpExportOptions,
  type TelemetryOptions,
  type TelemetrySnapshot,
  type TracerLike,
} from './telemetry.js'

export * from './types.js'
export * from './telemetry.js'

/** Options for `loadGlider`. */
export interface LoadOptions {
  telemetry?: TelemetryOptions
}

/** The raw exports glider's wasm module provides. */
interface Exports {
  memory: WebAssembly.Memory
  glider_open_memory(): number
  glider_open_bytes(bytes: number, len: number): number
  glider_close(db: number): void
  glider_query(db: number, q: number): number
  glider_query_json(db: number, q: number): number
  glider_schema_json(db: number): number
  glider_expand_json(db: number, id: bigint, limit: number): number
  glider_nodes_json(db: number, label: number, q: number, from: bigint, limit: number): number
  glider_edges_json(db: number, etype: number, q: number, from: bigint, limit: number): number
  glider_import_jsonl(db: number, jsonl: number): number
  glider_export_jsonl(db: number): number
  glider_stats(db: number): number
  glider_compact(db: number): number
  glider_last_error(): number
  glider_free(p: number): void
  glider_alloc(len: number): number
  glider_dealloc(p: number, len: number): void
  glider_version(): number
  glider_telemetry_json(): number
  glider_last_op_json(): number
  glider_db_metrics_json(db: number): number
  glider_metrics_otlp(dbs: number, n: number, service: number, nowUnixMs: number): number
  glider_metrics_prometheus(dbs: number, n: number): number
  glider_observe_duration_ms(ms: number): void
}

/** Anything we know how to turn into wasm bytes. */
export type WasmSource =
  | BufferSource
  | WebAssembly.Module
  | Response
  | Promise<Response>
  | URL
  | string

/**
 * Compile and instantiate the glider wasm module.
 *
 * With no argument it looks for `glider.wasm` next to this file, which is how
 * the published package is laid out. Pass a source explicitly when bundling,
 * or when serving the binary from somewhere else.
 */
export async function loadGlider(source?: WasmSource, options: LoadOptions = {}): Promise<GliderModule> {
  const instance = await instantiate(source ?? new URL('./glider.wasm', import.meta.url))
  return new GliderModule(instance.exports as unknown as Exports, options)
}

async function instantiate(source: WasmSource): Promise<WebAssembly.Instance> {
  const imports: WebAssembly.Imports = {}

  if (source instanceof WebAssembly.Module) {
    return new WebAssembly.Instance(source, imports)
  }
  if (isBufferSource(source)) {
    return (await WebAssembly.instantiate(source, imports)).instance
  }

  // A URL or path. In a browser or Deno, stream it. In Node, read it off disk,
  // because file: URLs cannot be fetched and streaming compilation would need
  // a Response we do not have.
  if (source instanceof URL || typeof source === 'string') {
    const url = source instanceof URL ? source : new URL(source, import.meta.url)
    if (url.protocol === 'file:') {
      const { readFile } = await import('node:fs/promises')
      const bytes = await readFile(url)
      return (await WebAssembly.instantiate(bytes, imports)).instance
    }
    return instantiateResponse(fetch(url), imports)
  }

  return instantiateResponse(source, imports)
}

async function instantiateResponse(
  res: Response | Promise<Response>,
  imports: WebAssembly.Imports,
): Promise<WebAssembly.Instance> {
  // instantiateStreaming needs the right Content-Type and is not universally
  // available; fall back to buffering rather than failing.
  if (typeof WebAssembly.instantiateStreaming === 'function') {
    try {
      return (await WebAssembly.instantiateStreaming(res, imports)).instance
    } catch {
      /* fall through */
    }
  }
  const bytes = await (await res).arrayBuffer()
  return (await WebAssembly.instantiate(bytes, imports)).instance
}

function isBufferSource(v: unknown): v is BufferSource {
  return v instanceof ArrayBuffer || ArrayBuffer.isView(v as ArrayBufferView)
}

/** An instantiated module. Cheap to keep; each `open()` is an isolated graph. */
export class GliderModule {
  readonly #e: Exports
  readonly #dec = new TextDecoder()
  readonly #enc = new TextEncoder()
  /** Open graphs, for per-database metrics. */
  readonly #open = new Set<GliderDb>()
  readonly #tracer: TracerLike | undefined
  readonly #duration: HistogramLike | undefined
  readonly #queryText: boolean
  /** Metrics read for the current collection; dropped when graphs come and go. */
  #cached: { at: number; snap: TelemetrySnapshot; dbs: [string, DbMetrics][] } | undefined

  constructor(exports: Exports, options: LoadOptions = {}) {
    this.#e = exports
    const t = options.telemetry ?? {}
    this.#tracer = t.tracer
    this.#queryText = t.queryText ?? true
    if (t.meter) {
      this.#duration = t.meter.createHistogram('glider.query.duration', {
        unit: 's',
        description: 'Statement duration.',
        advice: { explicitBucketBoundaries: DURATION_BOUNDS },
      })
      this.#observe(t.meter)
    }
  }

  // ---- telemetry -----------------------------------------------------

  /** Process-wide counters: statements by operation and outcome, rows,
   *  pages, and the duration histogram (fed by this wrapper's timings). */
  telemetry(): TelemetrySnapshot {
    return JSON.parse(this.take(this.#e.glider_telemetry_json()) ?? '{}') as TelemetrySnapshot
  }

  /** The engine's report on the most recent statement. */
  lastOp(): OpReport | null {
    const j = this.take(this.#e.glider_last_op_json())
    return j === null ? null : (JSON.parse(j) as OpReport)
  }

  /** Process counters and every open graph's state as an OTLP/HTTP JSON
   *  metrics request, ready to POST to `<collector>/v1/metrics`. */
  otlpMetrics(service = 'glider'): string {
    const out = this.#withHandles((p, n) =>
      this.withCString(service, (sp) => this.take(this.#e.glider_metrics_otlp(p, n, sp, nowMs()))),
    )
    if (out === null) throw new GliderError(this.#lastError() ?? 'could not render metrics')
    return out
  }

  /** The same in the Prometheus text format. */
  prometheus(): string {
    const out = this.#withHandles((p, n) => this.take(this.#e.glider_metrics_prometheus(p, n)))
    if (out === null) throw new GliderError(this.#lastError() ?? 'could not render metrics')
    return out
  }

  /**
   * POST `otlpMetrics()` to an OTLP/HTTP collector — metrics without an
   * OpenTelemetry SDK. Call it on an interval; the counters are cumulative.
   */
  async exportOtlp(opts: OtlpExportOptions): Promise<void> {
    const f = opts.fetch ?? fetch
    const url = opts.endpoint.replace(/\/+$/, '') + '/v1/metrics'
    const res = await f(url, {
      method: 'POST',
      headers: { 'content-type': 'application/json', ...(opts.headers ?? {}) },
      body: this.otlpMetrics(opts.service ?? 'glider'),
    })
    if (!res.ok) throw new GliderError(`OTLP export to ${url} failed: ${res.status}`)
  }

  /** Every open graph's handle, as a u32 array in wasm memory. */
  #withHandles<T>(fn: (ptr: number, n: number) => T): T {
    const handles = [...this.#open].map((db) => db.handle).filter((h) => h !== 0)
    if (handles.length === 0) return fn(0, 0)
    const len = handles.length * 4
    const ptr = this.#e.glider_alloc(len)
    if (!ptr) throw new GliderError(`could not allocate ${len} bytes in wasm memory`)
    try {
      const view = new DataView(this.#e.memory.buffer)
      handles.forEach((h, i) => view.setUint32(ptr + 4 * i, h, true))
      return fn(ptr, handles.length)
    } finally {
      this.#e.glider_dealloc(ptr, len)
    }
  }

  /** Observable instruments over the engine's counters and open graphs. */
  #observe(meter: MeterLike): void {
    // One snapshot per collection, shared by every callback in it.
    const read = () => {
      const at = monoMs()
      let cached = this.#cached
      if (!cached || at - cached.at > 50) {
        cached = this.#cached = {
          at,
          snap: this.telemetry(),
          dbs: [...this.#open].filter((d) => d.handle !== 0).map((d) => {
            const m = d.metrics()
            return [m.name, m]
          }),
        }
      }
      return cached
    }
    const counter = (name: string, unit: string, description: string, v: (s: TelemetrySnapshot) => number) =>
      meter.createObservableCounter(name, { unit, description }).addCallback((r) => r.observe(v(read().snap)))
    meter
      .createObservableCounter('glider.queries', { unit: '{statement}', description: 'Statements executed.' })
      .addCallback((r) => {
        for (const q of read().snap.queries) {
          if (q.ok) r.observe(q.ok, { 'db.operation.name': q.op, 'glider.outcome': 'ok' })
          if (q.error) r.observe(q.error, { 'db.operation.name': q.op, 'glider.outcome': 'error' })
        }
      })
    counter('glider.rows', '{row}', 'Rows returned by statements.', (s) => s.rows)
    counter('glider.touched', '{entity}', 'Nodes and relationships written by statements.', (s) => s.touched)
    counter('glider.page.reads', '{page}', 'Pages read from storage by statements.', (s) => s.page_reads)
    counter('glider.page.writes', '{page}', 'Pages written to storage by statements.', (s) => s.page_writes)
    counter('glider.page.hits', '{page}', 'Page-cache hits during statements.', (s) => s.page_hits)
    counter('glider.page.misses', '{page}', 'Page-cache misses during statements.', (s) => s.page_misses)

    const perDb = (
      kind: 'gauge' | 'counter',
      name: string,
      unit: string,
      description: string,
      v: (d: DbMetrics) => number | null,
    ) => {
      const inst =
        kind === 'gauge'
          ? meter.createObservableGauge(name, { unit, description })
          : meter.createObservableCounter(name, { unit, description })
      inst.addCallback((r) => {
        for (const [db, m] of read().dbs) {
          const x = v(m)
          if (x !== null) r.observe(x, { 'glider.db': db })
        }
      })
    }
    perDb('gauge', 'glider.db.nodes', '{node}', 'Nodes in the database.', (d) => d.nodes)
    perDb('gauge', 'glider.db.edges', '{relationship}', 'Relationships in the database.', (d) => d.edges)
    perDb('gauge', 'glider.db.size', 'By', 'Bytes the database occupies.', (d) => d.bytes)
    perDb('gauge', 'glider.db.memory.limit', 'By', 'Memory limit of an in-memory database.', (d) => d.memory_limit)
    perDb('gauge', 'glider.db.log.size', 'By', 'Write-ahead log a crash would replay.', (d) => d.log_bytes)
    perDb('gauge', 'glider.db.cache.resident', '{page}', 'Pages resident in the cache.', (d) => d.resident_pages)
    perDb('gauge', 'glider.db.pages.allocated', '{page}', 'Pages in use.', (d) => d.allocated_pages)
    perDb('counter', 'glider.db.cache.hits', '{page}', 'Page-cache hits since open.', (d) => d.page_hits)
    perDb('counter', 'glider.db.cache.misses', '{page}', 'Page-cache misses since open.', (d) => d.page_misses)
    perDb('counter', 'glider.db.cache.evictions', '{page}', 'Pages evicted since open.', (d) => d.evictions)
    perDb('counter', 'glider.db.io.reads', '{page}', 'Pages read from storage since open.', (d) => d.page_reads)
    perDb('counter', 'glider.db.io.writes', '{page}', 'Pages written to storage since open.', (d) => d.page_writes)
    perDb('counter', 'glider.db.commits', '{transaction}', 'Transactions committed since open.', (d) => d.commits)
    perDb('counter', 'glider.db.rollbacks', '{transaction}', 'Transactions rolled back since open.', (d) => d.rollbacks)
    perDb('counter', 'glider.db.checkpoints', '{checkpoint}', 'Checkpoints since open.', (d) => d.checkpoints)
  }

  /**
   * Run one call against the engine under telemetry: time it, feed the
   * engine's histogram (it has no clock under wasm), and — with a tracer or
   * meter — record a span and a duration from the engine's report.
   * `statement` is the text for calls that run one, which have a report.
   * @internal
   */
  instrument<T>(name: string, statement: string | undefined, fn: () => T): T {
    const tracer = this.#tracer
    const span = tracer?.startSpan(`glider ${name}`, {
      kind: SPAN_KIND_CLIENT,
      attributes: { 'db.system.name': 'glider' },
    })
    const t0 = monoMs()
    let failure: unknown
    try {
      return fn()
    } catch (e) {
      failure = e
      throw e
    } finally {
      const ms = monoMs() - t0
      let attrs: Attributes = { 'db.system.name': 'glider', 'db.operation.name': name }
      if (statement !== undefined) {
        this.#e.glider_observe_duration_ms(ms)
        const r = span || this.#duration ? this.lastOp() : null
        if (r) attrs = queryAttributes(r, this.#queryText ? statement : undefined)
      }
      this.#duration?.record(ms / 1000, {
        'db.system.name': 'glider',
        'db.operation.name': attrs['db.operation.name'] ?? name,
        'glider.outcome': failure === undefined ? 'ok' : 'error',
      })
      if (span) {
        // `glider MATCH`, like the other runtimes, once the engine has said.
        if (statement !== undefined && attrs['db.operation.name'] !== name) {
          span.updateName?.(`glider ${attrs['db.operation.name']}`)
        }
        span.setAttributes(attrs)
        if (failure !== undefined) {
          const msg = failure instanceof Error ? failure.message : String(failure)
          span.recordException(failure instanceof Error ? failure : msg)
          span.setStatus({ code: STATUS_ERROR, message: msg })
        }
        span.end()
      }
    }
  }

  /** @internal */
  opened(db: GliderDb): void {
    this.#open.add(db)
    this.#cached = undefined
  }

  /** @internal */
  closed(db: GliderDb): void {
    this.#open.delete(db)
    this.#cached = undefined
  }

  /** glider's version string. */
  get version(): string {
    return this.#borrow(this.#e.glider_version()) ?? 'unknown'
  }

  /** Open a fresh in-memory graph. */
  open(): GliderDb {
    const handle = this.#e.glider_open_memory()
    if (!handle) throw new GliderError(this.#lastError() ?? 'could not open a graph')
    const db = new GliderDb(this, handle)
    this.opened(db)
    return db
  }

  /**
   * Open a graph from the bytes of a `.gldb` database file — read with
   * `File.arrayBuffer()`, `fetch`, or `fs.readFile`. The graph is in memory:
   * edits are not written back to the file. Persist with `exportJsonl()`.
   */
  openBytes(bytes: ArrayBuffer | ArrayBufferView): GliderDb {
    const src =
      bytes instanceof ArrayBuffer
        ? new Uint8Array(bytes)
        : new Uint8Array(bytes.buffer, bytes.byteOffset, bytes.byteLength)
    const len = src.byteLength
    // Allocate at least one byte so an empty file still gets a real pointer;
    // glider then reports it as too short, which is the useful error.
    const cap = Math.max(len, 1)
    const ptr = this.#e.glider_alloc(cap)
    if (!ptr) throw new GliderError(`could not allocate ${len} bytes in wasm memory`)
    let handle: number
    try {
      this.#bytes().set(src, ptr)
      handle = this.#e.glider_open_bytes(ptr, len)
    } finally {
      this.#e.glider_dealloc(ptr, cap)
    }
    if (!handle) throw new GliderError(this.#lastError() ?? 'could not open that file')
    const db = new GliderDb(this, handle)
    this.opened(db)
    return db
  }

  // ---- internals used by GliderDb ------------------------------------

  /** Bytes of linear memory. Re-read every time: growth detaches old views. */
  #bytes(): Uint8Array {
    return new Uint8Array(this.#e.memory.buffer)
  }

  /** Read a NUL-terminated string without taking ownership. */
  #borrow(ptr: number): string | null {
    if (!ptr) return null
    const m = this.#bytes()
    let end = ptr
    while (m[end] !== 0) end++
    return this.#dec.decode(m.subarray(ptr, end))
  }

  /** Read a NUL-terminated string and free it. */
  /** @internal */
  take(ptr: number): string | null {
    const s = this.#borrow(ptr)
    if (ptr) this.#e.glider_free(ptr)
    return s
  }

  /** @internal */
  lastError(): string | null {
    return this.#lastError()
  }

  #lastError(): string | null {
    return this.#borrow(this.#e.glider_last_error())
  }

  /**
   * Copy a string into the module as NUL-terminated UTF-8 and run `fn` with
   * the pointer, always releasing it afterwards.
   */
  /** @internal */
  withCString<T>(s: string, fn: (ptr: number) => T): T {
    const body = this.#enc.encode(s)
    const len = body.length + 1
    const ptr = this.#e.glider_alloc(len)
    if (!ptr) throw new GliderError(`could not allocate ${len} bytes in wasm memory`)
    try {
      // Take the view *after* alloc: growing memory detaches earlier views.
      const m = this.#bytes()
      m.set(body, ptr)
      m[ptr + body.length] = 0
      return fn(ptr)
    } finally {
      this.#e.glider_dealloc(ptr, len)
    }
  }

  /** As `withCString`, but an absent string is passed as NULL. */
  /** @internal */
  withOptCString<T>(s: string | undefined, fn: (ptr: number) => T): T {
    return s === undefined || s === '' ? fn(0) : this.withCString(s, fn)
  }

  /** @internal */
  get raw(): Exports {
    return this.#e
  }
}

/** One graph. Not shared between workers — wasm memory is per-instance. */
export class GliderDb {
  #handle: number

  constructor(
    private readonly mod: GliderModule,
    handle: number,
  ) {
    this.#handle = handle
  }

  #alive(): number {
    if (!this.#handle) throw new GliderError('this graph is closed')
    return this.#handle
  }

  /** Run a query and return the typed result, including the graph payload. */
  query(cypher: string): QueryResult {
    const db = this.#alive()
    const t0 = monoMs()
    const json = this.mod.instrument('query', cypher, () => {
      const j = this.mod.withCString(cypher, (p) => this.mod.take(this.mod.raw.glider_query_json(db, p)))
      if (j === null) throw new GliderError(this.mod.lastError() ?? 'query failed', cypher)
      return j
    })
    const r = JSON.parse(json) as QueryResult
    // The engine has no clock under wasm and reports 0; fill in ours.
    return r.ms ? r : { ...r, ms: monoMs() - t0 }
  }

  /**
   * Run a statement for its effect and return how many entities it touched.
   * Sugar over `query` for writes, where the rows are empty anyway.
   */
  run(cypher: string): number {
    return this.query(cypher).touched
  }

  /** Every node and relationship the query produced, ready to draw. */
  graph(cypher: string): QueryResult['graph'] {
    return this.query(cypher).graph
  }

  /** Labels, relationship types and indexes, with counts. */
  schema(): Schema {
    const db = this.#alive()
    const json = this.mod.instrument('SCHEMA', 'SCHEMA', () => this.mod.take(this.mod.raw.glider_schema_json(db)))
    if (json === null) throw new GliderError(this.mod.lastError() ?? 'schema failed')
    return JSON.parse(json) as Schema
  }

  /** Neighbours of one node, both directions, capped by `limit`. */
  expand(id: number, limit = 50): QueryResult['graph'] {
    const db = this.#alive()
    const json = this.mod.instrument('expand', undefined, () =>
      this.mod.take(this.mod.raw.glider_expand_json(db, BigInt(id), limit)),
    )
    if (json === null) throw new GliderError(this.mod.lastError() ?? `could not expand node ${id}`)
    return (JSON.parse(json) as { graph: QueryResult['graph'] }).graph
  }

  /**
   * A page of nodes, cursor-paged by id. Pass the previous page's `next` as
   * `from` to continue; `q` matches labels, property values and the id.
   */
  nodes(opts: PageOptions = {}): NodePage {
    const db = this.#alive()
    const json = this.mod.instrument('nodes', undefined, () =>
      this.mod.withOptCString(opts.label, (lp) =>
        this.mod.withOptCString(opts.q, (qp) =>
          this.mod.take(
            this.mod.raw.glider_nodes_json(db, lp, qp, BigInt(opts.from ?? 0), opts.limit ?? 50),
          ),
        ),
      ),
    )
    if (json === null) throw new GliderError(this.mod.lastError() ?? 'nodes failed')
    return JSON.parse(json) as NodePage
  }

  /** A page of relationships with their endpoints. Same contract as `nodes`. */
  edges(opts: PageOptions = {}): EdgePage {
    const db = this.#alive()
    const json = this.mod.instrument('edges', undefined, () =>
      this.mod.withOptCString(opts.type, (tp) =>
        this.mod.withOptCString(opts.q, (qp) =>
          this.mod.take(
            this.mod.raw.glider_edges_json(db, tp, qp, BigInt(opts.from ?? 0), opts.limit ?? 50),
          ),
        ),
      ),
    )
    if (json === null) throw new GliderError(this.mod.lastError() ?? 'edges failed')
    return JSON.parse(json) as EdgePage
  }

  /** Bulk load JSON Lines. Returns the number of entities imported. */
  importJsonl(jsonl: string): number {
    const db = this.#alive()
    const rc = this.mod.instrument('import', undefined, () =>
      this.mod.withCString(jsonl, (p) => this.mod.raw.glider_import_jsonl(db, p)),
    )
    if (rc < 0) throw new GliderError(this.mod.lastError() ?? 'import failed')
    return rc
  }

  /**
   * Dump the whole graph as JSON Lines. Under wasm this is how you persist:
   * hand the string to IndexedDB, OPFS, or your own server.
   */
  exportJsonl(): string {
    const db = this.#alive()
    const s = this.mod.instrument('export', undefined, () => this.mod.take(this.mod.raw.glider_export_jsonl(db)))
    if (s === null) throw new GliderError(this.mod.lastError() ?? 'export failed')
    return s
  }

  /** Node, edge, label and index counts. */
  stats(): QueryResult {
    const db = this.#alive()
    const json = this.mod.instrument('STATS', 'STATS', () => this.mod.take(this.mod.raw.glider_stats(db)))
    if (json === null) throw new GliderError(this.mod.lastError() ?? 'stats failed')
    // glider_stats predates the typed API and returns the flat shape, so give
    // it the same surface as everything else rather than leaking the
    // difference to callers.
    const flat = JSON.parse(json) as Omit<QueryResult, 'graph' | 'ms'>
    return { ...flat, graph: { nodes: [], edges: [] }, ms: 0 }
  }

  /** This graph's counts, size, cache and commit counters. */
  metrics(): DbMetrics {
    const json = this.mod.take(this.mod.raw.glider_db_metrics_json(this.#alive()))
    if (json === null) throw new GliderError(this.mod.lastError() ?? 'metrics failed')
    return JSON.parse(json) as DbMetrics
  }

  /** @internal The raw handle, 0 once closed. */
  get handle(): number {
    return this.#handle
  }

  /** Release the graph. Safe to call twice. */
  close(): void {
    if (this.#handle) {
      this.mod.raw.glider_close(this.#handle)
      this.#handle = 0
      this.mod.closed(this)
    }
  }

  /** Lets `using db = glider.open()` work under TS 5.2+ explicit resource management. */
  [Symbol.dispose](): void {
    this.close()
  }
}

export type { Cell, GliderNode, GliderRel }
