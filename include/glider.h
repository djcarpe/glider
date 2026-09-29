/* glider.h — embeddable property-graph database with graph algorithms.
 *
 * Link against libglider.a (iOS, desktop static) or libglider.so/.dll/.dylib.
 *
 * Memory:  every char* returned here is Rust-allocated. Return it with
 *          glider_free(). Never free(3) it. glider_last_error() and
 *          glider_version() return borrowed pointers — do not free those.
 * Threads: a glider_db is NOT thread-safe. One handle per thread, or hold
 *          your own mutex. The error slot is thread-local.
 * Errors:  pointer-returning calls give NULL on failure, int-returning calls
 *          give -1. glider_last_error() has the message.
 */

#ifndef GLIDER_H
#define GLIDER_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct GliderDb glider_db;

/* Durability modes for glider_open / glider_set_sync. */
#define GLIDER_SYNC_ALWAYS 0 /* fsync every commit: survives power loss  */
#define GLIDER_SYNC_NORMAL 1 /* survives process death, not power loss   */
#define GLIDER_SYNC_OFF    2 /* buffered; for bulk load                  */

/* --- lifetime ---------------------------------------------------------- */

glider_db *glider_open(const char *path, int sync);
/* cache_bytes: the page cache (0 = the default, 1 GiB). RAM use stays near
   it however large the database grows; the database is limited by disk. */
glider_db *glider_open_ex(const char *path, int sync, size_t cache_bytes);
glider_db *glider_open_memory(void);
/* An in-memory graph of at most max_bytes (0 = physical memory). Past it,
   writes fail with an error and roll back; the graph stays usable. */
glider_db *glider_open_memory_ex(uint64_t max_bytes);
/* An in-memory graph from the bytes of a .gldb file; edits are not written back. */
glider_db *glider_open_bytes(const unsigned char *bytes, size_t len);
void       glider_close(glider_db *db);

/* --- queries ----------------------------------------------------------- */

/* Returns {"columns":[...],"rows":[[...]],"message":...,"touched":n} */
char *glider_query(glider_db *db, const char *query);
char *glider_stats(glider_db *db);

/* --- durability -------------------------------------------------------- */

int glider_checkpoint(glider_db *db); /* call when the app backgrounds */
int glider_compact(glider_db *db);    /* a checkpoint, on paged storage */
int glider_set_sync(glider_db *db, int sync);
/* Checkpoint automatically once the write-ahead log passes `bytes`; 0 = off.
   Default 256 MiB. Runs inside the commit that crosses it. */
int glider_set_auto_compact(glider_db *db, uint64_t bytes);

/* --- bulk transfer ----------------------------------------------------- */

int   glider_import_jsonl(glider_db *db, const char *jsonl);
char *glider_export_jsonl(glider_db *db);

/* --- telemetry (docs/OBSERVABILITY.md) --------------------------------- */

/* Process-wide counters and duration histogram, as JSON. */
char *glider_telemetry_json(void);
/* The calling thread's last statement: op, rows, touched, page reads/hits/
   misses, duration_ns, error. NULL if none yet. */
char *glider_last_op_json(void);
/* One database's counts, size, cache and I/O counters, as JSON. */
char *glider_db_metrics_json(glider_db *db);
/* OTLP/HTTP JSON for <collector>/v1/metrics: process counters plus the n
   databases at dbs (NULL, 0 for none). service may be NULL; now_unix_ms 0
   uses the engine's clock. */
char *glider_metrics_otlp(glider_db *const *dbs, size_t n, const char *service,
                          double now_unix_ms);
/* The same in the Prometheus text exposition format. */
char *glider_metrics_prometheus(glider_db *const *dbs, size_t n);
/* Parent this thread's following statements under a W3C traceparent; NULL
   clears. Returns 0, or -1 if it does not parse. */
int   glider_trace_context(const char *traceparent);
/* Start the built-in OTLP exporter from OTEL_* environment variables.
   1 = running, 0 = not configured, -1 = bad configuration. Not in wasm. */
int   glider_telemetry_start(const char *service);
/* Push pending spans and metrics now; call before exit. Not in wasm. */
void  glider_telemetry_flush(void);
/* Feed a host-measured duration into the histogram (wasm hosts only). */
void  glider_observe_duration_ms(double ms);

/* --- misc -------------------------------------------------------------- */

const char *glider_last_error(void); /* borrowed, thread-local, may be NULL */
const char *glider_version(void);    /* borrowed, static                    */
void        glider_free(char *s);

#ifdef __cplusplus
}
#endif

#endif /* GLIDER_H */
