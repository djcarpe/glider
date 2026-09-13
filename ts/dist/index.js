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
import { GliderError, } from './types.js';
export * from './types.js';
/**
 * Compile and instantiate the glider wasm module.
 *
 * With no argument it looks for `glider.wasm` next to this file, which is how
 * the published package is laid out. Pass a source explicitly when bundling,
 * or when serving the binary from somewhere else.
 */
export async function loadGlider(source) {
    const instance = await instantiate(source ?? new URL('./glider.wasm', import.meta.url));
    return new GliderModule(instance.exports);
}
async function instantiate(source) {
    const imports = {};
    if (source instanceof WebAssembly.Module) {
        return new WebAssembly.Instance(source, imports);
    }
    if (isBufferSource(source)) {
        return (await WebAssembly.instantiate(source, imports)).instance;
    }
    // A URL or path. In a browser or Deno, stream it. In Node, read it off disk,
    // because file: URLs cannot be fetched and streaming compilation would need
    // a Response we do not have.
    if (source instanceof URL || typeof source === 'string') {
        const url = source instanceof URL ? source : new URL(source, import.meta.url);
        if (url.protocol === 'file:') {
            const { readFile } = await import('node:fs/promises');
            const bytes = await readFile(url);
            return (await WebAssembly.instantiate(bytes, imports)).instance;
        }
        return instantiateResponse(fetch(url), imports);
    }
    return instantiateResponse(source, imports);
}
async function instantiateResponse(res, imports) {
    // instantiateStreaming needs the right Content-Type and is not universally
    // available; fall back to buffering rather than failing.
    if (typeof WebAssembly.instantiateStreaming === 'function') {
        try {
            return (await WebAssembly.instantiateStreaming(res, imports)).instance;
        }
        catch {
            /* fall through */
        }
    }
    const bytes = await (await res).arrayBuffer();
    return (await WebAssembly.instantiate(bytes, imports)).instance;
}
function isBufferSource(v) {
    return v instanceof ArrayBuffer || ArrayBuffer.isView(v);
}
/** An instantiated module. Cheap to keep; each `open()` is an isolated graph. */
export class GliderModule {
    #e;
    #dec = new TextDecoder();
    #enc = new TextEncoder();
    constructor(exports) {
        this.#e = exports;
    }
    /** glider's version string. */
    get version() {
        return this.#borrow(this.#e.glider_version()) ?? 'unknown';
    }
    /** Open a fresh in-memory graph. */
    open() {
        const handle = this.#e.glider_open_memory();
        if (!handle)
            throw new GliderError(this.#lastError() ?? 'could not open a graph');
        return new GliderDb(this, handle);
    }
    // ---- internals used by GliderDb ------------------------------------
    /** Bytes of linear memory. Re-read every time: growth detaches old views. */
    #bytes() {
        return new Uint8Array(this.#e.memory.buffer);
    }
    /** Read a NUL-terminated string without taking ownership. */
    #borrow(ptr) {
        if (!ptr)
            return null;
        const m = this.#bytes();
        let end = ptr;
        while (m[end] !== 0)
            end++;
        return this.#dec.decode(m.subarray(ptr, end));
    }
    /** Read a NUL-terminated string and free it. */
    /** @internal */
    take(ptr) {
        const s = this.#borrow(ptr);
        if (ptr)
            this.#e.glider_free(ptr);
        return s;
    }
    /** @internal */
    lastError() {
        return this.#lastError();
    }
    #lastError() {
        return this.#borrow(this.#e.glider_last_error());
    }
    /**
     * Copy a string into the module as NUL-terminated UTF-8 and run `fn` with
     * the pointer, always releasing it afterwards.
     */
    /** @internal */
    withCString(s, fn) {
        const body = this.#enc.encode(s);
        const len = body.length + 1;
        const ptr = this.#e.glider_alloc(len);
        if (!ptr)
            throw new GliderError(`could not allocate ${len} bytes in wasm memory`);
        try {
            // Take the view *after* alloc: growing memory detaches earlier views.
            const m = this.#bytes();
            m.set(body, ptr);
            m[ptr + body.length] = 0;
            return fn(ptr);
        }
        finally {
            this.#e.glider_dealloc(ptr, len);
        }
    }
    /** @internal */
    get raw() {
        return this.#e;
    }
}
/** One graph. Not shared between workers — wasm memory is per-instance. */
export class GliderDb {
    mod;
    #handle;
    constructor(mod, handle) {
        this.mod = mod;
        this.#handle = handle;
    }
    #alive() {
        if (!this.#handle)
            throw new GliderError('this graph is closed');
        return this.#handle;
    }
    /** Run a query and return the typed result, including the graph payload. */
    query(cypher) {
        const db = this.#alive();
        const json = this.mod.withCString(cypher, (p) => this.mod.take(this.mod.raw.glider_query_json(db, p)));
        if (json === null) {
            throw new GliderError(this.mod.lastError() ?? 'query failed', cypher);
        }
        return JSON.parse(json);
    }
    /**
     * Run a statement for its effect and return how many entities it touched.
     * Sugar over `query` for writes, where the rows are empty anyway.
     */
    run(cypher) {
        return this.query(cypher).touched;
    }
    /** Every node and relationship the query produced, ready to draw. */
    graph(cypher) {
        return this.query(cypher).graph;
    }
    /** Labels, relationship types and indexes, with counts. */
    schema() {
        const db = this.#alive();
        const json = this.mod.take(this.mod.raw.glider_schema_json(db));
        if (json === null)
            throw new GliderError(this.mod.lastError() ?? 'schema failed');
        return JSON.parse(json);
    }
    /** Neighbours of one node, both directions, capped by `limit`. */
    expand(id, limit = 50) {
        const db = this.#alive();
        const json = this.mod.take(this.mod.raw.glider_expand_json(db, BigInt(id), limit));
        if (json === null)
            throw new GliderError(this.mod.lastError() ?? `could not expand node ${id}`);
        return JSON.parse(json).graph;
    }
    /** Bulk load JSON Lines. Returns the number of entities imported. */
    importJsonl(jsonl) {
        const db = this.#alive();
        const rc = this.mod.withCString(jsonl, (p) => this.mod.raw.glider_import_jsonl(db, p));
        if (rc < 0)
            throw new GliderError(this.mod.lastError() ?? 'import failed');
        return rc;
    }
    /**
     * Dump the whole graph as JSON Lines. Under wasm this is how you persist:
     * hand the string to IndexedDB, OPFS, or your own server.
     */
    exportJsonl() {
        const db = this.#alive();
        const s = this.mod.take(this.mod.raw.glider_export_jsonl(db));
        if (s === null)
            throw new GliderError(this.mod.lastError() ?? 'export failed');
        return s;
    }
    /** Node, edge, label and index counts. */
    stats() {
        const db = this.#alive();
        const json = this.mod.take(this.mod.raw.glider_stats(db));
        if (json === null)
            throw new GliderError(this.mod.lastError() ?? 'stats failed');
        // glider_stats predates the typed API and returns the flat shape, so give
        // it the same surface as everything else rather than leaking the
        // difference to callers.
        const flat = JSON.parse(json);
        return { ...flat, graph: { nodes: [], edges: [] }, ms: 0 };
    }
    /** Release the graph. Safe to call twice. */
    close() {
        if (this.#handle) {
            this.mod.raw.glider_close(this.#handle);
            this.#handle = 0;
        }
    }
    /** Lets `using db = glider.open()` work under TS 5.2+ explicit resource management. */
    [Symbol.dispose]() {
        this.close();
    }
}
//# sourceMappingURL=index.js.map