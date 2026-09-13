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
    if p.is_null() {
        return Err(format!("{what} was NULL"));
    }
    CStr::from_ptr(p)
        .to_str()
        .map_err(|_| format!("{what} was not valid UTF-8"))
}

unsafe fn as_db<'a>(p: *mut GliderDb) -> Result<&'a mut GliderDb, String> {
    if p.is_null() {
        return Err("database handle was NULL".into());
    }
    Ok(&mut *p)
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

/// Flush, close and free the handle. Safe to call with NULL.
///
/// # Safety
/// `db` must have come from `glider_open`/`glider_open_memory` and must not be
/// used afterwards.
#[no_mangle]
pub unsafe extern "C" fn glider_close(db: *mut GliderDb) {
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

// --------------------------------------------------------------------- query

/// Run one statement. Returns a JSON document
/// `{"columns":[...],"rows":[...],"message":...,"touched":n}`, or NULL on
/// error. Free the result with `glider_free`.
///
/// # Safety
/// `db` must be a live handle; `q` a NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn glider_query(db: *mut GliderDb, q: *const c_char) -> *mut c_char {
    guard(std::ptr::null_mut(), || {
        let db = as_db(db)?;
        let src = as_str(q, "query")?;
        let result = query::execute(&mut db.graph, src).map_err(|e| e.to_string())?;
        out_string(result.to_json())
    })
}

/// Node and edge counts, label and type histograms, file size — as JSON.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn glider_stats(db: *mut GliderDb) -> *mut c_char {
    guard(std::ptr::null_mut(), || {
        let db = as_db(db)?;
        let result = query::execute(&mut db.graph, "STATS").map_err(|e| e.to_string())?;
        out_string(result.to_json())
    })
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
    guard(-1, || {
        let db = as_db(db)?;
        db.graph.checkpoint().map_err(|e| e.to_string())?;
        Ok(0)
    })
}

/// Rewrite the log as a minimal snapshot, reclaiming deleted space.
/// Potentially slow and I/O heavy — do not call it on the UI thread, and on
/// iOS wrap it in a background task so the OS does not suspend you mid-write.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn glider_compact(db: *mut GliderDb) -> c_int {
    guard(-1, || {
        let db = as_db(db)?;
        db.graph.compact().map_err(|e| e.to_string())?;
        Ok(0)
    })
}

/// Change durability at runtime: 0 = always, 1 = normal, 2 = off.
/// Bulk import is much faster with 2, as long as you checkpoint after.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn glider_set_sync(db: *mut GliderDb, sync: c_int) -> c_int {
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

// ------------------------------------------------------------------ transfer

/// Load newline-delimited JSON. Returns the number of records applied, or -1.
///
/// # Safety
/// `db` must be a live handle; `jsonl` a NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn glider_import_jsonl(db: *mut GliderDb, jsonl: *const c_char) -> c_int {
    guard(-1, || {
        let db = as_db(db)?;
        let text = as_str(jsonl, "jsonl")?;
        let (n, e) = query::import_jsonl(&mut db.graph, text).map_err(|x| x.to_string())?;
        Ok((n + e) as c_int)
    })
}

/// Dump the whole graph as newline-delimited JSON. Deterministic, so it diffs
/// cleanly. Free with `glider_free`.
///
/// # Safety
/// `db` must be a live handle.
#[no_mangle]
pub unsafe extern "C" fn glider_export_jsonl(db: *mut GliderDb) -> *mut c_char {
    guard(std::ptr::null_mut(), || {
        let db = as_db(db)?;
        out_string(query::export_jsonl(&db.graph))
    })
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
    if !p.is_null() {
        drop(CString::from_raw(p));
    }
}

/// Library version, statically allocated. Do not free.
#[no_mangle]
pub extern "C" fn glider_version() -> *const c_char {
    static V: OnceLock<CString> = OnceLock::new();
    V.get_or_init(|| CString::new(crate::VERSION).unwrap()).as_ptr()
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
    guard(std::ptr::null_mut(), || {
        let db = as_db(db)?;
        let src = as_str(q, "query")?;
        out_string(crate::api::query_json(&mut db.graph, src)?)
    })
}

/// Labels, relationship types and indexes with counts, as JSON.
///
/// # Safety
/// `db` must be a live handle. Free the result with `glider_free`.
#[no_mangle]
pub unsafe extern "C" fn glider_schema_json(db: *mut GliderDb) -> *mut c_char {
    guard(std::ptr::null_mut(), || {
        let db = as_db(db)?;
        out_string(crate::api::schema_json(&mut db.graph)?)
    })
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
    guard(std::ptr::null_mut(), || {
        let db = as_db(db)?;
        out_string(crate::api::expand_json(&db.graph, id, limit.min(10_000))?)
    })
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
    if !ptr.is_null() && len != 0 {
        drop(Vec::from_raw_parts(ptr, 0, len));
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
