# glider — three reviews

Three passes over the same 11k lines, from three angles. Everything below was
checked against the code rather than inferred from the docs; line references
are to the tree as shipped. Findings are ordered by what I would fix first.

Counts: 48 tests passing, two binaries, zero dependencies.

---

## 1. The Rust engineer

### R1 — Reads serialize behind a write lock (`server.rs:23`)

`Arc<Mutex<Graph>>` with a thread per connection. Every query takes the
exclusive lock, so ten concurrent `MATCH` statements run one at a time on a
machine with sixteen idle cores. Nothing in a read path mutates.

Split `execute` into read and write halves — the parser already knows which a
statement is — take `RwLock::read()` for the former, and concurrency arrives
for the cost of a type change. The one wrinkle is `CALL … write:`, which is a
read that writes; classify by the tail clause, not the verb.

### R2 — Variable-length expansion is quadratic (`query.rs:~1398`)

```rust
let mut seen: Vec<u64> = vec![current];
…
if seen.contains(&adj.other) { continue; }
```

`Vec::contains` is a linear scan, inside the per-neighbour loop, inside the
per-depth loop. A three-hop expansion that touches 50k nodes does ~10⁹
comparisons. Swap for the `IdSet` already in `graph.rs` and it is O(1) per
probe. This is the single cheapest performance fix in the tree.

### R3 — No fuzz target on the decoder

`Op::decode` and `replay` parse bytes that, since `glider-stream restore`
landed, may have come from an S3 bucket rather than from this process. The
allocation guards are already there (`store.rs:187,267`, `codec.rs:154` all
clamp with `.min(…)`), which is more care than most projects take — but the
proof should be a `cargo fuzz` target over `replay`, not a reading of the
source.

### R4 — `neighbors()` allocates per hop

Returns an owned `Vec` inside the hot expansion loop. An iterator borrowing the
adjacency slice removes an allocation per node per hop. Worth doing after R2,
and measurable on the two-hop benchmark that currently takes 11.8s.

### R5 — Property *values* are not interned

`graph.rs` interns labels, edge types and property keys to `u32`, but
`Value::Text` is a `String` per property. A million nodes carrying
`status: "RUNNING"` store that string a million times. A value-side intern pool
for short repeated strings is a plausible 2× memory win on exactly the
workloads this is aimed at — worth measuring before building.

### R6 — Unbounded lifetime in the FFI (`ffi.rs`)

```rust
unsafe fn as_db<'a>(p: *mut GliderDb) -> Result<&'a mut GliderDb, String>
```

`'a` is inferred at the call site, so the compiler will hand out any lifetime
asked for, including two overlapping `&mut`. It is sound as used — every caller
drops the reference before returning — but the signature does not say so. Tie
it to a local borrow, or leave it and add
`#![forbid(unsafe_op_in_unsafe_fn)]` plus a comment explaining the invariant.

### R7 — No CI, no clippy gate, no MSRV check

The tree declares `rust-version = "1.74"` and nothing verifies it. A five-line
workflow running `cargo test`, `cargo clippy -- -D warnings`, and a build for
`aarch64-linux-android` and `aarch64-apple-ios` would keep the portability
claim honest.

### Credit where it is due

Bounded allocations on untrusted input, an identity hasher for `u64` keys
rather than SipHash, `catch_unwind` at every FFI boundary, `std::error::Error`
implemented, and iterative SCC and DFS so deep graphs do not blow the stack.
These are the things usually missing.

---

## 2. The SQLite engineer

### S1 — Opening is O(write history), not O(data) *(the structural one)*

Measured on this machine:

| | file | open |
|---|---|---|
| fresh 100k-node graph | 15.4 MB | 571 ms |
| after 800k property writes | 35.3 MB | 855 ms |
| after `COMPACT` | 15.8 MB | 616 ms |

Every open replays every byte, at roughly 27 MB/s. A 1 GB log takes ~38 seconds
to open, and the log grows with *writes*, not with data — a small graph that is
updated constantly gets slower to open forever, until someone compacts it.

SQLite does not have this problem because the main database file is
materialized state and the WAL is only the recent delta. Glider has no
materialized form at all; `COMPACT` produces one and then immediately starts
appending to it again.

The fix that fits the architecture: a checkpoint record *inside* the log
carrying the materialized image, so replay starts at the last checkpoint
instead of at byte 0. Compaction already computes exactly this
(`snapshot_ops`); it just throws away the ability to resume from it. Same file,
same format, no second file to keep consistent — and replication keeps working
because the checkpoint is just more bytes in the log.

### S2 — No directory fsync after create or rename (`store.rs:311,444`)

The compaction path fsyncs the temp file and renames it, which is the right
shape, but never fsyncs the *parent directory*. On POSIX the rename is not
durable until the directory entry is synced; a crash in that window can leave
the old file, or neither. Same for file creation. Three lines:

```rust
if let Some(dir) = path.parent() {
    let _ = File::open(dir).and_then(|d| d.sync_all()); // no-op on Windows
}
```

### S3 — Torn-tail recovery silently forked the replica lineage — **fixed in this pass**

The bug: `Store::open` discarded a torn tail and truncated, but kept the same
generation id. Under `Sync::Normal` the discarded bytes may already have
reached a replica — the OS had them, this process did not survive to commit
them. The next write then puts *different* bytes at those same offsets, and a
replica that keeps appending to the same lineage restores a file that is
CRC-valid and wrong at the seam: the worst failure mode a backup tool has,
because nothing reports an error.

This is precisely what SQLite's WAL checksum salts and Litestream's generations
exist to prevent. Truncation now mints a new generation, so replicas see the
discontinuity and start a fresh lineage. Test:
`discarding_a_torn_tail_starts_a_new_generation`, which also asserts that a
clean reopen does *not* churn the generation.

### S4 — No read-only open

`Store::open` always requests write access and always truncates a torn tail.
So you cannot point a second process at a database to run queries, cannot query
a file on read-only media, and cannot inspect a restored replica without taking
write access to it. A read-only path is small — skip truncation, skip the
writer — and unlocks the reporting sidecar everyone eventually wants.

### S5 — No lock file, so two writers corrupt silently

Known and documented, still the sharpest edge in the project. SQLite has had
advisory locking since the beginning. A `.lock` sidecar created with
`create_new(true)`, removed on drop, with `--force` to break a stale one, is
pure `std`, portable, and perhaps forty lines. Without it the honest advice is
"be careful", which is not advice.

### S6 — No integrity check for the database itself

`glider wal verify` checks a replica; nothing checks a live file. A
`glider db.gldb verify` that walks every frame, validates CRCs, and reports the
first bad offset and how much is recoverable is a natural counterpart to
`PRAGMA integrity_check`, and reuses `scan_committed_end` almost entirely.

### S7 — Readers cannot run while a writer runs

Not just the server (R1) — the design has one exclusive owner of the graph.
SQLite's WAL mode lets readers proceed against a consistent snapshot while a
writer appends. Glider could do the same, since a reader only needs an
immutable view: copy-on-write on the index structures, or generation-tagged
reads.

---

## 3. The graph database engineer

### G1 — There is no planner: matching always starts at the first pattern element

`match_chain` (`query.rs:1351`) seeds from `chain.nodes[0]` and expands left to
right, in the order you typed. `candidate_nodes` does consult the property
index — but only for whichever node happens to be written first. So:

```cypher
MATCH (a:Person)-[:KNOWS]->(b:Person {email: "x@y.z"}) RETURN a
```

scans every `:Person` and filters, while an index on `:Person(email)` sits
unused, purely because `b` was written second. Reversing the pattern by hand
fixes it, which is the tell.

Anchor selection by estimated selectivity — already-bound variable, then
indexed equality, then smallest label set, then everything — plus the ability
to expand backwards along a relationship, is the largest query win available
here, and it is maybe sixty lines given the matcher's shape. Neo4j's planner is
enormous; the first 5% of it is most of the benefit.

### G2 — Variable-length paths use node uniqueness, not relationship uniqueness

Cypher forbids reusing a *relationship* within a path. Glider forbids revisiting
a *node*, and the `seen` set is shared across the whole traversal rather than
per-path. Two consequences:

- **Cycles are unfindable.** `MATCH (a)-[:R*2..3]->(a) RETURN a` returns nothing,
  ever, because `a` is in `seen` from the start. For a dependency graph that is
  the query you most want.
- **Cardinality differs.** Each target is yielded once, at its shortest depth,
  where Cypher yields one row per distinct path.

The current behaviour is cheaper and is often what people actually want —
reachability, not path enumeration. But it should be a decision with a name in
the documentation, not a difference discovered in production. Offer both:
reachability by default, Cypher semantics under `*..` with a flag or a distinct
syntax.

### G3 — No `MERGE`, no constraints

For a knowledge or context graph the dominant write is "this entity again,
maybe already here". Without `MERGE` every ingest is read-then-write with a
race in the middle, and without a uniqueness constraint there is nothing to
make `MERGE` correct even once it exists. These two go together and I would
build them together: `CREATE CONSTRAINT ON :Person(email) IS UNIQUE`, enforced
on write using the existing hash index, then `MERGE` on top.

### G4 — No query parameters

Every query is built by string concatenation. That is an injection surface
through `POST /query` and through the C ABI, it prevents plan caching later,
and it makes the FFI awkward for exactly the mobile embedders the last round
was aimed at. `execute(g, src, params)` with `$name` in the grammar is a
contained change and should land before anyone builds a second application on
this.

### G5 — No `WITH`, `UNWIND`, `OPTIONAL MATCH`, or `CASE`

`WITH` is the one that matters: without a projection stage there is no
aggregate-then-filter, no mid-query pipelining, no multi-part query at all.
`OPTIONAL MATCH` is left-join semantics — "every process and its most recent
alarm, if any" is currently two queries and a join in the host language.
`UNWIND` turns a list into rows and is how bulk parameterised writes are
expressed.

### G6 — No `EXPLAIN` / `PROFILE`

You cannot see why a query was slow, which makes G1 invisible to users: the
fast and slow spellings of the same query look identical. Given how simple the
matcher is, printing the chosen anchor, the expansion order, and rows touched
per step is an afternoon, and it would have surfaced G1 on its own.

### G7 — Indexes are exact-match only

No range index (`WHERE n.started_at > …` scans), no composite, no full-text, no
relationship property index. Range is the one to add first: the structure is
already a `BTreeMap<VKey, Vec<u64>>`, so ordered scans are nearly free —
`indexed_lookup` just needs a range variant.

---

## Opportunities

These are not gaps. They are things this architecture can do that Neo4j and
SQLite cannot, and they all fall out of the log being the database.

### O1 — Time travel

The log *is* the history. Replaying to a prefix reconstructs any past state
exactly, and `wal` already knows how to find the prefix for a timestamp. A
query form like `MATCH (n:Order) AS OF '2026-09-01' RETURN n` is a replay plus
a read, and for a knowledge graph that lives in a git repo — where "what did we
believe last Tuesday" is a real question — it is a genuine differentiator.
Neo4j needs application-level bitemporal modelling for this.

### O2 — Change data capture, free

The log is already an ordered stream of typed mutations. `glider tail --follow
--format jsonl` turns a database into an event source with no outbox table, no
trigger, no polling, and no dual-write problem — the thing that makes CDC
pipelines hard elsewhere is the thing this format already is.

### O3 — Read replicas from object storage

Because a replica is byte-identical and the whole graph lives in memory, a
standby is a replicator that replays incoming segments into a `Graph` instead
of writing them to disk. `glider-stream serve --replica s3://…` would be a
warm read-only follower serving queries seconds behind the primary. Litestream
needed a custom VFS to approximate this; here it is mostly wiring that already
exists.

### O4 — The graph algorithms are the product

PageRank, betweenness, SCC, k-core and community detection in a single 1 MB
binary with no server is not something the alternatives offer. Neo4j puts these
behind GDS, a separate licensed product with its own memory model. Leaning into
"analytics on a graph that fits in RAM, embedded anywhere" is a sharper story
than competing on query-language completeness — and it argues for spending the
next effort on G1 and G6 (making the queries that feed the algorithms fast and
legible) rather than on chasing Cypher feature parity.

---

## If I had a week

1. **S5** lock file, **S2** directory fsync — correctness, both small.
2. **G1** anchor selection by selectivity, **G6** `EXPLAIN` — the performance
   cliff and the instrument that reveals it.
3. **R2** `IdSet` in the expander, **R1** `RwLock` — free throughput.
4. **S1** in-log checkpoints — before anyone has a log large enough for it to
   hurt.
5. **G4** parameters, then **G3** constraints and `MERGE`.
