# Benchmarks: glider vs SQLite vs Memgraph

How glider was tested against SQLite and Memgraph, what was measured, how the
answers were checked, and what the numbers say. Everything here was produced
by the harness in `bench/` on one machine on 2026-09-27; the raw results are
committed in [`bench/results/`](../bench/results/) and every table below is
computed from them.

The short version: glider was the fastest of the three on 18 to 21 of the 30
reads at every size from 500 MiB to 25 GiB. Its lead is widest on
graph-shaped work: shortest paths (milliseconds against minutes for recursive
SQL), traversals, counts, variable-length patterns and index lookups. A few
1–2 hop reads go to SQLite by small margins. SQLite stays faster on
relational-shaped work (whole-label scans, `GROUP BY`, degree counts, bulk
inserts and updates), by up to about 45×, and on PageRank at 25 GiB, where
glider's out-of-core path timed out. Memgraph, which keeps the whole graph in
RAM, was competitive but rarely fastest, and could not hold the 10 GiB and
25 GiB graphs on a 31 GiB machine.

## Contents

- [What was compared](#what-was-compared)
- [The data](#the-data)
- [The workloads](#the-workloads)
- [How it was measured](#how-it-was-measured)
- [How answers were checked](#how-answers-were-checked)
- [Observability](#observability)
- [Results](#results)
- [Engine changes made along the way](#engine-changes-made-along-the-way)
- [Caveats and known issues](#caveats-and-known-issues)
- [Reproducing it](#reproducing-it)

## What was compared

| engine | version | storage | how it was driven |
|---|---|---|---|
| glider | this repository (paged engine), `profiling` build | pages on disk, 1 GiB page cache | its shell, `glider <db> --json` with `.timer` |
| SQLite | 3.53.4 | pages on disk, 1 GiB page cache, WAL | Python's `sqlite3` in a worker process |
| Memgraph | 3.13.1 (`memgraph/memgraph-mage`) | in memory, Docker capped at 24 GB | Bolt, the `neo4j` Python driver |

Machine: 8 cores, 31 GiB RAM, NVMe SSD, Linux (Arch). Every glider and SQLite
process ran under a 12 GiB address-space cap.

**SQLite schema.** What a SQL user would build for this graph: one table per
label, one table per relationship type with `FOREIGN KEY`s to both ends and
`(src, dst)` plus `(dst, src)` indexes (32 relationship indexes), and the same
property indexes glider keeps: `Person(email, age, country)`, `Order(ref)`,
`Product(sku)`, `Event(seq)`. `MENTIONS` points at three tables, so it carries
a kind column instead of a foreign key. Loaded from `glider export`, then
indexed, `PRAGMA foreign_key_check`ed (0 violations at every size) and
`ANALYZE`d.

**Memgraph schema.** The same property graph, with every node carrying its
glider id as `gid` (indexed per label) so queries that pin a node pin the same
one, plus the same property indexes. Loaded with `LOAD CSV` in
`IN_MEMORY_ANALYTICAL` mode, then switched to `IN_MEMORY_TRANSACTIONAL`.

## The data

`bench/src/bin/scale-gen.rs` writes a deterministic graph sized by bytes: the
same seed and size always give the same graph. It is deliberately varied:
people in communities of 1,000 with power-law hubs (`KNOWS`), reciprocal
`FOLLOWS`, a company tree, products in dense 5-cliques (`SIMILAR`) and a
category tree, orders with repeated order lines (multi-edges), a citation DAG,
documents mentioning three kinds of node, one long `NEXT` chain through every
event, and self-loops (`RETRY`). About 1% of ids are holes, as deletes would
leave, and property values cover every type glider stores, including long
non-ASCII text.

Sizes run were 500 MiB, 1, 5, 10 and 25 GiB (the generator's target for
glider's legacy file format; each engine's actual size is in the table).

### Datasets and builds

| size | nodes | relationships | glider file | glider bulk load | SQLite file | SQLite load + index + FK check | Memgraph load (RAM) |
|---|---|---|---|---|---|---|---|
| 500MiB | 609,111 | 3,890,438 | 0.75 GB | 11 s | 0.58 GB | 0.7 min | 44 s (1.74GiB) |
| 1GiB | 1,247,371 | 8,027,661 | 1.54 GB | 21 s | 1.20 GB | 1.5 min | 89 s (2.77GiB) |
| 5GiB | 6,237,595 | 40,209,610 | 7.78 GB | 1.8 min | 6.07 GB | 8.0 min | 460 s (10.79GiB) |
| 10GiB | 12,474,662 | 80,387,292 | 15.64 GB | 3.5 min | 12.25 GB | 15.9 min | not run |
| 25GiB | 31,186,200 | 200,772,785 | 39.19 GB | 10.4 min | 31.21 GB | 97.7 min | not run |

## The workloads

**30 reads** (`queries()` in `bench/sqlite_compare.py`), each in glider's
Cypher, hand-written SQL, and Memgraph's Cypher:

| shape | queries |
|---|---|
| point lookup | by indexed text (Person.email, Order.ref), by id |
| 1 hop | a community member's friends, a power-law hub's followers (count and average age), a sorted page of friends' names, an order's lines (multi-edges), a document's mentions by label |
| 2–3 hops | friends of friends (all and distinct), 3 hops out, buyer → orders → products, co-purchase recommendations, a 2-hop walk through dense cliques |
| variable length | category ancestors (tree, `*1..10`), `NEXT*1..200` along a chain, `CITES*1..3` in a DAG |
| traversal | BFS to 2 and 3 hops with depths, shortest path between two people |
| index scan | a low-cardinality index (country), an index plus a property filter |
| label / edge scan | an unindexed filter over a whole label, `GROUP BY` over a label, top-k over large rows, an edge-property filter over one relationship type, self-loops |
| global | top 5 by in-degree, count every node |

**5 graph algorithms.** glider's built-in procedures against the same
algorithm written in SQL, and Memgraph's MAGE modules:

| algorithm | glider | SQLite | Memgraph |
|---|---|---|---|
| degree centrality, top 10 by `KNOWS` in-degree | `CALL degree` | `GROUP BY dst` | Cypher aggregation |
| PageRank, 5 iterations over `KNOWS` | `CALL pagerank(tolerance: 0)` | iterated SQL with glider's formula: every node, dangling mass shared evenly | `pagerank.get` on a projection of the `KNOWS` subgraph |
| weakly connected components over `KNOWS` | `CALL wcc` | minimum-label propagation until nothing changes | `weakly_connected_components.get` on a projection |
| BFS to 3 hops | `CALL bfs` | recursive CTE | `*BFS ..3` |
| shortest path | `CALL shortestpath` | recursive CTE, depth ≤ 12 | `*BFS` between two bound nodes |

**6 write workloads**, the same operations through each engine's query
language, statement by statement:

| workload | durability |
|---|---|
| insert a node × 1,000, autocommit | glider `sync normal`, SQLite `synchronous=NORMAL`, Memgraph default WAL |
| insert a node × 200, fsync every commit | glider `sync always`, SQLite `synchronous=FULL`; Memgraph's WAL flush interval is a startup flag, so it ran with its default |
| bulk insert 50,000 nodes in one transaction | normal |
| insert 5,000 edges between nodes found by an indexed property, one transaction | normal |
| update 10,000 properties found by index, one transaction | normal |
| delete 500 nodes with all their relationships, one transaction (SQLite: every relationship table, then the row) | normal |

## How it was measured

- **Inside each engine.** glider by its shell's `.timer` (parse, plan,
  execute, build the result); SQLite around `execute` + `fetchall` in its
  worker; Memgraph by the parse, plan and execution times in each query's
  summary. Transport (pipes, Bolt) is outside all three, except Memgraph's
  autocommit writes, which are timed from the client because the commit is
  not in the summary.
- **Cold and warm.** Before a size's reads, the glider and SQLite files were
  evicted from the OS page cache (`posix_fadvise(DONTNEED)`) and each engine
  started as a fresh process: the first run of each query is **cold**, the
  median of the next five is **warm**. Memgraph's data never leaves memory,
  so its "cold" is the first run after loading. Anything slower than 20 s
  cold ran once.
- **Order.** Per size: build, reads, algorithms, writes. Writes change the
  data, so they ran last; their correctness check (below) accounts for it.
- **Limits.** 15 minutes per read and write, 30 minutes per algorithm. A
  timeout is reported as a timeout, not dropped.
- **Parameters.** SQLite and Memgraph's writes used parameters (prepared
  statements / cached plans); glider's shell has no parameters, so its writes
  sent literals and were parsed each time.

## How answers were checked

Every read and algorithm was compared against glider's answer, and a
benchmark only counts as agreeing when the answers match:

- rows are compared after normalising numbers (ints and floats that are equal
  compare equal, floats to 9 significant digits); queries without `ORDER BY`
  are compared as sets;
- BFS compares the set of (node, depth); shortest path compares hop counts
  (any shortest path is correct);
- PageRank compares the ranking and scores to a relative 1e-6 (glider vs
  SQLite). Memgraph's MAGE PageRank ranks only the `KNOWS` subgraph, a
  different formula, so its ranking is shown but not counted;
- connected components compares the component count;
- writes compare the Person and `KNOWS` counts in each engine afterwards.

Result: **every glider/SQLite check agreed at all five sizes**, and every
Memgraph check that ran agreed except the (not comparable) PageRank ranking.

Checking caught one real bug in glider, now fixed: a fixed-length pattern
could use the same relationship twice (see below).

## Observability

`bench/lgtm/up.sh` starts `grafana/otel-lgtm` (Grafana, Loki, Tempo,
Prometheus, Pyroscope and an OpenTelemetry collector) and Grafana Alloy.

- **Traces.** `bench/otel.py` sends OTLP/HTTP JSON (standard library only):
  one trace per benchmark per size, a span per engine and per run.
- **Logs.** A log line per run with the trace and span ids, so Loki and Tempo
  link to each other.
- **Metrics.** `bench_duration_milliseconds`, labelled by engine, size,
  benchmark and run; the dashboard in `bench/lgtm/dashboard.json`.
- **Flamegraphs.** Alloy's `pyroscope.ebpf` samples the `glider` process, the
  SQLite worker (found by its `--sqlite-worker` argument) and Memgraph at 97 Hz
  with 1 s batches. glider runs as its `profiling` build: release plus
  symbols, frame pointers and legacy symbol mangling
  (`RUSTC_BOOTSTRAP=1 RUSTFLAGS="-C force-frame-pointers=yes -Zunstable-options -C symbol-mangling-version=legacy"`),
  which the eBPF profiler can unwind and demangle. After its timed runs, each
  read repeats for 1.5 s so even microsecond queries have a flamegraph; the
  report links every row to its window.

## Results

Fastest engine per read (warm; Memgraph where it ran):

| size | glider | SQLite | Memgraph |
|---|---|---|---|
| 500 MiB | 18 | 10 | 2 |
| 1 GiB | 20 | 9 | 1 |
| 5 GiB | 20 | 8 | 2 |
| 10 GiB | 21 | 9 | not run |
| 25 GiB | 21 | 9 | not run |

Peak memory while reading (glider / SQLite): 633 MB / 617 MB at 500 MiB,
1.28 GB / 713 MB at 1 GiB, 2.58 GB / 1.09 GB at 5 GiB, 4.06 GB / 1.21 GB at
10 GiB, 8.61 GB / 1.39 GB at 25 GiB. Memgraph held 1.7, 2.8 and 10.8 GiB
resident for the 500 MiB, 1 GiB and 5 GiB graphs.

### Reads: warm median (fastest in bold)


#### 1GiB

| query | glider | SQLite | Memgraph | answers |
|---|---|---|---|---|
| point: indexed text (Person.email) | **13.0 µs** | 25.0 µs | 44.0 µs | agree |
| point: indexed text (Order.ref) | **11.0 µs** | 28.0 µs | 55.0 µs | agree |
| point: by id | 24.0 µs | **15.0 µs** | 25.0 µs | agree |
| 1-hop out: community (KNOWS) | **20.0 µs** | 52.0 µs | 81.0 µs | agree |
| 1-hop in: power-law hub (KNOWS) | **184 µs** | 242 µs | 604 µs | agree |
| 1-hop in: hub followers' average age | 8.36 ms | **3.58 ms** | 3.93 ms | agree |
| 2-hop: friends of friends | **19.0 µs** | 33.0 µs | 94.0 µs | agree |
| 2-hop: distinct friends of friends | **10.0 µs** | 35.0 µs | 42.0 µs | agree |
| 3-hop: KNOWS out to depth 3 | **44.0 µs** | 53.0 µs | 59.0 µs | agree |
| 2-hop bipartite: buyer -> orders -> products | **43.0 µs** | **43.0 µs** | 79.0 µs | agree |
| recommendation: co-purchased products | **76.0 µs** | 77.0 µs | 205 µs | agree |
| 1-hop fetch: friends' names, sorted page | **11.0 µs** | 45.0 µs | 61.0 µs | agree |
| BFS: everyone within 2 KNOWS hops, with depth | **9.0 µs** | 50.0 µs | 57.0 µs | agree |
| BFS: everyone within 3 KNOWS hops, with depth | **87.0 µs** | 128 µs | 224 µs | agree |
| shortest path: person to person (KNOWS, <= 12 hops) | **442 µs** | 16.9 s | 1.15 s | agree |
| multi-edges: order lines | **22.0 µs** | 55.0 µs | 76.0 µs | agree |
| dense clique: SIMILAR 2-hop | **12.0 µs** | 39.0 µs | 35.0 µs | agree |
| tree: category ancestors (var-length) | **38.0 µs** | 77.0 µs | 62.0 µs | agree |
| chain: NEXT*1..200 | 237 µs | **146 µs** | 252 µs | agree |
| DAG: CITES*1..3 from a document | **14.0 µs** | 60.0 µs | 31.0 µs | agree |
| heterogeneous: MENTIONS by label | **15.0 µs** | 70.0 µs | 39.0 µs | agree |
| index, low cardinality (country) | **281 µs** | 433 µs | 6.94 ms | agree |
| index + property filter | 8.12 ms | 3.44 ms | **3.38 ms** | agree |
| self-loops: RETRY | 4.81 ms | **136 µs** | 90.6 ms | agree |
| label scan + unindexed filter | 68.2 ms | **14.6 ms** | 136 ms | agree |
| aggregate: group by | 141 ms | **108 ms** | 174 ms | agree |
| top-k: ORDER BY LIMIT over big rows | 156 ms | **34.6 ms** | 332 ms | agree |
| edge scan + edge-property filter | 122 ms | **22.2 ms** | 248 ms | agree |
| degree: top 5 KNOWS in-degree | 2.75 s | **204 ms** | 1.53 s | agree |
| count all nodes | **13.0 µs** | 10.3 ms | 55.6 ms | agree |

#### 10GiB

| query | glider | SQLite | Memgraph | answers |
|---|---|---|---|---|
| point: indexed text (Person.email) | **12.0 µs** | 32.0 µs | — | agree |
| point: indexed text (Order.ref) | **10.0 µs** | 38.0 µs | — | agree |
| point: by id | **9.0 µs** | 15.0 µs | — | agree |
| 1-hop out: community (KNOWS) | **21.0 µs** | 47.0 µs | — | agree |
| 1-hop in: power-law hub (KNOWS) | **729 µs** | 1.02 ms | — | agree |
| 1-hop in: hub followers' average age | 44.1 ms | **21.7 ms** | — | agree |
| 2-hop: friends of friends | **36.0 µs** | 46.0 µs | — | agree |
| 2-hop: distinct friends of friends | **12.0 µs** | 41.0 µs | — | agree |
| 3-hop: KNOWS out to depth 3 | **25.0 µs** | 65.0 µs | — | agree |
| 2-hop bipartite: buyer -> orders -> products | 71.0 µs | **58.0 µs** | — | agree |
| recommendation: co-purchased products | **56.0 µs** | 82.0 µs | — | agree |
| 1-hop fetch: friends' names, sorted page | **11.0 µs** | 38.0 µs | — | agree |
| BFS: everyone within 2 KNOWS hops, with depth | **12.0 µs** | 29.0 µs | — | agree |
| BFS: everyone within 3 KNOWS hops, with depth | **59.0 µs** | 76.0 µs | — | agree |
| shortest path: person to person (KNOWS, <= 12 hops) | **600 µs** | 2.7 min | — | agree |
| multi-edges: order lines | **27.0 µs** | 48.0 µs | — | agree |
| dense clique: SIMILAR 2-hop | **26.0 µs** | 57.0 µs | — | agree |
| tree: category ancestors (var-length) | **28.0 µs** | 69.0 µs | — | agree |
| chain: NEXT*1..200 | **138 µs** | 156 µs | — | agree |
| DAG: CITES*1..3 from a document | **22.0 µs** | 65.0 µs | — | agree |
| heterogeneous: MENTIONS by label | **42.0 µs** | 100 µs | — | agree |
| index, low cardinality (country) | **2.79 ms** | 4.38 ms | — | agree |
| index + property filter | 85.1 ms | **35.5 ms** | — | agree |
| self-loops: RETRY | 51 ms | **1.2 ms** | — | agree |
| label scan + unindexed filter | 696 ms | **156 ms** | — | agree |
| aggregate: group by | 1.44 s | **1.23 s** | — | agree |
| top-k: ORDER BY LIMIT over big rows | 3.43 s | **1.73 s** | — | agree |
| edge scan + edge-property filter | 2.41 s | **241 ms** | — | agree |
| degree: top 5 KNOWS in-degree | 40.5 s | **2.09 s** | — | agree |
| count all nodes | **5.0 µs** | 1.66 s | — | agree |

#### 25GiB

| query | glider | SQLite | Memgraph | answers |
|---|---|---|---|---|
| point: indexed text (Person.email) | **12.0 µs** | 23.0 µs | — | agree |
| point: indexed text (Order.ref) | **27.0 µs** | 28.0 µs | — | agree |
| point: by id | **4.0 µs** | 15.0 µs | — | agree |
| 1-hop out: community (KNOWS) | 44.0 µs | **38.0 µs** | — | agree |
| 1-hop in: power-law hub (KNOWS) | **1.33 ms** | 1.78 ms | — | agree |
| 1-hop in: hub followers' average age | **731 ms** | 22.2 s | — | agree |
| 2-hop: friends of friends | 46.0 µs | **35.0 µs** | — | agree |
| 2-hop: distinct friends of friends | **21.0 µs** | 45.0 µs | — | agree |
| 3-hop: KNOWS out to depth 3 | **54.0 µs** | 78.0 µs | — | agree |
| 2-hop bipartite: buyer -> orders -> products | 68.0 µs | **52.0 µs** | — | agree |
| recommendation: co-purchased products | **82.0 µs** | 89.0 µs | — | agree |
| 1-hop fetch: friends' names, sorted page | **12.0 µs** | 40.0 µs | — | agree |
| BFS: everyone within 2 KNOWS hops, with depth | **37.0 µs** | 49.0 µs | — | agree |
| BFS: everyone within 3 KNOWS hops, with depth | **144 µs** | 157 µs | — | agree |
| shortest path: person to person (KNOWS, <= 12 hops) | **1.04 ms** | 7.7 min | — | agree |
| multi-edges: order lines | **27.0 µs** | 48.0 µs | — | agree |
| dense clique: SIMILAR 2-hop | **27.0 µs** | 61.0 µs | — | agree |
| tree: category ancestors (var-length) | **24.0 µs** | 79.0 µs | — | agree |
| chain: NEXT*1..200 | **144 µs** | 157 µs | — | agree |
| DAG: CITES*1..3 from a document | **31.0 µs** | 53.0 µs | — | agree |
| heterogeneous: MENTIONS by label | **18.0 µs** | 60.0 µs | — | agree |
| index, low cardinality (country) | **7.2 ms** | 11.2 ms | — | agree |
| index + property filter | 1.69 s | **107 ms** | — | agree |
| self-loops: RETRY | 132 ms | **3.08 ms** | — | agree |
| label scan + unindexed filter | 1.7 s | **418 ms** | — | agree |
| aggregate: group by | 3.58 s | **3.33 s** | — | agree |
| top-k: ORDER BY LIMIT over big rows | **8.57 s** | 10.6 s | — | agree |
| edge scan + edge-property filter | 6.05 s | **767 ms** | — | agree |
| degree: top 5 KNOWS in-degree | 1.8 min | **5.7 s** | — | agree |
| count all nodes | **8.0 µs** | 11.3 s | — | agree |

### Graph algorithms (first run)

| algorithm | 500MiB glider / SQLite | 1GiB glider / SQLite | 5GiB glider / SQLite | 10GiB glider / SQLite | 25GiB glider / SQLite |
|---|---|---|---|---|---|
| degree centrality: top 10 KNOWS in-degree | 1.2 s / 140 ms | 2.31 s / 271 ms | 9.7 s / 1.31 s | 20 s / 2.54 s | 56.8 s / 6.04 s |
| PageRank: 5 iterations over KNOWS | 493 ms / 10.4 s | 3.55 s / 23.1 s | 27.7 s / 1.3 min | 5.9 min / 2.6 min | timeout / 7.1 min |
| weakly connected components over KNOWS | 942 ms / 8.91 s | 2.07 s / 18.2 s | 17.1 s / 1.9 min | 51.4 s / 4.6 min | 10.7 min / 12.4 min |
| BFS: 3 hops over KNOWS | 257 µs / 345 µs | 157 µs / 236 µs | 749 µs / 170 µs | 24.7 ms / 187 µs | 51.4 ms / 331 µs |
| shortest path over KNOWS | 263 µs / 7.9 s | 556 µs / 16.6 s | 4.83 ms / 55.7 s | 4.59 ms / 2.3 min | 16.1 ms / 7.5 min |

### Writes (total time for the workload)

| workload | 500MiB glider / SQLite | 1GiB glider / SQLite | 5GiB glider / SQLite | 10GiB glider / SQLite | 25GiB glider / SQLite |
|---|---|---|---|---|---|
| insert a node, autocommit (x1000) | 58.1 ms / 120 ms | 77.9 ms / 122 ms | 83.6 ms / 130 ms | 103 ms / 128 ms | 96.9 ms / 267 ms |
| insert a node, fsync every commit (x200) | 431 ms / 442 ms | 416 ms / 458 ms | 395 ms / 2.27 s | 427 ms / 443 ms | 401 ms / 449 ms |
| bulk insert 50,000 nodes, one transaction | 643 ms / 214 ms | 638 ms / 256 ms | 760 ms / 236 ms | 679 ms / 237 ms | 727 ms / 241 ms |
| insert 5,000 edges between indexed nodes, one transaction | 1.15 s / 2.38 s | 1.05 s / 4.45 s | 1.94 s / 9.43 s | 8.7 s / 11.2 s | 12.8 s / 15.5 s |
| update 10,000 properties by index, one transaction | 847 ms / 661 ms | 1.05 s / 1.55 s | 2.29 s / 5.8 s | 7.81 s / 9.73 s | 19.1 s / 10.3 s |
| delete 500 nodes with their edges, one transaction | 1.9 s / 1.84 s | 2.78 s / 3.68 s | 4.31 s / 3.09 s | 5.72 s / 4.27 s | 7.18 s / 4.81 s |

### glider before and after the engine changes (10 GiB, warm)

| query | before | after | SQLite |
|---|---|---|---|
| count all nodes | 10.7 s | 5.0 µs | 1.66 s |
| BFS: everyone within 2 KNOWS hops, with depth | 3.91 s | 12.0 µs | 29.0 µs |
| shortest path: person to person (KNOWS, <= 12 hops) | 1.5 min | 600 µs | 2.7 min |
| BFS: everyone within 3 KNOWS hops, with depth | 3.93 s | 59.0 µs | 76.0 µs |
| index, low cardinality (country) | 1.27 s | 2.79 ms | 4.38 ms |
| 1-hop in: power-law hub (KNOWS) | 40.3 ms | 729 µs | 1.02 ms |
| self-loops: RETRY | 1.99 s | 51 ms | 1.2 ms |
| edge scan + edge-property filter | 31.1 s | 2.41 s | 241 ms |
| label scan + unindexed filter | 3.93 s | 696 ms | 156 ms |
| aggregate: group by | 5.81 s | 1.44 s | 1.23 s |
| 3-hop: KNOWS out to depth 3 | 87.0 µs | 25.0 µs | 65.0 µs |
| recommendation: co-purchased products | 145 µs | 56.0 µs | 82.0 µs |
| degree: top 5 KNOWS in-degree | 1.7 min | 40.5 s | 2.09 s |
| 2-hop bipartite: buyer -> orders -> products | 144 µs | 71.0 µs | 58.0 µs |
| index + property filter | 163 ms | 85.1 ms | 35.5 ms |
| top-k: ORDER BY LIMIT over big rows | 5.95 s | 3.43 s | 1.73 s |
| 2-hop: distinct friends of friends | 20.0 µs | 12.0 µs | 41.0 µs |
| chain: NEXT*1..200 | 209 µs | 138 µs | 156 µs |
| tree: category ancestors (var-length) | 40.0 µs | 28.0 µs | 69.0 µs |
| point: indexed text (Person.email) | 17.0 µs | 12.0 µs | 32.0 µs |
| point: indexed text (Order.ref) | 14.0 µs | 10.0 µs | 38.0 µs |
| 1-hop in: hub followers' average age | 60.5 ms | 44.1 ms | 21.7 ms |
| 1-hop fetch: friends' names, sorted page | 14.0 µs | 11.0 µs | 38.0 µs |
| 2-hop: friends of friends | 45.0 µs | 36.0 µs | 46.0 µs |
| point: by id | 11.0 µs | 9.0 µs | 15.0 µs |
| 1-hop out: community (KNOWS) | 23.0 µs | 21.0 µs | 47.0 µs |
| DAG: CITES*1..3 from a document | 24.0 µs | 22.0 µs | 65.0 µs |
| dense clique: SIMILAR 2-hop | 21.0 µs | 26.0 µs | 57.0 µs |
| multi-edges: order lines | 13.0 µs | 27.0 µs | 48.0 µs |
| heterogeneous: MENTIONS by label | 17.0 µs | 42.0 µs | 100 µs |



The interactive report (`python3 bench/suite_report.py`) has every size, cold
and warm, the queries, the answers, SQLite's plans, and links into Grafana.

## Engine changes made along the way

The first comparison (`bench/results/compare/`) showed SQLite ahead on most
queries. Profiling and query plans pointed at five causes, all fixed:

1. **Traversals built a whole-graph projection first.** `CALL bfs`, `dfs`,
   `subgraph` and unweighted `shortestpath` now walk the adjacency directly
   (`src/traverse.rs`), keeping state only for what they reach; shortest path
   searches from both ends. They fall back to the whole-graph algorithms if a
   walk outgrows the algorithm memory budget.
2. **Patterns never started from an edge type.** The planner now anchors on a
   relationship type when it has fewer edges than any node anchor has
   candidates (`()-[r:RARE]->()`), streaming that type's edges.
3. **Counts enumerated rows.** `count` over a whole label, an index bucket, an
   edge type, or one typed hop from an indexed node is answered from stored
   counters and adjacency range counts.
4. **Records were read by lookup and decoded repeatedly.** Label and node
   scans read records with one cursor in id order; a one-entry record cache
   serves the repeated reads of a row (pattern check, `WHERE`, each returned
   property); binding names are shared rather than copied per row.
5. **Typed expansion read every edge.** The adjacency key is now
   `(node, direction, type, edge)`, so a typed expansion or degree is a range
   scan. Files from before rebuild their adjacency once on open.

Plus relationship uniqueness (a match never uses the same relationship twice),
found by the answer checks, and `glider export` now streams instead of building
the dump in memory (it aborted on multi-GB graphs).

## Caveats and known issues

- **One machine, one run.** Numbers are from a single run per configuration on
  one desktop. Warm medians are stable; cold numbers depend on the SSD and
  what else the OS had cached.
- **Memgraph is partial.** It ran fully at 500 MiB and 1 GiB. At 5 GiB the run
  was stopped for low system memory after the reads and one algorithm; 10 GiB
  would need about 22 GiB of RAM and 25 GiB about 54 GiB, which this machine
  could not provide alongside everything else. It is an in-memory database;
  this is the trade it makes, not a defect.
- **glider's memory grows with data size while reading** (8.6 GB peak at
  25 GiB with a 1 GiB page cache), so something in the read path is not
  bounded. Under investigation.
- **glider's out-of-core PageRank is slow.** Past 10 GiB the algorithm state
  no longer fits the default 256 MB `--work-mem` and PageRank runs from pages:
  356 s at 10 GiB, over the 30-minute limit at 25 GiB (SQLite: 7 min). A
  larger `--work-mem` keeps it in memory.
- **Scans and aggregates remain SQLite's.** `GROUP BY` and degree top-5 are
  dominated by glider's row-at-a-time grouping (`glider::query::FirstSeen` in
  the flamegraphs).
- **Run history.** The first full run was discarded after the relationship
  uniqueness fix (it used the old binary); the 25 GiB algorithms were re-run
  after a harness bug let a timed-out glider algorithm hand its late answer to
  the next one. The discarded results are not in `bench/results/`.

## Reproducing it

```sh
bench/lgtm/up.sh                      # observability stack + glider profiling build
cargo build --release --workspace     # scale-gen
bench/wasm/prepare.sh                 # optional: the in-browser comparison
python3 bench/suite.py                # glider + SQLite, all sizes (hours at 25 GiB)
uv run --with neo4j python3 bench/memgraph_bench.py --sizes 500MiB,1GiB
python3 bench/suite_report.py --out site/index.html --wasm-dir site/wasm
```

Datasets go to `../glider-bench-data` (override with `GLIDER_BENCH_DATA`),
outside the repository: the Elixir bindings' build reads every file under this
crate, and multi-gigabyte files here exhaust its memory. Plan on about 180 GB
of disk for every size and engine at once.

| file | what it is |
|---|---|
| `bench/suite.py` | the glider/SQLite suite: builds, reads, algorithms, writes, telemetry |
| `bench/sqlite_compare.py` | the query list, SQLite schema and loader |
| `bench/memgraph_bench.py` | Memgraph: CSV export, load, translated queries, MAGE |
| `bench/otel.py` | OTLP traces, logs and metrics, standard library only |
| `bench/lgtm/` | Grafana stack, Alloy eBPF profiling, the dashboard |
| `bench/suite_report.py`, `bench/suite.html` | the interactive report |
| `bench/wasm/` | glider and SQLite as WebAssembly in a browser worker |
| `bench/results/` | raw results of every run described here |
