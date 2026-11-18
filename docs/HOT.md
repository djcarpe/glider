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
- **Hot is remembered** (part B; not built yet). At each checkpoint and
  on close the list of hot page numbers is written beside the database
  (`<db>-hot`). At open it is read back and those pages are loaded before
  the first query. The file is advice: if it is missing, stale or damaged,
  the database opens exactly as it would without it, and anything in it
  that is not a live page of this database is skipped.
- **Hot travels with a base** (part B; not built yet). A replica's base
  carries the list (`<generation>/base/<lsn>/hot`), so a database restored
  from object storage is warm on the pages its source found hot.

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
  tick. Warm-up has to happen in stretches the cache can hold (part B's
  list from a previous open is the other way in).
- **Long scans are time.** The clock is lookups, so a scan's own lookups
  cool the hot set too. A hot page survives about 80 ticks without being
  read (`80 * max(frames, 1024)` lookups): through a scan of about 80
  times the cache, more for caches of under 1024 frames, not through
  arbitrarily long ones.
- **No list across opens yet.** Every open is cold until part B.

## Reading it

`STATS` (when the database has a hot set), `Graph::stats`, `pager::Stats`
and the telemetry snapshot report `hot_pages` (resident now), `hot_hits`
(lookups that found their page hot), `promotions`, `demotions` and
`prewarmed` (loaded from the list at open; always 0 until part B). `STATS`
and `Stats` also give `hot_capacity`, the places the set has:
`min(hot_size / page_size, frames / 2)`. In OpenTelemetry and Prometheus
they are `glider.db.cache.hot.{resident,hits,promotions,demotions,prewarmed}`.

## For part B

`Pager` (and `Graph`, through `pager()`) already has what loading and
saving a list need:

- `Pager::hot_pages() -> Vec<u64>`: the hot page numbers now, hottest
  first. Call it between transactions; every number in it is a live page
  of the committed state or of the open transaction.
- `Pager::get(pno)` then `Pager::mark_hot(pno) -> bool`: makes a page that
  is in the cache hot now, at heat 4, as if just promoted. False if it is
  not resident, there is no hot set, or the set is full of pages at least
  as hot. It does not count `prewarmed`; `hot::Hot::prewarmed` is the
  counter to bump.
- `Pager::set_hot_size(bytes)`, called by `Graph::open_opts` after
  recovery; load a list after it, so the replayed log does not count as
  use.

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
