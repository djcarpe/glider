# glider vs SQLite: the benchmark suite

The methodology and results are written up in
[`docs/BENCHMARKS.md`](../docs/BENCHMARKS.md); raw results are in
[`results/`](results/). This page is how to run it.

`bench/suite.py` runs the same graph workloads against glider and SQLite at
500 MiB, 1, 5, 10 and 25 GiB, checks that both engines give the same answers,
and records traces, logs, metrics and CPU flamegraphs in a local Grafana LGTM
stack. `bench/suite_report.py` turns the results into one HTML page.

## Run it

```sh
bench/lgtm/up.sh                         # Grafana, Loki, Tempo, Prometheus, Pyroscope, Alloy; profiling build
bench/wasm/prepare.sh                    # optional: the in-browser WebAssembly comparison
python3 bench/suite.py                   # all sizes, all phases (hours at 25 GiB)
python3 bench/suite.py --sizes 1GiB --only reads --q "BFS,shortest"
python3 bench/suite_report.py --out site/index.html --wasm-dir site/wasm
```

Datasets and results go to `../glider-bench-data/suite` (or `$GLIDER_BENCH_DATA/suite`),
outside the repository: glider_ex's Rustler build reads every file under this
crate, and multi-gigabyte files here exhaust memory.

## What runs

| phase | glider | SQLite |
|---|---|---|
| build | `scale-gen --paged` bulk load | `glider export` streamed into a table per label and per relationship type, FKs, both-way indexes |
| reads | 30 Cypher queries: lookups, 1–3 hops, var-length, BFS, shortest path, scans, aggregates | the same in SQL, recursive CTEs where needed |
| algorithms | `CALL degree / pagerank / wcc / bfs / shortestpath` | the same algorithms in SQL (iterated for PageRank and WCC) |
| writes | inserts (autocommit, fsync, bulk), indexed edge inserts, updates, detach-deletes | the same through SQL |

Reads run cold (fresh process, files evicted) and then warm (median of five),
followed by a 1.5 s profiling window. Writes run last because they change the
data; re-run with `--rebuild` for pristine datasets.

## Observability

Every benchmark is a trace (a span per engine and per run) with a log line per
run and a `bench_duration_milliseconds` gauge. Grafana Alloy profiles the
`glider` process and the SQLite worker (found by its `--sqlite-worker`
argument) with eBPF. glider runs as its `profiling` build: release plus
symbols, frame pointers and legacy symbol mangling so the profiler can name
its functions. The report links each row to its trace, logs and both
flamegraphs; the dashboard is at http://localhost:3000/d/glider-vs-sqlite.

## In the browser

`bench/wasm/bench-worker.mjs` runs both engines compiled to WebAssembly in a
Web Worker on a generated graph and checks every answer. `node
bench/wasm/node-test.mjs` runs the same thing headless.
