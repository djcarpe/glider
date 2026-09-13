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
glider_db *glider_open_memory(void);
void       glider_close(glider_db *db);

/* --- queries ----------------------------------------------------------- */

/* Returns {"columns":[...],"rows":[[...]],"message":...,"touched":n} */
char *glider_query(glider_db *db, const char *query);
char *glider_stats(glider_db *db);

/* --- durability -------------------------------------------------------- */

int glider_checkpoint(glider_db *db); /* call when the app backgrounds */
int glider_compact(glider_db *db);    /* slow; not on the UI thread    */
int glider_set_sync(glider_db *db, int sync);

/* --- bulk transfer ----------------------------------------------------- */

int   glider_import_jsonl(glider_db *db, const char *jsonl);
char *glider_export_jsonl(glider_db *db);

/* --- misc -------------------------------------------------------------- */

const char *glider_last_error(void); /* borrowed, thread-local, may be NULL */
const char *glider_version(void);    /* borrowed, static                    */
void        glider_free(char *s);

#ifdef __cplusplus
}
#endif

#endif /* GLIDER_H */
