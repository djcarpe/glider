//! The C ABI. This is what Swift, Kotlin, Dart, Go, Python and everything else
//! links against — same shape SQLite exposes, for the same reason: a C ABI is
//! the one calling convention every platform agrees on.
//!
//! Rules of the road:
//!
//! * Every string returned by this module was allocated by Rust. Hand it back
//!   to `glider_free`. Do not call `free(3)` on it.
//! * A `glider_db` handle is **not** thread-safe. One handle per thread, or
//!   put your own lock around it. The engine holds the whole graph in memory
//!   and mutates it in place; concurrent access is a data race, not a stall.
//! * A function that returns a pointer returns NULL on failure; one that
//!   returns `int` returns 0 on success and -1 on failure. Either way,
//!   `glider_last_error()` has the detail, on the calling thread.
//! * Panics are caught at this boundary and turned into errors. Unwinding
//!   across an FFI edge is undefined behaviour, and a database bug should not
//!   take down the host app.

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::OnceLock;

use crate::graph::Graph;
use crate::query;
use crate::store::Sync;

/// Opaque handle. Callers only ever see `*mut GliderDb`.
pub struct GliderDb {
    graph: Graph,
}

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn set_error(msg: impl Into<Vec<u8>>) {
    let c = CString::new(msg).unwrap_or_else(|_| CString::new("error").unwrap());
    LAST_ERROR.with(|e| *e.borrow_mut() = Some(c));
}

fn clear_error() {
    LAST_ERROR.with(|e| *e.borrow_mut() = None);
}

/// Run `f`, converting both errors and panics into `fallback` plus a message
/// on the thread-local error slot.
fn guard<T>(fallback: T, f: impl FnOnce() -> Result<T, String>) -> T {
    clear_error();
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(v)) => v,
        Ok(Err(msg)) => {
            set_error(msg);
            fallback
        }
        Err(panic) => {
            let detail = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown".into());
            set_error(format!("panic in glider: {detail}"));
            fallback
        }
    }
}

unsafe fn as_str<'a>(p: *const c_char, what: &str) -> Result<&'a str, String> {
    unsafe {
        if p.is_null() {
            return Err(format!("{what} was NULL"));
        }
        CStr::from_ptr(p)
            .to_str()
            .map_err(|_| format!("{what} was not valid UTF-8"))
    }
}

unsafe fn as_db<'a>(p: *mut GliderDb) -> Result<&'a mut GliderDb, String> {
    unsafe {
        if p.is_null() {
            return Err("database handle was NULL".into());
        }
        Ok(&mut *p)
    }
}

fn out_string(s: String) -> Result<*mut c_char, String> {
    CString::new(s)
        .map(|c| c.into_raw())
        .map_err(|_| "result contained an interior NUL byte".to_string())
}

// ------------------------------------------------------------------ lifetime

/// Open (or create) a database file. Returns NULL on failure.
///
/// `sync` is 0 = always, 1 = normal, 2 = off. On mobile, prefer 1 and call
/// `glider_checkpoint` when the app backgrounds.
///
/// # Safety
/// `path` must be a NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn glider_open(path: *const c_char, sync: c_int) -> *mut GliderDb {
    unsafe {
        guard(std::ptr::null_mut(), || {
            let path = as_str(path, "path")?;
            let sync = match sync {
                0 => Sync::Always,
                2 => Sync::Off,
                _ => Sync::Normal,
            };
            let graph = Graph::open(Path::new(path), sync).map_err(|e| e.to_string())?;
            Ok(Box::into_raw(Box::new(GliderDb { graph })))
        })
    }
}

/// `glider_open` with the memory knob exposed: `cache_bytes` is the page
/// cache (0 = the default, 1 GiB). RAM use stays near it however large the
/// database grows; the database itself is limited only by the disk.
///
/// # Safety
/// `path` must be a NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn glider_open_ex(
    path: *const c_char,
    sync: c_int,
    cache_bytes: usize,
) -> *mut GliderDb {
    unsafe {
        guard(std::ptr::null_mut(), || {
            let path = as_str(path, "path")?;
            let opts = crate::OpenOptions {
                sync: match sync {
                    0 => Sync::Always,
                    2 => Sync::Off,
                    _ => Sync::Normal,
                },
                cache_size: if cache_bytes == 0 {
                    crate::storage::pager::DEFAULT_CACHE_BYTES
                } else {
                    cache_bytes as u64
                },
                ..crate::OpenOptions::default()
            };
            let graph = Graph::open_opts(Path::new(path), opts).map_err(|e| e.to_string())?;
            Ok(Box::into_raw(Box::new(GliderDb { graph })))
        })
    }
}

/// A `:memory:` graph that may occupy at most `max_bytes` (0 = the
/// machine's physical memory). Past the limit, writes fail with an error
/// and are rolled back; the graph stays usable. NULL on error.
#[no_mangle]
pub extern "C" fn glider_open_memory_ex(max_bytes: u64) -> *mut GliderDb {
    guard(std::ptr::null_mut(), || {
        let graph = if max_bytes == 0 {
            Graph::memory()
        } else {
            Graph::memory_with_limit(max_bytes)
        };
        Ok(Box::into_raw(Box::new(GliderDb { graph })))
    })
}

/// A graph that never touches disk. Useful for tests, caches, and scratch
/// work on a device where you do not want to spend storage.
#[no_mangle]
pub extern "C" fn glider_open_memory() -> *mut GliderDb {
    guard(std::ptr::null_mut(), || {
        Ok(Box::into_raw(Box::new(GliderDb {
            graph: Graph::memory(),
        })))
    })
}

/// An in-memory graph loaded from the bytes of a `.gldb` file. Edits are not
/// written back anywhere. For hosts with no filesystem, chiefly wasm, where
/// the embedder reads the file and passes the bytes in. NULL on error.
///
/// # Safety
/// `bytes` must point to `len` readable bytes (or be NULL with `len` 0).
#[no_mangle]
pub unsafe extern "C" fn glider_open_bytes(bytes: *const u8, len: usize) -> *mut GliderDb {
    unsafe {
        guard(std::ptr::null_mut(), || {
            let data: &[u8] = if len == 0 {
                &[]
            } else if bytes.is_null() {
                return Err("bytes is NULL".into());
            } else {
                std::slice::from_raw_parts(bytes, len)
            };
            let graph = Graph::from_bytes(data).map_err(|e| e.to_string())?;
            Ok(Box::into_raw(Box::new(GliderDb { graph })))
        })
    }
}

/// Flush, close and free the handle. Safe to call with NULL.
///
/// # Safety
/// `db` must have come from `glider_open`/`glider_open_memory` and must not be
/// used afterwards.
#[no_mangle]
pub unsafe extern "C" fn glider_close(db: *mut GliderDb) {
    unsafe {
        if db.is_null() {
            return;
        }
        let _ = guard(0, || {
            let mut boxed = Box::from_raw(db);
            let _ = boxed.graph.checkpoint();
            drop(boxed);
            Ok(0)
        });
    }
}

// --------------------------------------------------------------------- query

/// Run one statement. Returns a JSON document
/// `{"columns":[...],"rows":[...],"message":...,"touched":n}`, or NULL on
/// error. Free the result with `glider_free`.
///
/// # Safety
/// `db` must be a live handle; `q` a NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn glider_query(db: *mut GliderDb, q: *const c_char) -> *mut c_char {
    unsafe {
        guard(std::ptr::null_mut(), || {
            let db = as_db(db)?;
            let src = as_str(q, "query")?;
            let result = query::execute(&mut db.graph, src).map_err(|e| e.to_string())?;
            out_string(result.to_json())
        })
    }
}

/// Node and edge counts, label and type histograms, file size — as JSON.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn glider_stats(db: *mut GliderDb) -> *mut c_char {
    unsafe {
        guard(std::ptr::null_mut(), || {
            let db = as_db(db)?;
            let result = query::execute(&mut db.graph, "STATS").map_err(|e| e.to_string())?;
            out_string(result.to_json())
        })
    }
}

// ------------------------------------------------------------------ durability

/// Force committed writes to durable storage. Returns 0 on success.
///
/// Call this from `sceneDidEnterBackground` on iOS and `onStop` on Android.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn glider_checkpoint(db: *mut GliderDb) -> c_int {
    unsafe {
        guard(-1, || {
            let db = as_db(db)?;
            db.graph.checkpoint().map_err(|e| e.to_string())?;
            Ok(0)
        })
    }
}

/// On paged storage, the same as `glider_checkpoint`: fold the log into the
/// pages. Freed pages are reused as the database grows, so there is no
/// separate rewrite step. Kept for callers written against the older engine.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn glider_compact(db: *mut GliderDb) -> c_int {
    unsafe {
        guard(-1, || {
            let db = as_db(db)?;
            db.graph.compact().map_err(|e| e.to_string())?;
            Ok(0)
        })
    }
}

/// Checkpoint automatically once this many bytes of write-ahead log have
/// accumulated (0 = only on close or `glider_checkpoint`). Bounds the time a
/// crash recovery can take. Default 256 MiB.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn glider_set_auto_compact(db: *mut GliderDb, bytes: u64) -> c_int {
    unsafe {
        guard(-1, || {
            let db = as_db(db)?;
            db.graph.set_checkpoint_bytes(if bytes == 0 { u64::MAX } else { bytes });
            Ok(0)
        })
    }
}

/// Change durability at runtime: 0 = always, 1 = normal, 2 = off.
/// Bulk import is much faster with 2, as long as you checkpoint after.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn glider_set_sync(db: *mut GliderDb, sync: c_int) -> c_int {
    unsafe {
        guard(-1, || {
            let db = as_db(db)?;
            db.graph.set_sync(match sync {
                0 => Sync::Always,
                2 => Sync::Off,
                _ => Sync::Normal,
            });
            Ok(0)
        })
    }
}

// ------------------------------------------------------------------ transfer

/// Load newline-delimited JSON. Returns the number of records applied, or -1.
///
/// # Safety
/// `db` must be a live handle; `jsonl` a NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn glider_import_jsonl(db: *mut GliderDb, jsonl: *const c_char) -> c_int {
    unsafe {
        guard(-1, || {
            let db = as_db(db)?;
            let text = as_str(jsonl, "jsonl")?;
            let (n, e) = query::import_jsonl(&mut db.graph, text).map_err(|x| x.to_string())?;
            Ok((n + e) as c_int)
        })
    }
}

/// Dump the whole graph as newline-delimited JSON. Deterministic, so it diffs
/// cleanly. Free with `glider_free`.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn glider_export_jsonl(db: *mut GliderDb) -> *mut c_char {
    unsafe {
        guard(std::ptr::null_mut(), || {
            let db = as_db(db)?;
            out_string(query::export_jsonl(&db.graph))
        })
    }
}

// -------------------------------------------------------------------- errors

/// The last error on *this thread*, or NULL if the last call succeeded.
/// Borrowed — do not free it, and copy it before your next glider call.
#[no_mangle]
pub extern "C" fn glider_last_error() -> *const c_char {
    LAST_ERROR.with(|e| match &*e.borrow() {
        Some(c) => c.as_ptr(),
        None => std::ptr::null(),
    })
}

/// Free a string returned by this library.
///
/// # Safety
/// `p` must have come from a glider function that returns `char *`, and must
/// not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn glider_free(p: *mut c_char) {
    unsafe {
        if !p.is_null() {
            drop(CString::from_raw(p));
        }
    }
}

/// Library version, statically allocated. Do not free.
#[no_mangle]
pub extern "C" fn glider_version() -> *const c_char {
    static V: OnceLock<CString> = OnceLock::new();
    V.get_or_init(|| CString::new(crate::VERSION).unwrap())
        .as_ptr()
}

// --------------------------------------------------------- typed JSON API

/// Run a query and return the *typed* result used by the browser console and
/// the TypeScript bindings.
///
/// Unlike `glider_query`, nodes and relationships come back as real JSON
/// objects tagged `"_e":"node"` / `"_e":"rel"`, plus a deduplicated
/// `graph:{nodes,edges}` payload ready to draw. See `api.rs` for why the flat
/// form cannot be parsed reliably by a client.
///
/// # Safety
/// `q` must be a NUL-terminated UTF-8 string. Free the result with
/// `glider_free`.
#[no_mangle]
pub unsafe extern "C" fn glider_query_json(db: *mut GliderDb, q: *const c_char) -> *mut c_char {
    unsafe {
        guard(std::ptr::null_mut(), || {
            let db = as_db(db)?;
            let src = as_str(q, "query")?;
            out_string(crate::api::query_json(&mut db.graph, src)?)
        })
    }
}

/// Labels, relationship types and indexes with counts, as JSON.
///
/// # Safety
/// `db` must be a live handle. Free the result with `glider_free`.
#[no_mangle]
pub unsafe extern "C" fn glider_schema_json(db: *mut GliderDb) -> *mut c_char {
    unsafe {
        guard(std::ptr::null_mut(), || {
            let db = as_db(db)?;
            out_string(crate::api::schema_json(&mut db.graph)?)
        })
    }
}

/// Neighbours of one node as a drawable `{graph:{nodes,edges}}` payload.
///
/// # Safety
/// `db` must be a live handle. Free the result with `glider_free`.
#[no_mangle]
pub unsafe extern "C" fn glider_expand_json(
    db: *mut GliderDb,
    id: u64,
    limit: usize,
) -> *mut c_char {
    unsafe {
        guard(std::ptr::null_mut(), || {
            let db = as_db(db)?;
            out_string(crate::api::expand_json(&db.graph, id, limit.min(10_000))?)
        })
    }
}

/// Optional C string: NULL means "not given" rather than an error.
unsafe fn as_opt_str<'a>(p: *const c_char, what: &str) -> Result<Option<&'a str>, String> {
    if p.is_null() {
        return Ok(None);
    }
    unsafe { as_str(p, what).map(Some) }
}

/// A page of nodes for the explorer: `{nodes, next, total}`. `label` and `q`
/// may be NULL; `from` is the id cursor (0 for the first page).
///
/// # Safety
/// `db` must be a live handle; `label` and `q` NULL or NUL-terminated UTF-8.
/// Free the result with `glider_free`.
#[no_mangle]
pub unsafe extern "C" fn glider_nodes_json(
    db: *mut GliderDb,
    label: *const c_char,
    q: *const c_char,
    from: u64,
    limit: usize,
) -> *mut c_char {
    unsafe {
        guard(std::ptr::null_mut(), || {
            let db = as_db(db)?;
            let label = as_opt_str(label, "label")?;
            let q = as_opt_str(q, "q")?;
            out_string(crate::api::nodes_json(
                &db.graph,
                label,
                q,
                from,
                limit.clamp(1, 1000),
            ))
        })
    }
}

/// A page of edges with their endpoints: `{edges, nodes, next, total}`.
/// Same contract as `glider_nodes_json`, filtering by relationship `etype`.
///
/// # Safety
/// As `glider_nodes_json`.
#[no_mangle]
pub unsafe extern "C" fn glider_edges_json(
    db: *mut GliderDb,
    etype: *const c_char,
    q: *const c_char,
    from: u64,
    limit: usize,
) -> *mut c_char {
    unsafe {
        guard(std::ptr::null_mut(), || {
            let db = as_db(db)?;
            let etype = as_opt_str(etype, "type")?;
            let q = as_opt_str(q, "q")?;
            out_string(crate::api::edges_json(
                &db.graph,
                etype,
                q,
                from,
                limit.clamp(1, 1000),
            ))
        })
    }
}

// ------------------------------------------------------------------ telemetry
//
// The engine keeps the numbers; the host exports them (see
// docs/OBSERVABILITY.md). Everything here is available on every target,
// wasm included, except starting the built-in OTLP exporter, which needs
// threads and sockets.

/// The process-wide counters as JSON: statements by operation and outcome,
/// rows, touched, page traffic, and the duration histogram.
#[no_mangle]
pub extern "C" fn glider_telemetry_json() -> *mut c_char {
    guard(std::ptr::null_mut(), || out_string(crate::telemetry::snapshot().to_json()))
}

/// The calling thread's most recent statement as JSON — operation, rows,
/// touched, page reads/hits/misses, duration where the target has a clock,
/// error — or NULL if this thread has run none. Read it right after a call to
/// annotate the host's own span for that call.
#[no_mangle]
pub extern "C" fn glider_last_op_json() -> *mut c_char {
    guard(std::ptr::null_mut(), || match crate::telemetry::last_op() {
        Some(r) => out_string(r.to_json()),
        None => Ok(std::ptr::null_mut()),
    })
}

/// One database's state as JSON: its telemetry name, counts, size, cache and
/// I/O counters, log.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn glider_db_metrics_json(db: *mut GliderDb) -> *mut c_char {
    unsafe {
        guard(std::ptr::null_mut(), || {
            let db = as_db(db)?;
            let mut out = String::from("{\"name\":");
            crate::value::write_json_string(&db.graph.telemetry_name(), &mut out);
            out.push(',');
            out.push_str(&db.graph.telemetry().to_json()[1..]);
            out_string(out)
        })
    }
}

/// `(name, metrics)` for each non-NULL handle in `dbs[..n]`.
unsafe fn db_list(dbs: *const *mut GliderDb, n: usize) -> Vec<(String, crate::telemetry::DbMetrics)> {
    unsafe {
        if dbs.is_null() {
            return Vec::new();
        }
        std::slice::from_raw_parts(dbs, n)
            .iter()
            .filter(|db| !db.is_null())
            .map(|db| {
                let g = &(**db).graph;
                (g.telemetry_name(), g.telemetry())
            })
            .collect()
    }
}

/// Process counters, plus the state of the `n` databases at `dbs` (NULL
/// and 0 for none), as an OTLP/HTTP JSON metrics request ready to POST to
/// `<collector>/v1/metrics`. `service` (NULL for "glider") becomes
/// `service.name`. `now_unix_ms` is the host's clock; pass 0 to use the
/// engine's (not available under wasm).
///
/// # Safety
/// `dbs` must be NULL or point to `n` handles, each NULL or live; `service`
/// NULL or a NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn glider_metrics_otlp(
    dbs: *const *mut GliderDb,
    n: usize,
    service: *const c_char,
    now_unix_ms: f64,
) -> *mut c_char {
    unsafe {
        guard(std::ptr::null_mut(), || {
            let service = as_opt_str(service, "service")?.unwrap_or("glider");
            let now = if now_unix_ms > 0.0 {
                (now_unix_ms * 1e6) as u64
            } else {
                crate::telemetry::now_unix_ns().ok_or("no clock on this target: pass now_unix_ms")?
            };
            let res = crate::telemetry::default_resource(service);
            out_string(crate::telemetry::otlp_metrics_json(&res, now, &db_list(dbs, n)))
        })
    }
}

/// Process counters, plus the state of the `n` databases at `dbs`, in the
/// Prometheus text format.
///
/// # Safety
/// As `glider_metrics_otlp`.
#[no_mangle]
pub unsafe extern "C" fn glider_metrics_prometheus(dbs: *const *mut GliderDb, n: usize) -> *mut c_char {
    unsafe { guard(std::ptr::null_mut(), || out_string(crate::telemetry::prometheus(&db_list(dbs, n)))) }
}

/// Add a duration the host measured to the statement-duration histogram.
/// For hosts where the engine has no clock (wasm); native builds time
/// statements themselves, so calling this there counts them twice.
#[no_mangle]
pub extern "C" fn glider_observe_duration_ms(ms: f64) {
    if ms.is_finite() && ms >= 0.0 {
        crate::telemetry::observe_duration((ms * 1e6) as u64);
    }
}

/// Start the built-in OTLP/HTTP exporter from the standard `OTEL_*`
/// environment variables, with `service` (NULL for "glider") as the default
/// `service.name`. Returns 1 if an exporter is running, 0 if the environment
/// asks for none (no endpoint, or `OTEL_SDK_DISABLED`), -1 on a bad
/// configuration. Once started, every statement becomes a span and every
/// open database is reported with the metrics. Idempotent.
///
/// # Safety
/// `service` must be NULL or a NUL-terminated UTF-8 string.
#[cfg(not(target_arch = "wasm32"))]
#[no_mangle]
pub unsafe extern "C" fn glider_telemetry_start(service: *const c_char) -> c_int {
    unsafe {
        guard(-1, || {
            let service = as_opt_str(service, "service")?.unwrap_or("glider");
            crate::telemetry::otlp::install_from_env(service).map(|on| on as c_int)
        })
    }
}

/// Push pending spans and metrics now. Call before the process exits.
#[cfg(not(target_arch = "wasm32"))]
#[no_mangle]
pub extern "C" fn glider_telemetry_flush() {
    crate::telemetry::otlp::flush();
}

/// Parent the calling thread's following statements under a W3C
/// `traceparent` (e.g. the incoming request's). NULL clears it. Returns 0,
/// or -1 if the header does not parse.
///
/// # Safety
/// `traceparent` must be NULL or a NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn glider_trace_context(traceparent: *const c_char) -> c_int {
    unsafe {
        guard(-1, || {
            let ctx = match as_opt_str(traceparent, "traceparent")? {
                None => None,
                Some(s) => Some(crate::telemetry::TraceContext::parse(s).ok_or("not a valid traceparent")?),
            };
            crate::telemetry::set_context(ctx);
            Ok(0)
        })
    }
}

// ------------------------------------------------------- guest-side memory
//
// A C caller has malloc. A WebAssembly caller does not: JavaScript cannot put
// a string into the module's linear memory without asking the module to
// reserve the bytes first. These two exports are that mechanism.
//
// They are a *separate* allocation channel from `glider_free`. Buffers from
// `glider_alloc` go back to `glider_dealloc` with the same length; strings
// returned by glider functions go to `glider_free`. Crossing the two is
// undefined behaviour, because the layouts differ.

/// Reserve `len` bytes inside the module's memory and return a pointer.
/// Returns NULL if `len` is 0 or the allocation fails.
#[no_mangle]
pub extern "C" fn glider_alloc(len: usize) -> *mut u8 {
    if len == 0 {
        return std::ptr::null_mut();
    }
    let mut buf = Vec::<u8>::new();
    if buf.try_reserve_exact(len).is_err() {
        return std::ptr::null_mut();
    }
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

/// Release a buffer obtained from `glider_alloc`.
///
/// # Safety
/// `ptr` must have come from `glider_alloc` with exactly this `len`, and must
/// not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn glider_dealloc(ptr: *mut u8, len: usize) {
    unsafe {
        if !ptr.is_null() && len != 0 {
            drop(Vec::from_raw_parts(ptr, 0, len));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cstr(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    #[test]
    fn round_trip_through_the_c_abi() {
        unsafe {
            let db = glider_open_memory();
            assert!(!db.is_null());

            let q = cstr(r#"CREATE (a:Person {name:"Ada"})-[:KNOWS]->(b:Person {name:"Bob"})"#);
            let out = glider_query(db, q.as_ptr());
            assert!(!out.is_null());
            glider_free(out);

            let q = cstr("MATCH (a)-[:KNOWS]->(b) RETURN b.name");
            let out = glider_query(db, q.as_ptr());
            let json = CStr::from_ptr(out).to_str().unwrap().to_string();
            assert!(json.contains("Bob"), "{json}");
            glider_free(out);

            assert_eq!(glider_checkpoint(db), 0);
            glider_close(db);
        }
    }

    #[test]
    fn bad_input_sets_an_error_instead_of_unwinding() {
        unsafe {
            let db = glider_open_memory();
            let q = cstr("MATCH (((");
            assert!(glider_query(db, q.as_ptr()).is_null());
            let err = glider_last_error();
            assert!(!err.is_null());
            assert!(glider_query(std::ptr::null_mut(), q.as_ptr()).is_null());
            glider_close(db);
        }
    }
}
