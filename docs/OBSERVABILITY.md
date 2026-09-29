# Observability

glider runs in three kinds of host: natively (the CLI, `glider serve`, the C
ABI, a Rust crate), as a wasm module under JavaScript, and as a NIF inside the
BEAM (`glider_ex`). Each already has an observability stack, so the engine
doesn't impose one. It keeps the numbers itself, std only like everything
else, and each host exports them the way it normally would. The metric and
attribute names are the same in every runtime, so a single dashboard covers
all of them.

## What the engine records

`src/telemetry.rs`, on every target:

* **Process counters.** Statements by operation and outcome, rows returned,
  entities touched, and page-cache traffic, plus a statement-duration
  histogram. These are relaxed atomics.
* **A report per statement** (`OpReport`): the operation, the procedure for
  `CALL`, rows, touched, page reads/writes/hits/misses, the duration where the
  target has a clock, and the error. `query::execute_with` records one for
  every statement, including ones that fail to parse, and keeps the latest one
  per thread.
* **Per-database state** (`DbMetrics`): node and edge counts, size, the
  memory limit, cache residency, allocated pages, the log a crash would
  replay, and the cumulative cache, I/O, commit, rollback and checkpoint
  counters.

Renderers produce OTLP/HTTP JSON (`otlp_metrics_json`, `otlp_traces_json`) and
Prometheus text (`prometheus`) from one list of metrics, so the names can't
drift between formats.

The cost is about 90 ns per statement: a monotonic clock read, two
uncontended lock acquisitions for the page counters, and a few atomic adds.
On a 2.4 µs cached point lookup that's under 4%. It's noise for anything that
touches storage.

## Names

Spans follow the OpenTelemetry database conventions, with a `glider.` prefix
for the rest.

| span | kind | when |
|---|---|---|
| `glider <OP>` (`glider MATCH`, `glider CALL`, ...) | client | every statement |
| `<METHOD> <route>` (`POST /api/query`) | server | every `glider serve` request |
| `glider transaction`, `glider checkpoint`, `glider import`, `glider export` | client | glider_ex |
| `glider expand`, `glider nodes`, `glider edges`, `glider import`, `glider export` | client | wasm |

| attribute | on |
|---|---|
| `db.system.name` = `glider` | statement spans |
| `db.operation.name`: `MATCH` `CREATE` `CALL` `INDEX` `EXPLAIN` `STATS` `SCHEMA` `COMPACT` `CLEAR` `BEGIN` `COMMIT` `ROLLBACK` `HELP` `INVALID` | statement spans, `glider.queries` |
| `db.query.text` (the statement, capped at 2 KiB; parameter values are never included) | statement spans |
| `db.response.returned_rows` | statement spans |
| `db.stored_procedure.name` (the algorithm a `CALL` ran) | statement spans |
| `glider.touched`, `glider.page.reads`, `glider.page.writes`, `glider.page.hits`, `glider.page.misses` | statement spans |
| `glider.db` (the file path, or `:memory:<n>`) | statement spans, per-database metrics |
| `glider.outcome`: `ok` or `error` | `glider.queries` |

A failed statement's span has status ERROR with the engine's message.

| metric (OTLP) | Prometheus | type |
|---|---|---|
| `glider.queries` | `glider_queries_total` | counter, by `db.operation.name`, `glider.outcome` |
| `glider.query.duration` (s) | `glider_query_duration_seconds` | histogram, 10 µs to 10 s |
| `glider.rows` | `glider_rows_total` | counter |
| `glider.touched` | `glider_touched_total` | counter |
| `glider.page.{reads,writes,hits,misses}` | `glider_page_*_total` | counters, during statements |
| `glider.db.nodes`, `glider.db.edges` | `glider_db_nodes`, `glider_db_edges` | gauges, by `glider.db` |
| `glider.db.size` (By) | `glider_db_size_bytes` | gauge |
| `glider.db.memory.limit` (By) | `glider_db_memory_limit_bytes` | gauge, in-memory graphs with a limit |
| `glider.db.log.size` (By) | `glider_db_log_size_bytes` | gauge |
| `glider.db.cache.resident`, `glider.db.pages.allocated` | `glider_db_cache_resident`, `glider_db_pages_allocated` | gauges |
| `glider.db.cache.{hits,misses,evictions}` | `glider_db_cache_*_total` | counters since open |
| `glider.db.io.{reads,writes}` | `glider_db_io_*_total` | counters since open |
| `glider.db.{commits,rollbacks,checkpoints}` | `glider_db_*_total` | counters since open |

## Native: CLI, `glider serve`, the C ABI, Rust

The engine includes an OTLP/HTTP JSON exporter, written against `std::net`
with a background thread. It is configured by the standard environment
variables:

| variable | meaning |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | collector base URL; `/v1/traces` and `/v1/metrics` are appended |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` | full URL per signal |
| `OTEL_EXPORTER_OTLP_HEADERS` | `key=value,...` sent with every request (percent-decoded) |
| `OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES` | the resource |
| `OTEL_TRACES_EXPORTER=none`, `OTEL_METRICS_EXPORTER=none` | turn a signal off |
| `OTEL_SDK_DISABLED=true` | turn everything off |
| `OTEL_METRIC_EXPORT_INTERVAL` | ms between metric pushes; default 60000 |
| `OTEL_BSP_SCHEDULE_DELAY` | ms between span batches; default 5000 |
| `GLIDER_OTEL_QUERY_TEXT=false` | leave `db.query.text` off spans |

It supports `http://` only, because there's no TLS in std. Point it at a local
collector or agent, which is how OTLP is normally deployed anyway. The
payload is JSON rather than protobuf; every OTLP/HTTP receiver accepts both.
If the collector goes away, the first failure goes to stderr and later ones
are only counted. A full span queue drops spans rather than blocking a
statement.

With the exporter installed, every statement is a span. Its parent is the
calling thread's trace context if one is set; otherwise it starts a new trace.

* **`glider` (CLI).** Installs the exporter from the environment on start and
  flushes it on exit.

      OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 glider social.gldb serve

* **`glider serve`.** Reads each request's W3C `traceparent` header. Each
  request becomes a server span under it, and the request's statements become
  children of that span. `GET /metrics` serves Prometheus text for scraping,
  with or without an exporter.
* **Query responses.** `POST /api/query`, and `glider_query_json` in the C and
  wasm builds, include the statement's report as `op`: `{op, rows, touched,
  page_reads, page_writes, page_hits, page_misses, duration_ns}`. The browser
  console shows it under each result as "24 pages · 100% cached".
* **The C ABI** (`include/glider.h`):

      glider_telemetry_start("my-app");        /* 1 running, 0 not configured, -1 bad config */
      glider_trace_context(incoming_traceparent);
      char *r = glider_query(db, q);            /* a span, child of that context */
      glider_trace_context(NULL);
      ...
      glider_telemetry_flush();                 /* before exit */

  `glider_metrics_prometheus` and `glider_metrics_otlp` render on demand for
  hosts that prefer to pull. `glider_last_op_json` returns the report on the
  calling thread's last statement, for annotating a span the host made itself.
* **Rust.** `glider::telemetry::otlp::install_from_env("svc")`,
  `telemetry::set_context(TraceContext::parse(header))`, `telemetry::last_op()`,
  `telemetry::snapshot()`, `Graph::telemetry()`.

## wasm (`ts/`)

The wasm module has no clock, no sockets and no imports, and it keeps it that
way. The TypeScript wrapper times each call, feeds the time into the engine's
histogram (`glider_observe_duration_ms`), and reads the engine's report for
the statement. It then hands both to OpenTelemetry if you pass it a tracer and
a meter:

```ts
import { trace, metrics } from '@opentelemetry/api'

const glider = await loadGlider(undefined, {
  telemetry: { tracer: trace.getTracer('glider'), meter: metrics.getMeter('glider') },
})
```

Spans parent under the active context, so an app span around the call is the
parent. The tracer and meter are typed structurally, which keeps the package
free of runtime dependencies. Without an SDK, `glider.exportOtlp({ endpoint })`
POSTs the engine's own OTLP metrics with `fetch`, and `glider.prometheus()`,
`glider.otlpMetrics()`, `glider.telemetry()`, `glider.lastOp()` and
`db.metrics()` are available to pull from. Details are in `ts/README.md`.

## BEAM (`glider_ex`)

`Glider` emits `:telemetry` span events: `[:glider, :query | :transaction |
:checkpoint | :import | :export, :start | :stop | :exception]`. The engine's
report arrives as `:stop` measurements and metadata.

* `Glider.OpenTelemetry.setup/0` turns the events into OpenTelemetry spans. A
  span's parent is the calling process's current span, and inside a
  `Glider.transaction/2` it's the transaction's span.
* `Glider.Telemetry` has the event catalogue, `snapshot/0`, `metrics/1`,
  `prometheus/1` and `otlp_metrics/2`. It also has `emit_db_metrics/1` for
  `:telemetry_poller`, and `start_exporter/1`, which runs the engine's native
  exporter above from inside the NIF. That exporter sends metrics only by
  default, since the BEAM's own SDK does the tracing with the right parents.

The engine's report crosses the NIF boundary in-band with the result, not
through the per-thread "last statement" slot, because consecutive NIF calls
can run on different dirty scheduler threads.

## Trying it

`bench/lgtm/up.sh` starts Grafana's LGTM image (collector, Tempo, Prometheus,
Loki) on localhost:

```sh
bench/lgtm/up.sh
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318 OTEL_METRIC_EXPORT_INTERVAL=5000 \
  glider /tmp/demo.gldb serve
curl -H 'traceparent: 00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01' \
  --data 'CREATE (:Person {name:"Ada"})' localhost:7878/api/query
```

In Grafana, Explore → Tempo shows trace `0af76519…` with `POST /api/query` →
`glider CREATE`, and Explore → Prometheus shows `glider_*`.

`tests/telemetry.rs` covers the native path against a fake collector.
`ts/test` and `glider_ex/test/telemetry_test.exs` cover the other two
runtimes, with the latter using the real OpenTelemetry SDK.
