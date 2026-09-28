#!/usr/bin/env python3
"""Render ../glider-bench-data/suite/results.jsonl (bench/suite.py) as one
page: a tab per size with reads, algorithms and writes, links from every row
into the local Grafana (trace, logs, flamegraphs), a cross-size view with the
before/after of the engine changes, and an in-browser WebAssembly
comparison.

    python3 bench/suite_report.py --out site/index.html --wasm-dir site/wasm

The page loads bench/wasm/bench-worker.mjs and the two wasm builds from
./wasm/ next to it; --wasm-dir copies them there.
"""

import argparse
import json
import os
import shutil
import sqlite3
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import sqlite_compare  # noqa: E402
import suite  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))


def collect(path, before_path):
    sizes, builds, mem = [], {}, {}
    reads, algos, writes = {}, {}, {}
    mg = {"mg-read": {}, "mg-algo": {}, "mg-write": {}}
    for line in open(path):
        d = json.loads(line)
        size = d.get("size")
        if size and size not in sizes:
            sizes.append(size)
        ph = d["phase"]
        if ph == "build":
            builds.setdefault(size, {})[d["engine"]] = d
        elif ph == "memory":
            mem[size] = d
        elif ph == "read":
            reads.setdefault(size, {})[d["name"]] = d
        elif ph == "algo":
            algos.setdefault(size, {})[d["name"]] = d
        elif ph == "write":
            writes.setdefault(size, {})[d["name"]] = d
        elif ph == "mg-load":
            builds.setdefault(size, {})["memgraph"] = d
        elif ph in mg:
            mg[ph][(size, d["name"])] = d
    # Memgraph's results join the glider/SQLite rows they were checked against.
    for ph, table in (("mg-read", reads), ("mg-algo", algos), ("mg-write", writes)):
        for (size, name), d in mg[ph].items():
            row = table.get(size, {}).get(name)
            if row is None:
                continue
            row["memgraph"] = d["memgraph"]
            row["mg_match"] = d.get("match")
            if ph == "mg-algo" and d.get("kind") == "pagerank":
                # MAGE's PageRank ranks the KNOWS subgraph; glider's (and the
                # SQL) rank every node, so the rankings are not comparable.
                row["mg_match"] = None
                row["memgraph"]["note"] = "MAGE ranks the KNOWS subgraph only; glider and SQLite rank every node, so the rankings differ by design"
            row["mg_cypher"] = d.get("cypher")
            if d.get("counts"):
                row.setdefault("counts", {})["memgraph"] = d["counts"]
            t = d.get("telemetry") or {}
            if t.get("memgraph_window"):
                row.setdefault("telemetry", {})["memgraph_window"] = t["memgraph_window"]
                row["telemetry"]["mg_trace_id"] = t.get("trace_id")
    # Why Memgraph is missing or partial at a size.
    mg_status = {}
    # Scale from the largest size Memgraph loaded (smaller ones carry more
    # fixed overhead per GiB).
    gib_of = lambda sz: int(sz[:-3]) if sz.endswith("GiB") else 0.5  # noqa: E731
    loaded = [(gib_of(sz), b["memgraph"]) for sz, b in builds.items() if b.get("memgraph", {}).get("ok")
              and str(b["memgraph"].get("memory", "")).endswith("GiB")]
    per_gib = None
    if loaded:
        g, ref = max(loaded, key=lambda x: x[0])
        per_gib = float(ref["memory"][:-3]) / g
    for sz in sizes:
        mgb = builds.get(sz, {}).get("memgraph")
        n_algo = sum(1 for (s2, _), _d in mg["mg-algo"].items() if s2 == sz)
        n_write = sum(1 for (s2, _), _d in mg["mg-write"].items() if s2 == sz)
        if not mgb:
            need = per_gib * gib_of(sz) if per_gib else None
            if need and need < 31:
                mg_status[sz] = (f"not run: Memgraph keeps the whole graph in memory, about {need:.0f} GiB here, and the "
                                 f"5 GiB run had already been stopped for low system memory on this 31 GiB machine")
            else:
                mg_status[sz] = (f"not run: Memgraph keeps the whole graph in memory and this graph would need about "
                                 f"{need:.0f} GiB of RAM, more than this 31 GiB machine has")
        elif n_algo < 5 or n_write < 6:
            mg_status[sz] = (f"partial: the run was stopped for low system memory after the reads, {n_algo} of 5 algorithms "
                             f"and {n_write} of 6 writes")
    order = {s: i for i, s in enumerate(suite.SIZES)}
    sizes = sorted([s for s in sizes if s in reads or s in algos or s in writes], key=lambda s: order.get(s, 99))
    before = {}
    if before_path and os.path.exists(before_path):
        for line in open(before_path):
            d = json.loads(line)
            if d.get("phase") == "query":
                before.setdefault(d["size"], {})[d["name"]] = {
                    "glider_warm_ms": d["glider"]["warm_ms"] or d["glider"]["cold_ms"],
                    "sqlite_warm_ms": d["sqlite"]["warm_ms"] or d["sqlite"]["cold_ms"]}
    return {
        "sizes": sizes, "builds": builds, "memory": mem,
        "reads": {s: list(reads.get(s, {}).values()) for s in sizes},
        "algos": {s: list(algos.get(s, {}).values()) for s in sizes},
        "writes": {s: list(writes.get(s, {}).values()) for s in sizes},
        "before": before, "sqlite_version": sqlite3.sqlite_version, "mg_status": mg_status,
        "grafana": os.environ.get("GRAFANA_URL", "http://localhost:3000"),
    }


def main():
    ap = argparse.ArgumentParser()
    # The committed results (bench/results/) when there is no fresh run.
    live = os.path.join(suite.DATA, "results.jsonl")
    kept = os.path.join(HERE, "results", "suite", "results.jsonl")
    ap.add_argument("--results", default=live if os.path.exists(live) else kept)
    live_b = os.path.join(sqlite_compare.DATA, "results.jsonl")
    ap.add_argument("--before", default=live_b if os.path.exists(live_b) else os.path.join(HERE, "results", "compare", "results.jsonl"))
    ap.add_argument("--out", default=os.path.join(suite.DATA, "site", "index.html"))
    ap.add_argument("--wasm-dir", help="copy the WebAssembly benchmark (worker and both engines) here")
    a = ap.parse_args()
    data = collect(a.results, a.before)
    tpl = open(os.path.join(HERE, "suite.html")).read()
    blob = json.dumps(data, ensure_ascii=False).replace("</", "<\\/")
    os.makedirs(os.path.dirname(os.path.abspath(a.out)), exist_ok=True)
    open(a.out, "w").write(tpl.replace("/*__DATA__*/null", blob))
    if a.wasm_dir:
        src = os.path.join(HERE, "wasm")
        shutil.copytree(src, a.wasm_dir, dirs_exist_ok=True)
    print(f"wrote {a.out} ({len(data['sizes'])} sizes)")


if __name__ == "__main__":
    main()
