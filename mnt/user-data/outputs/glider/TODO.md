# glider — TODO

Everything found across the three review passes and the build sessions before
them, deduplicated and ordered. IDs in brackets cross-reference `REVIEW.md`
where one exists; items marked **new** were found while compiling this list and
are not in the review.

Effort: **S** = under half a day · **M** = one to three days · **L** = a week or more.

---

## Correctness bugs

These produce wrong results or lost data. Nothing else should go first.

- [x] **BUG-1 · Torn-tail recovery forked the replica lineage** [S3] — **fixed**
  `store.rs`. Truncation kept the generation id, so a replica could splice
  pre-crash and post-crash bytes into a CRC-valid, wrong database. Truncation
  now mints a new generation. Test:
  `discarding_a_torn_tail_starts_a_new_generation`.

- [x] **BUG-2 · `glider-stream restore` silently truncates at a gap** · **new** · S
  `stream/replicator.rs`, the `while let Some(…) = l.segments.iter().find(…)`
  loop simply ended when no segment started at `end`. **Fixed:** a hole with
  segments above it now fails the restore, names the gap offset and the number
  of unusable segments, and removes the half-written output. Test:
  `a_missing_segment_fails_the_restore`.

- [x] **BUG-3 · `ExecBackend` temp files collide across replicas** · **new** · S
  `stream/backend.rs:197,205` name temp files `glider-put-{pid}` /
  `glider-get-{pid}`. The daemon runs one thread per (database, replica) pair
  inside one process, so two concurrent puts shared a path and overwrote each
  other's bytes mid-upload. **Fixed:** temp names now carry a per-operation
  counter.

- [x] **BUG-4 · `lag_bytes` is zeroed after a partial ship** · **new** · S
  `stream/replicator.rs` sets the gauge to 0 after a successful segment even if
  the writer had produced more since the scan. **Fixed:** the gauge is
  re-measured after the PUT instead of assumed zero.

- [x] **BUG-5 · Two writers corrupt a database with no warning** [S5] · S
  **Fixed:** a `.lock` sidecar created with `create_new(true)`, holding the pid,
  released on drop, `--force` to break one. On Linux a holder that is provably
  gone (`/proc/<pid>` absent) is reclaimed automatically; everywhere else
  unknown means alive and the human decides. Tests:
  `a_second_writer_is_refused_rather_than_corrupting`,
  `a_lock_left_by_a_dead_process_is_reclaimed`.

---

## Storage and durability

- [ ] **DUR-1 · Opening rebuilds the graph, and that is nearly all of the cost** [S1] · L
  **Diagnosis corrected.** I originally blamed replaying superseded history and
  proposed in-log checkpoints. Measured on a 159 MB log (5,000,000 records),
  with `verify` used to separate the phases:

  ```
  read + framing + CRC32          303 ms     I/O is 20 ms of this; page cache is 8 GB/s
  + decode every record          +554 ms
  + build the in-memory graph  +10,760 ms    93% of the total
  ```

  Peak RSS while opening that file is **2,170 MB — 13.7× the file on disk**.
  It is not reading that costs, it is *constructing*: ~2.15 µs per record spent
  allocating a labels Vec and a props Vec per node, a props Vec per edge, and
  pushing into two adjacency Vecs that live inside hash maps.

  So checkpoints alone will not fix open time. They remove the history tax —
  real, and worth having — but a checkpoint still has to be turned back into
  hash maps and Vecs, which is the 93%. Fixing open properly means a
  materialised image that is *mapped* rather than built: arena-allocated,
  offset-addressed instead of pointer- and hash-addressed, written as a blob
  and `mmap`ed. That is a storage rewrite, and it is the same rewrite DUR-6
  needs for working sets beyond RAM. Do them together or not at all.

  Extrapolated, a 1 GB log wants ~73 s and ~14 GB of RAM. On a 4 GB machine it
  does not open at all.

- [x] **DUR-2 · No directory fsync after create or rename** [S2] · S
  **Fixed:** `sync_parent()` after file creation and after the compaction
  rename. No-op on Windows.

- [ ] **DUR-3 · No read-only open** [S4] · M
  `Store::open` always takes write access and always truncates a torn tail. So
  no second process can query a live database, nothing can read a file on
  read-only media, and inspecting a restored replica requires write access.

- [x] **DUR-4 · No integrity check for a live database** [S6] · S
  **Done.** `glider <db> verify` walks every frame, checks CRCs, decodes every
  payload, and reports records, transactions, and where the log stopped making
  sense — distinguishing an ordinary torn tail (at or above the last commit)
  from corruption below it. Reads without locking, so it is safe against a
  database another process has open. Test:
  `verify_distinguishes_a_torn_tail_from_corruption`.

- [ ] **DUR-5 · Readers cannot run concurrently with a writer** [S7] · L
  One exclusive owner of the graph. SQLite's WAL mode lets readers proceed
  against a consistent snapshot while a writer appends. Needs copy-on-write
  index structures or generation-tagged reads.

- [ ] **DUR-7 · A single large transaction is buffered entirely in memory** · **new** · M
  `replay` holds decoded ops in a `batch` until the commit marker, which is
  correct — uncommitted ops must not reach the graph — but unbounded. The
  benchmark file above is 5,000,000 records in **two** transactions, so opening
  it holds ~2.5M fully decoded ops, each with its own allocated strings, before
  a single one is applied. That is a second copy of the log in the most
  expensive representation available, and it is most of the 13.7× amplification.
  Bound it: spill to a temp file past a threshold, or apply speculatively with
  an undo log.

- [ ] **PERF-7 · 13.7× memory amplification** · **new** · L
  A 159 MB file becomes 2.17 GB of live graph. Per-node and per-edge `Vec`
  allocations dominate. Inline storage for the common small cases (one label,
  a handful of properties) without an allocation, arena-backed property
  storage, and CSR-style adjacency built once instead of a `Vec` per node
  would each take a bite. Pairs with PERF-4 (value interning). Measure before
  building: `verify` now gives a clean baseline that excludes graph
  construction entirely.

- [ ] **DUR-6 · Working set cannot exceed RAM** · L
  The structural ceiling, documented in `MOBILE.md` (~150k nodes on a low-end
  Android device). Fixing it means a paged B-tree with a buffer pool — a
  rewrite of `store.rs` and `graph.rs`, not a flag. Check first whether
  sharding by subgraph covers the real cases; for per-user or per-repo graphs
  it usually does.

---

## Query engine

- [x] **QRY-1 · No planner: matching always starts at the first pattern element** [G1] · M
  `query.rs:1351`. `MATCH (a:Person)-[:KNOWS]->(b:Person {email:"x"})` scans
  every `:Person` while the index on `email` sits unused, purely because `b`
  was written second. Add anchor selection by estimated selectivity (bound
  **Fixed.** Every node pattern is now costed (bound variable → `id()`
  predicate → indexed equality → label count → all nodes), the cheapest becomes
  the anchor, and the chain is walked outward from it: rightwards as written,
  then leftwards with each relationship direction flipped. `WHERE id(x) = n`
  conjuncts are pushed into anchor selection; an `OR` is correctly not treated
  as a pin. Middle anchors work — a two-sided pattern expands both ways.

  Measured on the 100k-node benchmark graph, with open time separated out:

  ```
                              before      after
  open + replay only            516 ms     516 ms
  MATCH (a)-[*1..3]->(b)
    WHERE id(a) = 1          ~11,000 ms      42 ms
  ```

  Tests: `the_planner_anchors_on_the_selective_end`,
  `an_id_predicate_is_pushed_into_the_anchor`,
  `a_middle_anchor_expands_in_both_directions`.

  Still open from this area: no cost model for relationship fan-out (a
  `Chain` with two equally selective ends picks the first), and no join
  reordering *between* comma-separated patterns.

- [ ] **QRY-2 · Variable-length paths use node uniqueness, not relationship uniqueness** [G2] · M
  `query.rs:~1398`. Cycles are unfindable — `MATCH (a)-[:R*2..3]->(a)` returns
  nothing, ever — and each target is yielded once at its shortest depth rather
  than once per path. Decide deliberately: keep reachability semantics as the
  documented default, add Cypher semantics behind a flag.

- [ ] **QRY-3 · No query parameters** [G4] · M
  Every query is string concatenation. Injection surface through `POST /query`
  and the C ABI, blocks plan caching, awkward for mobile embedders. Add `$name`
  to the grammar and a params map to `execute`. Should land before a second
  application is built on this.

- [ ] **QRY-4 · No `MERGE`, no constraints** [G3] · L
  For a knowledge graph the dominant write is "this entity again, maybe already
  here". Without `MERGE` every ingest is read-then-write with a race; without
  `CREATE CONSTRAINT … IS UNIQUE` there is nothing to make `MERGE` correct.
  Build them together on top of the existing hash index.

- [ ] **QRY-5 · No `WITH`, `UNWIND`, `OPTIONAL MATCH`, `CASE`** [G5] · L
  `WITH` matters most: no projection stage means no aggregate-then-filter and
  no multi-part queries. `OPTIONAL MATCH` is left-join semantics. `UNWIND` is
  how bulk parameterised writes get expressed (pairs with QRY-3).

- [x] **QRY-6 · No `EXPLAIN` / `PROFILE`** [G6] · S
  The fast and slow spellings of a query look identical, which makes QRY-1
  invisible to users. Print the chosen anchor, expansion order, and rows
  touched per step. An afternoon, and it would have surfaced QRY-1 on its own.

- [ ] **QRY-7 · Indexes are exact-match only** [G7] · M
  No range, composite, full-text, or relationship-property index. Range first:
  the structure is already `BTreeMap<VKey, Vec<u64>>`, so ordered scans are
  nearly free — `indexed_lookup` needs a range variant.

- [ ] **QRY-8 · `utils` module is not reachable from the query language** · S
  The `digraph_utils` companion exists as a Rust API with 7 tests but has no
  `CALL utils.*` procedures. The `CALL` parser needs to accept a dotted name
  (three lines) plus a dispatch table for `components`, `condensation`,
  `reachable`, `reaching`, `arborescence_root`, `loop_vertices`, `subgraph` and
  the rest. Named as a loose end when the module landed; still open.

---

## Performance

- [x] **PERF-1 · Variable-length expansion is quadratic** [R2] · S
  **Fixed:** the visited set is now `IdSet` rather than `Vec` + linear
  `contains`.

- [ ] **PERF-2 · Reads serialize behind a write lock** [R1] · S
  `server.rs:23` uses `Arc<Mutex<Graph>>`, so ten concurrent `MATCH` statements
  run one at a time. Split read and write paths and take `RwLock::read()` for
  reads. Watch out for `CALL … write:`, which is a read that writes.

- [ ] **PERF-3 · `neighbors()` allocates a `Vec` per hop** [R4] · S
  Inside the hot expansion loop. An iterator borrowing the adjacency slice
  removes an allocation per node per hop. Measure against the two-hop benchmark
  currently at 11.8s.

- [ ] **PERF-4 · Property values are not interned** [R5] · M
  Labels, edge types and keys are interned to `u32`; `Value::Text` is a
  `String` per property. A million nodes with `status: "RUNNING"` store it a
  million times. Plausible 2× memory win on exactly this workload — measure
  before building.

- [ ] **PERF-5 · CSR projections are rebuilt on every `CALL`** · **new** · M
  Each algorithm invocation builds a fresh CSR, O(V+E). Running five
  algorithms over the same projection pays it five times. Cache by
  (direction, edge type, weight property) and invalidate on write.

- [ ] **PERF-6 · Whole segments are held in memory** · **new** · M
  `stream/replicator.rs:391` `read_range` allocates the full byte range, and
  `ship` sends it as one object. A tailer disconnected for a day against a busy
  database tries to allocate multiple GB — and S3 caps a single PUT at 5 GB.
  Chunk at a configurable maximum segment size, or stream the body.

---

## Replication tooling

- [ ] **REP-1 · No compression** · M
  Litestream compresses; glider does not. On a graph log — heavy in repeated
  label, key and string bytes — this is real money. LZ4 block format is a small
  compressor and a tiny decompressor, both fully specified, and keeps the
  zero-dependency property.

- [ ] **REP-2 · No encryption at rest** · S
  Segments are raw database bytes. Today the answer is `age`/`gpg` in the
  `exec-put` hook, which works but is undocumented as a worked example. Either
  document it properly or make it a first-class replica option.

- [ ] **REP-3 · No TLS for the native S3 client** · L / won't-do
  Plaintext endpoints only (MinIO, Ceph, Garage, LocalStack); AWS proper
  delegates to the `aws` CLI. A hand-rolled TLS stack would be worse than a
  dependency, and a dependency ends the portability claim. The realistic
  improvement is making the `exec` path feel less like a fallback.

- [ ] **REP-4 · `-restore-if-db-not-exists` on replicate** · S
  What makes container startup a one-liner: restore on boot if the file is
  absent, otherwise carry on. Litestream has it; this does not.

- [ ] **REP-5 · No `reset`** · S
  Litestream's `reset` clears local state and forces a fresh snapshot. Here the
  equivalent is deleting a replica directory by hand.

- [ ] **REP-6 · Restore does not verify segment checksums before assembling** · S
  `replicator::restore` checks only the header of the result. Validate each
  object's frames as they are applied so a corrupt object is named, rather than
  surfacing later as a replay failure with no provenance.

- [ ] **REP-7 · No ETag / upload verification** · S
  S3 returns an ETag; nothing compares it. A silently truncated upload is
  discovered at restore time.

- [x] **REP-8 · Retention does O(n²) listings** · **new** · S
  **Fixed:** the live generation's listing is hoisted out of the loop.

- [ ] **REP-9 · No offset-based restore target** · S
  Litestream has `-txid`; glider has byte offsets and exposes only
  `-timestamp`. `-offset N` is trivial and more precise than segment-granular
  time.

- [ ] **REP-10 · No packaging** · S
  No systemd unit, no Dockerfile, no shell completions, no man page. Litestream
  ships all of these and it is most of why it gets deployed.

- [ ] **REP-11 · Not implemented, probably fine** — `register`/`unregister`/
  `start`/`stop`, the IPC socket, the MCP server, the read-only VFS. Listed for
  completeness; none are load-bearing. The VFS idea reappears as OPP-3 in a
  better form.

---

## Mobile and portability

- [ ] **MOB-1 · `scripts/build-all.sh` is untested for iOS and Android** · M
  No Xcode or NDK in the build environment where it was written. The iOS and
  Android paths need one real run each before anyone trusts them.

- [ ] **MOB-2 · No JNI shim crate** · M
  Android currently means JNA or hand-written glue. A `bindings/android` crate
  depending on the `jni` crate keeps the core dependency-free while giving
  Kotlin zero-marshalling calls. This is what I would actually ship.

- [ ] **MOB-3 · No Swift package** · S
  The XCFramework has to be dragged into Xcode by hand. A SwiftPM
  `binaryTarget` plus a thin Swift wrapper around the C ABI is a small,
  high-leverage piece of polish.

- [ ] **MOB-4 · FFI has an unbounded lifetime** [R6] · S
  `ffi.rs` `as_db<'a>` lets the compiler hand out any lifetime, including two
  overlapping `&mut`. Sound as used, unstated in the signature. Tie it to a
  local borrow, or add `#![forbid(unsafe_op_in_unsafe_fn)]` and document the
  invariant.

- [ ] **MOB-5 · wasm has no bindings** · M
  `wasm32-unknown-unknown` compiles, but nothing wires the FFI to JS. The C ABI
  is the model to copy.

---

## Testing and CI

- [ ] **TEST-1 · No CI at all** [R7] · S
  `rust-version = "1.74"` is declared and nothing verifies it. A workflow
  running tests, `clippy -D warnings`, and cross-target builds for Android and
  iOS keeps the portability claim honest.

- [ ] **TEST-2 · No fuzz target on the decoder** [R3] · M
  `Op::decode` and `replay` now parse bytes that may have come from an S3
  bucket. Allocation guards exist (`store.rs:187,267`, `codec.rs:154`), but the
  proof should be `cargo fuzz`, not a reading of the source.

- [ ] **TEST-3 · No crash-injection matrix** · M
  One torn-tail test exists. What is missing is kill-during-write across sync
  modes, kill-during-compaction, and kill-during-replication, asserting the
  database opens and the replica restores every time.

- [ ] **TEST-4 · No multi-process test** · S
  Everything runs single-process. Concurrent reader-while-writer and the
  two-writer case (once BUG-5 lands) both need real processes.

- [ ] **TEST-5 · No soak test** · M
  Nothing runs for hours. Replication lag, memory growth and log growth over a
  long run are exactly where the remaining bugs live.

---

## Documentation

- [ ] **DOC-1 · Reachability-vs-Cypher path semantics** · S
  QRY-2's behaviour must be documented whichever way it is resolved. A
  difference discovered in production is much worse than one written down.

- [ ] **DOC-2 · Worked example for encrypted replication** · S
  Pairs with REP-2.

- [ ] **DOC-3 · Capacity guidance** · S
  `MOBILE.md` has mobile numbers; there is nothing equivalent for servers —
  bytes per node and edge, replay speed, when to compact.

---

## Opportunities

Not gaps. Things this architecture can do that the alternatives cannot, all
falling out of the log being the database.

- [ ] **OPP-1 · Time-travel queries** [O1] · L
  `MATCH (n:Order) AS OF '2026-09-01'` is a replay to a prefix, and `wal`
  already knows how to find the prefix for a timestamp. Neo4j needs
  application-level bitemporal modelling for this. For a knowledge graph in a
  git repo, "what did we believe last Tuesday" is a real question.

- [ ] **OPP-2 · Change data capture, free** [O2] · M
  `glider tail --follow --format jsonl` turns the database into an event source
  with no outbox table, no triggers, no polling, no dual-write problem. The
  thing that makes CDC hard elsewhere is the thing this format already is.

- [ ] **OPP-3 · Read replicas from object storage** [O3] · L
  A standby is a replicator that replays incoming segments into a `Graph`
  instead of writing them to disk. `glider-stream serve --replica s3://…` would
  be a warm read-only follower seconds behind the primary. Litestream needed a
  custom VFS to approximate this.

- [ ] **OPP-4 · Lean into the algorithms** [O4] · —
  PageRank, betweenness, SCC, k-core and community detection in a 1 MB binary
  with no server is not something the alternatives offer; Neo4j puts these
  behind GDS as a separately licensed product. This is a positioning decision,
  and it argues for spending effort on QRY-1 and QRY-6 — making the queries
  that feed the algorithms fast and legible — over chasing Cypher parity.

---

## Suggested order

**Week 1 — stop the bleeding.** ~~BUG-2, BUG-3, BUG-4, BUG-5, DUR-2~~ — done,
along with PERF-1 and REP-8. 52 tests passing.

**Week 2 — the performance cliff and the instrument that reveals it.**
~~QRY-1, QRY-6, PERF-1~~ — done. PERF-2 (`RwLock` reads) remains.

With the planner in, **opening the file is the dominant cost in every
measurement**. Profiling it (see DUR-1) showed my original diagnosis was wrong:
the cost is not replaying history, it is rebuilding the in-memory graph, which
is 93% of open time and 13.7× the file in RAM. DUR-1, DUR-6 and PERF-7 are one
piece of work, not three.

**Week 3 — make it usable by others.** QRY-3 parameters, QRY-8 `CALL utils.*`,
DUR-4 verify, REP-4, TEST-1.

**Then the structural ones.** DUR-1 in-log checkpoints before any log grows
large enough to hurt, QRY-4 constraints and `MERGE`, and whichever of OPP-1
through OPP-3 matches where this is actually going.
