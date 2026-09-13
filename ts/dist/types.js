/**
 * Result types mirroring glider's typed JSON API (`src/api.rs`).
 *
 * A note on ids. glider ids are `u64`. They arrive here through JSON, so they
 * land as JavaScript numbers and are exact only below 2^53. That is ~9
 * quadrillion nodes, well past the point where a memory-resident graph stops
 * fitting in RAM, so it is not a practical limit — but it is why `id` is
 * `number` here rather than `bigint`.
 */
/** Narrow a cell to a node. */
export function isNode(c) {
    return typeof c === 'object' && c !== null && !Array.isArray(c) && c._e === 'node';
}
/** Narrow a cell to a relationship. */
export function isRel(c) {
    return typeof c === 'object' && c !== null && !Array.isArray(c) && c._e === 'rel';
}
/** Thrown for any failure reported by the engine. */
export class GliderError extends Error {
    query;
    constructor(message, query) {
        super(message);
        this.query = query;
        this.name = 'GliderError';
    }
}
//# sourceMappingURL=types.js.map