# glider on k3s: the scale demo

Two graphs — **1 GiB** (1.25M nodes, 8M relationships) and **5 GiB** (6.2M
nodes, 40M relationships) — each served by `glider serve` from a pod with the
**same 768 Mi memory limit**, on one worker node, from one local disk. A
benchmark job runs the scale suite's 23 queries (and, with `--algos`, three
whole-graph algorithms) against both and prints the comparison. Prometheus
scrapes every pod's `/metrics`, so the page cache, I/O and the process's
resident memory are in Grafana while it runs.

The point being demonstrated: the file grows 5×, the pod does not. RAM is
the page cache (`--cache-size 256M`) plus working memory (`--work-mem
128M`) plus a few MB, however large the graph.

```sh
k8s/demo/deploy.sh            # build the image, push to Harbor, apply, start a bench job
k8s/demo/deploy.sh bench      # another benchmark run
kubectl logs -n glider-demo job/glider-bench-<run> -f
```

| what | where |
|---|---|
| servers | https://glider-1g.home, https://glider-5g.home (the console UI; `POST /query` with a Cypher body; `GET /metrics`) |
| data and results | PVC `glider-data` on `k3s-4`: `/data/scale-*.gldb`, `/data/gen-*.log`, `/data/results/*.json` |
| generator | Job `glider-gen-<size>`: `scale-gen --paged` bulk build — external sorts and bottom-up trees, ~600 MB of RAM at any size |
| memory | `glider_process_memory_bytes`, `glider_process_memory_peak_bytes`; `container_memory_working_set_bytes{namespace="glider-demo"}` from cAdvisor |

Everything is pinned to one node (`NODE=k3s-4` in `deploy.sh`) because
`local-path` volumes are node-local. `SIZES="1g 5g 10g"` adds a size; disk
is the only cost (a paged file is ~1.4× its nominal size, plus the
generator's temporary sort runs).

## Files

- `00-base.yaml` — namespace and the shared volume
- `stack.yaml.tmpl` — per size: generator Job, Deployment, Service, Ingress
- `40-bench.yaml.tmpl` — the benchmark Job
- `bench.py` — the harness (std lib only; imports `bench/scale.py` for the
  id layout and the query list, so the queries are exactly `bench/PAGED.md`'s)
- `../../Dockerfile` — `rust:1-slim-bookworm` builder, `python:3.12-slim-bookworm` runtime

## Reading the numbers

- **first** is the cold number: the server had answered nothing before that
  query. The OS page cache on the node may still hold parts of a freshly
  generated file, so "cold" here means cold for glider, not for the disk.
- **warm** is the median of three repeats.
- `reads` beside each query in the log is the page reads it cost the
  server; `rss` is the process's resident memory after it.
- Whole-graph algorithms spill their per-node state to `/data/*-tmp/` once
  it exceeds `--work-mem`; at 5 GiB and 128 MiB they run out of core and are
  slow by design. Raise `--work-mem` to trade memory for time.

## Grafana

Import `grafana-dashboard.json` (Dashboards → New → Import, pick the
Prometheus datasource). It plots the servers' resident memory against the
pod limit, container working set from cAdvisor, statement latency
percentiles, statements per second, page reads and the cache miss ratio,
all split by graph size.

## Measured (2026-11-08, k3s-4: 4 cores, 4 GiB RAM, local disk)

Job `glider-bench-20261108-102139`; full record in `/data/results/` on the
volume. Both servers: `--cache-size 256M --work-mem 128M`, pod limit 768 Mi.

| graph | nodes | relationships | file | build | peak RSS of the server |
|---|---|---|---|---|---|
| 1g | 1,247,371 | 8,027,661 | 1.43 GiB | 1 min 52 s | 303 MB |
| 5g | 6,237,595 | 40,209,610 | 7.25 GiB | 8 min 03 s | 490 MB |

| query (first / warm) | 1g | 5g |
|---|---|---|
| point: indexed text (Person.email) | 62.8 ms / 9.4 ms | 22.8 ms / 12.9 ms |
| point: unindexed, by id() | 26.2 ms / 6.2 ms | 24.0 ms / 5.9 ms |
| 1-hop in: power-law hub (KNOWS) | 49.8 ms / 7.1 ms | 80.2 ms / 11.8 ms |
| 2-hop: friends of friends | 9.4 ms / 11.7 ms | 43.4 ms / 8.8 ms |
| 2-hop bipartite: buyer -> orders -> products | 7.7 ms / 5.8 ms | 198.5 ms / 6.7 ms |
| tree: category ancestors (var-length) | 7.7 ms / 6.8 ms | 76.2 ms / 6.3 ms |
| chain: NEXT*1..200 | 7.0 ms / 8.7 ms | 33.4 ms / 10.6 ms |
| index, low cardinality (country) | 8.5 ms / 6.8 ms | 28.6 ms / 11.9 ms |
| index + property filter | 2.25 s / 39.2 ms | 14.86 s / 14.38 s |
| self-loops: RETRY | 330.7 ms / 26.4 ms | 2.22 s / 99.3 ms |
| label scan + unindexed filter | 575.5 ms / 335.8 ms | 2.68 s / 1.58 s |
| aggregate: group by | 694.8 ms / 657.5 ms | 3.53 s / 3.33 s |
| top-k: ORDER BY LIMIT over big rows | 3.53 s / 1.38 s | 18.28 s / 17.41 s |
| edge scan + edge-property filter | 2.60 s / 514.3 ms | 14.29 s / 15.19 s |
| count all nodes | 6.9 ms / 17.3 ms | 6.4 ms / 7.0 ms |
| algo: pagerank 5 iterations (1g, run separately) | 47 s | — |

What to read from it:

- **Memory did not move with the data.** 5× the file, the same pod, peak
  RSS under 500 MB in both. ~6 ms of every warm number is the HTTP round
  trip through Traefik.
- **Point lookups and traversals are flat** across sizes: a few page reads
  each, cached after the first touch.
- **Scans scale with what they read**, and the cache decides whether warm
  helps: `index + property filter` reads 5,383 pages at 1g (84 MiB, fits
  the 256 MiB cache: 2.25 s → 39 ms) and 27,016 pages at 5g (422 MiB, does
  not fit: 14.9 s → 14.4 s). The same is true of `top-k` (94k pages) and
  the edge scan (57k). A larger cache, or a sorted/immutable layout
  (`docs/LAKE.md`), is the lever.
- **Whole-graph algorithms out of core are disk-bound here:** `wcc` on 1g
  took 2.7 min at 128 MiB of working memory (4 s on a desktop with a 1 GiB
  cache). On this hardware run them with `--work-mem` of a few GiB on a
  16 GiB node.

Found along the way, fixed in glider: a container's server is pid 1, so a
lock left by a killed server named a pid that was "alive" again and the pod
crash-looped. The lock now records the holder's start time
(`src/store.rs`); the init container also clears a leftover lock, which
`Recreate` makes safe.
