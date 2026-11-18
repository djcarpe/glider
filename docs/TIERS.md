# Where the pages live

A glider database is pages and a log. This note is about where the pages are
while a database is open, and what that means for the copy kept elsewhere.

| open as | pages | log | survives the process | replicates |
|---|---|---|---|---|
| `:memory:` | RAM | none | no | no |
| `:memory:`, replicated | RAM | RAM, until it is shipped | up to the last shipped segment, in the replica | yes |
| a file | disk, behind a page cache | disk | yes | yes |
| a file, with a hot set | disk, behind a page cache and a protected hot set | disk | yes | yes |

The first and third rows are what glider has always had. The second is this
note. The fourth is in [HOT.md](HOT.md).

## One replica, two places to restore it

A replica is base snapshots plus the log shipped after them
([REPLICATION.md](REPLICATION.md)):

```
<generation>/base/<lsn>/db, data/*.seg, base.json     the pages of one moment
<generation>/segments/<offset>-<unix>.seg             log from <offset>, whole transactions
```

Nothing in that says where the pages were when they were shipped or where
they go when they come back. So:

- a database open **in memory** ships the same objects a file does, and
- a replica restores **into memory** as well as to a file,

in any combination. A graph flown from memory today restores to a file
tomorrow, and the other way round, on the same lineage. That is the whole
design; the rest is how each half is done without touching a disk.

## Shipping from memory

A `:memory:` database has no log: its transactions are atomic and that is
all. Replicating it starts by giving it one.

**The log, in memory.** `storage::log::MemLog` holds the same frames a file's
log holds (`kind | len | payload | crc`, a commit marker ending each
transaction), addressed by the same log positions, in memory. Commit and
rollback mean what they mean on disk. It keeps what has not been shipped and
drops what has. It is bounded: past `log_cap` (64 MiB by default) of
committed, unshipped log it drops all of it, and the replica is told the log
it needs is gone, exactly as a file's replica is told when a checkpoint
deleted it. The answer is the same in both cases: take a new base. Data is
never lost by this; what is lost is point-in-time granularity for that
stretch. Only committed log counts: the open transaction is kept whole until
it commits or rolls back.

A memory database that is not replicated still keeps no log and pays
nothing.

**A base, from memory.** Pages are copy-on-write and shared by reference
(`Arc<[u8]>`), and a transaction never modifies a page the committed state
uses. So a consistent snapshot of the committed state is a copy of the
page table: page numbers and references, taken under the database's lock in
the time it takes to copy that many pointers, with no page copied. The
writer carries on at once; a page it changes afterwards is a new page, and
the snapshot keeps the old one alive until it is done.

The snapshot is then written out, outside the lock, as the image a file
would hold at a checkpoint: a superblock naming the committed roots, the
generation and the log position the snapshot was taken at; every page,
checksummed as it leaves memory; and a free list for the page numbers that
hold nothing. It is cut into segment files of 64 MiB so no object is larger
than that. `Pager::open` reads that image from a file and
`Pager::from_parts` reads it from bytes: it is the same image.

**A pass** (`stream::memory::MemoryReplicator`):

1. With the database locked: decide. A base is needed when the replica has
   none, when one is forced or due (`snapshot.interval`), or when the log
   the replica needs is gone. Then take either a snapshot or the log bytes
   from the last shipped position to the last commit.
2. With the database unlocked: upload. A base's files first and `base.json`
   last, so a base is whole only once it is all there; a segment as one
   object.
3. With the database locked again: record what is now shipped, which lets
   the log below it go.

A pass that fails part way has recorded nothing: the next one sends the same
segment again, or takes a base again. Only whole transactions are ever in a
segment, because both ends of it are commit positions.

The first time a replicator talks to a backend it checks that the base it
believes in is really there. A replicator pointed at an empty prefix takes a
base however much it thinks it has shipped elsewhere.

## Restoring into memory

`stream::memory::restore` downloads the newest base at or before the target
time and the log after it, as `stream::paged::restore` does, and then,
instead of laying files out:

1. builds a memory pager from the base's bytes (`Pager::from_parts`),
   keeping the generation and the log position in its superblock;
2. replays the log by the rules a file's log is replayed by on open
   (`storage::log::replay_shipped`, a reader over bytes beside the file's),
   through the same code that applies a file's records, one committed
   transaction at a time, stopping at a gap or a torn or damaged frame;
3. gives the database a `MemLog` that starts where the replay ended.

The result is a graph on the replica's lineage: same generation, log
positions carrying on. A replicator resumed against the backend it was
restored from ships only what is written from then on; no new base — as
long as the replay reached the end of what was shipped. If it stopped short,
at a gap or a damaged frame, the position the replicator believes in is not
in the new log, it is told that log is gone, and it takes a new base.

## Limits

- **Memory is still the limit.** The whole graph is in RAM, bounded by
  `max_memory`; a restore that does not fit fails with `Full` and leaves
  nothing behind.
- **A base in flight holds old pages.** Until a snapshot is dropped it keeps
  alive every page the writer replaced or freed since it was taken, outside
  `max_memory`'s count: a graph rewritten wholesale while its base is being
  written can briefly need up to twice its size.
- **The open transaction's log is held whole.** However large, until it
  commits or rolls back; `log_cap` counts committed log only.
- **Not a new lineage in the browser.** A generation is made from the clock
  and the process id, and wasm has neither: there `enable_log` fails on a
  graph with no lineage yet, and only a graph restored from a replica, which
  keeps the replica's, has a log.
- **A base is a whole image.** Every base uploads every page. Incremental
  bases are PAGED-6, for both kinds of database.
- **What is in RAM and not yet shipped dies with the process.** That window
  is the interval between passes. Pass after every commit for none.
- **One replica per memory database** keeps its log. A second replicator
  works, and takes a base whenever the first has let log go that it needs.
- **`log_cap` bounds the log, not the graph.** A burst of writes larger than
  it between passes costs a base at the next pass.
- **No encryption, no compression,** as for files.

## In the code

| | |
|---|---|
| `storage::log::MemLog` | the log in memory: append, commit, rollback, read a range, let go of what is shipped |
| `storage::log::replay_shipped` | shipped log replayed from bytes, by the rules a file's log is replayed by |
| `storage::pager::Snapshot` | the committed page table of a memory pager, and the image it writes |
| `storage::pager::Pager::from_parts` | a memory pager from an image's segment files |
| `storage::db::Db` | a memory database with a log (`enable_log`, `snapshot`, `release_log`); `from_replica` |
| `stream::memory` | `MemoryReplicator` (capture, upload, acknowledge, and `sync` over a `Mutex<Graph>`), `restore` |
| `Graph::enable_log`, `Graph::generation` | what an embedder calls |
| `Graph::log_span`, `log_committed_end`, `log_range`, `release_log`, `snapshot`, `from_replica` | what `stream::memory` calls |

Embedders with their own lock (the BEAM binding) call `capture`, `upload`
and `acknowledge` themselves, holding the lock only for the first and the
last.
