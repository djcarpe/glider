/**
 * Telemetry for the wasm build.
 *
 * The engine keeps the numbers — statement counters, a duration histogram,
 * per-statement reports, per-database state — but under wasm it has no clock
 * and no sockets (the module has no imports at all). So this side of the
 * boundary times each call, reads the engine's report for it, and hands both
 * to OpenTelemetry:
 *
 *   import { trace, metrics } from '@opentelemetry/api'
 *   const glider = await loadGlider(undefined, {
 *     telemetry: { tracer: trace.getTracer('glider'), meter: metrics.getMeter('glider') },
 *   })
 *
 * Nothing here imports `@opentelemetry/api`: the tracer and meter are typed
 * structurally, so any object with that shape works and the package keeps no
 * runtime dependencies. Without an SDK at all, `exportOtlp` POSTs the
 * engine's own OTLP/HTTP JSON metrics to a collector with `fetch`.
 *
 * Metric and attribute names match every other glider runtime; see
 * docs/OBSERVABILITY.md.
 */

/** Attribute values, as `@opentelemetry/api` accepts them. */
export type AttrValue = string | number | boolean
export type Attributes = Record<string, AttrValue>

/** The part of an OpenTelemetry `Span` glider uses. */
export interface SpanLike {
  setAttributes(attributes: Attributes): unknown
  /** Used when present, to name a statement's span by its operation. */
  updateName?(name: string): unknown
  setStatus(status: { code: number; message?: string }): unknown
  recordException(exception: Error | string): unknown
  end(): void
}

/** The part of an OpenTelemetry `Tracer` glider uses. `startSpan` parents the
 *  span under the active context, as the OpenTelemetry API does. */
export interface TracerLike {
  startSpan(name: string, options?: { kind?: number; attributes?: Attributes }): SpanLike
}

export interface ObservableResultLike {
  observe(value: number, attributes?: Attributes): void
}
export interface ObservableLike {
  addCallback(cb: (result: ObservableResultLike) => void): void
}
export interface HistogramLike {
  record(value: number, attributes?: Attributes): void
}

/** The part of an OpenTelemetry `Meter` glider uses. */
export interface MeterLike {
  createHistogram(name: string, options?: InstrumentOptions): HistogramLike
  createObservableCounter(name: string, options?: InstrumentOptions): ObservableLike
  createObservableGauge(name: string, options?: InstrumentOptions): ObservableLike
}

export interface InstrumentOptions {
  unit?: string
  description?: string
  advice?: { explicitBucketBoundaries?: number[] }
}

export interface TelemetryOptions {
  /** Each call becomes a span, a child of whatever span is active. */
  tracer?: TracerLike
  /** Counters, gauges and a duration histogram, read from the engine. */
  meter?: MeterLike
  /** Record statements as `db.query.text`. Default true. */
  queryText?: boolean
}

/** What one statement did, from the engine (`glider_last_op_json`). */
export interface OpReport {
  readonly op: string
  readonly procedure?: string
  readonly rows: number
  readonly touched: number
  readonly page_reads: number
  readonly page_writes: number
  readonly page_hits: number
  readonly page_misses: number
  /** Absent under wasm; the host measures it. */
  readonly duration_ns?: number
  readonly error?: string
}

/** Process-wide counters (`glider_telemetry_json`). */
export interface TelemetrySnapshot {
  readonly queries: { op: string; ok: number; error: number }[]
  readonly rows: number
  readonly touched: number
  readonly page_reads: number
  readonly page_writes: number
  readonly page_hits: number
  readonly page_misses: number
  readonly duration: {
    count: number
    sum_ns: number
    bounds_s: number[]
    buckets: number[]
  }
  readonly start_unix_ns: number
}

/** One database's state (`glider_db_metrics_json`). */
export interface DbMetrics {
  /** How telemetry names this graph (`glider.db`). */
  readonly name: string
  readonly nodes: number
  readonly edges: number
  readonly bytes: number
  readonly memory_limit: number | null
  readonly page_size: number
  readonly resident_pages: number
  readonly allocated_pages: number
  readonly log_bytes: number
  readonly page_reads: number
  readonly page_writes: number
  readonly page_hits: number
  readonly page_misses: number
  readonly evictions: number
  readonly commits: number
  readonly rollbacks: number
  readonly checkpoints: number
}

/** OpenTelemetry span kinds and status codes, without importing the API. */
export const SPAN_KIND_CLIENT = 2
export const STATUS_OK = 1
export const STATUS_ERROR = 2

/** Histogram bounds, in seconds — the same as the engine's. */
export const DURATION_BOUNDS = [
  0.00001, 0.00005, 0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25,
  0.5, 1, 10,
]

/** Longest `db.query.text` recorded, in characters. */
export const MAX_QUERY_TEXT = 2048

/** Span attributes for a statement: the same names every runtime uses. */
export function queryAttributes(r: OpReport, text?: string): Attributes {
  const a: Attributes = {
    'db.system.name': 'glider',
    'db.operation.name': r.op,
    'db.response.returned_rows': r.rows,
    'glider.touched': r.touched,
    'glider.page.reads': r.page_reads,
    'glider.page.writes': r.page_writes,
    'glider.page.hits': r.page_hits,
    'glider.page.misses': r.page_misses,
  }
  if (r.procedure) a['db.stored_procedure.name'] = r.procedure
  if (text !== undefined) a['db.query.text'] = text.slice(0, MAX_QUERY_TEXT)
  return a
}

/** Where `exportOtlp` sends metrics, and how. */
export interface OtlpExportOptions {
  /** Collector base URL, e.g. `http://localhost:4318`; `/v1/metrics` is appended. */
  endpoint: string
  /** `service.name`. Default `glider`. */
  service?: string
  /** Extra request headers, e.g. authorization. */
  headers?: Record<string, string>
  /** A `fetch` to use; defaults to the global one. */
  fetch?: typeof fetch
}

/** Wall-clock milliseconds. */
export function nowMs(): number {
  return typeof performance !== 'undefined' && typeof performance.timeOrigin === 'number'
    ? performance.timeOrigin + performance.now()
    : Date.now()
}

/** A monotonic millisecond timer. */
export function monoMs(): number {
  return typeof performance !== 'undefined' ? performance.now() : Date.now()
}
