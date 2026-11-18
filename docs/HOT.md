# The hot set

A file-backed database keeps its pages on disk and reads them through a
cache. This note is about the part of the graph that is used again and
again, and keeping it in memory on purpose.

## What the cache does not do

The page cache (`storage::pager::Cache`) keeps recently used pages, up to
`cache_size`, and evicts with CLOCK. Three things follow from "recently":

- **A scan flushes it.** One pass over a large label touches every page
  once, and by the end the pages that were being looked up all day have been
  swept out with the rest.
- **Another database flushes it.** Every database in a process shares one
  cache budget. When the budget is spent, room is made by clearing the
  caches of other databases that are idle at that instant, whole. A
  database that was busy a second ago starts again from nothing.
- **Every open is cold.** Nothing remembers what was worth having.

None of that is wrong for a cache. It is wrong for the root of a tree, the
adjacency of a hub, or the few hundred nodes a service asks about all day.

## What the hot set is

A bounded number of cache frames (`hot_size` bytes of them) reserved for
pages that are **used often**, which is not the same as recently:

- **Often** means in many separate stretches of work. Lookups are counted
  in ticks (a tick is a fixed number of cache lookups); a page earns heat
  once per tick it is used in, however many times it is used inside that
  tick, and heat halves as ticks pass without it. A scan uses a leaf page
  in one tick and moves on: it earns nothing. The root of the tree it
  walked is used in every tick: it is hot, and it should be.
- **Hot pages are not evicted** by the cache's own sweep, and not by
  another database wanting room. They are given up only when a hotter page
  needs the place, when they cool, when the page itself is freed, when the
  process's budget is cut below what the hot sets alone hold, or when a
  cache has no cold frame that nobody holds (its own coolest hot frame
  goes, or another database's, rather than the statement failing).
- **Hot follows the data, not the page number.** A page is copied before it
  is changed (copy-on-write); the copy inherits the heat.
- **Hot is remembered.** At each checkpoint and on a clean close the list
  of hot page numbers is written beside the database (`<db>-hot`). At the
  next open it is read back and those pages are loaded, and made hot,
  before `open` returns. The file is advice: if it is missing, damaged,
  another database's or from a checkpoint the file never reached, the
  database opens exactly as it would without it, and a page in it that is
  not, now, the data it named is not loaded (see *Remembering it*).
- **Hot travels with a base.** A replica's base carries the list of the
  checkpoint it copies (`<generation>/base/<lsn>/hot`), and a restore lays
  it beside the database, so a database restored from object storage is
  warm on the pages its source found hot.

`hot_size` is zero by default, and zero is exactly the engine without any of
this: no counting, no file.

```
glider big.gldb --cache-size 256M --hot-size 32M
```

```rust
Graph::open_opts(path, OpenOptions { cache_size: 256 << 20, hot_size: 32 << 20, ..Default::default() })
```

The hot set lives inside the cache's frames and the process's budget: it is
never more than half the cache, and memory use is bounded as before.

## What it is not

- **Not a second copy of the graph.** Hot pages are the same pages, in the
  same frames, read by the same code. Nothing is decoded ahead of time.
- **Not a promise.** A cut to the process's budget can take hot frames; the
  list on disk can be stale.
- **Not for `:memory:`.** There every page is already in memory.
- **Not a change to the file format.** The database file is untouched;
  the list is a sidecar, and a base without one restores as before.

## The rule for "often"

Exactly, as `src/storage/hot.rs` implements it:

- **The clock is the database's own cache lookups**, every page read
  through the cache, hit or miss, counted from when the hot set was turned
  on (after recovery at open: replaying the log is not the workload). A
  **tick** is `T` lookups, `T` being the cache's frames but at least 1024.
  Lookup `n` is in tick `n / T`. An idle database's clock stands still.
- **A page earns one heat per tick it is read in**, however often within
  it, up to 31. Heat belongs to a page while it is in the cache; a page
  evicted (cold, by the sweep) starts again from nothing.
- **Every heat halves, rounding down, as each tick that is a multiple of
  16 begins.**
- **A page joins the hot set when its heat reaches 4**: into a free place,
  or in place of the coolest hot page if that page's heat is now lower
  (equal is not enough).
- **A hot page leaves** when its heat has halved to 0, when a hotter page
  takes its place, when its page is freed or rewritten as other data, when
  a cache with no cold frame to give takes it, or when the process's budget
  takes it. When copy-on-write moves a page's data to a new number, the
  copy takes its heat and its place (and the original keeps its heat, cold,
  until it is freed, so a rollback finds it warm).

What follows:

- Read in four ticks with no halving between them, a page is hot at the
  fourth. Read in ticks 13, 14 and 15, it has 3, halved to 1 as tick 16
  begins, and needs ticks 16, 17 and 18.
- **One stretch of reads is never hot up to `2T + 1` lookups**: it falls in
  at most three ticks. That is a scan's leaf: read for a while, then never
  again. `2T + 2` lookups in a row can reach a fourth tick. The tree's root
  and upper levels are read in every tick of a workload that uses the tree,
  and become hot; that is correct.
- In steady use, a page read in `r` of every 16 ticks has heat near `r`
  just after a halving and near `2r` just before the next, so a page read
  steadily in two ticks out of sixteen or more becomes hot, and one read in
  fewer does not; pages near that line come and go. A working set spread so
  thin that each of its pages is read less often than that is not "often"
  here, however much it is used in total (see *Measured*). Since a tick is
  the cache's frames, a larger cache has longer ticks and a lower bar.
- A hot page nobody reads is gone after at most five halvings: 80 ticks
  from heat 31, 48 from a page just promoted (4, 2, 1, 0). Nothing else
  needs to want its place.

The rule's edges are tested in `src/storage/hot.rs` (the bookkeeping alone)
and `tests/hot_set.rs` (`the_rule_at_its_edges_through_pager_reads`,
through real pager reads).

## What it costs

With `hot_size` 0 the cache has no hot set at all (`Option::None`): a hit
costs one more branch than before, the sweep one more branch per frame it
looks at, and nothing is counted or allocated. The zero test
(`zero_hot_size_reads_hits_misses_and_evicts_as_before`) checks the
reads, writes, hits, misses and evictions, and a fingerprint of exactly
which reads missed, against numbers taken from the engine before the hot
set existed.

With a hot set, a hit does what it did (a hash lookup, the CLOCK bit, an
`Arc` clone) and then constant work in `Hot::touch`:

- every lookup: a bounds check, two compares on the frame's 16-byte heat
  record, a decrement of the tick counter, and a look at one ring head
  (to retire at most one page whose heat has run out);
- the first lookup of a page in a tick: a shift-and-subtract for its
  decayed heat, two stores, and moving it between two doubly linked rings
  (six index writes), or, if it reaches 4, a promotion: at most 31 ring
  heads looked at and at most one demotion;
- once per tick, an increment; once per 16 ticks, 31 ring splices,
  however large the set.

Nothing walks the cache or the set per lookup. The one walk is when a cache
has no cold frame nobody holds: it looks for the coolest hot frame nobody
holds, at most the size of the set. Memory: 16 bytes per cache frame and 16
per hot place, inside the 512 bytes of bookkeeping the process budget
already charges each frame.

On the benchmark machine the difference on a hit is inside the noise (see
*Measured*, the `--scan 0` rows).

## What this does not do

- **Heat needs residence.** A page earns heat only while it is in the
  cache. A workload whose scans flush the cache between every use of its
  pages never warms them up: the sweep evicts them before their fourth
  tick. Warm-up has to happen in stretches the cache can hold (the list
  from a previous open is the other way in).
- **Long scans are time.** The clock is lookups, so a scan's own lookups
  cool the hot set too. A hot page survives about 80 ticks without being
  read (`80 * max(frames, 1024)` lookups): through a scan of about 80
  times the cache, more for caches of under 1024 frames, not through
  arbitrarily long ones.
- **Loading is part of opening.** The list is loaded before `open`
  returns, on the opening thread: at most the hot set's room in page reads
  (`hot_size` bytes), in page order. Nothing runs in the background, so no
  query ever races it; an open with a 32 MiB hot set of 16 KiB pages does
  up to 2,048 reads before it returns.
- **The first entries, not the first good ones.** The loader looks at as
  many entries as the set has room for, hottest first, and skips the bad
  ones; it does not read further down the list to make up for them. A list
  from an older checkpoint (see below) can leave the set part empty.
- **A stale entry costs a read.** Whether a listed page still holds what it
  held needs its header: such an entry is read, found out, and dropped,
  not kept in the cache.
- **Opens without a hot set leave the list as it was.** They neither read
  nor write it, so it ages while they change the database, and is worth
  less, page by page, at the next open that has one.
- **Not synced.** After a power loss the list may be missing or empty;
  the database opens cold.

## Reading it

`STATS` (when the database has a hot set), `Graph::stats`, `pager::Stats`
and the telemetry snapshot report `hot_pages` (resident now), `hot_hits`
(lookups that found their page hot), `promotions`, `demotions` and
`prewarmed` (pages loaded from the list at open, and made hot). `STATS`
and `Stats` also give `hot_capacity`, the places the set has:
`min(hot_size / page_size, frames / 2)`. In OpenTelemetry and Prometheus
they are `glider.db.cache.hot.{resident,hits,promotions,demotions,prewarmed}`.

## Remembering it

`src/storage/hotlist.rs` has the format and the checks; in short:

```
0     8   magic "GLIDERHT"
8     4   version (1)
12    4   page size
16    16  generation
32    8   epoch of the checkpoint it was taken at (the superblock's)
40    8   n
48    8n  page numbers, hottest first
48+8n 4   CRC-32 of everything before it
```

- **Written** by `Db::checkpoint` just before it writes the superblock,
  under the epoch that superblock will carry, and on a clean close (one
  with no open transaction) whether or not that close checkpoints, to
  `<db>-hot.tmp` and renamed over `<db>-hot`: a reader sees the old list
  or the new one, and whoever sees a new superblock finds its list. A
  checkpoint that then fails leaves a list of a checkpoint the file never
  reached, which the next open passes over. Its cost is the hot set, 8 bytes a
  page, never the database. With `hot_size` 0 nothing is written, and an
  existing list is left alone.
- **Read** by `Graph::open_opts` after recovery and after the hot set is
  turned on (`Db::prewarm`, `Pager::prewarm`), before the schema is
  loaded and before `open` returns. Not read with `hot_size` 0. The whole
  list is passed over when it is not whole (length, checksum), is another
  database's (generation, page size), or was taken at a checkpoint newer
  than the one the database opened from (an older copy of the file put
  back beside a newer list).
- **Each page** is then checked. Skipped without a read: numbers past the
  high-water mark, the superblocks, and pages in any free list (free, freed
  since and waiting for a checkpoint, held for a backup, or the free list's
  own pages). Read, then dropped: a page whose header epoch is after the
  list's. Pages are copy-on-write, so a number handed out again carries the
  epoch of its new data, and a page whose epoch is not after the list's
  holds exactly what it held when the list was written. That is what lets
  an older list (from before a crash whose log was replayed, or before
  opens without a hot set) be used for what is left of it.
- **Loading never makes room by taking a hot frame**, this database's or
  another's, nor a frame the process's budget does not have: a page goes
  into a new frame if the pool has one, or in place of one of this
  database's own cold frames, and when neither is there loading stops.
  Loaded pages are hot at once, at heat 4, as if just promoted, and count
  in `prewarmed`. Loading is not a lookup: it does not move the clock.
- **Carried.** `replica::Tailer::copy_base` reads the writer's list before
  it copies the pages and puts it in the base as `hot` when it is this
  database's and not newer than the checkpoint copied (the writer writes
  it before that checkpoint's superblock, so it is that checkpoint's
  unless a later checkpoint has replaced it, and then the base goes
  without).
  glider-stream uploads it with the base, before `base.json`, and drops it
  from the spool with the pages. `replica::restore` (and so
  `glider wal restore` and `glider-stream restore`) lays it beside the
  restored database as `<db>-hot`. The restore's own check opens without a
  hot set, replays the log and checkpoints; the list is then older than the
  database, and the first open with a hot set loads what the log did not
  rewrite. Builds without this ignore the file in a base, and restore as
  before.

`Pager::hot_pages()`, `Pager::mark_hot(pno)` and `Pager::set_hot_size` are
the pieces this is made of; `Pager::hot_list()` and `Pager::prewarm()` put
them together.

## Measured

`bench/src/bin/hotbench.rs`, built with `cargo build --release -p
glider-bench --bin hotbench`. Machine: Intel i7-6700K (4 cores, 8
threads), 32 GiB, Linux, shared with other work (load average 11 to 16
during the runs, so latencies move by a third from run to run). The
operating system's page cache was not dropped: "disk" reads mostly came
from it, so read counts are exact and latencies understate a real disk.
16 KiB pages, an 8 MiB cache (512 frames). Each run: warm-up of eight
random lookups per person, then 10 rounds of (a full scan, then 2,000
timed lookups); a lookup is a person, everyone they know, and their
records. Five runs of each, alternating; the median is shown, reads were
identical in every run.

```
hotbench scans    --dir D [--people 5000] --hot-mb H [--scan 0]
hotbench pressure --dir D --hot-mb H
hotbench reopen   --dir D --hot-mb 1 --list 0|1 [--restore 1] [--lookups 100]
```

| workload | hot set | lookup reads (20,000 lookups) | p50 | p99 | hot pages |
|---|---|---|---|---|---|
| scans: 1,000 people, 1M items (78 MiB, ~10× the cache) | none | 430 | 11.1 µs | 26.0 µs | — |
| | 1 MiB | **0** | 10.5 µs | 21.1 µs | 44 of 64 |
| scans: 5,000 people, same items | none | 2,026 | 12.9 µs | 33.0 µs | — |
| | 4 MiB | 925 | 12.7 µs | 30.7 µs | 108 of 256 |
| pressure: A 1,000 people, B scans 400k items (31 MiB) in a 12 MiB process pool | none | 430 | 10.2 µs | 24.5 µs | — |
| | 1 MiB | **0** | 9.0 µs | 16.9 µs | 43 of 64 |
| hits only (`--scan 0`, 1,000 people) | none | 0 | 11.5 µs | 18.1 µs | — |
| | 1 MiB | 0 | 12.5 µs | 23.2 µs | 44 of 64 |

- **Scans:** with the hot set the people's pages stay through every scan
  and the lookups read nothing; without it each round's first lookups read
  the 43 pages back. The tail improves; the median barely moves, because
  most lookups in a round hit the cache either way and the re-reads come
  from the OS cache.
- **Pressure:** without a hot set B's scans empty idle A each round
  (A's resident pages fall to 0); with one, A keeps its 43 hot pages
  (and only those) and reads nothing.
- **Where it does not help much:** with 5,000 people read uniformly, each
  of their pages is read in about one or two ticks out of sixteen, right at
  the rule's line: about 110 of them are hot at a time, and pages near the
  line keep joining and leaving (1,462 promotions and 1,352 demotions in a
  one-round run). Reads halve; the rest stay cold although the set had
  room. That is the rule doing what it says, not a bound being hit. With a
  32 MiB cache instead (ticks of 2,048 lookups, so the bar is half as
  high), one run had 196 hot and the lookups read 20 pages in three rounds.
- **Hits:** with every page resident, the hot set's bookkeeping is not
  measurable on this machine: the two rows differ by less than the spread
  between runs of the same row (7.9 to 12.1 µs at p50 without it).

**Across opens** (`reopen`; the `scans` database, 1,000 people, warmed
with a 1 MiB hot set and closed, then opened again; three runs of each,
the median shown, reads identical in every run):

| second open | list | `open` | pages read by `open` | loaded | first 100 lookups: reads | p50 | first 2,000 lookups: reads |
|---|---|---|---|---|---|---|---|
| the database | none | 0.26 ms | 2 | 0 | 41 | 15.0 µs | 44 |
| | its own | 0.92 ms | 46 | 44 | **0** | 13.2 µs | **0** |
| restored from a replica | none | 0.24 ms | 2 | 0 | 41 | 15.9 µs | 44 |
| | the base's | 4.8 ms | 46 | 44 | **0** | 12.3 µs | **0** |

- The list moves the 44 reads the first lookups would have made into
  `open`, in page order, and the lookups after it read nothing. Without
  it, 41 of the 44 come in the first hundred lookups.
- The restored database was opened once before, without a hot set, as
  `glider wal restore` checks what it restored; its list was still whole.
  Its slower `open` (4 to 8 ms against about 1) was the same in every
  run and is not explained here: the 44 reads are the same pages of a
  file written moments before.
