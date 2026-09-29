//! Telemetry: one metric model for every runtime glider is built for.
//!
//! glider runs natively (the CLI, `glider serve`, the C ABI), as a wasm module
//! under JavaScript, and as a NIF inside the BEAM. Those hosts have very
//! different observability stacks, so the engine does not pick one. It keeps
//! the numbers — std only, like everything else — and each host exports them
//! through whatever it already uses:
//!
//! * **Counters and a latency histogram**, process-wide: statements by
//!   operation and outcome, rows returned, entities touched, page-cache
//!   traffic. Plain atomics, so recording is a handful of relaxed adds.
//! * **A report per statement** ([`OpReport`]): the operation, rows, pages
//!   read and hit, and the duration where the target has a clock. Hosts read
//!   it after a call to annotate their own span — the wasm module has no
//!   clock and no imports, so the JavaScript side times the call and asks the
//!   engine for the rest.
//! * **Per-database state** ([`DbMetrics`]): counts, cache, log, memory.
//!
//! Renderers turn these into OTLP/HTTP JSON ([`otlp_metrics_json`],
//! [`otlp_traces_json`]) and Prometheus text ([`prometheus`]), so every host
//! emits the same metric names. Natively there is also a small OTLP exporter
//! ([`otlp`]) configured by the standard `OTEL_*` environment variables; while
//! it is installed, every statement becomes a `glider.query` span, parented
//! under the caller's W3C trace context ([`set_context`]).
//!
//! Naming follows the OpenTelemetry database conventions where they apply
//! (`db.system.name`, `db.operation.name`, `db.query.text`) and uses a
//! `glider.` prefix for the rest. docs/OBSERVABILITY.md lists every name.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crate::value::write_json_string;

/// Instrumentation scope name, and the value of `db.system.name`.
pub const SCOPE: &str = "glider";

// ---------------------------------------------------------------- operations

/// Statement kinds, the value of `db.operation.name`. `INVALID` is a
/// statement that failed to parse.
pub const OPS: [&str; 14] = [
    "MATCH", "CREATE", "CALL", "INDEX", "EXPLAIN", "STATS", "SCHEMA", "COMPACT", "CLEAR", "BEGIN",
    "COMMIT", "ROLLBACK", "HELP", "INVALID",
];

fn op_index(op: &str) -> usize {
    OPS.iter().position(|o| *o == op).unwrap_or(OPS.len() - 1)
}

/// Upper bounds, in seconds, of the query-duration histogram buckets. From
/// 10µs (a point lookup served from cache) to 10s (a whole-graph algorithm).
pub const DURATION_BOUNDS: [f64; 16] = [
    0.00001, 0.00005, 0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25,
    0.5, 1.0, 10.0,
];

const NOPS: usize = OPS.len();
const NBUCKETS: usize = DURATION_BOUNDS.len() + 1;

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU64 = AtomicU64::new(0);

#[allow(clippy::declare_interior_mutable_const)]
const ZERO2: [AtomicU64; 2] = [ZERO; 2];
/// `[op][0 = ok, 1 = error]`
static QUERIES: [[AtomicU64; 2]; NOPS] = [ZERO2; NOPS];
static ROWS: AtomicU64 = ZERO;
static TOUCHED: AtomicU64 = ZERO;
static PAGE_READS: AtomicU64 = ZERO;
static PAGE_WRITES: AtomicU64 = ZERO;
static PAGE_HITS: AtomicU64 = ZERO;
static PAGE_MISSES: AtomicU64 = ZERO;
static BUCKETS: [AtomicU64; NBUCKETS] = [ZERO; NBUCKETS];
static DURATION_NS: AtomicU64 = ZERO;
static TIMED: AtomicU64 = ZERO;
/// Unix time the counters started from, for OTLP's cumulative start time.
/// Natively the first statement; under wasm, the first time the host
/// renders metrics and supplies a clock.
static START_NS: AtomicU64 = ZERO;

// ------------------------------------------------------------------- clocks

/// Nanoseconds since the Unix epoch, or `None` where the target has no clock
/// (`wasm32-unknown-unknown` traps on `SystemTime::now`).
#[cfg(not(target_arch = "wasm32"))]
pub fn now_unix_ns() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_nanos() as u64)
}

#[cfg(target_arch = "wasm32")]
pub fn now_unix_ns() -> Option<u64> {
    None
}

/// A monotonic stopwatch that is a no-op without a clock. Reads only the
/// monotonic clock on the hot path; wall time is derived when a span needs it.
#[derive(Clone, Copy)]
pub struct Stopwatch {
    #[cfg(not(target_arch = "wasm32"))]
    start: std::time::Instant,
}

impl Stopwatch {
    pub fn start() -> Stopwatch {
        Stopwatch {
            #[cfg(not(target_arch = "wasm32"))]
            start: std::time::Instant::now(),
        }
    }

    /// Unix nanoseconds at `start()`, where there is a clock: now, less the
    /// time elapsed since.
    pub fn started_unix_ns(&self) -> Option<u64> {
        Some(now_unix_ns()?.saturating_sub(self.elapsed_ns()?))
    }

    pub fn elapsed_ns(&self) -> Option<u64> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            Some(self.start.elapsed().as_nanos() as u64)
        }
        #[cfg(target_arch = "wasm32")]
        {
            None
        }
    }
}

fn note_start(now: u64) {
    if now > 0 {
        let _ = START_NS.compare_exchange(0, now, Relaxed, Relaxed);
    }
}

// ------------------------------------------------------------------ reports

/// What one statement did. The engine records one for every statement run
/// through `query::execute_with`; [`last_op`] returns the calling thread's
/// most recent.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OpReport {
    /// One of [`OPS`].
    pub op: &'static str,
    /// For `CALL`, the procedure (algorithm) name.
    pub procedure: Option<String>,
    pub rows: u64,
    pub touched: u64,
    /// Page-cache traffic during the statement.
    pub page_reads: u64,
    pub page_writes: u64,
    pub page_hits: u64,
    pub page_misses: u64,
    /// Wall time, where the target has a clock.
    pub duration_ns: Option<u64>,
    pub error: Option<String>,
}

thread_local! {
    static LAST: RefCell<Option<OpReport>> = const { RefCell::new(None) };
}

/// The calling thread's most recent statement report.
pub fn last_op() -> Option<OpReport> {
    LAST.with(|l| l.borrow().clone())
}

/// Fold a finished statement into the process counters and keep it as the
/// thread's last report.
pub fn record(r: OpReport) {
    QUERIES[op_index(r.op)][r.error.is_some() as usize].fetch_add(1, Relaxed);
    ROWS.fetch_add(r.rows, Relaxed);
    TOUCHED.fetch_add(r.touched, Relaxed);
    PAGE_READS.fetch_add(r.page_reads, Relaxed);
    PAGE_WRITES.fetch_add(r.page_writes, Relaxed);
    PAGE_HITS.fetch_add(r.page_hits, Relaxed);
    PAGE_MISSES.fetch_add(r.page_misses, Relaxed);
    if let Some(ns) = r.duration_ns {
        observe_duration(ns);
    }
    LAST.with(|l| *l.borrow_mut() = Some(r));
}

/// Add one duration to the histogram. Hosts without a clock inside the
/// engine (wasm) call this with the time they measured around the call, so
/// the histogram is populated on every target.
pub fn observe_duration(ns: u64) {
    let secs = ns as f64 / 1e9;
    let b = DURATION_BOUNDS
        .iter()
        .position(|bound| secs <= *bound)
        .unwrap_or(DURATION_BOUNDS.len());
    BUCKETS[b].fetch_add(1, Relaxed);
    DURATION_NS.fetch_add(ns, Relaxed);
    TIMED.fetch_add(1, Relaxed);
}

impl OpReport {
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(192);
        out.push_str("{\"op\":");
        write_json_string(self.op, &mut out);
        if let Some(p) = &self.procedure {
            out.push_str(",\"procedure\":");
            write_json_string(p, &mut out);
        }
        out.push_str(&format!(
            ",\"rows\":{},\"touched\":{},\"page_reads\":{},\"page_writes\":{},\"page_hits\":{},\"page_misses\":{}",
            self.rows, self.touched, self.page_reads, self.page_writes, self.page_hits, self.page_misses
        ));
        if let Some(ns) = self.duration_ns {
            out.push_str(&format!(",\"duration_ns\":{ns}"));
        }
        if let Some(e) = &self.error {
            out.push_str(",\"error\":");
            write_json_string(e, &mut out);
        }
        out.push('}');
        out
    }
}

// ----------------------------------------------------------------- snapshot

/// The process-wide counters at one instant.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// `(op, ok, errors)` for every op that has run.
    pub queries: Vec<(&'static str, u64, u64)>,
    pub rows: u64,
    pub touched: u64,
    pub page_reads: u64,
    pub page_writes: u64,
    pub page_hits: u64,
    pub page_misses: u64,
    /// Cumulative counts per bucket of [`DURATION_BOUNDS`], plus overflow.
    pub duration_buckets: Vec<u64>,
    pub duration_sum_ns: u64,
    pub duration_count: u64,
    pub start_unix_ns: u64,
}

pub fn snapshot() -> Snapshot {
    let queries = OPS
        .iter()
        .enumerate()
        .filter_map(|(i, op)| {
            let ok = QUERIES[i][0].load(Relaxed);
            let err = QUERIES[i][1].load(Relaxed);
            (ok + err > 0).then_some((*op, ok, err))
        })
        .collect();
    Snapshot {
        queries,
        rows: ROWS.load(Relaxed),
        touched: TOUCHED.load(Relaxed),
        page_reads: PAGE_READS.load(Relaxed),
        page_writes: PAGE_WRITES.load(Relaxed),
        page_hits: PAGE_HITS.load(Relaxed),
        page_misses: PAGE_MISSES.load(Relaxed),
        duration_buckets: BUCKETS.iter().map(|b| b.load(Relaxed)).collect(),
        duration_sum_ns: DURATION_NS.load(Relaxed),
        duration_count: TIMED.load(Relaxed),
        start_unix_ns: START_NS.load(Relaxed),
    }
}

impl Snapshot {
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\"queries\":[");
        for (i, (op, ok, err)) in self.queries.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!("{{\"op\":\"{op}\",\"ok\":{ok},\"error\":{err}}}"));
        }
        out.push_str(&format!(
            "],\"rows\":{},\"touched\":{},\"page_reads\":{},\"page_writes\":{},\"page_hits\":{},\"page_misses\":{},\
             \"duration\":{{\"count\":{},\"sum_ns\":{},\"bounds_s\":{},\"buckets\":{}}},\"start_unix_ns\":{}}}",
            self.rows,
            self.touched,
            self.page_reads,
            self.page_writes,
            self.page_hits,
            self.page_misses,
            self.duration_count,
            self.duration_sum_ns,
            json_f64s(&DURATION_BOUNDS),
            json_u64s(&self.duration_buckets),
            self.start_unix_ns,
        ));
        out
    }
}

fn json_f64s(v: &[f64]) -> String {
    let parts: Vec<String> = v.iter().map(|x| format!("{x}")).collect();
    format!("[{}]", parts.join(","))
}

fn json_u64s(v: &[u64]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", parts.join(","))
}

// ------------------------------------------------------------- per database

/// One database's state, for gauges and per-database counters.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DbMetrics {
    pub nodes: u64,
    pub edges: u64,
    /// File (and segments) for a file-backed graph, pages held for `:memory:`.
    pub bytes: u64,
    /// `:memory:` limit.
    pub memory_limit: Option<u64>,
    pub page_size: u64,
    pub resident_pages: u64,
    pub allocated_pages: u64,
    /// Log a crash right now would replay.
    pub log_bytes: u64,
    /// Cumulative since the database was opened.
    pub page_reads: u64,
    pub page_writes: u64,
    pub page_hits: u64,
    pub page_misses: u64,
    pub evictions: u64,
    pub commits: u64,
    pub rollbacks: u64,
    pub checkpoints: u64,
}

impl DbMetrics {
    pub fn to_json(&self) -> String {
        format!(
            "{{\"nodes\":{},\"edges\":{},\"bytes\":{},\"memory_limit\":{},\"page_size\":{},\"resident_pages\":{},\
             \"allocated_pages\":{},\"log_bytes\":{},\"page_reads\":{},\"page_writes\":{},\"page_hits\":{},\
             \"page_misses\":{},\"evictions\":{},\"commits\":{},\"rollbacks\":{},\"checkpoints\":{}}}",
            self.nodes,
            self.edges,
            self.bytes,
            self.memory_limit.map(|m| m.to_string()).unwrap_or_else(|| "null".into()),
            self.page_size,
            self.resident_pages,
            self.allocated_pages,
            self.log_bytes,
            self.page_reads,
            self.page_writes,
            self.page_hits,
            self.page_misses,
            self.evictions,
            self.commits,
            self.rollbacks,
            self.checkpoints,
        )
    }
}

// ------------------------------------------------------------- trace context

/// A W3C trace context: the span the next statement on this thread should be
/// a child of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceContext {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub sampled: bool,
}

thread_local! {
    static CONTEXT: Cell<Option<TraceContext>> = const { Cell::new(None) };
}

/// Set (or clear) the calling thread's trace context. Returns the previous
/// one so a caller can restore it.
pub fn set_context(ctx: Option<TraceContext>) -> Option<TraceContext> {
    CONTEXT.with(|c| c.replace(ctx))
}

pub fn context() -> Option<TraceContext> {
    CONTEXT.with(|c| c.get())
}

impl TraceContext {
    /// Parse a `traceparent` header: `00-<32 hex>-<16 hex>-<2 hex flags>`.
    pub fn parse(s: &str) -> Option<TraceContext> {
        let mut parts = s.trim().split('-');
        let version = parts.next()?;
        let trace = parts.next()?;
        let span = parts.next()?;
        let flags = parts.next()?;
        if version.len() != 2
            || version == "ff"
            || trace.len() != 32
            || span.len() != 16
            || flags.len() != 2
        {
            return None;
        }
        if version == "00" && parts.next().is_some() {
            return None;
        }
        let mut trace_id = [0u8; 16];
        let mut span_id = [0u8; 8];
        unhex(trace, &mut trace_id)?;
        unhex(span, &mut span_id)?;
        let mut f = [0u8; 1];
        unhex(flags, &mut f)?;
        if trace_id == [0; 16] || span_id == [0; 8] {
            return None;
        }
        Some(TraceContext {
            trace_id,
            span_id,
            sampled: f[0] & 1 == 1,
        })
    }

    pub fn traceparent(&self) -> String {
        format!(
            "00-{}-{}-{}",
            hex(&self.trace_id),
            hex(&self.span_id),
            if self.sampled { "01" } else { "00" }
        )
    }
}

pub fn hex(b: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(D[(x >> 4) as usize] as char);
        s.push(D[(x & 15) as usize] as char);
    }
    s
}

fn unhex(s: &str, out: &mut [u8]) -> Option<()> {
    let b = s.as_bytes();
    if b.len() != out.len() * 2 {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    for (i, o) in out.iter_mut().enumerate() {
        *o = (nib(b[2 * i])? << 4) | nib(b[2 * i + 1])?;
    }
    Some(())
}

/// Random bytes for trace and span ids. std has no RNG, but `RandomState` is
/// seeded from the OS per process and re-keyed per call, which is plenty for
/// identifiers (they need to be unique, not secret).
pub fn random_bytes(out: &mut [u8]) {
    use std::hash::{BuildHasher, Hasher};
    static SEQ: AtomicU64 = ZERO;
    for chunk in out.chunks_mut(8) {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(SEQ.fetch_add(1, Relaxed));
        h.write_u64(now_unix_ns().unwrap_or(0));
        let v = h.finish().to_le_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
    }
}

// -------------------------------------------------------------------- spans

#[derive(Clone, Debug, PartialEq)]
pub enum Attr {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl From<&str> for Attr {
    fn from(s: &str) -> Attr {
        Attr::Str(s.to_string())
    }
}
impl From<String> for Attr {
    fn from(s: String) -> Attr {
        Attr::Str(s)
    }
}
impl From<u64> for Attr {
    fn from(v: u64) -> Attr {
        Attr::Int(v.min(i64::MAX as u64) as i64)
    }
}
impl From<i64> for Attr {
    fn from(v: i64) -> Attr {
        Attr::Int(v)
    }
}
impl From<bool> for Attr {
    fn from(v: bool) -> Attr {
        Attr::Bool(v)
    }
}

/// OTLP span kinds.
pub const KIND_INTERNAL: u8 = 1;
pub const KIND_SERVER: u8 = 2;
pub const KIND_CLIENT: u8 = 3;

/// A finished span.
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub parent: Option<[u8; 8]>,
    pub name: String,
    pub kind: u8,
    pub start_unix_ns: u64,
    pub end_unix_ns: u64,
    pub attrs: Vec<(String, Attr)>,
    /// Status: `None` is OK.
    pub error: Option<String>,
}

impl Span {
    /// A span id under `parent`, or a new trace if there is none.
    pub fn ids(parent: Option<TraceContext>) -> ([u8; 16], [u8; 8], Option<[u8; 8]>) {
        let mut span_id = [0u8; 8];
        random_bytes(&mut span_id);
        match parent {
            Some(p) => (p.trace_id, span_id, Some(p.span_id)),
            None => {
                let mut t = [0u8; 16];
                random_bytes(&mut t);
                (t, span_id, None)
            }
        }
    }
}

/// The attributes every host puts on a statement's span, from its report.
/// `text` is the statement; it is truncated to keep spans bounded.
pub fn query_attrs(r: &OpReport, text: Option<&str>) -> Vec<(String, Attr)> {
    let mut a: Vec<(String, Attr)> = vec![
        ("db.system.name".into(), SCOPE.into()),
        ("db.operation.name".into(), r.op.into()),
        ("db.response.returned_rows".into(), r.rows.into()),
        ("glider.touched".into(), r.touched.into()),
        ("glider.page.reads".into(), r.page_reads.into()),
        ("glider.page.writes".into(), r.page_writes.into()),
        ("glider.page.hits".into(), r.page_hits.into()),
        ("glider.page.misses".into(), r.page_misses.into()),
    ];
    if let Some(p) = &r.procedure {
        a.push(("db.stored_procedure.name".into(), p.clone().into()));
    }
    if let Some(t) = text {
        a.push(("db.query.text".into(), truncate(t, MAX_QUERY_TEXT).into()));
    }
    a
}

/// Longest `db.query.text` recorded, in bytes.
pub const MAX_QUERY_TEXT: usize = 2048;

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// -------------------------------------------------------------- OTLP / JSON

/// Resource attributes: `service.name` and friends.
pub type Resource = [(String, String)];

fn attr_json(k: &str, v: &Attr, out: &mut String) {
    out.push_str("{\"key\":");
    write_json_string(k, out);
    out.push_str(",\"value\":{");
    match v {
        Attr::Str(s) => {
            out.push_str("\"stringValue\":");
            write_json_string(s, out);
        }
        // OTLP/JSON encodes 64-bit integers as strings.
        Attr::Int(i) => out.push_str(&format!("\"intValue\":\"{i}\"")),
        Attr::Float(f) if f.is_finite() => out.push_str(&format!("\"doubleValue\":{f}")),
        Attr::Float(_) => out.push_str("\"doubleValue\":0"),
        Attr::Bool(b) => out.push_str(&format!("\"boolValue\":{b}")),
    }
    out.push_str("}}");
}

fn attrs_json(attrs: &[(String, Attr)], out: &mut String) {
    out.push('[');
    for (i, (k, v)) in attrs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        attr_json(k, v, out);
    }
    out.push(']');
}

fn resource_json(res: &Resource, out: &mut String) {
    let attrs: Vec<(String, Attr)> = res
        .iter()
        .map(|(k, v)| (k.clone(), Attr::Str(v.clone())))
        .collect();
    out.push_str("{\"attributes\":");
    attrs_json(&attrs, out);
    out.push('}');
}

fn scope_json(out: &mut String) {
    out.push_str(&format!(
        "{{\"name\":\"{SCOPE}\",\"version\":\"{}\"}}",
        crate::VERSION
    ));
}

/// Default resource: `service.name` plus the telemetry SDK identity.
pub fn default_resource(service: &str) -> Vec<(String, String)> {
    vec![
        ("service.name".into(), service.into()),
        ("telemetry.sdk.name".into(), SCOPE.into()),
        ("telemetry.sdk.language".into(), "rust".into()),
        ("telemetry.sdk.version".into(), crate::VERSION.into()),
    ]
}

/// A metrics builder: the OTLP `metrics` array and the Prometheus text are
/// rendered from the same list, so names cannot drift between them.
struct Metric {
    /// OTLP name, e.g. `glider.queries`; Prometheus replaces `.` with `_`.
    name: &'static str,
    unit: &'static str,
    help: &'static str,
    kind: Kind,
    points: Vec<(Vec<(String, Attr)>, u64)>,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Counter,
    Gauge,
}

fn db_attr(name: &str) -> Vec<(String, Attr)> {
    vec![("glider.db".into(), name.into())]
}

fn collect(s: &Snapshot, dbs: &[(String, DbMetrics)]) -> Vec<Metric> {
    let mut m = Vec::new();
    let one = |v: u64| vec![(Vec::new(), v)];

    let mut q = Vec::new();
    for (op, ok, err) in &s.queries {
        for (outcome, n) in [("ok", ok), ("error", err)] {
            if *n > 0 {
                q.push((
                    vec![
                        ("db.operation.name".to_string(), Attr::from(*op)),
                        ("glider.outcome".to_string(), Attr::from(outcome)),
                    ],
                    *n,
                ));
            }
        }
    }
    m.push(Metric {
        name: "glider.queries",
        unit: "{statement}",
        help: "Statements executed.",
        kind: Kind::Counter,
        points: q,
    });
    m.push(Metric {
        name: "glider.rows",
        unit: "{row}",
        help: "Rows returned by statements.",
        kind: Kind::Counter,
        points: one(s.rows),
    });
    m.push(Metric {
        name: "glider.touched",
        unit: "{entity}",
        help: "Nodes and relationships written by statements.",
        kind: Kind::Counter,
        points: one(s.touched),
    });
    m.push(Metric {
        name: "glider.page.reads",
        unit: "{page}",
        help: "Pages read from storage by statements.",
        kind: Kind::Counter,
        points: one(s.page_reads),
    });
    m.push(Metric {
        name: "glider.page.writes",
        unit: "{page}",
        help: "Pages written to storage by statements.",
        kind: Kind::Counter,
        points: one(s.page_writes),
    });
    m.push(Metric {
        name: "glider.page.hits",
        unit: "{page}",
        help: "Page-cache hits during statements.",
        kind: Kind::Counter,
        points: one(s.page_hits),
    });
    m.push(Metric {
        name: "glider.page.misses",
        unit: "{page}",
        help: "Page-cache misses during statements.",
        kind: Kind::Counter,
        points: one(s.page_misses),
    });

    let per_db = |f: &dyn Fn(&DbMetrics) -> Option<u64>| -> Vec<(Vec<(String, Attr)>, u64)> {
        dbs.iter()
            .filter_map(|(n, d)| f(d).map(|v| (db_attr(n), v)))
            .collect()
    };
    if !dbs.is_empty() {
        m.push(Metric {
            name: "glider.db.nodes",
            unit: "{node}",
            help: "Nodes in the database.",
            kind: Kind::Gauge,
            points: per_db(&|d| Some(d.nodes)),
        });
        m.push(Metric {
            name: "glider.db.edges",
            unit: "{relationship}",
            help: "Relationships in the database.",
            kind: Kind::Gauge,
            points: per_db(&|d| Some(d.edges)),
        });
        m.push(Metric {
            name: "glider.db.size",
            unit: "By",
            help: "Bytes the database occupies (file, or pages held in memory).",
            kind: Kind::Gauge,
            points: per_db(&|d| Some(d.bytes)),
        });
        m.push(Metric {
            name: "glider.db.memory.limit",
            unit: "By",
            help: "Memory limit of an in-memory database.",
            kind: Kind::Gauge,
            points: per_db(&|d| d.memory_limit),
        });
        m.push(Metric {
            name: "glider.db.log.size",
            unit: "By",
            help: "Write-ahead log a crash would replay.",
            kind: Kind::Gauge,
            points: per_db(&|d| Some(d.log_bytes)),
        });
        m.push(Metric {
            name: "glider.db.cache.resident",
            unit: "{page}",
            help: "Pages resident in the cache.",
            kind: Kind::Gauge,
            points: per_db(&|d| Some(d.resident_pages)),
        });
        m.push(Metric {
            name: "glider.db.pages.allocated",
            unit: "{page}",
            help: "Pages in use.",
            kind: Kind::Gauge,
            points: per_db(&|d| Some(d.allocated_pages)),
        });
        m.push(Metric {
            name: "glider.db.cache.hits",
            unit: "{page}",
            help: "Page-cache hits since open.",
            kind: Kind::Counter,
            points: per_db(&|d| Some(d.page_hits)),
        });
        m.push(Metric {
            name: "glider.db.cache.misses",
            unit: "{page}",
            help: "Page-cache misses since open.",
            kind: Kind::Counter,
            points: per_db(&|d| Some(d.page_misses)),
        });
        m.push(Metric {
            name: "glider.db.cache.evictions",
            unit: "{page}",
            help: "Pages evicted since open.",
            kind: Kind::Counter,
            points: per_db(&|d| Some(d.evictions)),
        });
        m.push(Metric {
            name: "glider.db.io.reads",
            unit: "{page}",
            help: "Pages read from storage since open.",
            kind: Kind::Counter,
            points: per_db(&|d| Some(d.page_reads)),
        });
        m.push(Metric {
            name: "glider.db.io.writes",
            unit: "{page}",
            help: "Pages written to storage since open.",
            kind: Kind::Counter,
            points: per_db(&|d| Some(d.page_writes)),
        });
        m.push(Metric {
            name: "glider.db.commits",
            unit: "{transaction}",
            help: "Transactions committed since open.",
            kind: Kind::Counter,
            points: per_db(&|d| Some(d.commits)),
        });
        m.push(Metric {
            name: "glider.db.rollbacks",
            unit: "{transaction}",
            help: "Transactions rolled back since open.",
            kind: Kind::Counter,
            points: per_db(&|d| Some(d.rollbacks)),
        });
        m.push(Metric {
            name: "glider.db.checkpoints",
            unit: "{checkpoint}",
            help: "Checkpoints since open.",
            kind: Kind::Counter,
            points: per_db(&|d| Some(d.checkpoints)),
        });
    }
    m
}

/// Process counters and the given databases as an OTLP/HTTP JSON
/// `ExportMetricsServiceRequest`, for POSTing to `<collector>/v1/metrics`.
/// `now_unix_ns` comes from the host where the engine has no clock (wasm).
pub fn otlp_metrics_json(res: &Resource, now_unix_ns: u64, dbs: &[(String, DbMetrics)]) -> String {
    note_start(now_unix_ns);
    let s = snapshot();
    let start = if s.start_unix_ns == 0 {
        now_unix_ns
    } else {
        s.start_unix_ns.min(now_unix_ns)
    };
    let times = format!("\"startTimeUnixNano\":\"{start}\",\"timeUnixNano\":\"{now_unix_ns}\"");

    let mut out = String::with_capacity(8192);
    out.push_str("{\"resourceMetrics\":[{\"resource\":");
    resource_json(res, &mut out);
    out.push_str(",\"scopeMetrics\":[{\"scope\":");
    scope_json(&mut out);
    out.push_str(",\"metrics\":[");

    let mut first = true;
    for m in collect(&s, dbs) {
        if m.points.is_empty() {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&format!(
            "{{\"name\":\"{}\",\"unit\":\"{}\",\"description\":\"{}\",",
            m.name, m.unit, m.help
        ));
        out.push_str(match m.kind {
            Kind::Counter => {
                "\"sum\":{\"aggregationTemporality\":2,\"isMonotonic\":true,\"dataPoints\":["
            }
            Kind::Gauge => "\"gauge\":{\"dataPoints\":[",
        });
        for (i, (attrs, v)) in m.points.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"attributes\":");
            attrs_json(attrs, &mut out);
            // OTLP's asInt is a signed 64-bit integer.
            out.push_str(&format!(",{times},\"asInt\":\"{}\"}}", (*v).min(i64::MAX as u64)));
        }
        out.push_str("]}}");
    }

    // The histogram. OTLP wants per-bucket counts, not cumulative ones.
    if s.duration_count > 0 {
        if !first {
            out.push(',');
        }
        out.push_str(
            "{\"name\":\"glider.query.duration\",\"unit\":\"s\",\"description\":\"Statement duration.\",\
             \"histogram\":{\"aggregationTemporality\":2,\"dataPoints\":[{\"attributes\":[],",
        );
        out.push_str(&format!(
            "{times},\"count\":\"{}\",\"sum\":{},\"bucketCounts\":[{}],\"explicitBounds\":{}}}]}}}}",
            s.duration_count,
            s.duration_sum_ns as f64 / 1e9,
            s.duration_buckets
                .iter()
                .map(|b| format!("\"{b}\""))
                .collect::<Vec<_>>()
                .join(","),
            json_f64s(&DURATION_BOUNDS),
        ));
    }
    out.push_str("]}]}]}");
    out
}

/// Finished spans as an OTLP/HTTP JSON `ExportTraceServiceRequest`, for
/// `<collector>/v1/traces`.
pub fn otlp_traces_json(res: &Resource, spans: &[Span]) -> String {
    let mut out = String::with_capacity(512 * spans.len() + 256);
    out.push_str("{\"resourceSpans\":[{\"resource\":");
    resource_json(res, &mut out);
    out.push_str(",\"scopeSpans\":[{\"scope\":");
    scope_json(&mut out);
    out.push_str(",\"spans\":[");
    for (i, s) in spans.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"traceId\":\"{}\",\"spanId\":\"{}\",",
            hex(&s.trace_id),
            hex(&s.span_id)
        ));
        if let Some(p) = s.parent {
            out.push_str(&format!("\"parentSpanId\":\"{}\",", hex(&p)));
        }
        out.push_str("\"name\":");
        write_json_string(&s.name, &mut out);
        out.push_str(&format!(
            ",\"kind\":{},\"startTimeUnixNano\":\"{}\",\"endTimeUnixNano\":\"{}\",\"attributes\":",
            s.kind,
            s.start_unix_ns,
            s.end_unix_ns.max(s.start_unix_ns)
        ));
        attrs_json(&s.attrs, &mut out);
        match &s.error {
            // STATUS_CODE_ERROR
            Some(e) => {
                out.push_str(",\"status\":{\"code\":2,\"message\":");
                write_json_string(e, &mut out);
                out.push_str("}}");
            }
            None => out.push_str(",\"status\":{\"code\":1}}"),
        }
    }
    out.push_str("]}]}]}");
    out
}

// ---------------------------------------------------------------- prometheus

fn prom_name(otlp: &str, unit: &str, counter: bool) -> String {
    let mut n = otlp.replace('.', "_");
    match unit {
        "By" => n.push_str("_bytes"),
        "s" => n.push_str("_seconds"),
        _ => {}
    }
    if counter {
        n.push_str("_total");
    }
    n
}

fn prom_labels(attrs: &[(String, Attr)]) -> String {
    if attrs.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = attrs
        .iter()
        .map(|(k, v)| {
            let v = match v {
                Attr::Str(s) => s.clone(),
                Attr::Int(i) => i.to_string(),
                Attr::Float(f) => f.to_string(),
                Attr::Bool(b) => b.to_string(),
            };
            let v = v
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n");
            format!("{}=\"{}\"", k.replace('.', "_"), v)
        })
        .collect();
    format!("{{{}}}", parts.join(","))
}

/// Process counters and the given databases in the Prometheus text
/// exposition format, for a `/metrics` endpoint.
pub fn prometheus(dbs: &[(String, DbMetrics)]) -> String {
    let s = snapshot();
    let mut out = String::with_capacity(4096);
    for m in collect(&s, dbs) {
        let counter = m.kind == Kind::Counter;
        let name = prom_name(m.name, m.unit, counter);
        out.push_str(&format!(
            "# HELP {name} {}\n# TYPE {name} {}\n",
            m.help,
            if counter { "counter" } else { "gauge" }
        ));
        for (attrs, v) in &m.points {
            out.push_str(&format!("{name}{} {v}\n", prom_labels(attrs)));
        }
    }
    let h = "glider_query_duration_seconds";
    out.push_str(&format!(
        "# HELP {h} Statement duration.\n# TYPE {h} histogram\n"
    ));
    let mut cum = 0;
    for (i, b) in s.duration_buckets.iter().enumerate() {
        cum += b;
        let le = DURATION_BOUNDS
            .get(i)
            .map(|x| x.to_string())
            .unwrap_or_else(|| "+Inf".into());
        out.push_str(&format!("{h}_bucket{{le=\"{le}\"}} {cum}\n"));
    }
    out.push_str(&format!(
        "{h}_sum {}\n{h}_count {}\n",
        s.duration_sum_ns as f64 / 1e9,
        s.duration_count
    ));
    out
}

// ------------------------------------------------------------ query spans

/// Called by `query::execute_with` once a statement finishes: record it, and
/// if an exporter is installed, emit its span.
pub(crate) fn finish(
    report: OpReport,
    text: &str,
    watch: &Stopwatch,
    db: Option<(u64, String, DbMetrics)>,
) {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(exp) = otlp::installed() {
        // The statement ended a moment ago; its duration fixes the start.
        if let (Some(end), Some(dur)) = (now_unix_ns(), report.duration_ns) {
            let start = end.saturating_sub(dur);
            let parent = context();
            if parent.map(|p| p.sampled).unwrap_or(true) && exp.traces_enabled() {
                let (trace_id, span_id, parent_id) = Span::ids(parent);
                let mut attrs = query_attrs(&report, exp.query_text().then_some(text));
                if let Some((_, name, _)) = &db {
                    attrs.push(("glider.db".into(), name.clone().into()));
                }
                exp.span(Span {
                    trace_id,
                    span_id,
                    parent: parent_id,
                    name: format!("glider {}", report.op),
                    kind: KIND_CLIENT,
                    start_unix_ns: start,
                    end_unix_ns: end,
                    attrs,
                    error: report.error.clone(),
                });
            }
        }
        if let Some((id, name, m)) = db {
            exp.observe_db(id, name, m);
        }
    }
    #[cfg(target_arch = "wasm32")]
    let _ = (text, watch, db);
    if START_NS.load(Relaxed) == 0 {
        if let Some(start) = watch.started_unix_ns() {
            note_start(start);
        }
    }
    record(report);
}

/// Whether statements should snapshot their database for the exporter.
pub(crate) fn wants_db_metrics() -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        otlp::installed().is_some()
    }
    #[cfg(target_arch = "wasm32")]
    {
        false
    }
}

/// A database was closed: stop reporting it.
pub(crate) fn forget_db(_id: u64) {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(exp) = otlp::installed() {
        exp.forget_db(_id);
    }
}

/// A process-unique id per open database, for the exporter's registry.
pub(crate) fn next_db_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Relaxed)
}

// ------------------------------------------------------------- the exporter

/// A std-only OTLP/HTTP JSON exporter, for native builds. Configured from
/// the standard environment:
///
/// | variable | meaning |
/// |---|---|
/// | `OTEL_SDK_DISABLED=true` | install nothing |
/// | `OTEL_EXPORTER_OTLP_ENDPOINT` | base URL, `/v1/traces` and `/v1/metrics` appended |
/// | `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `..._METRICS_ENDPOINT` | full URLs per signal |
/// | `OTEL_EXPORTER_OTLP_HEADERS` | `k=v,k2=v2`, sent with every request |
/// | `OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES` | the resource |
/// | `OTEL_TRACES_EXPORTER=none`, `OTEL_METRICS_EXPORTER=none` | turn a signal off |
/// | `OTEL_METRIC_EXPORT_INTERVAL` | ms between metric pushes (default 60000) |
/// | `OTEL_BSP_SCHEDULE_DELAY` | ms between span batches (default 5000) |
/// | `GLIDER_OTEL_QUERY_TEXT=false` | leave `db.query.text` off spans |
///
/// Only `http://` is spoken (no TLS in std): point it at a local collector or
/// agent, which is how OTLP is normally deployed anyway.
#[cfg(not(target_arch = "wasm32"))]
pub mod otlp {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Condvar, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    /// Spans held before new ones are dropped (and counted).
    const MAX_QUEUE: usize = 4096;

    #[derive(Clone, Debug, PartialEq)]
    pub struct Url {
        pub host: String,
        pub port: u16,
        pub path: String,
    }

    impl Url {
        pub fn parse(s: &str) -> Result<Url, String> {
            let rest = s.strip_prefix("http://").ok_or_else(|| {
                format!("{s}: only http:// endpoints are supported (no TLS in std)")
            })?;
            let (hostport, path) = match rest.find('/') {
                Some(i) => (&rest[..i], &rest[i..]),
                None => (rest, ""),
            };
            // `[v6]:port`, `[v6]`, `host:port`, `host`.
            let split = match hostport.strip_prefix('[') {
                Some(v6) => v6
                    .split_once(']')
                    .map(|(h, rest)| (h, rest.strip_prefix(':'))),
                None => Some(match hostport.split_once(':') {
                    Some((h, p)) => (h, Some(p)),
                    None => (hostport, None),
                }),
            };
            let Some((host, port)) = split else {
                return Err(format!("{s}: bad host"));
            };
            let port = match port {
                Some(p) => p.parse().map_err(|_| format!("{s}: bad port"))?,
                None => 80,
            };
            if host.is_empty() {
                return Err(format!("{s}: no host"));
            }
            Ok(Url {
                host: host.to_string(),
                port,
                path: if path.is_empty() {
                    "/".into()
                } else {
                    path.into()
                },
            })
        }

        fn join(&self, signal: &str) -> Url {
            let mut u = self.clone();
            u.path = format!("{}/{signal}", u.path.trim_end_matches('/'));
            u
        }
    }

    pub struct Config {
        pub traces: Option<Url>,
        pub metrics: Option<Url>,
        pub headers: Vec<(String, String)>,
        pub resource: Vec<(String, String)>,
        pub metric_interval: Duration,
        pub span_delay: Duration,
        pub query_text: bool,
    }

    impl Config {
        /// Read the environment. `Ok(None)` means telemetry is not asked for
        /// (no endpoint, or the SDK disabled).
        pub fn from_env(default_service: &str) -> Result<Option<Config>, String> {
            Config::from_vars(default_service, |k| std::env::var(k).ok())
        }

        pub fn from_vars(
            default_service: &str,
            var: impl Fn(&str) -> Option<String>,
        ) -> Result<Option<Config>, String> {
            let on = |k: &str| var(k).map(|v| v.trim().to_ascii_lowercase());
            if on("OTEL_SDK_DISABLED").as_deref() == Some("true") {
                return Ok(None);
            }
            let base = var("OTEL_EXPORTER_OTLP_ENDPOINT").filter(|s| !s.trim().is_empty());
            let signal =
                |specific: &str, name: &str, exporter: &str| -> Result<Option<Url>, String> {
                    if on(exporter).as_deref() == Some("none") {
                        return Ok(None);
                    }
                    if let Some(s) = var(specific).filter(|s| !s.trim().is_empty()) {
                        return Url::parse(s.trim()).map(Some);
                    }
                    match &base {
                        Some(b) => Url::parse(b.trim()).map(|u| Some(u.join(name))),
                        None => Ok(None),
                    }
                };
            let traces = signal(
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                "v1/traces",
                "OTEL_TRACES_EXPORTER",
            )?;
            let metrics = signal(
                "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
                "v1/metrics",
                "OTEL_METRICS_EXPORTER",
            )?;
            if traces.is_none() && metrics.is_none() {
                return Ok(None);
            }
            let headers = var("OTEL_EXPORTER_OTLP_HEADERS")
                .map(|h| parse_pairs(&h))
                .unwrap_or_default();
            let service = var("OTEL_SERVICE_NAME")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| default_service.to_string());
            let mut resource = default_resource(&service);
            for (k, v) in var("OTEL_RESOURCE_ATTRIBUTES")
                .map(|r| parse_pairs(&r))
                .unwrap_or_default()
            {
                if k == "service.name" && var("OTEL_SERVICE_NAME").is_some() {
                    continue;
                }
                match resource.iter_mut().find(|(rk, _)| *rk == k) {
                    Some(slot) => slot.1 = v,
                    None => resource.push((k, v)),
                }
            }
            let ms = |k: &str, d: u64| {
                Duration::from_millis(
                    var(k)
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(d)
                        .max(10),
                )
            };
            Ok(Some(Config {
                traces,
                metrics,
                headers,
                resource,
                metric_interval: ms("OTEL_METRIC_EXPORT_INTERVAL", 60_000),
                span_delay: ms("OTEL_BSP_SCHEDULE_DELAY", 5_000),
                query_text: !matches!(
                    on("GLIDER_OTEL_QUERY_TEXT").as_deref(),
                    Some("false" | "0" | "off")
                ),
            }))
        }
    }

    /// `k=v,k2=v2` with percent-decoding, as the OTEL_* list variables use.
    fn parse_pairs(s: &str) -> Vec<(String, String)> {
        s.split(',')
            .filter_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                let k = pct_decode(k.trim());
                (!k.is_empty()).then(|| (k, pct_decode(v.trim())))
            })
            .collect()
    }

    fn pct_decode(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                if let Ok(x) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(x);
                    i += 3;
                    continue;
                }
            }
            out.push(b[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    struct State {
        spans: Vec<Span>,
        /// Latest state of every open database that has run a statement.
        dbs: BTreeMap<u64, (String, DbMetrics)>,
        stop: bool,
        flush_now: bool,
    }

    pub struct Exporter {
        cfg: Config,
        state: Mutex<State>,
        wake: Condvar,
        dropped: AtomicU64,
        failures: AtomicU64,
        warned: AtomicBool,
        /// Serialises pushes so `flush` and the worker never interleave.
        push_lock: Mutex<()>,
    }

    static INSTALLED: OnceLock<Arc<Exporter>> = OnceLock::new();

    /// The process exporter, if one is installed.
    pub fn installed() -> Option<&'static Arc<Exporter>> {
        INSTALLED.get()
    }

    /// Install the process exporter from the environment, once. Returns
    /// whether one is (now) installed; `Err` for a malformed configuration.
    /// Safe to call repeatedly: later calls are no-ops.
    pub fn install_from_env(default_service: &str) -> Result<bool, String> {
        install_from_env_signals(default_service, true)
    }

    /// As [`install_from_env`], but with spans off when `traces` is false —
    /// for hosts that trace through their own SDK (the BEAM) and only want
    /// the engine's metrics from this exporter.
    pub fn install_from_env_signals(default_service: &str, traces: bool) -> Result<bool, String> {
        if INSTALLED.get().is_some() {
            return Ok(true);
        }
        match Config::from_env(default_service)? {
            Some(mut cfg) => {
                if !traces {
                    cfg.traces = None;
                }
                if cfg.traces.is_none() && cfg.metrics.is_none() {
                    return Ok(false);
                }
                install(cfg);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Install with an explicit configuration. The first install wins.
    pub fn install(cfg: Config) -> &'static Arc<Exporter> {
        let mut fresh = false;
        let exp = INSTALLED.get_or_init(|| {
            fresh = true;
            Arc::new(Exporter {
                cfg,
                state: Mutex::new(State {
                    spans: Vec::new(),
                    dbs: BTreeMap::new(),
                    stop: false,
                    flush_now: false,
                }),
                wake: Condvar::new(),
                dropped: ZERO,
                failures: ZERO,
                warned: AtomicBool::new(false),
                push_lock: Mutex::new(()),
            })
        });
        if fresh {
            note_start(now_unix_ns().unwrap_or(0));
            let worker = Arc::clone(exp);
            let _ = std::thread::Builder::new()
                .name("glider-otlp".into())
                .spawn(move || worker.run());
        }
        exp
    }

    /// Push everything pending now, synchronously. Call before exit.
    pub fn flush() {
        if let Some(e) = installed() {
            e.push_spans();
            e.push_metrics();
        }
    }

    impl Exporter {
        fn lock(&self) -> std::sync::MutexGuard<'_, State> {
            self.state.lock().unwrap_or_else(|p| p.into_inner())
        }

        pub fn traces_enabled(&self) -> bool {
            self.cfg.traces.is_some()
        }

        pub fn query_text(&self) -> bool {
            self.cfg.query_text
        }

        pub fn resource(&self) -> &[(String, String)] {
            &self.cfg.resource
        }

        /// Queue a finished span.
        pub fn span(&self, s: Span) {
            if !self.traces_enabled() {
                return;
            }
            let mut st = self.lock();
            if st.spans.len() >= MAX_QUEUE {
                self.dropped.fetch_add(1, Relaxed);
                return;
            }
            st.spans.push(s);
            if st.spans.len() >= MAX_QUEUE / 2 {
                st.flush_now = true;
                self.wake.notify_one();
            }
        }

        /// Record a database's latest state, reported with every metric push.
        pub fn observe_db(&self, id: u64, name: String, m: DbMetrics) {
            self.lock().dbs.insert(id, (name, m));
        }

        pub fn forget_db(&self, id: u64) {
            self.lock().dbs.remove(&id);
        }

        /// Spans dropped because the queue was full.
        pub fn dropped_spans(&self) -> u64 {
            self.dropped.load(Relaxed)
        }

        /// Pushes the collector refused or never received.
        pub fn failures(&self) -> u64 {
            self.failures.load(Relaxed)
        }

        fn run(self: Arc<Self>) {
            let mut next_metrics = Instant::now() + self.cfg.metric_interval;
            let mut next_spans = Instant::now() + self.cfg.span_delay;
            loop {
                {
                    let mut st = self.lock();
                    let due = next_metrics.min(next_spans);
                    while !st.stop && !st.flush_now && Instant::now() < due {
                        let left = due.saturating_duration_since(Instant::now());
                        st = match self.wake.wait_timeout(st, left) {
                            Ok((g, _)) => g,
                            Err(p) => p.into_inner().0,
                        };
                    }
                    if st.stop {
                        return;
                    }
                    st.flush_now = false;
                }
                let now = Instant::now();
                if now >= next_spans || self.lock().spans.len() >= MAX_QUEUE / 2 {
                    self.push_spans();
                    next_spans = now + self.cfg.span_delay;
                }
                if now >= next_metrics {
                    self.push_metrics();
                    next_metrics = now + self.cfg.metric_interval;
                }
            }
        }

        fn push_spans(&self) {
            let Some(url) = &self.cfg.traces else { return };
            let _g = self.push_lock.lock().unwrap_or_else(|p| p.into_inner());
            let batch = std::mem::take(&mut self.lock().spans);
            if batch.is_empty() {
                return;
            }
            let body = otlp_traces_json(&self.cfg.resource, &batch);
            self.post(url, &body);
        }

        fn push_metrics(&self) {
            let Some(url) = &self.cfg.metrics else { return };
            let _g = self.push_lock.lock().unwrap_or_else(|p| p.into_inner());
            let dbs: Vec<(String, DbMetrics)> = self.lock().dbs.values().cloned().collect();
            let body = otlp_metrics_json(&self.cfg.resource, now_unix_ns().unwrap_or(0), &dbs);
            self.post(url, &body);
        }

        fn post(&self, url: &Url, body: &str) {
            if let Err(e) = post(url, &self.cfg.headers, body) {
                self.failures.fetch_add(1, Relaxed);
                // Once: a missing collector must not flood the host's stderr.
                if !self.warned.swap(true, Relaxed) {
                    eprintln!("glider: OTLP export to http://{}:{}{} failed: {e} (further failures are counted, not logged)", url.host, url.port, url.path);
                }
            }
        }
    }

    /// One HTTP/1.1 POST of a JSON body; Ok when the collector answers 2xx.
    pub fn post(url: &Url, headers: &[(String, String)], body: &str) -> Result<(), String> {
        let addr = (url.host.as_str(), url.port)
            .to_socket_addrs()
            .map_err(|e| e.to_string())?
            .next()
            .ok_or("no address")?;
        let timeout = Duration::from_secs(5);
        let mut s = TcpStream::connect_timeout(&addr, timeout).map_err(|e| e.to_string())?;
        s.set_read_timeout(Some(timeout)).ok();
        s.set_write_timeout(Some(timeout)).ok();
        let mut req = format!(
            "POST {} HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nUser-Agent: glider/{}\r\n",
            url.path,
            url.host,
            url.port,
            body.len(),
            crate::VERSION
        );
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
        s.write_all(body.as_bytes()).map_err(|e| e.to_string())?;
        s.flush().map_err(|e| e.to_string())?;
        let mut head = [0u8; 64];
        let mut n = 0;
        while n < 12 {
            match s.read(&mut head[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) => return Err(e.to_string()),
            }
        }
        let line = String::from_utf8_lossy(&head[..n]);
        let code: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| format!("bad response: {line:?}"))?;
        if (200..300).contains(&code) {
            Ok(())
        } else {
            Err(format!("collector answered {code}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceparent_round_trips_and_rejects_garbage() {
        let s = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let c = TraceContext::parse(s).unwrap();
        assert!(c.sampled);
        assert_eq!(c.traceparent(), s);
        assert!(
            TraceContext::parse("00-00000000000000000000000000000000-00f067aa0ba902b7-01")
                .is_none()
        );
        assert!(
            TraceContext::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7").is_none()
        );
        assert!(
            TraceContext::parse("00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01")
                .is_none()
        );
        assert!(
            TraceContext::parse("ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
                .is_none()
        );
    }

    #[test]
    fn random_ids_differ() {
        let (mut a, mut b) = ([0u8; 16], [0u8; 16]);
        random_bytes(&mut a);
        random_bytes(&mut b);
        assert_ne!(a, b);
        assert_ne!(a, [0; 16]);
    }

    #[test]
    fn histogram_buckets_by_upper_bound() {
        let before = snapshot();
        observe_duration(5_000); // 5µs -> first bucket
        observe_duration(20_000_000_000); // 20s -> overflow
        let after = snapshot();
        assert_eq!(after.duration_buckets[0] - before.duration_buckets[0], 1);
        let last = DURATION_BOUNDS.len();
        assert_eq!(
            after.duration_buckets[last] - before.duration_buckets[last],
            1
        );
        assert_eq!(after.duration_count - before.duration_count, 2);
    }

    #[test]
    fn renders_otlp_and_prometheus() {
        record(OpReport {
            op: "MATCH",
            rows: 3,
            ..Default::default()
        });
        let db = DbMetrics {
            nodes: 7,
            memory_limit: Some(1 << 20),
            page_hits: u64::MAX,
            ..Default::default()
        };
        let dbs = vec![(":memory:".to_string(), db)];
        let j = otlp_metrics_json(&default_resource("t"), 1_700_000_000_000_000_000, &dbs);
        assert!(j.starts_with(
            "{\"resourceMetrics\":[{\"resource\":{\"attributes\":[{\"key\":\"service.name\""
        ));
        assert!(j.contains("\"name\":\"glider.queries\""));
        assert!(j.contains("\"name\":\"glider.db.nodes\""));
        assert!(j.contains("{\"key\":\"glider.db\",\"value\":{\"stringValue\":\":memory:\"}}"));
        assert!(j.ends_with("]}]}]}"));
        // OTLP's asInt is signed: collectors reject anything above i64::MAX.
        assert!(!j.contains(&u64::MAX.to_string()));
        assert!(j.contains(&format!("\"asInt\":\"{}\"", i64::MAX)));
        let p = prometheus(&dbs);
        assert!(p.contains("# TYPE glider_queries_total counter"));
        assert!(
            p.contains("glider_queries_total{db_operation_name=\"MATCH\",glider_outcome=\"ok\"}")
        );
        assert!(p.contains("glider_db_nodes{glider_db=\":memory:\"} 7"));
        assert!(p.contains("glider_db_memory_limit_bytes{glider_db=\":memory:\"} 1048576"));
        assert!(p.contains("glider_query_duration_seconds_bucket{le=\"+Inf\"}"));
    }

    #[test]
    fn spans_render_with_parent_and_status() {
        let ctx =
            TraceContext::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").unwrap();
        let (t, s, p) = Span::ids(Some(ctx));
        let span = Span {
            trace_id: t,
            span_id: s,
            parent: p,
            name: "glider MATCH".into(),
            kind: KIND_CLIENT,
            start_unix_ns: 10,
            end_unix_ns: 20,
            attrs: vec![
                ("db.system.name".into(), "glider".into()),
                ("n".into(), Attr::Int(3)),
            ],
            error: Some("bad \"quote\"".into()),
        };
        let j = otlp_traces_json(&default_resource("t"), &[span]);
        assert!(j.contains("\"traceId\":\"4bf92f3577b34da6a3ce929d0e0e4736\""));
        assert!(j.contains("\"parentSpanId\":\"00f067aa0ba902b7\""));
        assert!(j.contains("\"intValue\":\"3\""));
        assert!(j.contains("\"status\":{\"code\":2,\"message\":\"bad \\\"quote\\\"\"}"));
    }

    #[test]
    fn config_reads_the_standard_variables() {
        use otlp::{Config, Url};
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(Config::from_vars("svc", env(&[])).unwrap().is_none());
        assert!(Config::from_vars(
            "svc",
            env(&[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
                ("OTEL_SDK_DISABLED", "true")
            ])
        )
        .unwrap()
        .is_none());
        let c = Config::from_vars(
            "svc",
            env(&[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://collector:4318/"),
                ("OTEL_TRACES_EXPORTER", "none"),
                (
                    "OTEL_EXPORTER_OTLP_HEADERS",
                    "authorization=Bearer%20x,x-a=b",
                ),
                (
                    "OTEL_RESOURCE_ATTRIBUTES",
                    "deployment.environment=prod,service.name=ignored",
                ),
                ("OTEL_SERVICE_NAME", "api"),
                ("OTEL_METRIC_EXPORT_INTERVAL", "1500"),
            ]),
        )
        .unwrap()
        .unwrap();
        assert!(c.traces.is_none());
        assert_eq!(
            c.metrics,
            Some(Url {
                host: "collector".into(),
                port: 4318,
                path: "/v1/metrics".into()
            })
        );
        assert_eq!(c.headers[0], ("authorization".into(), "Bearer x".into()));
        assert!(c.resource.contains(&("service.name".into(), "api".into())));
        assert!(c
            .resource
            .contains(&("deployment.environment".into(), "prod".into())));
        assert_eq!(c.metric_interval.as_millis(), 1500);
        assert!(Url::parse("https://x").is_err());
        assert_eq!(Url::parse("http://[::1]:4318").unwrap().host, "::1");
    }
}
