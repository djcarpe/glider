# glider lake

> **Status: design, with phase 0 in progress.** Nothing described here is
> on `main` as a usable command yet; the *Phases* section says what exists.

A lake is many glider databases, each an **immutable file**, under one
**metadata log** that says which files are live, what is in each, and how
the set has changed over time — the shape of Delta Lake and Apache Iceberg,
applied to property graphs. The files and the log live in a directory or an
S3 bucket. Readers open a snapshot of the log and query the union of its
files; writers add files and commit; nothing already written ever changes.

Still zero dependencies, no mmap, no unsafe.

## Why

Two scaling problems are easy to conflate; a lake addresses one of them.

1. **Memory grew while reading one big file.** `bench/results/suite`
   recorded peak RSS of 0.63 / 1.28 / 2.58 / 4.06 / 8.61 GB for the 0.5 / 1
   / 5 / 10 / 25 GiB graphs, with a 1 GiB page cache: linear, about 0.3 GB
   per GiB of data. It was one query grouping by node with every group
   held in memory — found and fixed as LAKE-0 (`docs/TODO.md`). A lake
   reads through the same engine and would have inherited it, which is
   why it was found first rather than designed around. The remaining
   unbounded readers are `ORDER BY` without `LIMIT` and `DISTINCT`
   (PAGED-3), bounded by the size of the answer rather than the graph.

2. **Scans, aggregates and algorithms on the paged engine are slow, and
   everything is one file behind one mutex.** Edge scan at 5 GiB: 13 s
   (the snapshot-image engine did it in 2.8 s); algorithms 7–10× slower
   than the image; readers serialize behind the writer (PERF-2). Immutable,
   bulk-built, statistics-pruned files fix this class of problem: trees
   built bottom-up read sequentially, a label scan opens only files that
   have the label, and files are independent, so they are read in
   parallel with no shared lock.

A third reason is the one ragentic actually has: many small, versioned
graphs ("gliders": `ragentic/docs/gliders.md`) built from manifests,
published, and queried together under one token budget. Today each is a
JSONL artifact loaded into a `:memory:` graph on demand. One glider version
= one lake file is a direct fit, and the org graph stays the hot,
transactional glider it is now.

## What already exists

| needed | have |
|---|---|
| a writer for immutable, sorted, sequential files | `Graph::bulk_load` (`src/graph.rs`): external sort + `btree::Builder`, every page written once and full |
| a pluggable page source | `Store::Mem \| Store::File` in `storage/pager.rs`; reads go through `locate` → `read_exact_at` |
| object storage | `stream/s3.rs` (SigV4, PUT/GET/DELETE/LIST), `stream/backend.rs` (`file://`, `s3://`, `exec:`) |
| a consistent copy of a live graph | the replication hold (`storage/db.rs`, `replica.rs`): checkpoint under a hold, copy pages while the writer continues |
| a read API every query goes through | `Graph`'s read methods (`node`, `scan_nodes`, `for_each_adj`, `indexed_lookup`, …) |

Missing, and added in phase 0: a read-only open (no lock, no log), ranged
GET and put-if-absent on the backends.

**Not** a starting point: the glider-stream replica layout (`<gen>/base/…`
+ log segments). That is a backup of one mutable file in the Litestream
mould — read whole, not partitioned, not stitchable. The lake is its
sibling: glider-stream protects the hot graph; the lake is the cold tier.

## Layout

```text
<lake>/
  _lake.json                               format, page size, name; written once
  _log/00000000000000000001.json           commit 1
  _log/00000000000000000002.json           commit 2 …
  _snapshots/00000000000000000010.json     every N commits: the whole state at that version
  data/00000012-<uuid>.gldb                immutable data files (file id 12)
  dv/00000012-00000000000000000019.dv      deletion vector for file 12 as of version 19
```

A **data file** is an ordinary paged glider database (format 4), built once
with `bulk_load` and never written again: no `-wal/` directory, no `.lock`,
no `-data/` segments (a file is capped at one segment; split the data
instead). Opened standalone it is a valid, queryable glider database. Its
name carries its lake file id and a uuid, so a name is never reused.

The **log** is one JSON object per commit, numbered from 1 with 20 digits,
so a lexical listing is the history. A commit is the unit of atomicity:
everything in it becomes visible together, or nothing does.

A **snapshot** is the full live state at one version — every live file
with its statistics, the interner, the current deletion vector of every
file — so a reader does not replay the whole log. Delta calls this a
checkpoint; the word is taken here.

## The log

```json
{
  "version": 7,
  "parent": "b1f2…",
  "ts": 1794170000,
  "writer": "ragentic@node-a",
  "message": "publish docs/handbook v12",
  "actions": [
    {"strings": {"from": 1024, "values": ["Doc", "Section", "LINKS_TO", "title"]}},
    {"add": {
      "file": 12,
      "path": "data/00000012-9f3c1a….gldb",
      "bytes": 1537064960,
      "sha256": "e3b0…",
      "page_size": 16384,
      "nodes": 1247371, "edges": 8027661,
      "max_node": 1247370, "max_edge": 8027660,
      "labels": {"3": 400000, "7": 20000},
      "types":  {"11": 8000000, "12": 27661},
      "indexes": [{"label": 3, "key": 9, "min": "a@…", "max": "z@…", "distinct": 400000}],
      "source": {"kind": "publish", "db": "orgs/acme.gldb", "generation": "…", "lsn": 1234}
    }},
    {"remove": {"file": 4, "reason": "compacted into 12"}},
    {"tombstones": {"file": 9, "path": "dv/00000009-00000000000000000007.dv", "count": 120}}
  ]
}
```

- `parent` is the SHA-256 of the previous commit's bytes: a reader that
  follows the chain knows it saw every commit and none was replaced.
- **`strings`** extends the lake-wide **interner**: labels, relationship
  types and property keys are `u32` ids in every file, and comparisons in
  the query engine are on those ids, so files must agree. The interner is
  append-only; ids are never reused or renamed. A file is built with the
  interner as of its commit and carries the dense prefix of it in its own
  catalog (the first `n` strings, where `n` is the highest id it uses), so
  it is also self-describing when opened alone. Within a commit, `strings`
  comes before the `add`s that use it.
- **`add`** carries everything the planner needs to skip the file: label
  and type counts, id ranges, and for each index the min/max of the
  indexed values and the distinct count. The checksum is of the whole
  file; `bytes` lets a reader size a download before it starts.
- **`remove`** retires a file from the live set. The bytes stay until
  `vacuum`, so an older version can still be read.
- **`tombstones`** replaces the file's deletion vector. A deletion vector
  is the sorted list of a file's deleted local node and edge ids, delta +
  varint encoded, with a header and a CRC. Deleting a node tombstones its
  edges too. A new vector supersedes the previous one entirely; vectors
  are small (a file is immutable, so its deletions are bounded by its
  size) and whole replacement keeps the reader simple.

### Node and edge ids

A lake id is 64 bits: the **file id in the top 24** and the file's local id
in the low 40. Standalone, a file's ids are its local ids; through the lake
they are prefixed, so ids are unique across the union and the `nN` ids
`context` hands a model are stable for the life of a snapshot. Ids are
not stable across compaction (the rows get new local ids in the new file),
exactly as Iceberg's row positions are not. Identity that must survive
compaction is a key property, as ragentic's `_key` already is.

## Committing

1. Read `_lake.json`. Find the current version: the newest snapshot, then
   the log entries after it (S3 `ListObjectsV2` has been strongly
   consistent since 2020; `_log/` is small).
2. Upload data files and deletion vectors **first**. Their names are
   unique, so an abandoned commit leaves only unreferenced bytes for
   `vacuum`, never a dangling reference.
3. Write `_log/<N+1>.json` with **put-if-absent**: `create_new` on a
   filesystem; `If-None-Match: *` on S3 (AWS since August 2024; MinIO,
   Ceph RGW Squid+, Garage 1.0+). A backend that cannot do it (`exec:`,
   an S3 server that ignores the header) is refused for writes unless the
   lake is configured `single-writer`, which is a promise, not a lock.
4. **A conflict** (the entry exists: someone committed `N+1` first) reloads
   the state and retries as `N+2` if the two commits are disjoint — the
   other commit removed none of the files this one read or removed, and
   tombstoned none of the files this one tombstones. Otherwise the commit
   fails and the caller decides. This is Delta's optimistic rule.
5. Every 10 commits the committer also writes `_snapshots/<N>.json`. It is
   derived, so losing it costs a longer replay and nothing else.

There is no lock file, no leader and no daemon. Two writers on different
machines commit to the same lake safely because the object store
arbitrates version numbers.

## Reading

```rust
let lake = Lake::open("s3://graphs/acme", LakeOptions {
    version: None,            // or Some(19): time travel
    as_of: None,              // or a unix time: the newest version at or before it
    mirror: "/var/cache/glider-lake",
    cache_size: 1 << 30,      // page cache shared by every open file
    work_mem: 256 << 20,
    ..Default::default()
})?;
```

- **Snapshot selection.** Newest snapshot at or below the target version,
  plus the log after it. The result is an in-memory `State`: files with
  stats, interner, deletion vectors.
- **Files open lazily and read-only**: no lock, no write-ahead log, free
  list not loaded. On S3 a file is fetched once into the mirror under its
  SHA-256 and verified; immutability means a mirrored file is valid for
  ever and can be shared by every process on the machine. (Phase 2 adds a
  page source that reads ranges straight from the object, through the
  same CLOCK cache, for files too large or too cold to mirror.)
- **Pruning.** A label scan opens only files whose `labels` has the label;
  a typed expansion only files with the type; an indexed lookup only files
  whose min/max covers the value. Counts come from the stats, minus
  tombstones, without opening anything.
- **The union is the graph.** `Lake` implements the read half of `Graph`'s
  API over its files: `node(id)` routes by file id; scans iterate the
  candidate files; `indexed_lookup` fans out. Cypher runs unchanged over
  it (read-only). Scans and aggregates over many files run one worker per
  file and merge, with no shared lock.
- **Deletion vectors** are applied at the lowest level: a tombstoned
  node or edge is invisible to every read, including adjacency from the
  other end.

### Stitching

Table partitions are independent; graph partitions are not, because an
edge can cross files. The first version takes the position Iceberg takes:
**files are independent, and joins are the engine's job.**

- An edge lives in one file with both ends. **No edge crosses files.**
- A reference from one file to a node in another is a **key**: a property
  (ragentic: `_key`, and `glider:<ns>/<name>/<key>` refs) resolved with an
  indexed lookup that the stats prune to the files that can hold it.
  `Lake::resolve(label, key, value)` does this; in Cypher it is the
  ordinary two-pattern query with a `WHERE a.ref = b._key`.
- A `MATCH` pattern never crosses a file boundary through an edge. This is
  documented behaviour, not a limitation to hide.

Cross-file adjacency — an edge whose reverse entry lives in another
file — is a later design (a per-commit *boundary file* holding in-edges
for nodes of earlier files) and is only worth it if whole-lake traversals
become the workload. Whole-lake algorithms (`CALL pagerank` over the
union) work in the first version through the out-of-core projection,
which already streams adjacency from pages; they are not fast.

## Writing

| command | what it does |
|---|---|
| `glider lake init URL` | writes `_lake.json` |
| `glider lake publish URL <db>` | a consistent copy of a live glider as a new file: checkpoint under a hold, bulk-build from its pages in id order (strings remapped onto the lake interner), upload, commit `strings` + `add` |
| `glider lake publish URL -jsonl FILE` | the same from a JSONL dump (a ragentic glider artifact) |
| `glider lake delete URL -file N -ids …` or by query | new deletion vector for the file, commit `tombstones` |
| `glider lake compact URL [-files …]` | `bulk_load` over the union of the chosen files minus their tombstones into one file; commit `remove`×n + `add` |
| `glider lake vacuum URL -retention 7d` | delete data and dv objects referenced by no version newer than the retention horizon |
| `glider lake log URL` / `files URL` / `show URL -version N` | history, the live set and its stats, one version's state |
| `glider URL -version N -c '…'` / `serve` | query a lake, read-only, at a version |

Updates are deletes plus a new file, as in Delta. Small, frequent updates
belong in the hot graph; the lake is for what has settled. A reasonable
cadence is ragentic's: publish on build, compact when a namespace has
accumulated more than a handful of files, vacuum nightly.

## Object storage

- Files are uploaded as single PUTs up to 5 GB and multipart above that
  (phase 2; `PERF-6`). Downloads verify the SHA-256 from the log.
- Reads in phase 1 go through the mirror. Phase 2 adds `Store::Object`: a
  page source that fetches 1 MiB extents by ranged GET into the CLOCK
  cache. It needs `get_range` on the backend (phase 0) and nothing else
  new; the pager already treats a page as bytes at an offset.
- The native client speaks plaintext only (MinIO, Ceph, Garage,
  LocalStack). Real AWS over TLS goes through `exec:` as glider-stream
  does — which makes such a lake single-writer, since `aws s3 cp` has no
  put-if-absent. (The `aws` CLI does pass `--if-none-match` from v2.17; a
  later phase can let `exec-put` declare that it honours it.)

## What it is not

- **Not Iceberg- or Delta-compatible.** The files are glider pages, not
  Parquet; Spark and DuckDB cannot read them. Making them readable means a
  Parquet writer in std (Thrift compact protocol plus a few encodings —
  feasible, a project of its own) and node/edge tables. Not planned until
  someone needs it.
- **Not a fix for problem 1 above.** See LAKE-0.
- **Not a transaction system across the hot graph and the lake.** A
  publish is a copy of a checkpoint; what happened in the hot graph after
  it is in the next publish.
- **Not multi-writer on `exec:` backends.**

## Phases

Items carry `LAKE-` ids in `docs/TODO.md`.

**Phase 0 — prerequisites**
- [x] LAKE-0 · find the linear memory growth in the read path and bound it (grouping spills)
- [ ] LAKE-1 · read-only open: `OpenOptions::read_only`, no lock, no log, `--read-only`
- [ ] LAKE-2 · backends: `get_range`, `put_if_absent`; S3 conditional PUT and ranged GET

**Phase 1 — a lake on a directory**
- [ ] LAKE-3 · `_lake.json`, log entries, snapshots, `State` from a version; commit with conflict rule
- [ ] LAKE-4 · `publish` from a live graph (hold + bulk build with interner remap) and from JSONL
- [ ] LAKE-5 · `Lake` read API over the union with pruning; Cypher over it; deletion vectors
- [ ] LAKE-6 · `compact`, `vacuum`, `log`, `files`, `show`; time travel by version and time
- [ ] LAKE-7 · tests: differential (lake of k files vs one graph of their union, every read compared), crash in every step of a commit, two concurrent committers

**Phase 2 — S3 and scale**
- [ ] LAKE-8 · S3 backend with the mirror; multipart upload; `Store::Object` ranged reads
- [ ] LAKE-9 · parallel scans and aggregates, one worker per file
- [ ] LAKE-10 · ragentic: a glider build publishes to the lake; `context` over a lake

## Open decisions

Taken for now, easy to revisit before phase 1 lands:

- **Architecture, not interoperability.** "Like Delta" means immutable
  files + a log + object storage, not Parquet. (Open: does anything need
  Spark or DuckDB to read these files?)
- **Union + key join** for stitching, matching how gliders link today.
  (Open: is any workload a whole-graph traversal across files?)
- **Lake-wide interner, files carry its dense prefix.** The alternative —
  a per-file interner remapped at open — costs a translation on every
  read and was rejected.
- **File id in the top 24 bits of the id.** 16M files per lake; 1T nodes
  per file.
- **Whole-vector tombstone replacement**, not incremental vectors.
- **Order of phases** follows which problem is biting: if it is one big
  org graph, LAKE-0 and the paged engine's scan speed come first; if it is
  many gliders queried together, phase 1 does.
