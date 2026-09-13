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
 * What does not work under wasm: anything file-backed. `wasm32-unknown-unknown`
 * has no filesystem, so graphs are in-memory only. Persist by exporting JSONL
 * and storing that yourself (IndexedDB, OPFS, a fetch to your server).
 */
import { type Cell, type GliderNode, type GliderRel, type QueryResult, type Schema } from './types.js';
export * from './types.js';
/** The raw exports glider's wasm module provides. */
interface Exports {
    memory: WebAssembly.Memory;
    glider_open_memory(): number;
    glider_close(db: number): void;
    glider_query(db: number, q: number): number;
    glider_query_json(db: number, q: number): number;
    glider_schema_json(db: number): number;
    glider_expand_json(db: number, id: bigint, limit: number): number;
    glider_import_jsonl(db: number, jsonl: number): number;
    glider_export_jsonl(db: number): number;
    glider_stats(db: number): number;
    glider_compact(db: number): number;
    glider_last_error(): number;
    glider_free(p: number): void;
    glider_alloc(len: number): number;
    glider_dealloc(p: number, len: number): void;
    glider_version(): number;
}
/** Anything we know how to turn into wasm bytes. */
export type WasmSource = BufferSource | WebAssembly.Module | Response | Promise<Response> | URL | string;
/**
 * Compile and instantiate the glider wasm module.
 *
 * With no argument it looks for `glider.wasm` next to this file, which is how
 * the published package is laid out. Pass a source explicitly when bundling,
 * or when serving the binary from somewhere else.
 */
export declare function loadGlider(source?: WasmSource): Promise<GliderModule>;
/** An instantiated module. Cheap to keep; each `open()` is an isolated graph. */
export declare class GliderModule {
    #private;
    constructor(exports: Exports);
    /** glider's version string. */
    get version(): string;
    /** Open a fresh in-memory graph. */
    open(): GliderDb;
    /** Read a NUL-terminated string and free it. */
    /** @internal */
    take(ptr: number): string | null;
    /** @internal */
    lastError(): string | null;
    /**
     * Copy a string into the module as NUL-terminated UTF-8 and run `fn` with
     * the pointer, always releasing it afterwards.
     */
    /** @internal */
    withCString<T>(s: string, fn: (ptr: number) => T): T;
    /** @internal */
    get raw(): Exports;
}
/** One graph. Not shared between workers — wasm memory is per-instance. */
export declare class GliderDb {
    #private;
    private readonly mod;
    constructor(mod: GliderModule, handle: number);
    /** Run a query and return the typed result, including the graph payload. */
    query(cypher: string): QueryResult;
    /**
     * Run a statement for its effect and return how many entities it touched.
     * Sugar over `query` for writes, where the rows are empty anyway.
     */
    run(cypher: string): number;
    /** Every node and relationship the query produced, ready to draw. */
    graph(cypher: string): QueryResult['graph'];
    /** Labels, relationship types and indexes, with counts. */
    schema(): Schema;
    /** Neighbours of one node, both directions, capped by `limit`. */
    expand(id: number, limit?: number): QueryResult['graph'];
    /** Bulk load JSON Lines. Returns the number of entities imported. */
    importJsonl(jsonl: string): number;
    /**
     * Dump the whole graph as JSON Lines. Under wasm this is how you persist:
     * hand the string to IndexedDB, OPFS, or your own server.
     */
    exportJsonl(): string;
    /** Node, edge, label and index counts. */
    stats(): QueryResult;
    /** Release the graph. Safe to call twice. */
    close(): void;
    /** Lets `using db = glider.open()` work under TS 5.2+ explicit resource management. */
    [Symbol.dispose](): void;
}
export type { Cell, GliderNode, GliderRel };
//# sourceMappingURL=index.d.ts.map